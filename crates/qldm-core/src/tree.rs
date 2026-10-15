//! The device tree shown by the main window, ported from `src/ui/devicemodel.cpp` and
//! `src/ui/devicefilter.cpp` without any toolkit types.
//!
//! [`TreeModel`] keeps the inventory and projects it into one of seven views (by type,
//! connection, driver, resources). Every projection has a single computer root and counts
//! devices, never grouping rows. Nodes refer to devices by path and generation, so a reused
//! path can never inherit an old selection. [`Filter`] reproduces the recursive quick/deep
//! filter: a row stays when it matches or when something below it does.
use crate::i18n::{arg, arg2, tr};
use crate::label::device_display_name;
use crate::model::{category_label, reconcile, Device};
use crate::resourceformat::{resource_rows, same_device_resources};
use crate::search::{SearchMatch, SearchRecord};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum View {
    Type,
    Connection,
    DevicesByDriver,
    DriversByDevice,
    DriversByType,
    ResourcesByType,
    ResourcesByConnection,
}

impl View {
    pub const ALL: [View; 7] =
        [View::Type, View::Connection, View::DevicesByDriver, View::DriversByDevice, View::DriversByType, View::ResourcesByType, View::ResourcesByConnection];

    pub fn id(self) -> &'static str {
        match self {
            View::Type => "type",
            View::Connection => "connection",
            View::DevicesByDriver => "devicesByDriver",
            View::DriversByDevice => "driversByDevice",
            View::DriversByType => "driversByType",
            View::ResourcesByType => "resourcesByType",
            View::ResourcesByConnection => "resourcesByConnection",
        }
    }

    pub fn label(self) -> String {
        tr(match self {
            View::Type => "Devices by type",
            View::Connection => "Devices by connection",
            View::DevicesByDriver => "Devices by driver",
            View::DriversByDevice => "Drivers by device",
            View::DriversByType => "Drivers by type",
            View::ResourcesByType => "Resources by type",
            View::ResourcesByConnection => "Resources by connection",
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TreeNode {
    pub key: String,
    /// Display text, rebuilt with the inventory.
    pub label: String,
    pub category: String,
    /// Record lookup id; empty for synthetic rows (groups, driver rows, resources).
    pub path: String,
    /// Projection identity: a replaced device gets new nodes.
    pub generation: u64,
    /// Resource rows target this device but are not device records.
    pub owner_path: String,
    /// Plain metadata/provenance for tooltips.
    pub description: String,
    /// An ancestor kept only to show real connection context.
    pub context: bool,
    pub children: Vec<TreeNode>,
}

impl TreeNode {
    /// The node without its children.
    fn clone_shallow(&self) -> TreeNode {
        TreeNode { children: Vec::new(), ..self.clone() }
    }

    pub fn is_resource(&self) -> bool {
        !self.owner_path.is_empty()
    }
}

/// Same device metadata (generation excluded), so unchanged live snapshots change nothing.
fn same_metadata(a: &Device, b: &Device) -> bool {
    a.path == b.path
        && a.parent_path == b.parent_path
        && a.subsystem == b.subsystem
        && a.sysname == b.sysname
        && a.devtype == b.devtype
        && a.driver == b.driver
        && a.driver_module == b.driver_module
        && a.properties == b.properties
        && a.name == b.name
        && a.name_source == b.name_source
        && a.name_translated == b.name_translated
        && a.name_candidate == b.name_candidate
        && a.represented_by_path == b.represented_by_path
        && a.property_sources == b.property_sources
        && a.category == b.category
        && a.event_instance == b.event_instance
        && a.incarnation == b.incarnation
        && a.hidden == b.hidden
        && a.attributes == b.attributes
        && same_device_resources(&a.resources, &b.resources)
}

/// Builder arena: nodes by index, so parents can be looked up while a view is assembled.
#[derive(Default)]
struct Arena {
    nodes: Vec<TreeNode>,
    children: Vec<Vec<usize>>,
}

impl Arena {
    fn add(&mut self, parent: Option<usize>, key: String, label: String, category: &str, path: &str) -> usize {
        let index = self.nodes.len();
        self.nodes.push(TreeNode { key, label, category: category.into(), path: path.into(), ..Default::default() });
        self.children.push(Vec::new());
        if let Some(parent) = parent {
            self.children[parent].push(index);
        }
        index
    }

    /// Nests the arena into a tree, sorting siblings by label (case-insensitive) then key.
    fn into_tree(self, root: usize) -> TreeNode {
        let mut slots: Vec<Option<TreeNode>> = self.nodes.into_iter().map(Some).collect();
        fn build(index: usize, slots: &mut [Option<TreeNode>], children: &[Vec<usize>]) -> TreeNode {
            let mut node = slots[index].take().expect("each node is used once");
            node.children = children[index].iter().map(|c| build(*c, slots, children)).collect();
            node.children.sort_by(|a, b| a.label.to_lowercase().cmp(&b.label.to_lowercase()).then_with(|| a.key.cmp(&b.key)));
            node
        }
        build(root, &mut slots, &self.children)
    }
}

#[derive(Debug)]
pub struct TreeModel {
    devices: Vec<Device>,
    lookup: HashMap<String, usize>,
    root: Option<TreeNode>,
    show_internal: bool,
    view: View,
    generation: u64,
    visible: usize,
    hostname: String,
}

impl Default for TreeModel {
    fn default() -> Self {
        Self::new(crate::system::machine_hostname())
    }
}

impl TreeModel {
    pub fn new(hostname: impl Into<String>) -> Self {
        Self {
            devices: Vec::new(),
            lookup: HashMap::new(),
            root: None,
            show_internal: false,
            view: View::Type,
            generation: 0,
            visible: 0,
            hostname: hostname.into(),
        }
    }

    pub fn view(&self) -> View {
        self.view
    }

    pub fn show_internal(&self) -> bool {
        self.show_internal
    }

    pub fn visible_count(&self) -> usize {
        self.visible
    }

    pub fn inventory_count(&self) -> usize {
        self.devices.len()
    }

    /// The single computer root, once an inventory was set.
    pub fn root(&self) -> Option<&TreeNode> {
        self.root.as_ref()
    }

    pub fn set_view(&mut self, view: View) {
        if self.view != view {
            self.view = view;
            self.rebuild();
        }
    }

    pub fn set_show_internal(&mut self, show: bool) {
        if show != self.show_internal {
            self.show_internal = show;
            self.rebuild();
        }
    }

    fn computer_label(&self) -> String {
        if self.hostname.is_empty() {
            tr("This computer")
        } else {
            self.hostname.clone()
        }
    }

    /// Installs a new inventory. `removed_paths` (and their descendants) lose their identities;
    /// `identity_lost` drops every identity. Returns `false` when nothing observable changed.
    pub fn set_inventory(&mut self, mut devices: Vec<Device>, removed_paths: &HashSet<String>, identity_lost: bool) -> bool {
        let previous: Vec<Device> = if identity_lost {
            Vec::new()
        } else {
            self.devices
                .iter()
                .filter(|d| {
                    let mut path = d.path.as_str();
                    loop {
                        if removed_paths.contains(path) {
                            return false;
                        }
                        match path.rfind('/') {
                            Some(slash) => path = &path[..slash],
                            None => return true,
                        }
                    }
                })
                .cloned()
                .collect()
        };
        reconcile(&mut devices, &previous, &mut self.generation);
        let mut changed: HashSet<String> = HashSet::new();
        for d in &devices {
            match self.lookup.get(&d.path).map(|i| &self.devices[*i]) {
                Some(old) if old.generation == d.generation && same_metadata(d, old) => {}
                _ => {
                    changed.insert(d.path.clone());
                }
            }
        }
        // Grouped-record tooltips also change when a member disappears or moves.
        let representatives: HashMap<&str, &str> = devices.iter().map(|d| (d.path.as_str(), d.represented_by_path.as_str())).collect();
        for d in &self.devices {
            if representatives.get(d.path.as_str()) != Some(&d.represented_by_path.as_str()) {
                changed.insert(d.path.clone());
                if !d.represented_by_path.is_empty() {
                    changed.insert(d.represented_by_path.clone());
                }
            }
        }
        let additions: Vec<String> =
            devices.iter().filter(|d| changed.contains(&d.path) && !d.represented_by_path.is_empty()).map(|d| d.represented_by_path.clone()).collect();
        changed.extend(additions);
        let label = self.computer_label();
        if changed.is_empty() && self.root.as_ref().is_some_and(|r| r.label == label) {
            return false;
        }
        self.devices = devices;
        self.rebuild();
        true
    }

    pub fn device(&self, path: &str, generation: u64) -> Option<&Device> {
        self.lookup.get(path).map(|i| &self.devices[*i]).filter(|d| d.generation == generation)
    }

    /// Current generation of a device path, if present.
    pub fn generation_of(&self, path: &str) -> Option<u64> {
        self.lookup.get(path).map(|i| self.devices[*i].generation)
    }

    /// Index path (child numbers from the root) of the first node for a device instance.
    pub fn find_device(&self, path: &str, generation: u64) -> Option<Vec<usize>> {
        self.device(path, generation)?;
        let root = self.root.as_ref()?;
        fn walk(node: &TreeNode, path: &str, trail: &mut Vec<usize>) -> bool {
            for (i, child) in node.children.iter().enumerate() {
                trail.push(i);
                if child.path == path || walk(child, path, trail) {
                    return true;
                }
                trail.pop();
            }
            false
        }
        let mut trail = Vec::new();
        walk(root, path, &mut trail).then_some(trail)
    }

    /// Exact node of a device instance by key (device rows and resource rows alike).
    pub fn find_node(&self, key: &str, path: &str, generation: u64) -> Option<Vec<usize>> {
        self.device(path, generation)?;
        let root = self.root.as_ref()?;
        fn walk(node: &TreeNode, key: &str, path: &str, generation: u64, trail: &mut Vec<usize>) -> bool {
            for (i, child) in node.children.iter().enumerate() {
                trail.push(i);
                if (child.key == key && child.generation == generation && (child.path == path || child.owner_path == path))
                    || walk(child, key, path, generation, trail)
                {
                    return true;
                }
                trail.pop();
            }
            false
        }
        let mut trail = Vec::new();
        walk(root, key, path, generation, &mut trail).then_some(trail)
    }

    /// Visible devices in tree order, once each, with the label the tree shows for them.
    pub fn search_records(&self) -> Vec<SearchRecord> {
        let mut grouped: HashMap<&str, Vec<&str>> = HashMap::new();
        for d in &self.devices {
            if !d.represented_by_path.is_empty() {
                grouped.entry(d.represented_by_path.as_str()).or_default().push(d.path.as_str());
            }
        }
        let mut records = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        fn collect<'a>(
            node: &'a TreeNode,
            model: &'a TreeModel,
            grouped: &HashMap<&str, Vec<&str>>,
            seen: &mut HashSet<&'a str>,
            records: &mut Vec<SearchRecord>,
        ) {
            for child in &node.children {
                if !child.path.is_empty() && seen.insert(child.path.as_str()) {
                    let d = &model.devices[model.lookup[&child.path]];
                    records.push(SearchRecord {
                        device: d.clone(),
                        label: child.label.clone(),
                        grouped_paths: grouped.get(d.path.as_str()).map(|g| g.join("\n")).unwrap_or_default(),
                    });
                }
                collect(child, model, grouped, seen, records);
            }
        }
        if let Some(root) = &self.root {
            collect(root, self, &grouped, &mut seen, &mut records);
        }
        records
    }

    /// Tooltip for a node: provenance for synthetic rows, identity and grouping for devices.
    pub fn tooltip(&self, node: &TreeNode) -> String {
        if !node.description.is_empty() {
            return node.description.clone();
        }
        let Some(d) = self.device(if node.path.is_empty() { &node.owner_path } else { &node.path }, node.generation) else {
            return node.label.clone();
        };
        let efi_hidden = d.subsystem == "efivarfs" && !self.show_internal;
        let mut lines = vec![
            node.label.clone(),
            if efi_hidden { d.parent_path.clone() } else { d.path.clone() },
            format!("{}: {}", tr("Subsystem"), d.subsystem),
            format!("{}: {}", tr("Raw name source"), d.name_source),
            format!("{}: {}", tr("Raw name"), d.name),
            format!("{}: {}", tr("Kernel name"), if efi_hidden { &d.name } else { &d.sysname }),
        ];
        if d.subsystem == "power_supply" {
            for key in ["type", "manufacturer", "model_name"] {
                if let Some(a) = d.attributes.get(key).filter(|a| a.state == crate::model::ReadState::Available && !a.value.is_empty()) {
                    lines.push(format!("{key}: {}", a.value));
                }
            }
        }
        if node.context {
            lines.push(tr("Ancestor retained for connection context"));
        }
        if !d.represented_by_path.is_empty() {
            lines.push(format!("{}: {}", tr("Represented by"), d.represented_by_path));
        }
        let grouped: Vec<&str> = self.devices.iter().filter(|r| r.represented_by_path == d.path).map(|r| r.path.as_str()).collect();
        if !grouped.is_empty() {
            lines.push(format!("{}:\n{}", tr("Grouped records"), grouped.join("\n")));
        }
        lines.join("\n")
    }

    fn rebuild(&mut self) {
        self.lookup = self.devices.iter().enumerate().map(|(i, d)| (d.path.clone(), i)).collect();
        let show = self.show_internal;
        let mut members: Vec<&Device> = self.devices.iter().collect();
        members.sort_by(|a, b| a.path.cmp(&b.path));
        let mut labels: HashMap<&str, String> = HashMap::new();
        let mut counts: HashMap<String, usize> = HashMap::new();
        for d in &members {
            let label = device_display_name(d, show);
            if show || !d.hidden {
                *counts.entry(format!("{}:{label}", d.category)).or_default() += 1;
            }
            labels.insert(d.path.as_str(), label);
        }
        let mut namespace_numbers: HashMap<String, usize> = HashMap::new();
        for d in &members {
            let label = labels[d.path.as_str()].clone();
            if counts.get(&format!("{}:{label}", d.category)).copied().unwrap_or(0) <= 1 {
                continue;
            }
            if d.subsystem == "efivarfs" && !show {
                let n = namespace_numbers.entry(label.clone()).or_default();
                *n += 1;
                labels.insert(d.path.as_str(), arg2(tr("%1 (namespace %2)"), &label, &n.to_string()));
            } else {
                labels.insert(d.path.as_str(), device_display_name(d, true));
            }
        }
        members.sort_by(|a, b| labels[a.path.as_str()].to_lowercase().cmp(&labels[b.path.as_str()].to_lowercase()).then_with(|| a.path.cmp(&b.path)));

        let mut arena = Arena::default();
        let computer = arena.add(None, "computer".into(), self.computer_label(), "", "");
        let view = self.view;
        let device_node = |arena: &mut Arena, parent: usize, d: &Device| {
            let suffix = if view == View::ResourcesByType { format!(":{}", arena.nodes[parent].key) } else { String::new() };
            let index = arena.add(Some(parent), format!("device:{}{suffix}", d.path), labels[d.path.as_str()].clone(), d.category, &d.path);
            arena.nodes[index].generation = d.generation;
            index
        };
        let driver_key = |d: &Device| if d.driver.is_empty() { "unbound".to_string() } else { format!("{}:{}", d.subsystem, d.driver) };
        let driver_label = |d: &Device| {
            if d.driver.is_empty() {
                tr("No directly bound driver")
            } else {
                arg2(tr("%1: %2 (driver)"), &d.subsystem, &d.driver)
            }
        };
        let mut groups: HashMap<String, usize> = HashMap::new();
        let mut group = |arena: &mut Arena, parent: usize, key: String, label: String, category: &str| -> usize {
            if let Some(found) = groups.get(&key) {
                return *found;
            }
            let index = arena.add(Some(parent), key.clone(), label, category, "");
            groups.insert(key, index);
            index
        };
        let has_resources = |d: &Device| d.resources.as_ref().is_some_and(|r| r.has_information());
        let resource_node = |arena: &mut Arena, parent: usize, d: &Device, row: &crate::resourceformat::ResourceRow| {
            let mut label = arg2(tr("%1: %2"), &row.kind, &row.setting);
            if !row.details.is_empty() {
                label.push_str(&format!(" — {}", row.details));
            }
            let key = format!("{}/resource:{}", arena.nodes[parent].key, row.key);
            let index = arena.add(Some(parent), key, label.clone(), d.category, "");
            let description = format!("{label}\n{}\n{}", arg(tr("Device: %1"), &labels[d.path.as_str()]), arg(tr("Source: %1"), &row.source));
            let node = &mut arena.nodes[index];
            node.owner_path = d.path.clone();
            node.generation = d.generation;
            node.description = description;
        };
        let by_resource_type = view == View::ResourcesByType;
        let by_resource_connection = view == View::ResourcesByConnection;
        if by_resource_type {
            for d in &members {
                if (!show && d.hidden) || !has_resources(d) {
                    continue;
                }
                let mut device_groups: HashMap<String, usize> = HashMap::new();
                for row in resource_rows(d.resources.as_ref().expect("checked")) {
                    let category = group(&mut arena, computer, format!("resource-type:{}", row.group), row.kind.clone(), "other");
                    let item = *device_groups.entry(row.group.clone()).or_insert_with(|| device_node(&mut arena, category, d));
                    resource_node(&mut arena, item, d, &row);
                }
            }
        } else if view == View::Connection || by_resource_connection {
            // Only real inventory parent links; hidden ancestors provide context.
            let mut included: HashSet<&str> = HashSet::new();
            for d in &members {
                if (!show && d.hidden) || (by_resource_connection && !has_resources(d)) {
                    continue;
                }
                let mut path = d.path.as_str();
                let mut chain: HashSet<&str> = HashSet::new();
                while let Some(index) = self.lookup.get(path) {
                    if !chain.insert(path) {
                        break;
                    }
                    included.insert(path);
                    path = self.devices[*index].parent_path.as_str();
                }
            }
            let mut nodes: HashMap<String, usize> = HashMap::new();
            #[allow(clippy::too_many_arguments)] // Recursive helper over the view builder's local state.
            fn ensure(
                d: &Device,
                model: &TreeModel,
                included: &HashSet<&str>,
                arena: &mut Arena,
                nodes: &mut HashMap<String, usize>,
                building: &mut HashSet<String>,
                computer: usize,
                make: &dyn Fn(&mut Arena, usize, &Device) -> usize,
                show: bool,
            ) -> usize {
                if let Some(found) = nodes.get(&d.path) {
                    return *found;
                }
                building.insert(d.path.clone());
                let mut parent = computer;
                if included.contains(d.parent_path.as_str()) && !building.contains(&d.parent_path) {
                    let parent_device = &model.devices[model.lookup[&d.parent_path]];
                    parent = ensure(parent_device, model, included, arena, nodes, building, computer, make, show);
                }
                let item = make(arena, parent, d);
                arena.nodes[item].context = !show && d.hidden;
                nodes.insert(d.path.clone(), item);
                building.remove(&d.path);
                item
            }
            let mut building = HashSet::new();
            for d in &members {
                if !included.contains(d.path.as_str()) {
                    continue;
                }
                let item = ensure(d, self, &included, &mut arena, &mut nodes, &mut building, computer, &device_node, show);
                if by_resource_connection && (show || !d.hidden) && has_resources(d) {
                    for row in resource_rows(d.resources.as_ref().expect("checked")) {
                        resource_node(&mut arena, item, d, &row);
                    }
                }
            }
        } else {
            for d in &members {
                if !show && d.hidden {
                    continue;
                }
                let mut parent = computer;
                if view == View::Type || view == View::DriversByType {
                    parent = group(&mut arena, parent, format!("category:{}", d.category), category_label(d.category), d.category);
                }
                if view == View::DevicesByDriver || view == View::DriversByType {
                    let prefix = if view == View::DriversByType { format!("{}:", d.category) } else { String::new() };
                    parent = group(&mut arena, parent, format!("driver:{prefix}{}", driver_key(d)), driver_label(d), d.category);
                }
                let item = device_node(&mut arena, parent, d);
                if view != View::DriversByDevice {
                    continue;
                }
                let item_key = arena.nodes[item].key.clone();
                let driver = arena.add(Some(item), format!("{item_key}/driver"), driver_label(d), d.category, "");
                if !d.driver.is_empty() {
                    let label = if d.driver_module.is_empty() {
                        tr("Owning module / built-in status undetermined")
                    } else {
                        arg(tr("%1 (kernel module)"), &d.driver_module)
                    };
                    arena.add(Some(driver), format!("{item_key}/module"), label, d.category, "");
                }
                // Explicitly separate ancestor binding from the selected device's binding.
                let mut path = d.parent_path.as_str();
                let mut visited: HashSet<&str> = HashSet::from([d.path.as_str()]);
                while let Some(index) = self.lookup.get(path) {
                    if !visited.insert(path) {
                        break;
                    }
                    let ancestor = &self.devices[*index];
                    if !ancestor.driver.is_empty() {
                        let label = arg2(tr("Parent-device driver: %1 — %2"), &driver_label(ancestor), &labels[path]);
                        let relation = arena.add(Some(item), format!("{item_key}/parent-driver"), label, ancestor.category, "");
                        if !ancestor.driver_module.is_empty() {
                            arena.add(
                                Some(relation),
                                format!("{item_key}/parent-module"),
                                arg(tr("%1 (kernel module)"), &ancestor.driver_module),
                                ancestor.category,
                                "",
                            );
                        }
                        break;
                    }
                    path = ancestor.parent_path.as_str();
                }
            }
        }
        let tree = arena.into_tree(computer);
        let mut visible: HashSet<&str> = HashSet::new();
        fn count<'a>(node: &'a TreeNode, visible: &mut HashSet<&'a str>) {
            for child in &node.children {
                if !child.path.is_empty() {
                    visible.insert(child.path.as_str());
                }
                count(child, visible);
            }
        }
        count(&tree, &mut visible);
        self.visible = visible.len();
        self.root = Some(tree);
    }
}

/// Quick (display-name) or deep (metadata) filter state, applied to a [`TreeModel`] snapshot.
#[derive(Clone, Debug, Default)]
pub struct Filter {
    query: String,
    deep: bool,
    matches: HashMap<String, SearchMatch>,
}

impl Filter {
    pub fn set_query(&mut self, query: &str, deep: bool) {
        self.query = query.to_string();
        self.deep = deep;
        self.matches.clear();
    }

    pub fn add_matches(&mut self, matches: &[SearchMatch]) {
        for m in matches {
            self.matches.insert(m.path.clone(), m.clone());
        }
    }

    pub fn active(&self) -> bool {
        !self.query.is_empty()
    }

    pub fn deep(&self) -> bool {
        self.deep
    }

    fn contains(&self, text: &str) -> bool {
        text.to_lowercase().contains(&self.query.to_lowercase())
    }

    fn deep_match(&self, path: &str, generation: u64) -> bool {
        self.deep && self.matches.get(path).is_some_and(|m| m.generation == generation)
    }

    /// Row-level acceptance, before recursive retention of ancestors.
    fn accepts(&self, node: &TreeNode, ancestors: &[&TreeNode]) -> bool {
        if !self.active() || node.key == "computer" {
            return true;
        }
        if node.is_resource() && self.contains(&node.label) {
            return true;
        }
        if node.path.is_empty() {
            // Keep driver/module/resource children when their reporting device matches.
            for owner in ancestors.iter().rev() {
                if owner.path.is_empty() {
                    continue;
                }
                return self.contains(&owner.label) || self.deep_match(&owner.path, owner.generation);
            }
            return false;
        }
        self.contains(&node.label) || self.deep_match(&node.path, node.generation)
    }

    /// The tree with only matching rows and the ancestors that lead to them (recursive
    /// filtering: a row is kept when it matches or anything below it does).
    pub fn apply(&self, root: &TreeNode) -> TreeNode {
        fn filtered<'a>(node: &'a TreeNode, stack: &mut Vec<&'a TreeNode>, filter: &Filter) -> Option<TreeNode> {
            let accepted = filter.accepts(node, stack);
            stack.push(node);
            let children: Vec<TreeNode> = node.children.iter().filter_map(|c| filtered(c, stack, filter)).collect();
            stack.pop();
            (accepted || !children.is_empty()).then(|| TreeNode { children, ..node.clone_shallow() })
        }
        let mut stack = Vec::new();
        filtered(root, &mut stack, self).unwrap_or_else(|| root.clone_shallow())
    }

    /// Why a device row matched a deep search (`None` for a name match or an inactive filter).
    pub fn match_reason(&self, node: &TreeNode) -> Option<String> {
        if !self.active() || !self.deep || self.contains(&node.label) {
            return None;
        }
        self.matches.get(&node.path).filter(|m| m.generation == node.generation).map(|m| m.field.clone())
    }
}

/// Unique devices shown in a (filtered) tree; grouping, driver and resource rows do not count.
pub fn visible_device_count(root: &TreeNode) -> usize {
    fn walk<'a>(node: &'a TreeNode, paths: &mut HashSet<&'a str>) {
        for child in &node.children {
            if !child.path.is_empty() {
                paths.insert(child.path.as_str());
            }
            walk(child, paths);
        }
    }
    let mut paths = HashSet::new();
    walk(root, &mut paths);
    paths.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ReadState;
    use crate::resources::{DeviceResource, DeviceResources, ResourceType};
    use std::sync::Arc;

    fn fixture() -> Device {
        let mut d = Device {
            path: "/sys/devices/fixture".into(),
            incarnation: "fixture:1".into(),
            name: "Visible device".into(),
            name_source: "udev: ID_MODEL".into(),
            category: "other",
            subsystem: "pci".into(),
            sysname: "fixture".into(),
            ..Default::default()
        };
        d.properties.insert("ID_SERIAL".into(), "CaseSensitiveSerial".into());
        d
    }
    fn none() -> HashSet<String> {
        HashSet::new()
    }
    fn model_with(devices: Vec<Device>) -> TreeModel {
        let mut model = TreeModel::new("testhost");
        model.set_inventory(devices, &none(), false);
        model
    }
    fn gen(model: &TreeModel, path: &str) -> u64 {
        model.generation_of(path).expect("device present")
    }
    fn node<'a>(model: &'a TreeModel, trail: &[usize]) -> &'a TreeNode {
        trail.iter().fold(model.root().expect("root"), |n, i| &n.children[*i])
    }
    fn filtered(model: &TreeModel, filter: &Filter) -> TreeNode {
        filter.apply(model.root().unwrap())
    }
    fn count(model: &TreeModel, filter: &Filter) -> usize {
        visible_device_count(&filtered(model, filter))
    }

    #[test]
    fn search_inputs_exclude_hidden_devices_and_name_search_ignores_metadata() {
        let device = fixture();
        let hidden = Device { path: format!("{}/hidden", device.path), hidden: true, ..device.clone() };
        let mut model = model_with(vec![device.clone(), hidden]);
        let records = model.search_records();
        assert_eq!(records.len(), 1, "hidden devices are excluded from search inputs");
        let record = &records[0];
        let mut filter = Filter::default();
        filter.set_query("casesensitiveserial", false);
        let shown = filtered(&model, &filter);
        assert_eq!((visible_device_count(&shown), shown.children.len()), (0, 0), "name search ignores metadata; the computer root stays");
        filter.set_query("casesensitiveserial", true);
        filter.add_matches(&[SearchMatch { path: record.device.path.clone(), generation: record.device.generation + 1, field: "ID_SERIAL".into() }]);
        assert_eq!(count(&model, &filter), 0, "a stale generation cannot match a reused path");
        filter.add_matches(&[SearchMatch { path: record.device.path.clone(), generation: record.device.generation, field: "ID_SERIAL".into() }]);
        let shown = filtered(&model, &filter);
        assert_eq!((visible_device_count(&shown), shown.children.len()), (1, 1), "a deep match keeps its category");
        assert_eq!(filter.match_reason(&shown.children[0].children[0]).as_deref(), Some("ID_SERIAL"));
        filter.set_query(&record.label.to_uppercase(), false);
        assert_eq!(count(&model, &filter), 1, "name search uses the exact displayed label, case-insensitively");
        filter.set_query("", false);
        assert_eq!(count(&model, &filter), 1);
        model.set_show_internal(true);
        assert_eq!((model.search_records().len(), count(&model, &filter)), (2, 2), "internal visibility applies to both modes");
    }

    fn views_fixture() -> (Device, Device, Device) {
        let mut pci = fixture();
        pci.driver = "shared".into();
        pci.driver_module = "owner".into();
        let usb = Device {
            path: "/sys/devices/usb-fixture".into(),
            name: "USB fixture".into(),
            subsystem: "usb".into(),
            driver: "shared".into(),
            driver_module: "owner".into(),
            ..pci.clone()
        };
        let child = Device {
            path: format!("{}/child", pci.path),
            parent_path: pci.path.clone(),
            name: "Child fixture".into(),
            driver: String::new(),
            driver_module: String::new(),
            ..pci.clone()
        };
        (pci, usb, child)
    }

    #[test]
    fn connection_view_keeps_real_ancestry_and_hidden_context() {
        let (pci, usb, child) = views_fixture();
        let context = Device { hidden: true, ..pci.clone() };
        let mut model = model_with(vec![context, child.clone(), usb]);
        model.set_view(View::Connection);
        assert_eq!(model.visible_count(), 3, "hidden ancestor kept as context");
        let trail = model.find_device(&child.path, gen(&model, &child.path)).unwrap();
        assert_eq!(node(&model, &trail[..trail.len() - 1]).path, pci.path, "the actual parent record");
        let context_node = node(&model, &trail[..trail.len() - 1]);
        assert!(context_node.context && model.tooltip(context_node).contains("Ancestor retained for connection context"));
        let mut filter = Filter::default();
        filter.set_query("Child fixture", false);
        assert_eq!(count(&model, &filter), 2, "matching device keeps its ancestry");
    }

    #[test]
    fn driver_projections_stay_bus_qualified_and_never_inflate_counts() {
        let (pci, usb, child) = views_fixture();
        let mut model = model_with(vec![pci.clone(), child.clone(), usb.clone()]);
        let mut filter = Filter::default();
        model.set_view(View::DevicesByDriver);
        assert_eq!(model.root().unwrap().children.len(), 3, "same driver name on two buses is distinct from the unbound group");
        assert_eq!((model.visible_count(), count(&model, &filter)), (3, 3));
        model.set_view(View::DriversByDevice);
        let trail = model.find_device(&child.path, gen(&model, &child.path)).unwrap();
        let child_node = node(&model, &trail);
        assert_eq!(child_node.children.len(), 2, "unbound child has a separate parent-driver relationship");
        assert!(child_node.children.iter().all(|c| c.path.is_empty()), "synthetic rows cannot retarget Properties");
        assert_eq!((model.search_records().len(), count(&model, &filter)), (3, 3), "module rows are ignored");
        model.set_view(View::DriversByType);
        let computer = model.root().unwrap();
        assert!(computer.children.len() == 1 && computer.children[0].children.len() == 3, "bus-qualified groups nest within category");
        assert!(model.find_device(&child.path, gen(&model, &child.path)).is_some(), "instance selection survives view changes");

        let unknown = Device { driver_module: String::new(), ..pci.clone() };
        model.set_inventory(vec![unknown, child, usb], &none(), false);
        model.set_view(View::DriversByDevice);
        let trail = model.find_device(&pci.path, gen(&model, &pci.path)).unwrap();
        let driver_row = &node(&model, &trail).children[0];
        assert!(driver_row.children[0].label.contains("undetermined"), "a missing module link proves nothing");
        filter.set_query("Visible device", false);
        let shown = filtered(&model, &filter);
        fn locate<'a>(node: &'a TreeNode, path: &str) -> Option<&'a TreeNode> {
            node.children.iter().find_map(|c| if c.path == path { Some(c) } else { locate(c, path) })
        }
        let kept = locate(&shown, &pci.path).expect("the matching device is kept");
        assert!(kept.children.len() == 1 && kept.children[0].children.len() == 1, "filtering keeps the driver and module-status rows");
    }

    #[test]
    fn every_projection_has_one_computer_root_and_counts_devices_only() {
        let (pci, usb, child) = views_fixture();
        let mut model = model_with(vec![pci, child, usb]);
        for view in [View::Type, View::Connection, View::DevicesByDriver, View::DriversByDevice, View::DriversByType] {
            model.set_view(view);
            let root = model.root().unwrap();
            assert!(root.key == "computer" && root.path.is_empty() && root.label == "testhost");
            assert_eq!((model.visible_count(), model.search_records().len()), (3, 3), "{view:?}");
        }
        model.set_view(View::Type);
        assert_eq!(model.visible_count(), 3);
        assert_eq!(TreeModel::new("").computer_label(), "This computer");
    }

    #[test]
    fn live_updates_keep_instances_and_replace_removed_ones() {
        let (pci, usb, child) = views_fixture();
        let mut model = model_with(vec![pci.clone(), child.clone(), usb.clone()]);
        let pci_generation = gen(&model, &pci.path);
        assert!(!model.set_inventory(vec![pci.clone(), child.clone(), usb.clone()], &none(), false), "unchanged snapshots change nothing");
        let renamed = Device { name: "ZZ renamed fixture".into(), ..pci.clone() };
        assert!(model.set_inventory(vec![renamed.clone(), child.clone(), usb.clone()], &none(), false));
        assert_eq!(gen(&model, &pci.path), pci_generation, "a rename keeps the instance");
        assert!(model.search_records().iter().any(|r| r.label.contains("ZZ renamed")));
        // Remove/add at the same path, even with identical inode hints, replaces the parent
        // and its descendants.
        let old_child = gen(&model, &child.path);
        let removed: HashSet<String> = [pci.path.clone()].into();
        model.set_inventory(vec![renamed.clone(), child.clone(), usb.clone()], &removed, false);
        assert_ne!(gen(&model, &pci.path), pci_generation);
        assert_ne!(gen(&model, &child.path), old_child);
        assert!(model.device(&pci.path, pci_generation).is_none());
        // Event loss invalidates everything.
        let usb_generation = gen(&model, &usb.path);
        model.set_inventory(vec![renamed.clone(), child.clone(), usb.clone()], &none(), true);
        assert_ne!(gen(&model, &usb.path), usb_generation);
        for view in [View::Type, View::Connection, View::DevicesByDriver, View::DriversByDevice, View::DriversByType] {
            model.set_view(view);
            let stable = gen(&model, &usb.path);
            model.set_inventory(vec![renamed.clone(), usb.clone()], &none(), false);
            assert!(gen(&model, &usb.path) == stable && model.visible_count() == 2, "removal keeps unrelated rows in {view:?}");
            model.set_inventory(vec![renamed.clone(), child.clone(), usb.clone()], &none(), false);
            assert!(gen(&model, &usb.path) == stable && model.visible_count() == 3, "addition keeps unrelated rows in {view:?}");
        }
    }

    fn io_resources(source: &str, start: u64) -> Arc<DeviceResources> {
        let item = |kind, start, end, mode: &str, pci, index, flags, suffix: &str| DeviceResource {
            kind,
            start,
            end,
            mode: mode.into(),
            pci,
            index,
            flags,
            source: format!("{source}/{suffix}"),
            ..Default::default()
        };
        Arc::new(DeviceResources {
            items: vec![
                item(ResourceType::Irq, start, start, "msix", false, -1, 0, &format!("msi_irqs/{start}")),
                item(ResourceType::Memory, 0x8000_0000, 0x8000_0fff, "", true, 0, 0x0010_2200, "resource"),
            ],
            ..Default::default()
        })
    }

    #[test]
    fn resource_views_group_by_type_deduplicate_devices_and_follow_ancestry() {
        let (pci_base, usb, child_base) = views_fixture();
        let bus = Device { path: "/sys/devices/pci-root".into(), name: "Real parent context".into(), hidden: true, resources: None, ..pci_base.clone() };
        let pci = Device { resources: Some(io_resources(&pci_base.path, 121)), parent_path: bus.path.clone(), ..pci_base.clone() };
        let child = Device { resources: Some(io_resources(&child_base.path, 121)), ..child_base.clone() };
        let mut model = model_with(vec![bus.clone(), pci.clone(), child.clone(), usb.clone()]);
        model.set_view(View::ResourcesByType);
        assert_eq!(model.root().unwrap().children.len(), 2, "memory and IRQ groups");
        assert_eq!((model.visible_count(), model.search_records().len()), (2, 2), "devices deduplicated across type memberships");
        let pci_generation = gen(&model, &pci.path);
        let trail = model.find_device(&pci.path, pci_generation).unwrap();
        let resource = &node(&model, &trail).children[0];
        assert!(resource.is_resource() && resource.path.is_empty());
        assert!(resource.owner_path == pci.path && resource.generation == pci_generation, "instance-scoped owner, not a device record");
        assert!(resource.description.contains("Source:") && resource.label.contains("MSI-X") || resource.label.contains("BAR 0"));
        let key = resource.key.clone();
        assert!(model.find_node(&key, &pci.path, pci_generation).is_some(), "exact node restored by generation");

        // Equivalent owned snapshots rebuild nothing; a changed assignment keeps the device.
        let clone = Device { resources: Some(Arc::new((**pci.resources.as_ref().unwrap()).clone())), ..pci.clone() };
        assert!(!model.set_inventory(vec![bus.clone(), clone, child.clone(), usb.clone()], &none(), false));
        let reassigned = Device { resources: Some(io_resources(&pci.path, 122)), ..pci.clone() };
        assert!(model.set_inventory(vec![bus.clone(), reassigned, child.clone(), usb.clone()], &none(), false));
        assert_eq!(gen(&model, &pci.path), pci_generation, "resource-only changes keep the reporting device");
        assert!(model.find_node(&key, &pci.path, pci_generation).is_none(), "the old assignment row is gone");
        model.set_inventory(vec![bus.clone(), pci.clone(), child.clone(), usb.clone()], &none(), false);

        let mut filter = Filter::default();
        filter.set_query("121", false);
        assert_eq!(count(&model, &filter), 2, "resource settings are found and both shared owners kept");
        filter.set_query("", false);

        model.set_view(View::ResourcesByConnection);
        let trail = model.find_device(&pci.path, gen(&model, &pci.path)).unwrap();
        assert_eq!(node(&model, &trail[..trail.len() - 1]).path, bus.path, "real hidden ancestry");
        assert!(model.visible_count() == 3 && model.search_records().len() == 3, "unrelated no-resource devices are excluded");
        let child_trail = model.find_device(&child.path, gen(&model, &child.path)).unwrap();
        assert_eq!(&child_trail[..child_trail.len() - 1], trail.as_slice(), "children follow recorded parents");
        assert_eq!(node(&model, &child_trail).children.len(), 2, "direct resource rows under the device");
        filter.set_query("121", false);
        assert_eq!(count(&model, &filter), 3, "resource filtering keeps connection-context ancestors");
    }

    #[test]
    fn equal_labels_are_disambiguated_and_efi_namespaces_numbered() {
        let a = Device { path: "/sys/devices/a".into(), sysname: "a0".into(), ..fixture() };
        let b = Device { path: "/sys/devices/b".into(), sysname: "b0".into(), ..fixture() };
        let model = model_with(vec![a, b]);
        let labels: Vec<&str> = model.root().unwrap().children[0].children.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["Visible device [a0]", "Visible device [b0]"]);

        let efi = |path: &str, guid: &str| {
            let mut d = Device {
                path: path.into(),
                subsystem: "efivarfs".into(),
                category: "efi",
                name: "SecureBoot".into(),
                sysname: path.rsplit('/').next().unwrap().into(),
                hidden: true,
                incarnation: "1:1".into(),
                ..Default::default()
            };
            d.properties.insert("EFI_VENDOR_GUID".into(), guid.into());
            d
        };
        let mut model = TreeModel::new("h");
        model.set_show_internal(true);
        model.set_inventory(
            vec![efi("/sys/firmware/efi/efivars/SecureBoot-1", "guid1"), efi("/sys/firmware/efi/efivars/SecureBoot-2", "guid2")],
            &none(),
            false,
        );
        let labels: Vec<&str> = model.root().unwrap().children[0].children.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["Secure Boot [guid1]", "Secure Boot [guid2]"], "internal view shows the vendor GUID");
        model.set_show_internal(false);
        assert_eq!(model.visible_count(), 0, "firmware records follow the internal toggle");
    }

    #[test]
    fn tooltips_describe_identity_and_grouping() {
        let wifi = Device { path: "/sys/devices/pci/wifi".into(), sysname: "wifi".into(), ..fixture() };
        let net = Device {
            path: format!("{}/net/wlo1", wifi.path),
            subsystem: "net".into(),
            sysname: "wlo1".into(),
            name: "wlo1".into(),
            represented_by_path: wifi.path.clone(),
            hidden: true,
            category: "network",
            ..fixture()
        };
        let model = model_with(vec![wifi.clone(), net.clone()]);
        let trail = model.find_device(&wifi.path, gen(&model, &wifi.path)).unwrap();
        let tooltip = model.tooltip(node(&model, &trail));
        assert!(tooltip.contains("Subsystem: pci") && tooltip.contains("Raw name source: udev: ID_MODEL"), "{tooltip}");
        assert!(tooltip.contains("Grouped records:") && tooltip.contains(&net.path), "{tooltip}");
        let root = model.root().unwrap();
        assert_eq!(model.tooltip(root), "testhost", "synthetic rows fall back to their label");
        let _ = ReadState::Available;
    }
}
