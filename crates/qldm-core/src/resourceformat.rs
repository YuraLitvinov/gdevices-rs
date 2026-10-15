//! Resource table rows for the UI, ported from `src/devices/resourceformat.cpp`.
use crate::i18n::{arg, tr};
use crate::propertytext::property_read_value;
use crate::resources::{Allocation, DeviceResources, ResourceType};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceRow {
    pub key: String,
    pub kind: String,
    pub setting: String,
    pub details: String,
    pub source: String,
    /// Resource type id as a number, or `"issues"`.
    pub group: String,
}

fn type_id(kind: ResourceType) -> i32 {
    match kind {
        ResourceType::Memory => 0,
        ResourceType::Io => 1,
        ResourceType::Irq => 2,
        ResourceType::Dma => 3,
        ResourceType::Bus => 4,
    }
}

pub fn resource_rows(data: &DeviceResources) -> Vec<ResourceRow> {
    let mut rows = Vec::new();
    for r in &data.items {
        let kind = tr(match r.kind {
            ResourceType::Memory => "Memory range",
            ResourceType::Io => "I/O range",
            ResourceType::Irq => "IRQ",
            ResourceType::Dma => "DMA channel",
            ResourceType::Bus => "Bus range",
        });
        let setting = match r.allocation {
            Allocation::Assigned if matches!(r.kind, ResourceType::Irq | ResourceType::Dma) => r.start.to_string(),
            Allocation::Assigned => {
                let width = if r.kind == ResourceType::Memory { 16 } else { 4 };
                format!("{:0width$X} – {:0width$X}", r.start, r.end)
            }
            Allocation::Unassigned => tr("Unassigned"),
            Allocation::Disabled => tr("Disabled"),
            Allocation::Unavailable => tr("Address unavailable (zeroed or masked)"),
        };
        let mut details: Vec<String> = Vec::new();
        if r.pci {
            details.push(match r.index {
                0..=5 => arg(tr("BAR %1"), &r.index.to_string()),
                6 => tr("Expansion ROM"),
                _ => arg(tr("PCI resource %1"), &r.index.to_string()),
            });
        }
        // Decode only stable flags; never interpret overlap or BUSY as a conflict.
        for (bit, text) in [(0x2000, "Prefetchable"), (0x4000, "Read-only"), (0x10_0000, "64-bit memory"), (0x20_0000, "Bridge window")] {
            if r.flags & bit != 0 {
                details.push(tr(text));
            }
        }
        if r.pci {
            details.push(arg(tr("Flags: %1"), &format!("0x{:x}", r.flags)));
        }
        match r.mode.as_str() {
            "msi" => details.push("MSI".into()),
            "msix" => details.push("MSI-X".into()),
            "reported" => details.push(tr("Reported irq attribute; active mode not established")),
            _ => {}
        }
        if data.pnp_state == "disabled" {
            details.push(tr("PnP device disabled"));
        }
        rows.push(ResourceRow {
            key: format!("{}:{}:{}:{}:{}:{}", r.source, type_id(r.kind), r.index, r.start, r.end, r.mode),
            kind,
            setting,
            details: details.join("; "),
            source: r.source.clone(),
            group: type_id(r.kind).to_string(),
        });
    }
    for problem in &data.issues {
        rows.push(ResourceRow {
            key: format!("issue:{}", problem.source),
            kind: tr("Resource metadata"),
            setting: property_read_value(&problem.error),
            details: tr("Read or parse failed"),
            source: problem.source.clone(),
            group: "issues".into(),
        });
    }
    // Preserve multiple identical reported rows without colliding projection keys.
    let mut duplicates: HashMap<String, usize> = HashMap::new();
    for row in &mut rows {
        let seen = duplicates.entry(row.key.clone()).or_insert(0);
        row.key = format!("{}:{seen}", row.key);
        *seen += 1;
    }
    rows
}

/// Equal content, treating an absent snapshot as an empty one.
pub fn same_device_resources(a: &Option<Arc<DeviceResources>>, b: &Option<Arc<DeviceResources>>) -> bool {
    if let (Some(x), Some(y)) = (a, b) {
        if Arc::ptr_eq(x, y) {
            return true;
        }
    }
    let empty = DeviceResources::default();
    a.as_deref().unwrap_or(&empty) == b.as_deref().unwrap_or(&empty)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::{parse_pci_resources, parse_pnp_resources};

    #[test]
    fn pci_rows_describe_bars_and_flags() {
        let data = parse_pci_resources("0x00000007c0000000 0x00000007cfffffff 0x00142200\n0x0000ef00 0x0000efff 0x00000101\n0 0xfff 0x20000200\n", "s");
        let rows = resource_rows(&data);
        assert_eq!(rows[0].kind, "Memory range");
        assert_eq!(rows[0].setting, "00000007C0000000 – 00000007CFFFFFFF");
        assert_eq!(rows[0].details, "BAR 0; Prefetchable; 64-bit memory; Flags: 0x142200");
        assert_eq!(rows[1].setting, "EF00 – EFFF");
        assert_eq!(rows[2].setting, "Unassigned");
        assert_eq!(rows[0].group, "0");
        assert!(rows[0].key.ends_with(":0"));
    }

    #[test]
    fn irq_modes_issues_and_duplicate_keys() {
        let mut data = parse_pnp_resources("state = disabled\nirq 5\nirq 5\n", "p");
        data.issues.push(crate::resources::ResourceIssue {
            source: "x/irq".into(),
            error: crate::model::Attribute { state: crate::model::ReadState::PermissionDenied, ..Default::default() },
        });
        let rows = resource_rows(&data);
        assert_eq!((rows[0].setting.as_str(), rows[0].details.as_str()), ("Disabled", "PnP device disabled"));
        assert_ne!(rows[0].key, rows[1].key, "identical rows keep distinct keys");
        let issue = rows.last().unwrap();
        assert_eq!((issue.group.as_str(), issue.setting.as_str()), ("issues", "Permission denied"));
    }

    #[test]
    fn same_resources_treats_none_as_empty() {
        let a = Some(Arc::new(parse_pnp_resources("state = active\nirq 5\n", "p")));
        let b = Some(Arc::new(parse_pnp_resources("state = active\nirq 5\n", "p")));
        assert!(same_device_resources(&a, &a.clone()));
        assert!(same_device_resources(&a, &b));
        assert!(!same_device_resources(&a, &None));
        assert!(same_device_resources(&None, &Some(Arc::new(DeviceResources::default()))));
    }
}
