//! Camera capability reading, ported from `src/devices/cameraproperties.cpp`.
//!
//! V4L2 capability, format, frame-size and frame-interval enumeration only: the node is opened
//! read-only and non-blocking, no format is ever set and no stream is started. The queries run
//! in a short-lived helper process (`qldm-camera-helper`) so a driver that hangs in an ioctl can
//! be killed after the 2 s budget instead of wedging the application. The helper dies with its
//! parent, and a helper that cannot be reaped blocks further helpers rather than piling up.
use crate::dirfd::{io_errno, Dir};
use crate::model::{Attribute, ReadState};
use crate::v4l2_sys::*;
use serde_json::{json, Map, Value};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const REPORT_LIMIT: usize = 1024 * 1024;
const MODE_LIMIT: usize = 2048;
const ENUMERATION_LIMIT: u32 = 128;
const QUERY_LIMIT: u32 = 4096;
const BUDGET: Duration = Duration::from_millis(2000);

const BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
const BUF_TYPE_VIDEO_CAPTURE_MPLANE: u32 = 9;
const FRMSIZE_DISCRETE: u32 = 1;
const FRMSIZE_CONTINUOUS: u32 = 2;
const FRMSIZE_STEPWISE: u32 = 3;
const FRMIVAL_DISCRETE: u32 = 1;
const FRMIVAL_CONTINUOUS: u32 = 2;
const FRMIVAL_STEPWISE: u32 = 3;

/// `_IOC(dir, 'V', nr, size)` for the generic ioctl encoding used on Linux.
const fn ioc(direction: u64, nr: u64, size: usize) -> libc::c_ulong {
    ((direction << 30) | ((size as u64) << 16) | ((b'V' as u64) << 8) | nr) as libc::c_ulong
}
const IOC_READ: u64 = 2;
const IOC_RW: u64 = 3;
const VIDIOC_QUERYCAP: libc::c_ulong = ioc(IOC_READ, 0, std::mem::size_of::<v4l2_capability>());
const VIDIOC_ENUM_FMT: libc::c_ulong = ioc(IOC_RW, 2, std::mem::size_of::<v4l2_fmtdesc>());
const VIDIOC_ENUM_FRAMESIZES: libc::c_ulong = ioc(IOC_RW, 74, std::mem::size_of::<v4l2_frmsizeenum>());
const VIDIOC_ENUM_FRAMEINTERVALS: libc::c_ulong = ioc(IOC_RW, 75, std::mem::size_of::<v4l2_frmivalenum>());

#[derive(Clone, Debug, Default)]
pub struct CameraProperties {
    pub applicable: bool,
    pub state: Attribute,
    pub node: String,
    /// Owned, bounded helper report: each mode retains its queue/format/size/interval relation.
    pub report: Value,
}

fn failure(error: i32) -> Attribute {
    let state = match error {
        libc::EACCES | libc::EPERM => ReadState::PermissionDenied,
        libc::ENODEV | libc::ENOENT | libc::ENXIO => ReadState::Removed,
        libc::ENOTTY | libc::EOPNOTSUPP | libc::EINVAL => ReadState::Unsupported,
        _ => ReadState::Error,
    };
    Attribute { state, value: String::new(), error }
}

fn failed(error: i32) -> Value {
    json!({ "error": error })
}

/// A fixed-size C string field that must be NUL-terminated valid UTF-8.
fn text(field: &[u8]) -> Option<String> {
    let length = field.iter().position(|b| *b == 0)?;
    String::from_utf8(field[..length].to_vec()).ok()
}

fn fraction(value: &v4l2_fract) -> Value {
    json!([value.numerator, value.denominator])
}

fn valid_fraction(value: &v4l2_fract) -> bool {
    value.numerator != 0 && value.denominator != 0
}

/// `a <= b` as fractions, compared exactly.
fn ordered(a: &v4l2_fract, b: &v4l2_fract) -> bool {
    a.numerator as u64 * b.denominator as u64 <= b.numerator as u64 * a.denominator as u64
}

struct Query {
    fd: i32,
    started: Instant,
    calls: u32,
    error: i32,
}

impl Query {
    fn new(fd: i32) -> Self {
        Self { fd, started: Instant::now(), calls: 0, error: 0 }
    }

    /// Plain ioctl passthrough (no libv4l converters, emulated formats or capture setup).
    fn get<T>(&mut self, request: libc::c_ulong, value: &mut T) -> bool {
        self.calls += 1;
        if self.calls > QUERY_LIMIT {
            self.error = libc::EOVERFLOW;
            return false;
        }
        loop {
            if self.started.elapsed() >= BUDGET {
                self.error = libc::ETIMEDOUT;
                return false;
            }
            // SAFETY: `value` is a valid, exclusively borrowed struct of the size the request
            // number encodes; the kernel reads and writes only within it.
            if unsafe { libc::ioctl(self.fd, request as _, value as *mut T) } == 0 {
                self.error = 0;
                return true;
            }
            self.error = io_errno(&std::io::Error::last_os_error());
            if self.error != libc::EINTR {
                return false;
            }
        }
    }
}

fn zeroed<T>() -> T {
    // SAFETY: only used for the plain-old-data V4L2 structs, for which all-zero is valid.
    unsafe { std::mem::zeroed() }
}

fn intervals(query: &mut Query, format: u32, width: u32, height: u32) -> Value {
    let mut discrete = Vec::new();
    for index in 0..=ENUMERATION_LIMIT {
        let mut value: v4l2_frmivalenum = zeroed();
        value.index = index;
        value.pixel_format = format;
        value.width = width;
        value.height = height;
        if !query.get(VIDIOC_ENUM_FRAMEINTERVALS, &mut value) {
            if query.error == libc::EINVAL && index > 0 {
                return json!({ "kind": "discrete", "values": discrete });
            }
            return failed(query.error);
        }
        if value.index != index || value.pixel_format != format || value.width != width || value.height != height {
            return failed(libc::EPROTO);
        }
        if index == ENUMERATION_LIMIT {
            return failed(libc::EOVERFLOW);
        }
        // SAFETY: the active union member is selected by `type_`, as the kernel documents.
        unsafe {
            match value.type_ {
                FRMIVAL_DISCRETE => {
                    if !valid_fraction(&value.__bindgen_anon_1.discrete) {
                        return failed(libc::EPROTO);
                    }
                    discrete.push(fraction(&value.__bindgen_anon_1.discrete));
                }
                FRMIVAL_STEPWISE | FRMIVAL_CONTINUOUS if index == 0 => {
                    let range = value.__bindgen_anon_1.stepwise;
                    if !valid_fraction(&range.min)
                        || !valid_fraction(&range.max)
                        || !ordered(&range.min, &range.max)
                        || (value.type_ == FRMIVAL_STEPWISE && !valid_fraction(&range.step))
                    {
                        return failed(libc::EPROTO);
                    }
                    return json!({
                        "kind": if value.type_ == FRMIVAL_CONTINUOUS { "continuous" } else { "stepwise" },
                        "min": fraction(&range.min), "max": fraction(&range.max), "step": fraction(&range.step),
                    });
                }
                _ => return failed(libc::EPROTO),
            }
        }
    }
    failed(libc::EOVERFLOW)
}

/// Opens the sysfs device directory and returns `dev:ino`, for the helper's identity check.
fn directory_identity(path: &Path) -> std::io::Result<String> {
    let info = Dir::open(path)?.metadata()?;
    Ok(format!("{}:{}", info.dev(), info.ino()))
}

/// Everything the helper reads, as a JSON object. `identity` is the parent's `dev:ino` of the
/// sysfs directory; any mismatch (a replaced device) is `ENODEV`.
fn query_device(path: &str, node: &str, identity: &str) -> Value {
    let directory = match Dir::open(path) {
        Ok(d) => d,
        Err(e) => return failed(io_errno(&e)),
    };
    let selected = match directory.metadata() {
        Ok(m) => m,
        Err(e) => return failed(io_errno(&e)),
    };
    if format!("{}:{}", selected.dev(), selected.ino()) != identity {
        return failed(libc::ENODEV);
    }
    let dev = directory.read_text("dev");
    if dev.state != ReadState::Available {
        return failed(dev.error);
    }
    let numbers: Vec<&str> = dev.value.split(':').collect();
    let (Some(major), Some(minor)) = (numbers.first().and_then(|n| n.parse::<u32>().ok()), numbers.get(1).and_then(|n| n.parse::<u32>().ok())) else {
        return failed(libc::EPROTO);
    };
    if numbers.len() != 2 {
        return failed(libc::EPROTO);
    }
    let device = match std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOFOLLOW).open(node) {
        Ok(f) => f,
        Err(e) => return failed(io_errno(&e)),
    };
    let opened = match device.metadata() {
        Ok(m) => m,
        Err(e) => return failed(io_errno(&e)),
    };
    let (rmajor, rminor) = (libc::major(opened.rdev()), libc::minor(opened.rdev()));
    if !opened.file_type().is_char_device() || rmajor != major || rminor != minor {
        return failed(libc::ENODEV);
    }
    let binding = format!("/sys/dev/char/{major}:{minor}");
    let same_device = || {
        let Ok(current) = std::fs::metadata(path) else { return false };
        let Ok(now) = std::fs::symlink_metadata(node) else { return false };
        current.dev() == selected.dev()
            && current.ino() == selected.ino()
            && now.file_type().is_char_device()
            && now.rdev() == opened.rdev()
            && now.ino() == opened.ino()
            && now.dev() == opened.dev()
            && std::fs::canonicalize(&binding).is_ok_and(|p| p == Path::new(path))
    };
    if !same_device() {
        return failed(libc::ENODEV);
    }
    let mut query = Query::new(device.as_raw_fd());
    let mut capability: v4l2_capability = zeroed();
    if !query.get(VIDIOC_QUERYCAP, &mut capability) {
        return failed(query.error);
    }
    let (Some(card), Some(driver), Some(bus)) = (text(&capability.card), text(&capability.driver), text(&capability.bus_info)) else {
        return failed(libc::EPROTO);
    };
    let caps = if capability.capabilities & V4L2_CAP_DEVICE_CAPS != 0 { capability.device_caps } else { capability.capabilities };
    if caps & (V4L2_CAP_VIDEO_CAPTURE | V4L2_CAP_VIDEO_CAPTURE_MPLANE) == 0 || caps & (V4L2_CAP_VIDEO_M2M | V4L2_CAP_VIDEO_M2M_MPLANE) != 0 {
        return json!({ "applicable": false });
    }
    let mut result = Map::new();
    result.insert("applicable".into(), json!(true));
    result.insert("card".into(), json!(card));
    result.insert("driver".into(), json!(driver));
    result.insert("bus".into(), json!(bus));
    let mut modes: Vec<Value> = Vec::new();
    let mut collection_error = 0;
    'queues: for (queue, capability_bit) in [(BUF_TYPE_VIDEO_CAPTURE, V4L2_CAP_VIDEO_CAPTURE), (BUF_TYPE_VIDEO_CAPTURE_MPLANE, V4L2_CAP_VIDEO_CAPTURE_MPLANE)] {
        if caps & capability_bit == 0 {
            continue;
        }
        let mut formats: Vec<v4l2_fmtdesc> = Vec::new();
        for index in 0..=ENUMERATION_LIMIT {
            let mut format: v4l2_fmtdesc = zeroed();
            format.type_ = queue;
            format.index = index;
            if !query.get(VIDIOC_ENUM_FMT, &mut format) {
                if query.error != libc::EINVAL || index == 0 {
                    collection_error = query.error;
                }
                break;
            }
            if text(&format.description).is_none() {
                collection_error = libc::EPROTO;
                break;
            }
            if format.type_ != queue || format.index != index || format.pixelformat == 0 {
                collection_error = libc::EPROTO;
                break;
            }
            if index == ENUMERATION_LIMIT {
                collection_error = libc::EOVERFLOW;
                break;
            }
            formats.push(format);
        }
        for format in &formats {
            let mut base = Map::new();
            base.insert("queue".into(), json!(if queue == BUF_TYPE_VIDEO_CAPTURE { "single" } else { "multi" }));
            base.insert("format".into(), json!(format.pixelformat));
            base.insert("description".into(), json!(text(&format.description).unwrap_or_default()));
            base.insert("flags".into(), json!(format.flags));
            let mut sizes: Vec<Map<String, Value>> = Vec::new();
            for index in 0..=ENUMERATION_LIMIT {
                let mut size: v4l2_frmsizeenum = zeroed();
                size.index = index;
                size.pixel_format = format.pixelformat;
                if !query.get(VIDIOC_ENUM_FRAMESIZES, &mut size) {
                    if query.error != libc::EINVAL || index == 0 {
                        let mut row = base.clone();
                        row.insert("size_error".into(), json!(query.error));
                        sizes.push(row);
                    }
                    break;
                }
                if index == ENUMERATION_LIMIT {
                    collection_error = libc::EOVERFLOW;
                    break;
                }
                if size.index != index || size.pixel_format != format.pixelformat {
                    collection_error = libc::EPROTO;
                    break;
                }
                let mut row = base.clone();
                // SAFETY: the active union member is selected by `type_`.
                unsafe {
                    match size.type_ {
                        FRMSIZE_DISCRETE => {
                            let d = size.__bindgen_anon_1.discrete;
                            if d.width == 0 || d.height == 0 {
                                collection_error = libc::EPROTO;
                                break;
                            }
                            row.insert("size_kind".into(), json!("discrete"));
                            row.insert("width".into(), json!(d.width));
                            row.insert("height".into(), json!(d.height));
                        }
                        FRMSIZE_STEPWISE | FRMSIZE_CONTINUOUS if index == 0 => {
                            let r = size.__bindgen_anon_1.stepwise;
                            let bad = r.min_width == 0
                                || r.min_height == 0
                                || r.min_width > r.max_width
                                || r.min_height > r.max_height
                                || r.step_width == 0
                                || r.step_height == 0
                                || (size.type_ == FRMSIZE_CONTINUOUS && (r.step_width != 1 || r.step_height != 1));
                            if bad {
                                collection_error = libc::EPROTO;
                                break;
                            }
                            row.insert("size_kind".into(), json!(if size.type_ == FRMSIZE_CONTINUOUS { "continuous" } else { "stepwise" }));
                            for (key, value) in [
                                ("width", r.min_width),
                                ("height", r.min_height),
                                ("max_width", r.max_width),
                                ("max_height", r.max_height),
                                ("step_width", r.step_width),
                                ("step_height", r.step_height),
                            ] {
                                row.insert(key.into(), json!(value));
                            }
                        }
                        _ => {
                            collection_error = libc::EPROTO;
                            break;
                        }
                    }
                }
                let discrete = size.type_ == FRMSIZE_DISCRETE;
                sizes.push(row);
                if !discrete {
                    break;
                }
            }
            for mut row in sizes {
                // Ranges stay ranges: never imply one rate list applies to every size.
                if row.get("size_kind") == Some(&json!("discrete")) {
                    let width = row["width"].as_u64().unwrap_or(0) as u32;
                    let height = row["height"].as_u64().unwrap_or(0) as u32;
                    row.insert("intervals".into(), intervals(&mut query, format.pixelformat, width, height));
                }
                if modes.len() == MODE_LIMIT {
                    collection_error = libc::EOVERFLOW;
                    break;
                }
                modes.push(Value::Object(row));
            }
            if query.error == libc::ETIMEDOUT || query.error == libc::EOVERFLOW {
                collection_error = query.error;
            }
            if modes.len() == MODE_LIMIT {
                collection_error = libc::EOVERFLOW;
            }
            if query.error == libc::ETIMEDOUT || query.error == libc::EOVERFLOW || modes.len() == MODE_LIMIT {
                break 'queues;
            }
        }
    }
    if !same_device() {
        return failed(libc::ENODEV);
    }
    result.insert("modes".into(), Value::Array(modes));
    result.insert("error".into(), json!(collection_error));
    Value::Object(result)
}

/// Entry point of the helper process: `qldm-camera-helper --camera-query <sysfs path> <node>
/// <dev:ino of the sysfs directory>`. Prints one compact JSON object on stdout.
pub fn run_camera_query_helper(args: &[String]) -> i32 {
    // If the parent dies, do not leave an ordinary helper running independently.
    // SAFETY: prctl with PR_SET_PDEATHSIG only changes this process's own signal setting.
    let armed = unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } == 0;
    // SAFETY: getppid has no preconditions.
    if !armed || unsafe { libc::getppid() } == 1 {
        return 1;
    }
    let [flag, path, node, identity] = args else { return 2 };
    if flag != "--camera-query" {
        return 2;
    }
    let mut bytes = serde_json::to_vec(&query_device(path, node, identity)).unwrap_or_default();
    if bytes.len() > REPORT_LIMIT {
        bytes = serde_json::to_vec(&failed(libc::EOVERFLOW)).unwrap_or_default();
    }
    use std::io::Write;
    match std::io::stdout().lock().write_all(&bytes) {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

/// A helper that could not be reaped after being killed; no new helper starts until it is.
static PENDING: Mutex<Option<Child>> = Mutex::new(None);

/// Default helper location: next to the running executable.
pub fn default_helper_path() -> std::path::PathBuf {
    std::env::current_exe().ok().and_then(|exe| exe.parent().map(|dir| dir.join("qldm-camera-helper"))).unwrap_or_else(|| "qldm-camera-helper".into())
}

/// Reads a camera's V4L2 capabilities through the helper at `helper`. `directory` is the open
/// sysfs device directory and `node` its `/dev/video*` node (empty when udev reported none).
pub fn collect_camera_properties(directory: &Dir, sysfs_path: &str, node: &str, helper: &Path, cancel: Option<&AtomicBool>) -> CameraProperties {
    let mut result = CameraProperties { applicable: true, node: node.to_string(), ..Default::default() };
    if node.is_empty() {
        return result;
    }
    if !node.starts_with("/dev/") || node.len() > 4096 || sysfs_path.len() > 4096 || node.contains('\0') {
        result.state = failure(libc::EPROTO);
        return result;
    }
    // One properties worker serializes requests. Never accumulate stuck helpers.
    {
        let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(child) = pending.as_mut() {
            match child.try_wait() {
                Ok(None) => {
                    result.state = failure(libc::EAGAIN);
                    return result;
                }
                _ => *pending = None,
            }
        }
    }
    let identity = match directory.metadata() {
        Ok(info) => format!("{}:{}", info.dev(), info.ino()),
        Err(e) => {
            result.state = failure(io_errno(&e));
            return result;
        }
    };
    let mut child = match Command::new(helper)
        .args(["--camera-query", sysfs_path, node, &identity])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            result.state = failure(io_errno(&e));
            return result;
        }
    };
    let mut stdout = child.stdout.take().expect("piped stdout");
    // SAFETY: switching the read end of our own pipe to non-blocking mode.
    unsafe {
        let flags = libc::fcntl(stdout.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(stdout.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    let started = Instant::now();
    let mut bytes: Vec<u8> = Vec::new();
    let (mut eof, mut exit) = (false, None);
    let mut error = 0;
    while !eof || exit.is_none() {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            error = libc::ECANCELED;
            break;
        }
        if started.elapsed() >= BUDGET {
            error = libc::ETIMEDOUT;
            break;
        }
        let mut buffer = [0u8; 4096];
        let mut count = -1isize;
        match stdout.read(&mut buffer) {
            Ok(0) => eof = true,
            Ok(n) => {
                count = n as isize;
                bytes.extend_from_slice(&buffer[..n]);
                if bytes.len() > REPORT_LIMIT {
                    error = libc::EOVERFLOW;
                    break;
                }
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted) => {}
            Err(e) => {
                error = io_errno(&e);
                break;
            }
        }
        if exit.is_none() {
            match child.try_wait() {
                Ok(Some(status)) => exit = Some(status),
                Ok(None) => {}
                Err(e) => {
                    error = io_errno(&e);
                    break;
                }
            }
        }
        if !eof && count <= 0 {
            let mut wait = libc::pollfd { fd: stdout.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: `wait` is a valid single-element array for the call's duration.
            unsafe { libc::poll(&mut wait, 1, 25) };
        } else if eof && exit.is_none() {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    if error != 0 {
        if exit.is_none() {
            let _ = child.kill();
            if !matches!(child.try_wait(), Ok(Some(_))) {
                *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
            }
        }
        result.state = failure(error);
        return result;
    }
    if !exit.is_some_and(|s| s.success() && s.signal().is_none()) {
        result.state = failure(libc::EPROTO);
        return result;
    }
    let Some(report) = serde_json::from_slice::<Value>(&bytes).ok().filter(Value::is_object) else {
        result.state = failure(libc::EPROTO);
        return result;
    };
    if let Some(applicable) = report.get("applicable").and_then(Value::as_bool) {
        result.applicable = applicable;
    }
    let report_error = report.get("error").and_then(Value::as_i64).unwrap_or(0) as i32;
    result.state = if report_error != 0 { failure(report_error) } else { Attribute { state: ReadState::Available, ..Default::default() } };
    result.report = report;
    result
}

/// Sysfs directory identity the helper expects, for callers that hold only a path.
pub fn identity_of(path: &Path) -> std::io::Result<String> {
    directory_identity(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_the_kernel_headers() {
        // Printed with a C program against /usr/include/linux/videodev2.h.
        assert_eq!(VIDIOC_QUERYCAP, 0x8068_5600);
        assert_eq!(VIDIOC_ENUM_FMT, 0xc040_5602);
        assert_eq!(VIDIOC_ENUM_FRAMESIZES, 0xc02c_564a);
        assert_eq!(VIDIOC_ENUM_FRAMEINTERVALS, 0xc034_564b);
        assert_eq!(std::mem::size_of::<v4l2_capability>(), 104);
        assert_eq!(std::mem::size_of::<v4l2_fmtdesc>(), 64);
        assert_eq!(std::mem::size_of::<v4l2_frmsizeenum>(), 44);
        assert_eq!(std::mem::size_of::<v4l2_frmivalenum>(), 52);
    }

    #[test]
    fn text_fields_must_be_terminated_utf8() {
        assert_eq!(text(b"Integrated Camera\0\0\0").as_deref(), Some("Integrated Camera"));
        assert_eq!(text(b"no terminator"), None);
        assert_eq!(text(&[0xff, 0xfe, 0]), None);
    }

    #[test]
    fn fractions_compare_exactly() {
        let f = |n, d| v4l2_fract { numerator: n, denominator: d };
        assert!(ordered(&f(1, 30), &f(1, 15)));
        assert!(ordered(&f(1, 30), &f(1, 30)));
        assert!(!ordered(&f(1, 10), &f(1, 30)));
        assert!(valid_fraction(&f(1, 30)) && !valid_fraction(&f(0, 30)) && !valid_fraction(&f(1, 0)));
        assert_eq!(fraction(&f(1, 30)), json!([1, 30]));
    }

    #[test]
    fn helper_rejects_mismatched_identities_and_bad_arguments() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().to_string_lossy().into_owned();
        assert_eq!(query_device(&path, "/dev/null", "1:2")["error"], libc::ENODEV);
        assert_eq!(query_device("/nonexistent/x", "/dev/null", "1:2")["error"], libc::ENOENT);
        let identity = directory_identity(tmp.path()).unwrap();
        // No readable "dev" file in the fixture: reported as the read failure.
        assert_eq!(query_device(&path, "/dev/null", &identity)["error"], libc::ENOENT);
        std::fs::write(tmp.path().join("dev"), "garbage\n").unwrap();
        assert_eq!(query_device(&path, "/dev/null", &identity)["error"], libc::EPROTO);
        std::fs::write(tmp.path().join("dev"), "1:3\n").unwrap();
        // /dev/null is 1:3 but is not a V4L2 node: the QUERYCAP ioctl is not supported.
        assert_eq!(query_device(&path, "/dev/null", &identity)["error"], libc::ENODEV, "sysfs binding differs");
        assert_eq!(run_camera_query_helper(&["--bogus".into(), path.clone(), "n".into(), identity]), 2);
    }

    #[test]
    fn empty_and_invalid_nodes_are_handled_before_spawning() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = Dir::open(tmp.path()).unwrap();
        let helper = Path::new("/nonexistent/helper");
        let path = tmp.path().to_string_lossy().into_owned();
        let none = collect_camera_properties(&dir, &path, "", helper, None);
        assert!(none.applicable && none.state.state == ReadState::Unavailable);
        let bad = collect_camera_properties(&dir, &path, "relative/video0", helper, None);
        assert_eq!(bad.state.error, libc::EPROTO);
        let missing = collect_camera_properties(&dir, &path, "/dev/video0", helper, None);
        assert_eq!(missing.state.state, ReadState::Removed, "an absent helper executable is ENOENT");
    }
}
