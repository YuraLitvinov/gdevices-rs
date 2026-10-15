//! Device resources (PCI BARs, PnP assignments, MSI vectors), ported from
//! `src/devices/deviceresources.cpp`. Raw kernel evidence only: overlap and BUSY are never
//! interpreted as conflicts, and a malformed table is rejected whole rather than shifted.
use crate::model::{Attribute, Device, ReadState};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicBool, Ordering};

// Stable userspace resource flag values: Linux include/linux/ioport.h.
const TYPE_MASK: u64 = 0x0000_1f00;
const IO: u64 = 0x0000_0100;
const MEMORY: u64 = 0x0000_0200;
pub const WINDOW: u64 = 0x0020_0000;
const DISABLED: u64 = 0x1000_0000;
const UNSET: u64 = 0x2000_0000;
const INPUT_LIMIT: usize = 65536;
const ROW_LIMIT: usize = 256;
const VECTOR_LIMIT: usize = 4096;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ResourceType {
    #[default]
    Memory,
    Io,
    Irq,
    Dma,
    Bus,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Allocation {
    #[default]
    Assigned,
    Unassigned,
    Disabled,
    Unavailable,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceResource {
    pub kind: ResourceType,
    pub allocation: Allocation,
    pub start: u64,
    pub end: u64,
    pub flags: u64,
    /// PCI resource slot, not necessarily a BAR; `-1` when not a PCI table row.
    pub index: i32,
    pub pci: bool,
    /// MSI/MSI-X or reported irq; raw kernel evidence only.
    pub mode: String,
    pub source: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceIssue {
    pub source: String,
    pub error: Attribute,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceResources {
    pub items: Vec<DeviceResource>,
    pub issues: Vec<ResourceIssue>,
    pub pnp_state: String,
}

impl DeviceResources {
    pub fn has_information(&self) -> bool {
        !self.items.is_empty() || !self.issues.is_empty()
    }
}

fn resource(kind: ResourceType) -> DeviceResource {
    DeviceResource { kind, index: -1, ..Default::default() }
}

fn failure(error: i32) -> Attribute {
    let state = match error {
        libc::EACCES | libc::EPERM => ReadState::PermissionDenied,
        libc::ENODEV => ReadState::Removed,
        libc::ENOENT => ReadState::Unavailable,
        _ => ReadState::Error,
    };
    Attribute { state, value: String::new(), error }
}

fn issue(result: &mut DeviceResources, source: &str, error: i32) {
    result.issues.push(ResourceIssue { source: source.to_string(), error: failure(error) });
}

fn reject(result: &mut DeviceResources, source: &str) {
    result.items.clear();
    issue(result, source, libc::EINVAL);
}

/// `^(?:0[xX])?[0-9a-fA-F]{1,16}$`
fn hex_number(text: &str) -> Option<u64> {
    let digits = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")).unwrap_or(text);
    if (1..=16).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        u64::from_str_radix(digits, 16).ok()
    } else {
        None
    }
}

/// `^[0-9]{1,10}$`, at most 32 bits.
fn decimal_number(text: &str) -> Option<u64> {
    if (1..=10).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_digit()) {
        text.parse::<u64>().ok().filter(|v| *v <= 0xffff_ffff)
    } else {
        None
    }
}

pub fn parse_pci_resources(text: &str, source: &str) -> DeviceResources {
    let mut result = DeviceResources::default();
    if text.len() > INPUT_LIMIT {
        issue(&mut result, source, libc::EOVERFLOW);
        return result;
    }
    let lines: Vec<&str> = text.split('\n').collect();
    let mut index = 0usize;
    for (line, record) in lines.iter().enumerate() {
        let record = record.trim();
        if record.is_empty() && line == lines.len() - 1 {
            continue;
        }
        if index >= ROW_LIMIT {
            issue(&mut result, source, libc::EOVERFLOW);
            break;
        }
        let fields: Vec<&str> = record.split_whitespace().collect();
        let parsed = match fields[..] {
            [a, b, c] => hex_number(a).zip(hex_number(b)).zip(hex_number(c)).map(|((s, e), f)| (s, e, f)),
            _ => None,
        };
        let Some((start, end, flags)) = parsed else {
            // Do not shift slot numbers or invent ranges after malformed input.
            reject(&mut result, source);
            return result;
        };
        let slot = index as i32;
        index += 1;
        if start == 0 && end == 0 && flags == 0 {
            continue; // Empty resource slot.
        }
        let kind = match flags & TYPE_MASK {
            IO => ResourceType::Io,
            MEMORY => ResourceType::Memory,
            _ => continue,
        };
        let mut r = DeviceResource { pci: true, index: slot, start, end, flags, source: source.to_string(), ..resource(kind) };
        if flags & DISABLED != 0 {
            r.allocation = Allocation::Disabled;
        } else if flags & UNSET != 0 {
            r.allocation = Allocation::Unassigned;
        } else if start == 0 && end == 0 {
            r.allocation = Allocation::Unavailable;
        } else if end < start {
            reject(&mut result, source);
            return result;
        }
        result.items.push(r);
    }
    result
}

/// `^(0[xX][0-9a-fA-F]+|0)-(0[xX][0-9a-fA-F]+|0)( window)?$`
fn pnp_range(value: &str) -> Option<(u64, u64, bool)> {
    let (range, window) = match value.strip_suffix(" window") {
        Some(range) => (range, true),
        None => (value, false),
    };
    let (a, b) = range.split_once('-')?;
    let shape = |s: &str| s == "0" || (s.len() > 2 && (s.starts_with("0x") || s.starts_with("0X")));
    (shape(a) && shape(b)).then_some(())?;
    Some((hex_number(a)?, hex_number(b)?, window))
}

pub fn parse_pnp_resources(text: &str, source: &str) -> DeviceResources {
    let mut result = DeviceResources::default();
    if text.len() > INPUT_LIMIT {
        issue(&mut result, source, libc::EOVERFLOW);
        return result;
    }
    let lines: Vec<&str> = text.trim().split('\n').collect();
    if lines[0] != "state = active" && lines[0] != "state = disabled" {
        issue(&mut result, source, libc::EINVAL);
        return result;
    }
    result.pnp_state = lines[0][8..].to_string();
    for (line, text) in lines.iter().enumerate().skip(1) {
        if line > ROW_LIMIT {
            issue(&mut result, source, libc::EOVERFLOW);
            break;
        }
        let Some((kind, value)) = text.trim().split_once(' ').filter(|(k, v)| matches!(*k, "io" | "mem" | "irq" | "dma" | "bus") && !v.is_empty()) else {
            reject(&mut result, source);
            return result;
        };
        let kind = match kind {
            "io" => ResourceType::Io,
            "mem" => ResourceType::Memory,
            "irq" => ResourceType::Irq,
            "dma" => ResourceType::Dma,
            _ => ResourceType::Bus,
        };
        let mut r = DeviceResource { source: source.to_string(), ..resource(kind) };
        if value == "disabled" {
            r.allocation = Allocation::Disabled;
        } else if matches!(kind, ResourceType::Irq | ResourceType::Dma) {
            if value == "-1" {
                r.allocation = Allocation::Unassigned;
            } else if let Some(number) = decimal_number(value) {
                r.start = number;
            } else {
                reject(&mut result, source);
                return result;
            }
            r.end = r.start;
        } else {
            match pnp_range(value).filter(|(s, e, _)| e >= s) {
                Some((start, end, window)) => {
                    (r.start, r.end) = (start, end);
                    if window {
                        r.flags |= WINDOW;
                    }
                    if start == 0 && end == 0 {
                        r.allocation = Allocation::Unavailable;
                    }
                }
                None => {
                    reject(&mut result, source);
                    return result;
                }
            }
        }
        if result.pnp_state == "disabled" {
            r.allocation = Allocation::Disabled;
        }
        result.items.push(r);
    }
    result
}

/// Opens `name` inside an already-open directory without following a final symlink. Going
/// through the directory's own descriptor keeps a swapped path from redirecting the read.
fn open_in(dir: &File, name: &str, flags: i32) -> std::io::Result<File> {
    OpenOptions::new().read(true).custom_flags(flags | libc::O_CLOEXEC | libc::O_NOFOLLOW).open(format!("/proc/self/fd/{}/{name}", dir.as_raw_fd()))
}

fn read_metadata(dir: &File, name: &str, limit: usize, cancel: Option<&AtomicBool>) -> Attribute {
    let mut file = match open_in(dir, name, 0) {
        Ok(f) => f,
        Err(e) => return failure(e.raw_os_error().unwrap_or(libc::EIO)),
    };
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 4096];
    while bytes.len() <= limit {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return failure(libc::ECANCELED);
        }
        let wanted = buffer.len().min(limit + 1 - bytes.len());
        match file.read(&mut buffer[..wanted]) {
            Ok(0) => break,
            Ok(n) => bytes.extend_from_slice(&buffer[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return failure(e.raw_os_error().unwrap_or(libc::EIO)),
        }
    }
    if bytes.len() > limit {
        return failure(libc::EOVERFLOW);
    }
    Attribute { state: ReadState::Available, value: String::from_utf8_lossy(&bytes).into_owned(), error: 0 }
}

fn record_read_error(result: &mut DeviceResources, source: &str, value: Attribute) {
    if value.state != ReadState::Available && value.state != ReadState::Unavailable {
        result.issues.push(ResourceIssue { source: source.to_string(), error: value });
    }
}

/// Collects PCI or PnP resources for `device` from its open sysfs directory. Other
/// subsystems return nothing: no parent resources are guessed.
pub fn collect_device_resources(dir: &File, device: &Device, cancel: Option<&AtomicBool>) -> DeviceResources {
    let mut result = DeviceResources::default();
    let cancelled = || cancel.is_some_and(|c| c.load(Ordering::Relaxed));
    let append = |result: &mut DeviceResources, parsed: DeviceResources| {
        result.items.extend(parsed.items);
        result.issues.extend(parsed.issues);
        result.pnp_state = parsed.pnp_state;
    };
    if device.subsystem == "pnp" {
        let data = read_metadata(dir, "resources", INPUT_LIMIT, cancel);
        let source = format!("{}/resources", device.path);
        if data.state == ReadState::Available {
            append(&mut result, parse_pnp_resources(&data.value, &source));
        } else {
            record_read_error(&mut result, &source, data);
        }
        return result;
    }
    if device.subsystem != "pci" {
        return result;
    }
    let ranges = read_metadata(dir, "resource", INPUT_LIMIT, cancel);
    let source = format!("{}/resource", device.path);
    if ranges.state == ReadState::Available {
        append(&mut result, parse_pci_resources(&ranges.value, &source));
    } else {
        record_read_error(&mut result, &source, ranges);
    }

    let msi_source = format!("{}/msi_irqs", device.path);
    let mut msi_observed = false;
    match open_in(dir, "msi_irqs", libc::O_DIRECTORY) {
        Ok(msi) => {
            let mut vectors = Vec::new();
            let mut examined = 0;
            match fs::read_dir(format!("/proc/self/fd/{}", msi.as_raw_fd())) {
                Err(e) => issue(&mut result, &msi_source, e.raw_os_error().unwrap_or(libc::EIO)),
                Ok(entries) => {
                    for entry in entries {
                        if cancelled() {
                            break;
                        }
                        let entry = match entry {
                            Ok(e) => e,
                            Err(e) => {
                                issue(&mut result, &msi_source, e.raw_os_error().unwrap_or(libc::EIO));
                                break;
                            }
                        };
                        examined += 1;
                        if examined > VECTOR_LIMIT {
                            issue(&mut result, &msi_source, libc::EOVERFLOW);
                            break;
                        }
                        let filename = entry.file_name().to_string_lossy().into_owned();
                        let Some(number) = decimal_number(&filename).filter(|n| *n != 0) else {
                            issue(&mut result, &msi_source, libc::EINVAL);
                            continue;
                        };
                        msi_observed = true;
                        let mode = read_metadata(&msi, &filename, 32, cancel);
                        let vector_source = format!("{msi_source}/{filename}");
                        if mode.state != ReadState::Available {
                            // Even a vanished vector is an explicit incomplete observation.
                            result.issues.push(ResourceIssue { source: vector_source, error: mode });
                            continue;
                        }
                        let value = mode.value.trim();
                        if value != "msi" && value != "msix" {
                            issue(&mut result, &vector_source, libc::EINVAL);
                            continue;
                        }
                        vectors.push(DeviceResource {
                            start: number,
                            end: number,
                            mode: value.to_string(),
                            source: vector_source,
                            ..resource(ResourceType::Irq)
                        });
                    }
                }
            }
            vectors.sort_by_key(|v| v.start);
            result.items.extend(vectors);
        }
        Err(e) if e.raw_os_error() != Some(libc::ENOENT) => {
            issue(&mut result, &msi_source, e.raw_os_error().unwrap_or(libc::EIO));
        }
        Err(_) => {}
    }

    // irq may be the first MSI vector or the legacy INTx number during MSI-X. Prefer the
    // complete msi_irqs allocation; never invent an active INTx IRQ.
    if !msi_observed && !cancelled() {
        let irq_source = format!("{}/irq", device.path);
        let irq = read_metadata(dir, "irq", 32, cancel);
        if irq.state != ReadState::Available {
            record_read_error(&mut result, &irq_source, irq);
        } else {
            match decimal_number(irq.value.trim()) {
                None => issue(&mut result, &irq_source, libc::EINVAL),
                Some(0) => {}
                Some(number) => {
                    result.items.push(DeviceResource { start: number, end: number, mode: "reported".into(), source: irq_source, ..resource(ResourceType::Irq) })
                }
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pci_table_keeps_slots_types_and_64_bit_addresses() {
        let pci = parse_pci_resources(
            "0x00000007c0000000 0x00000007cfffffff 0x00102200\n\
             0x000000000000ef00 0x000000000000efff 0x00000100\n\
             0 0 0\n\
             0 0xfff 0x20000200\n\
             0x1000 0x1fff 0x10000200\n\
             0 0 0x200\n",
            "fixture/resource",
        );
        assert!(pci.issues.is_empty());
        assert_eq!(pci.items.len(), 5);
        assert_eq!((pci.items[0].start, pci.items[0].end), (0x7_c000_0000, 0x7_cfff_ffff));
        assert_eq!((pci.items[1].kind, pci.items[1].start), (ResourceType::Io, 0xef00));
        assert_eq!((pci.items[2].index, pci.items[2].allocation), (3, Allocation::Unassigned));
        assert_eq!(pci.items[3].allocation, Allocation::Disabled);
        assert_eq!(pci.items[4].allocation, Allocation::Unavailable);
        assert_eq!(pci.items[0].source, "fixture/resource");
    }

    #[test]
    fn malformed_pci_input_is_rejected_whole() {
        let bad = parse_pci_resources("0x1000 0x1fff 0x200\ninvalid\n", "fixture/resource");
        assert!(bad.items.is_empty());
        assert_eq!(bad.issues.len(), 1);
        for text in ["0x2000 0x1000 0x200\n", "0x10000000000000000 0x1fff 0x200\n"] {
            assert!(!parse_pci_resources(text, "fixture/resource").issues.is_empty(), "{text}");
        }
        assert!(!parse_pci_resources(&"x".repeat(65537), "fixture/resource").issues.is_empty());
        assert!(!parse_pci_resources("0 0 0\n", "fixture/resource").has_information(), "empty slots alone");
    }

    #[test]
    fn pnp_assignments_are_typed() {
        let pnp = parse_pnp_resources("state = active\nio 0x60-0x60\nirq 0\nmem 0xf0000000-0xffffffff window\nirq disabled\ndma 4\n", "fixture/resources");
        assert!(pnp.issues.is_empty());
        assert_eq!(pnp.items.len(), 5);
        assert_eq!((pnp.items[1].kind, pnp.items[1].start, pnp.items[1].allocation), (ResourceType::Irq, 0, Allocation::Assigned));
        assert_eq!(pnp.items[2].flags, WINDOW);
        assert_eq!(pnp.items[3].allocation, Allocation::Disabled);
        let inactive = parse_pnp_resources("state = disabled\nio 0x60-0x64\n", "fixture/resources");
        assert_eq!((inactive.items.len(), inactive.items[0].allocation), (1, Allocation::Disabled));
        assert!(!parse_pnp_resources("state = active\nmem 0x20-0x10\n", "fixture/resources").issues.is_empty());
        assert!(!parse_pnp_resources("irq 7\n", "fixture/resources").issues.is_empty());
        let none = parse_pnp_resources("state = active\nirq -1\nio 0-0\n", "f");
        assert_eq!((none.items[0].allocation, none.items[1].allocation), (Allocation::Unassigned, Allocation::Unavailable));
    }

    fn pci_device(dir: &std::path::Path) -> Device {
        Device { path: dir.to_string_lossy().into_owned(), subsystem: "pci".into(), ..Default::default() }
    }

    #[test]
    fn collect_prefers_msi_vectors_over_the_legacy_irq() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("resource"), "0x1000 0x1fff 0x200\n").unwrap();
        fs::write(root.join("irq"), "16\n").unwrap();
        fs::create_dir(root.join("msi_irqs")).unwrap();
        fs::write(root.join("msi_irqs/130"), "msix\n").unwrap();
        fs::write(root.join("msi_irqs/129"), "msix\n").unwrap();
        fs::write(root.join("msi_irqs/bad"), "msix\n").unwrap();
        let dir = File::open(root).unwrap();
        let found = collect_device_resources(&dir, &pci_device(root), None);
        let irqs: Vec<_> = found.items.iter().filter(|r| r.kind == ResourceType::Irq).map(|r| (r.start, r.mode.as_str())).collect();
        assert_eq!(irqs, [(129, "msix"), (130, "msix")], "sorted MSI vectors, no INTx guess");
        assert!(found.issues.iter().any(|i| i.source.ends_with("msi_irqs") && i.error.error == libc::EINVAL));
        assert_eq!(found.items.iter().filter(|r| r.pci).count(), 1);
    }

    #[test]
    fn collect_falls_back_to_reported_irq_and_ignores_irq_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("irq"), "16\n").unwrap();
        let dir = File::open(root).unwrap();
        let found = collect_device_resources(&dir, &pci_device(root), None);
        assert_eq!(found.items.len(), 1);
        assert_eq!((found.items[0].start, found.items[0].mode.as_str()), (16, "reported"));
        assert!(found.issues.is_empty(), "missing resource file is not an issue");
        fs::write(root.join("irq"), "0\n").unwrap();
        assert!(collect_device_resources(&dir, &pci_device(root), None).items.is_empty());
        fs::write(root.join("irq"), "x\n").unwrap();
        assert_eq!(collect_device_resources(&dir, &pci_device(root), None).issues[0].error.error, libc::EINVAL);
    }

    #[test]
    fn collect_pnp_and_other_subsystems() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("resources"), "state = active\nio 0x60-0x60\n").unwrap();
        let dir = File::open(tmp.path()).unwrap();
        let pnp = Device { path: "/sys/x".into(), subsystem: "pnp".into(), ..Default::default() };
        let found = collect_device_resources(&dir, &pnp, None);
        assert_eq!((found.items.len(), found.pnp_state.as_str()), (1, "active"));
        let other = Device { subsystem: "usb".into(), ..Default::default() };
        assert!(!collect_device_resources(&dir, &other, None).has_information());
    }
}
