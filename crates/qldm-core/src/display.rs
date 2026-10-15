//! Desktop-session display state for one DRM connector, ported from
//! `src/devices/displayproperties.cpp` and `displaybackend.h`.
//!
//! The kernel connector name is matched against the session protocol's output names; a name
//! that matches zero or several outputs is never guessed, and the session type decides the
//! backend (Wayland is never silently replaced by XWayland's emulated outputs). Numeric strings
//! use the C locale. Each backend runs on its own short-lived thread under a 2 s budget.
use crate::dirfd::available;
use crate::model::{Attribute, Device, ReadState};
use crate::sysfs::bounded_directory_names;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub const OUTPUT_LIMIT: usize = 256;
pub const TEXT_LIMIT: usize = 4096;
pub const QUERY_TIMEOUT: Duration = Duration::from_millis(2000);

#[derive(Clone, Debug, Default)]
pub struct DisplayProperties {
    pub backend: String,
    pub output: String,
    pub status: Attribute,
    pub values: HashMap<String, Attribute>,
    pub sources: HashMap<String, String>,
}

impl DisplayProperties {
    pub(crate) fn put(&mut self, key: &str, value: impl Into<String>, source: &str) {
        self.values.insert(key.into(), available(value));
        self.sources.insert(key.into(), source.into());
    }
}

pub(crate) fn error(number: i32) -> Attribute {
    Attribute { state: ReadState::Error, value: String::new(), error: number }
}

/// RandR's modesetting driver uses `HDMI-N` for the kernel's `HDMI-A-N`.
pub(crate) fn canonical_name(name: &str) -> String {
    match name.strip_prefix("HDMI-A-") {
        Some(rest) => format!("HDMI-{rest}"),
        None => name.to_string(),
    }
}

/// `QString::number(value, 'g', 8)`: up to 8 significant digits, trailing zeros removed.
pub(crate) fn number(value: f64) -> String {
    crate::i18n::format_g(value, 8)
}

/// Which session protocol to ask, from the environment.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Session {
    Wayland,
    X11,
    None,
}

pub(crate) fn session_from(session_type: &str, wayland_display: &str, display: &str) -> Session {
    let wayland = session_type == "wayland" || (session_type != "x11" && !wayland_display.is_empty());
    if wayland {
        Session::Wayland
    } else if !display.is_empty() {
        Session::X11
    } else {
        Session::None
    }
}

/// Runs `work` on a thread and waits at most [`QUERY_TIMEOUT`], polling `cancel`. `Err` is
/// `ETIMEDOUT` or `ECANCELED`; a thread that outlives the wait is abandoned.
pub(crate) fn run_bounded<T: Send + 'static>(cancel: Option<&AtomicBool>, work: impl FnOnce() -> T + Send + 'static) -> Result<T, i32> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(work());
    });
    let started = Instant::now();
    loop {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Err(libc::ECANCELED);
        }
        let remaining = QUERY_TIMEOUT.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(libc::ETIMEDOUT);
        }
        match receiver.recv_timeout(remaining.min(Duration::from_millis(50))) {
            Ok(value) => return Ok(value),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err(libc::EIO),
        }
    }
}

/// `connection_status` is the connector's `status` read from the open device directory (not
/// the possibly stale inventory snapshot).
pub fn collect_display_properties(device: &Device, connection_status: &Attribute, cancel: Option<&AtomicBool>) -> DisplayProperties {
    let mut result = DisplayProperties::default();
    if device.subsystem != "drm" || !device.sysname.contains('-') {
        result.status.state = ReadState::NotApplicable;
        return result;
    }
    if connection_status.state == ReadState::Available && connection_status.value == "disconnected" {
        result.status = available("disconnected");
        result.sources.insert("configuration".into(), format!("{}/status", device.path));
        return result;
    }
    let connector = device.sysname[device.sysname.find('-').expect("checked") + 1..].to_string();
    // Session protocols name outputs, not GPU-qualified kernel connectors. Do not silently
    // attribute eDP-1/DP-1 to the wrong GPU on multi-card systems.
    let names = match bounded_directory_names("/sys/class/drm", (OUTPUT_LIMIT * 2) as i64, &[], cancel) {
        Ok(names) => names,
        Err(code) => {
            result.status = error(code);
            return result;
        }
    };
    let matches = names.iter().filter_map(|name| name.split_once('-')).filter(|(_, rest)| canonical_name(rest) == canonical_name(&connector)).count();
    if matches != 1 {
        result.status = error(if matches > 0 { libc::ENOTUNIQ } else { libc::ENODEV });
        return result;
    }
    let env = |key: &str| std::env::var(key).unwrap_or_default();
    let backend = session_from(&env("XDG_SESSION_TYPE"), &env("WAYLAND_DISPLAY"), &env("DISPLAY"));
    let outcome = match backend {
        Session::Wayland => run_bounded(cancel, move || crate::display_wayland::collect(&connector)),
        Session::X11 => run_bounded(cancel, move || crate::display_x11::collect(&connector)),
        Session::None => return result,
    };
    // Never fall back to XWayland's emulated outputs after a Wayland failure.
    match outcome {
        Ok(found) => found,
        Err(code) => {
            result.backend = if backend == Session::Wayland { "Wayland" } else { "X11 / RandR" }.into();
            result.status = error(code);
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hdmi_connector_names_are_canonicalised() {
        assert_eq!(canonical_name("HDMI-A-1"), "HDMI-1");
        assert_eq!(canonical_name("HDMI-1"), "HDMI-1");
        assert_eq!(canonical_name("eDP-1"), "eDP-1");
    }

    #[test]
    fn numbers_follow_g8() {
        for (value, want) in [
            (60.0, "60"),
            (59.94, "59.94"),
            (59.940059, "59.940059"),
            (1.5, "1.5"),
            (0.0, "0"),
            (144.0, "144"),
            (1.0 / 3.0, "0.33333333"),
            (123456789.0, "1.2345679e+08"),
            (0.00001234, "1.234e-05"),
            (9.99999999, "10"),
            (-2.25, "-2.25"),
            (100.0, "100"),
        ] {
            assert_eq!(number(value), want, "{value}");
        }
    }

    #[test]
    fn the_session_environment_chooses_the_backend() {
        assert_eq!(session_from("wayland", "", ""), Session::Wayland);
        assert_eq!(session_from("", "wayland-0", ":0"), Session::Wayland, "a Wayland display wins when the type is unset");
        assert_eq!(session_from("x11", "wayland-0", ":0"), Session::X11, "an explicit x11 session type wins");
        assert_eq!(session_from("tty", "", ""), Session::None);
    }

    #[test]
    fn non_connectors_and_disconnected_outputs_do_not_query_a_session() {
        let card = Device { subsystem: "drm".into(), sysname: "card0".into(), ..Default::default() };
        assert_eq!(collect_display_properties(&card, &Attribute::default(), None).status.state, ReadState::NotApplicable);
        let connector = Device { subsystem: "drm".into(), sysname: "card0-DP-1".into(), path: "/sys/p".into(), ..Default::default() };
        let disconnected = Attribute { state: ReadState::Available, value: "disconnected".into(), error: 0 };
        let found = collect_display_properties(&connector, &disconnected, None);
        assert_eq!(found.status.value, "disconnected");
        assert_eq!(found.sources["configuration"], "/sys/p/status");
    }

    #[test]
    fn an_unknown_connector_is_not_attributed_to_any_output() {
        let connector = Device { subsystem: "drm".into(), sysname: "card0-NOSUCH-9".into(), ..Default::default() };
        let found = collect_display_properties(&connector, &Attribute::default(), None);
        assert_eq!((found.status.state, found.status.error), (ReadState::Error, libc::ENODEV));
    }

    #[test]
    fn bounded_work_times_out_and_cancels() {
        assert_eq!(run_bounded(None, || 7).unwrap(), 7);
        let cancel = AtomicBool::new(true);
        assert_eq!(run_bounded(Some(&cancel), || std::thread::sleep(Duration::from_millis(300))), Err(libc::ECANCELED));
    }
}
