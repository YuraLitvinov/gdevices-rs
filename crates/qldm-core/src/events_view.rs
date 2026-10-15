//! What the Events tab shows, ported from `PropertiesDialog::updateEvents` and its helpers in
//! `src/ui/propertiesdialog.cpp`: the retained udev events of exactly one device instance, newest
//! first, with an explanation of the scope and its gaps. A removed device keeps the events it had
//! displayed, as a read-only instance snapshot.
use crate::events::{DeviceEvent, DeviceEventsSnapshot};
use crate::i18n::{arg, argn, format_local_iso, format_system_time_iso_ms, tr};
use crate::model::Device;

pub fn event_description(event: &DeviceEvent) -> String {
    match event.action.as_str() {
        "add" => tr("Device addition observed"),
        "remove" => tr("Device removal observed"),
        "change" => tr("Device change reported"),
        "bind" => tr("Driver binding reported"),
        "unbind" => tr("Driver unbinding reported"),
        "move" => tr("Device path move reported"),
        "online" => tr("Device online event reported"),
        "offline" => tr("Device offline event reported"),
        other => arg(tr("udev action: %1"), other),
    }
}

fn observed(event: &DeviceEvent) -> String {
    event.observed.map(format_system_time_iso_ms).unwrap_or_default()
}

/// Full detail text of one event. Each field is formatted once, so a literal `%9` inside a device
/// string can never become a later placeholder.
pub fn event_text(event: &DeviceEvent) -> String {
    let value = |text: &str| if text.is_empty() { tr("Unavailable in event payload") } else { text.to_string() };
    [
        arg(tr("Observed: %1"), &observed(event)),
        arg(tr("Elapsed since monitor startup: %1 ms"), &event.elapsed_ms.to_string()),
        tr("Source: live udev observation"),
        arg(tr("Action: %1"), &value(&event.action)),
        arg(tr("Description: %1"), &event_description(event)),
        arg(tr("Kernel path: %1"), &event.path),
        arg(tr("Subsystem: %1"), &value(&event.subsystem)),
        arg(tr("Device type: %1"), &value(&event.devtype)),
        arg(tr("Driver in event payload: %1"), &value(&event.driver)),
        arg(tr("udev sequence: %1"), &if event.sequence != 0 { event.sequence.to_string() } else { value("") }),
        arg(tr("Initialization stamp: %1"), &value(&event.initialized)),
        arg(tr("Local instance token: %1"), &event.instance.to_string()),
        arg(tr("Previous path: %1"), &value(&event.old_path)),
    ]
    .join("\n")
}

/// One table row of the Events tab: observed time, event type, description.
pub fn event_row(event: &DeviceEvent) -> [String; 3] {
    [observed(event), event.action.clone(), event_description(event)]
}

fn same_event(a: &DeviceEvent, b: &DeviceEvent) -> bool {
    a.sequence == b.sequence && a.elapsed_ms == b.elapsed_ms && a.observed == b.observed && a.action == b.action
}

/// Whether two displayed lists are identical, so unrelated events do not reset selection or
/// scroll position.
pub fn events_unchanged(a: &[DeviceEvent], b: &[DeviceEvent]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| same_event(x, y) && x.path == y.path && x.instance == y.instance)
}

pub struct EventsUpdate {
    /// Explanation of what is covered, shown as the tab notice's tooltip and prefixed to copies.
    pub scope: String,
    pub events: Vec<DeviceEvent>,
}

/// Computes the displayed events for `device`. `displayed` is what the tab currently shows;
/// once the device is `removed`, events it had displayed are kept even if globally evicted.
pub fn update_events(history: &DeviceEventsSnapshot, monitor_note: &str, device: &Device, removed: bool, displayed: &[DeviceEvent]) -> EventsUpdate {
    let started = format_local_iso(history.started.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64));
    let mut scope = argn(
        tr("Live udev events since %1. Receipt times; exact device instance only. Latest %3 events per device; %2 retained globally. Historical logs are not queried."),
        &[
            &started.unwrap_or_else(|| tr("monitor startup")),
            &DeviceEventsSnapshot::LIMIT.to_string(),
            &DeviceEventsSnapshot::PER_DEVICE_LIMIT.to_string(),
        ],
    );
    let mut note = |text: String| {
        scope.push('\n');
        scope.push_str(&text);
    };
    if history.evicted > 0 {
        note(arg(tr("%1 older events evicted from the global history."), &history.evicted.to_string()));
    }
    if history.unassociated > 0 {
        note(arg(tr("%1 events could not be associated safely with an inventory instance."), &history.unassociated.to_string()));
    }
    if history.gaps > 0 {
        note(arg(tr("%1 event-identity resets after monitor failures/losses; coverage is incomplete."), &history.gaps.to_string()));
    }
    if !history.monitoring {
        note(tr("Live event monitoring is unavailable. Viewing and retained events remain available."));
    }
    if !monitor_note.is_empty() {
        note(monitor_note.to_string());
    }
    if device.event_instance == 0 {
        note(tr("No safe live-event association is available for this instance."));
    }
    if device.subsystem == "efivarfs" {
        note(tr("EFI variables are reconciled by inventory scans; libudev does not supply their value-change history."));
    }
    let mut next: Vec<DeviceEvent> = Vec::new();
    for event in history.records.iter().rev() {
        if device.event_instance == 0 || event.instance != device.event_instance || event.path != device.path {
            continue;
        }
        next.push(event.clone());
        if next.len() == DeviceEventsSnapshot::PER_DEVICE_LIMIT {
            break;
        }
    }
    if removed {
        for old in displayed {
            let retained = next.iter().any(|e| same_event(e, old));
            if !retained && next.len() < DeviceEventsSnapshot::PER_DEVICE_LIMIT {
                next.push(old.clone());
            }
        }
        note(tr("Removed/replaced device: retained events are a read-only instance snapshot."));
    }
    if next.is_empty() {
        note(tr("No retained live events for this instance. This does not establish that the device has had no errors."));
    }
    EventsUpdate { scope, events: next }
}

/// Text copied from the Events tab: the scope followed by the chosen events.
pub fn events_copy_text(scope: &str, events: &[&DeviceEvent]) -> String {
    let mut text = vec![scope.to_string()];
    text.extend(events.iter().map(|e| event_text(e)));
    text.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn event(sequence: u64, action: &str, instance: u64, path: &str) -> DeviceEvent {
        DeviceEvent {
            observed: Some(SystemTime::UNIX_EPOCH + Duration::from_millis(1_700_000_000_000 + sequence)),
            elapsed_ms: sequence as i64,
            sequence,
            instance,
            path: path.into(),
            action: action.into(),
            subsystem: "usb".into(),
            ..Default::default()
        }
    }
    fn device(instance: u64) -> Device {
        Device { path: "/sys/devices/usb1".into(), event_instance: instance, ..Default::default() }
    }
    fn history(records: Vec<DeviceEvent>) -> DeviceEventsSnapshot {
        DeviceEventsSnapshot { started: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000), records, monitoring: true, ..Default::default() }
    }

    #[test]
    fn only_the_exact_instance_is_shown_newest_first() {
        let records = vec![
            event(1, "add", 7, "/sys/devices/usb1"),
            event(2, "change", 8, "/sys/devices/usb1"),
            event(3, "change", 7, "/sys/devices/other"),
            event(4, "remove", 7, "/sys/devices/usb1"),
        ];
        let update = update_events(&history(records), "", &device(7), false, &[]);
        assert_eq!(update.events.iter().map(|e| e.sequence).collect::<Vec<_>>(), [4, 1]);
        assert!(update.scope.starts_with("Live udev events since 20"));
        assert!(update.scope.contains("Latest 256 events per device; 1024 retained globally."));
        assert!(!update.scope.contains("No retained"));
        let none = update_events(&history(vec![]), "", &device(7), false, &[]);
        assert!(none.scope.contains("No retained live events for this instance."));
        let unassociated = update_events(&history(vec![event(1, "add", 0, "/sys/devices/usb1")]), "", &device(0), false, &[]);
        assert!(unassociated.events.is_empty() && unassociated.scope.contains("No safe live-event association"));
    }

    #[test]
    fn scope_reports_gaps_and_unavailable_monitoring() {
        let mut h = history(vec![]);
        h.evicted = 3;
        h.unassociated = 2;
        h.gaps = 1;
        h.monitoring = false;
        let scope = update_events(&h, "Changing device branches are being reconciled", &device(1), false, &[]).scope;
        for part in [
            "3 older events evicted from the global history.",
            "2 events could not be associated safely with an inventory instance.",
            "1 event-identity resets after monitor failures/losses; coverage is incomplete.",
            "Live event monitoring is unavailable.",
            "Changing device branches are being reconciled",
        ] {
            assert!(scope.contains(part), "{part}");
        }
        let efi = Device { subsystem: "efivarfs".into(), ..device(1) };
        assert!(update_events(&history(vec![]), "", &efi, false, &[]).scope.contains("libudev does not supply their value-change history"));
    }

    #[test]
    fn a_removed_device_keeps_what_it_displayed() {
        let shown = vec![event(9, "change", 7, "/sys/devices/usb1"), event(1, "add", 7, "/sys/devices/usb1")];
        let h = history(vec![event(9, "change", 7, "/sys/devices/usb1")]);
        let update = update_events(&h, "", &device(7), true, &shown);
        assert_eq!(update.events.iter().map(|e| e.sequence).collect::<Vec<_>>(), [9, 1], "the evicted add event is retained");
        assert!(update.scope.contains("Removed/replaced device"));
        assert!(events_unchanged(&update.events, &update.events.clone()));
        assert!(!events_unchanged(&update.events, &update.events[..1]));
    }

    #[test]
    fn event_text_marks_missing_payload_and_describes_actions() {
        let text = event_text(&event(5, "bind", 3, "/sys/devices/usb1"));
        assert!(text.contains("Action: bind") && text.contains("Description: Driver binding reported"));
        assert!(text.contains("Driver in event payload: Unavailable in event payload"));
        assert!(text.contains("udev sequence: 5") && text.contains("Local instance token: 3"));
        assert_eq!(event_description(&event(1, "weird", 0, "p")), "udev action: weird");
        let literal = DeviceEvent { path: "/sys/devices/%9".into(), action: "add".into(), ..Default::default() };
        assert!(event_text(&literal).contains("Kernel path: /sys/devices/%9"), "device strings are never reinterpreted");
        let row = event_row(&event(5, "add", 3, "p"));
        assert!(row[0].len() == 23 && row[1] == "add" && row[2] == "Device addition observed", "{row:?}");
        let copied = events_copy_text("scope", &[&event(1, "add", 1, "p")]);
        assert!(copied.starts_with("scope\n\nObserved: "));
    }
}
