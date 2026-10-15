//! Display text for property reads, ported from `src/devices/propertytext.cpp`.
use crate::i18n::{arg, arg2, format_data_size, format_fixed, format_integer, format_local_iso, tr};
use crate::label::device_display_name;
use crate::model::{category_label, Attribute, Device, ReadState};
use crate::properties::DeviceProperties;
use crate::storage::{Format, StorageField};

/// Text for one attribute read: its value, or a plain-language reason it has none.
/// errno stays visible so copied text keeps the exact failure.
pub fn property_read_value(a: &Attribute) -> String {
    match a.state {
        ReadState::Available if a.value.is_empty() => tr("Empty value"),
        ReadState::Available => a.value.clone(),
        ReadState::Unavailable => tr("Unavailable"),
        ReadState::PermissionDenied => tr("Permission denied"),
        ReadState::Removed => tr("Device removed"),
        ReadState::Unsupported => tr("Unsupported"),
        ReadState::NotApplicable => tr("Not applicable"),
        ReadState::Error => {
            let text = match a.error {
                libc::ETIMEDOUT => "Query timed out (errno %1)",
                libc::EOVERFLOW => "Data exceeds supported limits (errno %1)",
                libc::EAGAIN => "Temporarily unavailable; retry the query (errno %1)",
                libc::EINVAL | libc::EPROTO => "Invalid data or query (errno %1)",
                libc::ECANCELED => "Query cancelled (errno %1)",
                _ => "Read error (errno %1)",
            };
            arg(tr(text), &a.error.to_string())
        }
    }
}

/// One row of the generic property table. `tab` selects the dialog page: 0 General, 1 Driver,
/// 2 Details, 3 Storage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PropertyEntry {
    pub id: String,
    pub label: String,
    pub value: String,
    pub source: String,
    pub advanced: bool,
    pub tab: i32,
    pub state: ReadState,
    pub error: i32,
}

impl PropertyEntry {
    fn source(&mut self, source: impl Into<String>) -> &mut Self {
        self.source = source.into();
        self
    }
    fn advanced(&mut self) -> &mut Self {
        self.advanced = true;
        self
    }
    fn tab(&mut self, tab: i32) -> &mut Self {
        self.tab = tab;
        self
    }
    fn state(&mut self, state: ReadState, error: i32) -> &mut Self {
        self.state = state;
        self.error = error;
        self
    }
}

#[derive(Default)]
struct Entries(Vec<PropertyEntry>);

impl Entries {
    fn add(&mut self, id: &str, label: String, value: String) -> &mut PropertyEntry {
        self.0.push(PropertyEntry { id: id.into(), label, value, source: String::new(), advanced: false, tab: 2, state: ReadState::Available, error: 0 });
        self.0.last_mut().expect("just pushed")
    }
}

/// Status wording for a row whose value could not be read, lower-cased for inline use.
pub fn details_property_status(entry: &PropertyEntry) -> String {
    if entry.state == ReadState::Available {
        return if entry.value.is_empty() { tr("Empty value").to_lowercase() } else { String::new() };
    }
    property_read_value(&Attribute { state: entry.state, value: String::new(), error: entry.error }).to_lowercase()
}

/// Display text for one storage field, including friendly explanations for known states.
pub fn storage_field_value(field: &StorageField) -> String {
    let value = &field.value;
    if field.format == Format::HealthQueryStatus && value.state == ReadState::Unavailable && value.error == libc::ETIMEDOUT {
        return tr("Not queried: shared health-query time limit reached");
    }
    if value.state == ReadState::Unsupported {
        match field.id.as_str() {
            "service" => return tr("UDisks2 support is not enabled in this build"),
            "health" => return tr("Drive health is not exposed through a supported UDisks2 interface"),
            "org.freedesktop.UDisks2.Drive.Ata/SmartGetAttributes" | "org.freedesktop.UDisks2.NVMe.Controller/SmartGetAttributes" => {
                return tr("Detailed health query is not supported by UDisks2");
            }
            _ => {}
        }
    }
    if value.state != ReadState::Available {
        return property_read_value(value);
    }
    let number = || value.value.parse::<u64>().ok();
    match field.format {
        Format::HealthQueryStatus | Format::Text => {
            if field.id == "service" && value.value == "cached" {
                return tr("Available; cached properties only");
            }
            if field.id == "mountinfo" && value.value == "application-namespace" {
                return tr("Application mount namespace only; other namespaces may differ. No filesystem is mounted or unlocked.");
            }
            if field.id == "mounts" && value.value == "no-observed-mount" {
                return tr("No mount observed in the application namespace");
            }
            property_read_value(value)
        }
        Format::Bytes => match number() {
            None => tr("Invalid capacity or sector-size metadata"),
            Some(bytes) if bytes > i64::MAX as u64 => arg(tr("%1 bytes"), &format_integer(bytes)),
            Some(bytes) => arg2(tr("%1 bytes (%2)"), &format_integer(bytes), &format_data_size(bytes)),
        },
        Format::Boolean => match value.value.as_str() {
            "1" => tr("Yes"),
            "0" => tr("No"),
            _ => tr("Invalid boolean metadata"),
        },
        Format::Removable => match value.value.as_str() {
            "1" => tr("Removable (UDisks2 hint)"),
            "0" => tr("Fixed (UDisks2 hint)"),
            _ => tr("Invalid removable metadata"),
        },
        Format::Epoch => match number().filter(|s| *s != 0 && *s <= i64::MAX as u64).and_then(|s| format_local_iso(s as i64)) {
            Some(text) => text,
            None => tr("Unavailable"),
        },
        Format::Kelvin => match value.value.parse::<f64>().ok().filter(|k| *k > 0.0) {
            Some(kelvin) => {
                let precision = if field.id.starts_with("org.freedesktop.UDisks2.NVMe.Controller/") { 0 } else { 1 };
                arg(tr("%1 °C"), &format_fixed(kelvin - 273.15, precision))
            }
            None => tr("Unavailable"),
        },
        Format::Percent | Format::Hours | Format::Seconds | Format::Minutes | Format::Count => {
            let Some(n) = number() else { return tr("Invalid health metadata") };
            let text = format_integer(n);
            match field.format {
                Format::Percent => arg(tr("%1%"), &text),
                Format::Seconds => arg2(tr("%1 seconds (%2 hours)"), &text, &format_fixed(n as f64 / 3600.0, 1)),
                Format::Hours => arg(tr("%1 hours"), &text),
                Format::Minutes => arg(tr("%1 minutes"), &text),
                _ => text,
            }
        }
        Format::AtaHealth => match value.value.as_str() {
            "1" => tr("Failure predicted by drive"),
            "0" => tr("No failure predicted by drive (not a guarantee of health)"),
            _ => tr("Unavailable"),
        },
        Format::NvmeHealth => {
            if value.value.is_empty() {
                return tr("No critical warnings reported (not a guarantee of health)");
            }
            value
                .value
                .split('\n')
                .filter(|w| !w.is_empty())
                .map(|warning| match warning {
                    "spare" => tr("Available spare is below its threshold"),
                    "temperature" => tr("Temperature is outside a permitted threshold"),
                    "degraded" => tr("NVM subsystem reliability is degraded"),
                    "readonly" => tr("Media is in read-only mode"),
                    "volatile_mem" => tr("Volatile-memory backup has failed"),
                    "pmr_readonly" => tr("Persistent memory region is read-only or unreliable"),
                    other => arg(tr("Unrecognized warning: %1"), other),
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
    }
}

fn hex_dump(bytes: &[u8]) -> String {
    bytes
        .chunks(16)
        .enumerate()
        .map(|(row, chunk)| {
            let hex = chunk.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ");
            let printable: String = chunk.iter().map(|&b| if (32..=126).contains(&b) { b as char } else { '.' }).collect();
            format!("{:08x}  {hex:<47}  {printable}", row * 16)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn attr_or(snapshot: &DeviceProperties, id: &str, fallback: &Attribute) -> Attribute {
    snapshot.values.get(id).unwrap_or(fallback).clone()
}

/// Builds the General, Driver and Details rows for a snapshot, keeping provenance beside
/// each value. `removed` marks a snapshot of a device that is no longer present. Storage rows
/// are added by the storage module.
pub fn property_entries(snapshot: &DeviceProperties, removed: bool) -> Vec<PropertyEntry> {
    let mut entries = Entries::default();
    let d = &snapshot.device;
    let efi = d.subsystem == "efivarfs";
    let not_applicable = Attribute { state: ReadState::NotApplicable, ..Default::default() };
    let raw = |id: &str| snapshot.values.get(id).cloned().unwrap_or_default();
    let source_of = |id: &str| snapshot.sources.get(id).cloned().unwrap_or_default();
    let add_raw = |entries: &mut Entries, id: &str, label: String, tab: i32, fallback: &Attribute| {
        let a = attr_or(snapshot, id, fallback);
        entries.add(id, label, property_read_value(&a)).source(source_of(id)).tab(tab).state(a.state, a.error);
    };

    if snapshot.storage.applicable {
        entries
            .add(
                "storage/scope",
                tr("Storage snapshot"),
                tr("Each entity is a separate storage layer; capacities are not added. Health is cached UDisks2 evidence, separate from operational status. Reload never requests SMART updates or self-tests. Deep search includes native metadata only."),
            )
            .tab(3);
        for note in &snapshot.storage.notes {
            entries.add(&format!("storage/note/{}", note.id), tr(note.label), storage_field_value(note)).source(note.source.clone()).tab(3);
        }
        for entity in &snapshot.storage.entities {
            let prefix = format!("storage/entity/{}", entity.id);
            entries.add(&prefix, tr("Storage entity"), entity.id.clone()).tab(3);
            for field in &entity.fields {
                entries.add(&format!("{prefix}/{}", field.id), tr(field.label), storage_field_value(field)).source(field.source.clone()).tab(3);
            }
        }
        for (index, edge) in snapshot.storage.links.iter().enumerate() {
            entries.add(&format!("storage/link/{index}"), tr(edge.label), format!("{}\n→ {}", edge.from, edge.to)).source(edge.source.clone()).tab(3);
        }
    }
    entries.add("name", tr("Device name"), device_display_name(d, false)).source(d.name_source.clone()).tab(0);
    entries.add("category", tr("Category"), category_label(d.category)).source(tr("Application grouping")).tab(0);

    let mut manufacturer = Attribute::default();
    let mut manufacturer_source = String::new();
    if efi {
        manufacturer = not_applicable.clone();
    } else {
        for id in ["sysfs/manufacturer", "sysfs/vendor_name", "udev/ID_VENDOR_FROM_DATABASE", "udev/ID_VENDOR"] {
            let candidate = raw(id);
            let usable = (candidate.state == ReadState::Available && !candidate.value.is_empty())
                || matches!(candidate.state, ReadState::PermissionDenied | ReadState::Error);
            if usable {
                manufacturer = candidate;
                manufacturer_source = source_of(id);
                break;
            }
        }
        if manufacturer.state == ReadState::Unavailable {
            for key in ["ID_VENDOR_FROM_DATABASE", "ID_VENDOR"] {
                let Some(value) = d.properties.get(key).filter(|v| !v.is_empty()) else { continue };
                manufacturer = Attribute { state: ReadState::Available, value: value.clone(), error: 0 };
                manufacturer_source = d.property_sources.get(key).cloned().unwrap_or_else(|| format!("udev: {key} ({})", d.path));
                break;
            }
        }
    }
    entries
        .add("manufacturer", tr("Manufacturer"), property_read_value(&manufacturer))
        .source(manufacturer_source)
        .tab(0)
        .state(manufacturer.state, manufacturer.error);

    let mut bus = raw("udev/ID_BUS");
    if bus.state != ReadState::Available {
        bus = raw("driver/bus");
    }
    if efi {
        bus = not_applicable.clone();
    }
    let bus_value = if efi {
        property_read_value(&bus)
    } else if bus.state == ReadState::Available {
        format!("{} / {}", bus.value, d.subsystem)
    } else {
        d.subsystem.clone()
    };
    entries
        .add("bus", tr("Bus / subsystem"), bus_value)
        .source(tr("Device udev metadata and direct driver bus"))
        .tab(0)
        .state(if efi { ReadState::NotApplicable } else { ReadState::Available }, 0);
    entries.add("location", tr("Location"), if efi { d.parent_path.clone() } else { d.path.clone() }).source(tr("Kernel path")).tab(0);

    let status = if removed {
        tr("Removed or replaced; showing a snapshot.")
    } else if snapshot.state != ReadState::Available {
        tr("Present in the last inventory; properties have not been verified.")
    } else {
        let mut status = tr("Present at the last property read.");
        if !efi {
            let binding = raw("driver/name");
            status.push('\n');
            status += &if binding.state == ReadState::Available {
                if binding.value.is_empty() {
                    tr("No directly bound driver; this alone is not a fault.")
                } else {
                    arg(tr("Driver bound: %1"), &binding.value)
                }
            } else {
                arg(tr("Driver binding: %1"), &property_read_value(&binding))
            };
            let authorization = raw("sysfs/authorized");
            match (authorization.state, authorization.value.as_str()) {
                (ReadState::Available, "0") => status += &format!("\n{}", tr("USB device/interface is unauthorized.")),
                (ReadState::Available, "1") => status += &format!("\n{}", tr("USB device/interface is authorized.")),
                (ReadState::PermissionDenied | ReadState::Error, _) => {
                    status += &format!("\n{}", arg(tr("USB authorization: %1"), &property_read_value(&authorization)));
                }
                _ => {}
            }
        }
        status
    };
    entries.add("status", tr("Device status"), status).source(tr("Observed presence, binding and USB authorization only")).tab(0);

    let mut binding = snapshot.values.get("driver/name").cloned().unwrap_or_else(|| {
        if d.driver.is_empty() {
            Attribute::default()
        } else {
            Attribute { state: ReadState::Available, value: d.driver.clone(), error: 0 }
        }
    });
    if efi {
        binding = not_applicable.clone();
    }
    let driver_name =
        if !efi && binding.state == ReadState::Available && binding.value.is_empty() { tr("No directly bound driver") } else { property_read_value(&binding) };
    let driver_source = if efi { String::new() } else { format!("{}/driver", d.path) };
    entries.add("driver/name", tr("Bound kernel driver"), driver_name.clone()).source(driver_source.clone()).tab(0).state(binding.state, binding.error);

    let mut identifiers: Vec<String> = Vec::new();
    for key in ["PCI_ID", "PCI_SUBSYS_ID", "PRODUCT", "ID_VENDOR_ID", "ID_MODEL_ID", "ID_SERIAL_SHORT"] {
        let a = snapshot.values.get(&format!("udev/{key}")).cloned().unwrap_or_else(|| {
            d.properties.get(key).map_or_else(Attribute::default, |v| Attribute { state: ReadState::Available, value: v.clone(), error: 0 })
        });
        if a.state == ReadState::Available && !a.value.is_empty() {
            identifiers.push(format!("{key}: {}", a.value));
        }
    }
    for key in ["vendor", "device", "idVendor", "idProduct", "serial", "hid", "uid"] {
        let a = raw(&format!("sysfs/{key}"));
        if a.state != ReadState::Unavailable && !(a.state == ReadState::Available && a.value.is_empty()) {
            identifiers.push(format!("{key}: {}", property_read_value(&a)));
        }
    }
    let identifiers_value = if efi {
        property_read_value(&not_applicable)
    } else if identifiers.is_empty() {
        tr("Unavailable")
    } else {
        identifiers.join("\n")
    };
    entries.add("identifiers", tr("Hardware identifiers"), identifiers_value).source(tr("Selected udev and direct sysfs identifiers")).tab(0).state(
        if efi {
            ReadState::NotApplicable
        } else if identifiers.is_empty() {
            ReadState::Unavailable
        } else {
            ReadState::Available
        },
        0,
    );

    entries.add("driver/binding", tr("Bound kernel driver"), driver_name).source(driver_source).tab(1).state(binding.state, binding.error);
    let module_fallback =
        if efi || (binding.state == ReadState::Available && binding.value.is_empty()) { not_applicable.clone() } else { Attribute::default() };
    add_raw(&mut entries, "driver/bus", tr("Driver bus"), 1, &module_fallback);
    add_raw(&mut entries, "module/name", tr("Owning kernel module"), 1, &module_fallback);
    let kind = attr_or(snapshot, "module/type", &module_fallback);
    let reportable = matches!(kind.state, ReadState::NotApplicable | ReadState::PermissionDenied | ReadState::Error);
    entries
        .add(
            "module/type",
            tr("Built-in or modular"),
            if kind.state == ReadState::Available {
                tr("Modular (kernel initstate evidence)")
            } else if reportable {
                property_read_value(&kind)
            } else {
                tr("Undetermined; a missing module link/version does not prove a built-in driver.")
            },
        )
        .source(source_of("module/type"))
        .tab(1)
        .state(if reportable { kind.state } else { ReadState::Available }, kind.error);
    add_raw(&mut entries, "module/runtime_version", tr("Running module version"), 1, &module_fallback);
    let labels = [
        tr("Installed module filename"),
        tr("Installed module version"),
        tr("Installed module description"),
        tr("Installed module author"),
        tr("Installed module license"),
        tr("Declared firmware names"),
    ];
    for (field, label) in ["filename", "version", "description", "author", "license", "firmware"].into_iter().zip(labels) {
        add_raw(&mut entries, &format!("module/{field}"), label, 1, &module_fallback);
    }
    let firmware_fallback = if d.subsystem == "nvme" { Attribute::default() } else { not_applicable.clone() };
    add_raw(&mut entries, "sysfs/firmware_rev", tr("Reported device firmware revision"), 1, &firmware_fallback);
    entries
        .add(
            "module/note",
            tr("Metadata scope"),
            if efi {
                property_read_value(&not_applicable)
            } else {
                tr("Installed metadata may differ from code already loaded before an update. Declared firmware names do not prove firmware is loaded or report its version.")
            },
        )
        .tab(1)
        .state(if efi { ReadState::NotApplicable } else { ReadState::Available }, 0);

    // Interpreted facts plus curated raw values, keeping provenance beside each.
    let parent_known = !d.parent_path.is_empty();
    let parent = entries.add("parent", tr("Parent device path"), if parent_known { d.parent_path.clone() } else { tr("Unavailable") });
    parent.source(tr("Recorded ancestry")).state(if parent_known { ReadState::Available } else { ReadState::Unavailable }, 0);
    if efi {
        parent.advanced();
    }
    entries.add("raw_name", tr("Original name"), d.name.clone()).source(d.name_source.clone()).advanced();
    entries.add("name_source", tr("Name source"), d.name_source.clone()).advanced();
    entries.add("sysname", tr("Kernel name"), d.sysname.clone()).advanced();
    entries.add("path", tr("Full kernel path"), d.path.clone()).advanced();
    if !efi {
        let a = raw("driver/path");
        entries.add("driver/path", tr("Direct driver path"), property_read_value(&a)).source(source_of("driver/path")).advanced().state(a.state, a.error);
    }
    let mut keys: Vec<&String> = snapshot.values.keys().collect();
    keys.sort();
    for key in keys {
        if !(key.starts_with("udev/") || key.starts_with("sysfs/")) || key == "sysfs/firmware_rev" {
            continue; // firmware_rev is already exposed on the Driver tab.
        }
        let a = raw(key);
        entries.add(key, key.clone(), property_read_value(&a)).source(source_of(key)).advanced().state(a.state, a.error);
    }
    let mut keys: Vec<&String> = d.properties.keys().collect();
    keys.sort();
    for key in keys {
        if snapshot.values.contains_key(&format!("udev/{key}")) {
            continue;
        }
        entries
            .add(&format!("inventory/{key}"), arg(tr("Inventory: %1"), key), d.properties[key].clone())
            .source(d.property_sources.get(key).cloned().unwrap_or_else(|| "inventory udev metadata".into()))
            .advanced();
    }
    let mut keys: Vec<&String> = d.attributes.keys().collect();
    keys.sort();
    for key in keys {
        let a = &d.attributes[key];
        entries
            .add(&format!("inventory/sysfs/{key}"), arg(tr("Inventory attribute: %1"), key), property_read_value(a))
            .source(tr("Last inventory; see name source for derived naming metadata"))
            .advanced()
            .state(a.state, a.error);
    }
    if efi {
        let hex = raw("efi/hex");
        let mut dump = property_read_value(&hex);
        if hex.state == ReadState::Available {
            dump = if snapshot.efi_bytes.is_empty() { tr("Empty file") } else { hex_dump(&snapshot.efi_bytes) };
            if snapshot.efi_truncated {
                dump += &format!("\n{}", tr("Truncated: first 65536 bytes shown."));
            }
        }
        entries
            .add("efi/hex", tr("UEFI variable — hex dump"), dump)
            .source(tr("Complete efivarfs file bytes, including the attribute prefix; contents are not decoded."))
            .state(hex.state, hex.error);
    }
    entries.0
}

/// Storage identity and content belong to the storage tabs even in advanced mode; generic
/// device/bus/driver facts remain available for all device classes.
pub fn is_details_property(entry: &PropertyEntry, device: &Device) -> bool {
    if entry.tab == 3 || entry.id.starts_with("storage/") {
        return false;
    }
    if device.subsystem != "block" && device.subsystem != "nvme" {
        return true;
    }
    let mut key = entry.id.as_str();
    for prefix in ["inventory/sysfs/", "inventory/", "udev/", "sysfs/"] {
        if let Some(rest) = key.strip_prefix(prefix) {
            key = rest;
            break;
        }
    }
    const HIDDEN_PREFIXES: [&str; 12] = ["ID_FS_", "ID_PART_", "ID_ATA", "ID_NVME", "ID_SCSI", "ID_DRIVE_", "ID_CDROM", "DM_", "MD_", "queue/", "dm/", "md/"];
    if HIDDEN_PREFIXES.iter().any(|p| key.starts_with(p)) {
        return false;
    }
    const IDENTITY_KEYS: [&str; 24] = [
        "ID_MODEL",
        "ID_MODEL_ENC",
        "ID_VENDOR",
        "ID_VENDOR_ENC",
        "ID_SERIAL",
        "ID_SERIAL_SHORT",
        "ID_REVISION",
        "ID_BUS",
        "ID_WWN",
        "ID_WWN_WITH_EXTENSION",
        "DEVNAME",
        "DISKSEQ",
        "size",
        "dev",
        "partition",
        "start",
        "ro",
        "removable",
        "model",
        "vendor",
        "serial",
        "rev",
        "firmware_rev",
        "wwid",
    ];
    !IDENTITY_KEYS.contains(&key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(state: ReadState, error: i32) -> Attribute {
        Attribute { state, value: String::new(), error }
    }

    #[test]
    fn states_have_plain_language_text() {
        assert_eq!(property_read_value(&read(ReadState::Available, 0)), "Empty value");
        assert_eq!(property_read_value(&Attribute { state: ReadState::Available, value: "x".into(), error: 0 }), "x");
        assert_eq!(property_read_value(&read(ReadState::PermissionDenied, 13)), "Permission denied");
        assert_eq!(property_read_value(&read(ReadState::Removed, 19)), "Device removed");
    }

    #[test]
    fn errors_keep_errno_and_classify_common_causes() {
        let text = |e| property_read_value(&read(ReadState::Error, e));
        assert_eq!(text(libc::ETIMEDOUT), "Query timed out (errno 110)");
        assert_eq!(text(libc::EOVERFLOW), "Data exceeds supported limits (errno 75)");
        assert_eq!(text(libc::EAGAIN), "Temporarily unavailable; retry the query (errno 11)");
        assert_eq!(text(libc::EPROTO), "Invalid data or query (errno 71)");
        assert_eq!(text(libc::ECANCELED), "Query cancelled (errno 125)");
        assert_eq!(text(libc::EIO), "Read error (errno 5)");
    }
}

#[cfg(test)]
mod entry_tests {
    use super::*;
    use crate::model::Attribute;

    fn available(value: &str) -> Attribute {
        Attribute { state: ReadState::Available, value: value.into(), error: 0 }
    }

    fn snapshot() -> DeviceProperties {
        let mut device = Device {
            path: "/sys/devices/pci0000:00/0000:00:14.3".into(),
            parent_path: "/sys/devices/pci0000:00".into(),
            subsystem: "pci".into(),
            sysname: "0000:00:14.3".into(),
            name: "Wi-Fi 7".into(),
            name_source: "hwdb: ID_MODEL_FROM_DATABASE (pci:x)".into(),
            category: "network",
            driver: "iwlwifi".into(),
            ..Default::default()
        };
        device.properties.insert("ID_VENDOR_FROM_DATABASE".into(), "Intel Corporation".into());
        device.properties.insert("PCI_ID".into(), "8086:7740".into());
        let mut snapshot = DeviceProperties { device, state: ReadState::Available, ..Default::default() };
        for (id, value) in [("driver/name", "iwlwifi"), ("driver/bus", "pci"), ("sysfs/vendor", "0x8086"), ("udev/PCI_ID", "8086:7740")] {
            snapshot.values.insert(id.into(), available(value));
        }
        snapshot
    }

    fn find<'a>(entries: &'a [PropertyEntry], id: &str) -> &'a PropertyEntry {
        entries.iter().find(|e| e.id == id).unwrap_or_else(|| panic!("missing entry {id}"))
    }

    #[test]
    fn general_and_driver_rows_report_evidence() {
        let entries = property_entries(&snapshot(), false);
        assert_eq!(find(&entries, "name").value, "Wi-Fi 7 [0000:00:14.3]");
        assert_eq!(find(&entries, "category").value, "Network adapters");
        assert_eq!(find(&entries, "manufacturer").value, "Intel Corporation", "falls back to inventory udev metadata");
        assert_eq!(find(&entries, "bus").value, "pci / pci");
        assert_eq!(find(&entries, "identifiers").value, "PCI_ID: 8086:7740\nvendor: 0x8086");
        assert_eq!(find(&entries, "status").value, "Present at the last property read.\nDriver bound: iwlwifi");
        assert_eq!(find(&entries, "driver/binding").tab, 1);
        assert_eq!(find(&entries, "module/type").value, "Undetermined; a missing module link/version does not prove a built-in driver.");
        assert!(find(&entries, "sysfs/vendor").advanced && find(&entries, "inventory/ID_VENDOR_FROM_DATABASE").advanced);
        assert!(entries.iter().all(|e| e.id != "udev/PCI_ID" || e.advanced));
    }

    #[test]
    fn removed_and_unverified_snapshots_say_so() {
        assert_eq!(find(&property_entries(&snapshot(), true), "status").value, "Removed or replaced; showing a snapshot.");
        let mut unverified = snapshot();
        unverified.state = ReadState::Error;
        assert_eq!(find(&property_entries(&unverified, false), "status").value, "Present in the last inventory; properties have not been verified.");
    }

    #[test]
    fn unbound_driver_and_usb_authorization_are_worded_plainly() {
        let mut s = snapshot();
        s.values.insert("driver/name".into(), available(""));
        s.values.insert("sysfs/authorized".into(), available("0"));
        let entries = property_entries(&s, false);
        assert_eq!(find(&entries, "driver/name").value, "No directly bound driver");
        assert!(find(&entries, "status").value.ends_with("USB device/interface is unauthorized."));
        assert_eq!(find(&entries, "module/name").state, ReadState::NotApplicable, "no driver means no module question");
    }

    #[test]
    fn efi_records_show_a_hex_dump_and_no_driver_facts() {
        let mut s = DeviceProperties {
            device: Device {
                subsystem: "efivarfs".into(),
                name: "SecureBoot".into(),
                path: "/p".into(),
                parent_path: "/d".into(),
                category: "efi",
                ..Default::default()
            },
            state: ReadState::Available,
            efi_bytes: (0u8..20).collect(),
            efi_truncated: true,
            ..Default::default()
        };
        s.values.insert("efi/hex".into(), available(""));
        let entries = property_entries(&s, false);
        assert_eq!(find(&entries, "bus").state, ReadState::NotApplicable);
        assert_eq!(find(&entries, "location").value, "/d");
        assert!(entries.iter().all(|e| e.id != "driver/path"));
        let dump = &find(&entries, "efi/hex").value;
        assert!(dump.starts_with("00000000  00 01 02 03 04 05 06 07 08 09 0a 0b 0c 0d 0e 0f  ................\n00000010  10 11 12 13"));
        assert!(dump.ends_with("Truncated: first 65536 bytes shown."));
    }

    #[test]
    fn block_devices_keep_storage_identity_out_of_details() {
        let block = Device { subsystem: "block".into(), ..Default::default() };
        let entry = |id: &str| PropertyEntry {
            id: id.into(),
            label: String::new(),
            value: String::new(),
            source: String::new(),
            advanced: true,
            tab: 2,
            state: ReadState::Available,
            error: 0,
        };
        assert!(!is_details_property(&entry("udev/ID_FS_TYPE"), &block));
        assert!(!is_details_property(&entry("sysfs/size"), &block));
        assert!(is_details_property(&entry("udev/PCI_ID"), &block));
        assert!(is_details_property(&entry("udev/ID_FS_TYPE"), &Device { subsystem: "pci".into(), ..Default::default() }));
        assert!(!is_details_property(&PropertyEntry { tab: 3, ..entry("x") }, &Device::default()));
        let status = |state, error| details_property_status(&PropertyEntry { state, error, ..entry("x") });
        assert_eq!(status(ReadState::PermissionDenied, 13), "permission denied");
        assert_eq!(status(ReadState::Available, 0), "empty value");
    }

    #[test]
    fn storage_values_explain_themselves() {
        use crate::storage::{Format, StorageField};
        let f = |id: &str, value: &str, format| StorageField {
            id: id.into(),
            label: "L",
            value: Attribute { state: ReadState::Available, value: value.into(), error: 0 },
            source: String::new(),
            format,
        };
        assert_eq!(storage_field_value(&f("capacity", "1000204886016", Format::Bytes)), "1,000,204,886,016 bytes (931.51 GiB)");
        assert_eq!(storage_field_value(&f("x", "abc", Format::Bytes)), "Invalid capacity or sector-size metadata");
        assert_eq!(storage_field_value(&f("x", "1", Format::Boolean)), "Yes");
        assert_eq!(storage_field_value(&f("x", "2", Format::Boolean)), "Invalid boolean metadata");
        assert_eq!(storage_field_value(&f("x", "306", Format::Kelvin)), "32.9 °C");
        assert_eq!(storage_field_value(&f("org.freedesktop.UDisks2.NVMe.Controller/SmartTemperature", "306", Format::Kelvin)), "33 °C");
        assert_eq!(storage_field_value(&f("x", "0", Format::Kelvin)), "Unavailable");
        assert_eq!(storage_field_value(&f("x", "7200", Format::Seconds)), "7,200 seconds (2.0 hours)");
        assert_eq!(storage_field_value(&f("x", "87", Format::Percent)), "87%");
        assert_eq!(storage_field_value(&f("x", "0", Format::AtaHealth)), "No failure predicted by drive (not a guarantee of health)");
        assert_eq!(storage_field_value(&f("x", "", Format::NvmeHealth)), "No critical warnings reported (not a guarantee of health)");
        assert_eq!(storage_field_value(&f("mounts", "no-observed-mount", Format::Text)), "No mount observed in the application namespace");
        let unsupported = StorageField { value: Attribute { state: ReadState::Unsupported, ..Default::default() }, ..f("service", "", Format::Text) };
        assert_eq!(storage_field_value(&unsupported), "UDisks2 support is not enabled in this build");
        let skipped = StorageField {
            value: Attribute { state: ReadState::Unavailable, error: libc::ETIMEDOUT, ..Default::default() },
            ..f("h", "", Format::HealthQueryStatus)
        };
        assert_eq!(storage_field_value(&skipped), "Not queried: shared health-query time limit reached");
    }

    #[test]
    fn storage_snapshots_add_a_scope_note_and_tab_three_rows() {
        use crate::storage::{StorageEntity, StorageProperties};
        let mut s = snapshot();
        s.storage = StorageProperties {
            applicable: true,
            entities: vec![StorageEntity { id: "/sys/devices/x/sda".into(), ..Default::default() }],
            ..Default::default()
        };
        let entries = property_entries(&s, false);
        assert_eq!(find(&entries, "storage/scope").tab, 3);
        assert_eq!(find(&entries, "storage/entity//sys/devices/x/sda").label, "Storage entity");
        assert!(!is_details_property(find(&entries, "storage/scope"), &s.device));
    }
}
