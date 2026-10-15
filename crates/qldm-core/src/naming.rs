//! Device naming priority chain, ported from `nameDevice()` in `src/devices/device.cpp`.
//! Direct identity/model metadata wins over static captions, which win over kernel names.
use crate::functionnames::{known_function_name, usb_root_hub};
use crate::model::{Device, ReadState};

/// Uses the device's own sysfs attribute as its name, only when the read succeeded.
fn attribute_name(d: &mut Device, key: &str) -> bool {
    match d.attributes.get(key) {
        Some(a) if a.state == ReadState::Available && !a.value.is_empty() => {
            d.name = a.value.clone();
            d.name_source = format!("sysfs: {}/{key}", d.path);
            true
        }
        _ => false,
    }
}

fn available(d: &Device, key: &str) -> Option<String> {
    d.attributes.get(key).filter(|a| a.state == ReadState::Available && !a.value.is_empty()).map(|a| a.value.clone())
}

pub fn name_device(d: &mut Device) {
    d.name_translated = false;
    d.name_candidate = false;
    let subsystem = d.subsystem.clone();

    if subsystem == "cpu" {
        if let Some(model) = available(d, "cpu_model") {
            d.name = model;
            d.name_source = format!("/proc/cpuinfo: processor {}", d.sysname.get(3..).unwrap_or(""));
            return;
        }
    }
    // Function names take priority over generic USB receiver/camera model names.
    if (subsystem == "input" || subsystem == "video4linux") && attribute_name(d, "name") {
        return;
    }
    if subsystem == "input" {
        let raw = d.properties.get("NAME").cloned().unwrap_or_default();
        let name = if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') { raw[1..raw.len() - 1].to_string() } else { raw };
        if !name.is_empty() {
            d.name = name;
            d.name_source = "udev: NAME".into();
            return;
        }
    }
    if subsystem == "video4linux" {
        if let Some(name) = d.properties.get("ID_V4L_PRODUCT").filter(|n| !n.is_empty()) {
            d.name = name.clone();
            d.name_source = "udev: ID_V4L_PRODUCT".into();
            return;
        }
    }
    if subsystem == "nvme" && attribute_name(d, "model") {
        return;
    }
    if subsystem == "hdaudio" {
        if let Some(chip) = available(d, "chip_name") {
            d.name = match available(d, "vendor_name") {
                Some(vendor) => format!("{vendor} {chip}"),
                None => chip,
            };
            d.name_source = format!("sysfs: {}/chip_name and available vendor_name", d.path);
            return;
        }
    }
    // Replace generic database root-hub text only for verified Linux root hubs.
    if subsystem == "usb" {
        if let Some(hub) = usb_root_hub(d) {
            d.name = hub.label.into();
            d.name_source = hub.source;
            d.name_translated = true;
            return;
        }
    }
    for key in ["ID_MODEL_FROM_DATABASE", "ID_MODEL", "NAME"] {
        if key == "ID_MODEL" && subsystem == "usb" && d.devtype == "usb_device" && attribute_name(d, "product") {
            return;
        }
        if let Some(value) = d.properties.get(key).filter(|v| !v.is_empty()) {
            d.name = value.clone();
            d.name_source = d.property_sources.get(key).cloned().unwrap_or_else(|| format!("udev: {key}"));
            return;
        }
    }
    if subsystem == "sound" && attribute_name(d, "id") {
        return;
    }
    if subsystem == "power_supply" && attribute_name(d, "model_name") {
        return;
    }
    if let Some(function) = known_function_name(d) {
        d.name = function.label.into();
        d.name_source = function.source;
        d.name_translated = true;
        return;
    }
    d.name = if d.sysname.is_empty() { d.path.clone() } else { d.sysname.clone() };
    d.name_source = "kernel name".into();
    // Describe unresolved bound functions using real binding evidence, without
    // claiming that a module/driver name is the hardware's marketing model.
    if !d.driver.is_empty() && subsystem != "cpu" {
        d.name = format!("{} — {}", d.driver, d.name);
        d.name_source = "direct kernel driver and kernel name".into();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Attribute;

    fn attr(state: ReadState, value: &str) -> Attribute {
        Attribute { state, value: value.into(), error: 0 }
    }
    fn dev(subsystem: &str) -> Device {
        Device { subsystem: subsystem.into(), ..Default::default() }
    }

    #[test]
    fn unknown_bound_function_gets_readable_fallback() {
        let mut d = Device { driver: "example".into(), sysname: "device0".into(), ..dev("unknown_bus") };
        name_device(&mut d);
        assert_eq!(d.name, "example — device0");
        assert!(!d.name_translated);
    }

    #[test]
    fn cpu_name_identifies_source_and_failed_reads_do_not_resolve() {
        let mut cpu = Device { sysname: "cpu0".into(), ..dev("cpu") };
        cpu.attributes.insert("cpu_model".into(), attr(ReadState::Available, "Intel(R) Core(TM) Ultra X7 358H"));
        name_device(&mut cpu);
        assert_eq!(cpu.name, "Intel(R) Core(TM) Ultra X7 358H");
        assert!(cpu.name_source.contains("processor 0"));
        cpu.attributes.insert("cpu_model".into(), attr(ReadState::PermissionDenied, "Intel"));
        name_device(&mut cpu);
        assert_eq!(cpu.name, "cpu0");
    }

    #[test]
    fn input_function_name_beats_generic_model() {
        let mut d = Device { sysname: "input14".into(), ..dev("input") };
        d.properties.insert("NAME".into(), "\"SYNA3517:00 06CB:CFC6 Touchpad\"".into());
        d.properties.insert("ID_MODEL".into(), "Generic receiver".into());
        name_device(&mut d);
        assert_eq!(d.name, "SYNA3517:00 06CB:CFC6 Touchpad");
    }

    #[test]
    fn acpi_identities_use_exact_ids_only() {
        let mut d = Device { sysname: "PNP0C09:00".into(), ..dev("platform") };
        d.properties.insert("MODALIAS".into(), "acpi:PNP0C09:".into());
        name_device(&mut d);
        assert_eq!((d.name.as_str(), d.name_translated), ("ACPI embedded controller", true));
        d.properties.insert("MODALIAS".into(), "acpi:INT340E:PNP0C02:".into());
        name_device(&mut d);
        assert_eq!(d.name, "Motherboard resources");
        assert!(d.name_source.ends_with("PNP0C02"));
        d.properties.insert("MODALIAS".into(), "acpi:UNKNOWNPNP0C02:".into());
        name_device(&mut d);
        assert_eq!((d.name.as_str(), d.name_translated), ("PNP0C09:00", false));
        d.properties.insert("MODALIAS".into(), "acpi:PNP0C09:".into());
        d.properties.insert("ID_MODEL_FROM_DATABASE".into(), "Actual model".into());
        name_device(&mut d);
        assert_eq!((d.name.as_str(), d.name_translated), ("Actual model", false));
    }

    #[test]
    fn pnp_modalias_parsing() {
        let mut d = dev("pnp");
        d.properties.insert("MODALIAS".into(), "pnp:dPNP0303".into());
        name_device(&mut d);
        assert_eq!(d.name, "PS/2 keyboard interface");
    }

    #[test]
    fn codec_ignores_failed_vendor_read() {
        let mut d = dev("hdaudio");
        d.attributes.insert("chip_name".into(), attr(ReadState::Available, "ALC-test"));
        d.attributes.insert("vendor_name".into(), attr(ReadState::PermissionDenied, "Stale vendor"));
        name_device(&mut d);
        assert_eq!(d.name, "ALC-test");
        d.attributes.insert("vendor_name".into(), attr(ReadState::Available, "Realtek"));
        name_device(&mut d);
        assert_eq!(d.name, "Realtek ALC-test");
    }

    #[test]
    fn usb_product_beats_generic_model_and_root_hubs_are_verified() {
        let mut d = Device { devtype: "usb_device".into(), ..dev("usb") };
        d.properties.insert("ID_MODEL".into(), "Generic_Model".into());
        d.attributes.insert("product".into(), attr(ReadState::Available, "Readable USB product"));
        name_device(&mut d);
        assert_eq!(d.name, "Readable USB product");

        let mut hub = Device { devtype: "usb_device".into(), sysname: "usb2".into(), ..dev("usb") };
        hub.properties.insert("DEVNUM".into(), "001".into());
        hub.properties.insert("PRODUCT".into(), "1d6b/3/702".into());
        hub.properties.insert("ID_MODEL_FROM_DATABASE".into(), "3.0 root hub".into());
        name_device(&mut hub);
        assert_eq!((hub.name.as_str(), hub.name_translated), ("USB 3.x root hub", true));
        hub.sysname = "2-1".into();
        name_device(&mut hub);
        assert_eq!((hub.name.as_str(), hub.name_translated), ("3.0 root hub", false));
    }

    #[test]
    fn services_faux_and_platform_aliases_need_exact_matches() {
        let mut service = Device { driver: "pciehp".into(), ..dev("pci_express") };
        name_device(&mut service);
        assert_eq!(service.name, "PCI Express hot-plug service");
        service.subsystem = "unknown_bus".into();
        name_device(&mut service);
        assert!(!service.name_translated, "driver names outside their subsystem imply nothing");

        let mut faux = Device { driver: "faux_driver".into(), sysname: "reg-dummy".into(), ..dev("faux") };
        name_device(&mut faux);
        assert_eq!(faux.name, "Dummy voltage regulator");
        faux.sysname = "reg-dummy-other".into();
        name_device(&mut faux);
        assert!(!faux.name_translated);

        let mut firmware = Device { sysname: "INTC1025:00".into(), ..dev("platform") };
        firmware.properties.insert("MODALIAS".into(), "acpi:INTC1025:INT_PTL_SINIT:".into());
        name_device(&mut firmware);
        assert_eq!(firmware.name, firmware.sysname, "vendor ACPI objects get no hardcoded labels");
        firmware.properties.insert("MODALIAS".into(), "platform:coretemp".into());
        name_device(&mut firmware);
        assert_eq!(firmware.name, "Intel CPU temperature monitor");
    }

    #[test]
    fn root_hub_caption_needs_the_linux_vendor_id() {
        let mut hub = Device { devtype: "usb_device".into(), sysname: "usb2".into(), ..dev("usb") };
        hub.properties.insert("DEVNUM".into(), "001".into());
        hub.properties.insert("PRODUCT".into(), "1234/3/702".into());
        name_device(&mut hub);
        assert!(!hub.name_translated);
    }

    #[test]
    fn hwdb_source_is_reported() {
        let mut d = dev("pci");
        d.properties.insert("ID_MODEL_FROM_DATABASE".into(), "Wi-Fi 7 BE211 320MHz".into());
        d.property_sources.insert("ID_MODEL_FROM_DATABASE".into(), "hwdb: ID_MODEL_FROM_DATABASE (pci:x)".into());
        name_device(&mut d);
        assert_eq!(d.name_source, "hwdb: ID_MODEL_FROM_DATABASE (pci:x)");
    }
}
