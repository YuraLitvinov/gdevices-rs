//! On-demand device properties, ported from `src/devices/deviceproperties.cpp`.
//!
//! One read-only, bounded pass over a single device directory: selected udev metadata, the
//! documented per-subsystem sysfs attributes, direct driver/module facts, PCIe link data, USB
//! interfaces, battery values, resources and (for UEFI records) the file bytes. Nothing here
//! writes, escalates privileges or queries hardware beyond sysfs. Storage, network, display and
//! camera groups are filled by their own modules.
use crate::camera::{collect_camera_properties, default_helper_path, CameraProperties};
use crate::dirfd::{available, failure, file_name, io_errno, read_limited, Dir, TEXT_LIMIT};
use crate::display::{collect_display_properties, DisplayProperties};
use crate::ethernet::collect_ethernet_properties;
use crate::model::{Attribute, Device, ReadState};
use crate::modulenames::{module_details, ModuleLookup};
use crate::monitor_edid::{collect_monitor_properties, MonitorProperties};
use crate::network::{collect_network_properties, NetworkProperties};
use crate::resources::{collect_device_resources, DeviceResources};
use crate::storage::{collect_storage_properties, StorageProperties};
use crate::sysfs::bounded_directory_names;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::sync::atomic::{AtomicBool, Ordering};

const EFI_LIMIT: usize = 65536;

/// USB-specific values stay separate from generic Details metadata.
#[derive(Clone, Debug, Default)]
pub struct UsbInterfaceProperties {
    pub name: String,
    pub path: String,
    pub values: HashMap<String, Attribute>,
}

#[derive(Clone, Debug, Default)]
pub struct UsbProperties {
    pub values: HashMap<String, Attribute>,
    pub interfaces: Vec<UsbInterfaceProperties>,
    pub interface_state: Attribute,
}

#[derive(Clone, Debug, Default)]
pub struct BatteryProperties {
    pub applicable: bool,
    pub values: HashMap<String, Attribute>,
    pub state: Attribute,
}

/// Owned raw results. Display labels, translations and hex formatting belong to the UI side.
#[derive(Clone, Debug, Default)]
pub struct DeviceProperties {
    pub device: Device,
    pub values: HashMap<String, Attribute>,
    pub sources: HashMap<String, String>,
    pub resources: DeviceResources,
    pub storage: StorageProperties,
    pub network: Vec<NetworkProperties>,
    pub network_state: Attribute,
    pub display: DisplayProperties,
    pub monitor: MonitorProperties,
    pub camera: CameraProperties,
    pub usb: UsbProperties,
    pub battery: BatteryProperties,
    /// Evidence from this PCI device's link attributes only.
    pub pcie_applicable: bool,
    /// Numeric PCI Express device/port type; no GPU-id inference.
    pub pcie_type: Attribute,
    pub pcie_type_source: String,
    pub pcie_type_requires_root: bool,
    pub efi_bytes: Vec<u8>,
    pub efi_truncated: bool,
    pub state: ReadState,
    pub error: i32,
    pub request: u64,
}

fn inode_identity(info: &std::fs::Metadata) -> String {
    format!("{}:{}", info.dev(), info.ino())
}

fn put(result: &mut DeviceProperties, id: &str, value: Attribute, source: impl Into<String>) {
    result.values.insert(id.to_string(), value);
    result.sources.insert(id.to_string(), source.into());
}

fn bounded_text(value: Option<&OsStr>) -> Attribute {
    match value {
        None => Attribute::default(),
        Some(v) if v.len() > TEXT_LIMIT => failure(libc::EOVERFLOW),
        Some(v) => available(v.to_string_lossy().into_owned()),
    }
}

fn unsupported_if_unimplemented(value: &mut Attribute) {
    if value.error == libc::EOPNOTSUPP || value.error == libc::ENOSYS {
        value.state = ReadState::Unsupported;
    }
}

fn same(a: &Attribute, b: &Attribute) -> bool {
    a == b
}

fn sysfs(result: &mut DeviceProperties, dir: &Dir, key: &str) {
    let source = format!("{}/{key}", result.device.path);
    put(result, &format!("sysfs/{key}"), dir.read_text(key), source);
}

// ---------------------------------------------------------------------------------------
// PCI Express
// ---------------------------------------------------------------------------------------

const PCI_STATUS: i64 = 0x06;
const PCI_STATUS_CAP_LIST: u32 = 0x10;
const PCI_HEADER_TYPE: i64 = 0x0e;
const PCI_CAPABILITY_LIST: i64 = 0x34;
const PCI_CB_CAPABILITY_LIST: i64 = 0x14;
const PCI_CAP_ID_EXP: u32 = 0x10;
const PCI_EXP_FLAGS: u32 = 2;
const PCI_EXP_FLAGS_VERS: u32 = 0x000f;
const PCI_EXP_FLAGS_TYPE: u32 = 0x00f0;

/// Finds the PCI Express device/port type by walking the standard capability list through
/// `read(offset, size)`. At most 48 aligned entries in bytes 0x40..0xff; no extended-space scan,
/// payload dump, BAR access or write.
fn pcie_type_from(read: &dyn Fn(i64, usize) -> Result<u32, i32>, source: &mut String) -> Attribute {
    let value = |offset, size| read(offset, size).map_err(failure);
    let status = match value(PCI_STATUS, 2) {
        Ok(v) => v,
        Err(a) => return a,
    };
    if status == 0xffff {
        return failure(libc::ENODEV);
    }
    if status & PCI_STATUS_CAP_LIST == 0 {
        return failure(libc::ENODATA);
    }
    let header = match value(PCI_HEADER_TYPE, 1) {
        Ok(v) => v & 0x7f,
        Err(a) => return a,
    };
    if header > 2 {
        return failure(libc::EPROTO);
    }
    let mut pointer = match value(if header == 2 { PCI_CB_CAPABILITY_LIST } else { PCI_CAPABILITY_LIST }, 1) {
        Ok(v) => v,
        Err(a) => return a,
    };
    let mut visited = [false; 256];
    for _ in 0..48 {
        if pointer == 0 {
            break;
        }
        if !(0x40..=0xfc).contains(&pointer) || pointer & 3 != 0 || visited[pointer as usize] {
            return failure(libc::EPROTO);
        }
        visited[pointer as usize] = true;
        let capability = match value(pointer as i64, 2) {
            Ok(v) => v,
            Err(a) => return a,
        };
        if capability & 0xff == PCI_CAP_ID_EXP {
            let flags = match value((pointer + PCI_EXP_FLAGS) as i64, 2) {
                Ok(v) => v,
                Err(a) => return a,
            };
            if flags == 0xffff || flags & PCI_EXP_FLAGS_VERS == 0 {
                return failure(libc::EPROTO);
            }
            source.push_str(&format!("; PCI Express flags at offset 0x{:02x}", pointer + PCI_EXP_FLAGS));
            return available(((flags & PCI_EXP_FLAGS_TYPE) >> 4).to_string());
        }
        pointer = capability >> 8;
    }
    failure(if pointer != 0 { libc::EPROTO } else { libc::ENODATA })
}

fn pcie_device_type(dir: &Dir, source: &mut String, cancel: Option<&AtomicBool>) -> Attribute {
    // Ordinary sysfs link reads remain usable without administrative privileges. Never
    // escalate or attempt a configuration-space read for a normal user.
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return failure(libc::EACCES);
    }
    let config = match dir.open_file("config") {
        Ok(f) => f,
        Err(e) => return failure(io_errno(&e)),
    };
    let read = |offset: i64, size: usize| -> Result<u32, i32> {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Err(libc::ECANCELED);
        }
        let mut bytes = [0u8; 2];
        match config.read_at(&mut bytes[..size], offset as u64) {
            Err(e) => Err(io_errno(&e)),
            Ok(n) if n != size => Err(libc::EMSGSIZE),
            Ok(_) => Ok(bytes[0] as u32 | if size == 2 { (bytes[1] as u32) << 8 } else { 0 }),
        }
    };
    pcie_type_from(&read, source)
}

fn pcie_properties(result: &mut DeviceProperties, dir: &Dir, cancel: Option<&AtomicBool>) {
    if result.device.subsystem != "pci" {
        return;
    }
    let mut values = Vec::new();
    let mut applicable = false;
    for key in ["current_link_speed", "current_link_width", "max_link_speed", "max_link_width"] {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return;
        }
        let mut value = dir.read_text(key);
        // Absent files alone do not establish PCIe applicability. Preserve actual read
        // failures so a present but unreadable PCIe device still has a tab.
        if value.state == ReadState::Available || value.error != libc::ENOENT {
            applicable = true;
        }
        if value.error == libc::EOPNOTSUPP || value.error == libc::ENOSYS {
            value.state = ReadState::Unsupported;
        }
        values.push((key, value));
    }
    if !applicable {
        return;
    }
    result.pcie_applicable = true;
    for (key, value) in values {
        let source = format!("{}/{key}", result.device.path);
        put(result, &format!("sysfs/{key}"), value, source);
    }
    result.pcie_type_source = format!("{}/config", result.device.path);
    // SAFETY: geteuid has no preconditions.
    result.pcie_type_requires_root = unsafe { libc::geteuid() } != 0;
    let mut source = std::mem::take(&mut result.pcie_type_source);
    result.pcie_type = pcie_device_type(dir, &mut source, cancel);
    result.pcie_type_source = source;
}

// ---------------------------------------------------------------------------------------
// Battery
// ---------------------------------------------------------------------------------------

fn battery_properties(result: &mut DeviceProperties, dir: &Dir, cancel: Option<&AtomicBool>) {
    if result.device.subsystem != "power_supply" {
        return;
    }
    let mut kind = result.values.get("sysfs/type").cloned().unwrap_or_default();
    unsupported_if_unimplemented(&mut kind);
    let previous = result.device.attributes.get("type").cloned().unwrap_or_default();
    // Inventory evidence controls only whether a failed snapshot gets a tab. It must not
    // replace an unreadable current type or supply battery values.
    if kind.state == ReadState::Available && kind.value != "Battery" {
        return;
    }
    if kind.state != ReadState::Available && (previous.state != ReadState::Available || previous.value != "Battery") {
        return;
    }
    let battery = &mut result.battery;
    battery.applicable = true;
    battery.values.insert("type".into(), kind.clone());
    if kind.state != ReadState::Available {
        battery.state = kind;
        return;
    }
    battery.state = available("");
    for key in ["manufacturer", "model_name", "serial_number", "present"] {
        battery.values.insert(key.into(), result.values.get(&format!("sysfs/{key}")).cloned().unwrap_or_default());
    }
    let present = battery.values["present"].clone();
    if present.state == ReadState::Available && present.value != "0" && present.value != "1" {
        battery.state = failure(libc::EINVAL);
        return;
    }
    let absent = present.state == ReadState::Available && present.value == "0";
    for key in [
        "status",
        "technology",
        "cycle_count",
        "capacity",
        "voltage_now",
        "energy_full_design",
        "energy_full",
        "energy_now",
        "charge_full_design",
        "charge_full",
        "charge_now",
        "current_now",
        "power_now",
    ] {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            battery.state = failure(libc::ECANCELED);
            battery.values.clear();
            return;
        }
        let mut value = if absent { Attribute { state: ReadState::NotApplicable, ..Default::default() } } else { dir.read_text(key) };
        unsupported_if_unimplemented(&mut value);
        battery.values.insert(key.into(), value);
    }
    // A power_supply node can remain while a removable battery disappears. Reject changed
    // presence/type evidence rather than presenting mixed values.
    let after_type = dir.read_text("type");
    let after_present = dir.read_text("present");
    if !same(&kind, &after_type) || !same(&present, &after_present) {
        battery.values.clear();
        battery.state = if after_type.state != ReadState::Available {
            after_type
        } else if !same(&present, &after_present) && after_present.state != ReadState::Available {
            after_present
        } else {
            failure(libc::EAGAIN)
        };
        unsupported_if_unimplemented(&mut battery.state);
    }
}

// ---------------------------------------------------------------------------------------
// USB
// ---------------------------------------------------------------------------------------

fn usb_properties(result: &mut DeviceProperties, dir: &Dir, cancel: Option<&AtomicBool>) {
    let device = result.device.clone();
    if device.subsystem != "usb" || device.devtype != "usb_device" {
        return;
    }
    let usb = &mut result.usb;
    for key in ["bConfigurationValue", "bNumConfigurations", "configuration", "bNumInterfaces", "bmAttributes", "bMaxPower"] {
        usb.values.insert(key.into(), dir.read_text(key));
    }
    let configuration = usb.values["bConfigurationValue"].clone();
    let unconfigured = configuration.state == ReadState::Available && (configuration.value.is_empty() || configuration.value == "0");
    let not_applicable = Attribute { state: ReadState::NotApplicable, ..Default::default() };
    if unconfigured {
        for key in ["configuration", "bNumInterfaces", "bmAttributes", "bMaxPower"] {
            usb.values.insert(key.into(), not_applicable.clone());
        }
        usb.interface_state = not_applicable.clone();
    }
    // Only direct USB interface children, never descendant devices behind hubs.
    let names = if unconfigured { Ok(Vec::new()) } else { bounded_directory_names(&device.path, 256, &[&format!("{}:*", device.sysname)], cancel) };
    if !unconfigured {
        usb.interface_state = match &names {
            Err(error) => failure(*error),
            Ok(_) => available(""),
        };
    }
    let listed = names.clone().unwrap_or_default();
    for name in &listed {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            usb.interfaces.clear();
            usb.interface_state = failure(libc::ECANCELED);
            break;
        }
        let mut interface = UsbInterfaceProperties { name: name.clone(), path: format!("{}/{name}", device.path), values: HashMap::new() };
        match dir.open_dir(name, false).and_then(|child| child.metadata().map(|m| (child, m))) {
            Err(e) => {
                interface.values.insert("state".into(), failure(io_errno(&e)));
            }
            Ok((child, before)) => {
                let driver = child.link_target("driver");
                for key in ["bInterfaceNumber", "bAlternateSetting", "bInterfaceClass", "bInterfaceSubClass", "bInterfaceProtocol", "interface"] {
                    interface.values.insert(key.into(), child.read_text(key));
                }
                let mut binding = driver.clone();
                if binding.state == ReadState::Available {
                    binding.value = file_name(&binding.value);
                } else if binding.state == ReadState::Unavailable && binding.error == libc::ENOENT {
                    binding = available("");
                }
                interface.values.insert("driver".into(), binding);
                let after_driver = child.link_target("driver");
                let after_setting = child.read_text("bAlternateSetting");
                let setting = interface.values["bAlternateSetting"].clone();
                match dir.lstat(name) {
                    Err(e) => {
                        interface.values.clear();
                        interface.values.insert("state".into(), failure(io_errno(&e)));
                    }
                    Ok(after) => {
                        if inode_identity(&before) != inode_identity(&after) || !same(&driver, &after_driver) || !same(&setting, &after_setting) {
                            interface.values.clear();
                            interface.values.insert("state".into(), failure(libc::EAGAIN));
                        }
                    }
                }
            }
        }
        usb.interfaces.push(interface);
    }
    // A listing failure must not become a successful empty/partial result.
    let count = usb.values["bNumInterfaces"].clone();
    let expected = count.value.parse::<u32>().ok();
    if !unconfigured && names.is_ok() && listed.is_empty() && count.state != ReadState::Available {
        usb.interface_state = count.clone();
    }
    if !unconfigured && names.is_ok() && count.state == ReadState::Available {
        let consistent = expected.is_some_and(|e| e <= 255 && e as usize == listed.len());
        if !consistent {
            usb.interfaces.clear();
            usb.interface_state = failure(if expected.is_some_and(|e| e <= 255) { libc::EAGAIN } else { libc::EINVAL });
        }
    }
    // Configuration changes invalidate both its declarations and interface list.
    let after = dir.read_text("bConfigurationValue");
    if !same(&configuration, &after) {
        usb.interfaces.clear();
        let changed = failure(libc::EAGAIN);
        for value in usb.values.values_mut() {
            *value = changed.clone();
        }
        usb.interface_state = changed;
    }
}

// ---------------------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------------------

/// Reads one interface directory. Address, MAC and (for non-wireless Ethernet) link data are
/// taken for the same ifindex; if the index changes or the device vanishes while reading, every
/// value is replaced by the reason rather than mixing two interfaces.
fn network_interface(dir: &Dir, path: &str, name: &str, ethernet_details: bool, cancel: Option<&AtomicBool>) -> NetworkProperties {
    let index = dir.read_text("ifindex");
    let link_type = dir.read_text("type");
    let number = index.value.parse::<u32>().ok().filter(|n| *n != 0);
    let usable_index = index.state == ReadState::Available && number.is_some();
    let is_ether = link_type.state == ReadState::Available && link_type.value == "1";
    let mut result = match number.filter(|_| usable_index) {
        Some(number) => collect_network_properties(name, number, is_ether, cancel),
        None => {
            let reason = if index.state == ReadState::Available { failure(libc::EINVAL) } else { index.clone() };
            NetworkProperties { address_state: reason.clone(), permanent_address: reason, ..Default::default() }
        }
    };
    result.name = name.to_string();
    result.path = path.to_string();
    result.link_type = link_type.clone();
    if link_type.state != ReadState::Available {
        result.permanent_address = link_type;
    }
    result.current_address = dir.read_text("address");
    result.assignment_type = dir.read_text("addr_assign_type");
    result.mtu = dir.read_text("mtu");
    if ethernet_details && is_ether {
        // ARPHRD_ETHER describes MAC framing and includes Wi-Fi. Check both kernel wireless
        // markers before issuing Ethernet-only ethtool queries.
        let mut wireless = false;
        let mut scope_error = 0;
        for marker in ["phy80211", "wireless"] {
            match dir.lstat(marker) {
                Ok(_) => wireless = true,
                Err(e) if io_errno(&e) != libc::ENOENT => scope_error = io_errno(&e),
                Err(_) => {}
            }
        }
        if !wireless && scope_error != 0 {
            result.ethernet.scope = failure(scope_error);
            result.ethernet.source = format!("{path}/phy80211; {path}/wireless");
        } else if !wireless && usable_index && !cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            result.ethernet = collect_ethernet_properties(name, number.expect("usable"), cancel);
        }
    }
    let after = dir.read_text("ifindex");
    let index_changed = index.state == ReadState::Available && (after.state != ReadState::Available || after.value != index.value);
    let removed = |a: &Attribute| a.state == ReadState::Removed;
    if index_changed || removed(&result.address_state) || removed(&result.permanent_address) || removed(&result.ethernet.scope) {
        result.addresses.clear();
        let reason = if index_changed {
            if after.state != ReadState::Available {
                after
            } else {
                failure(libc::EAGAIN)
            }
        } else {
            failure(libc::ENODEV)
        };
        result.address_state = reason.clone();
        result.permanent_address = reason.clone();
        result.current_address = reason.clone();
        result.assignment_type = reason.clone();
        result.mtu = reason.clone();
        if result.ethernet.applicable || result.ethernet.scope.state != ReadState::NotApplicable {
            result.ethernet.invalidate(&reason);
        }
    }
    result
}

fn network_properties(result: &mut DeviceProperties, dir: &Dir, ethernet_details: bool, cancel: Option<&AtomicBool>) {
    let device = result.device.clone();
    if device.subsystem == "net" {
        result.network.push(network_interface(dir, &device.path, &device.sysname, ethernet_details, cancel));
        result.network_state = available("");
        return;
    }
    if device.category != "network" {
        return;
    }
    // A single net interface is represented by its PCI function in the device tree. Read only
    // that function's direct net directory, never ancestor ports.
    let directory = match dir.open_dir("net", false) {
        Ok(d) => d,
        Err(e) => {
            result.network_state = failure(io_errno(&e));
            return;
        }
    };
    let path = format!("{}/net", device.path);
    let names = match bounded_directory_names(&path, 256, &[], cancel) {
        Ok(names) => names,
        Err(error) => {
            result.network_state = failure(error);
            return;
        }
    };
    result.network_state = available("");
    for name in names {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            break;
        }
        let interface_path = format!("{path}/{name}");
        match directory.open_dir(&name, false) {
            Err(e) => {
                let error = failure(io_errno(&e));
                result.network.push(NetworkProperties {
                    name,
                    path: interface_path,
                    address_state: error.clone(),
                    permanent_address: error.clone(),
                    current_address: error.clone(),
                    assignment_type: error.clone(),
                    link_type: error.clone(),
                    mtu: error,
                    ..Default::default()
                });
            }
            Ok(interface) => result.network.push(network_interface(&interface, &interface_path, &name, ethernet_details, cancel)),
        }
    }
}

// ---------------------------------------------------------------------------------------
// Driver / module
// ---------------------------------------------------------------------------------------

const MODULE_FIELDS: [&str; 6] = ["filename", "version", "description", "author", "license", "firmware"];

fn module_metadata(result: &mut DeviceProperties, driver: &Dir, cancel: Option<&AtomicBool>) {
    let mut owner = driver.link_target("module");
    // ENOENT is absent evidence, not evidence of a built-in driver.
    if owner.state == ReadState::Available {
        owner.value = file_name(&owner.value);
    }
    let driver_path = result.values.get("driver/path").map(|a| a.value.clone()).unwrap_or_default();
    put(result, "module/name", owner.clone(), format!("{driver_path}/module"));
    for key in MODULE_FIELDS {
        put(result, &format!("module/{key}"), Attribute::default(), "installed module metadata");
    }
    put(result, "module/type", Attribute::default(), "kernel module initstate");
    put(result, "module/runtime_version", Attribute::default(), "kernel module version");
    if owner.state != ReadState::Available || owner.value.is_empty() {
        return;
    }
    match driver.open_dir("module", true) {
        Ok(module) => {
            put(result, "module/runtime_version", module.read_text("version"), format!("/sys/module/{}/version", owner.value));
            let state = module.read_text("initstate");
            let source = format!("/sys/module/{}/initstate", owner.value);
            put(result, "module/type", if state.state == ReadState::Available { Attribute::default() } else { state.clone() }, source.clone());
            if state.state == ReadState::Available && matches!(state.value.as_str(), "live" | "coming" | "going") {
                put(result, "module/type", available("modular"), source);
            }
        }
        Err(e) => {
            let error = failure(io_errno(&e));
            put(result, "module/runtime_version", error.clone(), "kernel module version");
            put(result, "module/type", error, "kernel module initstate");
        }
    }
    let all_fields = |result: &mut DeviceProperties, error: Attribute, skip_filename: bool| {
        for key in MODULE_FIELDS {
            if skip_filename && key == "filename" {
                continue;
            }
            result.values.insert(format!("module/{key}"), error.clone());
        }
    };
    match module_details(&owner.value) {
        ModuleLookup::Unsupported => {
            for key in MODULE_FIELDS {
                result.values.entry(format!("module/{key}")).or_default().state = ReadState::Unsupported;
            }
        }
        ModuleLookup::Failed(error) => all_fields(result, failure(error), false),
        ModuleLookup::Missing => {}
        ModuleLookup::Found(details) => {
            put(result, "module/filename", details.filename.clone(), "libkmod: running-kernel installed module index");
            match details.info {
                Err(error) => all_fields(result, failure(error), true),
                Ok(entries) => {
                    for (examined, (key, value)) in entries.into_iter().enumerate() {
                        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                            break;
                        }
                        if examined >= 4096 {
                            all_fields(result, failure(libc::EOVERFLOW), true);
                            break;
                        }
                        if key.state != ReadState::Available || key.value == "filename" || !MODULE_FIELDS.contains(&key.value.as_str()) {
                            continue;
                        }
                        let id = format!("module/{}", key.value);
                        let previous = result.values.get(&id).cloned().unwrap_or_default();
                        if previous.state == ReadState::Error {
                            continue;
                        }
                        let mut value = value;
                        if value.state == ReadState::Available && previous.state == ReadState::Available {
                            value.value = format!("{}\n{}", previous.value, value.value);
                            if value.value.len() > TEXT_LIMIT {
                                value = failure(libc::EOVERFLOW);
                            }
                        }
                        let source = if details.filename.state == ReadState::Available {
                            format!("{}: .modinfo/{}", details.filename.value, key.value)
                        } else {
                            format!("libkmod: installed metadata/{}", key.value)
                        };
                        put(result, &id, value, source);
                    }
                    if details.too_many {
                        all_fields(result, failure(libc::EOVERFLOW), true);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// Collection
// ---------------------------------------------------------------------------------------

fn removed_or(error: i32) -> ReadState {
    if error == libc::ENOENT || error == libc::ENODEV {
        ReadState::Removed
    } else {
        failure(error).state
    }
}

const UDEV_KEYS: [&str; 25] = [
    "ID_VENDOR_FROM_DATABASE",
    "ID_VENDOR",
    "ID_MODEL_FROM_DATABASE",
    "ID_MODEL",
    "ID_BUS",
    "ID_PATH",
    "ID_PATH_TAG",
    "ID_SERIAL",
    "ID_SERIAL_SHORT",
    "ID_REVISION",
    "ID_VENDOR_ID",
    "ID_MODEL_ID",
    "PCI_ID",
    "PCI_SUBSYS_ID",
    "PCI_CLASS",
    "PCI_SLOT_NAME",
    "PRODUCT",
    "PHYS",
    "MODALIAS",
    "DEVNAME",
    "DEVTYPE",
    "DRIVER",
    "ID_NET_DRIVER",
    "ID_USB_DRIVER",
    "ID_FIRMWARE_FROM_DATABASE",
];

fn subsystem_attributes(d: &Device) -> &'static [&'static str] {
    match d.subsystem.as_str() {
        "pci" => &["vendor", "device", "subsystem_vendor", "subsystem_device", "class", "revision", "modalias"],
        "usb" => &["idVendor", "idProduct", "bcdDevice", "manufacturer", "product", "serial", "authorized", "busnum", "devnum", "version", "speed", "modalias"],
        "input" => &["name", "phys", "uniq", "modalias"],
        "nvme" => &["model", "serial", "firmware_rev", "state"],
        "power_supply" => &["type", "manufacturer", "model_name", "serial_number", "present"],
        "net" => &["address", "addr_assign_type", "type", "mtu", "ifindex", "operstate", "flags"],
        "drm" if d.sysname.contains('-') => &["status", "enabled"],
        "hdaudio" => &["vendor_name", "chip_name", "vendor_id", "subsystem_id", "revision_id"],
        "acpi" => &["hid", "uid", "path", "modalias"],
        "pnp" | "platform" | "hid" => &["modalias"],
        "video4linux" => &["name", "index"],
        "sound" if d.sysname.starts_with("card") => &["id"],
        _ => &[],
    }
}

/// Collects the properties of `device`. `cached_storage_service` is false for deep search,
/// which skips desktop-session and service-backed groups. Results are always an owned
/// snapshot; the state says whether the instance was verified still present.
pub fn collect_device_properties(device: &Device, cached_storage_service: bool, cancel: Option<&AtomicBool>) -> DeviceProperties {
    let mut result = DeviceProperties { device: device.clone(), ..Default::default() };
    let d = device;
    let cancelled = || cancel.is_some_and(|c| c.load(Ordering::Relaxed));
    let efi = d.subsystem == "efivarfs";
    if efi {
        match std::fs::symlink_metadata(&d.path) {
            Err(e) => {
                result.error = io_errno(&e);
                result.state = removed_or(result.error);
                return result;
            }
            Ok(entry) => {
                if inode_identity(&entry) != d.incarnation {
                    result.state = ReadState::Removed;
                    return result;
                }
                if !entry.file_type().is_file() || entry.mode() & 0o004 == 0 {
                    result.state = ReadState::PermissionDenied;
                    result.error = libc::EACCES;
                    return result;
                }
            }
        }
    }
    // The device directory (or, for UEFI records, the file) is opened once and every further
    // read is anchored to it.
    let mut efi_file = None;
    let mut dir = None;
    let before = if efi {
        match std::fs::OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW).open(&d.path) {
            Ok(file) => {
                let info = file.metadata();
                efi_file = Some(file);
                info
            }
            Err(e) => Err(e),
        }
    } else {
        match Dir::open(&d.path) {
            Ok(opened) => {
                let info = opened.metadata();
                dir = Some(opened);
                info
            }
            Err(e) => Err(e),
        }
    };
    let before = match before {
        Ok(info) => info,
        Err(e) => {
            result.error = io_errno(&e);
            result.state = removed_or(result.error);
            return result;
        }
    };
    let identity = inode_identity(&before);
    let same_identity = if efi { identity == d.incarnation } else { d.incarnation.starts_with(&format!("{identity}:")) };
    if !same_identity {
        result.state = ReadState::Removed;
        return result;
    }
    result.state = ReadState::Available;
    if let Some(mut file) = efi_file {
        if !before.file_type().is_file() || before.mode() & 0o004 == 0 {
            result.state = ReadState::PermissionDenied;
            result.error = libc::EACCES;
            return result;
        }
        match read_limited(&mut file, EFI_LIMIT, &cancelled) {
            Err(error) => put(&mut result, "efi/hex", failure(error), "efivarfs: complete file bytes"),
            Ok(mut bytes) => {
                result.efi_truncated = bytes.len() > EFI_LIMIT;
                bytes.truncate(EFI_LIMIT);
                result.efi_bytes = bytes;
                put(&mut result, "efi/hex", available(""), "efivarfs: complete file bytes, no decoding");
            }
        }
    } else if let Some(dir) = dir {
        let Ok(raw) = udev::Device::from_syspath(std::path::Path::new(&d.path)) else {
            result.state = ReadState::Error;
            result.error = libc::EIO;
            return result;
        };
        let stamp = bounded_text(raw.property_value("USEC_INITIALIZED")).value;
        if format!("{identity}:{stamp}") != d.incarnation {
            result.state = ReadState::Removed;
            return result;
        }
        // Selected udev metadata only; no unbounded property dump.
        for key in UDEV_KEYS {
            let value = bounded_text(raw.property_value(key));
            if value.state != ReadState::Unavailable {
                put(&mut result, &format!("udev/{key}"), value, format!("udev: {key} on {}", d.path));
            }
        }
        let driver_link = dir.link_target("driver");
        let mut driver_path = driver_link.clone();
        if driver_path.state == ReadState::Available {
            let target = std::path::Path::new(&driver_path.value);
            let joined = if target.is_absolute() { target.to_path_buf() } else { std::path::Path::new(&d.path).join(target) };
            driver_path.value = clean_path(&joined.to_string_lossy());
        }
        put(&mut result, "driver/path", driver_path.clone(), format!("{}/driver", d.path));
        let mut binding = driver_link.clone();
        if binding.state == ReadState::Available {
            binding.value = file_name(&binding.value);
        } else if binding.state == ReadState::Unavailable && binding.error == libc::ENOENT {
            binding = available("");
        }
        put(&mut result, "driver/name", binding, format!("{}/driver", d.path));
        if driver_link.state == ReadState::Available {
            // Follow this one documented kernel link, never a guessed parent driver.
            match dir.open_dir("driver", true) {
                Ok(driver) => {
                    let subsystem = dir.link_target("subsystem");
                    let bus = if subsystem.state == ReadState::Available { available(file_name(&subsystem.value)) } else { subsystem };
                    put(&mut result, "driver/bus", bus, format!("{}/subsystem", d.path));
                    module_metadata(&mut result, &driver, cancel);
                }
                Err(e) => put(&mut result, "module/name", failure(io_errno(&e)), format!("{}/module", driver_path.value)),
            }
        }
        for key in subsystem_attributes(d) {
            if cancelled() {
                break;
            }
            sysfs(&mut result, &dir, key);
        }
        if cached_storage_service && !cancelled() {
            pcie_properties(&mut result, &dir, cancel);
        }
        if !cancelled() {
            result.resources = collect_device_resources(dir.file(), d, cancel);
        }
        if !cancelled() {
            result.storage = collect_storage_properties(d, cached_storage_service, cancel);
        }
        if cached_storage_service && d.subsystem == "video4linux" && d.sysname.starts_with("video") && !cancelled() {
            let node = bounded_text(raw.devnode().map(|p| p.as_os_str())).value;
            result.camera = collect_camera_properties(&dir, &d.path, &node, &default_helper_path(), cancel);
        }
        // Desktop-session queries belong to explicit properties requests, not deep search.
        if cached_storage_service && d.subsystem == "drm" && d.sysname.contains('-') && !cancelled() {
            result.monitor = collect_monitor_properties(&dir, &d.path, cancel);
            if !cancelled() {
                result.display = collect_display_properties(d, &dir.read_text("status"), cancel);
            }
        }
        if !cancelled() {
            network_properties(&mut result, &dir, cached_storage_service, cancel);
        }
        if cached_storage_service && !cancelled() {
            usb_properties(&mut result, &dir, cancel);
        }
        if cached_storage_service && !cancelled() {
            battery_properties(&mut result, &dir, cancel);
        }
        // Binding can change during a read even if the device instance remains.
        let after_link = dir.link_target("driver");
        if !same(&after_link, &driver_link) {
            result.state = ReadState::Error;
            result.error = libc::EAGAIN;
        }
        let _ = dir.file().as_raw_fd();
    }
    match std::fs::symlink_metadata(&d.path) {
        Err(e) => {
            result.error = io_errno(&e);
            result.state = removed_or(result.error);
        }
        Ok(after) => {
            if inode_identity(&after) != identity {
                result.state = ReadState::Removed;
            } else if efi && after.mode() & 0o004 == 0 {
                result.state = ReadState::PermissionDenied;
                result.error = libc::EACCES;
                result.efi_bytes.clear();
            }
        }
    }
    if cancelled() {
        result.state = ReadState::Error;
        result.error = libc::ECANCELED;
    }
    result
}

/// `QDir::cleanPath` for absolute paths: collapses `.`, `..` and repeated separators.
fn clean_path(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    format!("/{}", parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::path::Path;

    fn write(dir: &Path, name: &str, text: &str) {
        fs::write(dir.join(name), text).unwrap();
    }

    fn fixture_properties(device: Device) -> DeviceProperties {
        DeviceProperties { device, ..Default::default() }
    }

    #[test]
    fn clean_path_collapses_relative_driver_links() {
        assert_eq!(clean_path("/sys/devices/pci0000:00/0000:00:14.3/../../../bus/pci/drivers/iwlwifi"), "/sys/bus/pci/drivers/iwlwifi");
        assert_eq!(clean_path("/a//b/./c"), "/a/b/c");
    }

    fn pcie_config(bytes: Vec<u8>) -> impl Fn(i64, usize) -> Result<u32, i32> {
        move |offset, size| {
            let o = offset as usize;
            if o + size > bytes.len() {
                return Err(libc::EMSGSIZE);
            }
            Ok(bytes[o] as u32 | if size == 2 { (bytes[o + 1] as u32) << 8 } else { 0 })
        }
    }

    #[test]
    fn pcie_type_follows_the_capability_list() {
        let mut config = vec![0u8; 256];
        config[0x06] = 0x10; // status: capability list present
        config[0x0e] = 0x00; // header type 0
        config[0x34] = 0x40; // first capability
        config[0x40] = 0x01; // power management, next 0x50
        config[0x41] = 0x50;
        config[0x50] = 0x10; // PCI Express, end of list
        config[0x52] = 0x02 | (4 << 4); // version 2, type 4 (root port)
        let mut source = "cfg".to_string();
        let found = pcie_type_from(&pcie_config(config.clone()), &mut source);
        assert_eq!((found.state, found.value.as_str()), (ReadState::Available, "4"));
        assert!(source.ends_with("offset 0x52"));

        let mut loop_config = config.clone();
        loop_config[0x51] = 0x40; // 0x50 -> 0x40 -> 0x50 ...
        loop_config[0x50] = 0x01;
        loop_config[0x41] = 0x50;
        assert_eq!(pcie_type_from(&pcie_config(loop_config), &mut source).error, libc::EPROTO, "cycle rejected");

        let mut no_caps = config.clone();
        no_caps[0x06] = 0;
        assert_eq!(pcie_type_from(&pcie_config(no_caps), &mut source).error, libc::ENODATA);
        let mut gone = config.clone();
        gone[0x06] = 0xff;
        gone[0x07] = 0xff;
        assert_eq!(pcie_type_from(&pcie_config(gone), &mut source).state, ReadState::Removed);
        let mut misaligned = config;
        misaligned[0x34] = 0x42;
        assert_eq!(pcie_type_from(&pcie_config(misaligned), &mut source).error, libc::EPROTO);
    }

    #[test]
    fn battery_values_and_absence() {
        let tmp = tempfile::tempdir().unwrap();
        for (k, v) in [("type", "Battery"), ("present", "1"), ("manufacturer", "ACME"), ("capacity", "87\n"), ("status", "Charging")] {
            write(tmp.path(), k, v);
        }
        let dir = Dir::open(tmp.path()).unwrap();
        let mut result = fixture_properties(Device { subsystem: "power_supply".into(), ..Default::default() });
        for key in ["type", "present", "manufacturer"] {
            sysfs(&mut result, &dir, key);
        }
        battery_properties(&mut result, &dir, None);
        let battery = &result.battery;
        assert!(battery.applicable && battery.state.state == ReadState::Available);
        assert_eq!(battery.values["capacity"].value, "87");
        assert_eq!(battery.values["voltage_now"].state, ReadState::Unavailable, "missing file is unavailable, not zero");

        write(tmp.path(), "present", "0");
        let mut absent = fixture_properties(Device { subsystem: "power_supply".into(), ..Default::default() });
        for key in ["type", "present"] {
            sysfs(&mut absent, &dir, key);
        }
        battery_properties(&mut absent, &dir, None);
        assert_eq!(absent.battery.values["capacity"].state, ReadState::NotApplicable);

        write(tmp.path(), "present", "maybe");
        let mut invalid = fixture_properties(Device { subsystem: "power_supply".into(), ..Default::default() });
        for key in ["type", "present"] {
            sysfs(&mut invalid, &dir, key);
        }
        battery_properties(&mut invalid, &dir, None);
        assert_eq!(invalid.battery.state.error, libc::EINVAL);

        write(tmp.path(), "type", "Mains");
        let mut mains = fixture_properties(Device { subsystem: "power_supply".into(), ..Default::default() });
        sysfs(&mut mains, &dir, "type");
        battery_properties(&mut mains, &dir, None);
        assert!(!mains.battery.applicable, "only batteries get battery values");
    }

    #[test]
    fn usb_interfaces_are_direct_children_and_validated() {
        let tmp = tempfile::tempdir().unwrap();
        let device_dir = tmp.path().join("1-2");
        fs::create_dir(&device_dir).unwrap();
        for (k, v) in [("bConfigurationValue", "1"), ("bNumInterfaces", "1"), ("bNumConfigurations", "1")] {
            write(&device_dir, k, v);
        }
        let iface = device_dir.join("1-2:1.0");
        fs::create_dir(&iface).unwrap();
        for (k, v) in [("bInterfaceNumber", "00"), ("bAlternateSetting", " 0"), ("bInterfaceClass", "03")] {
            write(&iface, k, v);
        }
        symlink("../../../drivers/usbhid", iface.join("driver")).unwrap();
        fs::create_dir(device_dir.join("1-2.3")).unwrap(); // a child device behind a hub
        let device = Device {
            path: device_dir.to_string_lossy().into(),
            sysname: "1-2".into(),
            subsystem: "usb".into(),
            devtype: "usb_device".into(),
            ..Default::default()
        };
        let dir = Dir::open(&device_dir).unwrap();
        let mut result = fixture_properties(device.clone());
        usb_properties(&mut result, &dir, None);
        assert_eq!(result.usb.interface_state.state, ReadState::Available);
        assert_eq!(result.usb.interfaces.len(), 1, "descendant devices are not interfaces");
        let values = &result.usb.interfaces[0].values;
        assert_eq!((values["driver"].value.as_str(), values["bInterfaceClass"].value.as_str()), ("usbhid", "03"));

        write(&device_dir, "bNumInterfaces", "2");
        let mut mismatch = fixture_properties(device.clone());
        usb_properties(&mut mismatch, &dir, None);
        assert!(mismatch.usb.interfaces.is_empty());
        assert_eq!(mismatch.usb.interface_state.error, libc::EAGAIN, "count disagrees with the listing");

        write(&device_dir, "bConfigurationValue", "0");
        let mut unconfigured = fixture_properties(device);
        usb_properties(&mut unconfigured, &dir, None);
        assert_eq!(unconfigured.usb.interface_state.state, ReadState::NotApplicable);
        assert_eq!(unconfigured.usb.values["bMaxPower"].state, ReadState::NotApplicable);
    }

    #[test]
    fn efi_variable_bytes_are_read_with_identity_and_permission_checks() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("Var-8be4df61-93ca-11d2-aa0d-00e098032b8c");
        fs::write(&path, [7u8, 0, 0, 0, 1]).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let info = fs::symlink_metadata(&path).unwrap();
        let device = Device {
            path: path.to_string_lossy().into(),
            subsystem: "efivarfs".into(),
            incarnation: format!("{}:{}", info.dev(), info.ino()),
            ..Default::default()
        };
        let found = collect_device_properties(&device, true, None);
        assert_eq!(found.state, ReadState::Available);
        assert_eq!(found.efi_bytes, [7, 0, 0, 0, 1]);
        assert!(!found.efi_truncated);

        let mut other = device.clone();
        other.incarnation = "1:2".into();
        assert_eq!(collect_device_properties(&other, true, None).state, ReadState::Removed);

        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let private = collect_device_properties(&device, true, None);
        assert_eq!((private.state, private.error), (ReadState::PermissionDenied, libc::EACCES));
        fs::remove_file(&path).unwrap();
        assert_eq!(collect_device_properties(&device, true, None).state, ReadState::Removed);
    }

    #[test]
    fn real_pci_and_net_devices_collect_without_error() {
        let devices = crate::scan().expect("scan");
        let pci = devices.iter().find(|d| d.subsystem == "pci").expect("a pci device");
        let found = collect_device_properties(pci, true, None);
        assert_eq!(found.state, ReadState::Available, "{:?}", found.error);
        assert_eq!(found.values["sysfs/vendor"].state, ReadState::Available);
        assert!(found.values.contains_key("driver/path") && found.values.contains_key("driver/name"));
        assert!(found.values.keys().any(|k| k.starts_with("udev/")));
        if let Some(bound) = devices.iter().find(|d| d.subsystem == "pci" && !d.driver.is_empty()) {
            let found = collect_device_properties(bound, true, None);
            assert_eq!(found.values["driver/name"].value, bound.driver);
            assert!(found.values.contains_key("module/name") && found.values.contains_key("driver/bus"));
        }
        let cancelled = AtomicBool::new(true);
        let stopped = collect_device_properties(pci, true, Some(&cancelled));
        assert_eq!((stopped.state, stopped.error), (ReadState::Error, libc::ECANCELED));
    }

    #[test]
    fn net_interfaces_read_their_own_directory_and_reject_changed_indexes() {
        let tmp = tempfile::tempdir().unwrap();
        let lo = std::ffi::CString::new("lo").unwrap();
        // SAFETY: valid NUL-terminated name.
        let index = unsafe { libc::if_nametoindex(lo.as_ptr()) };
        for (k, v) in [
            ("ifindex", index.to_string()),
            ("type", "772".into()),
            ("address", "00:00:00:00:00:00".into()),
            ("addr_assign_type", "0".into()),
            ("mtu", "65536".into()),
        ] {
            write(tmp.path(), k, &v);
        }
        let dir = Dir::open(tmp.path()).unwrap();
        let found = network_interface(&dir, "/sys/devices/virtual/net/lo", "lo", true, None);
        assert_eq!((found.name.as_str(), found.mtu.value.as_str()), ("lo", "65536"));
        assert_eq!(found.link_type.value, "772");
        assert_eq!(found.permanent_address.state, ReadState::NotApplicable, "not an Ethernet-style link");
        assert!(found.addresses.iter().any(|a| a.address == "127.0.0.1"));
        assert!(!found.ethernet.applicable, "ethtool queries are Ethernet-only");

        write(tmp.path(), "ifindex", "0");
        let invalid = network_interface(&dir, "p", "lo", true, None);
        assert_eq!(invalid.address_state.error, libc::EINVAL);
        fs::remove_file(tmp.path().join("ifindex")).unwrap();
        let missing = network_interface(&dir, "p", "lo", true, None);
        assert_eq!(missing.address_state.state, ReadState::Unavailable);
        write(tmp.path(), "ifindex", &(index + 500).to_string());
        let moved = network_interface(&dir, "p", "lo", true, None);
        assert_eq!(moved.address_state.state, ReadState::Removed, "name no longer maps to that index");
        assert_eq!(moved.mtu.state, ReadState::Removed);
    }

    #[test]
    fn real_net_and_network_pci_devices_collect_interfaces() {
        let devices = crate::scan().expect("scan");
        let lo = devices.iter().find(|d| d.subsystem == "net" && d.sysname == "lo").expect("loopback");
        let found = collect_device_properties(lo, true, None);
        assert_eq!(found.network_state.state, ReadState::Available);
        assert_eq!(found.network.len(), 1);
        assert!(found.network[0].addresses.iter().any(|a| a.address == "127.0.0.1"));
        // A PCI network function lists its direct interface(s) when it has any.
        if let Some(pci) = devices.iter().find(|d| d.subsystem == "pci" && d.category == "network") {
            let found = collect_device_properties(pci, false, None);
            assert!(matches!(found.network_state.state, ReadState::Available | ReadState::Unavailable), "{:?}", found.network_state);
            assert!(found.network.iter().all(|n| !n.name.is_empty()));
        }
    }
}
