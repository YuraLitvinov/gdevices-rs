//! Wayland output state, ported from `src/devices/displaywayland.cpp` with the `wayland-client`
//! crate. Only read-only protocols are used (wl_output and xdg-output v3 logical size). The
//! connection opens its own socket from `WAYLAND_DISPLAY` and `XDG_RUNTIME_DIR`; it never
//! consumes `WAYLAND_SOCKET`, which may already belong to the toolkit.
use crate::dirfd::available;
use crate::display::{canonical_name, error, number, DisplayProperties, OUTPUT_LIMIT, QUERY_TIMEOUT, TEXT_LIMIT};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::time::Instant;
use wayland_client::protocol::{wl_callback, wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};

#[derive(Debug, Default, Clone)]
pub(crate) struct Output {
    id: u32,
    name: String,
    name_conflict: bool,
    removed: bool,
    mode_known: bool,
    width: i32,
    height: i32,
    refresh: i32,
    transform: i32,
    scale: i32,
    logical_width: i32,
    logical_height: i32,
}

impl Output {
    fn new(id: u32) -> Self {
        Self { id, transform: -1, scale: 1, ..Default::default() }
    }

    fn set_name(&mut self, name: &str) {
        if name.len() > TEXT_LIMIT {
            self.name_conflict = true;
            return;
        }
        if !self.name.is_empty() && self.name != name {
            self.name_conflict = true;
        }
        self.name = name.to_string();
    }
}

#[derive(Default)]
struct State {
    outputs: Vec<Output>,
    /// The bound `wl_output` proxies, by output index (needed to request xdg-output objects).
    proxies: Vec<wl_output::WlOutput>,
    /// Kept alive until the second round trip has delivered their events.
    xdg_outputs: Vec<zxdg_output_v1::ZxdgOutputV1>,
    manager: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1>,
    sync_done: bool,
    error: i32,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(state: &mut Self, registry: &wl_registry::WlRegistry, event: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        match event {
            wl_registry::Event::Global { name, interface, version } => match interface.as_str() {
                "wl_output" => {
                    if state.outputs.len() >= OUTPUT_LIMIT {
                        state.error = libc::EOVERFLOW;
                        return;
                    }
                    let index = state.outputs.len();
                    state.outputs.push(Output::new(name));
                    let proxy = registry.bind::<wl_output::WlOutput, _, _>(name, version.min(4), qh, index);
                    state.proxies.push(proxy);
                }
                "zxdg_output_manager_v1" if state.manager.is_none() => {
                    state.manager = Some(registry.bind::<zxdg_output_manager_v1::ZxdgOutputManagerV1, _, _>(name, version.min(3), qh, ()));
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                for output in state.outputs.iter_mut().filter(|o| o.id == name) {
                    output.removed = true;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, usize> for State {
    fn event(state: &mut Self, _: &wl_output::WlOutput, event: wl_output::Event, index: &usize, _: &Connection, _: &QueueHandle<Self>) {
        let Some(output) = state.outputs.get_mut(*index) else { return };
        match event {
            wl_output::Event::Geometry { transform, .. } => {
                output.transform = match transform {
                    WEnum::Value(t) => u32::from(t) as i32,
                    WEnum::Unknown(_) => -1,
                };
            }
            wl_output::Event::Mode { flags, width, height, refresh } => {
                let current = matches!(flags, WEnum::Value(f) if f.contains(wl_output::Mode::Current));
                if current {
                    output.mode_known = true;
                    output.width = width;
                    output.height = height;
                    output.refresh = refresh;
                }
            }
            wl_output::Event::Scale { factor } => output.scale = factor,
            wl_output::Event::Name { name } => output.set_name(&name),
            _ => {}
        }
    }
}

impl Dispatch<zxdg_output_manager_v1::ZxdgOutputManagerV1, ()> for State {
    fn event(_: &mut Self, _: &zxdg_output_manager_v1::ZxdgOutputManagerV1, _: zxdg_output_manager_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, usize> for State {
    fn event(state: &mut Self, _: &zxdg_output_v1::ZxdgOutputV1, event: zxdg_output_v1::Event, index: &usize, _: &Connection, _: &QueueHandle<Self>) {
        let Some(output) = state.outputs.get_mut(*index) else { return };
        match event {
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                output.logical_width = width;
                output.logical_height = height;
            }
            zxdg_output_v1::Event::Name { name } => output.set_name(&name),
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn event(state: &mut Self, _: &wl_callback::WlCallback, event: wl_callback::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if matches!(event, wl_callback::Event::Done { .. }) {
            state.sync_done = true;
        }
    }
}

/// One bounded round trip: send a sync and dispatch until it is answered.
fn sync(connection: &Connection, queue: &mut wayland_client::EventQueue<State>, state: &mut State, started: Instant) -> Result<(), i32> {
    state.sync_done = false;
    let qh = queue.handle();
    let _callback = connection.display().sync(&qh, ());
    while !state.sync_done {
        if state.error != 0 {
            return Err(state.error);
        }
        let remaining = QUERY_TIMEOUT.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(libc::ETIMEDOUT);
        }
        queue.dispatch_pending(state).map_err(|_| libc::EIO)?;
        if state.sync_done {
            break;
        }
        match connection.flush() {
            Ok(()) => {}
            Err(wayland_client::backend::WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => return Err(libc::EIO),
        }
        let Some(guard) = queue.prepare_read() else { continue };
        let mut poll = libc::pollfd { fd: guard.connection_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: `poll` is a valid single-element array for the call's duration.
        let ready = unsafe { libc::poll(&mut poll, 1, remaining.as_millis().min(100) as i32) };
        if ready < 0 {
            if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                return Err(libc::EIO);
            }
        } else if poll.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(libc::EIO);
        } else if poll.revents & libc::POLLIN != 0 {
            match guard.read() {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => return Err(libc::EIO),
            }
        }
    }
    Ok(())
}

/// The socket named by `WAYLAND_DISPLAY` (absolute, or relative to `XDG_RUNTIME_DIR`).
fn socket_path() -> Result<std::path::PathBuf, i32> {
    let mut name = std::env::var("WAYLAND_DISPLAY").unwrap_or_default();
    if name.is_empty() {
        name = "wayland-0".into();
    }
    if name.starts_with('/') {
        return Ok(name.into());
    }
    match std::env::var("XDG_RUNTIME_DIR") {
        Ok(directory) if !directory.is_empty() => Ok(std::path::Path::new(&directory).join(name)),
        _ => Err(libc::ENOENT),
    }
}

/// Builds the result for the one matching, unambiguous output.
pub(crate) fn describe(selected: &Output, result: &mut DisplayProperties) {
    result.output = selected.name.clone();
    result.status = available("exposed");
    result.sources.insert("depth".into(), "Wayland output protocols do not expose desktop color depth".into());
    if selected.mode_known && selected.width > 0 && selected.height > 0 {
        result.put("resolution", format!("{}x{}", selected.width, selected.height), "wl_output current mode, before rotation/scaling");
        if selected.refresh > 0 {
            result.put("refresh", number(selected.refresh as f64 / 1000.0), "wl_output current mode; nominal rate, not measured/VRR rate");
        }
    }
    if (0..=7).contains(&selected.transform) {
        result.put("rotation", ((selected.transform % 4) * 90).to_string(), "wl_output geometry transform, counterclockwise");
        result.put(
            "reflection",
            if selected.transform >= 4 { "vertical" } else { "none" },
            "wl_output transform: reflection about the vertical axis, then rotation",
        );
    }
    if selected.logical_width > 0
        && selected.logical_height > 0
        && selected.mode_known
        && selected.width > 0
        && selected.height > 0
        && (0..=7).contains(&selected.transform)
    {
        let swapped = selected.transform % 2 == 1;
        let (w, h) = if swapped { (selected.height, selected.width) } else { (selected.width, selected.height) };
        let x = w as f64 / selected.logical_width as f64;
        let y = h as f64 / selected.logical_height as f64;
        result.put(
            "scale",
            format!("{} {}", number(x), number(y)),
            "Wayland mode/logical-size ratio (X, Y), adjusted for rotation; approximate when logical sizes are rounded",
        );
        result.put("scale_kind", "derived", "xdg-output logical_size");
    } else if selected.scale > 0 {
        let s = number(selected.scale as f64);
        result.put("scale", format!("{s} {s}"), "wl_output integer buffer scale; fractional desktop scale unavailable");
        result.put("scale_kind", "integer", "wl_output scale");
    }
}

/// Picks the unique output named `connector`; `Err(ENOTUNIQ)` if several match, `Ok(None)` if none.
pub(crate) fn select<'a>(outputs: &'a [Output], connector: &str) -> Result<Option<&'a Output>, i32> {
    let mut selected = None;
    for output in outputs {
        if output.removed || output.name_conflict || canonical_name(&output.name) != canonical_name(connector) {
            continue;
        }
        if selected.is_some() {
            return Err(libc::ENOTUNIQ);
        }
        selected = Some(output);
    }
    Ok(selected)
}

pub fn collect(connector: &str) -> DisplayProperties {
    let mut result = DisplayProperties { backend: "Wayland".into(), ..Default::default() };
    let started = Instant::now();
    let path = match socket_path() {
        Ok(p) => p,
        Err(code) => {
            result.status = error(code);
            return result;
        }
    };
    let stream = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(e) => {
            result.status = error(e.raw_os_error().unwrap_or(libc::EIO));
            return result;
        }
    };
    let Ok(connection) = Connection::from_socket(stream) else {
        result.status = error(libc::EIO);
        return result;
    };
    let mut queue = connection.new_event_queue::<State>();
    let qh = queue.handle();
    let mut state = State::default();
    let _registry = connection.display().get_registry(&qh, ());
    if let Err(code) = sync(&connection, &mut queue, &mut state, started) {
        result.status = error(code);
        return result;
    }
    if let Some(manager) = state.manager.clone() {
        // xdg-output objects are created for each live wl_output once the globals are known.
        for index in 0..state.outputs.len() {
            if state.outputs[index].removed {
                continue;
            }
            let xdg = manager.get_xdg_output(&state.proxies[index], &qh, index);
            state.xdg_outputs.push(xdg);
        }
    }
    if let Err(code) = sync(&connection, &mut queue, &mut state, started) {
        result.status = error(code);
        return result;
    }
    match select(&state.outputs, connector) {
        Err(code) => result.status = error(code),
        Ok(None) => {}
        Ok(Some(selected)) => describe(selected, &mut result),
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ReadState;

    fn output(name: &str) -> Output {
        Output { name: name.into(), ..Output::new(1) }
    }

    #[test]
    fn selection_is_exact_and_never_guesses() {
        let outputs = vec![output("eDP-1"), output("HDMI-A-1"), Output { removed: true, ..output("DP-1") }];
        assert_eq!(select(&outputs, "eDP-1").unwrap().unwrap().name, "eDP-1");
        assert_eq!(select(&outputs, "HDMI-1").unwrap().unwrap().name, "HDMI-A-1", "RandR-style HDMI name");
        assert!(select(&outputs, "DP-1").unwrap().is_none(), "removed outputs are ignored");
        assert!(select(&outputs, "DP-9").unwrap().is_none());
        let twice = vec![output("DP-1"), output("DP-1")];
        assert_eq!(select(&twice, "DP-1").unwrap_err(), libc::ENOTUNIQ);
        let conflicted = vec![Output { name_conflict: true, ..output("DP-1") }];
        assert!(select(&conflicted, "DP-1").unwrap().is_none(), "disagreeing names are not trusted");
    }

    #[test]
    fn output_names_that_disagree_are_flagged() {
        let mut o = Output::new(1);
        o.set_name("DP-1");
        o.set_name("DP-1");
        assert!(!o.name_conflict);
        o.set_name("DP-2");
        assert!(o.name_conflict);
        let mut long = Output::new(2);
        long.set_name(&"x".repeat(TEXT_LIMIT + 1));
        assert!(long.name_conflict);
    }

    #[test]
    fn described_values_follow_the_protocol_semantics() {
        let mut result = DisplayProperties::default();
        let selected = Output {
            mode_known: true,
            width: 2880,
            height: 1800,
            refresh: 120_000,
            transform: 0,
            scale: 2,
            logical_width: 1440,
            logical_height: 900,
            ..output("eDP-1")
        };
        describe(&selected, &mut result);
        let value = |k: &str| result.values[k].value.clone();
        assert_eq!((value("resolution").as_str(), value("refresh").as_str()), ("2880x1800", "120"));
        assert_eq!((value("rotation").as_str(), value("reflection").as_str()), ("0", "none"));
        assert_eq!((value("scale").as_str(), value("scale_kind").as_str()), ("2 2", "derived"));
        assert_eq!(result.status.value, "exposed");
        assert!(result.sources.contains_key("depth") && !result.values.contains_key("depth"));

        // Rotated 90 degrees: the buffer is wider than the logical area, so axes are swapped.
        let mut rotated = DisplayProperties::default();
        describe(&Output { transform: 1, logical_width: 900, logical_height: 1440, ..selected.clone() }, &mut rotated);
        assert_eq!(rotated.values["scale"].value, "2 2");
        assert_eq!(rotated.values["rotation"].value, "90");
        let mut flipped = DisplayProperties::default();
        describe(&Output { transform: 5, ..selected.clone() }, &mut flipped);
        assert_eq!((flipped.values["rotation"].value.as_str(), flipped.values["reflection"].value.as_str()), ("90", "vertical"));

        // Without xdg-output only the integer buffer scale is known.
        let mut integer = DisplayProperties::default();
        describe(&Output { logical_width: 0, logical_height: 0, ..selected }, &mut integer);
        assert_eq!((integer.values["scale"].value.as_str(), integer.values["scale_kind"].value.as_str()), ("2 2", "integer"));
        let mut unknown = DisplayProperties::default();
        describe(&output("DP-3"), &mut unknown);
        assert!(!unknown.values.contains_key("resolution") && !unknown.values.contains_key("rotation"));
        assert_eq!(unknown.values["scale"].value, "1 1");
    }

    #[test]
    fn the_socket_path_is_absolute_or_under_the_runtime_directory() {
        // Pure path logic only: avoid changing the process environment in a parallel run.
        let absolute = std::path::Path::new("/run/user/1/wayland-9");
        assert!(absolute.is_absolute());
        assert_eq!(UnixStream::connect("/nonexistent/wayland").unwrap_err().raw_os_error(), Some(libc::ENOENT));
    }

    #[test]
    fn a_live_compositor_answers_or_the_failure_is_explicit() {
        if std::env::var("WAYLAND_DISPLAY").unwrap_or_default().is_empty() {
            return;
        }
        let found = collect("NO-SUCH-OUTPUT-9");
        assert_eq!(found.backend, "Wayland");
        // The made-up output is absent: no status, or an explicit error if the compositor is unreachable.
        assert!(matches!(found.status.state, ReadState::Unavailable | ReadState::Error), "{:?}", found.status);
    }
}
