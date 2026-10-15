//! Translation entry point. Captions are English source text (the gettext msgid); a catalogue in
//! the bound text domain translates them, and anything untranslated stays English.
//! Core stays free of any UI toolkit.
use std::ffi::{c_char, c_int, CStr, CString};
use std::path::Path;

// glibc ships gettext in libc itself, so no extra library or crate is needed.
extern "C" {
    fn setlocale(category: c_int, locale: *const c_char) -> *mut c_char;
    fn bindtextdomain(domain: *const c_char, dir: *const c_char) -> *mut c_char;
    fn bind_textdomain_codeset(domain: *const c_char, codeset: *const c_char) -> *mut c_char;
    fn dgettext(domain: *const c_char, msgid: *const c_char) -> *mut c_char;
}
const LC_ALL: c_int = 6;

/// Text domain of the installed catalogues (`<locale dir>/<lang>/LC_MESSAGES/<domain>.mo`).
pub const TEXT_DOMAIN: &str = "qlinuxdevicemanager";
const DOMAIN: &CStr = c"qlinuxdevicemanager";

/// Selects the user's locale and binds the catalogue directory. `QLDM_LOCALE_DIR` overrides the
/// compiled-in `<prefix>/share/locale` (used by tests and uninstalled runs). Call once at
/// startup; failures leave the English source text in place.
pub fn init() {
    let dir = std::env::var_os("QLDM_LOCALE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| Path::new(option_env!("QLDM_LOCALE_DIR").unwrap_or("/usr/share/locale")).to_path_buf());
    let Ok(dir) = CString::new(std::os::unix::ffi::OsStrExt::as_bytes(dir.as_os_str())) else { return };
    // SAFETY: all pointers are valid NUL-terminated strings for the duration of each call.
    unsafe {
        setlocale(LC_ALL, c"".as_ptr());
        bindtextdomain(DOMAIN.as_ptr(), dir.as_ptr());
        bind_textdomain_codeset(DOMAIN.as_ptr(), c"UTF-8".as_ptr());
    }
}

/// Translates an English source string through the bound catalogue (identity when untranslated).
pub fn tr(text: &str) -> String {
    let Ok(msgid) = CString::new(text) else { return text.to_string() };
    if text.is_empty() {
        return String::new(); // dgettext("") would return the catalogue header.
    }
    // SAFETY: dgettext returns msgid itself or a pointer into the loaded catalogue, valid here.
    unsafe {
        let translated = dgettext(DOMAIN.as_ptr(), msgid.as_ptr());
        if translated.is_null() {
            text.to_string()
        } else {
            CStr::from_ptr(translated).to_string_lossy().into_owned()
        }
    }
}

/// Replaces the first `%1` placeholder, as `QString::arg` did.
pub fn arg(text: String, value: &str) -> String {
    text.replacen("%1", value, 1)
}

/// Replaces `%1` and `%2` (in that order of appearance, as `QString::arg` twice would).
pub fn arg2(text: String, first: &str, second: &str) -> String {
    text.replacen("%1", first, 1).replacen("%2", second, 1)
}

/// Replaces `%1`..`%9` with `values` in order (multi-argument `QString::arg`).
pub fn argn(text: String, values: &[&str]) -> String {
    let mut text = text;
    for (index, value) in values.iter().enumerate().take(9) {
        text = text.replacen(&format!("%{}", index + 1), value, 1);
    }
    text
}

/// `%g`-style number (`QLocale::toString(x, 'g', digits)`): up to `digits` significant digits,
/// trailing zeros removed, exponent form outside 1e-4 ..< 10^digits.
pub fn format_g(value: f64, digits: usize) -> String {
    if value == 0.0 || !value.is_finite() {
        return if value.is_nan() {
            "nan".into()
        } else if value.is_infinite() {
            if value > 0.0 {
                "inf".into()
            } else {
                "-inf".into()
            }
        } else {
            "0".into()
        };
    }
    let exponent = value.abs().log10().floor() as i32;
    // Rounding to 8 digits can carry into the next decade (9.9999999 -> 10).
    let rounded: f64 = format!("{:.*e}", digits - 1, value).parse().unwrap_or(value);
    let exponent = if rounded.abs() >= 10f64.powi(exponent + 1) { exponent + 1 } else { exponent };
    let trim = |text: String| {
        if text.contains('.') {
            text.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            text
        }
    };
    if !(-4..digits as i32).contains(&exponent) {
        let text = format!("{:.*e}", digits - 1, value);
        let (mantissa, power) = text.split_once('e').expect("exponent form");
        let power: i32 = power.parse().expect("integer exponent");
        return format!("{}e{}{:02}", trim(mantissa.to_string()), if power < 0 { '-' } else { '+' }, power.abs());
    }
    trim(format!("{:.*}", (digits as i32 - 1 - exponent).max(0) as usize, value))
}

/// Integer with thousands separators, as `QLocale::toString(qlonglong)` gives for English.
/// Locale-specific separators arrive with the gettext work.
pub fn format_integer(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(c);
    }
    grouped
}

/// Fixed-point number (`QLocale::toString(double, 'f', precision)`).
pub fn format_fixed(value: f64, precision: usize) -> String {
    format!("{value:.precision$}")
}

/// IEC data size like `QLocale::formattedDataSize(bytes)`: plain bytes below 1 KiB, else two
/// decimals with a binary unit.
pub fn format_data_size(bytes: u64) -> String {
    format_data_size_with(bytes, 2)
}

/// Like [`format_data_size`] with `precision` decimals (capped at `3 * power`, as Qt does).
pub fn format_data_size_with(bytes: u64, precision: usize) -> String {
    const UNITS: [&str; 7] = ["bytes", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    if bytes == 0 {
        return "0 bytes".into();
    }
    let power = ((63 - bytes.leading_zeros()) / 10) as usize;
    if power == 0 {
        return format!("{bytes} bytes");
    }
    format!("{} {}", format_fixed(bytes as f64 / 1024f64.powi(power as i32), precision.min(3 * power)), UNITS[power])
}

/// `YYYY-MM-DDTHH:MM:SS` in local time (`Qt::ISODate`), or `None` if the time is unrepresentable.
pub fn format_local_iso(seconds: i64) -> Option<String> {
    let time = seconds as libc::time_t;
    // SAFETY: `tm` is a plain-old-data out-parameter, and localtime_r/strftime only write
    // within the buffers given.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&time, &mut tm).is_null() {
            return None;
        }
        let mut buffer = [0u8; 32];
        let format = c"%Y-%m-%dT%H:%M:%S";
        let written = libc::strftime(buffer.as_mut_ptr().cast(), buffer.len(), format.as_ptr(), &tm);
        (written > 0).then(|| String::from_utf8_lossy(&buffer[..written]).into_owned())
    }
}

/// `YYYY-MM-DDTHH:MM:SS.mmm` in local time (`Qt::ISODateWithMs`).
pub fn format_system_time_iso_ms(time: std::time::SystemTime) -> String {
    let since = time.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let seconds = format_local_iso(since.as_secs() as i64).unwrap_or_default();
    format!("{seconds}.{:03}", since.subsec_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_group_and_sizes_follow_qt() {
        assert_eq!(format_integer(0), "0");
        assert_eq!(format_integer(999), "999");
        assert_eq!(format_integer(1234567), "1,234,567");
        assert_eq!(format_data_size(0), "0 bytes");
        assert_eq!(format_data_size(512), "512 bytes");
        assert_eq!(format_data_size(1024), "1.00 KiB");
        assert_eq!(format_data_size(1234), "1.21 KiB");
        assert_eq!(format_data_size(1_000_204_886_016), "931.51 GiB");
        assert_eq!(format_fixed(42.0, 1), "42.0");
        assert_eq!(arg("%1 bytes".into(), "5"), "5 bytes");
    }

    #[test]
    fn local_time_is_iso_formatted() {
        let text = format_local_iso(1_792_023_126).unwrap();
        assert_eq!((text.len(), &text[4..5], &text[10..11]), (19, "-", "T"));
    }
}
