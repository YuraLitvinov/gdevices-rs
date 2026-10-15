//! Read-only ethtool snapshot for one Ethernet-style, non-wireless interface, ported from
//! `src/devices/ethernetproperties.cpp`.
//!
//! Uses a private, non-blocking generic-netlink socket with single `GET` requests only: no
//! dumps, no notification subscriptions and no `SET` commands, so link state and modes are
//! observed without ever changing an interface. A shared 2 s budget covers the family lookup
//! and every reply for one interface; replies are strictly validated, and a snapshot whose
//! carrier changed between two reads is rejected whole.
use crate::dirfd::{available, io_errno};
use crate::model::{Attribute, ReadState};
use std::collections::HashSet;
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const REPLY_LIMIT: usize = 65536;
const BIT_LIMIT: u32 = 4096;
const BUDGET: Duration = Duration::from_millis(2000);

// Generic netlink and ethtool netlink constants (linux/genetlink.h, linux/ethtool_netlink.h).
const NLMSG_HDRLEN: usize = 16;
const GENL_HDRLEN: usize = 4;
const NLA_HDRLEN: usize = 4;
const NLA_F_NESTED: u16 = 0x8000;
const NLA_TYPE_MASK: u16 = 0x3fff;
const NLM_F_REQUEST: u16 = 1;
const NLM_F_MULTI: u16 = 2;
const NLM_F_DUMP_INTR: u16 = 16;
const NLMSG_ERROR: u16 = 2;
const GENL_ID_CTRL: u16 = 0x10;
const GENL_MIN_ID: u16 = 0x10;
const CTRL_CMD_NEWFAMILY: u8 = 1;
const CTRL_CMD_GETFAMILY: u8 = 3;
const CTRL_ATTR_FAMILY_ID: u16 = 1;
const CTRL_ATTR_FAMILY_NAME: u16 = 2;
const ETHTOOL_GENL_VERSION: u8 = 1;
const ETHTOOL_MSG_LINKMODES_GET: u8 = 4;
const ETHTOOL_MSG_LINKSTATE_GET: u8 = 6;
const A_HEADER_DEV_INDEX: u16 = 1;
const A_LINKSTATE_HEADER: u16 = 1;
const A_LINKSTATE_LINK: u16 = 2;
const A_LINKMODES_HEADER: u16 = 1;
const A_LINKMODES_AUTONEG: u16 = 2;
const A_LINKMODES_OURS: u16 = 3;
const A_LINKMODES_PEER: u16 = 4;
const A_LINKMODES_SPEED: u16 = 5;
const A_LINKMODES_DUPLEX: u16 = 6;
const A_BITSET_NOMASK: u16 = 1;
const A_BITSET_SIZE: u16 = 2;
const A_BITSET_BITS: u16 = 3;
const A_BITSET_BITS_BIT: u16 = 1;
const A_BITSET_BIT_INDEX: u16 = 1;
const A_BITSET_BIT_NAME: u16 = 2;
const A_BITSET_BIT_VALUE: u16 = 3;

#[derive(Clone, Debug)]
pub struct EthernetProperties {
    pub applicable: bool,
    pub scope: Attribute,
    pub carrier: Attribute,
    /// Mbps, not measured throughput.
    pub speed: Attribute,
    /// Linux `DUPLEX_*` value.
    pub duplex: Attribute,
    /// Linux `AUTONEG_*` value.
    pub autoneg: Attribute,
    /// Kernel-provided bit names, one per line.
    pub supported_modes: Attribute,
    pub partner_modes: Attribute,
    pub source: String,
}

impl Default for EthernetProperties {
    fn default() -> Self {
        let not_applicable = Attribute { state: ReadState::NotApplicable, ..Default::default() };
        Self {
            applicable: false,
            scope: not_applicable,
            carrier: Attribute::default(),
            speed: Attribute::default(),
            duplex: Attribute::default(),
            autoneg: Attribute::default(),
            supported_modes: Attribute::default(),
            partner_modes: Attribute::default(),
            source: String::new(),
        }
    }
}

impl EthernetProperties {
    /// Replaces every value with `reason` (the interface changed or was withdrawn).
    pub fn invalidate(&mut self, reason: &Attribute) {
        for value in
            [&mut self.scope, &mut self.carrier, &mut self.speed, &mut self.duplex, &mut self.autoneg, &mut self.supported_modes, &mut self.partner_modes]
        {
            *value = reason.clone();
        }
    }
}

fn failure(error: i32) -> Attribute {
    let state = match error {
        libc::EACCES | libc::EPERM => ReadState::PermissionDenied,
        libc::ENODEV | libc::ENXIO => ReadState::Removed,
        libc::EOPNOTSUPP | libc::ENOSYS | libc::ENOENT | libc::EPROTONOSUPPORT | libc::EAFNOSUPPORT => ReadState::Unsupported,
        libc::ENODATA => ReadState::Unavailable,
        _ => ReadState::Error,
    };
    Attribute { state, value: String::new(), error }
}

// ---------------------------------------------------------------------------------------
// Netlink attribute encoding / decoding (pure, unit-tested)
// ---------------------------------------------------------------------------------------

type Items = Vec<(u16, Vec<u8>)>;

fn align4(n: usize) -> usize {
    (n + 3) & !3
}

/// Splits a netlink attribute stream; any malformed length rejects the whole stream.
fn attributes(bytes: &[u8]) -> Option<Items> {
    let mut items = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        if bytes.len() - offset < NLA_HDRLEN {
            return None;
        }
        let length = u16::from_ne_bytes([bytes[offset], bytes[offset + 1]]) as usize;
        let kind = u16::from_ne_bytes([bytes[offset + 2], bytes[offset + 3]]) & NLA_TYPE_MASK;
        if length < NLA_HDRLEN || length > bytes.len() - offset || align4(length) > bytes.len() - offset {
            return None;
        }
        items.push((kind, bytes[offset + NLA_HDRLEN..offset + length].to_vec()));
        offset += align4(length);
    }
    Some(items)
}

/// The single attribute of `kind`; `Err` if it appears more than once.
fn item(items: &Items, kind: u16) -> Result<Option<&[u8]>, ()> {
    let mut found = None;
    for (k, data) in items {
        if *k == kind {
            if found.is_some() {
                return Err(());
            }
            found = Some(data.as_slice());
        }
    }
    Ok(found)
}

fn number<const N: usize>(data: Option<&[u8]>) -> Option<[u8; N]> {
    data.and_then(|d| d.try_into().ok())
}

fn append_attribute(bytes: &mut Vec<u8>, kind: u16, value: &[u8]) {
    let length = NLA_HDRLEN + value.len();
    bytes.extend_from_slice(&(length as u16).to_ne_bytes());
    bytes.extend_from_slice(&kind.to_ne_bytes());
    bytes.extend_from_slice(value);
    bytes.resize(bytes.len() + align4(length) - length, 0);
}

fn device_header(index: u32) -> Vec<u8> {
    let mut header = Vec::new();
    append_attribute(&mut header, A_HEADER_DEV_INDEX, &index.to_ne_bytes());
    header
}

/// The reply header must nest this interface's index.
fn matches_device(items: &Items, header_kind: u16, index: u32) -> bool {
    let Ok(Some(header)) = item(items, header_kind) else { return false };
    let Some(fields) = attributes(header) else { return false };
    matches!(item(&fields, A_HEADER_DEV_INDEX), Ok(value) if number::<4>(value).map(u32::from_ne_bytes) == Some(index))
}

fn integer(items: &Items, kind: u16, byte: bool) -> Attribute {
    let Ok(value) = item(items, kind) else { return failure(libc::EPROTO) };
    let Some(value) = value else { return failure(libc::ENODATA) };
    if byte {
        match number::<1>(Some(value)) {
            Some([n]) => available(n.to_string()),
            None => failure(libc::EPROTO),
        }
    } else {
        match number::<4>(Some(value)) {
            Some(n) => available(u32::from_ne_bytes(n).to_string()),
            None => failure(libc::EPROTO),
        }
    }
}

/// Decodes a compact or full bit-set into the kernel's names, one per line.
fn modes(items: &Items, kind: u16, supported: bool, cancel: Option<&AtomicBool>) -> Attribute {
    let Ok(data) = item(items, kind) else { return failure(libc::EPROTO) };
    let Some(data) = data else { return failure(libc::ENODATA) };
    let Some(fields) = attributes(data) else { return failure(libc::EPROTO) };
    let (Ok(nomask), Ok(size_data), Ok(bits)) = (item(&fields, A_BITSET_NOMASK), item(&fields, A_BITSET_SIZE), item(&fields, A_BITSET_BITS)) else {
        return failure(libc::EPROTO);
    };
    let Some(bits) = bits else { return failure(libc::EPROTO) };
    if nomask.is_some_and(|n| !n.is_empty()) || (supported && nomask.is_some()) {
        return failure(libc::EPROTO);
    }
    let mut size = BIT_LIMIT;
    if let Some(raw) = size_data {
        match number::<4>(Some(raw)) {
            Some(n) => size = u32::from_ne_bytes(n),
            None => return failure(libc::EPROTO),
        }
    }
    if size > BIT_LIMIT {
        return failure(libc::EOVERFLOW);
    }
    let Some(entries) = attributes(bits) else { return failure(libc::EPROTO) };
    if entries.len() > BIT_LIMIT as usize {
        return failure(libc::EOVERFLOW);
    }
    let mut seen = HashSet::new();
    let mut names = Vec::new();
    for (entry_kind, entry) in &entries {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return failure(libc::ECANCELED);
        }
        if *entry_kind != A_BITSET_BITS_BIT {
            return failure(libc::EPROTO);
        }
        let Some(bit) = attributes(entry) else { return failure(libc::EPROTO) };
        let (Ok(index_data), Ok(name), Ok(set)) = (item(&bit, A_BITSET_BIT_INDEX), item(&bit, A_BITSET_BIT_NAME), item(&bit, A_BITSET_BIT_VALUE)) else {
            return failure(libc::EPROTO);
        };
        let Some(index) = number::<4>(index_data).map(u32::from_ne_bytes) else { return failure(libc::EPROTO) };
        if index >= size || !seen.insert(index) || set.is_some_and(|s| !s.is_empty()) {
            return failure(libc::EPROTO);
        }
        let mut label = String::new();
        if let Some(name) = name {
            // A NUL-terminated string with no embedded NUL.
            let terminated = name.last() == Some(&0) && name.iter().position(|b| *b == 0) == Some(name.len() - 1);
            if name.is_empty() || name.len() > 128 || !terminated {
                return failure(libc::EPROTO);
            }
            label = String::from_utf8_lossy(&name[..name.len() - 1]).into_owned();
        }
        if label.is_empty() {
            label = format!("bit {index}");
        }
        // OURS mask = supported bits (every listed entry); PEER NOMASK = advertised bits
        // (every entry). In value/mask form only VALUE bits apply.
        if supported || nomask.is_some() || set.is_some() {
            names.push(label);
        }
    }
    if names.is_empty() {
        failure(libc::ENODATA)
    } else {
        available(names.join("\n"))
    }
}

// ---------------------------------------------------------------------------------------
// The request socket
// ---------------------------------------------------------------------------------------

struct Query {
    fd: Option<OwnedFd>,
    error: i32,
    sequence: u32,
    started: Instant,
}

impl Query {
    fn new() -> Self {
        // SAFETY: plain socket(2) call; the returned descriptor is owned by `OwnedFd`.
        let raw = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK, libc::NETLINK_GENERIC) };
        let (fd, error) = if raw < 0 {
            (None, io_errno(&std::io::Error::last_os_error()))
        } else {
            // SAFETY: `raw` is a freshly created descriptor nobody else owns.
            (Some(unsafe { OwnedFd::from_raw_fd(raw) }), 0)
        };
        Self { fd, error, sequence: 0, started: Instant::now() }
    }

    /// Sends one request and returns the attributes of the matching reply.
    fn request(&mut self, family: u16, command: u8, reply_command: u8, attrs: &[u8], cancel: Option<&AtomicBool>) -> Option<Items> {
        let fd = self.fd.as_ref()?.as_raw_fd();
        self.error = 0;
        if self.started.elapsed() >= BUDGET {
            self.error = libc::ETIMEDOUT;
            return None;
        }
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            self.error = libc::ECANCELED;
            return None;
        }
        self.sequence = self.sequence.wrapping_add(1);
        let total = NLMSG_HDRLEN + GENL_HDRLEN + attrs.len();
        let mut message = Vec::with_capacity(total);
        message.extend_from_slice(&(total as u32).to_ne_bytes());
        message.extend_from_slice(&family.to_ne_bytes());
        message.extend_from_slice(&NLM_F_REQUEST.to_ne_bytes());
        message.extend_from_slice(&self.sequence.to_ne_bytes());
        message.extend_from_slice(&0u32.to_ne_bytes()); // port id: let the kernel pick
        message.push(command);
        message.push(if family == GENL_ID_CTRL { 1 } else { ETHTOOL_GENL_VERSION });
        message.extend_from_slice(&[0, 0]);
        message.extend_from_slice(attrs);
        // SAFETY: sockaddr_nl is plain data; the pointers are valid for the call.
        let sent = unsafe {
            let mut kernel: libc::sockaddr_nl = std::mem::zeroed();
            kernel.nl_family = libc::AF_NETLINK as u16;
            libc::sendto(
                fd,
                message.as_ptr().cast(),
                message.len(),
                0,
                (&kernel as *const libc::sockaddr_nl).cast(),
                std::mem::size_of::<libc::sockaddr_nl>() as u32,
            )
        };
        if sent != message.len() as isize {
            self.error = if sent < 0 { io_errno(&std::io::Error::last_os_error()) } else { libc::EIO };
            return None;
        }
        let mut buffer = vec![0u8; REPLY_LIMIT];
        while self.started.elapsed() < BUDGET {
            if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                self.error = libc::ECANCELED;
                return None;
            }
            let remaining = BUDGET.saturating_sub(self.started.elapsed()).as_millis() as i32;
            let mut ready = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
            // SAFETY: `ready` is a valid single-element array for the call's duration.
            let status = unsafe { libc::poll(&mut ready, 1, remaining.clamp(0, 50)) };
            if status < 0 {
                let error = io_errno(&std::io::Error::last_os_error());
                if error == libc::EINTR {
                    continue;
                }
                self.error = error;
                return None;
            }
            if status == 0 {
                continue;
            }
            if ready.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                self.error = libc::EIO;
                return None;
            }
            // SAFETY: the buffer and the out-parameters are valid for the call.
            let (count, sender_ok) = unsafe {
                let mut sender: libc::sockaddr_nl = std::mem::zeroed();
                let mut size = std::mem::size_of::<libc::sockaddr_nl>() as u32;
                let count =
                    libc::recvfrom(fd, buffer.as_mut_ptr().cast(), REPLY_LIMIT, libc::MSG_TRUNC, (&mut sender as *mut libc::sockaddr_nl).cast(), &mut size);
                (count, size as usize == std::mem::size_of::<libc::sockaddr_nl>() && sender.nl_family == libc::AF_NETLINK as u16 && sender.nl_pid == 0)
            };
            if count < 0 {
                let error = io_errno(&std::io::Error::last_os_error());
                if error == libc::EAGAIN || error == libc::EINTR {
                    continue;
                }
                self.error = error;
                return None;
            }
            let count = count as usize;
            if count > REPLY_LIMIT {
                self.error = libc::EOVERFLOW;
                return None;
            }
            if !sender_ok {
                continue; // Only the kernel may answer.
            }
            match parse_reply(&buffer[..count], self.sequence, family, reply_command) {
                Reply::Items(items) => return Some(items),
                Reply::Ignore => {}
                Reply::Error(error) => {
                    self.error = error;
                    return None;
                }
            }
        }
        self.error = libc::ETIMEDOUT;
        None
    }
}

enum Reply {
    Items(Items),
    /// Nothing for this request in the datagram (other sequence, or an ACK).
    Ignore,
    Error(i32),
}

/// Walks the messages in one datagram for the reply to `sequence`.
fn parse_reply(datagram: &[u8], sequence: u32, family: u16, reply_command: u8) -> Reply {
    let mut offset = 0;
    while offset < datagram.len() {
        if datagram.len() - offset < NLMSG_HDRLEN {
            return Reply::Error(libc::EPROTO);
        }
        let header = &datagram[offset..offset + NLMSG_HDRLEN];
        let length = u32::from_ne_bytes(header[0..4].try_into().expect("4 bytes")) as usize;
        let kind = u16::from_ne_bytes([header[4], header[5]]);
        let flags = u16::from_ne_bytes([header[6], header[7]]);
        let seq = u32::from_ne_bytes(header[8..12].try_into().expect("4 bytes"));
        if length < NLMSG_HDRLEN || length > datagram.len() - offset || align4(length) > datagram.len() - offset {
            return Reply::Error(libc::EPROTO);
        }
        let data = &datagram[offset + NLMSG_HDRLEN..offset + length];
        offset += align4(length);
        if seq != sequence {
            continue;
        }
        if kind == NLMSG_ERROR {
            if data.len() < 4 {
                return Reply::Error(libc::EPROTO);
            }
            let code = i32::from_ne_bytes(data[0..4].try_into().expect("4 bytes"));
            if !(-4095..=0).contains(&code) {
                return Reply::Error(libc::EPROTO);
            }
            if code == 0 {
                continue;
            }
            return Reply::Error(-code);
        }
        if kind != family || flags & (NLM_F_MULTI | NLM_F_DUMP_INTR) != 0 || data.len() < GENL_HDRLEN {
            return Reply::Error(libc::EPROTO);
        }
        if data[0] != reply_command {
            return Reply::Error(libc::EPROTO);
        }
        return match attributes(&data[GENL_HDRLEN..]) {
            Some(items) => Reply::Items(items),
            None => Reply::Error(libc::EPROTO),
        };
    }
    Reply::Ignore
}

fn if_index(name: &str) -> u32 {
    CString::new(name).map_or(0, |c| {
        // SAFETY: `c` is a valid NUL-terminated string.
        unsafe { libc::if_nametoindex(c.as_ptr()) }
    })
}

pub fn valid_interface_name(name: &str, index: u32) -> bool {
    !name.is_empty() && name.len() < libc::IFNAMSIZ && !name.contains('\0') && index != 0
}

/// Collects link state and modes for a non-wireless Ethernet interface identified by name and
/// ifindex; a name that no longer maps to that index is reported as removed.
pub fn collect_ethernet_properties(name: &str, index: u32, cancel: Option<&AtomicBool>) -> EthernetProperties {
    let mut result = EthernetProperties {
        applicable: true,
        scope: available(""),
        source: format!("ethtool generic netlink: LINKSTATE_GET / LINKMODES_GET; ifindex {index} ({name})"),
        ..Default::default()
    };
    if !valid_interface_name(name, index) {
        result.invalidate(&failure(libc::EINVAL));
        return result;
    }
    if if_index(name) != index {
        result.invalidate(&failure(libc::ENODEV));
        return result;
    }
    let mut query = Query::new();
    if query.fd.is_none() {
        result.invalidate(&failure(query.error));
        return result;
    }
    let mut lookup = Vec::new();
    append_attribute(&mut lookup, CTRL_ATTR_FAMILY_NAME, b"ethtool\0");
    let Some(family) = query.request(GENL_ID_CTRL, CTRL_CMD_GETFAMILY, CTRL_CMD_NEWFAMILY, &lookup, cancel) else {
        result.invalidate(&failure(query.error));
        return result;
    };
    let id = item(&family, CTRL_ATTR_FAMILY_ID).ok().flatten().and_then(|d| number::<2>(Some(d))).map(u16::from_ne_bytes);
    let Some(id) = id.filter(|id| *id >= GENL_MIN_ID) else {
        result.invalidate(&failure(libc::EPROTO));
        return result;
    };
    let mut state_request = Vec::new();
    append_attribute(&mut state_request, A_LINKSTATE_HEADER | NLA_F_NESTED, &device_header(index));
    let mut modes_request = Vec::new();
    append_attribute(&mut modes_request, A_LINKMODES_HEADER | NLA_F_NESTED, &device_header(index));

    let read_carrier = |query: &mut Query| -> Attribute {
        match query.request(id, ETHTOOL_MSG_LINKSTATE_GET, ETHTOOL_MSG_LINKSTATE_GET, &state_request, cancel) {
            Some(state) if matches_device(&state, A_LINKSTATE_HEADER, index) => integer(&state, A_LINKSTATE_LINK, true),
            Some(_) => failure(libc::EPROTO),
            None => failure(query.error),
        }
    };
    result.carrier = read_carrier(&mut query);
    // Settings and carrier support are independent; a failed carrier query must not hide
    // available capabilities or autonegotiation configuration.
    match query.request(id, ETHTOOL_MSG_LINKMODES_GET, ETHTOOL_MSG_LINKMODES_GET, &modes_request, cancel) {
        Some(settings) if matches_device(&settings, A_LINKMODES_HEADER, index) => {
            result.speed = integer(&settings, A_LINKMODES_SPEED, false);
            result.duplex = integer(&settings, A_LINKMODES_DUPLEX, true);
            result.autoneg = integer(&settings, A_LINKMODES_AUTONEG, true);
            result.supported_modes = modes(&settings, A_LINKMODES_OURS, true, cancel);
            result.partner_modes = modes(&settings, A_LINKMODES_PEER, false, cancel);
        }
        Some(_) => {
            let bad = failure(libc::EPROTO);
            (result.speed, result.duplex, result.autoneg) = (bad.clone(), bad.clone(), bad.clone());
            (result.supported_modes, result.partner_modes) = (bad.clone(), bad);
        }
        None => {
            let bad = failure(query.error);
            (result.speed, result.duplex, result.autoneg) = (bad.clone(), bad.clone(), bad.clone());
            (result.supported_modes, result.partner_modes) = (bad.clone(), bad);
        }
    }
    if result.carrier.state == ReadState::Available {
        let after = read_carrier(&mut query);
        if after.state != ReadState::Available {
            result.invalidate(&after);
        } else if after.value != result.carrier.value {
            result.invalidate(&failure(libc::EAGAIN));
        }
    }
    let not_applicable = Attribute { state: ReadState::NotApplicable, ..Default::default() };
    if result.carrier.state == ReadState::Available && result.carrier.value == "0" {
        (result.speed, result.duplex, result.partner_modes) = (not_applicable.clone(), not_applicable.clone(), not_applicable.clone());
    } else if result.carrier.state != ReadState::Available {
        let carrier = result.carrier.clone();
        (result.speed, result.duplex, result.partner_modes) = (carrier.clone(), carrier.clone(), carrier);
    }
    if result.autoneg.state == ReadState::Available && result.autoneg.value == "0" {
        result.partner_modes = not_applicable;
    }
    if if_index(name) != index {
        result.invalidate(&failure(libc::ENODEV));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nested(kind: u16, children: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        append_attribute(&mut out, kind | NLA_F_NESTED, &children.concat());
        out
    }
    fn leaf(kind: u16, value: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        append_attribute(&mut out, kind, value);
        out
    }
    fn bit(index: u32, name: Option<&str>, value: bool) -> Vec<u8> {
        let mut parts = vec![leaf(A_BITSET_BIT_INDEX, &index.to_ne_bytes())];
        if let Some(name) = name {
            parts.push(leaf(A_BITSET_BIT_NAME, format!("{name}\0").as_bytes()));
        }
        if value {
            parts.push(leaf(A_BITSET_BIT_VALUE, &[]));
        }
        nested(A_BITSET_BITS_BIT, &parts)
    }
    fn bitset(kind: u16, extra: &[Vec<u8>], bits: &[Vec<u8>]) -> Items {
        let mut children = extra.to_vec();
        children.push(nested(A_BITSET_BITS, bits));
        attributes(&nested(kind, &children)).unwrap()
    }

    #[test]
    fn attribute_streams_are_strict() {
        let good = [leaf(1, &[1, 2, 3]), leaf(2, &7u32.to_ne_bytes())].concat();
        let items = attributes(&good).unwrap();
        assert_eq!((items[0].0, items[0].1.len(), items[1].0), (1, 3, 2));
        assert!(attributes(&good[..good.len() - 1]).is_none(), "truncated");
        assert!(attributes(&[3, 0, 1, 0]).is_none(), "length below header size");
        assert!(attributes(&[0xff, 0xff, 1, 0, 0, 0, 0, 0]).is_none(), "length past the end");
        assert!(item(&attributes(&[leaf(1, &[1]), leaf(1, &[2])].concat()).unwrap(), 1).is_err(), "duplicates rejected");
        assert_eq!(attributes(&nested(5, &[])).unwrap()[0].0, 5, "nested flag masked from the type");
    }

    #[test]
    fn integers_and_device_headers() {
        let items = attributes(&[leaf(2, &[1]), leaf(5, &1000u32.to_ne_bytes()), leaf(9, &[1, 2])].concat()).unwrap();
        assert_eq!(integer(&items, 2, true).value, "1");
        assert_eq!(integer(&items, 5, false).value, "1000");
        assert_eq!(integer(&items, 5, true).error, libc::EPROTO, "wrong width");
        assert_eq!(integer(&items, 7, true).state, ReadState::Unavailable, "absent is ENODATA");
        let reply = attributes(&nested(A_LINKSTATE_HEADER, &[leaf(A_HEADER_DEV_INDEX, &3u32.to_ne_bytes())])).unwrap();
        assert!(matches_device(&reply, A_LINKSTATE_HEADER, 3));
        assert!(!matches_device(&reply, A_LINKSTATE_HEADER, 4), "another interface");
        assert!(!matches_device(&attributes(&[]).unwrap(), A_LINKSTATE_HEADER, 3), "missing header");
    }

    #[test]
    fn supported_modes_list_every_entry_and_partner_modes_use_the_value_bits() {
        let supported = bitset(
            A_LINKMODES_OURS,
            &[leaf(A_BITSET_SIZE, &8u32.to_ne_bytes())],
            &[bit(0, Some("10baseT/Half"), false), bit(5, Some("1000baseT/Full"), false), bit(6, None, false)],
        );
        assert_eq!(modes(&supported, A_LINKMODES_OURS, true, None).value, "10baseT/Half\n1000baseT/Full\nbit 6");

        let partner =
            bitset(A_LINKMODES_PEER, &[leaf(A_BITSET_SIZE, &8u32.to_ne_bytes())], &[bit(1, Some("10baseT/Full"), true), bit(5, Some("1000baseT/Full"), false)]);
        assert_eq!(modes(&partner, A_LINKMODES_PEER, false, None).value, "10baseT/Full", "only value bits when not nomask");
        let advertised = bitset(
            A_LINKMODES_PEER,
            &[leaf(A_BITSET_NOMASK, &[]), leaf(A_BITSET_SIZE, &8u32.to_ne_bytes())],
            &[bit(1, Some("10baseT/Full"), false), bit(5, Some("1000baseT/Full"), false)],
        );
        assert_eq!(modes(&advertised, A_LINKMODES_PEER, false, None).value, "10baseT/Full\n1000baseT/Full", "nomask: every entry");
        let none = bitset(A_LINKMODES_PEER, &[leaf(A_BITSET_SIZE, &8u32.to_ne_bytes())], &[bit(1, Some("x"), false)]);
        assert_eq!(modes(&none, A_LINKMODES_PEER, false, None).state, ReadState::Unavailable, "no set bits");
    }

    #[test]
    fn malformed_bit_sets_are_rejected() {
        let duplicate = bitset(A_LINKMODES_OURS, &[leaf(A_BITSET_SIZE, &8u32.to_ne_bytes())], &[bit(1, Some("a"), false), bit(1, Some("b"), false)]);
        assert_eq!(modes(&duplicate, A_LINKMODES_OURS, true, None).error, libc::EPROTO);
        let out_of_range = bitset(A_LINKMODES_OURS, &[leaf(A_BITSET_SIZE, &4u32.to_ne_bytes())], &[bit(4, Some("a"), false)]);
        assert_eq!(modes(&out_of_range, A_LINKMODES_OURS, true, None).error, libc::EPROTO);
        let too_big = bitset(A_LINKMODES_OURS, &[leaf(A_BITSET_SIZE, &9000u32.to_ne_bytes())], &[bit(1, Some("a"), false)]);
        assert_eq!(modes(&too_big, A_LINKMODES_OURS, true, None).error, libc::EOVERFLOW);
        let unterminated = {
            let parts = vec![leaf(A_BITSET_BIT_INDEX, &1u32.to_ne_bytes()), leaf(A_BITSET_BIT_NAME, b"abc")];
            bitset(A_LINKMODES_OURS, &[leaf(A_BITSET_SIZE, &8u32.to_ne_bytes())], &[nested(A_BITSET_BITS_BIT, &parts)])
        };
        assert_eq!(modes(&unterminated, A_LINKMODES_OURS, true, None).error, libc::EPROTO);
        let nomask_on_supported = bitset(A_LINKMODES_OURS, &[leaf(A_BITSET_NOMASK, &[])], &[bit(1, Some("a"), false)]);
        assert_eq!(modes(&nomask_on_supported, A_LINKMODES_OURS, true, None).error, libc::EPROTO);
        assert_eq!(modes(&attributes(&[]).unwrap(), A_LINKMODES_OURS, true, None).state, ReadState::Unavailable);
        let cancelled = AtomicBool::new(true);
        assert_eq!(modes(&duplicate, A_LINKMODES_OURS, true, Some(&cancelled)).error, libc::ECANCELED);
    }

    fn datagram(kind: u16, flags: u16, seq: u32, payload: &[u8]) -> Vec<u8> {
        let total = NLMSG_HDRLEN + payload.len();
        let mut out = Vec::new();
        out.extend_from_slice(&(total as u32).to_ne_bytes());
        out.extend_from_slice(&kind.to_ne_bytes());
        out.extend_from_slice(&flags.to_ne_bytes());
        out.extend_from_slice(&seq.to_ne_bytes());
        out.extend_from_slice(&0u32.to_ne_bytes());
        out.extend_from_slice(payload);
        out.resize(out.len() + align4(total) - total, 0);
        out
    }

    #[test]
    fn replies_are_matched_by_sequence_family_and_command() {
        let family = 0x1c;
        let body = [&[6u8, 1, 0, 0][..], &leaf(2, &[1])].concat();
        match parse_reply(&datagram(family, 0, 7, &body), 7, family, 6) {
            Reply::Items(items) => assert_eq!(items[0].0, 2),
            _ => panic!("expected items"),
        }
        assert!(matches!(parse_reply(&datagram(family, 0, 8, &body), 7, family, 6), Reply::Ignore), "other sequence");
        assert!(matches!(parse_reply(&datagram(family, 0, 7, &body), 7, family, 4), Reply::Error(libc::EPROTO)), "wrong command");
        assert!(matches!(parse_reply(&datagram(family + 1, 0, 7, &body), 7, family, 6), Reply::Error(libc::EPROTO)), "wrong family");
        assert!(matches!(parse_reply(&datagram(family, NLM_F_MULTI, 7, &body), 7, family, 6), Reply::Error(libc::EPROTO)), "multipart");
        let err = |code: i32| [&code.to_ne_bytes()[..], &[0u8; 16][..]].concat();
        assert!(matches!(parse_reply(&datagram(NLMSG_ERROR, 0, 7, &err(-95)), 7, family, 6), Reply::Error(95)));
        assert!(matches!(parse_reply(&datagram(NLMSG_ERROR, 0, 7, &err(0)), 7, family, 6), Reply::Ignore), "ACK");
        assert!(matches!(parse_reply(&datagram(NLMSG_ERROR, 0, 7, &err(5)), 7, family, 6), Reply::Error(libc::EPROTO)), "positive code");
        assert!(matches!(parse_reply(&[1, 2, 3], 7, family, 6), Reply::Error(libc::EPROTO)), "short");
    }

    #[test]
    fn invalid_names_and_withdrawn_interfaces_are_reported_before_any_socket_use() {
        assert_eq!(collect_ethernet_properties("", 1, None).carrier.error, libc::EINVAL);
        assert_eq!(collect_ethernet_properties("eth0", 0, None).carrier.error, libc::EINVAL);
        assert_eq!(collect_ethernet_properties(&"x".repeat(16), 1, None).carrier.error, libc::EINVAL);
        let gone = collect_ethernet_properties("qldm-no-such", 99_999, None);
        assert_eq!((gone.carrier.state, gone.scope.state), (ReadState::Removed, ReadState::Removed));
        assert!(gone.applicable);
    }

    #[test]
    fn real_ethernet_interfaces_answer_or_fail_explicitly() {
        for entry in std::fs::read_dir("/sys/class/net").unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_ether = std::fs::read_to_string(entry.path().join("type")).is_ok_and(|t| t.trim() == "1");
            let wireless = entry.path().join("wireless").exists() || entry.path().join("phy80211").exists();
            if !is_ether || wireless {
                continue;
            }
            let index = if_index(&name);
            let found = collect_ethernet_properties(&name, index, None);
            assert_eq!(found.scope.state, ReadState::Available, "{name}");
            // A reachable kernel answers carrier with 0/1; otherwise the failure is explicit.
            match found.carrier.state {
                ReadState::Available => assert!(matches!(found.carrier.value.as_str(), "0" | "1"), "{name}"),
                other => assert_ne!(other, ReadState::NotApplicable, "{name}: {:?}", found.carrier),
            }
            if found.carrier.value == "0" {
                assert_eq!(found.speed.state, ReadState::NotApplicable, "{name}");
            }
        }
    }
}
