//! The live inventory session, ported from `Enumerator::run()` in `src/devices/enumerator.cpp`.
//!
//! One worker thread owns the udev monitor, the event history and every scan. At most one
//! snapshot is outstanding: the UI calls [`Session::acknowledge`] after applying it. Refresh
//! requests coalesce, event bursts are debounced, and a lost or overflowed monitor never lets
//! old identities pass as current.
use crate::enumerate::scan_cancellable;
use crate::events::{DeviceEvent, DeviceEventHistory};
use crate::model::Inventory;
use crate::monitorstate::MonitorState;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};
use udev::AsRaw;

const RETRY_MS: i64 = 2000;
const PERIODIC_MS: i64 = 30000;
const QUIET_MS: i64 = 100;
const BURST_MS: i64 = 500;

struct Shared {
    request: AtomicU64,
    acknowledged: AtomicBool,
    interrupt: AtomicBool,
}

/// Handle to the worker. Dropping it requests interruption and waits for the worker.
pub struct Session {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Session {
    /// Starts the worker. `on_inventory` runs on the worker thread for every published
    /// snapshot; hand the value to the UI thread there and call [`Session::acknowledge`]
    /// once it has been applied. Nothing further is published until then.
    pub fn start(on_inventory: impl Fn(Inventory) + Send + 'static) -> Self {
        let shared = Arc::new(Shared { request: AtomicU64::new(0), acknowledged: AtomicBool::new(true), interrupt: AtomicBool::new(false) });
        let worker = Arc::clone(&shared);
        let thread =
            std::thread::Builder::new().name("qldm-enumerator".into()).spawn(move || Worker::new(worker).run(&on_inventory)).expect("spawn enumerator thread");
        Self { shared, thread: Some(thread) }
    }

    /// Requests a manual refresh; coalesces with other pending requests.
    pub fn request_refresh(&self) -> u64 {
        self.shared.request.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn acknowledge(&self) {
        self.shared.acknowledged.store(true, Ordering::SeqCst);
    }

    /// Asks the worker to stop between records without waiting. A kernel metadata read already
    /// in progress cannot be cancelled, so [`Session::join`] can still wait briefly.
    pub fn request_stop(&self) {
        self.shared.interrupt.store(true, Ordering::SeqCst);
    }

    pub fn join(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.request_stop();
        self.join();
    }
}

fn clear_errno() {
    // SAFETY: __errno_location returns this thread's valid errno slot.
    unsafe { *libc::__errno_location() = 0 }
}

struct Worker {
    shared: Arc<Shared>,
    clock: Instant,
    monitor: Option<udev::MonitorSocket>,
    events: MonitorState,
    history: DeviceEventHistory,
    history_gap_pending: bool,
    retry_at: i64,
    first_event: i64,
    last_event: i64,
    pending: bool,
    monitor_note: String,
}

impl Worker {
    fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            clock: Instant::now(),
            monitor: None,
            events: MonitorState::default(),
            history: DeviceEventHistory::default(),
            history_gap_pending: false,
            retry_at: 0,
            first_event: -1,
            last_event: 0,
            pending: true,
            monitor_note: String::new(),
        }
    }

    fn now(&self) -> i64 {
        self.clock.elapsed().as_millis() as i64
    }

    fn interrupted(&self) -> bool {
        self.shared.interrupt.load(Ordering::Relaxed)
    }

    fn failed(&mut self) {
        self.monitor = None;
        self.history.lose();
        self.history_gap_pending = true;
        self.events.lose();
        self.pending = true;
        self.retry_at = self.now() + RETRY_MS;
        self.monitor_note = "Live monitoring unavailable; retrying (F5 remains available)".into();
    }

    fn open_monitor(&mut self) {
        let Ok(socket) = udev::MonitorBuilder::new().and_then(|builder| builder.listen()) else {
            return self.failed();
        };
        // Best effort, unprivileged. Failure to enlarge the socket is harmless; an overflow
        // below triggers a fresh reconciliation and invalidates identities.
        // SAFETY: the pointer comes from a live monitor owned by `socket`.
        unsafe {
            udev::ffi::udev_monitor_set_receive_buffer_size(socket.as_raw(), 1024 * 1024);
        }
        self.monitor = Some(socket);
        self.monitor_note.clear();
        self.events.lose(); // A reopened monitor cannot vouch for instances during its gap.
        self.pending = true;
    }

    /// Bounded drain, even under a continuous stream. On overflow the monitor is reopened
    /// before scanning so an abandoned queue cannot masquerade as current state.
    fn drain(&mut self) {
        for _ in 0..MonitorState::LIMIT {
            let Some(monitor) = &self.monitor else { return };
            let mut poll = libc::pollfd { fd: monitor.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            clear_errno();
            // SAFETY: `poll` is a valid single-element array for the call's duration.
            let ready = unsafe { libc::poll(&mut poll, 1, 0) };
            if ready < 0 {
                if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    return;
                }
                return self.failed();
            }
            if poll.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                return self.failed();
            }
            if ready == 0 || poll.revents & libc::POLLIN == 0 {
                return;
            }
            clear_errno();
            let Some(event) = monitor.iter().next() else {
                let error = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                if matches!(error, libc::EAGAIN | libc::EINTR) {
                    return;
                }
                return self.failed();
            };
            let text = |value: Option<&std::ffi::OsStr>| value.map(|v| v.to_string_lossy().into_owned()).unwrap_or_default();
            let action = text(event.action());
            let path = event.syspath().to_string_lossy().into_owned();
            let initialized = text(event.property_value("USEC_INITIALIZED"));
            // Do not read sysfs contents during event reception. An inode hint is usable only
            // while this path is still present; removals use the already-observed instance,
            // never a newly reused path's inode.
            let incarnation = match std::fs::metadata(&path) {
                Ok(info) if action != "remove" => format!("{}:{}:{initialized}", info.dev(), info.ino()),
                _ => String::new(),
            };
            let old_path = text(event.property_value("DEVPATH_OLD")).strip_prefix("/devices/").map(|rest| format!("/sys/devices/{rest}")).unwrap_or_default();
            let observation = DeviceEvent {
                observed: Some(SystemTime::now()),
                elapsed_ms: self.now(),
                sequence: event.sequence_number(),
                path: path.clone(),
                action: action.clone(),
                subsystem: text(event.subsystem()),
                devtype: text(event.devtype()),
                driver: text(event.driver()),
                initialized,
                incarnation,
                old_path: old_path.clone(),
                ..Default::default()
            };
            drop(event);
            self.history.observe(observation);
            self.events.observe(&path, action == "remove");
            if path.starts_with("/sys/devices/") {
                self.last_event = self.now();
                if self.first_event < 0 {
                    self.first_event = self.last_event;
                }
            }
            if !old_path.is_empty() {
                self.events.observe(&old_path, true);
            }
        }
        self.failed();
    }

    fn collect(&self) -> Inventory {
        let mut inventory = Inventory { request: self.shared.request.load(Ordering::SeqCst), ..Default::default() };
        match scan_cancellable(Some(&self.shared.interrupt)) {
            Ok(result) => {
                inventory.devices = result.devices;
                inventory.skipped = result.skipped;
            }
            Err(message) => inventory.error = message,
        }
        inventory
    }

    fn run(mut self, publish: &dyn Fn(Inventory)) {
        let mut periodic_at = PERIODIC_MS;
        let mut handled_request = 0;
        let mut initial = true;
        let mut awaiting_gui = false;
        // Enable reception BEFORE the initial snapshot, and retain it across scans.
        self.open_monitor();
        while !self.interrupted() {
            self.drain();
            let now = self.now();
            if self.monitor.is_none() && now >= self.retry_at {
                self.open_monitor();
            }
            if self.events.dirty {
                self.pending = true;
                if self.first_event < 0 {
                    self.first_event = now;
                }
                // Quiet-time deadline is updated only when drain() receives events.
            }
            if awaiting_gui && self.shared.acknowledged.load(Ordering::SeqCst) {
                awaiting_gui = false;
            }
            let request = self.shared.request.load(Ordering::SeqCst);
            let manual = request != handled_request;
            let due = initial
                || manual
                || now >= periodic_at
                || (self.pending && (self.first_event < 0 || now - self.first_event >= BURST_MS || now - self.last_event >= QUIET_MS));
            if !awaiting_gui && due {
                let mut inventory = Inventory::default();
                // At most two scans per publication. Continuous churn cannot keep restarting a
                // scan forever; uncertain dependency branches are withheld.
                for _ in 0..2 {
                    self.events.begin_scan();
                    inventory = self.collect();
                    self.drain();
                    if !self.events.dirty || self.interrupted() || !inventory.error.is_empty() {
                        break;
                    }
                    if self.monitor.is_none() {
                        self.open_monitor();
                    }
                }
                if self.interrupted() {
                    break;
                }
                self.events.exclude_unsettled(&mut inventory);
                inventory.removed_paths = self.events.removed.clone();
                inventory.identity_lost = self.events.lost;
                inventory.monitor_note = self.monitor_note.clone();
                // MonitorState can also detect bounded-hint loss independently of socket
                // failure. It must end event associations as well as UI generations.
                if inventory.identity_lost && !initial && !self.history_gap_pending {
                    self.history.lose();
                }
                self.history.reconcile(&mut inventory.devices);
                inventory.events = self.history.snapshot(self.monitor.is_some());
                if self.events.dirty && inventory.monitor_note.is_empty() {
                    inventory.monitor_note = "Changing device branches are being reconciled".into();
                }
                handled_request = inventory.request;
                self.pending = self.events.dirty;
                self.events.removed.clear();
                self.events.lost = false;
                self.history_gap_pending = false;
                initial = false;
                self.first_event = if self.pending { self.now() } else { -1 };
                self.last_event = self.now();
                periodic_at = self.now() + if self.monitor.is_some() && inventory.error.is_empty() { PERIODIC_MS } else { RETRY_MS };
                self.shared.acknowledged.store(false, Ordering::SeqCst);
                awaiting_gui = true;
                publish(inventory);
            }
            // Short interruptible polling also observes manual refresh and UI acknowledgement.
            match &self.monitor {
                Some(monitor) => {
                    let mut poll = libc::pollfd { fd: monitor.as_raw_fd(), events: libc::POLLIN, revents: 0 };
                    // SAFETY: `poll` is a valid single-element array for the call's duration.
                    let ready = unsafe { libc::poll(&mut poll, 1, 50) };
                    if ready > 0 && poll.revents & libc::POLLIN != 0 {
                        self.last_event = self.now();
                    }
                    if ready < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                        self.failed();
                    }
                }
                None => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn session_publishes_one_snapshot_at_a_time_and_stops_promptly() {
        let (tx, rx) = mpsc::channel();
        let mut session = Session::start(move |inventory| {
            let _ = tx.send(inventory);
        });
        let first = rx.recv_timeout(Duration::from_secs(30)).expect("initial inventory");
        assert!(first.error.is_empty() && !first.devices.is_empty());
        assert_eq!(first.request, 0);
        assert!(first.devices.iter().filter(|d| d.subsystem == "pci").all(|d| d.event_instance != 0));
        assert!(rx.recv_timeout(Duration::from_millis(400)).is_err(), "no new snapshot before acknowledgement");

        let request = session.request_refresh();
        session.acknowledge();
        let second = rx.recv_timeout(Duration::from_secs(30)).expect("manual refresh");
        assert!(second.request >= request);
        assert!(first.events.started == second.events.started, "history persists across scans");

        let stopped = Instant::now();
        session.request_stop();
        session.join();
        assert!(stopped.elapsed() < Duration::from_secs(10));
    }
}
