//! Default type-view grouping, ported from `src/devices/devicepresentation.cpp`.
//!
//! Every record stays in the inventory with its real parent and driver binding; grouping only
//! sets `hidden` and `represented_by_path`. Missing parents or unrelated classes never
//! justify suppression.
use crate::model::{is_audio_jack_switch, Device, ReadState};
use std::collections::{HashMap, HashSet};

fn pci_class(d: &Device, base: u32, subclass: Option<u32>) -> bool {
    if d.subsystem != "pci" {
        return false;
    }
    let Some(code) = d.properties.get("PCI_CLASS").and_then(|v| u32::from_str_radix(v, 16).ok()) else {
        return false;
    };
    code >> 16 == base && subclass.is_none_or(|s| (code >> 8) & 0xff == s)
}

/// Follows recorded udev parents to the nearest requested subsystem. PCI callers stop at the
/// first function, so an outer controller cannot represent its child.
fn ancestor(d: &Device, devices: &[Device], lookup: &HashMap<&str, usize>, subsystem: &str, devtype: Option<&str>) -> Option<usize> {
    let mut path = d.parent_path.as_str();
    let mut visited = HashSet::new();
    while !path.is_empty() && visited.insert(path) {
        let index = *lookup.get(path)?;
        let parent = &devices[index];
        if parent.subsystem == subsystem && devtype.is_none_or(|t| parent.devtype == t) {
            return Some(index);
        }
        path = parent.parent_path.as_str();
    }
    None
}

/// Names a USB network interface from its owning USB device's database identity, falling back
/// to readable descriptors. Never guesses what underscores in ID_MODEL meant or borrows a
/// hub's or neighbour's name.
fn usb_network_name(network: &mut Device, usb: &Device) {
    let source_of = |key: &str| usb.property_sources.get(key).cloned().unwrap_or_else(|| format!("udev: {key}"));
    let property = |key: &str| usb.properties.get(key).map_or("", |v| v.trim()).to_string();
    let available = |key: &str| usb.attributes.get(key).filter(|a| a.state == ReadState::Available).map(|a| a.value.trim().to_string());
    let mut model = property("ID_MODEL_FROM_DATABASE");
    let mut model_source = source_of("ID_MODEL_FROM_DATABASE");
    if model.is_empty() {
        match available("product").filter(|p| !p.is_empty()) {
            Some(product) => {
                model = product;
                model_source = format!("sysfs: {}/product", usb.path);
            }
            None => return,
        }
    }
    let mut vendor = property("ID_VENDOR_FROM_DATABASE");
    let mut vendor_source = source_of("ID_VENDOR_FROM_DATABASE");
    if vendor.is_empty() {
        if let Some(manufacturer) = available("manufacturer") {
            vendor = manufacturer;
        }
        vendor_source = format!("sysfs: {}/manufacturer", usb.path);
    }
    let vendor_len = vendor.chars().count();
    let already_named =
        !vendor.is_empty() && model.to_lowercase().starts_with(&vendor.to_lowercase()) && model.chars().nth(vendor_len).is_none_or(|c| !c.is_alphanumeric());
    network.name = if vendor.is_empty() || already_named { model } else { format!("{vendor} {model}") };
    network.name_source = format!("USB device {}: {model_source}", usb.path);
    if !vendor.is_empty() && !already_named {
        network.name_source += &format!("; {vendor_source}");
    }
    network.name_translated = false;
    network.name_candidate = false;
}

pub fn refine_presentation(devices: &mut [Device]) {
    let paths: Vec<String> = devices.iter().map(|d| d.path.clone()).collect();
    let lookup: HashMap<&str, usize> = paths.iter().enumerate().map(|(i, p)| (p.as_str(), i)).collect();
    let mut functions: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..devices.len() {
        if devices[i].subsystem == "net" {
            // Stop at the nearest USB device even if it has no readable name; never continue
            // outward to a hub or host controller for a label.
            if let Some(usb) = ancestor(&devices[i], devices, &lookup, "usb", Some("usb_device")) {
                if !devices[usb].sysname.starts_with("usb") {
                    let owner = devices[usb].clone();
                    usb_network_name(&mut devices[i], &owner);
                }
            }
        }
        if is_audio_jack_switch(&devices[i]) {
            if let Some(card) = ancestor(&devices[i], devices, &lookup, "sound", None) {
                devices[i].represented_by_path = paths[card].clone();
            }
        }
        let d = &devices[i];
        if d.hidden || !matches!(d.subsystem.as_str(), "net" | "nvme" | "sound") {
            continue;
        }
        let Some(parent) = ancestor(d, devices, &lookup, "pci", None) else { continue };
        if devices[parent].hidden {
            continue;
        }
        let grouped_function = (d.subsystem == "net" && pci_class(&devices[parent], 2, None))
            || (d.subsystem == "sound" && d.sysname.starts_with("card") && pci_class(&devices[parent], 4, None));
        if grouped_function {
            functions.entry(parent).or_default().push(i);
        } else if d.subsystem == "nvme" && pci_class(&devices[parent], 1, Some(8)) {
            // The PCI function represents the controller; namespace disks stay separate.
            devices[i].hidden = true;
            devices[i].represented_by_path = paths[parent].clone();
        }
    }
    for (parent, children) in functions {
        if let [child] = children[..] {
            devices[child].hidden = true;
            devices[child].represented_by_path = paths[parent].clone();
        } else {
            // Multiple cards/ports need their independently useful function rows.
            // Suppress the extra bus row, never collapse distinct cards or interfaces.
            devices[parent].hidden = true;
        }
    }
    // A single ALSA card may itself be grouped under its PCI audio function.
    for i in 0..devices.len() {
        if !is_audio_jack_switch(&devices[i]) {
            continue;
        }
        if let Some(&card) = lookup.get(devices[i].represented_by_path.as_str()) {
            if !devices[card].represented_by_path.is_empty() {
                devices[i].represented_by_path = devices[card].represented_by_path.clone();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{classify, Attribute};

    fn pci(path: &str, class: &str) -> Device {
        let mut d = Device { path: path.into(), subsystem: "pci".into(), ..Default::default() };
        d.properties.insert("PCI_CLASS".into(), class.into());
        d
    }
    fn child(path: String, parent: &str, subsystem: &str, sysname: &str) -> Device {
        Device { path, parent_path: parent.into(), subsystem: subsystem.into(), sysname: sysname.into(), ..Default::default() }
    }
    fn refined(mut devices: Vec<Device>) -> Vec<Device> {
        devices.iter_mut().for_each(classify);
        refine_presentation(&mut devices);
        devices
    }

    #[test]
    fn one_wifi_function_and_one_interface_yield_one_row() {
        let wifi = pci("/sys/devices/pci0000:00/0000:00:14.3", "28000");
        let net = child(format!("{}/net/wlo1", wifi.path), &wifi.path, "net", "wlo1");
        let out = refined(vec![wifi.clone(), net]);
        assert!(!out[0].hidden && out[1].hidden);
        assert_eq!(out[1].represented_by_path, wifi.path);
        assert_eq!((out[1].parent_path.as_str(), out[1].driver.as_str()), (wifi.path.as_str(), ""), "ancestry kept");
    }

    #[test]
    fn independent_network_functions_survive() {
        let wifi = pci("/sys/devices/pci0000:00/0000:00:14.3", "28000");
        let a = child(format!("{}/net/wlo1", wifi.path), &wifi.path, "net", "wlo1");
        let b = child(format!("{}/net/other0", wifi.path), &wifi.path, "net", "other0");
        let out = refined(vec![wifi, a, b]);
        assert!(out[0].hidden && !out[1].hidden && !out[2].hidden);
    }

    #[test]
    fn nvme_controller_pairs_but_disk_and_bridges_do_not() {
        let ctl = pci("/sys/devices/pci0000:00/0000:00:06.0/0000:01:00.0", "10802");
        let nvme = child(format!("{}/nvme/nvme0", ctl.path), &ctl.path, "nvme", "nvme0");
        let mut disk = child(format!("{}/nvme0n1", nvme.path), &ctl.path, "block", "nvme0n1");
        disk.devtype = "disk".into();
        let out = refined(vec![ctl.clone(), nvme.clone(), disk]);
        assert!(!out[0].hidden && out[1].hidden && !out[2].hidden, "controller row stays, disk not hidden");

        let mut second = ctl.clone();
        second.path = "/sys/devices/pci0000:00/0000:02:00.0".into();
        let two = refined(vec![ctl.clone(), nvme.clone(), second]);
        assert!(!two[0].hidden && !two[2].hidden, "a second controller is never merged");

        let bridge = pci(&ctl.path, "60400");
        assert!(!refined(vec![bridge, nvme])[1].hidden, "a PCI bridge is not an NVMe controller duplicate");
    }

    fn audio_fixture() -> (Device, Device, Device) {
        let audio = pci("/sys/devices/pci0000:00/0000:00:1f.3", "40300");
        let dsp = child(format!("{}/skl_hda_dsp_generic", audio.path), &audio.path, "platform", "");
        let card = child(format!("{}/sound/card0", dsp.path), &dsp.path, "sound", "card0");
        (audio, dsp, card)
    }

    #[test]
    fn alsa_card_groups_under_pci_audio_through_dsp_records() {
        let (audio, dsp, card) = audio_fixture();
        let out = refined(vec![audio.clone(), dsp.clone(), card.clone()]);
        assert!(!out[0].hidden && out[2].hidden);
        assert_eq!(out[2].represented_by_path, audio.path);

        let mut card1 = card.clone();
        card1.path = format!("{}/sound/card1", dsp.path);
        card1.sysname = "card1".into();
        let multi = refined(vec![audio.clone(), dsp.clone(), card.clone(), card1]);
        assert!(multi[0].hidden && !multi[2].hidden && !multi[3].hidden, "distinct cards stay visible");

        let usb = pci(&audio.path, "0c0330");
        assert!(!refined(vec![usb, dsp.clone(), card.clone()])[2].hidden, "a USB controller is not an audio function");

        let mut second = audio.clone();
        second.path = "/sys/devices/pci0000:00/0000:01:00.1".into();
        let two = refined(vec![audio, dsp, card, second]);
        assert!(!two[0].hidden && !two[3].hidden, "a separate audio controller is not merged");
    }

    #[test]
    fn alsa_switch_is_associated_with_the_audio_controller() {
        let (audio, dsp, card) = audio_fixture();
        let mut jack = child(format!("{}/input23", card.path), &card.path, "input", "input23");
        jack.properties.insert("NAME".into(), "\"sof-hda-dsp HDMI/DP,pcm=3\"".into());
        jack.properties.insert("PHYS".into(), "\"ALSA\"".into());
        jack.properties.insert("ID_INPUT_SWITCH".into(), "1".into());
        let out = refined(vec![audio.clone(), dsp, card, jack]);
        assert_eq!((out[3].category, out[3].hidden), ("audio", true));
        assert_eq!(out[3].represented_by_path, audio.path);
    }

    #[test]
    fn usb_network_interface_takes_the_owning_usb_devices_name() {
        let mut usb = Device {
            path: "/sys/devices/pci0000:00/usb1/1-2".into(),
            subsystem: "usb".into(),
            devtype: "usb_device".into(),
            sysname: "1-2".into(),
            ..Default::default()
        };
        usb.properties.insert("ID_MODEL_FROM_DATABASE".into(), "AX88179 Gigabit Ethernet".into());
        usb.properties.insert("ID_VENDOR_FROM_DATABASE".into(), "ASIX Electronics Corp.".into());
        let net = child(format!("{}/1-2:1.0/net/enx1", usb.path), &format!("{}/1-2:1.0", usb.path), "net", "enx1");
        let iface = child(format!("{}/1-2:1.0", usb.path), &usb.path, "usb", "1-2:1.0");
        let out = refined(vec![usb.clone(), iface, net]);
        assert_eq!(out[2].name, "ASIX Electronics Corp. AX88179 Gigabit Ethernet");
        assert!(out[2].name_source.starts_with("USB device /sys/devices/pci0000:00/usb1/1-2"));

        // Unreadable descriptors leave the name untouched, never borrowing a hub's.
        let mut bare = usb;
        bare.properties.clear();
        bare.attributes.insert("product".into(), Attribute { state: ReadState::PermissionDenied, value: "x".into(), error: 13 });
        let net2 = Device { name: "kernel".into(), ..child(format!("{}/net/enx2", bare.path), &bare.path, "net", "enx2") };
        assert_eq!(refined(vec![bare, net2])[1].name, "kernel");
    }

    #[test]
    fn unrelated_or_missing_parents_never_suppress() {
        let orphan = child("/sys/devices/x/net/eth9".into(), "/sys/devices/missing", "net", "eth9");
        assert!(!refined(vec![orphan])[0].hidden);
    }
}
