//! Storage properties, ported from `src/devices/storageproperties.cpp`.
//!
//! Native metadata (sysfs, udev, procfs) is always collected; cached UDisks2 metadata over the
//! system bus is added when the `udisks2` feature is on and the daemon is already running. The
//! service is only ever asked for its cache: no activation, no authorization prompt, no SMART
//! update and no self-test, so a drive is never woken. Relationships are explicit edges, never
//! capacity totals.
use crate::dirfd::{failure, io_errno};
use crate::model::{Attribute, Device, ReadState};
use crate::sysfs::bounded_directory_names;
#[cfg(feature = "udisks2")]
use std::collections::HashMap;
use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

const RECORD_LIMIT: usize = 4096;
const RELATED_LIMIT: usize = 256;
const TEXT_LIMIT: usize = 4096;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Format {
    #[default]
    Text,
    Bytes,
    Boolean,
    Removable,
    Epoch,
    Kelvin,
    Percent,
    Hours,
    Seconds,
    Minutes,
    Count,
    AtaHealth,
    NvmeHealth,
    HealthQueryStatus,
}

#[derive(Clone, Debug, Default)]
pub struct StorageField {
    pub id: String,
    /// Static translatable caption, not device input.
    pub label: &'static str,
    pub value: Attribute,
    pub source: String,
    pub format: Format,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AtaSmartAttribute {
    pub id: u8,
    /// UDisks2's drive-specific identifier, not guessed from the ID.
    pub name: String,
    pub flags: u16,
    pub current: i32,
    pub worst: i32,
    pub threshold: i32,
    pub interpreted: i64,
    /// UDisks2 unit; zero means no interpreted value.
    pub unit: i32,
}

#[derive(Clone, Debug, Default)]
pub struct StorageEntity {
    /// Canonical sysfs path or UDisks object path, snapshot-local.
    pub id: String,
    pub fields: Vec<StorageField>,
    pub ata_attributes: Vec<AtaSmartAttribute>,
}

#[derive(Clone, Debug, Default)]
pub struct StorageLink {
    pub from: String,
    pub to: String,
    pub label: &'static str,
    pub source: String,
}

#[derive(Clone, Debug, Default)]
pub struct StorageProperties {
    pub applicable: bool,
    pub entities: Vec<StorageEntity>,
    pub links: Vec<StorageLink>,
    pub notes: Vec<StorageField>,
}

fn ok(text: impl Into<String>) -> Attribute {
    Attribute { state: ReadState::Available, value: text.into(), error: 0 }
}

fn storage_failure(error: i32) -> Attribute {
    // ENODATA counts as unavailable here too; identical to the shared mapping.
    failure(error)
}

fn text_file(path: &str, limit: usize) -> Attribute {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => return storage_failure(io_errno(&e)),
    };
    let mut bytes = Vec::new();
    if (&mut file).take(limit as u64 + 1).read_to_end(&mut bytes).is_err() {
        return storage_failure(libc::EIO);
    }
    if bytes.len() > limit {
        return storage_failure(libc::EOVERFLOW);
    }
    ok(String::from_utf8_lossy(&bytes).trim())
}

fn field(fields: &mut Vec<StorageField>, id: &str, label: &'static str, value: Attribute, source: impl Into<String>, format: Format) {
    fields.push(StorageField { id: id.into(), label, value, source: source.into(), format });
}

fn native(entity: &mut StorageEntity, key: &str, label: &'static str, format: Format) {
    let path = format!("{}/{key}", entity.id);
    let value = text_file(&path, TEXT_LIMIT);
    field(&mut entity.fields, key, label, value, path, format);
}

fn metadata(entity: &mut StorageEntity, raw: Option<&udev::Device>, key: &str, label: &'static str) {
    let value = raw.and_then(|d| d.property_value(key)).map_or_else(Attribute::default, |v| {
        if v.len() > TEXT_LIMIT {
            storage_failure(libc::EOVERFLOW)
        } else {
            ok(v.to_string_lossy().into_owned())
        }
    });
    let source = format!("udev: {key} on {}", entity.id);
    field(&mut entity.fields, key, label, value, source, Format::Text);
}

/// Decodes mountinfo escapes once: replacing `\134` first would double-decode literal escapes.
fn unescape_mount(value: &str) -> String {
    value.replace("\\040", " ").replace("\\011", "\t").replace("\\012", "\n").replace("\\134", "\\")
}

fn link(result: &mut StorageProperties, from: &str, to: &str, label: &'static str, source: impl Into<String>) {
    if from.is_empty() || to.is_empty() || from == to || to == "/" {
        return;
    }
    if result.links.iter().any(|e| e.from == from && e.to == to && e.label == label) {
        return;
    }
    result.links.push(StorageLink { from: from.into(), to: to.into(), label, source: source.into() });
}

fn canonical(path: &str) -> String {
    std::fs::canonicalize(path).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default()
}

fn exists(path: &str) -> bool {
    std::fs::symlink_metadata(path).is_ok() || Path::new(path).exists()
}

fn parent_canonical(path: &str) -> String {
    Path::new(path).parent().map(|p| canonical(&p.to_string_lossy())).unwrap_or_default()
}

fn file_name(path: &str) -> String {
    Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn inode_identity(path: &str) -> String {
    std::fs::symlink_metadata(path).map(|i| format!("{}:{}", i.dev(), i.ino())).unwrap_or_default()
}

/// Parses `/proc/self/mountinfo` into `major:minor` -> mount point descriptions.
pub fn parse_mountinfo(text: &str) -> BTreeMap<String, Vec<String>> {
    let mut mounts: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in text.split('\n') {
        let parts: Vec<&str> = line.split(' ').collect();
        let Some(separator) = parts.iter().position(|p| *p == "-") else { continue };
        if parts.len() < 6 || separator < 6 || parts.len() <= separator + 2 {
            continue;
        }
        mounts.entry(parts[2].to_string()).or_default().push(format!(
            "{} [{}; root={}]",
            unescape_mount(parts[4]),
            parts[separator + 1],
            unescape_mount(parts[3])
        ));
    }
    mounts
}

/// Collects storage metadata for a `block` device or an `nvme` controller; any other device
/// returns an inapplicable (empty) snapshot. `cached_service` is false for deep search, which
/// still searches native metadata and relations but skips service snapshots.
pub fn collect_storage_properties(device: &Device, cached_service: bool, cancel: Option<&AtomicBool>) -> StorageProperties {
    let mut result = StorageProperties { applicable: device.subsystem == "block" || device.subsystem == "nvme", ..Default::default() };
    if !result.applicable {
        return result;
    }
    let cancelled = || cancel.is_some_and(|c| c.load(Ordering::Relaxed));
    let directory = "/sys/class/block";
    let names = match bounded_directory_names(directory, RECORD_LIMIT as i64, &[], cancel) {
        Ok(names) => names,
        Err(error) => {
            field(&mut result.notes, "limit", "Storage inventory", storage_failure(error), directory, Format::Text);
            return result;
        }
    };
    let mut paths: BTreeMap<String, String> = BTreeMap::new();
    for name in &names {
        let path = canonical(&format!("{directory}/{name}"));
        if path.starts_with("/sys/devices/") {
            paths.insert(name.clone(), path);
        }
    }
    let all_paths: HashSet<&String> = paths.values().collect();
    let mut wanted: HashSet<String> = HashSet::new();
    if device.subsystem == "block" {
        wanted.insert(device.path.clone());
    } else {
        for path in paths.values().filter(|p| p.starts_with(&format!("{}/", device.path))) {
            wanted.insert(path.clone());
        }
    }
    let mut native_links: Vec<StorageLink> = Vec::new();
    for path in paths.values() {
        if cancelled() {
            return result;
        }
        if exists(&format!("{path}/partition")) {
            native_links.push(StorageLink {
                from: path.clone(),
                to: parent_canonical(path),
                label: "Partition of",
                source: format!("{path}/partition (kernel ancestry)"),
            });
        }
        for (dir, label) in [("slaves", "Backed by"), ("holders", "Used by")] {
            let links = format!("{path}/{dir}");
            let related = match bounded_directory_names(&links, RECORD_LIMIT as i64, &[], cancel) {
                Ok(names) => names,
                Err(libc::ENOENT) => Vec::new(),
                Err(error) => {
                    field(&mut result.notes, "limit", "Storage relationships", storage_failure(error), links, Format::Text);
                    return result;
                }
            };
            for name in related {
                let target = canonical(&format!("{links}/{name}"));
                if all_paths.contains(&target) {
                    native_links.push(StorageLink { from: path.clone(), to: target, label, source: format!("{links}/{name}") });
                }
                if native_links.len() > RECORD_LIMIT * 4 {
                    field(&mut result.notes, "limit", "Storage relationships", storage_failure(libc::EOVERFLOW), directory, Format::Text);
                    return result;
                }
            }
        }
    }
    for _ in 0..RELATED_LIMIT {
        let before = wanted.len();
        for edge in &native_links {
            if wanted.contains(&edge.from) || wanted.contains(&edge.to) {
                wanted.insert(edge.from.clone());
                wanted.insert(edge.to.clone());
            }
        }
        if wanted.len() > RELATED_LIMIT {
            field(&mut result.notes, "limit", "Related storage metadata", storage_failure(libc::EOVERFLOW), directory, Format::Text);
            return result;
        }
        if wanted.len() == before {
            break;
        }
    }
    let identities: BTreeMap<String, String> = wanted.iter().map(|p| (p.clone(), inode_identity(p))).collect();
    let mounts = text_file("/proc/self/mountinfo", 4 * 1024 * 1024);
    field(
        &mut result.notes,
        "mountinfo",
        "Mount metadata",
        if mounts.state == ReadState::Available { ok("application-namespace") } else { mounts.clone() },
        "/proc/self/mountinfo",
        Format::Text,
    );
    let mount_points = if mounts.state == ReadState::Available { parse_mountinfo(&mounts.value) } else { BTreeMap::new() };

    // Sorting is deterministic and each canonical block path appears once.
    let mut selected: Vec<String> = wanted.iter().cloned().collect();
    selected.sort();
    for path in &selected {
        if cancelled() {
            return result;
        }
        let mut entity = StorageEntity { id: path.clone(), ..Default::default() };
        let raw = udev::Device::from_syspath(Path::new(path)).ok();
        metadata(&mut entity, raw.as_ref(), "DEVNAME", "Device node");
        let number = text_file(&format!("{path}/dev"), TEXT_LIMIT);
        field(&mut entity.fields, "dev", "Major:minor", number.clone(), format!("{path}/dev"), Format::Text);
        let mut capacity = text_file(&format!("{path}/size"), TEXT_LIMIT);
        if capacity.state == ReadState::Available {
            capacity = match capacity.value.parse::<u64>().ok().and_then(|s| s.checked_mul(512)) {
                Some(bytes) => ok(bytes.to_string()),
                None => storage_failure(libc::EOVERFLOW),
            };
        }
        field(&mut entity.fields, "capacity", "Block capacity", capacity, format!("{path}/size (512-byte units)"), Format::Bytes);
        let partition = exists(&format!("{path}/partition"));
        let queue_owner = if partition { parent_canonical(path) } else { path.clone() };
        for (key, label) in [("logical_block_size", "Logical sector size"), ("physical_block_size", "Physical sector size")] {
            let source = format!("{queue_owner}/queue/{key}");
            field(&mut entity.fields, &format!("queue/{key}"), label, text_file(&source, TEXT_LIMIT), source, Format::Bytes);
        }
        native(&mut entity, "removable", "Kernel removable media", Format::Boolean);
        native(&mut entity, "ro", "Block read-only", Format::Boolean);
        if partition {
            native(&mut entity, "partition", "Partition number", Format::Text);
            native(&mut entity, "start", "Partition start (512-byte sectors)", Format::Text);
        }
        for (key, label) in [
            ("ID_MODEL", "Disk model"),
            ("ID_VENDOR", "Manufacturer"),
            ("ID_SERIAL_SHORT", "Serial number"),
            ("ID_REVISION", "Firmware revision"),
            ("ID_BUS", "Transport"),
            ("ID_FS_TYPE", "Filesystem / content type"),
            ("ID_FS_USAGE", "Content usage"),
            ("ID_FS_LABEL", "Filesystem / content label"),
            ("ID_FS_UUID", "Filesystem / content UUID"),
            ("ID_PART_TABLE_TYPE", "Partition table type"),
        ] {
            metadata(&mut entity, raw.as_ref(), key, label);
        }
        if exists(&format!("{path}/dm")) {
            native(&mut entity, "dm/name", "Device-mapper name", Format::Text);
            native(&mut entity, "dm/uuid", "Device-mapper UUID", Format::Text);
        }
        if exists(&format!("{path}/md")) {
            native(&mut entity, "md/level", "RAID level", Format::Text);
            native(&mut entity, "md/array_state", "RAID state", Format::Text);
            native(&mut entity, "md/degraded", "RAID missing members", Format::Text);
        }
        // Read only selected SCSI/NVMe identity metadata, with parent source kept explicit.
        let hardware = canonical(&format!("{path}/device"));
        if hardware.starts_with("/sys/devices/") {
            let subsystem = canonical(&format!("{hardware}/subsystem"));
            if file_name(&subsystem) == "nvme" {
                field(&mut entity.fields, "native-transport", "Native transport", ok("NVMe"), format!("{hardware}/subsystem"), Format::Text);
            } else if raw.as_ref().and_then(|d| d.property_value("ID_ATA_SATA")).is_some_and(|v| v == "1") {
                field(&mut entity.fields, "native-transport", "Native transport", ok("SATA"), format!("udev: ID_ATA_SATA on {path}"), Format::Text);
            }
            for (key, label) in [
                ("model", "Hardware model"),
                ("vendor", "Hardware vendor"),
                ("rev", "Hardware revision"),
                ("serial", "Hardware serial"),
                ("firmware_rev", "Hardware firmware revision"),
            ] {
                let source = format!("{hardware}/{key}");
                field(&mut entity.fields, &format!("hardware/{key}"), label, text_file(&source, TEXT_LIMIT), source, Format::Text);
            }
        }
        let points = mount_points.get(&number.value).cloned().unwrap_or_default();
        let joined = points.join("\n");
        let mounts_value = if mounts.state != ReadState::Available {
            mounts.clone()
        } else if number.state != ReadState::Available {
            number.clone()
        } else if !points.is_empty() {
            if joined.len() > TEXT_LIMIT {
                storage_failure(libc::EOVERFLOW)
            } else {
                ok(joined)
            }
        } else {
            ok("no-observed-mount")
        };
        field(
            &mut entity.fields,
            "mounts",
            "Mount points (application namespace)",
            mounts_value,
            format!("/proc/self/mountinfo : {}", number.value),
            Format::Text,
        );
        result.entities.push(entity);
    }
    for edge in &native_links {
        if wanted.contains(&edge.from) && wanted.contains(&edge.to) {
            link(&mut result, &edge.from, &edge.to, edge.label, edge.source.clone());
        }
    }
    if device.subsystem == "nvme" {
        let mut controller = StorageEntity { id: device.path.clone(), ..Default::default() };
        native(&mut controller, "model", "Controller model", Format::Text);
        native(&mut controller, "serial", "Controller serial number", Format::Text);
        native(&mut controller, "firmware_rev", "Controller firmware revision", Format::Text);
        result.entities.insert(0, controller);
        for path in selected.iter().filter(|p| p.starts_with(&format!("{}/", device.path))) {
            link(&mut result, &device.path, path, "Controller namespace / partition", "sysfs ancestry");
        }
    }
    if cached_service && !cancelled() {
        #[cfg(feature = "udisks2")]
        service::add_service(&mut result, &wanted, &device.path, cancel);
    }
    if !cfg!(feature = "udisks2") {
        field(
            &mut result.notes,
            "service",
            "UDisks2 metadata",
            Attribute { state: ReadState::Unsupported, ..Default::default() },
            "UDisks2 support not enabled in this build",
            Format::Text,
        );
    }
    // Related paths and device numbers can also be reused. Reject the whole storage snapshot
    // if a native entity changed while the service was queried.
    for (path, before) in &identities {
        if before.is_empty() || inode_identity(path) != *before {
            result.entities.clear();
            result.links.clear();
            result.notes.clear();
            field(&mut result.notes, "changed", "Storage topology changed during collection", storage_failure(libc::EAGAIN), path.clone(), Format::Text);
            break;
        }
    }
    result
}

// ---------------------------------------------------------------------------------------
// UDisks2 over the system bus
// ---------------------------------------------------------------------------------------

#[cfg(feature = "udisks2")]
mod service {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    use zbus::blocking::Connection;
    use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

    type Props = HashMap<String, OwnedValue>;
    type Interfaces = BTreeMap<String, Props>;
    type Objects = BTreeMap<String, Interfaces>;

    const PREFIX: &str = "org.freedesktop.UDisks2.";
    const DESTINATION: &str = "org.freedesktop.UDisks2";
    /// Shared wait budget for all detailed health calls of one snapshot.
    const HEALTH_BUDGET: Duration = Duration::from_millis(2500);

    // ---- value helpers (the QVariant conversions used by the C++) ----

    fn text_of(value: &Value) -> String {
        match value {
            Value::Str(s) => s.to_string(),
            Value::ObjectPath(p) => p.to_string(),
            Value::Signature(s) => s.to_string(),
            Value::Bool(b) => b.to_string(),
            Value::U8(n) => n.to_string(),
            Value::I16(n) => n.to_string(),
            Value::U16(n) => n.to_string(),
            Value::I32(n) => n.to_string(),
            Value::U32(n) => n.to_string(),
            Value::I64(n) => n.to_string(),
            Value::U64(n) => n.to_string(),
            Value::F64(n) => n.to_string(),
            Value::Value(inner) => text_of(inner),
            _ => String::new(),
        }
    }

    fn string_list(value: &Value) -> Vec<String> {
        match value {
            Value::Array(items) => items.iter().map(text_of).collect(),
            Value::Value(inner) => string_list(inner),
            other => vec![text_of(other)],
        }
    }

    fn to_f64(value: &Value) -> f64 {
        match value {
            Value::U8(n) => *n as f64,
            Value::I16(n) => *n as f64,
            Value::U16(n) => *n as f64,
            Value::I32(n) => *n as f64,
            Value::U32(n) => *n as f64,
            Value::I64(n) => *n as f64,
            Value::U64(n) => *n as f64,
            Value::F64(n) => *n,
            Value::Bool(b) => *b as u8 as f64,
            Value::Str(s) => s.parse().unwrap_or(0.0),
            Value::Value(inner) => to_f64(inner),
            _ => 0.0,
        }
    }

    fn to_u64(value: &Value) -> u64 {
        match value {
            Value::U8(n) => *n as u64,
            Value::U16(n) => *n as u64,
            Value::U32(n) => *n as u64,
            Value::U64(n) => *n,
            Value::I16(n) => (*n).max(0) as u64,
            Value::I32(n) => (*n).max(0) as u64,
            Value::I64(n) => (*n).max(0) as u64,
            Value::Bool(b) => *b as u64,
            Value::Str(s) => s.parse().unwrap_or(0),
            Value::Value(inner) => to_u64(inner),
            _ => 0,
        }
    }

    fn to_bool(value: &Value) -> bool {
        match value {
            Value::Bool(b) => *b,
            Value::Str(s) => !matches!(s.as_str(), "" | "0" | "false"),
            other => to_f64(other) != 0.0,
        }
    }

    fn object_path(props: Option<&Props>, key: &str) -> String {
        match props.and_then(|p| p.get(key)).map(|v| &**v) {
            Some(Value::ObjectPath(p)) => p.to_string(),
            _ => String::new(),
        }
    }

    fn iface<'a>(interfaces: &'a Interfaces, name: &str) -> Option<&'a Props> {
        interfaces.get(&format!("{PREFIX}{name}"))
    }

    // ---- D-Bus calls with a bounded wait ----

    enum CallError {
        Cancelled,
        TimedOut,
        Bus(zbus::Error),
    }

    fn dbus_error_name(error: &zbus::Error) -> String {
        match error {
            zbus::Error::MethodError(name, _, _) => name.to_string(),
            _ => String::new(),
        }
    }

    /// Runs `call` on a helper thread and waits at most `timeout`, polling `cancel` every 50 ms.
    /// A call that outlives the wait is abandoned; its result is dropped when it finally returns.
    fn call_with_timeout(
        timeout: Duration,
        cancel: Option<&AtomicBool>,
        call: impl FnOnce() -> zbus::Result<zbus::Message> + Send + 'static,
    ) -> Result<zbus::Message, CallError> {
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(call());
        });
        let started = Instant::now();
        loop {
            if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                return Err(CallError::Cancelled);
            }
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(CallError::TimedOut);
            }
            match receiver.recv_timeout(remaining.min(Duration::from_millis(50))) {
                Ok(Ok(message)) => return Ok(message),
                Ok(Err(error)) => return Err(CallError::Bus(error)),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(CallError::TimedOut),
            }
        }
    }

    fn timeout_like(name: &str) -> bool {
        matches!(name, "org.freedesktop.DBus.Error.NoReply" | "org.freedesktop.DBus.Error.Timeout" | "org.freedesktop.DBus.Error.TimedOut")
    }

    fn snapshot(result: &mut StorageProperties, cancel: Option<&AtomicBool>) -> Option<(Connection, Objects)> {
        let source = "org.freedesktop.UDisks2: ObjectManager.GetManagedObjects";
        let note = |result: &mut StorageProperties, value: Attribute| {
            field(&mut result.notes, "service", "UDisks2 metadata", value, source, Format::Text);
        };
        let Ok(connection) = Connection::system() else {
            note(result, storage_failure(libc::ENOTCONN));
            return None;
        };
        // Read the existing cache; never start a storage daemon.
        let owned = connection
            .call_method(Some("org.freedesktop.DBus"), "/org/freedesktop/DBus", Some("org.freedesktop.DBus"), "NameHasOwner", &(DESTINATION,))
            .ok()
            .and_then(|m| m.body().deserialize::<bool>().ok());
        if owned != Some(true) {
            note(result, Attribute { state: ReadState::Unavailable, value: String::new(), error: libc::EIO });
            return None;
        }
        let worker = connection.clone();
        let reply = call_with_timeout(Duration::from_millis(2500), cancel, move || {
            worker.call_method(Some(DESTINATION), "/org/freedesktop/UDisks2", Some("org.freedesktop.DBus.ObjectManager"), "GetManagedObjects", &())
        });
        let message = match reply {
            Err(CallError::Cancelled) => return None,
            Err(CallError::TimedOut) => {
                note(result, Attribute { state: ReadState::Error, value: String::new(), error: libc::ETIMEDOUT });
                return None;
            }
            Err(CallError::Bus(error)) => {
                let name = dbus_error_name(&error);
                let state = match name.as_str() {
                    "org.freedesktop.DBus.Error.AccessDenied" => ReadState::PermissionDenied,
                    "org.freedesktop.DBus.Error.ServiceUnknown" | "org.freedesktop.DBus.Error.UnknownObject" => ReadState::Unavailable,
                    _ => ReadState::Error,
                };
                note(result, Attribute { state, value: String::new(), error: if timeout_like(&name) { libc::ETIMEDOUT } else { libc::EIO } });
                return None;
            }
            Ok(message) => message,
        };
        type Wire = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;
        let Ok(raw) = message.body().deserialize::<Wire>() else {
            note(result, storage_failure(libc::EINVAL));
            return None;
        };
        if raw.len() > RECORD_LIMIT {
            note(result, storage_failure(libc::EOVERFLOW));
            return None;
        }
        note(result, ok("cached"));
        let objects: Objects = raw.into_iter().map(|(path, interfaces)| (path.to_string(), interfaces.into_iter().collect::<Interfaces>())).collect();
        Some((connection, objects))
    }

    fn service_field(entity: &mut StorageEntity, props: Option<&Props>, interface: &str, key: &str, label: &'static str, format: Format, zero_unknown: bool) {
        let mut data = Attribute::default();
        if let Some(value) = props.and_then(|p| p.get(key)) {
            let mut text = if format == Format::NvmeHealth { string_list(value).join("\n") } else { text_of(value) };
            if matches!(format, Format::Boolean | Format::Removable | Format::AtaHealth) {
                text = if to_bool(value) { "1" } else { "0" }.into();
            }
            if text.len() > TEXT_LIMIT {
                data = storage_failure(libc::EOVERFLOW);
            } else if !(format == Format::Text && text.is_empty()) && (!zero_unknown || to_f64(value) != 0.0) {
                data = ok(text);
            }
        }
        let source = format!("{} : {interface}.{key}", entity.id);
        field(&mut entity.fields, &format!("{interface}/{key}"), label, data, source, format);
    }

    /// Budget shared by all detailed health calls; it starts at the first one.
    #[derive(Default)]
    struct HealthBudget {
        started: Option<Instant>,
    }

    /// Cache-only `SmartGetAttributes` call: no activation, no interactive authorization.
    fn health_reply(
        connection: &Connection,
        path: &str,
        interface: &str,
        budget: &mut HealthBudget,
        cancel: Option<&AtomicBool>,
    ) -> Result<zbus::Message, Attribute> {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Err(storage_failure(libc::ECANCELED));
        }
        let started = *budget.started.get_or_insert_with(Instant::now);
        let remaining = HEALTH_BUDGET.saturating_sub(started.elapsed());
        // Unavailable + ETIMEDOUT on HealthQueryStatus means no request was sent.
        if remaining.is_zero() {
            return Err(Attribute { state: ReadState::Unavailable, value: String::new(), error: libc::ETIMEDOUT });
        }
        let (worker, path, interface) = (connection.clone(), path.to_string(), interface.to_string());
        let reply = call_with_timeout(remaining, cancel, move || {
            let options: HashMap<String, Value<'_>> = HashMap::new();
            worker.call_method(Some(DESTINATION), path.as_str(), Some(interface.as_str()), "SmartGetAttributes", &(options,))
        });
        match reply {
            Ok(message) => Ok(message),
            Err(CallError::Cancelled) => Err(storage_failure(libc::ECANCELED)),
            Err(CallError::TimedOut) => Err(storage_failure(libc::ETIMEDOUT)),
            Err(CallError::Bus(error)) => {
                let name = dbus_error_name(&error);
                let state = match name.as_str() {
                    "org.freedesktop.DBus.Error.AccessDenied" => ReadState::PermissionDenied,
                    "org.freedesktop.DBus.Error.UnknownMethod" | "org.freedesktop.DBus.Error.UnknownInterface" => ReadState::Unsupported,
                    "org.freedesktop.DBus.Error.ServiceUnknown" | "org.freedesktop.DBus.Error.UnknownObject" => ReadState::Unavailable,
                    _ => ReadState::Error,
                };
                Err(Attribute { state, value: String::new(), error: if timeout_like(&name) { libc::ETIMEDOUT } else { libc::EIO } })
            }
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Kind {
        U8,
        U16,
        U32,
        U64,
    }

    fn typed_number(value: &Value, kind: Kind) -> Option<u64> {
        match (kind, value) {
            (Kind::U8, Value::U8(n)) => Some(*n as u64),
            (Kind::U16, Value::U16(n)) => Some(*n as u64),
            (Kind::U32, Value::U32(n)) => Some(*n as u64),
            (Kind::U64, Value::U64(n)) => Some(*n),
            _ => None,
        }
    }

    /// Fields from an NVMe `SmartGetAttributes` reply.
    pub(super) fn nvme_fields(entity: &mut StorageEntity, values: &Props, interface: &str, source: &str) {
        let mut add = |key: &str, label: &'static str, kind: Kind, format: Format, zero_unknown: bool| {
            let mut data = Attribute::default();
            if let Some(value) = values.get(key) {
                match typed_number(value, kind) {
                    None => data = storage_failure(libc::EINVAL),
                    Some(n) if !zero_unknown || n != 0 => data = ok(n.to_string()),
                    Some(_) => {}
                }
            }
            field(&mut entity.fields, &format!("{interface}/SmartGetAttributes/{key}"), label, data, format!("{source} : {key}"), format);
        };
        add("avail_spare", "Cached available spare", Kind::U8, Format::Percent, false);
        add("spare_thresh", "Cached spare threshold", Kind::U8, Format::Percent, false);
        add("percent_used", "Cached estimated endurance used", Kind::U8, Format::Percent, false);
        add("total_data_read", "Cached total data read", Kind::U64, Format::Bytes, false);
        add("total_data_written", "Cached total data written", Kind::U64, Format::Bytes, false);
        add("ctrl_busy_time", "Cached controller busy time", Kind::U64, Format::Minutes, false);
        add("power_cycles", "Cached power cycles", Kind::U64, Format::Count, false);
        add("unsafe_shutdowns", "Cached unsafe shutdowns", Kind::U64, Format::Count, false);
        add("media_errors", "Cached media and data integrity errors", Kind::U64, Format::Count, false);
        add("num_err_log_entries", "Cached error information log entries", Kind::U64, Format::Count, false);
        add("wctemp", "Cached warning temperature threshold", Kind::U16, Format::Kelvin, true);
        add("cctemp", "Cached critical temperature threshold", Kind::U16, Format::Kelvin, true);
        add("warning_temp_time", "Cached time above warning temperature", Kind::U32, Format::Minutes, false);
        add("critical_temp_time", "Cached time above critical temperature", Kind::U32, Format::Minutes, false);

        const LABELS: [&str; 8] = [
            "Cached temperature sensor 1",
            "Cached temperature sensor 2",
            "Cached temperature sensor 3",
            "Cached temperature sensor 4",
            "Cached temperature sensor 5",
            "Cached temperature sensor 6",
            "Cached temperature sensor 7",
            "Cached temperature sensor 8",
        ];
        // At most eight items are decoded; a longer array is an overflow, a wrong type invalid.
        let mut sensor_state = Attribute::default();
        let mut temperatures: Vec<u16> = Vec::new();
        if let Some(sensors) = values.get("temp_sensors") {
            match &**sensors {
                Value::Array(items) => {
                    let parsed: Option<Vec<u16>> = items.iter().map(|v| if let Value::U16(n) = v { Some(*n) } else { None }).collect();
                    match parsed {
                        None => sensor_state = storage_failure(libc::EINVAL),
                        Some(list) if list.len() > 8 => sensor_state = storage_failure(libc::EOVERFLOW),
                        Some(list) => {
                            temperatures = list;
                            sensor_state = ok("");
                        }
                    }
                }
                _ => sensor_state = storage_failure(libc::EINVAL),
            }
        }
        for (i, label) in LABELS.iter().enumerate() {
            let mut data = if sensor_state.state == ReadState::Available { Attribute::default() } else { sensor_state.clone() };
            if sensor_state.state == ReadState::Available && temperatures.get(i).is_some_and(|k| *k != 0) {
                data = ok(temperatures[i].to_string());
            }
            field(
                &mut entity.fields,
                &format!("{interface}/SmartGetAttributes/temp_sensors/{}", i + 1),
                label,
                data,
                format!("{source} : temp_sensors[{i}]"),
                Format::Kelvin,
            );
        }
    }

    type AtaWire = (u8, String, u16, i32, i32, i32, i64, i32, HashMap<String, OwnedValue>);

    /// Validates the full list of ATA attributes; any bad entry rejects the whole list.
    pub(super) fn validate_ata(items: Vec<AtaWire>) -> Result<Vec<AtaSmartAttribute>, i32> {
        let mut attributes = Vec::new();
        let mut ids = HashSet::new();
        for (id, name, flags, current, worst, threshold, interpreted, unit, expansion) in items {
            if attributes.len() >= 256 {
                return Err(libc::EOVERFLOW);
            }
            if name.len() > 128 || expansion.len() > 64 {
                return Err(libc::EOVERFLOW);
            }
            let normalized = |n: i32| (-1..=255).contains(&n);
            if ids.contains(&id) || !normalized(current) || !normalized(worst) || !normalized(threshold) || unit < 0 {
                return Err(libc::EINVAL);
            }
            ids.insert(id);
            attributes.push(AtaSmartAttribute { id, name, flags, current, worst, threshold, interpreted, unit });
        }
        Ok(attributes)
    }

    fn nvme_attributes(entity: &mut StorageEntity, connection: &Connection, budget: &mut HealthBudget, cancel: Option<&AtomicBool>) {
        let interface = "org.freedesktop.UDisks2.NVMe.Controller";
        let source = format!("{} : {interface}.SmartGetAttributes", entity.id);
        let note = |entity: &mut StorageEntity, state: Attribute| {
            field(
                &mut entity.fields,
                &format!("{interface}/SmartGetAttributes"),
                "Cached NVMe health details",
                state,
                source.clone(),
                Format::HealthQueryStatus,
            );
        };
        let message = match health_reply(connection, &entity.id.clone(), interface, budget, cancel) {
            Ok(message) => message,
            Err(state) => return note(entity, state),
        };
        let Ok(values) = message.body().deserialize::<Props>() else { return note(entity, storage_failure(libc::EINVAL)) };
        if values.len() > 64 {
            return note(entity, storage_failure(libc::EOVERFLOW));
        }
        nvme_fields(entity, &values, interface, &source);
    }

    fn ata_attributes(entity: &mut StorageEntity, connection: &Connection, budget: &mut HealthBudget, cancel: Option<&AtomicBool>) {
        let interface = "org.freedesktop.UDisks2.Drive.Ata";
        let source = format!("{} : {interface}.SmartGetAttributes", entity.id);
        let note = |entity: &mut StorageEntity, state: Attribute| {
            field(
                &mut entity.fields,
                &format!("{interface}/SmartGetAttributes"),
                "Cached ATA SMART attributes",
                state,
                source.clone(),
                Format::HealthQueryStatus,
            );
        };
        let message = match health_reply(connection, &entity.id.clone(), interface, budget, cancel) {
            Ok(message) => message,
            Err(state) => return note(entity, state),
        };
        let Ok(items) = message.body().deserialize::<Vec<AtaWire>>() else { return note(entity, storage_failure(libc::EINVAL)) };
        match validate_ata(items) {
            Err(error) => note(entity, storage_failure(error)),
            Ok(list) if list.is_empty() => note(entity, Attribute::default()),
            Ok(list) => entity.ata_attributes = list, // Publish a complete, validated list only.
        }
    }

    pub(super) fn add_service(result: &mut StorageProperties, native_paths: &HashSet<String>, selected_path: &str, cancel: Option<&AtomicBool>) {
        let Some((connection, objects)) = snapshot(result, cancel) else { return };
        let cancelled = || cancel.is_some_and(|c| c.load(Ordering::Relaxed));
        let mut wanted: HashSet<String> = HashSet::new();
        let mut selected_drives: HashSet<String> = HashSet::new();
        let mut blocks: HashMap<String, String> = HashMap::new();
        for (key, interfaces) in &objects {
            let Some(number) = iface(interfaces, "Block").and_then(|p| p.get("DeviceNumber")) else { continue };
            let number = to_u64(number) as libc::dev_t;
            let path = canonical(&format!("/sys/dev/block/{}:{}", libc::major(number), libc::minor(number)));
            if native_paths.contains(&path) {
                wanted.insert(key.clone());
                blocks.insert(key.clone(), path.clone());
                if path == selected_path || path.starts_with(&format!("{selected_path}/")) {
                    let drive = object_path(iface(interfaces, "Block"), "Drive");
                    if !drive.is_empty() && drive != "/" {
                        selected_drives.insert(drive);
                    }
                }
            }
        }
        // No guessed model/serial matching; each drive object is retained once.
        const PAIRS: [(&str, &str); 7] = [
            ("Block", "Drive"),
            ("Block", "CryptoBackingDevice"),
            ("Block", "MDRaid"),
            ("Block", "MDRaidMember"),
            ("Partition", "Table"),
            ("Block.LVM2", "LogicalVolume"),
            ("LogicalVolume", "VolumeGroup"),
        ];
        for _ in 0..RELATED_LIMIT {
            let before = wanted.len();
            for (key, interfaces) in &objects {
                if !wanted.contains(key) {
                    continue;
                }
                for (interface, property) in PAIRS {
                    let target = object_path(iface(interfaces, interface), property);
                    if !target.is_empty() && target != "/" && objects.contains_key(&target) {
                        wanted.insert(target);
                    }
                }
            }
            if wanted.len() > RELATED_LIMIT {
                field(&mut result.notes, "service-limit", "Related storage metadata", storage_failure(libc::EOVERFLOW), "UDisks2", Format::Text);
                return;
            }
            if wanted.len() == before {
                break;
            }
        }
        let mut ordered: Vec<&String> = objects.keys().filter(|k| wanted.contains(*k)).collect();
        // Prioritize explicit selected-device associations; keep object-path order within each
        // tier and do not guess a single drive for spanning volumes.
        ordered.sort_by_key(|k| !selected_drives.contains(*k));
        let mut budget = HealthBudget::default();
        for path in ordered {
            if cancelled() {
                return;
            }
            let interfaces = &objects[path];
            let mut entity = StorageEntity { id: path.clone(), ..Default::default() };
            if let Some(block) = blocks.get(path) {
                link(result, block, path, "UDisks2 block metadata", "UDisks2 Block.DeviceNumber / sysfs dev");
            }
            let copy = |entity: &mut StorageEntity, interface: &str, key: &str, label: &'static str, format: Format, zero_unknown: bool| {
                service_field(entity, iface(interfaces, interface), &format!("{PREFIX}{interface}"), key, label, format, zero_unknown);
            };
            let has = |name: &str| iface(interfaces, name).is_some();
            if has("Drive") {
                copy(&mut entity, "Drive", "Vendor", "Drive manufacturer", Format::Text, false);
                copy(&mut entity, "Drive", "Model", "Drive model", Format::Text, false);
                copy(&mut entity, "Drive", "Serial", "Drive serial number", Format::Text, false);
                copy(&mut entity, "Drive", "Revision", "Drive firmware revision", Format::Text, false);
                copy(&mut entity, "Drive", "ConnectionBus", "Drive transport", Format::Text, false);
                copy(&mut entity, "Drive", "Size", "Drive capacity", Format::Bytes, true);
                copy(&mut entity, "Drive", "Removable", "Removable/fixed hint", Format::Removable, false);
                copy(&mut entity, "Drive", "MediaRemovable", "Drive media removable", Format::Boolean, false);
                copy(&mut entity, "Drive", "MediaAvailable", "Drive media available", Format::Boolean, false);
            }
            if has("Block") {
                copy(&mut entity, "Block", "IdUsage", "Content usage", Format::Text, false);
                copy(&mut entity, "Block", "IdType", "Filesystem / content type", Format::Text, false);
                copy(&mut entity, "Block", "IdVersion", "Filesystem / content version", Format::Text, false);
                copy(&mut entity, "Block", "IdLabel", "Filesystem / content label", Format::Text, false);
                copy(&mut entity, "Block", "IdUUID", "Filesystem / content UUID", Format::Text, false);
            }
            if has("Partition") {
                copy(&mut entity, "Partition", "Number", "Partition number", Format::Text, false);
                copy(&mut entity, "Partition", "Type", "Partition type", Format::Text, false);
                copy(&mut entity, "Partition", "Offset", "Partition offset", Format::Bytes, false);
                copy(&mut entity, "Partition", "Size", "Partition capacity", Format::Bytes, false);
                copy(&mut entity, "Partition", "Name", "Partition name", Format::Text, false);
                copy(&mut entity, "Partition", "UUID", "Partition UUID", Format::Text, false);
            }
            if has("PartitionTable") {
                copy(&mut entity, "PartitionTable", "Type", "Partition table type", Format::Text, false);
            }
            if has("MDRaid") {
                copy(&mut entity, "MDRaid", "Name", "RAID name", Format::Text, false);
                copy(&mut entity, "MDRaid", "Level", "RAID level", Format::Text, false);
                copy(&mut entity, "MDRaid", "UUID", "RAID UUID", Format::Text, false);
                copy(&mut entity, "MDRaid", "NumDevices", "RAID member count", Format::Text, false);
            }
            if has("LogicalVolume") {
                copy(&mut entity, "LogicalVolume", "Name", "Logical volume name", Format::Text, false);
                copy(&mut entity, "LogicalVolume", "Type", "Logical volume type", Format::Text, false);
            }
            if has("VolumeGroup") {
                copy(&mut entity, "VolumeGroup", "Name", "Volume group name", Format::Text, false);
            }
            let (ata, nvme) = (has("Drive.Ata"), has("NVMe.Controller"));
            if ata || nvme {
                let name = if ata { "Drive.Ata" } else { "NVMe.Controller" };
                let health = iface(interfaces, name);
                let full = format!("{PREFIX}{name}");
                if ata {
                    copy(&mut entity, "Drive.Ata", "SmartSupported", "ATA SMART supported", Format::Boolean, false);
                    copy(&mut entity, "Drive.Ata", "SmartEnabled", "ATA SMART enabled", Format::Boolean, false);
                }
                service_field(&mut entity, health, &full, "SmartUpdated", "Cached health updated", Format::Epoch, true);
                // Missing/zero timestamps invalidate every health assessment field.
                if health.and_then(|h| h.get("SmartUpdated")).is_some_and(|v| to_u64(v) != 0) {
                    service_field(&mut entity, health, &full, "SmartTemperature", "Cached temperature", Format::Kelvin, true);
                    if ata {
                        copy(&mut entity, "Drive.Ata", "SmartPowerOnSeconds", "Cached power-on time", Format::Seconds, true);
                        copy(&mut entity, "Drive.Ata", "SmartFailing", "Cached ATA failure prediction", Format::AtaHealth, false);
                        for (key, label) in [
                            ("SmartNumBadSectors", "Cached bad sectors"),
                            ("SmartNumAttributesFailing", "Cached failing attributes"),
                            ("SmartNumAttributesFailedInThePast", "Cached attributes failed in the past"),
                        ] {
                            let mut data = Attribute::default();
                            if let Some(count) = health.and_then(|h| h.get(key)) {
                                let number = match (&**count, key) {
                                    (Value::I64(n), "SmartNumBadSectors") => Some(*n),
                                    (Value::I32(n), _) => Some(*n as i64),
                                    _ => None,
                                };
                                match number {
                                    None => data = storage_failure(libc::EINVAL),
                                    Some(n) if n < -1 => data = storage_failure(libc::EINVAL),
                                    Some(n) if n >= 0 => data = ok(n.to_string()),
                                    Some(_) => {}
                                }
                            }
                            field(
                                &mut entity.fields,
                                &format!("{PREFIX}Drive.Ata/{key}"),
                                label,
                                data,
                                format!("{} : {PREFIX}Drive.Ata.{key}", entity.id),
                                Format::Count,
                            );
                        }
                        ata_attributes(&mut entity, &connection, &mut budget, cancel);
                    } else {
                        copy(&mut entity, "NVMe.Controller", "SmartCriticalWarning", "Cached NVMe critical warnings", Format::NvmeHealth, false);
                        copy(&mut entity, "NVMe.Controller", "SmartPowerOnHours", "Cached power-on time", Format::Hours, true);
                        nvme_attributes(&mut entity, &connection, &mut budget, cancel);
                    }
                } else {
                    let source = format!("{} : {full}.SmartUpdated (never updated or unavailable)", entity.id);
                    field(&mut entity.fields, "health", "Cached health", Attribute::default(), source, Format::Text);
                }
            } else if has("Drive") {
                let source = format!("{} : UDisks2 (no supported ATA or NVMe health interface)", entity.id);
                field(&mut entity.fields, "health", "Cached health", Attribute { state: ReadState::Unsupported, ..Default::default() }, source, Format::Text);
            }
            for (name, props) in interfaces {
                for key in ["Drive", "CryptoBackingDevice", "MDRaid", "MDRaidMember", "Table", "LogicalVolume", "VolumeGroup"] {
                    let target = object_path(Some(props), key);
                    if wanted.contains(&target) {
                        link(result, &entity.id, &target, "Storage relationship", format!("{name}.{key}"));
                    }
                }
            }
            result.entities.push(entity);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mountinfo_groups_mounts_by_device_number_and_decodes_escapes_once() {
        let text = "36 35 259:1 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p1 rw\n\
                    40 36 259:2 /my\\040dir /mnt/a\\040b rw - btrfs /dev/nvme0n1p2 rw\n\
                    41 36 259:2 /lit\\134040 /mnt/c rw - btrfs /dev/nvme0n1p2 rw\n\
                    short line\n";
        let mounts = parse_mountinfo(text);
        assert_eq!(mounts["259:1"], ["/ [ext4; root=/]"]);
        assert_eq!(mounts["259:2"][0], "/mnt/a b [btrfs; root=/my dir]");
        assert_eq!(mounts["259:2"][1], "/mnt/c [btrfs; root=/lit\\040]", "an escaped backslash is not decoded twice");
        assert!(!mounts.contains_key("short"));
    }

    #[test]
    fn links_skip_empty_self_root_and_duplicate_edges() {
        let mut result = StorageProperties::default();
        link(&mut result, "a", "b", "Used by", "s");
        link(&mut result, "a", "b", "Used by", "s");
        link(&mut result, "a", "a", "Used by", "s");
        link(&mut result, "", "b", "Used by", "s");
        link(&mut result, "a", "/", "Used by", "s");
        link(&mut result, "a", "b", "Backed by", "s");
        assert_eq!(result.links.len(), 2);
    }

    #[test]
    fn text_files_are_bounded_and_classified() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("v");
        std::fs::write(&file, " 8 \n").unwrap();
        assert_eq!(text_file(&file.to_string_lossy(), 16).value, "8");
        std::fs::write(&file, vec![b'x'; 20]).unwrap();
        assert_eq!(text_file(&file.to_string_lossy(), 16).error, libc::EOVERFLOW);
        assert_eq!(text_file(&tmp.path().join("none").to_string_lossy(), 16).state, ReadState::Unavailable);
    }

    #[test]
    fn non_storage_devices_are_not_applicable() {
        let device = Device { subsystem: "usb".into(), ..Default::default() };
        let found = collect_storage_properties(&device, true, None);
        assert!(!found.applicable && found.entities.is_empty() && found.notes.is_empty());
    }

    #[test]
    fn real_disks_report_native_metadata() {
        let devices = crate::scan().expect("scan");
        let Some(disk) = devices.iter().find(|d| d.subsystem == "block" && d.devtype == "disk") else { return };
        let found = collect_storage_properties(disk, false, None);
        assert!(found.applicable);
        let entity = found.entities.iter().find(|e| e.id == disk.path).expect("selected disk entity");
        let value = |id: &str| entity.fields.iter().find(|f| f.id == id).map(|f| f.value.clone()).unwrap_or_default();
        assert_eq!(value("capacity").state, ReadState::Available);
        assert!(value("capacity").value.parse::<u64>().unwrap() % 512 == 0);
        assert_eq!(value("dev").state, ReadState::Available);
        let mountinfo = found.notes.iter().find(|n| n.id == "mountinfo").expect("mountinfo note");
        assert_eq!(mountinfo.value.value, "application-namespace");
        // Deep search never queries the service; without the feature the note says why.
        let service_notes = found.notes.iter().filter(|n| n.id == "service").count();
        assert_eq!(service_notes, usize::from(!cfg!(feature = "udisks2")));
        let cancelled = AtomicBool::new(true);
        assert!(collect_storage_properties(disk, true, Some(&cancelled)).entities.is_empty());
    }

    #[cfg(feature = "udisks2")]
    mod service_tests {
        use super::super::service::{nvme_fields, validate_ata};
        use super::*;
        use zbus::zvariant::{OwnedValue, Value};

        fn owned(value: Value<'_>) -> OwnedValue {
            OwnedValue::try_from(value).unwrap()
        }

        #[test]
        fn nvme_values_are_typed_and_zero_means_unknown_only_where_documented() {
            let mut values = HashMap::new();
            values.insert("avail_spare".to_string(), owned(Value::U8(100)));
            values.insert("percent_used".to_string(), owned(Value::U64(3))); // wrong type
            values.insert("wctemp".to_string(), owned(Value::U16(0))); // zero = unknown
            values.insert("power_cycles".to_string(), owned(Value::U64(0))); // zero is a real count
            values.insert("temp_sensors".to_string(), owned(Value::from(vec![310u16, 0, 305])));
            let mut entity = StorageEntity { id: "/org/freedesktop/UDisks2/drives/x".into(), ..Default::default() };
            nvme_fields(&mut entity, &values, "org.freedesktop.UDisks2.NVMe.Controller", "src");
            let get = |id: &str| entity.fields.iter().find(|f| f.id.ends_with(id)).unwrap().value.clone();
            assert_eq!(get("avail_spare").value, "100");
            assert_eq!(get("percent_used").error, libc::EINVAL);
            assert_eq!(get("wctemp").state, ReadState::Unavailable);
            assert_eq!(get("power_cycles").value, "0");
            assert_eq!(get("temp_sensors/1").value, "310");
            assert_eq!(get("temp_sensors/2").state, ReadState::Unavailable);
            assert_eq!(get("temp_sensors/3").value, "305");
            assert_eq!(get("temp_sensors/8").state, ReadState::Unavailable);
        }

        #[test]
        fn too_many_or_mistyped_sensors_are_explicit_errors() {
            let mut entity = StorageEntity::default();
            let mut values = HashMap::new();
            values.insert("temp_sensors".to_string(), owned(Value::from(vec![1u16; 9])));
            nvme_fields(&mut entity, &values, "i", "s");
            assert!(entity.fields.iter().filter(|f| f.id.contains("temp_sensors")).all(|f| f.value.error == libc::EOVERFLOW));
            let mut other = StorageEntity::default();
            values.insert("temp_sensors".to_string(), owned(Value::from(vec![1u32])));
            nvme_fields(&mut other, &values, "i", "s");
            assert!(other.fields.iter().filter(|f| f.id.contains("temp_sensors")).all(|f| f.value.error == libc::EINVAL));
        }

        #[test]
        fn ata_lists_are_accepted_whole_or_rejected_whole() {
            let row = |id: u8, current: i32| (id, "raw-read-error-rate".to_string(), 0x2fu16, current, 100, 6, 0i64, 0, HashMap::new());
            let good = validate_ata(vec![row(1, 100), row(5, 200)]).unwrap();
            assert_eq!((good.len(), good[1].id), (2, 5));
            assert_eq!(validate_ata(vec![row(1, 100), row(1, 100)]), Err(libc::EINVAL), "duplicate ids");
            assert_eq!(validate_ata(vec![row(1, 300)]), Err(libc::EINVAL), "out-of-range value");
            let mut long = row(2, 1);
            long.1 = "x".repeat(129);
            assert_eq!(validate_ata(vec![long]), Err(libc::EOVERFLOW));
            let many: Vec<_> = (0..=256u32).map(|i| row((i % 255) as u8, 1)).collect();
            assert!(validate_ata(many).is_err());
        }

        #[test]
        fn live_service_snapshot_is_cache_only_when_available() {
            let devices = crate::scan().expect("scan");
            let Some(disk) = devices.iter().find(|d| d.subsystem == "block" && d.devtype == "disk") else { return };
            let found = collect_storage_properties(disk, true, None);
            let service = found.notes.iter().find(|n| n.id == "service").expect("service note");
            if service.value.state == ReadState::Available {
                assert_eq!(service.value.value, "cached");
                assert!(found.entities.iter().any(|e| e.id.starts_with("/org/freedesktop/UDisks2/")));
            } else {
                assert!(matches!(service.value.state, ReadState::Unavailable | ReadState::PermissionDenied | ReadState::Error));
            }
        }
    }
}
