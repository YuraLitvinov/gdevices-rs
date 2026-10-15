//! Coalesced udev hints that decide which branches of a scan are trustworthy, ported from
//! `src/devices/monitorstate.{h,cpp}`. Hints are bounded; overflow degrades to full
//! reconciliation rather than guessing.
use crate::model::Inventory;
use std::collections::HashSet;

fn related(a: &str, b: &str) -> bool {
    a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
}

#[derive(Debug, Default)]
pub struct MonitorState {
    pub changed: HashSet<String>,
    pub removed: HashSet<String>,
    pub lost: bool,
    pub dirty: bool,
}

impl MonitorState {
    pub const LIMIT: usize = 4096;

    pub fn observe(&mut self, path: &str, removal: bool) {
        if !path.starts_with("/sys/devices/") {
            return;
        }
        self.dirty = true;
        if self.changed.len() >= Self::LIMIT || self.removed.len() >= Self::LIMIT {
            self.lose();
            return;
        }
        self.changed.insert(path.to_string());
        if removal {
            self.removed.insert(path.to_string());
        }
    }

    pub fn lose(&mut self) {
        self.dirty = true;
        self.lost = true;
        self.changed.clear();
        self.removed.clear();
    }

    pub fn begin_scan(&mut self) {
        self.changed.clear();
        self.dirty = false;
        // Removal/identity-loss evidence survives all scans until the UI publishes.
    }

    /// Withholds records whose branch changed during collection. Real ancestry and
    /// presentation dependencies count: an ancestor's metadata can supply a child's name,
    /// grouping or driver relationship, and vice versa.
    pub fn exclude_unsettled(&self, inventory: &mut Inventory) {
        if !self.dirty {
            return;
        }
        if self.lost {
            inventory.skipped += inventory.devices.len();
            inventory.devices.clear();
            return;
        }
        let mut excluded = self.changed.clone();
        loop {
            let mut extended = false;
            for d in &inventory.devices {
                if excluded.contains(&d.path) {
                    if !d.represented_by_path.is_empty() && excluded.insert(d.represented_by_path.clone()) {
                        extended = true;
                    }
                    continue;
                }
                let affected =
                    (!d.represented_by_path.is_empty() && excluded.contains(&d.represented_by_path)) || self.changed.iter().any(|path| related(&d.path, path));
                if affected {
                    excluded.insert(d.path.clone());
                    extended = true;
                }
            }
            if !extended {
                break;
            }
        }
        let before = inventory.devices.len();
        inventory.devices.retain(|d| !excluded.contains(&d.path));
        inventory.skipped += before - inventory.devices.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Device;

    fn at(path: &str) -> Device {
        Device { path: path.into(), ..Default::default() }
    }

    #[test]
    fn coalescing_scans_and_exclusion_follow_the_cpp_rules() {
        let mut events = MonitorState::default();
        events.observe("/sys/devices/usb/device", true);
        events.observe("/sys/devices/usb/device", false);
        assert_eq!((events.changed.len(), events.removed.len()), (1, 1), "remove/add keeps replacement evidence");
        events.begin_scan();
        assert!(!events.dirty && events.changed.is_empty() && events.removed.len() == 1, "removals survive until publication");

        events.observe("/sys/devices/usb/device", false);
        let ancestor = at("/sys/devices/usb");
        let target = at("/sys/devices/usb/device");
        let child = Device { parent_path: target.path.clone(), ..at("/sys/devices/usb/device/interface") };
        let peer = Device { parent_path: ancestor.path.clone(), ..at("/sys/devices/usb/other") };
        let independent = at("/sys/devices/pci/card");
        let mut inventory = Inventory { devices: vec![ancestor, target, child, peer.clone(), independent.clone()], ..Default::default() };
        events.exclude_unsettled(&mut inventory);
        let kept: Vec<_> = inventory.devices.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(kept, [peer.path.as_str(), independent.path.as_str()], "ancestors and descendants withheld, siblings kept");
        assert_eq!(inventory.skipped, 3);

        events.lose();
        inventory.devices = vec![independent.clone()];
        events.exclude_unsettled(&mut inventory);
        assert!(inventory.devices.is_empty() && events.lost, "loss during collection withholds the snapshot");

        events.begin_scan();
        inventory.devices = vec![independent];
        events.exclude_unsettled(&mut inventory);
        assert!(inventory.devices.len() == 1 && events.lost, "recovery scan publishes while forcing new generations");
    }

    #[test]
    fn hints_are_bounded_and_canonical() {
        let mut events = MonitorState::default();
        for i in 0..=MonitorState::LIMIT {
            events.observe(&format!("/sys/devices/fixture/{i}"), true);
        }
        assert!(events.lost && events.changed.len() <= MonitorState::LIMIT && events.removed.len() <= MonitorState::LIMIT);
        let mut other = MonitorState::default();
        other.observe("/sys/class/usb", true);
        assert!(!other.dirty);
    }

    #[test]
    fn presentation_dependencies_extend_the_exclusion() {
        let mut events = MonitorState::default();
        events.observe("/sys/devices/pci/wifi", false);
        let wifi = at("/sys/devices/pci/wifi");
        let net = Device { represented_by_path: wifi.path.clone(), ..at("/sys/devices/pci/net/wlan0") };
        let mut inventory = Inventory { devices: vec![wifi, net], ..Default::default() };
        events.exclude_unsettled(&mut inventory);
        assert!(inventory.devices.is_empty(), "a represented row is withheld with its representative");
    }
}
