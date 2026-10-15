//! Device model and classification, ported from `src/devices/device.{h,cpp}`
//! and `isAudioJackSwitch` in `devicelabel.cpp`.
use crate::events::DeviceEventsSnapshot;
use crate::resources::DeviceResources;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ReadState {
    Available,
    #[default]
    Unavailable,
    PermissionDenied,
    Removed,
    Error,
    Unsupported,
    NotApplicable,
}

/// One sysfs attribute read: unknown or failed reads must never look like values.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Attribute {
    pub state: ReadState,
    pub value: String,
    pub error: i32,
}

#[derive(Clone, Debug, Default)]
pub struct Device {
    /// Canonical sysfs path; lookup key, not a hardware ID.
    pub path: String,
    pub parent_path: String,
    pub subsystem: String,
    pub sysname: String,
    pub devtype: String,
    /// Direct binding only; never inherited.
    pub driver: String,
    /// Observed `driver/module` link target name; absence is undetermined.
    pub driver_module: String,
    pub properties: HashMap<String, String>,
    pub attributes: HashMap<String, Attribute>,
    /// `st_dev:st_ino:USEC_INITIALIZED`; separates a reused path from the same instance.
    pub incarnation: String,
    /// Best available name and where it came from; raw kernel names are never rewritten.
    pub name: String,
    pub name_source: String,
    /// `name` is a static English caption that the UI translates.
    pub name_translated: bool,
    /// `name` came from an installed-module alias match, not a binding claim.
    pub name_candidate: bool,
    /// Source text for properties not read straight from udev (e.g. hwdb lookups).
    pub property_sources: HashMap<String, String>,
    /// Immutable resource snapshot, collected by the worker for pci and pnp devices.
    pub resources: Option<Arc<DeviceResources>>,
    /// Presentation grouping only: the row that stands in for this one, never an ownership edge.
    pub represented_by_path: String,
    /// Session-local live-event association token; zero is unassociated.
    pub event_instance: u64,
    /// Assigned by inventory reconciliation; changes when a path is reused by another instance.
    pub generation: u64,
    /// Untranslated category id from `CATEGORIES`.
    pub category: &'static str,
    pub hidden: bool,
}

/// One published scan: devices plus how trustworthy and complete it is.
#[derive(Clone, Debug, Default)]
pub struct Inventory {
    pub devices: Vec<Device>,
    pub error: String,
    pub skipped: usize,
    pub request: u64,
    /// Paths with removal evidence, including descendants of a removed ancestor.
    pub removed_paths: HashSet<String>,
    /// Event loss: no old generation can be trusted.
    pub identity_lost: bool,
    pub monitor_note: String,
    pub events: DeviceEventsSnapshot,
}

pub struct Category {
    pub id: &'static str,
    pub label: &'static str,
    pub icon: &'static str,
}

pub const CATEGORIES: [Category; 17] = [
    Category { id: "audio", label: "Audio devices", icon: "audio-card" },
    Category { id: "battery", label: "Batteries and power supplies", icon: "battery" },
    Category { id: "bluetooth", label: "Bluetooth", icon: "bluetooth" },
    Category { id: "camera", label: "Cameras and video devices", icon: "camera-web" },
    Category { id: "disk", label: "Disk drives", icon: "drive-harddisk" },
    Category { id: "display", label: "Display adapters", icon: "video-display" },
    Category { id: "hid", label: "Human interface devices", icon: "input-gaming" },
    Category { id: "keyboard", label: "Keyboards", icon: "input-keyboard" },
    Category { id: "mouse", label: "Mice and pointing devices", icon: "input-mouse" },
    Category { id: "monitor", label: "Display outputs", icon: "video-display" },
    Category { id: "network", label: "Network adapters", icon: "network-wired" },
    Category { id: "cpu", label: "Processors", icon: "cpu" },
    Category { id: "storage", label: "Storage controllers", icon: "drive-harddisk" },
    Category { id: "system", label: "System devices", icon: "computer" },
    Category { id: "usb", label: "USB controllers and devices", icon: "drive-removable-media-usb" },
    Category { id: "efi", label: "UEFI variables", icon: "preferences-system" },
    Category { id: "other", label: "Other devices", icon: "preferences-system" },
];

/// Translated caption for a category id; unknown ids read as "Other devices".
pub fn category_label(id: &str) -> String {
    crate::i18n::tr(category(id).label)
}

pub fn category(id: &str) -> &'static Category {
    CATEGORIES.iter().find(|c| c.id == id).unwrap_or(&CATEGORIES[16])
}

/// Assigns generations: an unchanged instance (same incarnation and event token at the same
/// path) keeps its old one, anything else gets a fresh one. Selection survives only on a match.
pub fn reconcile(next: &mut [Device], previous: &[Device], generation: &mut u64) {
    let old: HashMap<&str, &Device> = previous.iter().map(|d| (d.path.as_str(), d)).collect();
    for d in next {
        d.generation = match old.get(d.path.as_str()) {
            Some(before) if before.incarnation == d.incarnation && before.event_instance == d.event_instance => before.generation,
            _ => {
                *generation += 1;
                *generation
            }
        };
    }
}

impl Device {
    fn prop(&self, key: &str) -> &str {
        self.properties.get(key).map_or("", String::as_str)
    }
    fn flag(&self, key: &str) -> bool {
        self.prop(key) == "1"
    }
}

fn unquote(value: &str) -> &str {
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

/// Port of `inputDeviceName`: sysfs `name` attribute first, else the unquoted udev NAME.
pub fn input_device_name(d: &Device) -> &str {
    match d.attributes.get("name") {
        Some(a) if a.state == ReadState::Available && !a.value.is_empty() => &a.value,
        _ => unquote(d.prop("NAME")),
    }
}

/// Matches `(?:^| )(?:HDMI/DP,pcm=[0-9]+|Headphone|Mic)$` without a regex dependency.
fn is_jack_name(name: &str) -> bool {
    let at_boundary = |prefix: &str| prefix.is_empty() || prefix.ends_with(' ');
    for word in ["Headphone", "Mic"] {
        if let Some(prefix) = name.strip_suffix(word) {
            if at_boundary(prefix) {
                return true;
            }
        }
    }
    const PCM: &str = "HDMI/DP,pcm=";
    if let Some(at) = name.rfind(PCM) {
        let digits = &name[at + PCM.len()..];
        return !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) && at_boundary(&name[..at]);
    }
    false
}

pub fn is_audio_jack_switch(d: &Device) -> bool {
    d.subsystem == "input"
        && d.flag("ID_INPUT_SWITCH")
        && unquote(d.prop("PHYS")) == "ALSA"
        && !d.flag("ID_INPUT_KEYBOARD")
        && !d.flag("ID_INPUT_MOUSE")
        && !d.flag("ID_INPUT_TOUCHPAD")
        && !d.flag("ID_INPUT_POINTINGSTICK")
        && is_jack_name(input_device_name(d))
}

/// Port of `classify()` in `src/devices/device.cpp`.
pub fn classify(d: &mut Device) {
    let virt = d.path.starts_with("/sys/devices/virtual/");
    d.category = "other";
    d.hidden = virt;
    let n = d.sysname.clone();
    match d.subsystem.as_str() {
        "pci" => {
            let code = u32::from_str_radix(d.prop("PCI_CLASS"), 16).ok();
            d.category = match code.map(|c| (c >> 16, (c >> 8) & 0xff)) {
                Some((1, _)) => "storage",
                Some((2, _)) => "network",
                Some((3, _)) => "display",
                Some((4, _)) => "audio",
                Some((0x0c, 3)) => "usb",
                _ => "system",
            };
        }
        "usb" => {
            d.category = "usb";
            d.hidden |= d.devtype != "usb_device";
        }
        "block" => {
            d.category = "disk";
            d.hidden |= d.devtype != "disk";
        }
        "input" => {
            if is_audio_jack_switch(d) {
                d.category = "audio";
                d.hidden = true;
            } else if d.flag("ID_INPUT_KEYBOARD") {
                d.category = "keyboard";
            } else if d.flag("ID_INPUT_MOUSE") || d.flag("ID_INPUT_TOUCHPAD") || d.flag("ID_INPUT_POINTINGSTICK") {
                d.category = "mouse";
            } else {
                d.category = "hid";
            }
            // Keep inputN functions; event/js/mouse endpoints are redundant in the type view.
            d.hidden |= !n.starts_with("input");
        }
        "hid" | "hidraw" => {
            d.category = "hid";
            d.hidden = true;
        }
        "sound" => {
            d.category = "audio";
            d.hidden |= !n.starts_with("card");
        }
        "drm" => {
            let connector = n.contains('-');
            d.category = if connector { "monitor" } else { "display" };
            let read = |key: &str, want: &str| d.attributes.get(key).is_some_and(|a| a.state == ReadState::Available && a.value == want);
            // Hide a connector only when both reads establish disconnected + disabled;
            // unknown or failed reads must not masquerade as an unused output.
            let unused = read("status", "disconnected") && read("enabled", "disabled");
            d.hidden |= !connector || unused;
        }
        "net" => d.category = "network",
        "bluetooth" => {
            d.category = "bluetooth";
            d.hidden |= !n.starts_with("hci") || n.contains(':');
        }
        "video4linux" => d.category = "camera",
        "power_supply" => d.category = "battery",
        "cpu" => d.category = "cpu",
        s @ ("nvme" | "scsi_host" | "ata_port") => {
            d.category = "storage";
            d.hidden |= s != "nvme";
        }
        s @ ("platform" | "acpi" | "pnp") => {
            d.category = "system";
            d.hidden |= s == "acpi" && d.driver.is_empty();
        }
        // Unrecognized directly bound physical functions remain useful fallbacks.
        _ => d.hidden |= d.driver.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keyboard() -> Device {
        let mut d =
            Device { path: "/sys/devices/pci0000:00/usb1/1-1/input/input7".into(), subsystem: "input".into(), sysname: "input7".into(), ..Default::default() };
        d.properties.insert("ID_INPUT_KEYBOARD".into(), "1".into());
        d
    }

    #[test]
    fn keyboard_and_audio_functions_stay_visible() {
        let mut k = keyboard();
        classify(&mut k);
        assert_eq!((k.category, k.hidden), ("keyboard", false));
        let mut a = keyboard();
        a.subsystem = "sound".into();
        a.sysname = "card1".into();
        classify(&mut a);
        assert_eq!((a.category, a.hidden), ("audio", false));
    }

    #[test]
    fn input_endpoint_is_hidden() {
        let mut e = keyboard();
        e.sysname = "event3".into();
        classify(&mut e);
        assert!(e.hidden);
    }

    #[test]
    fn virtual_device_keeps_category_but_hides() {
        let mut d = Device { path: "/sys/devices/virtual/net/lo".into(), subsystem: "net".into(), ..Default::default() };
        classify(&mut d);
        assert_eq!((d.category, d.hidden), ("network", true));
    }

    #[test]
    fn partition_is_hidden() {
        let mut d = Device { subsystem: "block".into(), devtype: "partition".into(), ..Default::default() };
        classify(&mut d);
        assert!(d.hidden);
    }

    #[test]
    fn pci_class_decoding() {
        for (class, want) in [("0c0330", "usb"), ("020000", "network"), ("030000", "display"), ("040300", "audio"), ("010802", "storage"), ("060400", "system")]
        {
            let mut d = Device { subsystem: "pci".into(), ..Default::default() };
            d.properties.insert("PCI_CLASS".into(), class.into());
            classify(&mut d);
            assert_eq!(d.category, want, "PCI_CLASS {class}");
        }
        let mut bad = Device { subsystem: "pci".into(), ..Default::default() };
        classify(&mut bad);
        assert_eq!(bad.category, "system");
    }

    #[test]
    fn unknown_bound_function_is_visible_unbound_is_hidden() {
        let mut bound = Device { subsystem: "unknown_bus".into(), driver: "example".into(), ..Default::default() };
        classify(&mut bound);
        assert_eq!((bound.category, bound.hidden), ("other", false));
        let mut unbound = Device { subsystem: "unknown_bus".into(), ..Default::default() };
        classify(&mut unbound);
        assert!(unbound.hidden);
    }

    #[test]
    fn audio_jack_switch_is_hidden_audio() {
        let mut d = Device { subsystem: "input".into(), sysname: "input12".into(), ..Default::default() };
        d.properties.insert("ID_INPUT_SWITCH".into(), "1".into());
        d.properties.insert("PHYS".into(), "\"ALSA\"".into());
        d.properties.insert("NAME".into(), "\"HDA Intel PCH Headphone\"".into());
        classify(&mut d);
        assert_eq!((d.category, d.hidden), ("audio", true));
        d.properties.insert("NAME".into(), "\"HDA Intel PCH HDMI/DP,pcm=3\"".into());
        assert!(is_audio_jack_switch(&d));
        d.properties.insert("NAME".into(), "\"Headphones\"".into());
        assert!(!is_audio_jack_switch(&d));
    }

    #[test]
    fn drm_connector_hidden_only_when_disconnected_and_disabled() {
        let attr = |v: &str| Attribute { state: ReadState::Available, value: v.into(), error: 0 };
        let mut d = Device { subsystem: "drm".into(), sysname: "card0-HDMI-A-1".into(), ..Default::default() };
        d.attributes.insert("status".into(), attr("disconnected"));
        classify(&mut d);
        assert!(!d.hidden, "enabled read missing must not hide");
        d.attributes.insert("enabled".into(), attr("disabled"));
        classify(&mut d);
        assert_eq!((d.category, d.hidden), ("monitor", true));
    }

    #[test]
    fn reconcile_keeps_generation_only_for_the_same_instance() {
        let mut keyboard = keyboard();
        keyboard.incarnation = "first".into();
        let mut counter = 0;
        let mut first = vec![keyboard.clone()];
        reconcile(&mut first, &[], &mut counter);
        let mut same = vec![keyboard.clone()];
        reconcile(&mut same, &first, &mut counter);
        assert_eq!(same[0].generation, first[0].generation, "same instance keeps selection identity");

        let mut reused = vec![Device { incarnation: "second".into(), ..keyboard.clone() }];
        reconcile(&mut reused, &first, &mut counter);
        assert_ne!(reused[0].generation, first[0].generation, "reused path must not keep the old generation");

        let mut other = keyboard.clone();
        other.path.push('x');
        let mut pair = vec![keyboard.clone(), other];
        reconcile(&mut pair, &[], &mut counter);
        assert_ne!(pair[0].generation, pair[1].generation, "equal metadata must not merge devices");

        let mut before = vec![Device { event_instance: 1, ..keyboard.clone() }];
        reconcile(&mut before, &[], &mut counter);
        let mut after = vec![Device { event_instance: 2, ..keyboard }];
        reconcile(&mut after, &before, &mut counter);
        assert_ne!(after[0].generation, before[0].generation, "retired live-event identity gets a new generation");
    }

    #[test]
    fn category_table_matches_cpp_order() {
        let ids: Vec<_> = CATEGORIES.iter().map(|c| c.id).collect();
        assert_eq!(ids.first(), Some(&"audio"));
        assert_eq!(ids.last(), Some(&"other"));
        assert_eq!(category("nope").id, "other");
    }
}
