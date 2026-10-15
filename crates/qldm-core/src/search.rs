//! Quick and deep device search, ported from `src/devices/devicesearch.{h,cpp}`.
//!
//! A search document is the flat list of labelled values a device exposes: names, identifiers,
//! raw udev/sysfs values (with decimal/hex aliases for documented numeric fields), resources,
//! storage metadata and UEFI bytes. Deep search builds these on a worker, caches them per
//! device generation (bounded to 64 MiB), and reports matches in batches. Camera capabilities,
//! desktop display configuration and UDisks2 health are deliberately not indexed, and a snapshot
//! whose read was invalidated contributes no partial properties.
use crate::i18n::{arg, tr};
use crate::label::device_display_name;
use crate::model::{category_label, Attribute, Device, ReadState};
use crate::properties::{collect_device_properties, DeviceProperties};
use crate::propertytext::{property_entries, property_read_value};
use crate::resources::{Allocation, ResourceType};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const CACHE_LIMIT_BYTES: usize = 64 * 1024 * 1024;
const BATCH_INTERVAL: Duration = Duration::from_millis(100);
const BATCH_MATCHES: usize = 128;

#[derive(Clone, Debug)]
pub struct SearchRecord {
    pub device: Device,
    /// The exact label the source model currently presents.
    pub label: String,
    pub grouped_paths: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchField {
    pub label: String,
    pub value: String,
}

pub type SearchDocument = Vec<SearchField>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchMatch {
    pub path: String,
    pub generation: u64,
    pub field: String,
}

/// Progress and matches of one deep-search request. `matches` carries only the matches found
/// since the previous batch; the counters are cumulative.
#[derive(Clone, Debug, Default)]
pub struct SearchBatch {
    pub request: u64,
    pub matches: Vec<SearchMatch>,
    pub done: usize,
    pub total: usize,
    /// Records whose properties could not be fully read; their results may be incomplete.
    pub unavailable: usize,
}

/// A number as decimal, `0x` hex and zero-padded 16-digit hex, so any notation finds it.
fn numbers(value: u64) -> String {
    format!("{value} 0x{value:x} {value:016x}")
}

/// Aliases for documented numeric fields only; serial numbers and names stay literal.
fn numeric_aliases(key: &str, value: &str, subsystem: &str) -> String {
    const HEX_KEYS: [&str; 10] =
        ["idVendor", "idProduct", "bcdDevice", "vendor_id", "subsystem_id", "revision_id", "ID_VENDOR_ID", "ID_MODEL_ID", "PCI_CLASS", "flags"];
    const PCI_HEX: [&str; 6] = ["vendor", "device", "subsystem_vendor", "subsystem_device", "class", "revision"];
    const DECIMAL_KEYS: [&str; 6] = ["busnum", "devnum", "ifindex", "index", "authorized", "present"];
    let mut base = if HEX_KEYS.contains(&key) || (subsystem == "pci" && PCI_HEX.contains(&key)) {
        16
    } else if DECIMAL_KEYS.contains(&key) {
        10
    } else {
        0
    };
    let components: Vec<&str> = match key {
        "PCI_ID" | "PCI_SUBSYS_ID" => {
            base = 16;
            value.split(':').collect()
        }
        "PRODUCT" => {
            base = 16;
            value.split('/').collect()
        }
        _ => vec![value],
    };
    if base == 0 {
        return String::new();
    }
    components
        .iter()
        .filter_map(|part| {
            let part = part.trim();
            let digits = if base == 16 { part.strip_prefix("0x").or_else(|| part.strip_prefix("0X")).unwrap_or(part) } else { part };
            u64::from_str_radix(digits, base).ok()
        })
        .map(numbers)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Builds the searchable text of one device from its inventory record and property snapshot.
pub fn device_search_document(record: &SearchRecord, p: &DeviceProperties) -> SearchDocument {
    let mut doc: SearchDocument = Vec::new();
    let d = &record.device;
    let mut add = |doc: &mut SearchDocument, label: &str, value: &str| {
        if !value.is_empty() {
            doc.push(SearchField { label: label.to_string(), value: value.to_string() });
        }
    };
    let attribute = |doc: &mut SearchDocument, add: &mut dyn FnMut(&mut SearchDocument, &str, &str), key: &str, a: &Attribute, source: &str| {
        add(doc, key, &if a.state == ReadState::Available { a.value.clone() } else { property_read_value(a) });
        if a.error != 0 {
            add(doc, key, &a.error.to_string());
        }
        if a.state == ReadState::Available {
            add(doc, key, &numeric_aliases(key.rsplit('/').next().unwrap_or(key), &a.value, &d.subsystem));
        }
        add(doc, key, source);
    };
    let name_label = tr("Device name");
    add(&mut doc, &name_label, &record.label);
    add(&mut doc, &name_label, &device_display_name(d, false));
    add(&mut doc, &tr("Original name"), &d.name);
    add(&mut doc, &tr("Name source"), &d.name_source);
    add(&mut doc, &tr("Category"), &category_label(d.category));
    add(&mut doc, &tr("Category"), d.category);
    add(&mut doc, &tr("Full kernel path"), &d.path);
    add(&mut doc, &tr("Parent device path"), &d.parent_path);
    add(&mut doc, &tr("Kernel name"), &d.sysname);
    add(&mut doc, &tr("Bus / subsystem"), &d.subsystem);
    add(&mut doc, "DEVTYPE", &d.devtype);
    add(&mut doc, &tr("Bound kernel driver"), &d.driver);
    add(&mut doc, &tr("Represented by"), &d.represented_by_path);
    add(&mut doc, &tr("Grouped records"), &record.grouped_paths);
    for (key, value) in &d.properties {
        let a = Attribute { state: ReadState::Available, value: value.clone(), error: 0 };
        attribute(&mut doc, &mut add, key, &a, d.property_sources.get(key).map_or("", String::as_str));
    }
    for (key, a) in &d.attributes {
        attribute(&mut doc, &mut add, key, a, "");
    }
    // An invalidated read must not contribute partially collected properties.
    if p.state != ReadState::Available {
        let status = Attribute { state: p.state, value: String::new(), error: p.error };
        add(&mut doc, &tr("Device status"), &property_read_value(&status));
        return doc;
    }
    for (key, a) in &p.values {
        attribute(&mut doc, &mut add, key, a, p.sources.get(key).map_or("", String::as_str));
    }
    for entry in property_entries(p, false) {
        add(&mut doc, &entry.label, &entry.value);
        add(&mut doc, &entry.label, &entry.source);
    }
    if p.values.get("efi/hex").is_some_and(|a| a.state == ReadState::Available) {
        let compact: String = p.efi_bytes.iter().map(|b| format!("{b:02x}")).collect();
        add(&mut doc, &tr("UEFI variable — hex dump"), &compact);
    }
    add(&mut doc, &tr("PnP device state"), &p.resources.pnp_state);
    if p.resources.pnp_state == "disabled" {
        add(&mut doc, &tr("PnP device state"), &tr("PnP device disabled"));
    }
    if p.efi_truncated {
        add(&mut doc, &tr("UEFI variable — hex dump"), &tr("Truncated: first 65536 bytes shown."));
    }
    for r in &p.resources.items {
        let label = tr(match r.kind {
            ResourceType::Memory => "Memory range",
            ResourceType::Io => "I/O range",
            ResourceType::Irq => "IRQ",
            ResourceType::Dma => "DMA channel",
            ResourceType::Bus => "Bus range",
        });
        add(&mut doc, &label, &r.source);
        add(&mut doc, &label, &format!("{} {} {}", numbers(r.start), numbers(r.end), numbers(r.flags)));
        let width = if r.kind == ResourceType::Memory { 16 } else { 4 };
        add(&mut doc, &label, &format!("{:0width$x} – {:0width$x}", r.start, r.end));
        if r.pci {
            add(
                &mut doc,
                &label,
                &match r.index {
                    0..=5 => arg(tr("BAR %1"), &r.index.to_string()),
                    6 => tr("Expansion ROM"),
                    _ => arg(tr("PCI resource %1"), &r.index.to_string()),
                },
            );
        }
        match r.allocation {
            Allocation::Assigned => {}
            Allocation::Unassigned => add(&mut doc, &label, &tr("Unassigned")),
            Allocation::Disabled => add(&mut doc, &label, &tr("Disabled")),
            Allocation::Unavailable => add(&mut doc, &label, &tr("Address unavailable (zeroed or masked)")),
        }
        for (bit, text) in [(0x2000, "Prefetchable"), (0x4000, "Read-only"), (0x10_0000, "64-bit memory"), (0x20_0000, "Bridge window")] {
            if r.flags & bit != 0 {
                add(&mut doc, &label, &tr(text));
            }
        }
        add(&mut doc, &label, if r.mode == "msix" { "MSI-X" } else { &r.mode });
        if r.mode == "reported" {
            add(&mut doc, &label, &tr("Reported irq attribute; active mode not established"));
        }
    }
    for issue in &p.resources.issues {
        attribute(&mut doc, &mut add, &tr("Resource metadata"), &issue.error, &issue.source);
    }
    doc
}

/// The label of the first field whose value or label contains `query` (case-insensitive).
pub fn search_document_match(document: &SearchDocument, query: &str) -> Option<String> {
    if query.is_empty() {
        return None;
    }
    let needle = query.to_lowercase();
    document.iter().find(|f| f.value.to_lowercase().contains(&needle) || f.label.to_lowercase().contains(&needle)).map(|f| f.label.clone())
}

struct Cached {
    generation: u64,
    document: SearchDocument,
    unavailable: bool,
}

/// Deep-search state that outlives one request: the per-device document cache, which is dropped
/// whenever the inventory revision changes. Run it on a worker thread; it stops between records
/// once `cancel` is set.
#[derive(Default)]
pub struct DeepSearchWorker {
    cache: HashMap<String, Cached>,
    cache_bytes: usize,
    cached_revision: u64,
}

impl DeepSearchWorker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Searches `records` for `query`, calling `emit` with a batch whenever 100 ms have passed or
    /// 128 matches accumulated, and once at the end (unless cancelled).
    pub fn run(&mut self, records: &[SearchRecord], query: &str, revision: u64, request: u64, cancel: &AtomicBool, mut emit: impl FnMut(SearchBatch)) {
        if revision != self.cached_revision {
            self.cache.clear();
            self.cache_bytes = 0;
            self.cached_revision = revision;
        }
        let mut batch = SearchBatch { request, total: records.len(), ..Default::default() };
        let mut timer = Instant::now();
        for record in records {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let path = &record.device.path;
            let (document, unavailable) = match self.cache.get(path).filter(|c| c.generation == record.device.generation) {
                Some(cached) => (cached.document.clone(), cached.unavailable),
                None => {
                    let properties = collect_device_properties(&record.device, false, Some(cancel));
                    if cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    let unavailable = properties.state != ReadState::Available
                        || !properties.resources.issues.is_empty()
                        || properties.values.values().any(|v| matches!(v.state, ReadState::PermissionDenied | ReadState::Error));
                    let document = device_search_document(record, &properties);
                    let bytes: usize = document.iter().map(|f| 2 * (f.label.len() + f.value.len()) + std::mem::size_of::<SearchField>()).sum();
                    // Bound cached payload to 64 MiB. Uncached records are still fully searched.
                    if self.cache_bytes + bytes <= CACHE_LIMIT_BYTES {
                        self.cache.insert(path.clone(), Cached { generation: record.device.generation, document: document.clone(), unavailable });
                        self.cache_bytes += bytes;
                    }
                    (document, unavailable)
                }
            };
            if let Some(field) = search_document_match(&document, query) {
                batch.matches.push(SearchMatch { path: path.clone(), generation: record.device.generation, field });
            }
            batch.done += 1;
            if unavailable {
                batch.unavailable += 1;
            }
            if timer.elapsed() >= BATCH_INTERVAL || batch.matches.len() >= BATCH_MATCHES {
                emit(batch.clone());
                batch.matches.clear();
                timer = Instant::now();
            }
        }
        emit(batch);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Attribute;
    use crate::properties::DeviceProperties;
    use crate::resources::DeviceResource;
    use crate::storage::{Format, StorageEntity, StorageField, StorageProperties};

    fn fixture() -> (Device, DeviceProperties) {
        let mut device = Device {
            path: "/sys/devices/fixture".into(),
            incarnation: "fixture:1".into(),
            name: "Visible device".into(),
            category: "other",
            subsystem: "pci".into(),
            ..Default::default()
        };
        device.properties.insert("ID_SERIAL".into(), "CaseSensitiveSerial".into());
        let mut properties = DeviceProperties { device: device.clone(), state: ReadState::Available, ..Default::default() };
        properties.values.insert("sysfs/vendor".into(), Attribute { state: ReadState::Available, value: "0x8086".into(), error: 0 });
        properties.resources.items.push(DeviceResource {
            kind: ResourceType::Io,
            start: 0x300,
            end: 0x31f,
            source: "/sys/devices/fixture/resource".into(),
            index: -1,
            ..Default::default()
        });
        (device, properties)
    }
    fn record(device: &Device) -> SearchRecord {
        SearchRecord { device: device.clone(), label: "Visible device".into(), grouped_paths: String::new() }
    }

    #[test]
    fn raw_values_are_case_insensitive_and_numbers_have_aliases() {
        let (device, properties) = fixture();
        let document = device_search_document(&record(&device), &properties);
        assert!(search_document_match(&document, "casesensitiveserial").is_some(), "raw metadata is case-insensitive");
        assert!(search_document_match(&document, "32902").is_some(), "PCI vendor has a decimal alias");
        assert!(search_document_match(&document, "0x8086").is_some());
        assert_eq!(search_document_match(&document, "768").as_deref(), Some("I/O range"));
        assert!(search_document_match(&document, "0000000000000300").is_some(), "padded hex kept");
        assert!(search_document_match(&document, "").is_none());
        assert!(search_document_match(&document, "definitely absent").is_none());
    }

    #[test]
    fn numeric_aliases_apply_only_to_documented_fields() {
        assert_eq!(numeric_aliases("PCI_ID", "8086:7740", "pci"), "32902 0x8086 0000000000008086 30528 0x7740 0000000000007740");
        assert_eq!(numeric_aliases("PRODUCT", "18/6cb/cfc6/100", "input").split(' ').count(), 12);
        assert_eq!(numeric_aliases("busnum", "7", "usb"), "7 0x7 0000000000000007");
        assert_eq!(numeric_aliases("vendor", "0x8086", "pci").split(' ').next(), Some("32902"));
        assert_eq!(numeric_aliases("vendor", "0x8086", "usb"), "", "pci-only hex keys");
        assert_eq!(numeric_aliases("ID_SERIAL", "123456", "usb"), "", "serials stay literal");
        assert_eq!(numeric_aliases("busnum", "x", "usb"), "");
    }

    #[test]
    fn uefi_bytes_are_searchable_as_spaced_compact_and_printable() {
        let (device, _) = fixture();
        let efi = Device { subsystem: "efivarfs".into(), ..device };
        let mut properties = DeviceProperties { device: efi.clone(), state: ReadState::Available, efi_bytes: vec![0x41, 0x42, 0xff], ..Default::default() };
        properties.values.insert("efi/hex".into(), Attribute { state: ReadState::Available, ..Default::default() });
        let document = device_search_document(&SearchRecord { device: efi, label: "EFI fixture".into(), grouped_paths: String::new() }, &properties);
        for needle in ["41 42 ff", "AB.", "4142ff"] {
            assert!(search_document_match(&document, needle).is_some(), "{needle}");
        }
    }

    #[test]
    fn storage_is_searchable_but_invalid_snapshots_contribute_nothing() {
        let (device, mut storage_fixture) = fixture();
        let mut volume = StorageEntity { id: "/sys/devices/storage-fixture".into(), ..Default::default() };
        let field = |id: &str, label: &'static str, value: &str, source: &str, format| StorageField {
            id: id.into(),
            label,
            value: if value.is_empty() { Attribute::default() } else { Attribute { state: ReadState::Available, value: value.into(), error: 0 } },
            source: source.into(),
            format,
        };
        volume.fields.push(field("capacity", "Block capacity", "18446744073709551615", "fixture/size", Format::Bytes));
        volume.fields.push(field("health", "Cached NVMe critical warnings", "", "fixture/cache", Format::NvmeHealth));
        volume.fields.push(field("mounts", "Mount points", "/fixture mount", "fixture/mountinfo", Format::Text));
        storage_fixture.storage = StorageProperties { applicable: true, entities: vec![volume], ..Default::default() };
        let entries = property_entries(&storage_fixture, false);
        assert!(entries.iter().any(|e| e.id.ends_with("/health") && e.value == "Unavailable"), "missing health is never healthy");
        assert!(
            entries.iter().any(|e| e.id.ends_with("/capacity") && e.value.ends_with(" bytes") && !e.value.contains('(')),
            "exact bytes above the signed range"
        );
        let document = device_search_document(&record(&device), &storage_fixture);
        assert!(search_document_match(&document, "/fixture mount").is_some(), "mounts are searchable");
        storage_fixture.state = ReadState::Removed;
        let removed = device_search_document(&record(&device), &storage_fixture);
        assert!(search_document_match(&removed, "/fixture mount").is_none(), "removed instances add no partial storage results");
        let (_, mut properties) = fixture();
        properties.state = ReadState::Removed;
        let invalid = device_search_document(&record(&device), &properties);
        assert!(search_document_match(&invalid, "32902").is_none(), "invalidated properties add nothing");
        assert!(search_document_match(&invalid, "casesensitiveserial").is_some(), "the last inventory stays searchable");
        assert!(search_document_match(&invalid, "Device removed").is_some());
    }

    #[test]
    fn property_entry_statuses_stay_independent_of_displayed_text() {
        use crate::propertytext::details_property_status;
        let (_, mut status_fixture) = fixture();
        status_fixture.device.properties.clear();
        status_fixture.values.remove("sysfs/vendor");
        status_fixture.values.insert("sysfs/model".into(), Attribute { state: ReadState::Available, value: "Unavailable".into(), error: 0 });
        status_fixture.values.insert("sysfs/serial".into(), Attribute::default());
        status_fixture.values.insert("sysfs/revision".into(), Attribute { state: ReadState::PermissionDenied, value: String::new(), error: 13 });
        for entry in property_entries(&status_fixture, false) {
            match entry.id.as_str() {
                "sysfs/model" => assert!(details_property_status(&entry).is_empty(), "a literal status-like value stays meaningful"),
                "sysfs/serial" => assert!(entry.state == ReadState::Unavailable && details_property_status(&entry) == "unavailable"),
                "sysfs/revision" => assert!(entry.state == ReadState::PermissionDenied && details_property_status(&entry) == "permission denied"),
                "identifiers" => assert_eq!(entry.state, ReadState::Unavailable, "missing derived identifiers are unavailable"),
                _ => {}
            }
        }
        let mut efi_status = status_fixture.clone();
        efi_status.device.subsystem = "efivarfs".into();
        let identifiers = property_entries(&efi_status, false).into_iter().find(|e| e.id == "identifiers").unwrap();
        assert!(identifiers.state == ReadState::NotApplicable && details_property_status(&identifiers) == "not applicable");
    }

    #[test]
    fn deep_search_batches_caches_by_generation_and_cancels() {
        let devices = crate::scan().expect("scan");
        let wanted = devices.iter().find(|d| d.subsystem == "pci" && !d.hidden).expect("a pci device").clone();
        let mut records: Vec<SearchRecord> = devices
            .iter()
            .filter(|d| !d.hidden)
            .take(6)
            .map(|d| SearchRecord { device: d.clone(), label: device_display_name(d, false), grouped_paths: String::new() })
            .collect();
        if !records.iter().any(|r| r.device.path == wanted.path) {
            records.push(SearchRecord { device: wanted.clone(), label: "x".into(), grouped_paths: String::new() });
        }
        let cancel = AtomicBool::new(false);
        let mut worker = DeepSearchWorker::new();
        let mut batches = Vec::new();
        worker.run(&records, &wanted.sysname, 1, 7, &cancel, |b| batches.push(b));
        let last = batches.last().expect("final batch");
        assert_eq!((last.request, last.total, last.done), (7, records.len(), records.len()));
        let hits: Vec<_> = batches.iter().flat_map(|b| b.matches.iter()).collect();
        assert!(hits.iter().any(|m| m.path == wanted.path), "the kernel name finds its device");
        assert_eq!(worker.cache.len(), records.len(), "documents cached per device");

        // Same revision and generations: served from the cache, same answer.
        let mut again = Vec::new();
        worker.run(&records, &wanted.sysname, 1, 8, &cancel, |b| again.push(b));
        assert_eq!(again.iter().map(|b| b.matches.len()).sum::<usize>(), hits.len());
        // A new revision drops the cache.
        worker.run(&[], "x", 2, 9, &cancel, |_| {});
        assert!(worker.cache.is_empty());

        let stop = AtomicBool::new(true);
        let mut none = Vec::new();
        worker.run(&records, "x", 3, 10, &stop, |b| none.push(b));
        assert!(none.is_empty(), "a cancelled run publishes nothing");
    }
}
