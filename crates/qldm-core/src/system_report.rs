//! Text of the System Information window, ported from `src/ui/systemdialog.cpp` without toolkit
//! types: which fields each tab has, how every collected value is worded, the per-field source
//! tooltips, the MOK certificate blocks, the firmware memory records and the Copy text.
use crate::i18n::{arg, arg2, argn, format_data_size_with, format_fixed, format_integer, tr};
use crate::model::{Attribute, ReadState};
use crate::propertytext::property_read_value;
use crate::system::{FirmwareMemoryDevice, SystemProperties};

pub struct SystemField {
    pub key: &'static str,
    pub caption: String,
}

pub struct SystemSection {
    pub title: String,
    pub fields: Vec<SystemField>,
}

fn field(key: &'static str, caption: &str) -> SystemField {
    SystemField { key, caption: tr(caption) }
}

/// Tabs and their fields, in display order. Hardware also carries the MOK certificate block and
/// RAM/Memory the firmware memory records; both are variable-length and built separately.
pub fn sections() -> Vec<SystemSection> {
    vec![
        SystemSection {
            title: tr("Operating System"),
            fields: vec![
                field("hostname", "Hostname"),
                field("os", "Linux distribution"),
                field("architecture", "Kernel architecture"),
                field("kernel", "Kernel version"),
                field("build", "Kernel build"),
                field("boot", "Boot mode"),
                field("secure_boot", "Secure Boot"),
                field("virtualization", "Virtualization environment"),
                field("uptime", "Uptime"),
                field("gpu_rebar", "Resizable BAR (GPU)"),
            ],
        },
        SystemSection {
            title: tr("Hardware"),
            fields: vec![
                field("manufacturer", "System manufacturer"),
                field("model", "System model"),
                field("board_vendor", "Motherboard manufacturer"),
                field("board_model", "Motherboard model"),
                field("firmware", "Firmware version"),
                field("firmware_date", "Firmware date (reported)"),
            ],
        },
        SystemSection {
            title: tr("Processor"),
            fields: vec![
                field("cpu", "CPU model(s)"),
                field("sockets", "Sockets (reported topology)"),
                field("cores", "Physical cores (reported topology)"),
                field("logical", "Logical CPUs present"),
                field("online", "Logical CPUs online"),
                field("caches", "CPU caches (online CPUs)"),
                field("instruction_sets", "Instruction sets / CPU features"),
            ],
        },
        SystemSection { title: tr("RAM/Memory"), fields: vec![field("physical_memory", "Physical RAM"), field("memory", "Usable RAM")] },
    ]
}

const MEMORY_FIELDS: [(&str, &str); 12] = [
    ("PRESENT", "Presence"),
    ("SIZE", "Capacity (total)"),
    ("VOLATILE_SIZE", "Volatile capacity"),
    ("NON_VOLATILE_SIZE", "Nonvolatile capacity"),
    ("LOCATOR", "Device locator"),
    ("BANK_LOCATOR", "Bank locator"),
    ("TYPE", "Memory type"),
    ("FORM_FACTOR", "Form factor"),
    ("SPEED_MTS", "Rated transfer rate"),
    ("CONFIGURED_SPEED_MTS", "Configured transfer rate"),
    ("MANUFACTURER", "Manufacturer"),
    ("PART_NUMBER", "Part number"),
];

pub fn memory_scope() -> String {
    tr("Firmware-reported records; these do not establish removable modules, physical slots, memory channels, upgradeability or active ECC. Transfer rates are firmware declarations, not live measurements.")
}

fn bytes_text(bytes: u64, precision: usize) -> String {
    if bytes <= i64::MAX as u64 {
        format_data_size_with(bytes, precision)
    } else {
        arg(tr("%1 bytes"), &format_integer(bytes))
    }
}

pub fn memory_value(key: &str, value: &Attribute) -> String {
    if key == "PRESENT" && value.state == ReadState::Unavailable {
        return tr("Unknown (no explicit presence flag)");
    }
    if value.state != ReadState::Available {
        return property_read_value(value);
    }
    match key {
        "PRESENT" => {
            if value.value == "0" {
                tr("No device reported present")
            } else {
                tr("Device reported present")
            }
        }
        "SPEED_MTS" | "CONFIGURED_SPEED_MTS" => arg(tr("%1 MT/s"), &format_integer(value.value.parse().unwrap_or(0))),
        "SIZE" | "VOLATILE_SIZE" | "NON_VOLATILE_SIZE" => bytes_text(value.value.parse().unwrap_or(0), 2),
        _ => value.value.clone(),
    }
}

fn memory_lines(device: &FirmwareMemoryDevice) -> Vec<String> {
    MEMORY_FIELDS
        .iter()
        .map(|(key, caption)| format!("{}: {}", tr(caption), memory_value(key, &device.values.get(*key).cloned().unwrap_or_default())))
        .collect()
}

fn iec(bytes: &Attribute) -> String {
    if bytes.state != ReadState::Available {
        return property_read_value(bytes);
    }
    bytes_text(bytes.value.parse().unwrap_or(0), 0)
}

fn rebar_text(snapshot: &SystemProperties) -> String {
    if snapshot.gpu_rebar_status.state != ReadState::Available {
        return property_read_value(&snapshot.gpu_rebar_status);
    }
    if snapshot.gpu_rebar.is_empty() {
        return tr("No PCI display controller reported");
    }
    let mut devices = Vec::new();
    for gpu in &snapshot.gpu_rebar {
        let mut lines = vec![arg(tr("GPU [%1]"), &gpu.pci_address)];
        if gpu.status.state == ReadState::Unavailable {
            lines.push(tr("Capability: Not exposed by kernel"));
        } else if gpu.status.state != ReadState::Available {
            lines.push(property_read_value(&gpu.status));
        } else {
            lines.push(tr("Capability: Supported"));
            for (bar, bitmap) in &gpu.supported_sizes {
                let supported = if bitmap.state == ReadState::Available {
                    let bits = u64::from_str_radix(&bitmap.value, 16).unwrap_or(0);
                    (0..44).filter(|bit| bits & (1u64 << bit) != 0).map(|bit| bytes_text(1u64 << (bit + 20), 0)).collect::<Vec<_>>().join(", ")
                } else {
                    property_read_value(bitmap)
                };
                let current = iec(&gpu.current_sizes.get(bar).cloned().unwrap_or_default());
                lines.push(argn(tr("BAR %1 — Current allocation: %2; supported sizes: %3"), &[&bar.to_string(), &current, &supported]));
            }
        }
        lines.push(tr("BIOS setting: Unknown"));
        devices.push(lines.join("\n"));
    }
    devices.join("\n\n")
}

fn known(text: &str) -> String {
    if text.is_empty() {
        tr("Unavailable")
    } else {
        text.to_string()
    }
}

/// Wording of one field's value (also used for the Copy text).
pub fn display_value(snapshot: &SystemProperties, key: &str, value: &Attribute) -> String {
    if key == "virtualization" && value.state == ReadState::Unavailable {
        return tr("Unknown (no hypervisor identity reported)");
    }
    if key == "memory_devices" {
        let mut lines = vec![memory_scope()];
        let memory = &snapshot.memory_devices;
        if memory.status.state != ReadState::Available {
            lines.push(property_read_value(&memory.status));
        } else if memory.devices.is_empty() {
            lines.push(tr("No memory-device records reported"));
        } else {
            for device in &memory.devices {
                lines.push(arg2(tr("Memory device %1 (firmware record %2)"), &(device.index + 1).to_string(), &device.index.to_string()));
                lines.extend(memory_lines(device));
            }
        }
        return lines.join("\n");
    }
    if key == "gpu_rebar" {
        return rebar_text(snapshot);
    }
    if key == "mok" {
        let mok = &snapshot.mok;
        if mok.status.state != ReadState::Available {
            return property_read_value(&mok.status);
        }
        let mut certificates: Vec<String> = mok
            .certificates
            .iter()
            .map(|c| argn(tr("Owner: %1\nIssuer: %2\nExpires: %3"), &[&known(&c.subject), &known(&c.issuer), &known(&c.expires)]))
            .collect();
        if certificates.is_empty() {
            certificates.push(tr("No X.509 certificates in the exposed MOK list"));
        }
        if mok.other_signatures > 0 {
            certificates.push(arg(tr("%1 non-certificate signatures omitted"), &mok.other_signatures.to_string()));
        }
        return certificates.join("\n\n");
    }
    if value.state != ReadState::Available {
        return property_read_value(value);
    }
    match key {
        "secure_boot" => {
            if value.value == "enabled" {
                tr("Enabled")
            } else {
                tr("Disabled")
            }
        }
        "boot" => {
            if value.value == "uefi" {
                tr("UEFI")
            } else {
                tr("UEFI not exposed (legacy boot or restricted environment)")
            }
        }
        "virtualization" => arg(tr("Reported hypervisor: %1"), &value.value),
        "memory" | "physical_memory" => {
            const GIB: u64 = 1024 * 1024 * 1024;
            const MIB: u64 = 1024 * 1024;
            let bytes: u64 = value.value.parse().unwrap_or(0);
            let precision = if key == "physical_memory" && bytes.is_multiple_of(GIB) { 0 } else { 2 };
            let mut text = arg(tr("%1 GiB"), &format_fixed(bytes as f64 / GIB as f64, precision));
            let physical = snapshot.values.get("physical_memory").cloned().unwrap_or_default();
            if key == "memory" && physical.state == ReadState::Available {
                let installed: u64 = physical.value.parse().unwrap_or(0);
                if installed >= bytes {
                    text += &arg(tr(" (%1 MiB is system reserved)"), &format_fixed((installed - bytes) as f64 / MIB as f64, 0));
                }
            }
            text
        }
        "uptime" => {
            let seconds: u64 = value.value.parse().unwrap_or(0);
            argn(tr("%1 days, %2 hours, %3 minutes"), &[&(seconds / 86400).to_string(), &(seconds / 3600 % 24).to_string(), &(seconds / 60 % 60).to_string()])
        }
        "instruction_sets" => {
            let groups: Vec<&str> = value.value.split('\n').collect();
            groups
                .iter()
                .map(|group| {
                    let mut parts = group.split('\t');
                    let (cpus, features) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                    if groups.len() == 1 {
                        features.to_string()
                    } else {
                        arg2(tr("Logical CPUs %1: %2"), cpus, features)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n\n")
        }
        "caches" => value
            .value
            .split('\n')
            .map(|raw| {
                let parts: Vec<&str> = raw.split('\t').collect();
                let kind = parts.first().copied().unwrap_or("");
                let (level, ty) = kind.split_once(' ').unwrap_or((kind, ""));
                let ty = match ty {
                    "Data" => tr("data"),
                    "Instruction" => tr("instruction"),
                    "Unified" => tr("unified"),
                    other => other.to_string(),
                };
                let size = format_data_size_with(parts.get(1).and_then(|p| p.parse().ok()).unwrap_or(0), 2);
                argn(tr("%1 %2: %3 (%4 unique instances)"), &[level, &ty, &size, parts.get(2).copied().unwrap_or("")])
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => value.value.clone(),
    }
}

/// Tooltip text for a field: where it came from, plus what it does not establish.
pub fn field_source(snapshot: &SystemProperties, key: &str) -> String {
    let mut source = snapshot.sources.get(key).cloned().unwrap_or_default();
    let extra = match key {
        "secure_boot" => Some(tr("Firmware SecureBoot state; does not establish shim validation or kernel lockdown policy.")),
        "gpu_rebar" => Some(tr("GPU capability and current OS resource allocation are separate from the firmware setting. A small BAR is not proof of disabled ReBAR; a large BAR is not proof of driver use. No portable BIOS Enabled/Disabled value is inferred. Only sysfs metadata is read; BAR memory is never accessed.")),
        "instruction_sets" => Some(tr("Kernel-exposed instruction-set and CPU-feature flags for processors listed in /proc/cpuinfo. These include non-instruction features and may be limited by the kernel, operating system or hypervisor. Different reported feature lists remain separate; this is not a complete raw hardware capability inventory.")),
        "memory" => Some(tr("System reserved is reported physical RAM minus Linux MemTotal. It includes all memory unavailable to Linux, not just firmware reservations. It is omitted if physical capacity is unavailable or smaller than usable RAM.")),
        _ => None,
    };
    if let Some(extra) = extra {
        source.push('\n');
        source.push_str(&extra);
    }
    source
}

/// One block of the firmware memory section.
pub enum MemoryBlock {
    Message { text: String, tooltip: String },
    Heading(String),
    Field { caption: String, value: String, tooltip: String },
}

pub fn memory_blocks(snapshot: &SystemProperties) -> Vec<MemoryBlock> {
    let memory = &snapshot.memory_devices;
    if memory.status.state != ReadState::Available {
        return vec![MemoryBlock::Message {
            text: property_read_value(&memory.status),
            tooltip: snapshot.sources.get("memory_devices").cloned().unwrap_or_default(),
        }];
    }
    let mut blocks = Vec::new();
    if memory.devices.is_empty() {
        blocks.push(MemoryBlock::Message { text: tr("No memory-device records reported"), tooltip: String::new() });
    }
    for device in &memory.devices {
        blocks.push(MemoryBlock::Heading(arg(tr("Memory device %1"), &(device.index + 1).to_string())));
        for (key, caption) in MEMORY_FIELDS {
            blocks.push(MemoryBlock::Field {
                caption: tr(caption),
                value: memory_value(key, &device.values.get(key).cloned().unwrap_or_default()),
                tooltip: format!("libudev: dmi/id MEMORY_DEVICE_{}_{key}\n{}", device.index, memory_scope()),
            });
        }
    }
    blocks
}

pub fn mok_scope(snapshot: &SystemProperties) -> String {
    format!(
        "{}\n{}",
        snapshot.sources.get("mok").cloned().unwrap_or_default(),
        tr("Exposed runtime MOK certificates. Issuer and expiration are certificate metadata, not verification of the signer or proof that this key signed the running kernel. Dates are UTC. Firmware db certificates and pending enrollments are not included.")
    )
}

/// One block of the MOK section.
pub enum MokBlock {
    Message(String),
    Certificate { title: String, fields: Vec<(String, String, String)> },
}

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// `15 Oct 2030, 00:00:00 UTC` from an ISO UTC time, or the raw text when it is not one.
pub fn certificate_date(iso: &str) -> String {
    let parts = (|| {
        let (date, time) = iso.strip_suffix('Z')?.split_once('T')?;
        let mut d = date.split('-');
        let (year, month, day) = (d.next()?, d.next()?.parse::<usize>().ok()?, d.next()?.parse::<u32>().ok()?);
        MONTHS.get(month.checked_sub(1)?).map(|m| format!("{day} {m} {year}, {time}"))
    })();
    match parts {
        Some(text) => arg(tr("%1 UTC"), &text),
        None => known(iso),
    }
}

pub fn mok_blocks(snapshot: &SystemProperties) -> Vec<MokBlock> {
    let mok = &snapshot.mok;
    if mok.status.state != ReadState::Available || mok.certificates.is_empty() {
        return vec![MokBlock::Message(display_value(snapshot, "mok", &Attribute::default()))];
    }
    let mut blocks = Vec::new();
    for (index, certificate) in mok.certificates.iter().enumerate() {
        let title = if mok.certificates.len() == 1 { tr("MOK Certificate") } else { arg(tr("MOK Certificate %1"), &(index + 1).to_string()) };
        let owner = if certificate.subject_display.is_empty() { &certificate.subject } else { &certificate.subject_display };
        let issuer = if !certificate.subject.is_empty() && certificate.subject == certificate.issuer {
            tr("Same as owner")
        } else if certificate.issuer_display.is_empty() {
            certificate.issuer.clone()
        } else {
            certificate.issuer_display.clone()
        };
        blocks.push(MokBlock::Certificate {
            title,
            fields: vec![
                (tr("Owner:"), known(owner), certificate.subject.clone()),
                (tr("Issuer:"), known(&issuer), certificate.issuer.clone()),
                (tr("Expires:"), certificate_date(&certificate.expires), certificate.expires.clone()),
            ],
        });
    }
    if mok.other_signatures > 0 {
        blocks.push(MokBlock::Message(arg(tr("%1 non-certificate signatures omitted"), &mok.other_signatures.to_string())));
    }
    blocks
}

/// What "Copy all" places on the clipboard: every field as `Caption: value`.
pub fn copy_all(snapshot: &SystemProperties) -> String {
    let mut lines = Vec::new();
    for section in sections() {
        for f in &section.fields {
            lines.push(format!("{}: {}", f.caption, display_value(snapshot, f.key, &snapshot.values.get(f.key).cloned().unwrap_or_default())));
        }
        if section.fields.first().is_some_and(|f| f.key == "manufacturer") {
            lines.push(format!("{}: {}", tr("MOK certificates"), display_value(snapshot, "mok", &Attribute::default())));
        }
        if section.fields.first().is_some_and(|f| f.key == "physical_memory") {
            lines.push(format!("{}: {}", tr("RAM/Memory"), display_value(snapshot, "memory_devices", &Attribute::default())));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secureboot::{MokCertificate, MokCertificates};
    use crate::system::{FirmwareMemoryInformation, GpuResizableBar};
    use std::collections::{BTreeMap, HashMap};

    fn available(value: &str) -> Attribute {
        Attribute { state: ReadState::Available, value: value.into(), error: 0 }
    }
    fn snapshot() -> SystemProperties {
        let mut s = SystemProperties::default();
        for (k, v) in [
            ("hostname", "box"),
            ("os", "openSUSE Tumbleweed"),
            ("secure_boot", "enabled"),
            ("boot", "uefi"),
            ("uptime", "190861"),
            ("memory", "33370574848"),
            ("physical_memory", "34359738368"),
            ("virtualization", "KVM"),
            ("instruction_sets", "0, 1\tavx2 sse\n2\tsse"),
            ("caches", "L1 Data\t65536\t8\nL3 Unified\t33554432\t1"),
        ] {
            s.values.insert(k.into(), available(v));
        }
        s.sources.insert("memory".into(), "/proc/meminfo".into());
        s
    }
    fn value(s: &SystemProperties, key: &str) -> String {
        display_value(s, key, &s.values.get(key).cloned().unwrap_or_default())
    }

    #[test]
    fn plain_fields_are_worded_for_people() {
        let s = snapshot();
        assert_eq!(value(&s, "secure_boot"), "Enabled");
        assert_eq!(value(&s, "boot"), "UEFI");
        assert_eq!(value(&s, "uptime"), "2 days, 5 hours, 1 minutes");
        assert_eq!(value(&s, "physical_memory"), "32 GiB");
        assert_eq!(value(&s, "memory"), "31.08 GiB (943 MiB is system reserved)");
        assert_eq!(value(&s, "virtualization"), "Reported hypervisor: KVM");
        assert_eq!(value(&s, "instruction_sets"), "Logical CPUs 0, 1: avx2 sse\n\nLogical CPUs 2: sse");
        assert_eq!(value(&s, "caches"), "L1 data: 64.00 KiB (8 unique instances)\nL3 unified: 32.00 MiB (1 unique instances)");
        let mut single = snapshot();
        single.values.insert("instruction_sets".into(), available("0, 1\tavx2 sse"));
        assert_eq!(value(&single, "instruction_sets"), "avx2 sse", "one group needs no CPU prefix");
        let none = Attribute::default();
        assert_eq!(display_value(&s, "virtualization", &none), "Unknown (no hypervisor identity reported)");
        assert_eq!(display_value(&s, "boot", &available("not-exposed")), "UEFI not exposed (legacy boot or restricted environment)");
        assert_eq!(display_value(&s, "os", &none), "Unavailable");
        assert!(field_source(&s, "memory").starts_with("/proc/meminfo\nSystem reserved is reported physical RAM"));
        assert_eq!(field_source(&s, "hostname"), "");
    }

    #[test]
    fn system_reserved_memory_is_omitted_when_not_meaningful() {
        let mut s = snapshot();
        s.values.insert("physical_memory".into(), Attribute::default());
        assert_eq!(value(&s, "memory"), "31.08 GiB");
        s.values.insert("physical_memory".into(), available("1000"));
        assert_eq!(value(&s, "memory"), "31.08 GiB", "physical smaller than usable");
        s.values.insert("physical_memory".into(), available("34359738369"));
        assert_eq!(value(&s, "physical_memory"), "32.00 GiB", "a non-whole GiB keeps two decimals");
    }

    #[test]
    fn resizable_bar_lists_each_gpu_with_sizes() {
        let mut s = snapshot();
        s.gpu_rebar_status = available("1");
        s.gpu_rebar.push(GpuResizableBar {
            pci_address: "0000:03:00.0".into(),
            status: available("reported"),
            supported_sizes: BTreeMap::from([(0, available("1c0")), (2, Attribute::default())]),
            current_sizes: BTreeMap::from([(0, available("268435456"))]),
        });
        let text = display_value(&s, "gpu_rebar", &Attribute::default());
        assert!(text.starts_with("GPU [0000:03:00.0]\nCapability: Supported\n"), "{text}");
        assert!(text.contains("BAR 0 — Current allocation: 256 MiB; supported sizes: 64 MiB, 128 MiB, 256 MiB"), "{text}");
        assert!(text.contains("BAR 2 — Current allocation: Unavailable; supported sizes: Unavailable"), "{text}");
        assert!(text.ends_with("BIOS setting: Unknown"));
        s.gpu_rebar[0].status = Attribute::default();
        assert!(display_value(&s, "gpu_rebar", &Attribute::default()).contains("Capability: Not exposed by kernel"));
        s.gpu_rebar.clear();
        assert_eq!(display_value(&s, "gpu_rebar", &Attribute::default()), "No PCI display controller reported");
        s.gpu_rebar_status = Attribute { state: ReadState::PermissionDenied, ..Default::default() };
        assert_eq!(display_value(&s, "gpu_rebar", &Attribute::default()), "Permission denied");
    }

    #[test]
    fn mok_certificates_show_owner_issuer_and_utc_dates() {
        let mut s = snapshot();
        s.mok = MokCertificates {
            status: available(""),
            certificates: vec![MokCertificate {
                subject: "CN=Example Key".into(),
                issuer: "CN=Example Key".into(),
                expires: "2030-01-05T04:03:02Z".into(),
                subject_display: "Example Key".into(),
                issuer_display: "Example Key".into(),
            }],
            other_signatures: 2,
        };
        let blocks = mok_blocks(&s);
        let MokBlock::Certificate { title, fields } = &blocks[0] else { panic!("expected a certificate") };
        assert_eq!(title, "MOK Certificate");
        assert_eq!(fields[0].1, "Example Key");
        assert_eq!(fields[1].1, "Same as owner");
        assert_eq!(fields[2].1, "5 Jan 2030, 04:03:02 UTC");
        assert!(matches!(&blocks[1], MokBlock::Message(m) if m == "2 non-certificate signatures omitted"));
        assert!(display_value(&s, "mok", &Attribute::default()).starts_with("Owner: CN=Example Key\nIssuer: CN=Example Key\nExpires: 2030-01-05T04:03:02Z"));
        s.mok.certificates.push(s.mok.certificates[0].clone());
        let MokBlock::Certificate { title, .. } = &mok_blocks(&s)[1] else { panic!() };
        assert_eq!(title, "MOK Certificate 2");
        assert_eq!(certificate_date("garbage"), "garbage");
        assert_eq!(certificate_date(""), "Unavailable");
        s.mok = MokCertificates { status: available(""), ..Default::default() };
        assert_eq!(display_value(&s, "mok", &Attribute::default()), "No X.509 certificates in the exposed MOK list");
        s.mok.status = Attribute { state: ReadState::PermissionDenied, ..Default::default() };
        assert!(matches!(&mok_blocks(&s)[0], MokBlock::Message(m) if m == "Permission denied"));
        assert!(mok_scope(&s).contains("Dates are UTC."));
    }

    #[test]
    fn firmware_memory_records_keep_unknowns_unknown() {
        let mut s = snapshot();
        let mut device = FirmwareMemoryDevice { index: 0, values: HashMap::new() };
        for (k, v) in [("SIZE", "17179869184"), ("PRESENT", "1"), ("SPEED_MTS", "8533"), ("LOCATOR", "DIMM A")] {
            device.values.insert(k.into(), available(v));
        }
        s.memory_devices = FirmwareMemoryInformation { status: available("1"), devices: vec![device.clone()] };
        let blocks = memory_blocks(&s);
        assert!(matches!(&blocks[0], MemoryBlock::Heading(h) if h == "Memory device 1"));
        let get = |caption: &str| {
            blocks.iter().find_map(|b| match b {
                MemoryBlock::Field { caption: c, value, .. } if c == caption => Some(value.clone()),
                _ => None,
            })
        };
        assert_eq!(get("Capacity (total)").as_deref(), Some("16.00 GiB"));
        assert_eq!(get("Rated transfer rate").as_deref(), Some("8,533 MT/s"));
        assert_eq!(get("Presence").as_deref(), Some("Device reported present"));
        assert_eq!(get("Part number").as_deref(), Some("Unavailable"));
        device.values.remove("PRESENT");
        assert_eq!(memory_value("PRESENT", &Attribute::default()), "Unknown (no explicit presence flag)");
        let text = display_value(&s, "memory_devices", &Attribute::default());
        assert!(text.contains("Memory device 1 (firmware record 0)\nPresence: Device reported present"), "{text}");
        s.memory_devices = FirmwareMemoryInformation { status: available("0"), devices: vec![] };
        assert!(matches!(&memory_blocks(&s)[0], MemoryBlock::Message { text, .. } if text == "No memory-device records reported"));
        s.memory_devices.status = Attribute::default();
        assert!(matches!(&memory_blocks(&s)[0], MemoryBlock::Message { text, .. } if text == "Unavailable"));
        assert_eq!(memory_value("SIZE", &available("18446744073709551615")), "18,446,744,073,709,551,615 bytes");
    }

    #[test]
    fn copy_all_lists_every_field_in_tab_order_with_the_variable_sections() {
        let s = snapshot();
        let text = copy_all(&s);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "Hostname: box");
        assert!(lines.contains(&"Secure Boot: Enabled") && lines.contains(&"Uptime: 2 days, 5 hours, 1 minutes"));
        let at = |prefix: &str| lines.iter().position(|l| l.starts_with(prefix)).unwrap_or_else(|| panic!("missing {prefix}"));
        assert!(at("System manufacturer") < at("MOK certificates") && at("MOK certificates") < at("CPU model(s)"));
        assert!(at("Usable RAM") < at("RAM/Memory: Firmware-reported records"));
        assert_eq!(sections().len(), 4);
    }
}
