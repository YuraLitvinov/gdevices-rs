//! Static function captions for devices without model metadata, ported from
//! `src/devices/functionnames.cpp`. Only exact standard identifiers match.
use crate::model::Device;

#[derive(Debug, PartialEq, Eq)]
pub struct FunctionName {
    /// Untranslated English source text; the UI translates.
    pub label: &'static str,
    pub source: String,
}

const ACPI_IDS: [(&str, &str); 23] = [
    ("ACPI0003", "ACPI power source"),
    ("ACPI0007", "ACPI processor device"),
    ("ACPI0008", "Ambient light sensor"),
    ("ACPI000C", "ACPI processor aggregator"),
    ("ACPI000E", "ACPI time and alarm device"),
    ("ACPI000F", "User presence detector"),
    ("PNP0103", "High precision event timer"),
    ("PNP0303", "PS/2 keyboard interface"),
    ("PNP0A03", "PCI bus"),
    ("PNP0A08", "PCI Express bus"),
    ("PNP0B00", "Real-time clock"),
    ("PNP0C02", "Motherboard resources"),
    ("PNP0C09", "ACPI embedded controller"),
    ("PNP0C0A", "ACPI control-method battery"),
    ("PNP0C0B", "ACPI fan"),
    ("PNP0C0C", "ACPI power button"),
    ("PNP0C0D", "ACPI lid switch"),
    ("PNP0C0E", "ACPI sleep button"),
    ("PNP0C14", "ACPI WMI interface"),
    ("PNP0C50", "I2C human interface device"),
    ("PNP0C60", "Display sensor"),
    ("LNXTHERM", "ACPI thermal zone"),
    ("LNXVIDEO", "ACPI video controller"),
];

const PLATFORM_ALIASES: [(&str, &str); 3] = [
    ("platform:coretemp", "Intel CPU temperature monitor"),
    ("platform:rtc-efi", "EFI real-time clock interface"),
    ("platform:alarmtimer", "Kernel alarm timer"),
];

const PCI_SERVICES: [(&str, &str); 4] = [
    ("aer", "PCI Express advanced error reporting service"),
    ("pcie_bwctrl", "PCI Express bandwidth control service"),
    ("pcie_pme", "PCI Express power-management event service"),
    ("pciehp", "PCI Express hot-plug service"),
];

const FAUX_FUNCTIONS: [(&str, &str); 4] = [
    ("microcode", "CPU microcode interface"),
    ("reg-dummy", "Dummy voltage regulator"),
    ("regulatory", "Wireless regulatory interface"),
    ("snd-soc-dummy", "Dummy SoC audio component"),
];

fn prop<'a>(d: &'a Device, key: &str) -> &'a str {
    d.properties.get(key).map_or("", String::as_str)
}

/// Verified Linux USB root hubs only; external hubs have port-style names.
pub fn usb_root_hub(d: &Device) -> Option<FunctionName> {
    if d.subsystem != "usb" || d.devtype != "usb_device" {
        return None;
    }
    let bus: u32 = d.sysname.strip_prefix("usb")?.parse().ok()?;
    let address: u32 = prop(d, "DEVNUM").parse().ok()?;
    let product: Vec<&str> = prop(d, "PRODUCT").split('/').collect();
    if product.len() != 3 || bus == 0 || address != 1 {
        return None;
    }
    let vendor = u32::from_str_radix(product[0], 16).ok()?;
    let id = u32::from_str_radix(product[1], 16).ok()?;
    if vendor != 0x1d6b {
        return None;
    }
    let label = match id {
        1 => "USB 1.1 root hub",
        2 => "USB 2.0 root hub",
        3 => "USB 3.x root hub",
        _ => return None,
    };
    Some(FunctionName { label, source: "Linux USB root-hub topology and descriptor identity".into() })
}

pub fn known_function_name(d: &Device) -> Option<FunctionName> {
    if let Some(hub) = usb_root_hub(d) {
        return Some(hub);
    }
    let alias = prop(d, "MODALIAS");
    let identifiers: Vec<&str> = match d.subsystem.as_str() {
        "acpi" | "platform" | "i2c" if alias.starts_with("acpi:") => alias[5..].split(':').filter(|s| !s.is_empty()).collect(),
        // PNP modaliases encode each identifier as d<ID>, including compatible IDs.
        "pnp" if alias.starts_with("pnp:") => alias[4..].split('d').filter(|s| !s.is_empty()).collect(),
        _ => Vec::new(),
    };
    for id in identifiers {
        if let Some((_, label)) = ACPI_IDS.iter().find(|(known, _)| known.eq_ignore_ascii_case(id)) {
            return Some(FunctionName { label, source: format!("standard ACPI/PNP ID: {id}") });
        }
    }
    let table = |entries: &[(&'static str, &'static str)], key: &str| entries.iter().find(|(id, _)| *id == key).map(|(_, label)| *label);
    if d.subsystem == "platform" {
        if let Some(label) = table(&PLATFORM_ALIASES, alias) {
            return Some(FunctionName { label, source: format!("documented kernel platform alias: {alias}") });
        }
    }
    if d.subsystem == "pci_express" {
        if let Some(label) = table(&PCI_SERVICES, &d.driver) {
            return Some(FunctionName { label, source: format!("PCI Express service; direct driver: {}", d.driver) });
        }
    }
    if d.subsystem == "faux" {
        if let Some(label) = table(&FAUX_FUNCTIONS, &d.sysname) {
            return Some(FunctionName { label, source: format!("documented kernel faux function: {}", d.sysname) });
        }
    }
    if d.subsystem == "serial-base" && d.driver == "port" {
        return Some(FunctionName { label: "Serial port function", source: "serial-base subsystem; direct port binding".into() });
    }
    if d.subsystem == "serial-base" && d.driver == "ctrl" {
        return Some(FunctionName { label: "Serial controller function", source: "serial-base subsystem; direct ctrl binding".into() });
    }
    if d.subsystem == "bluetooth" && d.sysname.starts_with("hci") && !d.sysname.contains(':') {
        return Some(FunctionName { label: "Bluetooth host controller", source: "Bluetooth subsystem; host-controller interface".into() });
    }
    None
}
