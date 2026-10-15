//! udev enumeration, ported from `Enumerator::collect()` in `src/devices/enumerator.cpp`.
//! Only identity, binding, an explicit udev property allowlist and a fixed set of
//! documented sysfs attributes are read: no recursive reads or hardware queries.
use crate::cpuinfo::{parse_cpu_models, read_cpuinfo};
use crate::efivars::{scan_efi_variables, EFIVARS_DIRECTORY};
use crate::model::{classify, Attribute, Device, ReadState};
use crate::modulenames::ModuleNames;
use crate::naming::name_device;
use crate::presentation::refine_presentation;
use crate::resources::{collect_device_resources, DeviceResources, ResourceIssue};
use crate::sysfs::read_attribute;
use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const PROPERTY_ALLOWLIST: [&str; 19] = [
    "ID_MODEL_FROM_DATABASE",
    "ID_MODEL",
    "NAME",
    "ID_V4L_PRODUCT",
    "MODALIAS",
    "PCI_CLASS",
    "PCI_ID",
    "ID_VENDOR_FROM_DATABASE",
    "ID_VENDOR",
    "ID_BUS",
    "ID_INPUT_KEYBOARD",
    "ID_INPUT_MOUSE",
    "ID_INPUT_TOUCHPAD",
    "ID_INPUT_POINTINGSTICK",
    "ID_INPUT_SWITCH",
    "PRODUCT",
    "PHYS",
    "DEVNUM",
    "USEC_INITIALIZED",
];

fn text(value: Option<&std::ffi::OsStr>) -> String {
    value.map(|v| v.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Lazily read `/proc/cpuinfo` once per scan; only per-processor models are kept.
struct CpuModels {
    read: bool,
    state: Attribute,
    models: HashMap<u32, String>,
}

impl CpuModels {
    fn attribute_for(&mut self, sysname: &str) -> Attribute {
        if !self.read {
            let info = read_cpuinfo();
            if info.state == ReadState::Available {
                self.models = parse_cpu_models(&info.value);
            }
            self.state = Attribute { value: String::new(), ..info };
            self.read = true;
        }
        let mut model = Attribute { state: self.state.state, value: String::new(), error: self.state.error };
        let number = sysname.strip_prefix("cpu").and_then(|n| n.parse::<u32>().ok());
        match number.and_then(|n| self.models.get(&n)) {
            Some(value) => model.value = value.clone(),
            None if model.state == ReadState::Available => model.state = ReadState::Unavailable,
            None => {}
        }
        model
    }
}

/// The documented per-subsystem attributes, each read from this device's own directory.
fn read_attributes(d: &mut Device) {
    let single = match d.subsystem.as_str() {
        "input" | "video4linux" => Some("name"),
        "sound" if d.sysname.starts_with("card") => Some("id"),
        "power_supply" => Some("model_name"),
        "nvme" => Some("model"),
        "usb" if d.devtype == "usb_device" => Some("product"),
        _ => None,
    };
    let mut keys: Vec<&str> = Vec::new();
    match d.subsystem.as_str() {
        "hdaudio" => keys.extend(["vendor_name", "chip_name"]),
        "power_supply" => keys.extend(["type", "manufacturer"]),
        "usb" if d.devtype == "usb_device" => keys.push("manufacturer"),
        "drm" if d.sysname.contains('-') => keys.extend(["status", "enabled"]),
        _ => {}
    }
    keys.extend(single);
    for key in keys {
        let attribute = read_attribute(format!("{}/{key}", d.path));
        d.attributes.insert(key.to_string(), attribute);
    }
}

fn driver_module(d: &Device) -> String {
    if d.driver.is_empty() {
        return String::new();
    }
    match std::fs::canonicalize(format!("{}/driver/module", d.path)) {
        Ok(target) if target.starts_with("/sys/module/") => target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        _ => String::new(),
    }
}

/// Fills missing database names from the udev hwdb via the device's own MODALIAS (pci/usb only).
fn supplement_from_hwdb(hwdb: &udev::Hwdb, d: &mut Device) {
    let wants = |d: &Device, key: &str| d.properties.get(key).is_none_or(|v| v.is_empty());
    if !(d.subsystem == "pci" || d.subsystem == "usb") || !(wants(d, "ID_MODEL_FROM_DATABASE") || (d.subsystem == "usb" && wants(d, "ID_VENDOR_FROM_DATABASE")))
    {
        return;
    }
    let alias = d.properties.get("MODALIAS").cloned().unwrap_or_default();
    if alias.is_empty() {
        return;
    }
    for key in ["ID_MODEL_FROM_DATABASE", "ID_VENDOR_FROM_DATABASE"] {
        if !wants(d, key) {
            continue;
        }
        if let Some(value) = hwdb.query_one(alias.as_str(), key) {
            d.properties.insert(key.to_string(), value.to_string_lossy().into_owned());
            d.property_sources.insert(key.to_string(), format!("hwdb: {key} ({alias})"));
        }
    }
}

/// Opens the device directory without following a symlink and reads its resources; an open
/// failure becomes one explicit issue instead of a silent absence.
fn resources_for(d: &Device) -> DeviceResources {
    use std::os::unix::fs::OpenOptionsExt;
    let opened = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&d.path);
    match opened {
        Ok(dir) => collect_device_resources(&dir, d, None),
        Err(e) => {
            let error = e.raw_os_error().unwrap_or(libc::EIO);
            let state = match error {
                libc::EACCES | libc::EPERM => ReadState::PermissionDenied,
                libc::ENOENT => ReadState::Unavailable,
                libc::ENODEV => ReadState::Removed,
                _ => ReadState::Error,
            };
            DeviceResources {
                issues: vec![ResourceIssue { source: d.path.clone(), error: Attribute { state, value: String::new(), error } }],
                ..Default::default()
            }
        }
    }
}

/// Devices plus how many records could not be examined.
pub struct ScanResult {
    pub devices: Vec<Device>,
    pub skipped: usize,
}

pub fn scan() -> Result<Vec<Device>, String> {
    scan_cancellable(None).map(|result| result.devices)
}

/// Like [`scan`], but stops between records once `cancel` is set (the partial result is then
/// the caller's to discard) and reports skipped records.
pub fn scan_cancellable(cancel: Option<&AtomicBool>) -> Result<ScanResult, String> {
    let mut enumerator = udev::Enumerator::new().map_err(|e| format!("Could not create the udev context: {e}"))?;
    let found = enumerator.scan_devices().map_err(|e| format!("Could not enumerate kernel devices: {e}"))?;
    // hwdb is an optional local naming database, still accessed through libudev.
    let hwdb = udev::Hwdb::new().ok();
    let mut module_names = ModuleNames::new();
    let mut cpu_models = CpuModels { read: false, state: Attribute::default(), models: HashMap::new() };
    let mut devices = Vec::new();
    let mut skipped = 0;
    for raw in found {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            break;
        }
        let path = raw.syspath().to_string_lossy().into_owned();
        if !path.starts_with("/sys/devices/") {
            skipped += 1;
            continue;
        }
        let mut properties = HashMap::new();
        for key in PROPERTY_ALLOWLIST {
            if let Some(value) = raw.property_value(key) {
                properties.insert(key.to_string(), value.to_string_lossy().into_owned());
            }
        }
        let mut device = Device {
            path,
            parent_path: raw.parent().map(|p| p.syspath().to_string_lossy().into_owned()).unwrap_or_default(),
            subsystem: text(raw.subsystem()),
            sysname: raw.sysname().to_string_lossy().into_owned(),
            devtype: text(raw.devtype()),
            driver: text(raw.driver()),
            properties,
            ..Default::default()
        };
        if let Some(hwdb) = &hwdb {
            supplement_from_hwdb(hwdb, &mut device);
        }
        device.driver_module = driver_module(&device);
        if device.subsystem == "cpu" {
            let model = cpu_models.attribute_for(&device.sysname);
            device.attributes.insert("cpu_model".into(), model);
        }
        let Ok(before) = std::fs::metadata(&device.path) else {
            skipped += 1;
            continue;
        };
        device.incarnation = format!("{}:{}:{}", before.dev(), before.ino(), device.properties.get("USEC_INITIALIZED").map_or("", String::as_str));
        read_attributes(&mut device);
        if device.subsystem == "pci" || device.subsystem == "pnp" {
            device.resources = Some(Arc::new(resources_for(&device)));
        }
        name_device(&mut device);
        module_names.supplement(&mut device);
        // A path replaced while its attributes were read describes two devices: drop it.
        match std::fs::metadata(&device.path) {
            Ok(after) if after.dev() == before.dev() && after.ino() == before.ino() => {}
            _ => {
                skipped += 1;
                continue;
            }
        }
        classify(&mut device);
        devices.push(device);
    }
    if !cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
        refine_presentation(&mut devices);
        let efi = scan_efi_variables(std::path::Path::new(EFIVARS_DIRECTORY), cancel);
        skipped += efi.skipped;
        devices.extend(efi.devices);
    }
    Ok(ScanResult { devices, skipped })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_finds_devices_with_a_specific_category() {
        let devices = scan().expect("udev scan");
        assert!(!devices.is_empty());
        assert!(devices.iter().any(|d| d.category != "other"));
    }

    #[test]
    fn every_device_has_an_incarnation_and_drm_connectors_report_status() {
        let devices = scan().expect("udev scan");
        assert!(devices.iter().filter(|d| d.subsystem != "efivarfs").all(|d| d.incarnation.split(':').count() == 3));
        for connector in devices.iter().filter(|d| d.subsystem == "drm" && d.sysname.contains('-')) {
            let status = &connector.attributes["status"];
            assert_eq!(status.state, ReadState::Available, "{}", connector.path);
        }
    }

    #[test]
    fn every_device_has_a_name_and_source() {
        let devices = scan().expect("udev scan");
        assert!(devices.iter().all(|d| !d.name.is_empty() && !d.name_source.is_empty()));
        let cpu = devices.iter().find(|d| d.subsystem == "cpu").expect("a cpu device");
        assert!(cpu.name_source.starts_with("/proc/cpuinfo"), "{}", cpu.name_source);
    }

    #[test]
    fn pci_devices_carry_a_resource_snapshot() {
        let devices = scan().expect("udev scan");
        let pci: Vec<_> = devices.iter().filter(|d| d.subsystem == "pci").collect();
        assert!(!pci.is_empty() && pci.iter().all(|d| d.resources.is_some()));
        assert!(pci.iter().any(|d| d.resources.as_ref().is_some_and(|r| r.items.iter().any(|i| i.pci))));
    }

    #[test]
    fn cpu_devices_get_a_model_attribute() {
        let devices = scan().expect("udev scan");
        let cpu = devices.iter().find(|d| d.subsystem == "cpu").expect("a cpu device");
        assert!(cpu.attributes.contains_key("cpu_model"));
    }
}
