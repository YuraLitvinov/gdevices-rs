//! Cached DRM connector reports (EDID, mode names), ported from
//! `src/devices/monitorproperties.cpp`. Hardware-reported data only, separate from the
//! desktop-session settings in [`crate::display`]; overrides, adapters or KVMs may supply the
//! EDID. The kernel descriptor is framed and checksummed before anything parses it, and two
//! reads that disagree reject the whole result.
use crate::dirfd::{available, failure, read_limited, Dir};
use crate::model::{Attribute, ReadState};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Debug, Default)]
pub struct MonitorProperties {
    pub applicable: bool,
    pub descriptor: Attribute,
    pub values: HashMap<String, Attribute>,
    pub sources: HashMap<String, String>,
}

struct Report {
    state: Attribute,
    bytes: Vec<u8>,
}

fn read_report(dir: &Dir, name: &str, limit: usize, cancel: Option<&AtomicBool>) -> Report {
    let mut file = match dir.open_file(name) {
        Ok(f) => f,
        Err(e) => return Report { state: failure(crate::dirfd::io_errno(&e)), bytes: Vec::new() },
    };
    match read_limited(&mut file, limit, &|| cancel.is_some_and(|c| c.load(Ordering::Relaxed))) {
        Err(error) => Report { state: failure(error), bytes: Vec::new() },
        Ok(bytes) if bytes.len() > limit => Report { state: failure(libc::EOVERFLOW), bytes: Vec::new() },
        Ok(bytes) => Report { state: Attribute { state: ReadState::Available, ..Default::default() }, bytes },
    }
}

fn same_report(a: &Report, b: &Report) -> bool {
    a.state.state == b.state.state && a.state.error == b.state.error && a.bytes == b.bytes
}

/// Optional libdisplay-info, opened at run time (no headers or link-time dependency).
#[cfg(feature = "display-info")]
mod info {
    use std::ffi::{c_char, c_void, CStr};

    type ParseFn = unsafe extern "C" fn(*const c_void, usize) -> *mut c_void;
    type DestroyFn = unsafe extern "C" fn(*mut c_void);
    type PointerFn = unsafe extern "C" fn(*mut c_void) -> *const c_void;
    type StringFn = unsafe extern "C" fn(*mut c_void) -> *const c_char;
    type OwnedStringFn = unsafe extern "C" fn(*mut c_void) -> *mut c_char;

    pub struct DisplayInfo {
        _library: libloading::Library,
        parse: ParseFn,
        destroy: DestroyFn,
        get_edid: PointerFn,
        failure_message: StringFn,
        make: OwnedStringFn,
        model: OwnedStringFn,
        serial: OwnedStringFn,
    }

    pub struct Decoded {
        pub has_edid: bool,
        pub warnings: Option<String>,
        pub make: Option<Vec<u8>>,
        pub model: Option<Vec<u8>>,
        pub serial: Option<Vec<u8>>,
    }

    impl DisplayInfo {
        pub fn open() -> Option<Self> {
            // SAFETY: only the documented libdisplay-info entry points are resolved, each with
            // its exact C signature (di_info_* in libdisplay-info/info.h).
            unsafe {
                let library = ["libdisplay-info.so.3", "libdisplay-info.so.2", "libdisplay-info.so.1", "libdisplay-info.so.0"]
                    .into_iter()
                    .find_map(|name| libloading::Library::new(name).ok())?;
                macro_rules! sym {
                    ($name:literal, $ty:ty) => {
                        *library.get::<$ty>(concat!($name, "\0").as_bytes()).ok()?
                    };
                }
                Some(Self {
                    parse: sym!("di_info_parse_edid", ParseFn),
                    destroy: sym!("di_info_destroy", DestroyFn),
                    get_edid: sym!("di_info_get_edid", PointerFn),
                    failure_message: sym!("di_info_get_failure_msg", StringFn),
                    make: sym!("di_info_get_make", OwnedStringFn),
                    model: sym!("di_info_get_model", OwnedStringFn),
                    serial: sym!("di_info_get_serial", OwnedStringFn),
                    _library: library,
                })
            }
        }

        /// Parses `edid`; `Err(errno)` if the library rejects it.
        pub fn decode(&self, edid: &[u8]) -> Result<Decoded, i32> {
            // SAFETY: `edid` is a valid readable slice; every returned object is released on
            // all paths, and owned strings are freed with `free` as the library documents.
            unsafe {
                *libc::__errno_location() = 0;
                let info = (self.parse)(edid.as_ptr().cast(), edid.len());
                if info.is_null() {
                    let error = *libc::__errno_location();
                    return Err(if error == libc::ENOMEM { libc::ENOMEM } else { libc::EPROTO });
                }
                let owned = |p: *mut c_char| -> Option<Vec<u8>> {
                    if p.is_null() {
                        return None;
                    }
                    let bytes = CStr::from_ptr(p).to_bytes().to_vec();
                    libc::free(p.cast());
                    Some(bytes)
                };
                let warnings = (self.failure_message)(info);
                let decoded = Decoded {
                    has_edid: !(self.get_edid)(info).is_null(),
                    warnings: (!warnings.is_null()).then(|| CStr::from_ptr(warnings).to_string_lossy().into_owned()),
                    make: owned((self.make)(info)),
                    model: owned((self.model)(info)),
                    serial: owned((self.serial)(info)),
                };
                (self.destroy)(info);
                Ok(decoded)
            }
        }
    }
}

/// A bounded text value from library-owned bytes: empty is unreported, too long an error.
#[cfg(feature = "display-info")]
fn owned_string(bytes: Option<Vec<u8>>) -> Attribute {
    match bytes {
        None => Attribute::default(),
        Some(b) if b.len() > 4096 => failure(libc::EOVERFLOW),
        Some(b) if b.is_empty() => Attribute::default(),
        Some(b) => available(String::from_utf8_lossy(&b).into_owned()),
    }
}

/// Base-block fields decoded straight from the EDID 1.x layout, independent of any library.
pub struct BaseBlock {
    pub manufacturer_code: String,
    pub product: u16,
    pub width_cm: u8,
    pub height_cm: u8,
    pub digital: bool,
    /// Declared bits per colour component (digital, EDID 1.4 only).
    pub depth: Option<u8>,
    pub interface: Option<&'static str>,
}

pub fn decode_base_block(bytes: &[u8]) -> BaseBlock {
    let word = u16::from_be_bytes([bytes[8], bytes[9]]);
    let letter = |shift: u32| char::from(b'A' + (((word >> shift) & 0x1f) as u8).saturating_sub(1));
    let input = bytes[20];
    let digital = input & 0x80 != 0;
    let (mut depth, mut interface) = (None, None);
    if digital && bytes[19] >= 4 {
        depth = match (input >> 4) & 7 {
            1 => Some(6),
            2 => Some(8),
            3 => Some(10),
            4 => Some(12),
            5 => Some(14),
            6 => Some(16),
            _ => None,
        };
        interface = match input & 0x0f {
            1 => Some("DVI"),
            2 => Some("HDMI-A"),
            3 => Some("HDMI-B"),
            4 => Some("MDDI"),
            5 => Some("DisplayPort"),
            _ => None,
        };
    }
    BaseBlock {
        manufacturer_code: [letter(10), letter(5), letter(0)].iter().collect(),
        product: u16::from_le_bytes([bytes[10], bytes[11]]),
        width_cm: bytes[21],
        height_cm: bytes[22],
        digital,
        depth,
        interface,
    }
}

fn decode_descriptor(result: &mut MonitorProperties, bytes: &[u8], path: &str) {
    if bytes.is_empty() {
        result.descriptor = Attribute::default();
        return;
    }
    // Reject truncated/corrupt framing before passing a bounded report to the parser.
    const HEADER: [u8; 8] = [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00];
    if bytes.len() < 128 || bytes[..8] != HEADER || bytes.len() != 128 * (1 + bytes[126] as usize) {
        result.descriptor = failure(libc::EPROTO);
        return;
    }
    if bytes.chunks(128).any(|block| block.iter().map(|b| *b as u32).sum::<u32>() & 255 != 0) {
        result.descriptor = failure(libc::EPROTO);
        return;
    }
    let version = format!("{}.{}", bytes[18], bytes[19]);
    result.values.insert("version".into(), available(version.clone()));
    result.sources.insert("version".into(), format!("{path}/edid: bytes 18–19"));
    if bytes[18] != 1 || bytes[19] > 4 {
        result.descriptor = Attribute { state: ReadState::Unsupported, value: version, error: 0 };
        return;
    }
    #[cfg(not(feature = "display-info"))]
    {
        result.descriptor = Attribute { state: ReadState::Unsupported, ..Default::default() };
    }
    #[cfg(feature = "display-info")]
    {
        let Some(library) = info::DisplayInfo::open() else {
            result.descriptor = Attribute { state: ReadState::Unsupported, ..Default::default() };
            return;
        };
        let decoded = match library.decode(bytes) {
            Ok(d) => d,
            Err(error) => {
                result.descriptor = failure(error);
                return;
            }
        };
        if !decoded.has_edid {
            result.descriptor = Attribute { state: ReadState::Unsupported, value: version, error: 0 };
            return;
        }
        result.descriptor = Attribute { state: ReadState::Available, ..Default::default() };
        let mut put = |key: &str, value: Attribute, source: &str| {
            result.values.insert(key.into(), value);
            result.sources.insert(key.into(), format!("{path}/edid: {source} (libdisplay-info)"));
        };
        if let Some(warnings) = &decoded.warnings {
            let value = if warnings.len() > 16384 { failure(libc::EOVERFLOW) } else { available(warnings.trim()) };
            put("warnings", value, "parser conformance diagnostics; decoded values remain reported information");
        }
        put("manufacturer", owned_string(decoded.make), "reported manufacturer, resolved by the library's PNP database");
        put("model", owned_string(decoded.model), "product name, or product code if no name was supplied");
        put("serial", owned_string(decoded.serial), "serial text, or numeric serial if available");
        let base = decode_base_block(bytes);
        put("manufacturer_code", available(base.manufacturer_code), "PNP manufacturer code");
        put("product", available(format!("0x{:04x}", base.product)), "reported product code");
        put(
            "size",
            if base.width_cm > 0 && base.height_cm > 0 { available(format!("{} {}", base.width_cm, base.height_cm)) } else { Attribute::default() },
            "reported image dimensions in cm; unspecified/aspect-ratio encodings are not physical dimensions",
        );
        put("digital", available(if base.digital { "1" } else { "0" }), "base-block video input type");
        if base.digital {
            put(
                "depth",
                base.depth.map_or_else(Attribute::default, |d| available(d.to_string())),
                "declared bits per color component, not active link or framebuffer depth",
            );
            put("interface", base.interface.map_or_else(Attribute::default, available), "reported digital input interface");
        }
    }
}

/// Kernel mode names, duplicates combined, as one value per line.
fn mode_names(report: &Report, cancel: Option<&AtomicBool>) -> Attribute {
    if report.state.state != ReadState::Available {
        return report.state.clone();
    }
    if report.bytes.is_empty() {
        return Attribute::default();
    }
    let lines: Vec<&[u8]> = report.bytes.split(|b| *b == b'\n').collect();
    if lines.len() > 2049 {
        return failure(libc::EOVERFLOW);
    }
    let mut names: Vec<String> = Vec::new();
    let mut seen: HashSet<&[u8]> = HashSet::new();
    let mut count = 0;
    for line in lines {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return failure(libc::ECANCELED);
        }
        if line.is_empty() {
            continue;
        }
        count += 1;
        if count > 2048 || line.len() > 128 {
            return failure(libc::EOVERFLOW);
        }
        if line.iter().any(|c| !(32..=126).contains(c)) {
            return failure(libc::EILSEQ);
        }
        if seen.insert(line) {
            names.push(String::from_utf8_lossy(line).into_owned());
        }
    }
    if names.is_empty() {
        Attribute::default()
    } else {
        available(names.join("\n"))
    }
}

/// Explicit Properties snapshots only. `directory` is the selected connector directory.
pub fn collect_monitor_properties(directory: &Dir, path: &str, cancel: Option<&AtomicBool>) -> MonitorProperties {
    let mut result = MonitorProperties::default();
    let status = read_report(directory, "status", 64, cancel);
    let trimmed = |r: &Report| String::from_utf8_lossy(&r.bytes).trim().to_string();
    if status.state.state == ReadState::Available && trimmed(&status) == "disconnected" {
        return result;
    }
    result.applicable = true;
    result.sources.insert(
        "descriptor".into(),
        format!(
            "{path}/edid: cached kernel descriptor; header, length and every block checksum checked. EDID 1.0–1.4 identity/input fields are decoded with optional libdisplay-info. Overrides, adapters or KVMs may supply this report."
        ),
    );
    result.sources.insert(
        "modes".into(),
        format!(
            "{path}/modes: cached kernel mode names, with duplicate names combined. These may include driver additions or restrictions; names do not distinguish refresh rates. Not a guaranteed capability list or active configuration."
        ),
    );
    if status.state.state != ReadState::Available {
        result.descriptor = status.state.clone();
        result.sources.insert("descriptor".into(), format!("{path}/status: connection status could not be read"));
        result.values.insert("modes".into(), status.state);
        return result;
    }
    if !matches!(trimmed(&status).as_str(), "connected" | "unknown") {
        result.descriptor = failure(libc::EINVAL);
        result.sources.insert("descriptor".into(), format!("{path}/status: malformed connection status"));
        result.values.insert("modes".into(), result.descriptor.clone());
        return result;
    }
    let edid = read_report(directory, "edid", 32768, cancel);
    let modes = read_report(directory, "modes", 65536, cancel);
    // A monitor may be replaced without the connector directory changing identity.
    let after_edid = read_report(directory, "edid", 32768, cancel);
    let after_status = read_report(directory, "status", 64, cancel);
    if after_status.state.state != ReadState::Available || (after_edid.state.state != ReadState::Available && !same_report(&edid, &after_edid)) {
        result.descriptor = if after_status.state.state != ReadState::Available { after_status.state } else { after_edid.state };
        result.values.insert("modes".into(), result.descriptor.clone());
        return result;
    }
    if !same_report(&status, &after_status) || !same_report(&edid, &after_edid) {
        result.descriptor = failure(libc::EAGAIN);
        result.values.insert("modes".into(), result.descriptor.clone());
        return result;
    }
    if edid.state.state == ReadState::Available {
        decode_descriptor(&mut result, &edid.bytes, path);
    } else {
        result.descriptor = edid.state.clone();
    }
    result.values.insert("modes".into(), mode_names(&modes, cancel));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A valid 128-byte EDID 1.4 base block: Dell-coded, DisplayPort, 8 bpc, 52x29 cm, with a
    /// product-name and a serial-text descriptor.
    pub fn sample_edid() -> Vec<u8> {
        let mut b = vec![0u8; 128];
        b[..8].copy_from_slice(&[0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]);
        b[8..10].copy_from_slice(&[0x10, 0xac]); // "DEL"
        b[10..12].copy_from_slice(&[0xb1, 0xa0]); // product 0xa0b1
        b[12..16].copy_from_slice(&[0x01, 0x02, 0x03, 0x04]);
        b[16] = 10;
        b[17] = 30;
        b[18] = 1;
        b[19] = 4;
        b[20] = 0xa5; // digital, 8 bits, DisplayPort
        b[21] = 52;
        b[22] = 29;
        for slot in 0..4 {
            let at = 54 + slot * 18;
            let (tag, text): (u8, &[u8]) = match slot {
                0 => (0xfc, b"Test Monitor\n"),
                1 => (0xff, b"SER12345\n"),
                _ => (0x10, b""),
            };
            b[at..at + 4].copy_from_slice(&[0, 0, 0, tag]);
            if slot < 2 {
                b[at + 5..at + 5 + text.len()].copy_from_slice(text);
                for fill in &mut b[at + 5 + text.len()..at + 18] {
                    *fill = b' ';
                }
            }
        }
        let sum: u32 = b[..127].iter().map(|x| *x as u32).sum();
        b[127] = ((256 - (sum & 255)) & 255) as u8;
        b
    }

    fn connector(status: &str, edid: Option<&[u8]>, modes: &str) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("status"), status).unwrap();
        if let Some(edid) = edid {
            fs::write(tmp.path().join("edid"), edid).unwrap();
        }
        fs::write(tmp.path().join("modes"), modes).unwrap();
        tmp
    }

    fn collect(tmp: &tempfile::TempDir) -> MonitorProperties {
        collect_monitor_properties(&Dir::open(tmp.path()).unwrap(), "/sys/devices/x/card0-DP-1", None)
    }

    #[test]
    fn base_block_fields_are_decoded_without_a_library() {
        let base = decode_base_block(&sample_edid());
        assert_eq!(base.manufacturer_code, "DEL");
        assert_eq!((base.product, base.width_cm, base.height_cm), (0xa0b1, 52, 29));
        assert!(base.digital);
        assert_eq!((base.depth, base.interface), (Some(8), Some("DisplayPort")));
        let mut old = sample_edid();
        old[19] = 3;
        let base = decode_base_block(&old);
        assert_eq!((base.depth, base.interface), (None, None), "depth and interface exist from EDID 1.4");
    }

    #[test]
    fn disconnected_connectors_are_not_applicable() {
        let tmp = connector("disconnected\n", None, "");
        assert!(!collect(&tmp).applicable);
    }

    #[test]
    fn edid_is_framed_checksummed_and_decoded() {
        let good = connector("connected\n", Some(&sample_edid()), "1920x1080\n1920x1080\n1280x720\n");
        let found = collect(&good);
        assert!(found.applicable);
        assert_eq!(found.values["version"].value, "1.4");
        assert_eq!(found.values["modes"].value, "1920x1080\n1280x720", "duplicate names combined");
        match found.descriptor.state {
            ReadState::Available => {
                assert_eq!(found.values["manufacturer_code"].value, "DEL");
                assert_eq!(found.values["product"].value, "0xa0b1");
                assert_eq!(found.values["size"].value, "52 29");
                assert_eq!((found.values["digital"].value.as_str(), found.values["depth"].value.as_str()), ("1", "8"));
                assert_eq!(found.values["interface"].value, "DisplayPort");
                assert_eq!(found.values["model"].value, "Test Monitor");
                assert_eq!(found.values["serial"].value, "SER12345");
                assert!(found.values["manufacturer"].value.contains("Dell"), "{:?}", found.values["manufacturer"]);
            }
            other => assert_eq!(other, ReadState::Unsupported, "libdisplay-info absent: decoding is unsupported, not wrong"),
        }

        let mut bad_sum = sample_edid();
        bad_sum[127] ^= 1;
        assert_eq!(collect(&connector("connected\n", Some(&bad_sum), "")).descriptor.error, libc::EPROTO);
        let mut bad_header = sample_edid();
        bad_header[0] = 1;
        assert_eq!(collect(&connector("connected\n", Some(&bad_header), "")).descriptor.error, libc::EPROTO);
        let mut claims_extension = sample_edid();
        claims_extension[126] = 1;
        assert_eq!(collect(&connector("connected\n", Some(&claims_extension), "")).descriptor.error, libc::EPROTO, "length must match the block count");
        let mut v2 = sample_edid();
        v2[18] = 2;
        let sum: u32 = v2[..127].iter().map(|x| *x as u32).sum();
        v2[127] = ((256 - (sum & 255)) & 255) as u8;
        let unsupported = collect(&connector("connected\n", Some(&v2), ""));
        assert_eq!((unsupported.descriptor.state, unsupported.descriptor.value.as_str()), (ReadState::Unsupported, "2.4"));
        assert_eq!(collect(&connector("connected\n", Some(&[]), "")).descriptor.state, ReadState::Unavailable, "empty report");
    }

    #[test]
    fn status_and_mode_validation() {
        assert_eq!(collect(&connector("weird\n", None, "")).descriptor.error, libc::EINVAL);
        let unknown = collect(&connector("unknown\n", Some(&sample_edid()), "640x480\n"));
        assert!(unknown.applicable && unknown.values["modes"].value == "640x480");
        assert_eq!(collect(&connector("connected\n", None, "bad\u{7}mode\n")).values["modes"].error, libc::EILSEQ);
        let many = (0..2100).map(|i| format!("{i}x1\n")).collect::<String>();
        assert_eq!(collect(&connector("connected\n", None, &many)).values["modes"].error, libc::EOVERFLOW);
        assert_eq!(collect(&connector("connected\n", None, &"x".repeat(129))).values["modes"].error, libc::EOVERFLOW);
        let no_edid = collect(&connector("connected\n", None, "1x1\n"));
        assert_eq!(no_edid.descriptor.state, ReadState::Unavailable, "a connector without an edid file");
        let cancelled = AtomicBool::new(true);
        let tmp = connector("connected\n", None, "1x1\n");
        let stopped = collect_monitor_properties(&Dir::open(tmp.path()).unwrap(), "p", Some(&cancelled));
        assert_eq!(stopped.descriptor.error, libc::ECANCELED);
    }

    #[test]
    fn real_connectors_collect_without_error() {
        let Ok(entries) = fs::read_dir("/sys/class/drm") else { return };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.contains('-') || !name.starts_with("card") {
                continue;
            }
            let path = fs::canonicalize(entry.path()).unwrap();
            let found = collect_monitor_properties(&Dir::open(&path).unwrap(), &path.to_string_lossy(), None);
            if found.applicable && found.descriptor.state == ReadState::Available {
                assert!(found.values["manufacturer_code"].value.len() == 3, "{name}");
            }
        }
    }
}
