//! Display model for the Storage tab, ported from `src/devices/storagepresentation.cpp`.
//!
//! Native and UDisks2 entities that an exact `Block.DeviceNumber` link proves to be the same
//! block device are merged; names that merely look alike never are. Health is per drive and
//! always carries its cached scope. No widgets or services are touched here.
use crate::i18n::{arg, argn, format_fixed, format_integer, tr};
use crate::model::ReadState;
use crate::propertytext::storage_field_value;
use crate::storage::{AtaSmartAttribute, Format, StorageEntity, StorageField, StorageLink, StorageProperties};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StorageDisplayField {
    pub label: String,
    pub value: String,
    pub source: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StorageDisplayGroup {
    pub title: String,
    pub fields: Vec<StorageDisplayField>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StorageDisplayAtaAttribute {
    pub id: String,
    pub name: String,
    pub current: String,
    pub worst: String,
    pub threshold: String,
    pub interpreted: String,
    pub flags: String,
    pub source: String,
}

#[derive(Clone, Debug, Default)]
pub struct StorageDisplayRow {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub capacity: String,
    pub filesystem: String,
    pub mounts: String,
    pub source: String,
    pub fields: Vec<StorageDisplayField>,
    pub health_groups: Vec<StorageDisplayGroup>,
    pub ata_attributes: Vec<StorageDisplayAtaAttribute>,
}

#[derive(Clone, Debug, Default)]
pub struct StoragePresentation {
    pub overview: Vec<StorageDisplayField>,
    pub volumes: Vec<StorageDisplayRow>,
    pub health: Vec<StorageDisplayRow>,
    /// Unique explicit association with the selected device, if any.
    pub preferred_health_drive: String,
    pub notes: String,
}

fn field_of(label: impl Into<String>, value: impl Into<String>, source: impl Into<String>) -> StorageDisplayField {
    StorageDisplayField { label: label.into(), value: value.into(), source: source.into() }
}

fn health_field(f: &StorageField) -> bool {
    f.id == "health" || f.id.contains(".Drive.Ata/") || f.id.contains(".NVMe.Controller/")
}

fn drive_entity(e: &StorageEntity) -> bool {
    e.fields.iter().any(|f| f.id.starts_with("org.freedesktop.UDisks2.Drive/") || health_field(f))
}

fn available(f: &StorageField) -> bool {
    f.value.state == ReadState::Available && !f.value.value.is_empty()
}

fn value(entity: &StorageEntity, keys: &[&str]) -> String {
    for key in keys {
        if let Some(f) = entity.fields.iter().find(|f| f.id == *key && available(f)) {
            return storage_field_value(f);
        }
    }
    tr("Unavailable")
}

fn file_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or("").to_string()
}

fn name(entity: &StorageEntity) -> String {
    for key in [
        "DEVNAME",
        "dm/name",
        "org.freedesktop.UDisks2.LogicalVolume/Name",
        "org.freedesktop.UDisks2.VolumeGroup/Name",
        "org.freedesktop.UDisks2.MDRaid/Name",
        "org.freedesktop.UDisks2.Drive/Model",
    ] {
        if let Some(f) = entity.fields.iter().find(|f| f.id == key && available(f)) {
            return f.value.value.clone();
        }
    }
    // Kernel names/object basenames are display fallbacks, never identity keys.
    file_name(&entity.id)
}

fn kind(entity: &StorageEntity) -> String {
    for f in &entity.fields {
        if f.id == "partition" || f.id.contains(".Partition/") {
            return tr("Partition");
        }
        if f.id == "dm/name" {
            return tr("Device mapper");
        }
        if f.id == "md/level" || f.id.contains(".MDRaid/") {
            return tr("RAID");
        }
        if f.id.contains(".LogicalVolume/") {
            return tr("Logical volume");
        }
        if f.id.contains(".VolumeGroup/") {
            return tr("Volume group");
        }
    }
    tr("Block device")
}

fn display(f: &StorageField) -> StorageDisplayField {
    field_of(tr(f.label), storage_field_value(f), f.source.clone())
}

/// Aliases describe the same fact on one block entity. All sources are kept; disagreeing
/// available values are shown together rather than silently overwritten.
fn fact(output: &mut Vec<StorageDisplayField>, entity: &StorageEntity, caption: &str, keys: &[&str]) {
    let mut values: Vec<String> = Vec::new();
    let mut sources: Vec<String> = Vec::new();
    let mut fallback: Option<&StorageField> = None;
    for key in keys {
        for f in entity.fields.iter().filter(|f| f.id == *key) {
            fallback.get_or_insert(f);
            if !sources.contains(&f.source) {
                sources.push(f.source.clone());
            }
            if available(f) {
                let text = storage_field_value(f);
                if !values.contains(&text) {
                    values.push(text);
                }
            }
        }
    }
    let Some(fallback) = fallback else { return };
    let mut text = if values.is_empty() { storage_field_value(fallback) } else { values.join("\n") };
    if values.len() > 1 {
        text = format!("{}\n{text}", tr("Conflicting metadata:"));
    }
    output.push(field_of(tr(caption), text, sources.join("\n")));
}

fn ata_attribute(attribute: &AtaSmartAttribute, drive: &str) -> StorageDisplayAtaAttribute {
    let normalized = |n: i32| if n < 0 { tr("Unavailable") } else { format_integer(n as u64) };
    let interpreted = if attribute.unit == 0 || (attribute.unit == 4 && attribute.interpreted == 0) {
        tr("Unavailable")
    } else if attribute.interpreted < 0 {
        tr("Invalid health metadata")
    } else {
        let number = format_integer(attribute.interpreted as u64);
        match attribute.unit {
            1 => number,
            2 => arg(tr("%1 milliseconds"), &number),
            3 => arg(tr("%1 sectors"), &number),
            4 => arg(tr("%1 °C"), &format_fixed(attribute.interpreted as f64 / 1000.0 - 273.15, 1)),
            unit => arg(tr("Unavailable (unknown unit %1)"), &unit.to_string()),
        }
    };
    StorageDisplayAtaAttribute {
        id: attribute.id.to_string(),
        name: if attribute.name.is_empty() { tr("Unnamed attribute") } else { attribute.name.clone() },
        current: normalized(attribute.current),
        worst: normalized(attribute.worst),
        threshold: normalized(attribute.threshold),
        interpreted,
        flags: format!("0x{:04x}", attribute.flags),
        source: format!("{drive} : org.freedesktop.UDisks2.Drive.Ata.SmartGetAttributes : ID {}", attribute.id),
    }
}

fn caption_for(key: &str) -> Option<&'static str> {
    Some(match key {
        "health" => "Health reporting",
        "SmartUpdated" => "Last health update",
        "SmartSupported" => "ATA SMART supported",
        "SmartEnabled" => "ATA SMART enabled",
        "SmartFailing" => "Drive failure prediction",
        "SmartCriticalWarning" => "Critical warnings",
        "SmartPowerOnSeconds" | "SmartPowerOnHours" => "Power-on time",
        "SmartTemperature" => "Temperature",
        "SmartNumBadSectors" => "Bad sectors",
        "SmartNumAttributesFailing" => "Failing attributes",
        "SmartNumAttributesFailedInThePast" => "Attributes failed in the past",
        "avail_spare" => "Available spare",
        "spare_thresh" => "Spare threshold",
        "percent_used" => "Estimated endurance used",
        "total_data_read" => "Total data read",
        "total_data_written" => "Total data written",
        "ctrl_busy_time" => "Controller busy time",
        "power_cycles" => "Power cycles",
        "unsafe_shutdowns" => "Unsafe shutdowns",
        "media_errors" => "Media and data integrity errors",
        "num_err_log_entries" => "Error information log entries",
        "wctemp" => "Warning temperature threshold",
        "cctemp" => "Critical temperature threshold",
        "warning_temp_time" => "Time above warning temperature",
        "critical_temp_time" => "Time above critical temperature",
        _ => return None,
    })
}

fn health_groups(row: &mut StorageDisplayRow, entity: &StorageEntity) {
    let (mut summary, mut usage, mut errors, mut temperatures) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let (mut nvme, mut sensor_error_shown) = (false, false);
    for f in &entity.fields {
        if !health_field(f) {
            continue;
        }
        nvme = nvme || f.id.contains(".NVMe.Controller/");
        let key = f.id.rsplit('/').next().unwrap_or("");
        let mut shown = display(f);
        if let Some(caption) = caption_for(key) {
            shown.label = tr(caption);
        }
        if key == "SmartGetAttributes" {
            shown.label = if f.id.contains(".Drive.Ata/") { tr("ATA SMART attributes") } else { tr("NVMe health details") };
        }
        if f.id.contains("/temp_sensors/") {
            // Zero/unreported slots are absent evidence, not failed reads.
            if f.value.state == ReadState::Unavailable && f.value.error == 0 {
                continue;
            }
            if f.value.state != ReadState::Available {
                if sensor_error_shown {
                    continue;
                }
                sensor_error_shown = true;
                shown.label = tr("Temperature sensors");
            } else {
                shown.label = arg(tr("Temperature sensor %1"), key);
            }
            temperatures.push(shown);
        } else if f.format == Format::Kelvin || key.ends_with("temp_time") {
            temperatures.push(shown);
        } else if matches!(
            key,
            "SmartPowerOnSeconds" | "SmartPowerOnHours" | "total_data_read" | "total_data_written" | "ctrl_busy_time" | "power_cycles" | "unsafe_shutdowns"
        ) {
            usage.push(shown);
        } else if matches!(
            key,
            "SmartNumBadSectors" | "SmartNumAttributesFailing" | "SmartNumAttributesFailedInThePast" | "media_errors" | "num_err_log_entries"
        ) {
            errors.push(shown);
        } else if key == "SmartUpdated" {
            summary.insert(0, shown);
        } else {
            summary.push(shown);
        }
    }
    let mut group = |title: String, fields: Vec<StorageDisplayField>| {
        if !fields.is_empty() {
            row.health_groups.push(StorageDisplayGroup { title, fields });
        }
    };
    group(if nvme { tr("Health and endurance") } else { tr("Health") }, summary);
    group(tr("Usage"), usage);
    group(tr("Errors"), errors);
    group(tr("Temperatures"), temperatures);
    let mut attributes = entity.ata_attributes.clone();
    attributes.sort_by_key(|a| a.id);
    row.ata_attributes.extend(attributes.iter().map(|a| ata_attribute(a, &entity.id)));
}

fn relation(link: &StorageLink) -> String {
    let source = link.source.as_str();
    let text = if source.ends_with(".Table") {
        "Partition of"
    } else if source.ends_with(".CryptoBackingDevice") {
        "Backed by"
    } else if source.ends_with(".Drive") {
        "Drive"
    } else if source.ends_with(".MDRaidMember") {
        "RAID member of"
    } else if source.ends_with(".MDRaid") {
        "RAID device"
    } else if source.ends_with(".LogicalVolume") {
        "Logical volume"
    } else if source.ends_with(".VolumeGroup") {
        "Volume group"
    } else {
        link.label
    };
    tr(text)
}

fn overview(fields: &mut Vec<StorageDisplayField>, entity: &StorageEntity) {
    fields.push(field_of(tr("Device"), name(entity), entity.id.clone()));
    fact(fields, entity, "Model", &["model", "hardware/model", "ID_MODEL", "org.freedesktop.UDisks2.Drive/Model"]);
    fact(fields, entity, "Manufacturer", &["hardware/vendor", "ID_VENDOR", "org.freedesktop.UDisks2.Drive/Vendor"]);
    fact(fields, entity, "Serial number", &["serial", "hardware/serial", "ID_SERIAL_SHORT", "org.freedesktop.UDisks2.Drive/Serial"]);
    fact(
        fields,
        entity,
        "Firmware revision",
        &["firmware_rev", "hardware/firmware_rev", "hardware/rev", "ID_REVISION", "org.freedesktop.UDisks2.Drive/Revision"],
    );
    fact(fields, entity, "Transport", &["ID_BUS", "org.freedesktop.UDisks2.Drive/ConnectionBus"]);
    fact(fields, entity, "Native transport", &["native-transport"]);
    // A Drive.Size is a physical-drive fact, not a namespace/partition capacity alias.
    fact(fields, entity, "Block capacity", &["capacity"]);
    fact(fields, entity, "Drive capacity", &["org.freedesktop.UDisks2.Drive/Size"]);
    fact(fields, entity, "Logical sector size", &["queue/logical_block_size"]);
    fact(fields, entity, "Physical sector size", &["queue/physical_block_size"]);
    fact(fields, entity, "Kernel removable media", &["removable"]);
    fact(fields, entity, "Block read-only", &["ro"]);
    for f in &entity.fields {
        if f.id.ends_with("/Removable") || f.id.ends_with("/MediaRemovable") || f.id.ends_with("/MediaAvailable") {
            fields.push(display(f));
        }
    }
}

const BLOCK_LINK: &str = "UDisks2 Block.DeviceNumber / sysfs dev";

pub fn storage_presentation(snapshot: &StorageProperties, selected_path: &str) -> StoragePresentation {
    let mut result = StoragePresentation::default();
    let mut canonical: BTreeMap<String, String> = BTreeMap::new();
    let mut entities: BTreeMap<String, StorageEntity> = snapshot.entities.iter().map(|e| (e.id.clone(), e.clone())).collect();
    for link in &snapshot.links {
        if link.source != BLOCK_LINK || !entities.contains_key(&link.from) || !entities.contains_key(&link.to) {
            continue;
        }
        canonical.insert(link.to.clone(), link.from.clone());
        let merged = entities[&link.to].fields.clone();
        entities.get_mut(&link.from).expect("checked").fields.extend(merged);
    }
    for key in canonical.keys() {
        entities.remove(key);
    }
    let resolve = |id: &str| canonical.get(id).cloned().unwrap_or_else(|| id.to_string());

    let mut selected_drives: BTreeSet<String> = BTreeSet::new();
    for link in &snapshot.links {
        if resolve(&link.from) == selected_path && link.source.ends_with(".Drive") {
            selected_drives.insert(resolve(&link.to));
        }
    }
    // Controller dialogs cover their namespaces; this is an explicit descendant relation.
    if selected_path.starts_with("/sys/") {
        for link in &snapshot.links {
            if resolve(&link.from).starts_with(&format!("{selected_path}/")) && link.source.ends_with(".Drive") {
                selected_drives.insert(resolve(&link.to));
            }
        }
    }
    let mut selected = entities.get(selected_path).cloned().unwrap_or_default();
    // Identity aliases from a single explicitly associated drive share the overview. Drive
    // capacity retains its own caption: it is never a block capacity alias.
    if selected_drives.len() == 1 {
        let drive = selected_drives.iter().next().expect("one").clone();
        if let Some(entity) = entities.get(&drive) {
            selected.fields.extend(entity.fields.clone());
            result.overview.push(field_of(tr("Associated drive"), name(entity), drive));
        }
    }
    if entities.contains_key(selected_path) {
        overview(&mut result.overview, &selected);
    } else {
        result.overview.push(field_of(tr("Storage snapshot"), tr("Unavailable"), selected_path));
    }
    for entity in entities.values() {
        let mut row = StorageDisplayRow { id: entity.id.clone(), name: name(entity), source: entity.id.clone(), ..Default::default() };
        for (key, target) in &canonical {
            if *target == entity.id {
                row.source.push('\n');
                row.source.push_str(key);
            }
        }
        if drive_entity(entity) {
            let mut nodes: Vec<String> = Vec::new();
            for link in &snapshot.links {
                if resolve(&link.to) == entity.id && link.source.ends_with(".Drive") {
                    if let Some(block) = entities.get(&resolve(&link.from)) {
                        let partition = block.fields.iter().any(|f| f.id == "partition" || f.id.contains(".Partition/"));
                        if !partition {
                            let node = name(block);
                            if !nodes.contains(&node) {
                                nodes.push(node);
                            }
                        }
                    }
                }
            }
            if !nodes.is_empty() {
                row.name.push_str(&format!(" — {}", nodes.join(", ")));
            }
            row.kind = tr("Cached drive health");
            health_groups(&mut row, entity);
            if row.health_groups.is_empty() && row.ata_attributes.is_empty() {
                row.fields.push(field_of(tr("Cached health"), tr("Unavailable"), entity.id.clone()));
            }
            if selected_drives.len() == 1 && selected_drives.contains(&entity.id) {
                result.preferred_health_drive = entity.id.clone();
            }
            if selected_drives.len() > 1 && selected_drives.contains(&entity.id) {
                result.overview.push(field_of(tr("Associated drive"), row.name.clone(), entity.id.clone()));
                overview(&mut result.overview, entity);
            }
            result.health.push(row);
            continue;
        }
        // The selected NVMe controller is an overview, not another volume.
        let block = entity.fields.iter().any(|f| f.id == "dev");
        if entity.id == selected_path && entity.id.starts_with("/sys/") && !block && entity.fields.iter().any(|f| f.id == "firmware_rev") {
            continue;
        }
        row.kind = kind(entity);
        row.capacity = value(entity, &["capacity", "org.freedesktop.UDisks2.Partition/Size"]);
        row.filesystem = value(entity, &["ID_FS_TYPE", "org.freedesktop.UDisks2.Block/IdType"]);
        row.mounts = value(entity, &["mounts"]);
        let label = value(entity, &["ID_FS_LABEL", "org.freedesktop.UDisks2.Block/IdLabel"]);
        if label != tr("Unavailable") {
            row.name.push_str(&format!(" — {label}"));
        }
        const FACTS: [(&str, &[&str]); 7] = [
            ("Block capacity", &["capacity", "org.freedesktop.UDisks2.Partition/Size"]),
            ("Filesystem / content type", &["ID_FS_TYPE", "org.freedesktop.UDisks2.Block/IdType"]),
            ("Content usage", &["ID_FS_USAGE", "org.freedesktop.UDisks2.Block/IdUsage"]),
            ("Filesystem / content label", &["ID_FS_LABEL", "org.freedesktop.UDisks2.Block/IdLabel"]),
            ("Filesystem / content UUID", &["ID_FS_UUID", "org.freedesktop.UDisks2.Block/IdUUID"]),
            ("Partition number", &["partition", "org.freedesktop.UDisks2.Partition/Number"]),
            ("Partition table type", &["ID_PART_TABLE_TYPE", "org.freedesktop.UDisks2.PartitionTable/Type"]),
        ];
        let mut grouped: BTreeSet<&str> = BTreeSet::new();
        for (caption, keys) in FACTS {
            fact(&mut row.fields, entity, caption, keys);
            grouped.extend(keys.iter().copied());
        }
        for f in &entity.fields {
            if !health_field(f) && !grouped.contains(f.id.as_str()) {
                row.fields.push(display(f));
            }
        }
        for link in &snapshot.links {
            if resolve(&link.from) != entity.id || resolve(&link.to) == entity.id {
                continue;
            }
            let target = resolve(&link.to);
            let target_name = entities.get(&target).map_or_else(|| file_name(&target), name);
            row.fields.push(field_of(relation(link), target_name, format!("{}\n{target}", link.source)));
        }
        result.volumes.push(row);
    }
    if result.health.is_empty() {
        let row_name = entities.get(selected_path).map_or_else(|| file_name(selected_path), name);
        result.health.push(StorageDisplayRow {
            id: selected_path.to_string(),
            name: row_name,
            fields: vec![field_of(tr("Cached health"), tr("Unavailable"), tr("No supported cached drive health in this snapshot"))],
            ..Default::default()
        });
    }
    let notes: Vec<String> = snapshot.notes.iter().map(|note| format!("{}: {}\n{}", tr(note.label), storage_field_value(note), note.source)).collect();
    result.notes = notes.join("\n\n");
    result
}

pub fn storage_fields_text(fields: &[StorageDisplayField]) -> String {
    fields.iter().map(|f| format!("{}: {}\n{}", f.label, f.value, arg(tr("Source: %1"), &f.source))).collect::<Vec<_>>().join("\n\n")
}

pub fn storage_rows_text(rows: &[StorageDisplayRow]) -> String {
    rows.iter()
        .map(|row| {
            let mut sections = vec![row.name.clone()];
            if !row.health_groups.is_empty() || !row.ata_attributes.is_empty() {
                sections.push(tr("Cached drive health"));
            }
            if !row.fields.is_empty() {
                sections.push(storage_fields_text(&row.fields));
            }
            for group in &row.health_groups {
                sections.push(format!("{}\n{}", group.title, storage_fields_text(&group.fields)));
            }
            if !row.ata_attributes.is_empty() {
                let mut attributes = vec![tr("ATA SMART attributes")];
                for a in &row.ata_attributes {
                    attributes.push(argn(
                        tr("%1 (ID %2)\nNormalized value: %3\nWorst normalized value: %4\nThreshold: %5\nInterpreted value: %6\nFlags: %7\nSource: %8"),
                        &[&a.name, &a.id, &a.current, &a.worst, &a.threshold, &a.interpreted, &a.flags, &a.source],
                    ));
                }
                sections.push(attributes.join("\n\n"));
            }
            sections.join("\n\n")
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Attribute;

    fn sf(id: &str, value: &str, format: Format) -> StorageField {
        StorageField {
            id: id.into(),
            label: "Fixture",
            value: Attribute { state: ReadState::Available, value: value.into(), error: 0 },
            source: format!("fixture:{id}"),
            format,
        }
    }
    fn text(id: &str, value: &str) -> StorageField {
        sf(id, value, Format::Text)
    }
    fn entity(id: &str, fields: Vec<StorageField>) -> StorageEntity {
        StorageEntity { id: id.into(), fields, ata_attributes: vec![] }
    }
    fn edge(from: &str, to: &str, label: &'static str, source: &str) -> StorageLink {
        StorageLink { from: from.into(), to: to.into(), label, source: source.into() }
    }

    fn fixture() -> (StorageProperties, StorageEntity, StorageEntity, StorageEntity) {
        let native = entity(
            "/sys/devices/fixture/block/sda",
            vec![
                text("DEVNAME", "/dev/sda"),
                text("dev", "8:0"),
                sf("capacity", "1024", Format::Bytes),
                text("ID_FS_TYPE", "ext4"),
                text("ID_FS_LABEL", "data"),
                text("mounts", "/data"),
            ],
        );
        let cached_block = entity("/org/freedesktop/UDisks2/block_devices/sda", vec![text("org.freedesktop.UDisks2.Block/IdType", "xfs")]);
        let drive = entity(
            "/org/freedesktop/UDisks2/drives/fixture",
            vec![
                text("org.freedesktop.UDisks2.Drive/Model", "Fixture Drive"),
                sf("org.freedesktop.UDisks2.Drive/Size", "4096", Format::Bytes),
                sf("org.freedesktop.UDisks2.Drive.Ata/SmartFailing", "0", Format::AtaHealth),
            ],
        );
        let storage = StorageProperties {
            applicable: true,
            entities: vec![native.clone(), cached_block.clone(), drive.clone()],
            links: vec![
                edge(&native.id, &cached_block.id, "UDisks2 block metadata", BLOCK_LINK),
                edge(&cached_block.id, &drive.id, "Storage relationship", "org.freedesktop.UDisks2.Block.Drive"),
            ],
            notes: vec![],
        };
        (storage, native, cached_block, drive)
    }

    #[test]
    fn exact_block_mapping_merges_and_keeps_conflicts_visible() {
        let (storage, native, _, drive) = fixture();
        let presented = storage_presentation(&storage, &native.id);
        assert_eq!(presented.volumes.len(), 1);
        assert_eq!(presented.volumes[0].name, "/dev/sda — data");
        assert!(storage_fields_text(&presented.volumes[0].fields).contains("Conflicting metadata:"));
        assert!(presented.health.len() == 1 && presented.health[0].name.contains("/dev/sda"));
        assert!(storage_rows_text(&presented.health).contains("not a guarantee of health"));
        let overview = storage_fields_text(&presented.overview);
        assert!(overview.contains("Block capacity:") && overview.contains("Drive capacity:"));
        assert_eq!(presented.preferred_health_drive, drive.id);
    }

    #[test]
    fn related_drives_do_not_steal_the_preferred_health_drive() {
        let (storage, native, _, drive) = fixture();
        let mut related = storage.clone();
        let mut other = drive.clone();
        other.id = "/org/freedesktop/UDisks2/drives/aaa_related".into();
        related.entities.push(other.clone());
        let multiple = storage_presentation(&related, &native.id);
        assert!(multiple.health.len() == 2 && multiple.health[0].id == other.id);
        assert_eq!(multiple.preferred_health_drive, drive.id, "sort order cannot choose it");
        let controller = "/sys/devices/fixture";
        assert_eq!(storage_presentation(&related, controller).preferred_health_drive, drive.id);
        let other_block = entity(&format!("{controller}/block/sdb"), vec![text("DEVNAME", "/dev/sdb")]);
        related.entities.push(other_block.clone());
        related.links.push(edge(&other_block.id, &other.id, "Storage relationship", "org.freedesktop.UDisks2.Block.Drive"));
        assert!(storage_presentation(&related, controller).preferred_health_drive.is_empty(), "two explicit drives: no unique choice");
    }

    #[test]
    fn matching_basenames_never_merge_without_an_exact_mapping() {
        let (mut storage, native, ..) = fixture();
        storage.links.remove(0);
        assert_eq!(storage_presentation(&storage, &native.id).volumes.len(), 2);
        assert!(storage_presentation(&storage, &native.id).preferred_health_drive.is_empty());
        storage.entities = vec![native.clone()];
        storage.links.clear();
        let presented = storage_presentation(&storage, &native.id);
        assert!(presented.health.len() == 1 && storage_rows_text(&presented.health).contains("Unavailable"));
        assert!(presented.preferred_health_drive.is_empty());
    }

    #[test]
    fn nvme_health_groups_keep_overflow_zero_counters_sensors_and_unknown_warnings() {
        let nvme = "org.freedesktop.UDisks2.NVMe.Controller/";
        let attributes = format!("{nvme}SmartGetAttributes/");
        let sensor_error = |n: u8| StorageField {
            id: format!("{attributes}temp_sensors/{n}"),
            label: "Fixture",
            value: Attribute { state: ReadState::Error, value: String::new(), error: libc::EPROTO },
            source: "fixture:invalid".into(),
            format: Format::Kelvin,
        };
        let absent = StorageField {
            id: format!("{attributes}temp_sensors/2"),
            label: "Fixture",
            value: Attribute::default(),
            source: "fixture:absent".into(),
            format: Format::Kelvin,
        };
        let drive = entity(
            "/org/freedesktop/UDisks2/drives/nvme_fixture",
            vec![
                text("org.freedesktop.UDisks2.Drive/Model", "NVMe fixture"),
                sf(&format!("{nvme}SmartUpdated"), "1720000000", Format::Epoch),
                sf(&format!("{nvme}SmartCriticalWarning"), "readonly\nfuture_warning", Format::NvmeHealth),
                sf(&format!("{attributes}percent_used"), "150", Format::Percent),
                sf(&format!("{attributes}media_errors"), "0", Format::Count),
                sf(&format!("{attributes}temp_sensors/1"), "306", Format::Kelvin),
                absent,
                sensor_error(3),
                sensor_error(4),
            ],
        );
        let health = StorageProperties { entities: vec![drive.clone()], ..Default::default() };
        let report = storage_presentation(&health, &drive.id);
        let copied = storage_rows_text(&report.health);
        assert_eq!(report.health[0].health_groups[0].title, "Health and endurance");
        assert!(copied.contains("150%") && copied.contains("Media and data integrity errors: 0"));
        assert!(copied.contains("Temperature sensor 1: 33 °C") && !copied.contains("Temperature sensor 2"));
        assert_eq!(copied.matches("Temperature sensors:").count(), 1);
        assert!(copied.contains("Media is in read-only mode") && copied.contains("future_warning"));
    }

    #[test]
    fn ata_table_is_ordered_and_precise() {
        let mut drive = entity(
            "/org/freedesktop/UDisks2/drives/ata_fixture",
            vec![text("org.freedesktop.UDisks2.Drive/Model", "ATA fixture"), sf("org.freedesktop.UDisks2.Drive.Ata/SmartFailing", "0", Format::AtaHealth)],
        );
        let row = |id, name: &str, flags, current, worst, threshold, interpreted, unit| AtaSmartAttribute {
            id,
            name: name.into(),
            flags,
            current,
            worst,
            threshold,
            interpreted,
            unit,
        };
        drive.ata_attributes = vec![
            row(194, "temperature-celsius", 0, 100, 95, 0, 306000, 4),
            row(9, "power-on-hours", 0, 0, -1, 0, 1234567, 0),
            row(5, "reallocated-sector-count", 3, 100, 100, 36, 0, 3),
        ];
        let health = StorageProperties { entities: vec![drive.clone()], ..Default::default() };
        let report = storage_presentation(&health, &drive.id);
        let table = &report.health[0].ata_attributes;
        assert_eq!(table.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["5", "9", "194"]);
        assert_eq!((table[1].current.as_str(), table[1].worst.as_str(), table[1].interpreted.as_str()), ("0", "Unavailable", "Unavailable"));
        assert_eq!(table[2].interpreted, "32.9 °C");
        let copy = storage_rows_text(&report.health);
        assert!(copy.contains("reallocated-sector-count (ID 5)") && copy.contains("0 sectors"));
        assert!(copy.contains("Flags: 0x0003") && copy.contains(&format!("Source: {}", drive.id)));
    }

    #[test]
    fn controller_overview_is_not_a_volume_and_notes_are_listed() {
        let controller = entity("/sys/devices/pci/nvme0", vec![text("model", "Ctl"), text("firmware_rev", "1.0")]);
        let namespace = entity("/sys/devices/pci/nvme0/nvme0n1", vec![text("dev", "259:0")]);
        let storage =
            StorageProperties { applicable: true, entities: vec![controller.clone(), namespace], notes: vec![text("service", "cached")], ..Default::default() };
        let report = storage_presentation(&storage, &controller.id);
        assert_eq!(report.volumes.len(), 1, "controller is the overview, not a row");
        assert!(report.overview.iter().any(|f| f.label == "Model" && f.value == "Ctl"));
        assert!(report.notes.contains("Available; cached properties only"));
    }
}
