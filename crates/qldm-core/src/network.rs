//! Interface addresses and permanent MAC, ported from `src/devices/networkproperties.cpp`.
//!
//! Addresses are read with `getifaddrs` in the application's network namespace; the permanent
//! MAC comes from the read-only, unprivileged `ETHTOOL_GPERMADDR` ioctl. The interface name is
//! re-checked against its ifindex before and after, so a renamed or replaced interface is
//! reported as withdrawn instead of mixing two interfaces' data.
use crate::dirfd::io_errno;
use crate::ethernet::{valid_interface_name, EthernetProperties};
use crate::model::{Attribute, ReadState};
use std::ffi::{CStr, CString};
use std::sync::atomic::{AtomicBool, Ordering};

const SIOCETHTOOL: libc::c_ulong = 0x8946;
const ETHTOOL_GPERMADDR: u32 = 32;
/// Linux `MAX_ADDR_LEN`.
const ADDRESS_LIMIT: usize = 32;

#[derive(Clone, Debug, Default)]
pub struct NetworkAddress {
    pub family: &'static str,
    pub address: String,
    pub netmask: Attribute,
}

/// Owned worker-thread results for one interface.
#[derive(Clone, Debug, Default)]
pub struct NetworkProperties {
    pub name: String,
    pub path: String,
    pub current_address: Attribute,
    pub assignment_type: Attribute,
    pub link_type: Attribute,
    pub mtu: Attribute,
    pub addresses: Vec<NetworkAddress>,
    pub address_state: Attribute,
    pub permanent_address: Attribute,
    pub ethernet: EthernetProperties,
}

fn failure(error: i32) -> Attribute {
    let state = match error {
        libc::EACCES | libc::EPERM => ReadState::PermissionDenied,
        libc::ENODEV | libc::ENXIO => ReadState::Removed,
        libc::EOPNOTSUPP | libc::ENODATA => ReadState::Unavailable,
        _ => ReadState::Error,
    };
    Attribute { state, value: String::new(), error }
}

fn if_index(name: &str) -> u32 {
    CString::new(name).map_or(0, |c| {
        // SAFETY: `c` is a valid NUL-terminated string.
        unsafe { libc::if_nametoindex(c.as_ptr()) }
    })
}

/// Text of an IPv4/IPv6 socket address, or an `EAFNOSUPPORT` failure for other families.
///
/// # Safety
/// `address` must point to a valid `sockaddr` of the size its family implies.
unsafe fn ip_text(address: *const libc::sockaddr) -> Attribute {
    if address.is_null() {
        return Attribute::default();
    }
    // SAFETY: guaranteed by the caller; the family decides which union view is read.
    unsafe {
        match (*address).sa_family as i32 {
            libc::AF_INET => {
                let raw = (*address.cast::<libc::sockaddr_in>()).sin_addr.s_addr;
                crate::dirfd::available(std::net::Ipv4Addr::from(u32::from_be(raw)).to_string())
            }
            libc::AF_INET6 => {
                let bytes = (*address.cast::<libc::sockaddr_in6>()).sin6_addr.s6_addr;
                crate::dirfd::available(std::net::Ipv6Addr::from(bytes).to_string())
            }
            _ => failure(libc::EAFNOSUPPORT),
        }
    }
}

/// ETHTOOL_GPERMADDR: drivers may report all zeroes when no permanent address is known, which
/// is unavailable evidence rather than a MAC.
fn permanent_address(name: &str) -> Attribute {
    // SAFETY: plain socket(2); the descriptor is closed on every path below.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return failure(io_errno(&std::io::Error::last_os_error()));
    }
    // `struct ethtool_perm_addr { u32 cmd; u32 size; u8 data[]; }` with room for MAX_ADDR_LEN.
    let mut query = [0u8; 8 + ADDRESS_LIMIT];
    query[0..4].copy_from_slice(&ETHTOOL_GPERMADDR.to_ne_bytes());
    query[4..8].copy_from_slice(&(ADDRESS_LIMIT as u32).to_ne_bytes());
    // SAFETY: ifreq is plain data; `ifru_data` points at `query`, which outlives the ioctl.
    let (status, error) = unsafe {
        let mut request: libc::ifreq = std::mem::zeroed();
        for (slot, byte) in request.ifr_name.iter_mut().zip(name.bytes()) {
            *slot = byte as libc::c_char;
        }
        request.ifr_ifru.ifru_data = query.as_mut_ptr().cast();
        let status = libc::ioctl(fd, SIOCETHTOOL as _, &mut request);
        let error = io_errno(&std::io::Error::last_os_error());
        libc::close(fd);
        (status, error)
    };
    if status < 0 {
        return failure(error);
    }
    // This collector requests MAC addresses only for Ethernet-style interfaces.
    let size = u32::from_ne_bytes(query[4..8].try_into().expect("4 bytes")) as usize;
    if size != 6 {
        return Attribute::default();
    }
    let data = &query[8..8 + size];
    if data.iter().all(|b| *b == 0) {
        return Attribute::default();
    }
    crate::dirfd::available(data.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":"))
}

/// Collects addresses (and, for Ethernet-style interfaces, the permanent MAC) for `name`.
/// `index` must still be that name's ifindex.
pub fn collect_network_properties(name: &str, index: u32, ethernet: bool, cancel: Option<&AtomicBool>) -> NetworkProperties {
    let mut result = NetworkProperties::default();
    if !valid_interface_name(name, index) {
        result.address_state = failure(libc::EINVAL);
        result.permanent_address = result.address_state.clone();
        return result;
    }
    if if_index(name) != index {
        result.address_state = failure(libc::ENODEV);
        result.permanent_address = result.address_state.clone();
        return result;
    }
    let mut raw: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `raw`; it is released with freeifaddrs below.
    if unsafe { libc::getifaddrs(&mut raw) } != 0 {
        result.address_state = failure(io_errno(&std::io::Error::last_os_error()));
    } else {
        result.address_state = crate::dirfd::available("");
        let mut entry = raw;
        while !entry.is_null() {
            if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                result.addresses.clear();
                result.address_state = failure(libc::ECANCELED);
                break;
            }
            // SAFETY: `entry` is a live node of the list owned by getifaddrs until freed.
            let current = unsafe { &*entry };
            entry = current.ifa_next;
            if current.ifa_name.is_null() || current.ifa_addr.is_null() {
                continue;
            }
            // SAFETY: ifa_name is a NUL-terminated string.
            let label = unsafe { CStr::from_ptr(current.ifa_name) }.to_string_lossy();
            // getifaddrs may use the IPv4 address label (e.g. eth0:1) instead of the base
            // interface name. Include these secondary addresses.
            if label != name && !(label.starts_with(name) && label.as_bytes().get(name.len()) == Some(&b':')) {
                continue;
            }
            // SAFETY: ifa_addr is non-null and a valid sockaddr per getifaddrs.
            let family = unsafe { (*current.ifa_addr).sa_family } as i32;
            if family != libc::AF_INET && family != libc::AF_INET6 {
                continue;
            }
            // SAFETY: ifa_addr is a valid IPv4/IPv6 sockaddr (family checked above).
            let address = unsafe { ip_text(current.ifa_addr) };
            if address.state != ReadState::Available {
                result.addresses.clear();
                result.address_state = address;
                break;
            }
            let mut text = address.value;
            if family == libc::AF_INET6 {
                // SAFETY: an AF_INET6 address is a sockaddr_in6.
                let scope = unsafe { (*current.ifa_addr.cast::<libc::sockaddr_in6>()).sin6_scope_id };
                if scope != 0 {
                    text.push_str(&format!("%{scope}"));
                }
            }
            // SAFETY: the netmask, when present, has the same family and layout.
            let netmask = if !current.ifa_netmask.is_null() && unsafe { (*current.ifa_netmask).sa_family } as i32 == family {
                unsafe { ip_text(current.ifa_netmask) }
            } else {
                Attribute::default()
            };
            result.addresses.push(NetworkAddress { family: if family == libc::AF_INET { "IPv4" } else { "IPv6" }, address: text, netmask });
        }
        // SAFETY: `raw` came from a successful getifaddrs and is freed exactly once.
        unsafe { libc::freeifaddrs(raw) };
    }
    if !cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
        result.permanent_address = if ethernet { permanent_address(name) } else { Attribute { state: ReadState::NotApplicable, ..Default::default() } };
    }
    if if_index(name) != index {
        result.addresses.clear();
        result.address_state = failure(libc::ENODEV);
        result.permanent_address = result.address_state.clone();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loopback_index() -> u32 {
        if_index("lo")
    }

    #[test]
    fn invalid_and_withdrawn_interfaces_are_explicit() {
        assert_eq!(collect_network_properties("", 1, false, None).address_state.error, libc::EINVAL);
        assert_eq!(collect_network_properties("lo", 0, false, None).permanent_address.error, libc::EINVAL);
        let moved = collect_network_properties("lo", loopback_index() + 1000, false, None);
        assert_eq!((moved.address_state.state, moved.permanent_address.state), (ReadState::Removed, ReadState::Removed));
    }

    #[test]
    fn loopback_addresses_are_listed_and_mac_does_not_apply() {
        let found = collect_network_properties("lo", loopback_index(), false, None);
        assert_eq!(found.address_state.state, ReadState::Available);
        assert!(found.addresses.iter().any(|a| a.family == "IPv4" && a.address == "127.0.0.1"));
        let v4 = found.addresses.iter().find(|a| a.address == "127.0.0.1").unwrap();
        assert_eq!(v4.netmask.value, "255.0.0.0");
        assert!(found.addresses.iter().filter(|a| a.family == "IPv6").all(|a| a.address.contains(':')));
        assert_eq!(found.permanent_address.state, ReadState::NotApplicable);
        let cancelled = AtomicBool::new(true);
        let stopped = collect_network_properties("lo", loopback_index(), false, Some(&cancelled));
        assert_eq!(stopped.address_state.error, libc::ECANCELED);
        assert!(stopped.addresses.is_empty());
    }

    #[test]
    fn real_ethernet_interfaces_report_a_permanent_address_or_why_not() {
        for entry in std::fs::read_dir("/sys/class/net").unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if std::fs::read_to_string(entry.path().join("type")).map_or(true, |t| t.trim() != "1") {
                continue;
            }
            let found = collect_network_properties(&name, if_index(&name), true, None);
            match found.permanent_address.state {
                ReadState::Available => {
                    let parts: Vec<_> = found.permanent_address.value.split(':').collect();
                    assert!(parts.len() == 6 && parts.iter().all(|p| p.len() == 2), "{name}: {:?}", found.permanent_address);
                }
                other => assert_ne!(other, ReadState::NotApplicable, "{name}"),
            }
        }
    }
}
