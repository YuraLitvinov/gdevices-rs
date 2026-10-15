//! System information, ported from `src/devices/systemproperties.cpp`.
//!
//! One read-only pass over kernel and firmware interfaces: OS, kernel, CPU models and feature
//! flags, topology and caches, installed and usable memory, DMI identity, GPU Resizable BAR
//! capability, boot mode, Secure Boot and virtualization. Pure parsers are kept separate so
//! fixtures can drive them; no command is ever run and no shell syntax is evaluated. Unknown is
//! never reported as zero, and a value that changed during collection is rejected.
use crate::cpuinfo::{parse_cpu_models, read_cpuinfo};
use crate::dirfd::{available, failure as dir_failure};
use crate::model::{Attribute, ReadState};
use crate::secureboot::{collect_secure_boot_information, MokCertificates};
use crate::sysfs::bounded_directory_names;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};

fn failed(error: i32) -> Attribute {
    let state = match error {
        libc::EACCES | libc::EPERM => ReadState::PermissionDenied,
        libc::ENOENT => ReadState::Unavailable,
        _ => ReadState::Error,
    };
    Attribute { state, value: String::new(), error }
}

#[derive(Clone, Debug, Default)]
pub struct FirmwareMemoryDevice {
    /// Firmware record index, not a socket/channel identifier.
    pub index: usize,
    /// Stable udev property suffixes.
    pub values: HashMap<String, Attribute>,
}

#[derive(Clone, Debug, Default)]
pub struct FirmwareMemoryInformation {
    pub status: Attribute,
    pub devices: Vec<FirmwareMemoryDevice>,
}

#[derive(Clone, Debug, Default)]
pub struct GpuResizableBar {
    pub pci_address: String,
    /// Available: sysfs exposes the capability; Unavailable: not exposed.
    pub status: Attribute,
    /// Raw validated hexadecimal bitmaps, by BAR number.
    pub supported_sizes: BTreeMap<usize, Attribute>,
    /// Allocated aperture size in bytes, by BAR number.
    pub current_sizes: BTreeMap<usize, Attribute>,
}

#[derive(Clone, Debug, Default)]
pub struct SystemProperties {
    pub gpu_rebar_status: Attribute,
    pub gpu_rebar: Vec<GpuResizableBar>,
    pub memory_devices: FirmwareMemoryInformation,
    pub mok: MokCertificates,
    pub values: HashMap<String, Attribute>,
    pub sources: HashMap<String, String>,
}

/// Small text file, trimmed; an empty file is an unavailable value (not an empty string).
fn read_text(path: &str, limit: usize, cancel: Option<&AtomicBool>) -> Attribute {
    if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
        return failed(libc::ECANCELED);
    }
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => return failed(e.raw_os_error().unwrap_or(libc::EIO)),
    };
    let mut bytes = Vec::new();
    if let Err(e) = (&mut file).take(limit as u64 + 1).read_to_end(&mut bytes) {
        return failed(e.raw_os_error().unwrap_or(libc::EIO));
    }
    if bytes.len() > limit {
        return failed(libc::EOVERFLOW);
    }
    let text = String::from_utf8_lossy(&bytes).trim().to_string();
    if text.is_empty() {
        Attribute::default()
    } else {
        available(text)
    }
}

fn non_negative(value: &Attribute) -> Option<i64> {
    (value.state == ReadState::Available).then(|| value.value.parse::<i64>().ok()).flatten().filter(|n| *n >= 0)
}

fn unsigned_decimal(text: &str) -> Option<u64> {
    (!text.is_empty() && text.len() <= 20 && text.bytes().all(|b| b.is_ascii_digit())).then(|| text.parse().ok()).flatten()
}

fn simplified(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ---------------------------------------------------------------------------------------
// Pure parsers
// ---------------------------------------------------------------------------------------

/// Supported-size bitmap of a `resourceN_resize` attribute; bit `n` means `2^(n+20)` bytes.
pub fn parse_resizable_bar_sizes(text: &str) -> Attribute {
    if text.is_empty() || text.len() > 16 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return failed(libc::EINVAL);
    }
    let bitmap = match u64::from_str_radix(text, 16) {
        Ok(0) | Err(_) => return failed(libc::EINVAL),
        Ok(n) => n,
    };
    // Do not shift beyond a 64-bit size.
    if bitmap >> 44 != 0 {
        return failed(libc::EOVERFLOW);
    }
    available(format!("{bitmap:x}"))
}

/// Size in bytes of the memory aperture currently allocated to one of the six PCI BARs.
pub fn parse_resizable_bar_allocation(resources: &str, bar: usize) -> Attribute {
    if bar >= 6 || resources.len() > 65536 {
        return failed(libc::EINVAL);
    }
    let lines: Vec<&str> = resources.split('\n').collect();
    let Some(line) = lines.get(bar) else { return failed(libc::EINVAL) };
    let fields: Vec<String> = simplified(line).split(' ').map(str::to_string).collect();
    if fields.len() != 3 {
        return failed(libc::EINVAL);
    }
    let mut values = [0u64; 3];
    for (slot, field) in values.iter_mut().zip(&fields) {
        let digits = field.strip_prefix("0x").or_else(|| field.strip_prefix("0X")).unwrap_or(field);
        if digits.is_empty() || digits.len() > 16 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
            return failed(libc::EINVAL);
        }
        let Ok(parsed) = u64::from_str_radix(digits, 16) else { return failed(libc::EINVAL) };
        *slot = parsed;
    }
    let [start, end, flags] = values;
    if flags & 0x1f00 != 0x200 {
        return Attribute::default(); // No reported memory resource.
    }
    if flags & 0x3000_0000 != 0 {
        return Attribute::default(); // Unassigned or disabled resource, not a BIOS ReBAR verdict.
    }
    if start == 0 && end == 0 {
        return Attribute::default(); // Empty/masked addresses never mean a one-byte BAR.
    }
    if end < start {
        return failed(libc::EINVAL);
    }
    if end - start == u64::MAX {
        return failed(libc::EOVERFLOW);
    }
    available((end - start + 1).to_string())
}

/// Kernel feature tokens per processor, grouped so identical lists share one line
/// (`0, 1<TAB>avx2 sse`). Never a union: different CPUs keep different lists.
pub fn parse_cpu_instruction_sets(cpuinfo: &str) -> Attribute {
    if cpuinfo.len() > 4 * 1024 * 1024 {
        return failed(libc::EOVERFLOW);
    }
    let mut processors: BTreeMap<u32, String> = BTreeMap::new();
    let mut record: HashMap<&str, &str> = HashMap::new();
    let mut error: Option<Attribute> = None;
    let commit = |record: &HashMap<&str, &str>, processors: &mut BTreeMap<u32, String>, error: &mut Option<Attribute>| {
        let Some(id) = record.get("processor") else { return };
        let Some(id) = id.parse::<u32>().ok().filter(|id| !processors.contains_key(id)) else {
            *error = Some(failed(libc::EINVAL));
            return;
        };
        if processors.len() >= 4096 {
            *error = Some(failed(libc::EOVERFLOW));
            return;
        }
        let features = ["flags", "Features", "isa"].iter().filter_map(|k| record.get(k)).find(|v| !v.is_empty());
        let Some(features) = features else {
            error.get_or_insert_with(Attribute::default);
            return;
        };
        let mut tokens: Vec<&str> = features.split_whitespace().collect();
        if tokens.len() > 4096 {
            *error = Some(failed(libc::EOVERFLOW));
            return;
        }
        tokens.sort_unstable();
        tokens.dedup();
        processors.insert(id, tokens.join(" "));
    };
    for line in cpuinfo.split('\n') {
        if line.trim().is_empty() {
            commit(&record, &mut processors, &mut error);
            record.clear();
            continue;
        }
        let Some((key, value)) = line.split_once(':') else { continue };
        let key = key.trim();
        if matches!(key, "processor" | "flags" | "Features" | "isa") {
            record.insert(key, value.trim());
        }
    }
    commit(&record, &mut processors, &mut error);
    if let Some(error) = error {
        return error; // Includes the "incomplete" (unavailable) marker.
    }
    if processors.is_empty() {
        return Attribute::default();
    }
    let mut groups: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for (id, features) in &processors {
        groups.entry(features.as_str()).or_default().push(id.to_string());
    }
    available(groups.iter().map(|(features, ids)| format!("{}\t{features}", ids.join(", "))).collect::<Vec<_>>().join("\n"))
}

/// `PRETTY_NAME` of os-release without evaluating anything: quotes and the shell escapes
/// permitted inside them are decoded, `$(…)` stays literal text. Last definition wins.
pub fn parse_os_name(text: &str) -> Attribute {
    let mut result = available("Linux"); // Specified default, only after a successful file read.
    for line in text.split('\n') {
        let Some(value) = line.strip_prefix("PRETTY_NAME=") else { continue };
        let value = value.trim();
        let chars: Vec<char> = value.chars().collect();
        let quote = chars.first().copied().filter(|c| *c == '\'' || *c == '"');
        let body: &[char] = if let Some(q) = quote {
            if chars.len() < 2 || *chars.last().expect("non-empty") != q {
                return failed(libc::EINVAL);
            }
            &chars[1..chars.len() - 1]
        } else {
            &chars
        };
        let mut decoded = String::new();
        let mut i = 0;
        while i < body.len() {
            let c = body[i];
            if quote == Some(c) {
                return failed(libc::EINVAL);
            }
            if c == '\\' && quote != Some('\'') {
                i += 1;
                let Some(&next) = body.get(i) else { return failed(libc::EINVAL) };
                if quote == Some('"') && !matches!(next, '"' | '\\' | '$' | '`') {
                    decoded.push('\\');
                }
                decoded.push(next);
            } else {
                decoded.push(c);
            }
            i += 1;
        }
        result = if decoded.is_empty() { Attribute::default() } else { available(decoded) };
    }
    result
}

/// Kernel CPU list syntax (`0-3,8,10-11`) into actual CPU ids; any malformed input yields none.
pub fn parse_cpu_list(text: &str) -> Option<BTreeSet<u32>> {
    let mut parsed = BTreeSet::new();
    for range in text.split(',') {
        let limits: Vec<&str> = range.trim().split('-').collect();
        if limits.len() > 2 {
            return None;
        }
        let first: i64 = limits[0].parse().ok()?;
        let last: i64 = if limits.len() == 1 { first } else { limits[1].parse().ok()? };
        if first < 0 || last < first || last > 1_048_575 || last - first >= 4096 {
            return None;
        }
        parsed.extend((first..=last).map(|cpu| cpu as u32));
        if parsed.len() > 4096 {
            return None;
        }
    }
    (!parsed.is_empty()).then_some(parsed)
}

const MAX_MEMORY_DEVICES: u64 = 4096;
const MAX_DETAILED_MEMORY_DEVICES: u64 = 256;
const MAX_MEMORY_STRING: usize = 256;
const MAX_MEMORY_BYTES: usize = 1024 * 1024;
const MEMORY_SUFFIXES: [&str; 12] = [
    "SIZE",
    "PRESENT",
    "NON_VOLATILE_SIZE",
    "VOLATILE_SIZE",
    "LOCATOR",
    "BANK_LOCATOR",
    "TYPE",
    "FORM_FACTOR",
    "SPEED_MTS",
    "CONFIGURED_SPEED_MTS",
    "MANUFACTURER",
    "PART_NUMBER",
];

/// Installed RAM in bytes from the firmware-reported DMI properties.
pub fn parse_installed_memory(properties: &HashMap<String, String>) -> Attribute {
    let get = |key: &str| properties.get(key).map_or("", String::as_str);
    let Some(count) = properties.get("MEMORY_ARRAY_NUM_DEVICES") else { return Attribute::default() };
    let Some(count) = unsigned_decimal(count) else { return failed(libc::EINVAL) };
    if count > MAX_MEMORY_DEVICES {
        return failed(libc::EOVERFLOW);
    }
    let mut total: u64 = 0;
    for slot in 0..count {
        let prefix = format!("MEMORY_DEVICE_{slot}_");
        let present = get(&format!("{prefix}PRESENT"));
        let size = get(&format!("{prefix}SIZE"));
        if present == "0" {
            if !size.is_empty() && size != "0" {
                return failed(libc::EINVAL);
            }
            continue; // Explicitly empty slots do not require a capacity.
        }
        if !present.is_empty() && present != "1" {
            return failed(libc::EINVAL);
        }
        if size.is_empty() {
            return Attribute::default(); // Unknown slot invalidates the entire total.
        }
        let Some(bytes) = unsigned_decimal(size) else { return failed(libc::EINVAL) };
        if bytes == 0 {
            return Attribute::default();
        }
        let persistent = get(&format!("{prefix}NON_VOLATILE_SIZE"));
        if !persistent.is_empty() && persistent != "Unknown" {
            let Some(non_volatile) = unsigned_decimal(persistent) else { return failed(libc::EINVAL) };
            // SIZE may include persistent capacity; do not call that installed RAM.
            if non_volatile != 0 {
                return Attribute { state: ReadState::Unsupported, ..Default::default() };
            }
        }
        let Some(sum) = total.checked_add(bytes) else { return failed(libc::EOVERFLOW) };
        total = sum;
    }
    if total == 0 {
        Attribute::default()
    } else {
        available(total.to_string())
    }
}

/// Per-slot firmware records, keeping populated, unknown and explicitly empty slots apart.
pub fn parse_memory_devices(properties: &HashMap<String, String>) -> FirmwareMemoryInformation {
    let mut result = FirmwareMemoryInformation::default();
    let Some(count) = properties.get("MEMORY_ARRAY_NUM_DEVICES") else { return result };
    let Some(count) = unsigned_decimal(count) else {
        result.status = failed(libc::EINVAL);
        return result;
    };
    if count > MAX_DETAILED_MEMORY_DEVICES {
        result.status = failed(libc::EOVERFLOW);
        return result;
    }
    const PLACEHOLDERS: [&str; 6] = ["unknown", "not specified", "not provided", "none", "to be filled by o.e.m.", "default string"];
    let numeric = |key: &str| matches!(key, "SIZE" | "VOLATILE_SIZE" | "NON_VOLATILE_SIZE" | "SPEED_MTS" | "CONFIGURED_SPEED_MTS");
    let mut bytes = 0usize;
    for index in 0..count {
        let mut record = FirmwareMemoryDevice { index: index as usize, ..Default::default() };
        let prefix = format!("MEMORY_DEVICE_{index}_");
        for key in MEMORY_SUFFIXES {
            let raw = properties.get(&format!("{prefix}{key}")).map_or("", String::as_str);
            if raw.len() > MAX_MEMORY_STRING || bytes > MAX_MEMORY_BYTES - raw.len() {
                result.devices.clear();
                result.status = failed(libc::EOVERFLOW);
                return result;
            }
            bytes += raw.len();
            let text = raw.trim();
            let mut value = Attribute::default();
            if !text.is_empty() && !PLACEHOLDERS.contains(&text.to_lowercase().as_str()) {
                value = available(text);
                if key == "PRESENT" {
                    if text != "0" && text != "1" {
                        value = failed(libc::EINVAL);
                    }
                } else if numeric(key) {
                    match unsigned_decimal(text) {
                        None => value = failed(libc::EINVAL),
                        Some(0) if key == "SPEED_MTS" || key == "CONFIGURED_SPEED_MTS" => value = Attribute::default(),
                        Some(_) => {}
                    }
                }
            }
            record.values.insert(key.into(), value);
        }
        let present = record.values["PRESENT"].clone();
        if present.state == ReadState::Available && present.value == "0" {
            for key in ["SIZE", "VOLATILE_SIZE", "NON_VOLATILE_SIZE"] {
                let capacity = record.values.get_mut(key).expect("all suffixes inserted");
                let nonzero = capacity.state == ReadState::Available && capacity.value.parse::<u64>().unwrap_or(0) != 0;
                if capacity.state == ReadState::Error || nonzero {
                    *capacity = failed(libc::EINVAL);
                    record.values.insert("PRESENT".into(), failed(libc::EINVAL));
                } else {
                    *capacity = Attribute { state: ReadState::NotApplicable, ..Default::default() };
                }
            }
        } else {
            // A zero total capacity does not establish an empty device.
            let capacity = record.values.get_mut("SIZE").expect("inserted");
            if capacity.state == ReadState::Available && capacity.value.parse::<u64>().unwrap_or(1) == 0 {
                *capacity = Attribute::default();
            }
        }
        result.devices.push(record);
    }
    result.status = available(count.to_string());
    result
}

// ---------------------------------------------------------------------------------------
// Collection
// ---------------------------------------------------------------------------------------

fn installed_memory(result: &mut SystemProperties, cancel: Option<&AtomicBool>) {
    let cancelled = || cancel.is_some_and(|c| c.load(Ordering::Relaxed));
    let fail = |result: &mut SystemProperties, status: Attribute| {
        result.values.insert("physical_memory".into(), status.clone());
        result.memory_devices.status = status;
    };
    let device = match udev::Device::from_subsystem_sysname("dmi".into(), "id".into()) {
        Ok(d) => d,
        Err(e) => {
            let status = e.raw_os_error().map_or_else(Attribute::default, failed);
            return fail(result, status);
        }
    };
    let mut properties: HashMap<String, String> = HashMap::new();
    let mut copied = 0usize;
    let mut copy = |key: &str, limit: usize, properties: &mut HashMap<String, String>| -> bool {
        let Some(raw) = device.property_value(key) else { return true };
        let text = raw.to_string_lossy();
        if text.len() > limit || copied > MAX_MEMORY_BYTES - text.len() {
            return false;
        }
        copied += text.len();
        properties.insert(key.into(), text.into_owned());
        true
    };
    if !copy("MEMORY_ARRAY_NUM_DEVICES", 64, &mut properties) {
        return fail(result, failed(libc::EOVERFLOW));
    }
    let Some(count) = properties.get("MEMORY_ARRAY_NUM_DEVICES") else { return };
    let Some(count) = unsigned_decimal(count) else { return fail(result, failed(libc::EINVAL)) };
    if count > MAX_MEMORY_DEVICES {
        return fail(result, failed(libc::EOVERFLOW));
    }
    // Capacity and metadata share one udev snapshot. Optional metadata bounds must not
    // invalidate the installed-RAM calculation.
    for index in 0..count {
        if cancelled() {
            return fail(result, failed(libc::ECANCELED));
        }
        for suffix in ["SIZE", "PRESENT", "NON_VOLATILE_SIZE"] {
            if !copy(&format!("MEMORY_DEVICE_{index}_{suffix}"), 64, &mut properties) {
                return fail(result, failed(libc::EOVERFLOW));
            }
        }
    }
    result.values.insert("physical_memory".into(), parse_installed_memory(&properties));
    if count > MAX_DETAILED_MEMORY_DEVICES {
        result.memory_devices.status = failed(libc::EOVERFLOW);
        return;
    }
    for index in 0..count {
        if cancelled() {
            result.memory_devices.status = failed(libc::ECANCELED);
            return;
        }
        for suffix in MEMORY_SUFFIXES {
            let key = format!("MEMORY_DEVICE_{index}_{suffix}");
            if !properties.contains_key(&key) && !copy(&key, MAX_MEMORY_STRING, &mut properties) {
                result.memory_devices.status = failed(libc::EOVERFLOW);
                return;
            }
        }
    }
    result.memory_devices = parse_memory_devices(&properties);
}

fn gpu_resizable_bar(result: &mut SystemProperties, cancel: Option<&AtomicBool>) {
    let base = "/sys/bus/pci/devices";
    if let Err(e) = std::fs::read_dir(base) {
        result.gpu_rebar_status = failed(e.raw_os_error().unwrap_or(libc::EIO));
        return;
    }
    let devices = match bounded_directory_names(base, 4096, &[], cancel) {
        Ok(names) => names,
        Err(error) => {
            result.gpu_rebar_status = failed(error);
            return;
        }
    };
    let abort = |result: &mut SystemProperties, status: Attribute| {
        result.gpu_rebar.clear();
        result.gpu_rebar_status = status;
    };
    for name in &devices {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return abort(result, failed(libc::ECANCELED));
        }
        let path = format!("{base}/{name}");
        let Ok(before) = std::fs::metadata(&path) else {
            return abort(result, failed(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO)));
        };
        let class = read_text(&format!("{path}/class"), 32, cancel);
        let code = class.value.trim_start_matches("0x").pipe_hex();
        if class.state != ReadState::Available || code.is_none_or(|c| c > 0xff_ffff) {
            return abort(result, if class.state == ReadState::Available { failed(libc::EINVAL) } else { class });
        }
        if code.expect("checked") >> 16 != 0x03 {
            continue; // PCI display controllers only; no GPU-id database.
        }
        if result.gpu_rebar.len() >= 64 {
            return abort(result, failed(libc::EOVERFLOW));
        }
        let mut gpu = GpuResizableBar { pci_address: name.clone(), ..Default::default() };
        let mut resources = Attribute::default();
        let mut have_capability = false;
        for bar in 0..6usize {
            let bitmap_path = format!("{path}/resource{bar}_resize");
            if let Err(e) = std::fs::metadata(&bitmap_path) {
                if e.raw_os_error() == Some(libc::ENOENT) {
                    continue;
                }
                gpu.status = failed(e.raw_os_error().unwrap_or(libc::EIO));
                break;
            }
            let bitmap = read_text(&bitmap_path, 32, cancel);
            have_capability = true; // An existing sysfs attribute is positive evidence, even if unreadable.
            gpu.supported_sizes.insert(
                bar,
                match bitmap.state {
                    ReadState::Available => parse_resizable_bar_sizes(&bitmap.value),
                    ReadState::Unavailable => failed(libc::ENODATA),
                    _ => bitmap,
                },
            );
            if resources.state == ReadState::Unavailable && resources.error == 0 {
                resources = read_text(&format!("{path}/resource"), 65536, cancel);
            }
            gpu.current_sizes
                .insert(bar, if resources.state == ReadState::Available { parse_resizable_bar_allocation(&resources.value, bar) } else { resources.clone() });
        }
        if gpu.status.state == ReadState::Unavailable && gpu.status.error == 0 {
            gpu.status = if have_capability { available("reported") } else { Attribute::default() };
        }
        // Catch removal/replacement or allocation changes observed during collection.
        use std::os::unix::fs::MetadataExt;
        let mut stable = std::fs::metadata(&path).is_ok_and(|after| after.dev() == before.dev() && after.ino() == before.ino());
        if stable && have_capability {
            let final_resources = read_text(&format!("{path}/resource"), 65536, cancel);
            stable = resources == final_resources;
        }
        if stable && have_capability {
            for (bar, recorded) in &gpu.supported_sizes {
                let mut again = read_text(&format!("{path}/resource{bar}_resize"), 32, cancel);
                if again.state == ReadState::Available {
                    again = parse_resizable_bar_sizes(&again.value);
                } else if again.state == ReadState::Unavailable && again.error == 0 {
                    again = failed(libc::ENODATA);
                }
                if again != *recorded {
                    stable = false;
                }
            }
        }
        if !stable {
            gpu.status = failed(libc::EAGAIN);
            gpu.supported_sizes.clear();
            gpu.current_sizes.clear();
        }
        result.gpu_rebar.push(gpu);
    }
    match bounded_directory_names(base, 4096, &[], cancel) {
        Ok(after) if after == devices => {}
        Ok(_) => return abort(result, failed(libc::EAGAIN)),
        Err(error) => return abort(result, failed(error)),
    }
    result.gpu_rebar_status = available(result.gpu_rebar.len().to_string());
}

/// Hexadecimal text to a number (for sysfs `class`).
trait HexText {
    fn pipe_hex(&self) -> Option<u32>;
}
impl HexText for str {
    fn pipe_hex(&self) -> Option<u32> {
        u32::from_str_radix(self.trim(), 16).ok()
    }
}

fn cpu_topology(result: &mut SystemProperties, cancel: Option<&AtomicBool>) {
    let base = "/sys/devices/system/cpu";
    let present = read_text(&format!("{base}/present"), 4096, cancel);
    let online = read_text(&format!("{base}/online"), 4096, cancel);
    let present_cpus = (present.state == ReadState::Available).then(|| parse_cpu_list(&present.value)).flatten();
    let online_cpus = (online.state == ReadState::Available).then(|| parse_cpu_list(&online.value)).flatten();
    let count_or = |cpus: &Option<BTreeSet<u32>>, source: &Attribute| match cpus {
        Some(set) => available(set.len().to_string()),
        None if source.state == ReadState::Available => failed(libc::EINVAL),
        None => source.clone(),
    };
    result.values.insert("logical".into(), count_or(&present_cpus, &present));
    result.values.insert("online".into(), count_or(&online_cpus, &online));
    let mut packages: HashSet<String> = HashSet::new();
    let mut cores: HashSet<String> = HashSet::new();
    let mut topology_error = Attribute::default();
    let mut topology_complete = present_cpus.is_some();
    if let Some(cpus) = &present_cpus {
        for cpu in cpus {
            let path = format!("{base}/cpu{cpu}/topology/");
            let package = read_text(&format!("{path}physical_package_id"), 4096, cancel);
            let core = read_text(&format!("{path}core_id"), 4096, cancel);
            let die = read_text(&format!("{path}die_id"), 4096, cancel);
            let (Some(package_id), Some(core_id)) = (non_negative(&package), non_negative(&core)) else {
                topology_error = if package.state != ReadState::Available {
                    package
                } else if core.state != ReadState::Available {
                    core
                } else {
                    failed(libc::ENODATA)
                };
                topology_complete = false;
                break; // No apparently complete count from partial topology.
            };
            if matches!(die.state, ReadState::PermissionDenied | ReadState::Error) {
                topology_error = die;
                topology_complete = false;
                break;
            }
            packages.insert(package_id.to_string());
            let die_text = non_negative(&die).map_or_else(|| "unknown".to_string(), |d| d.to_string());
            cores.insert(format!("{package_id}:{die_text}:{core_id}"));
        }
    } else {
        topology_error = result.values["logical"].clone();
    }
    result.values.insert("sockets".into(), if topology_complete { available(packages.len().to_string()) } else { topology_error.clone() });
    result.values.insert("cores".into(), if topology_complete { available(cores.len().to_string()) } else { topology_error });
    for key in ["logical", "online", "sockets", "cores"] {
        result.sources.insert(key.into(), format!("{base}/present, online and cpu*/topology"));
    }

    let Some(active) = online_cpus else {
        result.values.insert("caches".into(), result.values["online"].clone());
        return;
    };
    // Deduplicate by level/type and canonical sharing set, never by CPU number.
    let mut seen: HashMap<String, u64> = HashMap::new();
    let mut totals: BTreeMap<String, (u64, u32)> = BTreeMap::new();
    let mut entries = 0i64;
    let mut cache_error = Attribute::default();
    let mut complete = true;
    'cpus: for cpu in &active {
        let path = format!("{base}/cpu{cpu}/cache");
        if let Err(e) = std::fs::read_dir(&path) {
            cache_error = failed(e.raw_os_error().unwrap_or(libc::EIO));
            complete = false;
            break;
        }
        let indexes = match bounded_directory_names(&path, 16384 - entries, &["index*"], cancel) {
            Ok(names) => names,
            Err(error) => {
                cache_error = failed(error);
                complete = false;
                break;
            }
        };
        if indexes.is_empty() {
            complete = false;
            break;
        }
        for index in indexes {
            entries += 1;
            let prefix = format!("{path}/{index}/");
            let level = read_text(&format!("{prefix}level"), 4096, cancel);
            let kind = read_text(&format!("{prefix}type"), 4096, cancel);
            let size = read_text(&format!("{prefix}size"), 4096, cancel);
            let sharing = read_text(&format!("{prefix}shared_cpu_list"), 4096, cancel);
            if let Some(bad) = [&level, &kind, &size, &sharing].into_iter().find(|v| v.state != ReadState::Available) {
                cache_error = bad.clone();
                complete = false;
                break 'cpus;
            }
            let (mut size_text, mut scale) = (size.value.as_str(), 1u64);
            if let Some(rest) = size_text.strip_suffix('K') {
                (size_text, scale) = (rest, 1024);
            } else if let Some(rest) = size_text.strip_suffix('M') {
                (size_text, scale) = (rest, 1024 * 1024);
            }
            let amount = size_text.parse::<u64>().ok();
            let level_number = non_negative(&level).filter(|l| *l != 0);
            let siblings = parse_cpu_list(&sharing.value);
            let valid = level_number.is_some()
                && amount.is_some_and(|a| a != 0 && a <= u64::MAX / scale)
                && siblings.as_ref().is_some_and(|s| s.contains(cpu))
                && matches!(kind.value.as_str(), "Data" | "Instruction" | "Unified");
            if !valid {
                cache_error = failed(libc::EINVAL);
                complete = false;
                break 'cpus;
            }
            let ids: Vec<String> = siblings.expect("valid").iter().map(u32::to_string).collect();
            let label = format!("L{} {}", level_number.expect("valid"), kind.value);
            let key = format!("{label}:{}", ids.join(","));
            let bytes = amount.expect("valid") * scale;
            if let Some(previous) = seen.get(&key) {
                if *previous != bytes {
                    cache_error = failed(libc::EAGAIN);
                    complete = false;
                    break 'cpus;
                }
            } else {
                seen.insert(key, bytes);
                let total = totals.entry(label).or_default();
                if bytes > i64::MAX as u64 || total.0 > i64::MAX as u64 - bytes {
                    cache_error = failed(libc::EOVERFLOW);
                    complete = false;
                    break 'cpus;
                }
                total.0 += bytes;
                total.1 += 1;
            }
        }
    }
    let lines: Vec<String> = totals.iter().map(|(kind, (bytes, count))| format!("{kind}\t{bytes}\t{count}")).collect();
    result.values.insert("caches".into(), if complete && !lines.is_empty() { available(lines.join("\n")) } else { cache_error });
    result.sources.insert("caches".into(), format!("{base}/cpu*/cache/index*/{{level,type,size,shared_cpu_list}}; online CPUs only"));
    // Detect observed hotplug during collection rather than publish mixed counts.
    if read_text(&format!("{base}/present"), 4096, cancel).value != present.value || read_text(&format!("{base}/online"), 4096, cancel).value != online.value {
        for key in ["logical", "online", "sockets", "cores", "caches"] {
            result.values.insert(key.into(), failed(libc::EAGAIN));
        }
    }
}

/// The machine's host name, or empty if unavailable.
pub fn machine_hostname() -> String {
    hostname().value
}

fn hostname() -> Attribute {
    let mut buffer = [0u8; 256];
    // SAFETY: the buffer is valid for its full length and gethostname NUL-terminates within it.
    let status = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len() - 1) };
    if status != 0 {
        return dir_failure(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO));
    }
    let length = buffer.iter().position(|b| *b == 0).unwrap_or(buffer.len());
    let name = String::from_utf8_lossy(&buffer[..length]).into_owned();
    if name.is_empty() {
        Attribute::default()
    } else {
        available(name)
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn cpuid_hypervisor() -> Option<String> {
    #[cfg(target_arch = "x86")]
    use std::arch::x86::{__cpuid, __get_cpuid_max};
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::{__cpuid, __get_cpuid_max};
    // CPUID is available on every x86 CPU this program can run on; the max-leaf query checks
    // that leaf 1 exists first, and the instructions only read processor identification.
    let (max, _) = __get_cpuid_max(0);
    if max < 1 || __cpuid(1).ecx & (1 << 31) == 0 {
        return None;
    }
    let leaf = __cpuid(0x4000_0000);
    if leaf.eax < 0x4000_0000 || leaf.eax >= 0x4001_0000 {
        return None;
    }
    let mut vendor = Vec::new();
    for register in [leaf.ebx, leaf.ecx, leaf.edx] {
        vendor.extend_from_slice(&register.to_le_bytes());
    }
    let name: String = String::from_utf8_lossy(&vendor).chars().filter(|c| *c != '\0').collect();
    Some(name.trim().to_string())
}

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
fn cpuid_hypervisor() -> Option<String> {
    None
}

pub fn collect_system_properties(cancel: Option<&AtomicBool>) -> SystemProperties {
    let mut result = SystemProperties::default();
    macro_rules! put {
        ($key:expr, $value:expr, $source:expr) => {{
            result.values.insert($key.to_string(), $value);
            result.sources.insert($key.to_string(), $source.to_string());
        }};
    }
    put!("hostname", hostname(), "gethostname");
    let mut os_path = "/etc/os-release";
    let mut os = read_text(os_path, 65536, cancel);
    if os.error == libc::ENOENT {
        os_path = "/usr/lib/os-release";
        os = read_text(os_path, 65536, cancel);
    }
    put!("os", if os.state == ReadState::Available { parse_os_name(&os.value) } else { os }, format!("{os_path}: PRETTY_NAME"));
    // SAFETY: utsname is plain data filled by uname.
    let kernel = unsafe {
        let mut name: libc::utsname = std::mem::zeroed();
        (libc::uname(&mut name) == 0).then_some(name)
    };
    let text = |field: &[libc::c_char]| {
        // SAFETY: utsname fields are NUL-terminated C strings.
        unsafe { std::ffi::CStr::from_ptr(field.as_ptr()) }.to_string_lossy().into_owned()
    };
    match kernel {
        Some(name) => {
            put!("architecture", available(text(&name.machine)), "uname.machine (kernel architecture)");
            put!("kernel", available(format!("{} {}", text(&name.sysname), text(&name.release))), "uname.sysname, release");
            put!("build", available(text(&name.version)), "uname.version");
        }
        None => {
            let error = dir_failure(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO));
            for key in ["architecture", "kernel", "build"] {
                put!(key, error.clone(), "uname");
            }
        }
    }
    let cpu = read_cpuinfo();
    let mut models: Vec<String> = parse_cpu_models(&cpu.value).into_values().collect();
    models.sort_by_key(|m| m.to_lowercase());
    models.dedup();
    put!(
        "cpu",
        if cpu.state == ReadState::Available {
            if models.is_empty() {
                Attribute::default()
            } else {
                available(models.join("\n"))
            }
        } else {
            cpu.clone()
        },
        "/proc/cpuinfo: per-processor model fields"
    );
    put!(
        "instruction_sets",
        if cpu.state == ReadState::Available { parse_cpu_instruction_sets(&cpu.value) } else { cpu.clone() },
        "/proc/cpuinfo: per-processor flags, Features or isa; kernel-exposed instruction sets and CPU features"
    );
    gpu_resizable_bar(&mut result, cancel);
    result.sources.insert(
        "gpu_rebar".into(),
        "/sys/bus/pci/devices: display-controller class, resourceN_resize supported-size bitmaps, resource allocation metadata; BIOS setting is not exposed"
            .into(),
    );
    cpu_topology(&mut result, cancel);
    installed_memory(&mut result, cancel);
    result
        .sources
        .insert("physical_memory".into(), "libudev: dmi/id MEMORY_ARRAY_NUM_DEVICES and MEMORY_DEVICE_n_SIZE/PRESENT; firmware-reported installed RAM".into());
    result.sources.insert("memory_devices".into(), "libudev: dmi/id cached MEMORY_ARRAY_NUM_DEVICES and MEMORY_DEVICE_n_* properties".into());
    let memory = read_text("/proc/meminfo", 65536, cancel);
    let mut total = if memory.state == ReadState::Available { Attribute::default() } else { memory.clone() };
    if let Some(line) = memory.value.split('\n').find(|l| l.starts_with("MemTotal:")) {
        let parts: Vec<&str> = line["MemTotal:".len()..].split_whitespace().collect();
        total = match parts.as_slice() {
            [kib, "kB"] => match kib.parse::<u64>().ok().filter(|k| *k != 0 && *k <= u64::MAX / 1024) {
                Some(kib) => available((kib * 1024).to_string()),
                None => failed(libc::EINVAL),
            },
            _ => failed(libc::EINVAL),
        };
    }
    put!("memory", total, "/proc/meminfo: MemTotal; usable physical RAM, not installed DIMM capacity");
    let uptime = read_text("/proc/uptime", 4096, cancel);
    let seconds = uptime.value.split(' ').next().and_then(|s| s.parse::<f64>().ok());
    put!(
        "uptime",
        if uptime.state != ReadState::Available {
            uptime.clone()
        } else {
            match seconds.filter(|s| s.is_finite() && *s >= 0.0 && *s < 1e12) {
                Some(s) => available((s as u64).to_string()),
                None => failed(libc::EINVAL),
            }
        },
        "/proc/uptime"
    );
    for (key, file) in [
        ("manufacturer", "sys_vendor"),
        ("model", "product_name"),
        ("board_vendor", "board_vendor"),
        ("board_model", "board_name"),
        ("firmware", "bios_version"),
        ("firmware_date", "bios_date"),
    ] {
        let path = format!("/sys/class/dmi/id/{file}");
        put!(key, read_text(&path, 4096, cancel), path);
    }
    let boot = match std::fs::metadata("/sys/firmware/efi") {
        Ok(m) if m.is_dir() => available("uefi"),
        Ok(_) => failed(libc::ENOTDIR),
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => available("not-exposed"),
        Err(e) => failed(e.raw_os_error().unwrap_or(libc::EIO)),
    };
    put!("boot", boot, "/sys/firmware/efi directory presence");
    let secure_boot = collect_secure_boot_information();
    put!("secure_boot", secure_boot.state, secure_boot.state_source);
    result.mok = secure_boot.mok;
    result.sources.insert("mok".into(), secure_boot.mok_source);
    let mut hypervisor = read_text("/sys/hypervisor/type", 4096, cancel);
    let mut hypervisor_source = "/sys/hypervisor/type".to_string();
    if hypervisor.state != ReadState::Available {
        if let Some(name) = cpuid_hypervisor() {
            hypervisor = if name.is_empty() { Attribute::default() } else { available(name) };
            hypervisor_source = "CPUID hypervisor-present bit and vendor leaf 0x40000000".into();
        }
    }
    put!("virtualization", hypervisor, hypervisor_source);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn os_names_decode_quotes_without_evaluating_anything() {
        assert_eq!(parse_os_name("PRETTY_NAME=\"Example Linux 2\"\n").value, "Example Linux 2");
        assert_eq!(parse_os_name("PRETTY_NAME='Example $literal'\n").value, "Example $literal");
        assert_eq!(parse_os_name("PRETTY_NAME=\"Escaped \\\"quote\\\" and \\$dollar\"\n").value, "Escaped \"quote\" and $dollar");
        assert_eq!(parse_os_name("PRETTY_NAME=\"$(must-not-run)\"\n").value, "$(must-not-run)");
        assert_eq!(parse_os_name("PRETTY_NAME=\"unterminated\n").state, ReadState::Error);
        assert_eq!(parse_os_name("NAME=Example\n").value, "Linux");
        assert_eq!(parse_os_name("PRETTY_NAME=First\nPRETTY_NAME=Second\n").value, "Second");
        assert_eq!(parse_os_name("PRETTY_NAME=\"a\\x\"\n").value, "a\\x", "unlisted escapes keep the backslash in double quotes");
    }

    #[test]
    fn cpu_lists_keep_real_ids_and_reject_bad_input_whole() {
        let cpus = parse_cpu_list("0-3,8,10-11").unwrap();
        assert!(cpus.len() == 7 && cpus.contains(&11) && !cpus.contains(&9));
        assert!(parse_cpu_list("3-1").is_none());
        assert!(parse_cpu_list("0-500000").is_none(), "expansion is bounded");
        for bad in ["0,", "-1", "word", "", "1-2-3"] {
            assert!(parse_cpu_list(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn instruction_sets_group_identical_processors_and_never_union() {
        assert_eq!(parse_cpu_instruction_sets("processor : 0\nflags : sse avx2 sse\n\nprocessor : 1\nflags : avx2 sse\n").value, "0, 1\tavx2 sse");
        let mixed = parse_cpu_instruction_sets("processor : 0\nflags : sse avx2\n\nprocessor : 1\nflags : sse\n");
        assert!(mixed.value.contains("0\tavx2 sse") && mixed.value.contains("1\tsse"));
        assert_eq!(parse_cpu_instruction_sets("processor : 0\nFeatures : aes asimd\n").value, "0\taes asimd");
        assert_eq!(parse_cpu_instruction_sets("processor : 0\nisa : rv64imafdc\n").value, "0\trv64imafdc");
        assert_eq!(parse_cpu_instruction_sets("processor : 0\nflags : sse\n\nprocessor : 1\nmodel name : Example\n").state, ReadState::Unavailable);
        assert_eq!(parse_cpu_instruction_sets("processor : 0\nflags : sse\n\nprocessor : 0\nflags : avx\n").error, libc::EINVAL);
        assert_eq!(parse_cpu_instruction_sets("Hardware : Board\nFeatures : aes\n").state, ReadState::Unavailable);
        assert_eq!(parse_cpu_instruction_sets(&"x".repeat(4 * 1024 * 1024 + 1)).error, libc::EOVERFLOW);
    }

    #[test]
    fn resizable_bar_bitmaps_and_allocations() {
        assert_eq!(parse_resizable_bar_sizes("00000000000001c0").value, "1c0");
        assert_eq!(parse_resizable_bar_sizes("100").value, "100");
        assert_eq!(parse_resizable_bar_sizes("0").error, libc::EINVAL);
        assert_eq!(parse_resizable_bar_sizes("wrong").error, libc::EINVAL);
        assert_eq!(parse_resizable_bar_sizes("100000000000").error, libc::EOVERFLOW);
        assert_eq!(parse_resizable_bar_allocation("0000009800000000 000000980fffffff 000000000014220c", 0).value, "268435456");
        for unavailable in ["0 0 200", "0 ffffffff 20000200", "100 1ff 100"] {
            assert_eq!(parse_resizable_bar_allocation(unavailable, 0).state, ReadState::Unavailable, "{unavailable}");
        }
        assert_eq!(parse_resizable_bar_allocation("200 100 200", 0).error, libc::EINVAL);
        assert_eq!(parse_resizable_bar_allocation("garbage", 0).error, libc::EINVAL);
        assert_eq!(parse_resizable_bar_allocation("0 ffffffffffffffff 200", 0).error, libc::EOVERFLOW);
        assert_eq!(parse_resizable_bar_allocation("0 0 0", 6).error, libc::EINVAL);
    }

    #[test]
    fn installed_memory_requires_every_slot_to_be_known() {
        let mut memory = map(&[("MEMORY_ARRAY_NUM_DEVICES", "2"), ("MEMORY_DEVICE_0_SIZE", "17179869184"), ("MEMORY_DEVICE_1_SIZE", "17179869184")]);
        assert_eq!(parse_installed_memory(&memory).value, "34359738368");
        memory.insert("MEMORY_ARRAY_NUM_DEVICES".into(), "3".into());
        memory.insert("MEMORY_DEVICE_2_PRESENT".into(), "0".into());
        assert_eq!(parse_installed_memory(&memory).value, "34359738368", "an explicitly empty slot is fine");
        memory.remove("MEMORY_DEVICE_2_PRESENT");
        assert_eq!(parse_installed_memory(&memory).state, ReadState::Unavailable, "unknown slot, no partial total");
        memory.insert("MEMORY_DEVICE_2_SIZE".into(), "garbage".into());
        assert_eq!(parse_installed_memory(&memory).state, ReadState::Error);
        memory.insert("MEMORY_DEVICE_2_SIZE".into(), "18446744073709551615".into());
        assert_eq!(parse_installed_memory(&memory).error, libc::EOVERFLOW);
        memory.insert("MEMORY_DEVICE_2_PRESENT".into(), "0".into());
        assert_eq!(parse_installed_memory(&memory).state, ReadState::Error, "a size on an empty slot conflicts");
        memory.remove("MEMORY_DEVICE_2_SIZE");
        memory.insert("MEMORY_DEVICE_0_NON_VOLATILE_SIZE".into(), "0".into());
        memory.insert("MEMORY_DEVICE_1_NON_VOLATILE_SIZE".into(), "Unknown".into());
        assert_eq!(parse_installed_memory(&memory).value, "34359738368");
        memory.insert("MEMORY_DEVICE_0_NON_VOLATILE_SIZE".into(), "17179869184".into());
        assert_eq!(parse_installed_memory(&memory).state, ReadState::Unsupported, "persistent memory is not RAM");
        memory.insert("MEMORY_ARRAY_NUM_DEVICES".into(), "4097".into());
        assert_eq!(parse_installed_memory(&memory).error, libc::EOVERFLOW);
        assert_eq!(parse_installed_memory(&HashMap::new()).state, ReadState::Unavailable, "missing firmware data is not zero RAM");
    }

    #[test]
    fn firmware_memory_records_stay_separate_and_explicit() {
        let mut records = map(&[
            ("MEMORY_ARRAY_NUM_DEVICES", "3"),
            ("MEMORY_DEVICE_0_SIZE", "17179869184"),
            ("MEMORY_DEVICE_0_LOCATOR", "Same locator"),
            ("MEMORY_DEVICE_1_LOCATOR", "Same locator"),
            ("MEMORY_DEVICE_0_TYPE", "LPDDR5"),
            ("MEMORY_DEVICE_0_SPEED_MTS", "8533"),
            ("MEMORY_DEVICE_0_CONFIGURED_SPEED_MTS", "0"),
            ("MEMORY_DEVICE_0_PART_NUMBER", "Unknown"),
            ("MEMORY_DEVICE_1_SIZE", "Unknown"),
            ("MEMORY_DEVICE_2_PRESENT", "0"),
        ]);
        let details = parse_memory_devices(&records);
        assert!(details.status.state == ReadState::Available && details.devices.len() == 3);
        let value = |d: &FirmwareMemoryInformation, i: usize, k: &str| d.devices[i].values[k].clone();
        assert_eq!(value(&details, 0, "PRESENT").state, ReadState::Unavailable);
        assert_eq!(value(&details, 0, "SIZE").value, "17179869184", "a missing presence flag keeps the known capacity");
        assert_eq!(value(&details, 0, "LOCATOR").value, value(&details, 1, "LOCATOR").value, "duplicate locators do not merge");
        assert_eq!(value(&details, 1, "SIZE").state, ReadState::Unavailable);
        assert_eq!(value(&details, 2, "SIZE").state, ReadState::NotApplicable, "unknown differs from explicitly empty");
        assert_eq!(value(&details, 0, "SPEED_MTS").value, "8533");
        assert_eq!(value(&details, 0, "CONFIGURED_SPEED_MTS").state, ReadState::Unavailable, "zero rate");
        assert_eq!(value(&details, 0, "PART_NUMBER").state, ReadState::Unavailable, "firmware placeholder");

        records.insert("MEMORY_DEVICE_0_NON_VOLATILE_SIZE".into(), "1073741824".into());
        records.insert("MEMORY_DEVICE_0_VOLATILE_SIZE".into(), "16106127360".into());
        records.insert("MEMORY_DEVICE_1_SPEED_MTS".into(), "bad".into());
        records.insert("MEMORY_DEVICE_2_SIZE".into(), "1024".into());
        let details = parse_memory_devices(&records);
        assert_eq!(value(&details, 0, "NON_VOLATILE_SIZE").value, "1073741824");
        assert_eq!(value(&details, 0, "VOLATILE_SIZE").value, "16106127360");
        assert_eq!(value(&details, 1, "SPEED_MTS").error, libc::EINVAL);
        assert_eq!(value(&details, 2, "SIZE").error, libc::EINVAL);
        assert_eq!(value(&details, 2, "PRESENT").error, libc::EINVAL, "contradictory empty record");

        records.insert("MEMORY_DEVICE_0_MANUFACTURER".into(), "x".repeat(257));
        let oversized = parse_memory_devices(&records);
        assert!(oversized.status.error == libc::EOVERFLOW && oversized.devices.is_empty(), "no partial list");
        records.remove("MEMORY_DEVICE_0_MANUFACTURER");
        records.insert("MEMORY_ARRAY_NUM_DEVICES".into(), "257".into());
        assert_eq!(parse_memory_devices(&records).status.error, libc::EOVERFLOW);
        assert_eq!(parse_memory_devices(&HashMap::new()).status.state, ReadState::Unavailable, "missing cache is not a permission failure");
        let zero = parse_memory_devices(&map(&[("MEMORY_ARRAY_NUM_DEVICES", "0")]));
        assert!(zero.status.state == ReadState::Available && zero.devices.is_empty());
    }

    #[test]
    fn this_machines_system_information_is_complete_and_explicit() {
        let found = collect_system_properties(None);
        let value = |k: &str| found.values[k].clone();
        assert_eq!(value("kernel").state, ReadState::Available);
        assert!(value("kernel").value.starts_with("Linux "));
        assert_eq!(value("os").state, ReadState::Available);
        assert!(!value("os").value.is_empty());
        assert!(value("memory").value.parse::<u64>().unwrap() > 0);
        assert!(value("uptime").value.parse::<u64>().unwrap() > 0);
        assert!(value("logical").value.parse::<usize>().unwrap() >= 1);
        assert!(matches!(value("boot").value.as_str(), "uefi" | "not-exposed"));
        assert!(found.values["cpu"].state == ReadState::Available && found.values["instruction_sets"].value.contains('\t'));
        assert!(found.sources["memory"].contains("MemTotal"));
        assert!(matches!(found.gpu_rebar_status.state, ReadState::Available | ReadState::Unavailable | ReadState::Error | ReadState::PermissionDenied));
        if found.values["caches"].state == ReadState::Available {
            assert!(found.values["caches"].value.lines().all(|l| l.split('\t').count() == 3));
        }
        let cancelled = AtomicBool::new(true);
        assert_eq!(collect_system_properties(Some(&cancelled)).values["os"].error, libc::ECANCELED);
    }
}
