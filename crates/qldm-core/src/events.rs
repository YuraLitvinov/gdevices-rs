//! Bounded device-event history and session-local live identities, ported from
//! `src/devices/deviceevents.{h,cpp}`. A record is an owned udev observation, not evidence of
//! hardware health. Tokens are independent of paths and serial numbers; monitor gaps retire
//! every association conservatively.
use crate::model::Device;
use std::collections::{HashMap, HashSet};
use std::time::SystemTime;

const FIELD_LIMIT: usize = 4096;

/// Wall time is the receipt time; monotonic elapsed time and sequence keep ordering.
#[derive(Clone, Debug, Default)]
pub struct DeviceEvent {
    pub observed: Option<SystemTime>,
    pub elapsed_ms: i64,
    pub sequence: u64,
    pub instance: u64,
    pub path: String,
    pub action: String,
    pub subsystem: String,
    pub devtype: String,
    pub driver: String,
    pub initialized: String,
    pub incarnation: String,
    pub old_path: String,
}

#[derive(Clone, Debug)]
pub struct DeviceEventsSnapshot {
    pub started: SystemTime,
    pub records: Vec<DeviceEvent>,
    pub evicted: u64,
    pub unassociated: u64,
    pub gaps: u64,
    pub monitoring: bool,
}

impl DeviceEventsSnapshot {
    pub const LIMIT: usize = 1024;
    pub const PER_DEVICE_LIMIT: usize = 256;
}

impl Default for DeviceEventsSnapshot {
    fn default() -> Self {
        Self { started: SystemTime::now(), records: Vec::new(), evicted: 0, unassociated: 0, gaps: 0, monitoring: false }
    }
}

#[derive(Clone, Debug, Default)]
struct Identity {
    token: u64,
    incarnation: String,
    initialized: String,
    retired: bool,
    /// Last observed sequence for this path; holes are normal.
    sequence: u64,
}

/// Worker-thread ownership only.
pub struct DeviceEventHistory {
    identities: HashMap<String, Identity>,
    snapshot: DeviceEventsSnapshot,
    next_token: u64,
}

fn bounded(value: &mut String) {
    if value.len() > FIELD_LIMIT {
        let cut = (0..=FIELD_LIMIT).rev().find(|i| value.is_char_boundary(*i)).unwrap_or(0);
        value.truncate(cut);
    }
}

impl DeviceEventHistory {
    pub const IDENTITY_LIMIT: usize = 16384;

    pub fn new(started: SystemTime) -> Self {
        Self { identities: HashMap::new(), snapshot: DeviceEventsSnapshot { started, ..Default::default() }, next_token: 0 }
    }

    /// Whether `path` has a registry slot, creating one while there is room.
    fn ensure_identity(&mut self, path: &str) -> bool {
        if self.identities.contains_key(path) {
            return true;
        }
        if self.identities.len() >= Self::IDENTITY_LIMIT {
            return false;
        }
        self.identities.insert(path.to_string(), Identity::default());
        true
    }

    fn retire(&mut self, path: &str) {
        let prefix = format!("{path}/");
        for (key, identity) in &mut self.identities {
            if key == path || key.starts_with(&prefix) {
                identity.retired = true;
            }
        }
    }

    pub fn observe(&mut self, mut event: DeviceEvent) {
        if !event.path.starts_with("/sys/devices/") || event.path.len() > FIELD_LIMIT {
            return;
        }
        // Bound all caller-owned strings too, including fixture/future collectors.
        for value in [
            &mut event.path,
            &mut event.action,
            &mut event.subsystem,
            &mut event.devtype,
            &mut event.driver,
            &mut event.initialized,
            &mut event.incarnation,
            &mut event.old_path,
        ] {
            bounded(value);
        }
        event.instance = 0; // Only this tracker may assign an association.
        let mut stale = false;
        let mut retire_self = false;
        if self.ensure_identity(&event.path) {
            let next = &mut self.next_token;
            let current = self.identities.get_mut(&event.path).expect("ensured");
            // Reordered/duplicate payloads cannot change identity state.
            stale = event.sequence != 0 && current.sequence != 0 && event.sequence <= current.sequence;
            if !stale {
                let stamp_changed = !current.initialized.is_empty() && !event.initialized.is_empty() && current.initialized != event.initialized;
                let inode_changed = !current.incarnation.is_empty() && !event.incarnation.is_empty() && current.incarnation != event.incarnation;
                let fresh = |next: &mut u64, event: &DeviceEvent| {
                    *next += 1;
                    Identity {
                        token: *next,
                        incarnation: event.incarnation.clone(),
                        initialized: event.initialized.clone(),
                        retired: false,
                        sequence: event.sequence,
                    }
                };
                if event.action == "add" {
                    if current.token == 0 || current.retired || stamp_changed || inode_changed {
                        *current = fresh(next, &event);
                    }
                    event.instance = current.token;
                } else if current.token != 0 && !stamp_changed && !inode_changed && (!current.retired || event.action == "remove") {
                    // Removal may have no readable inode, but can use a known stream identity.
                    // A late event with conflicting evidence stays unassociated.
                    event.instance = current.token;
                } else if current.token == 0 && event.action != "remove" && !event.incarnation.is_empty() {
                    *current = fresh(next, &event);
                    event.instance = current.token;
                }
                if event.sequence > current.sequence {
                    current.sequence = event.sequence;
                }
                retire_self = event.action == "remove" && !stamp_changed && !inode_changed;
            }
        }
        if retire_self {
            self.retire(&event.path);
        }
        if !stale && !event.old_path.is_empty() && event.old_path != event.path {
            let old = event.old_path.clone();
            self.retire(&old);
        }
        if event.instance == 0 {
            self.snapshot.unassociated += 1;
        }
        if self.snapshot.records.len() == DeviceEventsSnapshot::LIMIT {
            self.snapshot.records.remove(0);
            self.snapshot.evicted += 1;
        }
        self.snapshot.records.push(event);
    }

    /// A monitor gap: no association made before it can be trusted afterwards.
    pub fn lose(&mut self) {
        self.identities.clear();
        self.snapshot.gaps += 1;
    }

    /// Matches the confirmed inventory to live identities. Missing or withheld records keep no
    /// live association; their already-observed events stay in the ring for open dialogs.
    pub fn reconcile(&mut self, devices: &mut [Device]) {
        let mut present: HashSet<String> = HashSet::new();
        for device in devices.iter_mut() {
            present.insert(device.path.clone());
            device.event_instance = 0;
            if !self.ensure_identity(&device.path) {
                continue;
            }
            let initialized = device.properties.get("USEC_INITIALIZED").cloned().unwrap_or_default();
            let current = self.identities.get_mut(&device.path).expect("ensured");
            let same = current.token != 0
                && !current.retired
                && ((!current.incarnation.is_empty() && current.incarnation == device.incarnation)
                    || (current.incarnation.is_empty() && !current.initialized.is_empty() && current.initialized == initialized));
            if same {
                current.incarnation = device.incarnation.clone();
                current.initialized = initialized;
            } else {
                self.next_token += 1;
                *current = Identity { token: self.next_token, incarnation: device.incarnation.clone(), initialized, retired: false, sequence: 0 };
            }
            device.event_instance = current.token;
        }
        self.identities.retain(|path, _| present.contains(path));
    }

    pub fn snapshot(&self, monitoring: bool) -> DeviceEventsSnapshot {
        DeviceEventsSnapshot { monitoring, ..self.snapshot.clone() }
    }
}

impl Default for DeviceEventHistory {
    fn default() -> Self {
        Self::new(SystemTime::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn fixture(path: &str, stamp: &str) -> Device {
        let mut device = Device { path: path.into(), incarnation: format!("inode:{stamp}"), ..Default::default() };
        device.properties.insert("USEC_INITIALIZED".into(), stamp.into());
        device
    }

    fn observation(device: &Device, action: &str, sequence: u64) -> DeviceEvent {
        DeviceEvent {
            path: device.path.clone(),
            incarnation: if action == "remove" { String::new() } else { device.incarnation.clone() },
            initialized: device.properties.get("USEC_INITIALIZED").cloned().unwrap_or_default(),
            action: action.into(),
            sequence,
            observed: Some(SystemTime::UNIX_EPOCH + Duration::from_millis(100_000 + sequence)),
            elapsed_ms: sequence as i64,
            ..Default::default()
        }
    }

    #[test]
    fn identities_follow_observed_adds_removes_and_losses() {
        let mut history = DeviceEventHistory::new(SystemTime::UNIX_EPOCH + Duration::from_secs(100));
        let parent = fixture("/sys/devices/usb/fixture", "old");
        let child = fixture(&format!("{}/input", parent.path), "old-child");
        let mut devices = vec![parent.clone(), child.clone()];
        history.reconcile(&mut devices);
        let (old, old_child) = (devices[0].event_instance, devices[1].event_instance);
        assert!(old != 0 && old_child != 0 && old != old_child, "distinct devices, distinct identities");

        history.observe(observation(&parent, "change", 1));
        history.observe(observation(&parent, "remove", 2));
        history.observe(observation(&child, "remove", 3));
        let events = history.snapshot(true);
        assert_eq!(
            (events.records[0].instance, events.records[1].instance, events.records[2].instance),
            (old, old, old_child),
            "removal retains the observed parent/child instances"
        );

        // Even identical inode/initialization hints cannot undo observed removal.
        history.observe(observation(&parent, "add", 4));
        history.observe(observation(&child, "add", 5));
        history.reconcile(&mut devices);
        assert!(devices[0].event_instance != old && devices[1].event_instance != old_child, "reused path, new identities");
        assert_eq!(history.snapshot(true).records[3].instance, devices[0].event_instance, "add matches confirmed inventory");

        history.observe(observation(&parent, "remove", 2));
        assert_eq!(history.snapshot(true).records.last().unwrap().instance, 0, "stale removal cannot retire a replacement");
        let replacement = devices[0].event_instance;
        history.reconcile(&mut devices);
        assert_eq!(devices[0].event_instance, replacement);

        let stale = fixture(&parent.path, "stale-other-instance");
        history.observe(observation(&stale, "change", 6));
        assert_eq!(history.snapshot(true).records.last().unwrap().instance, 0, "conflicting evidence stays unassociated");

        history.lose();
        history.reconcile(&mut devices);
        let after = history.snapshot(false);
        assert!(devices[0].event_instance != replacement && after.gaps == 1 && !after.monitoring);
        let stable = devices[0].event_instance;
        history.reconcile(&mut devices);
        assert_eq!(devices[0].event_instance, stable, "ordinary snapshots neither rotate identity nor manufacture events");

        let before = history.snapshot(true).records.len();
        let mut invalid = observation(&parent, "change", 7);
        invalid.path = "/sys/class/usb".into();
        history.observe(invalid);
        assert_eq!(history.snapshot(true).records.len(), before, "noncanonical paths excluded");
    }

    #[test]
    fn history_is_bounded_and_payloads_are_truncated() {
        let parent = fixture("/sys/devices/usb/fixture", "old");
        let mut bounded = DeviceEventHistory::default();
        bounded.reconcile(&mut [parent.clone()]);
        for i in 0..DeviceEventsSnapshot::LIMIT + 3 {
            bounded.observe(observation(&parent, "change", i as u64 + 1));
        }
        let events = bounded.snapshot(true);
        assert_eq!(events.records.len(), DeviceEventsSnapshot::LIMIT);
        assert_eq!((events.evicted, events.records[0].sequence), (3, 4));

        let mut long = observation(&parent, "change", 2000);
        long.driver = "x".repeat(10000);
        bounded.observe(long);
        assert_eq!(bounded.snapshot(true).records.last().unwrap().driver.len(), 4096);
    }

    #[test]
    fn stale_add_payload_is_not_authoritative_and_registry_is_capped() {
        let parent = fixture("/sys/devices/usb/fixture", "old");
        let mut pending = DeviceEventHistory::default();
        pending.observe(observation(&parent, "add", 1));
        let mut unrelated = vec![fixture(&parent.path, "different")];
        pending.reconcile(&mut unrelated);
        assert_ne!(unrelated[0].event_instance, pending.snapshot(true).records[0].instance);

        let mut capped = DeviceEventHistory::default();
        let mut many: Vec<Device> = (0..=DeviceEventHistory::IDENTITY_LIMIT).map(|i| fixture(&format!("/sys/devices/fixture/{i}"), "stamp")).collect();
        capped.reconcile(&mut many);
        assert!(many[DeviceEventHistory::IDENTITY_LIMIT - 1].event_instance != 0);
        assert_eq!(many.last().unwrap().event_instance, 0, "overflow leaves the device viewable but unassociated");
    }

    #[test]
    fn rename_retires_the_old_path() {
        let mut history = DeviceEventHistory::default();
        let device = fixture("/sys/devices/a/old", "1");
        let mut devices = vec![device.clone()];
        history.reconcile(&mut devices);
        let mut moved = observation(&fixture("/sys/devices/a/new", "2"), "move", 9);
        moved.old_path = device.path.clone();
        history.observe(moved);
        history.observe(observation(&device, "change", 10));
        assert_eq!(history.snapshot(true).records.last().unwrap().instance, 0, "retired old path takes no late events");
    }
}
