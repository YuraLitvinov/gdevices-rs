//! Display captions, ported from `src/devices/devicelabel.cpp`. Reads owned metadata
//! only: no filesystem access. Static English captions pass through [`tr`].
use crate::efivars::efi_variable_label;
use crate::i18n::{arg, tr};
use crate::model::{input_device_name, is_audio_jack_switch, Device, ReadState};

fn prop<'a>(d: &'a Device, key: &str) -> &'a str {
    d.properties.get(key).map_or("", String::as_str)
}

/// A successful attribute read wins; a failed one yields nothing rather than the udev property.
fn supply_field(d: &Device, attribute: &str, property: &str) -> String {
    match d.attributes.get(attribute) {
        Some(a) => {
            if a.state == ReadState::Available {
                a.value.trim().to_string()
            } else {
                String::new()
            }
        }
        None => prop(d, property).trim().to_string(),
    }
}

/// `^([A-Z0-9]{4,8}:[0-9]{2}) ([0-9A-Fa-f]{4}):([0-9A-Fa-f]{4}) (Mouse|Touchpad)$`
fn generated_hid_name(name: &str) -> Option<(&str, u32, u32, &str)> {
    let mut parts = name.split(' ');
    let (id, ids, kind) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let (head, tail) = id.split_once(':')?;
    let head_ok = (4..=8).contains(&head.len()) && head.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit());
    if !head_ok || tail.len() != 2 || !tail.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (vendor, product) = ids.split_once(':')?;
    let hex4 = |s: &str| (s.len() == 4 && s.bytes().all(|b| b.is_ascii_hexdigit())).then(|| u32::from_str_radix(s, 16).ok()).flatten();
    if !matches!(kind, "Mouse" | "Touchpad") {
        return None;
    }
    Some((id, hex4(vendor)?, hex4(product)?, kind))
}

fn friendly_label(d: &Device) -> String {
    if d.subsystem == "power_supply" {
        match supply_field(d, "type", "POWER_SUPPLY_TYPE").as_str() {
            "Mains" => return tr("AC power adapter"),
            "Battery" => {
                let vendor = supply_field(d, "manufacturer", "POWER_SUPPLY_MANUFACTURER");
                let mut model = supply_field(d, "model_name", "POWER_SUPPLY_MODEL_NAME");
                // Firmware role placeholders add no useful model identity.
                if ["primary", "secondary", "battery"].iter().any(|p| model.eq_ignore_ascii_case(p)) {
                    model.clear();
                }
                let mut label = if vendor.is_empty() { tr("Battery") } else { arg(tr("%1 battery"), &vendor) };
                if !model.is_empty() {
                    label += &format!(" ({model})");
                }
                return label;
            }
            "USB" => {
                // UCSI names append the connector number to the controller's name.
                // Match the recorded direct parent, not an ambiguous trailing digit.
                let controller = d.parent_path.rsplit('/').next().unwrap_or("");
                let prefix = format!("ucsi-source-psy-{controller}");
                let port = d.sysname.strip_prefix(&prefix).and_then(|s| s.parse::<u32>().ok()).filter(|p| *p > 0);
                return match port {
                    Some(port) if !controller.is_empty() => arg(tr("USB-C power supply (port %1)"), &port.to_string()),
                    _ => tr("USB power supply"),
                };
            }
            _ => {}
        }
    }
    if is_audio_jack_switch(d) {
        let name = input_device_name(d);
        if name.ends_with(" Headphone") || name == "Headphone" {
            return tr("Headphone jack detection");
        }
        if name.ends_with(" Mic") || name == "Mic" {
            return tr("Microphone jack detection");
        }
        let pcm = name.rsplit_once("HDMI/DP,pcm=").map_or("", |(_, n)| n);
        return arg(tr("HDMI/DisplayPort jack detection (PCM %1)"), pcm);
    }
    if d.subsystem == "input" {
        // Shorten only the kernel-generated I2C HID identity form. Product names
        // from USB/Bluetooth and descriptive input names remain untouched.
        let product: Vec<&str> = prop(d, "PRODUCT").split('/').collect();
        let bus = if product.len() == 4 { u32::from_str_radix(product[0], 16).ok() } else { None };
        if let (Some((id, vendor, product_id, kind)), Some(0x18)) = (generated_hid_name(input_device_name(d)), bus) {
            let matches = u32::from_str_radix(product[1], 16).ok() == Some(vendor) && u32::from_str_radix(product[2], 16).ok() == Some(product_id);
            if !matches {
                return String::new();
            }
            if kind == "Touchpad" && prop(d, "ID_INPUT_TOUCHPAD") == "1" {
                return arg(tr("Touchpad (%1)"), id);
            }
            if kind == "Mouse" && prop(d, "ID_INPUT_MOUSE") == "1" && prop(d, "ID_INPUT_POINTINGSTICK") != "1" {
                return arg(tr("Mouse (%1)"), id);
            }
        }
    }
    String::new()
}

/// Caption for the tree and dialogs. Equal names are disambiguated by the kernel
/// identifier; `include_identifiers` also adds it to friendly captions.
pub fn device_display_name(d: &Device, include_identifiers: bool) -> String {
    if d.subsystem == "efivarfs" {
        let label = efi_variable_label(&d.name);
        return if include_identifiers { format!("{label} [{}]", d.properties.get("EFI_VENDOR_GUID").map_or("", String::as_str)) } else { label };
    }
    let label = friendly_label(d);
    if !label.is_empty() {
        return if include_identifiers { format!("{label} [{}]", d.sysname) } else { label };
    }
    if d.name == d.sysname || d.name_source == "direct kernel driver and kernel name" {
        return d.name.clone();
    }
    let mut label = if d.name_translated { tr(&d.name) } else { d.name.clone() };
    if d.name_candidate {
        label = arg(tr("%1 (module candidate)"), &label);
    }
    format!("{label} [{}]", d.sysname)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Attribute;

    fn attr(value: &str) -> Attribute {
        Attribute { state: ReadState::Available, value: value.into(), error: 0 }
    }
    fn named(name: &str, sysname: &str) -> Device {
        Device { name: name.into(), sysname: sysname.into(), name_source: "udev: ID_MODEL".into(), ..Default::default() }
    }

    #[test]
    fn plain_names_get_kernel_id_suffix_but_kernel_names_do_not() {
        assert_eq!(device_display_name(&named("Wi-Fi 7", "wlo1"), false), "Wi-Fi 7 [wlo1]");
        assert_eq!(device_display_name(&named("wlo1", "wlo1"), false), "wlo1");
        let mut candidate = named("Some driver", "dev0");
        candidate.name_candidate = true;
        assert_eq!(device_display_name(&candidate, false), "Some driver (module candidate) [dev0]");
    }

    #[test]
    fn battery_and_mains_labels() {
        let mut bat = Device { subsystem: "power_supply".into(), sysname: "BAT0".into(), ..Default::default() };
        bat.attributes.insert("type".into(), attr("Battery"));
        bat.attributes.insert("manufacturer".into(), attr("HP"));
        bat.attributes.insert("model_name".into(), attr("Primary"));
        assert_eq!(device_display_name(&bat, false), "HP battery");
        bat.attributes.insert("model_name".into(), attr("L1234"));
        assert_eq!(device_display_name(&bat, false), "HP battery (L1234)");
        let mut ac = Device { subsystem: "power_supply".into(), ..Default::default() };
        ac.attributes.insert("type".into(), attr("Mains"));
        assert_eq!(device_display_name(&ac, false), "AC power adapter");
    }

    #[test]
    fn usbc_port_uses_direct_parent() {
        let mut d = Device {
            subsystem: "power_supply".into(),
            sysname: "ucsi-source-psy-USBC000:0012".into(),
            parent_path: "/sys/devices/x/USBC000:001".into(),
            ..Default::default()
        };
        d.attributes.insert("type".into(), attr("USB"));
        assert_eq!(device_display_name(&d, false), "USB-C power supply (port 2)");
        d.parent_path = "/sys/devices/x/OTHER".into();
        assert_eq!(device_display_name(&d, false), "USB power supply");
    }

    #[test]
    fn touchpad_generated_identity_is_shortened_only_when_ids_match() {
        let mut d = Device { subsystem: "input".into(), sysname: "input14".into(), ..Default::default() };
        d.properties.insert("NAME".into(), "\"SYNA3517:00 06CB:CFC6 Touchpad\"".into());
        d.properties.insert("PRODUCT".into(), "18/6cb/cfc6/100".into());
        d.properties.insert("ID_INPUT_TOUCHPAD".into(), "1".into());
        assert_eq!(device_display_name(&d, false), "Touchpad (SYNA3517:00)");
        d.properties.insert("PRODUCT".into(), "18/6cb/1111/100".into());
        assert_ne!(device_display_name(&d, false), "Touchpad (SYNA3517:00)");
    }

    #[test]
    fn efi_variables_use_readable_labels() {
        let mut d = Device { subsystem: "efivarfs".into(), name: "SecureBoot".into(), ..Default::default() };
        d.properties.insert("EFI_VENDOR_GUID".into(), "8be4df61".into());
        assert_eq!(device_display_name(&d, false), "Secure Boot");
        assert_eq!(device_display_name(&d, true), "Secure Boot [8be4df61]");
    }

    #[test]
    fn stale_or_failed_battery_metadata_is_not_reused() {
        let mut bat = Device { subsystem: "power_supply".into(), sysname: "BAT0".into(), name: "Primary".into(), ..Default::default() };
        bat.properties.insert("POWER_SUPPLY_TYPE".into(), "Battery".into());
        bat.properties.insert("POWER_SUPPLY_MANUFACTURER".into(), "ExampleVendor".into());
        bat.properties.insert("POWER_SUPPLY_MODEL_NAME".into(), "Primary".into());
        assert_eq!(device_display_name(&bat, false), "ExampleVendor battery");
        assert_eq!(bat.name, "Primary", "raw name preserved");
        bat.properties.remove("POWER_SUPPLY_MANUFACTURER");
        assert_eq!(device_display_name(&bat, false), "Battery");
        bat.properties.insert("POWER_SUPPLY_MODEL_NAME".into(), "Real model".into());
        assert_eq!(device_display_name(&bat, false), "Battery (Real model)");
        bat.attributes.insert("manufacturer".into(), Attribute { state: ReadState::PermissionDenied, value: "Stale vendor".into(), error: 13 });
        bat.properties.insert("POWER_SUPPLY_MANUFACTURER".into(), "Stale vendor".into());
        assert!(!device_display_name(&bat, false).contains("Stale"));
    }

    #[test]
    fn jack_labels() {
        let mut d = Device { subsystem: "input".into(), sysname: "input12".into(), ..Default::default() };
        d.properties.insert("ID_INPUT_SWITCH".into(), "1".into());
        d.properties.insert("PHYS".into(), "\"ALSA\"".into());
        d.properties.insert("NAME".into(), "\"HDA Intel PCH Mic\"".into());
        assert_eq!(device_display_name(&d, false), "Microphone jack detection");
        d.properties.insert("NAME".into(), "\"HDA Intel PCH HDMI/DP,pcm=7\"".into());
        assert_eq!(device_display_name(&d, false), "HDMI/DisplayPort jack detection (PCM 7)");
    }
}
