//! X11/RandR output state, ported from `src/devices/displayx11.cpp` with the pure-Rust `x11rb`
//! connection (no dynamic loading of libxcb). Reads only; a result that spans a modeset or
//! hotplug is rejected instead of showing mixed data.
use crate::dirfd::available;
use crate::display::{canonical_name, error, number, DisplayProperties, OUTPUT_LIMIT, TEXT_LIMIT};
use crate::model::{Attribute, ReadState};
use x11rb::connection::Connection;
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::rust_connection::RustConnection;

/// Nominal refresh rate of a RandR mode (not measured, not VRR).
pub(crate) fn refresh_hz(dot_clock: u32, htotal: u16, vtotal: u16, interlace: bool, double_scan: bool) -> Option<f64> {
    if htotal == 0 || vtotal == 0 || dot_clock == 0 {
        return None;
    }
    let mut hz = dot_clock as f64 / htotal as f64 / vtotal as f64;
    if interlace {
        hz *= 2.0;
    }
    if double_scan {
        hz /= 2.0;
    }
    Some(hz)
}

/// Rotation in degrees, counterclockwise, if exactly one rotation bit is set.
pub(crate) fn rotation_degrees(rotation: u16) -> Option<&'static str> {
    match rotation & 15 {
        1 => Some("0"),
        2 => Some("90"),
        4 => Some("180"),
        8 => Some("270"),
        _ => None,
    }
}

pub(crate) fn reflection(rotation: u16) -> &'static str {
    match (rotation & 16 != 0, rotation & 32 != 0) {
        (true, true) => "xy",
        (true, false) => "x",
        (false, true) => "y",
        (false, false) => "none",
    }
}

/// Framebuffer pixels per output pixel (X, Y) for an axis-aligned transform; `custom` otherwise.
pub(crate) fn scale_text(m: &x11rb::protocol::render::Transform) -> (String, bool) {
    let diagonal = m.matrix12 == 0 && m.matrix21 == 0 && m.matrix31 == 0 && m.matrix32 == 0 && m.matrix33 == 65536 && m.matrix11 > 0 && m.matrix22 > 0;
    if diagonal {
        (format!("{} {}", number(m.matrix11 as f64 / 65536.0), number(m.matrix22 as f64 / 65536.0)), true)
    } else {
        ("custom".into(), false)
    }
}

pub fn collect(connector: &str) -> DisplayProperties {
    collect_on(None, connector)
}

/// Like [`collect`] on an explicit display name (`None` reads `$DISPLAY`).
pub(crate) fn collect_on(display: Option<&str>, connector: &str) -> DisplayProperties {
    let mut result = DisplayProperties { backend: "X11 / RandR".into(), ..Default::default() };
    let Ok((connection, screen_number)) = RustConnection::connect(display) else {
        result.status = error(libc::EIO);
        return result;
    };
    let fail = |result: &mut DisplayProperties, code: i32| {
        result.status = error(code);
    };
    macro_rules! reply {
        ($call:expr) => {
            match $call.map_err(|_| ()).and_then(|cookie| cookie.reply().map_err(|_| ())) {
                Ok(reply) => reply,
                Err(()) => {
                    fail(&mut result, libc::EPROTO);
                    return result;
                }
            }
        };
    }
    let version = reply!(connection.randr_query_version(1, 3));
    if version.major_version < 1 || (version.major_version == 1 && version.minor_version < 3) {
        return result;
    }
    let Some(screen) = connection.setup().roots.get(screen_number) else {
        fail(&mut result, libc::EIO);
        return result;
    };
    let root = screen.root;
    let resources = reply!(connection.randr_get_screen_resources_current(root));
    if resources.outputs.len() > OUTPUT_LIMIT || resources.modes.len() > TEXT_LIMIT {
        fail(&mut result, libc::EOVERFLOW);
        return result;
    }
    let mut selected: u32 = 0;
    let mut matches = 0;
    let mut disconnected = false;
    for output in &resources.outputs {
        let info = reply!(connection.randr_get_output_info(*output, resources.config_timestamp));
        if info.status != randr::SetConfig::SUCCESS {
            fail(&mut result, libc::EAGAIN);
            return result;
        }
        if info.name.len() > TEXT_LIMIT {
            fail(&mut result, libc::EOVERFLOW);
            return result;
        }
        let name = String::from_utf8_lossy(&info.name).into_owned();
        if canonical_name(&name) != canonical_name(connector) {
            continue;
        }
        matches += 1;
        result.output = name;
        selected = info.crtc;
        disconnected = info.connection == randr::Connection::DISCONNECTED;
    }
    if matches != 1 {
        if matches > 0 {
            fail(&mut result, libc::ENOTUNIQ);
        }
        return result;
    }
    if disconnected {
        result.status = available("disconnected");
        result.sources.insert("configuration".into(), "RandR output connection status".into());
        return result;
    }
    result.status = available(if selected != 0 { "configured" } else { "inactive" });
    result.put("depth", screen.root_depth.to_string(), "X11 screen root depth; not panel/link bits per channel");
    if selected == 0 {
        for key in ["resolution", "refresh", "rotation", "reflection", "scale"] {
            result.values.insert(key.into(), Attribute { state: ReadState::NotApplicable, ..Default::default() });
        }
        return result;
    }
    let crtc = reply!(connection.randr_get_crtc_info(selected, resources.config_timestamp));
    if crtc.status != randr::SetConfig::SUCCESS || crtc.mode == 0 {
        fail(&mut result, libc::EAGAIN);
        return result;
    }
    let Some(mode) = resources.modes.iter().find(|m| m.id == crtc.mode) else {
        fail(&mut result, libc::EAGAIN);
        return result;
    };
    result.put("resolution", format!("{}x{}", mode.width, mode.height), "RandR current mode, before rotation/scaling");
    let flags = u32::from(mode.mode_flags);
    if let Some(hz) = refresh_hz(
        mode.dot_clock,
        mode.htotal,
        mode.vtotal,
        flags & u32::from(randr::ModeFlag::INTERLACE) != 0,
        flags & u32::from(randr::ModeFlag::DOUBLE_SCAN) != 0,
    ) {
        result.put("refresh", number(hz), "RandR current mode pixel clock/totals; nominal rate, not measured/VRR rate");
    }
    let rotation = u16::from(crtc.rotation);
    if let Some(degrees) = rotation_degrees(rotation) {
        result.put("rotation", degrees, "RandR CRTC rotation, counterclockwise");
    }
    result.put("reflection", reflection(rotation), "RandR CRTC reflection flags");
    match connection.randr_get_crtc_transform(selected).map_err(|_| ()).and_then(|c| c.reply().map_err(|_| ())) {
        Err(()) => {
            result.values.insert("scale".into(), error(libc::EPROTO));
        }
        Ok(transform) if !transform.has_transforms => {
            result.sources.insert("scale".into(), "RandR CRTC transforms are not supported by this output".into());
        }
        Ok(transform) => {
            let (text, axis_aligned) = scale_text(&transform.current_transform);
            result.put(
                "scale",
                text,
                if axis_aligned {
                    "RandR transform: framebuffer pixels per output pixel (X, Y); excludes desktop DPI and toolkit scaling"
                } else {
                    "RandR uses a non-axis-aligned transform; no single scale factor describes it"
                },
            );
        }
    }
    // Reject a result spanning a modeset/hotplug rather than showing mixed data.
    match connection.randr_get_screen_resources_current(root).map_err(|_| ()).and_then(|c| c.reply().map_err(|_| ())) {
        Ok(after) if after.config_timestamp == resources.config_timestamp && after.timestamp == resources.timestamp => {}
        other => {
            result.values.clear();
            result.sources.clear();
            fail(&mut result, if other.is_ok() { libc::EAGAIN } else { libc::EPROTO });
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_accounts_for_interlace_and_double_scan() {
        let hz = refresh_hz(148_500_000, 2200, 1125, false, false).unwrap();
        assert!((hz - 60.0).abs() < 1e-9);
        assert!((refresh_hz(74_250_000, 2200, 1125, true, false).unwrap() - 60.0).abs() < 1e-9);
        assert!((refresh_hz(148_500_000, 2200, 1125, false, true).unwrap() - 30.0).abs() < 1e-9);
        assert_eq!(refresh_hz(0, 2200, 1125, false, false), None);
        assert_eq!(refresh_hz(1, 0, 1125, false, false), None);
    }

    #[test]
    fn rotation_and_reflection_flags() {
        assert_eq!((rotation_degrees(1), rotation_degrees(2), rotation_degrees(4), rotation_degrees(8)), (Some("0"), Some("90"), Some("180"), Some("270")));
        assert_eq!(rotation_degrees(3), None, "two rotation bits are not a rotation");
        assert_eq!(rotation_degrees(1 | 16), Some("0"), "reflection bits do not affect rotation");
        assert_eq!((reflection(1), reflection(1 | 16), reflection(1 | 32), reflection(1 | 48)), ("none", "x", "y", "xy"));
    }

    #[test]
    fn transforms_report_scale_only_when_axis_aligned() {
        let m = |m11, m22, m12, m33| x11rb::protocol::render::Transform {
            matrix11: m11,
            matrix12: m12,
            matrix13: 0,
            matrix21: 0,
            matrix22: m22,
            matrix23: 0,
            matrix31: 0,
            matrix32: 0,
            matrix33: m33,
        };
        assert_eq!(scale_text(&m(65536, 65536, 0, 65536)), ("1 1".into(), true));
        assert_eq!(scale_text(&m(131072, 98304, 0, 65536)), ("2 1.5".into(), true));
        assert_eq!(scale_text(&m(65536, 65536, 5, 65536)), ("custom".into(), false));
        assert!(!scale_text(&m(0, 65536, 0, 65536)).1, "non-positive scale");
    }

    #[test]
    fn an_absent_server_is_an_io_error_not_a_guess() {
        // A display that cannot exist: the connection fails before any query.
        let found = collect_on(Some(":987"), "eDP-1");
        assert_eq!(found.backend, "X11 / RandR");
        assert_eq!((found.status.state, found.status.error), (ReadState::Error, libc::EIO));
    }
}
