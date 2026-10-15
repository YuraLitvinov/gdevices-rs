//! Window settings, kept in `$XDG_CONFIG_HOME/QLinuxDeviceManager/settings.conf` as plain
//! `key = value` lines (no schema installation needed, so the binary also runs from a build
//! tree). Only the window geometry persists: the tree always starts on "Devices by type" with
//! default visibility and only the computer expanded. A missing, unreadable or corrupt file is
//! ignored and defaults apply; saving failures are silent.
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowSettings {
    pub width: i32,
    pub height: i32,
    pub maximized: bool,
}

impl Default for WindowSettings {
    fn default() -> Self {
        Self { width: 850, height: 650, maximized: false }
    }
}

fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("QLinuxDeviceManager").join("settings.conf"))
}

pub fn parse(text: &str) -> WindowSettings {
    let mut settings = WindowSettings::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else { continue };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "window.width" => {
                if let Some(n) = value.parse::<i32>().ok().filter(|n| (200..=16384).contains(n)) {
                    settings.width = n;
                }
            }
            "window.height" => {
                if let Some(n) = value.parse::<i32>().ok().filter(|n| (200..=16384).contains(n)) {
                    settings.height = n;
                }
            }
            "window.maximized" => settings.maximized = value == "true",
            _ => {}
        }
    }
    settings
}

pub fn serialize(settings: &WindowSettings) -> String {
    format!("window.width = {}\nwindow.height = {}\nwindow.maximized = {}\n", settings.width, settings.height, settings.maximized)
}

pub fn load() -> WindowSettings {
    config_path().and_then(|p| std::fs::read_to_string(p).ok()).map(|t| parse(&t)).unwrap_or_default()
}

pub fn save(settings: &WindowSettings) {
    let Some(path) = config_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, serialize(settings));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_and_ignore_garbage() {
        let saved = WindowSettings { width: 1024, height: 768, maximized: true };
        assert_eq!(parse(&serialize(&saved)), saved);
        assert_eq!(parse(""), WindowSettings::default());
        assert_eq!(parse("\u{0}\u{1}garbage\n=\nwindow.width = lots\nwindow.height = 5\n"), WindowSettings::default());
        assert_eq!(parse("window.width = 900\nunknown = 1\n").width, 900);
        assert_eq!(parse("window.width = 99999999\n").width, 850, "absurd sizes are ignored");
    }
}
