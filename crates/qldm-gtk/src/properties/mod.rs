//! The Properties window, ported from `src/ui/propertiesdialog.cpp`.
//!
//! A tabbed read-only report of one device instance: General and Driver facts, the specialised
//! hardware tabs (Network, Display, Camera, USB, PCIe, Battery), Details with advanced values,
//! Resources, Storage with volumes and cached health, and live udev Events. What each tab says
//! is built by `qldm_core` (`propertytext`, `report`, `storage_presentation`, `events_view`);
//! this module only presents it. The dialog is bound to one device instance (path, generation)
//! and never retargets itself: when the instance disappears it becomes a read-only snapshot.
pub(crate) mod widgets;

use gtk::glib;
use gtk::prelude::*;
use qldm_core::events::{DeviceEvent, DeviceEventsSnapshot};
use qldm_core::events_view::{event_row, event_text, events_copy_text, events_unchanged, update_events};
use qldm_core::i18n::{arg, arg2, tr};
use qldm_core::label::device_display_name;
use qldm_core::model::{Device, ReadState};
use qldm_core::properties::DeviceProperties;
use qldm_core::propertytext::{details_property_status, is_details_property, property_entries, property_read_value, PropertyEntry};
use qldm_core::report::{self, Report};
use qldm_core::resourceformat::resource_rows;
use qldm_core::storage_presentation::{storage_fields_text, storage_presentation, storage_rows_text, StoragePresentation};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use widgets::{add_field, copy_button, copy_to_clipboard, field_grid, groups_box, notice, plain_label, scrolled, storage_fields_grid, CellRow, TextTable};

struct State {
    snapshot: DeviceProperties,
    removed: bool,
    entries: Vec<PropertyEntry>,
    storage: StoragePresentation,
    displayed_events: Vec<DeviceEvent>,
    event_scope: String,
    // Selections that survive a rebuild.
    page: String,
    detail_id: String,
    volume_id: String,
    health_id: String,
}

pub struct PropertiesDialog {
    window: gtk::Window,
    banner: gtk::Label,
    notebook: gtk::Notebook,
    reload: gtk::Button,
    device: Device,
    state: RefCell<State>,
    advanced: Cell<bool>,
    // The Events page keeps its widgets across rebuilds so live updates do not disturb it.
    events_page: gtk::Box,
    events_notice: gtk::Label,
    events_table: TextTable,
    events_selection: gtk::SelectionModel,
    events_details: gtk::TextView,
    on_reload: Box<dyn Fn()>,
}

fn text_view() -> gtk::TextView {
    gtk::TextView::builder().editable(false).cursor_visible(false).wrap_mode(gtk::WrapMode::WordChar).left_margin(6).right_margin(6).top_margin(6).build()
}

fn button_row(buttons: &[&gtk::Button]) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.set_margin_start(8);
    row.set_margin_end(8);
    row.set_margin_top(6);
    row.set_margin_bottom(6);
    for button in buttons {
        row.append(*button);
    }
    row
}

fn page_with_notice(notice_text: &str, tip: &str, body: &impl IsA<gtk::Widget>, copy: &gtk::Button) -> gtk::Box {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    page.append(&notice(notice_text, tip));
    page.append(&scrolled(body));
    page.append(&button_row(&[copy]));
    page
}

fn report_page(report: &Report, notice_text: &str, tip: &str, copy_label: &str) -> gtk::Box {
    let body = groups_box(&report.groups);
    for table in &report.tables {
        let frame = gtk::Frame::new(Some(&table.title));
        let (view, _) = TextTable::new(&table.headers, false, &[]);
        view.set_rows(table.rows.iter().zip(&table.sources).map(|(r, s)| CellRow::new(r.clone(), s)).collect());
        let height = (table.rows.len() as i32 * 40).clamp(160, 420);
        let inner = gtk::ScrolledWindow::builder().child(&view.view).min_content_height(height).hexpand(true).build();
        frame.set_child(Some(&inner));
        body.append(&frame);
    }
    let text = report.copy_text.clone();
    page_with_notice(notice_text, tip, &body, &copy_button(copy_label, move || text.clone()))
}

impl PropertiesDialog {
    pub fn new(parent: &impl IsA<gtk::Window>, device: Device, on_reload: Box<dyn Fn()>) -> Rc<Self> {
        let window = gtk::Window::builder()
            .transient_for(parent)
            .title(arg(tr("%1 — Properties"), &device_display_name(&device, false)))
            .default_width(680)
            .default_height(560)
            .build();
        let banner = plain_label(&tr("Loading properties…"));
        banner.set_margin_start(8);
        banner.set_margin_end(8);
        banner.set_margin_top(6);
        let notebook = gtk::Notebook::new();
        notebook.set_vexpand(true);
        notebook.set_scrollable(true);
        let reload = gtk::Button::with_label(&tr("Reload properties"));
        let close = gtk::Button::with_label(&tr("Close"));
        let bottom = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        bottom.set_halign(gtk::Align::End);
        bottom.set_margin_top(6);
        bottom.set_margin_bottom(6);
        bottom.set_margin_end(8);
        bottom.append(&reload);
        bottom.append(&close);
        let layout = gtk::Box::new(gtk::Orientation::Vertical, 0);
        layout.append(&banner);
        layout.append(&notebook);
        layout.append(&bottom);
        window.set_child(Some(&layout));

        // The long-lived Events page.
        let events_page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let events_notice = notice(&tr("ℹ️ Live udev events since the application started"), "");
        events_page.append(&events_notice);
        let (events_table, events_selection) = TextTable::new(&[tr("Observed time"), tr("Event type"), tr("Description")], true, &[]);
        events_page.append(&scrolled(&events_table.view));
        let events_details = text_view();
        events_page.append(&gtk::ScrolledWindow::builder().child(&events_details).min_content_height(120).build());

        let this = Rc::new(Self {
            window,
            banner,
            notebook,
            reload,
            state: RefCell::new(State {
                snapshot: DeviceProperties { device: device.clone(), ..Default::default() },
                removed: false,
                entries: Vec::new(),
                storage: StoragePresentation::default(),
                displayed_events: Vec::new(),
                event_scope: String::new(),
                page: String::new(),
                detail_id: String::new(),
                volume_id: String::new(),
                health_id: String::new(),
            }),
            device,
            advanced: Cell::new(false),
            events_page,
            events_notice,
            events_table,
            events_selection,
            events_details,
            on_reload,
        });
        let weak = Rc::downgrade(&this);
        this.reload.connect_clicked({
            let weak = weak.clone();
            move |_| {
                if let Some(d) = weak.upgrade() {
                    (d.on_reload)();
                }
            }
        });
        let window = this.window.clone();
        close.connect_clicked(move |_| window.close());
        this.events_selection.connect_selection_changed({
            let weak = weak.clone();
            move |_, _, _| {
                if let Some(d) = weak.upgrade() {
                    d.show_event();
                }
            }
        });
        // Events page buttons.
        let copy_selection = gtk::Button::with_label(&tr("Copy selection"));
        let copy_all = gtk::Button::with_label(&tr("Copy all events"));
        copy_selection.connect_clicked({
            let weak = weak.clone();
            move |b| {
                if let Some(d) = weak.upgrade() {
                    copy_to_clipboard(b, &d.events_copy(true));
                }
            }
        });
        copy_all.connect_clicked({
            let weak = weak.clone();
            move |b| {
                if let Some(d) = weak.upgrade() {
                    copy_to_clipboard(b, &d.events_copy(false));
                }
            }
        });
        this.events_page.append(&button_row(&[&copy_selection, &copy_all]));
        this.rebuild();
        this
    }

    pub fn window(&self) -> &gtk::Window {
        &self.window
    }

    pub fn device(&self) -> &Device {
        &self.device
    }

    pub fn is_removed(&self) -> bool {
        self.state.borrow().removed
    }

    /// Titles of the tabs currently shown.
    pub fn tab_titles(&self) -> Vec<String> {
        (0..self.notebook.n_pages())
            .filter_map(|n| self.notebook.nth_page(Some(n)))
            .filter_map(|page| self.notebook.tab_label_text(&page).map(|t| t.to_string()))
            .collect()
    }

    /// Whether the first property read is still outstanding.
    pub fn is_loading(&self) -> bool {
        self.banner.is_visible() && self.banner.text() == tr("Loading properties…")
    }

    pub fn banner_text(&self) -> String {
        if self.banner.is_visible() {
            self.banner.text().to_string()
        } else {
            String::new()
        }
    }

    /// Selects a tab by title (test hook).
    pub fn show_tab(&self, title: &str) -> bool {
        for n in 0..self.notebook.n_pages() {
            let matches = self.notebook.nth_page(Some(n)).and_then(|p| self.notebook.tab_label_text(&p)).is_some_and(|t| t == title);
            if matches {
                self.notebook.set_current_page(Some(n));
                return true;
            }
        }
        false
    }

    pub fn present(&self) {
        self.window.present();
    }

    pub fn set_busy(&self, busy: bool) {
        let removed = self.is_removed();
        self.reload.set_sensitive(!busy && !removed);
        if busy && !removed {
            self.banner.set_text(&tr("Loading properties…"));
            self.banner.set_visible(true);
        }
    }

    /// Accepts a finished property read for this exact instance.
    pub fn accept_result(self: &Rc<Self>, result: DeviceProperties) {
        if self.is_removed() || result.device.path != self.device.path || result.device.generation != self.device.generation {
            return;
        }
        self.set_busy(false);
        if result.state == ReadState::Removed {
            self.mark_removed();
            return;
        }
        if result.state != ReadState::Available {
            self.banner.set_visible(true);
            self.banner.set_text(&arg(
                tr("Properties could not be refreshed: %1. Showing the previous snapshot."),
                &property_read_value(&qldm_core::model::Attribute { state: result.state, value: String::new(), error: result.error }),
            ));
            return;
        }
        self.state.borrow_mut().snapshot = result;
        self.banner.set_text("");
        self.banner.set_visible(false);
        self.rebuild();
    }

    /// Called with the current inventory record of this device (or `None`): a replaced or missing
    /// instance turns the dialog into a read-only snapshot. An inventory refresh never silently
    /// retargets it or reloads it.
    pub fn reconcile_instance(self: &Rc<Self>, current: Option<&Device>) {
        if self.is_removed() {
            return;
        }
        if current.is_none_or(|c| c.generation != self.device.generation || c.incarnation != self.device.incarnation) {
            self.mark_removed();
        }
    }

    pub fn mark_removed(self: &Rc<Self>) {
        {
            let mut state = self.state.borrow_mut();
            state.removed = true;
            state.event_scope.push('\n');
            state.event_scope.push_str(&tr("Removed/replaced device: retained events are a read-only instance snapshot."));
            self.events_notice.set_tooltip_text(Some(&state.event_scope));
        }
        self.reload.set_sensitive(false);
        self.banner.set_visible(true);
        self.banner.set_text(&tr("Device removed or replaced. This dialog is a read-only snapshot; close it and reopen Properties for the current device."));
        self.rebuild();
    }

    // ---- pages ----

    fn rebuild(self: &Rc<Self>) {
        let (previous_page, entries, storage) = {
            let mut state = self.state.borrow_mut();
            state.entries = property_entries(&state.snapshot, state.removed);
            state.storage = storage_presentation(&state.snapshot.storage, &self.device.path);
            (state.page.clone(), state.entries.clone(), state.storage.clone())
        };
        let page_label = |n: u32| self.notebook.nth_page(Some(n)).and_then(|p| self.notebook.tab_label_text(&p)).map(|t| t.to_string()).unwrap_or_default();
        let current = self.notebook.current_page().map(page_label).unwrap_or_default();
        let remember = if current.is_empty() { previous_page } else { current };
        while self.notebook.n_pages() > 0 {
            self.notebook.remove_page(Some(0));
        }
        let snapshot = self.state.borrow().snapshot.clone();
        let add = |title: String, page: &gtk::Box| {
            self.notebook.append_page(page, Some(&gtk::Label::new(Some(&title))));
        };
        add(tr("General"), &self.form_page(&entries, 0));
        add(tr("Driver"), &self.form_page(&entries, 1));
        let tip_net = tr("Addresses belong to the application's network namespace. Reload properties to refresh. Hardware MAC is the permanent address reported by the driver. An overridden MAC is the current address when it differs from the permanent address, or Linux reports a random, inherited or explicitly set address.");
        if let Some(r) = report::network_report(&snapshot) {
            add(tr("Network"), &report_page(&r, &tr("ℹ️ Network interface snapshot"), &tip_net, &tr("Copy network properties")));
        }
        if let Some(r) = report::display_report(&snapshot) {
            let tip = tr("Monitor information is reported by this connector’s cached EDID and kernel mode list; it does not guarantee capabilities. Active settings are shown separately below. Resolution and refresh rate describe the configured physical mode. Desktop color depth is not the panel or link depth. Wayland scale derived from logical size may include rounding; integer scale is a fallback. Reload properties to refresh. Unmatched or ambiguous connector names remain unavailable.");
            add(tr("Display"), &report_page(&r, &tr("ℹ️ Monitor hardware and display configuration snapshot"), &tip, &tr("Copy display properties")));
        }
        if let Some(r) = report::camera_report(&snapshot) {
            let tip = tr("Queries only: no capture, format changes or measured frame rates. Reload properties to refresh. Rows preserve the format, size and frame interval relationship. Size ranges do not imply a common rate list.");
            add(tr("Camera"), &report_page(&r, &tr("ℹ️ Driver-reported camera capabilities snapshot"), &tip, &tr("Copy camera properties")));
        }
        if let Some(r) = report::usb_report(&snapshot) {
            let tip = tr("Power information is declared by the active USB configuration, not measured consumption. USB version and speed are also available as raw values on Details. Reload properties to refresh.");
            add(tr("USB"), &report_page(&r, &tr("ℹ️ USB configuration snapshot"), &tip, &tr("Copy USB properties")));
        }
        if let Some(r) = report::pcie_report(&snapshot) {
            let tip = tr("Current link readings can reflect power management and topology; a lower speed is not a fault verdict. Device maxima do not establish the capacity of the upstream path. Reading these sysfs attributes may temporarily runtime-resume the device. Reload properties to refresh.");
            add(tr("PCIe"), &report_page(&r, &tr("ℹ️ PCIe link snapshot for this device"), &tip, &tr("Copy PCIe properties")));
        }
        if let Some(r) = report::battery_report(&snapshot) {
            let tip = tr("Readings are reported by the kernel and battery firmware. Capacity retention is an estimate from compatible full/design values, not a complete health assessment. Energy and charge units remain separate. Reload properties to refresh; no background monitoring or battery settings are changed.");
            add(tr("Battery"), &report_page(&r, &tr("ℹ️ Battery snapshot for this device"), &tip, &tr("Copy Battery properties")));
        }
        add(tr("Details"), &self.details_page(&entries));
        add(tr("Events"), &self.events_page);
        if snapshot.resources.has_information() {
            add(tr("Resources"), &self.resources_page(&snapshot));
        }
        if snapshot.storage.applicable {
            self.storage_pages(&storage, &add);
        }
        // Restore the tab the user was on.
        for n in 0..self.notebook.n_pages() {
            if !remember.is_empty() && page_label(n) == remember {
                self.notebook.set_current_page(Some(n));
            }
        }
        self.state.borrow_mut().page = remember;
    }

    fn form_page(&self, entries: &[PropertyEntry], tab: i32) -> gtk::Box {
        let grid = field_grid();
        for (row, entry) in entries.iter().filter(|e| e.tab == tab).enumerate() {
            add_field(&grid, row as i32, &entry.label, &entry.value, &entry.source);
        }
        let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        page.append(&scrolled(&grid));
        page
    }

    fn details_page(self: &Rc<Self>, entries: &[PropertyEntry]) -> gtk::Box {
        let shown: Vec<&PropertyEntry> = entries.iter().filter(|e| is_details_property(e, &self.device) && (!e.advanced || self.advanced.get())).collect();
        // Readable values first, then those with a status such as "unavailable".
        let (available, other): (Vec<_>, Vec<_>) = shown.iter().copied().partition(|e| details_property_status(e).is_empty());
        let ordered: Vec<&PropertyEntry> = available.into_iter().chain(other).collect();
        let labels: Vec<String> = ordered
            .iter()
            .map(|e| {
                let status = details_property_status(e);
                if status.is_empty() {
                    e.label.clone()
                } else {
                    arg2(tr("%1 (%2)"), &e.label, &status)
                }
            })
            .collect();
        let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let dropdown = gtk::DropDown::from_strings(&label_refs);
        dropdown.set_hexpand(true);
        let ids: Vec<String> = ordered.iter().map(|e| e.id.clone()).collect();
        let previous = self.state.borrow().detail_id.clone();
        if let Some(index) = ids.iter().position(|id| *id == previous) {
            dropdown.set_selected(index as u32);
        }
        let value = text_view();
        let source = plain_label("");
        let owned: Vec<PropertyEntry> = ordered.iter().map(|e| (*e).clone()).collect();
        let show = {
            let (value, source, owned) = (value.clone(), source.clone(), owned.clone());
            move |index: u32| match owned.get(index as usize) {
                Some(entry) => {
                    value.set_monospace(entry.id == "efi/hex");
                    value.set_wrap_mode(if entry.id == "efi/hex" { gtk::WrapMode::None } else { gtk::WrapMode::WordChar });
                    value.buffer().set_text(&entry.value);
                    source.set_text(&if entry.source.is_empty() { String::new() } else { arg(tr("Source: %1"), &entry.source) });
                }
                None => {
                    value.buffer().set_text("");
                    source.set_text("");
                }
            }
        };
        show(dropdown.selected());
        let advanced = gtk::CheckButton::with_mnemonic(&tr("Show _advanced properties"));
        advanced.set_active(self.advanced.get());

        let page = gtk::Box::new(gtk::Orientation::Vertical, 6);
        page.set_margin_start(8);
        page.set_margin_end(8);
        page.set_margin_top(8);
        let property_label = gtk::Label::with_mnemonic(&tr("_Property:"));
        property_label.set_xalign(0.0);
        property_label.set_mnemonic_widget(Some(&dropdown));
        page.append(&property_label);
        page.append(&dropdown);
        page.append(&gtk::ScrolledWindow::builder().child(&value).vexpand(true).build());
        page.append(&source);
        page.append(&advanced);
        let copy_value = copy_button(&tr("Copy value"), {
            let (value, dropdown) = (value.clone(), dropdown.clone());
            move || {
                let _ = &dropdown;
                let buffer = value.buffer();
                buffer.text(&buffer.start_iter(), &buffer.end_iter(), false).to_string()
            }
        });
        let copy_all = copy_button(&tr("Copy all values"), {
            let owned = owned.clone();
            move || {
                owned
                    .iter()
                    .map(|e| {
                        let mut text = format!("{}:\n{}", e.label, e.value);
                        if !e.source.is_empty() {
                            text += &format!("\n{}", arg(tr("Source: %1"), &e.source));
                        }
                        text
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n")
            }
        });
        page.append(&button_row(&[&copy_value, &copy_all]));
        self.wire_details(&advanced, &dropdown, ids, show);
        page
    }

    /// Shows the chosen property, remembers it across rebuilds, and rebuilds when "advanced" is
    /// toggled.
    fn wire_details(self: &Rc<Self>, advanced: &gtk::CheckButton, dropdown: &gtk::DropDown, ids: Vec<String>, show: impl Fn(u32) + 'static) {
        let weak = Rc::downgrade(self);
        advanced.connect_toggled({
            let weak = weak.clone();
            move |check| {
                if let Some(dialog) = weak.upgrade() {
                    dialog.advanced.set(check.is_active());
                    dialog.rebuild();
                }
            }
        });
        dropdown.connect_selected_notify(move |d| {
            show(d.selected());
            if let (Some(dialog), Some(id)) = (weak.upgrade(), ids.get(d.selected() as usize)) {
                dialog.state.borrow_mut().detail_id = id.clone();
            }
        });
    }

    fn resources_page(&self, snapshot: &DeviceProperties) -> gtk::Box {
        let (table, selection) = TextTable::new(&[tr("Resource type"), tr("Setting"), tr("Details")], true, &[]);
        let rows = resource_rows(&snapshot.resources);
        table.set_rows(rows.iter().map(|r| CellRow::new(vec![r.kind.clone(), r.setting.clone(), r.details.clone()], &r.source)).collect());
        let mut all: Vec<String> = Vec::new();
        if !snapshot.resources.pnp_state.is_empty() {
            let state = if snapshot.resources.pnp_state == "active" { tr("Active") } else { tr("Disabled") };
            all.push(arg(tr("PnP device state: %1"), &state));
        }
        all.extend(rows.iter().map(|r| format!("{}\t{}\t{}\n{}", r.kind, r.setting, r.details, arg(tr("Source: %1"), &r.source))));
        let all_text = all.join("\n\n");
        let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        page.append(&scrolled(&table.view));
        let copy_selection = gtk::Button::with_label(&tr("Copy selection"));
        copy_selection.set_sensitive(false);
        let copy_all = copy_button(&tr("Copy all resources"), move || all_text.clone());
        copy_selection.connect_clicked({
            let (selection, rows) = (selection.clone(), rows.clone());
            move |b| {
                let mut text = Vec::new();
                for (i, r) in rows.iter().enumerate() {
                    if selection.is_selected(i as u32) {
                        text.push(format!("{}\t{}\t{}\n{}", r.kind, r.setting, r.details, arg(tr("Source: %1"), &r.source)));
                    }
                }
                copy_to_clipboard(b, &text.join("\n\n"));
            }
        });
        selection.connect_selection_changed({
            let (copy_selection, selection) = (copy_selection.clone(), selection.clone());
            move |_, _, _| copy_selection.set_sensitive(selection.selection().size() > 0)
        });
        page.append(&button_row(&[&copy_selection, &copy_all]));
        page
    }

    fn storage_pages(self: &Rc<Self>, presentation: &StoragePresentation, add: &dyn Fn(String, &gtk::Box)) {
        let scope = tr("Read-only metadata snapshot. Each entity is a separate layer; capacities are not added. Kernel and UDisks2 block records are merged only by their exact device-number mapping. Source paths remain in tooltips and copied text. Deep search includes native metadata only.");
        // Overview.
        let overview = presentation.clone();
        let page = page_with_notice(
            &tr("ℹ️ Storage snapshot for this device"),
            &format!("{scope}\n\n{}", presentation.notes),
            &storage_fields_grid(&presentation.overview),
            &copy_button(&tr("Copy storage overview"), move || format!("{}\n\n{}", storage_fields_text(&overview.overview), overview.notes)),
        );
        add(tr("Storage"), &page);

        // Volumes: a table with the selected volume's fields below it.
        let (table, selection) = TextTable::new(&[tr("Device / label"), tr("Layer"), tr("Capacity"), tr("Content"), tr("Mount points")], false, &[]);
        table.set_rows(
            presentation
                .volumes
                .iter()
                .map(|v| CellRow::new(vec![v.name.clone(), v.kind.clone(), v.capacity.clone(), v.filesystem.clone(), v.mounts.clone()], &v.source))
                .collect(),
        );
        let details = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let volumes = presentation.volumes.clone();
        let show_volume = {
            let (details, volumes) = (details.clone(), volumes.clone());
            move |index: u32| {
                while let Some(child) = details.first_child() {
                    details.remove(&child);
                }
                match volumes.get(index as usize) {
                    Some(v) => details.append(&storage_fields_grid(&v.fields)),
                    None => details.append(&plain_label(&tr("No related volume metadata available."))),
                }
            }
        };
        let previous = self.state.borrow().volume_id.clone();
        let mut initial = volumes.iter().position(|v| v.id == previous).or_else(|| volumes.iter().position(|v| v.id == self.device.path)).unwrap_or(0);
        if volumes.is_empty() {
            initial = usize::MAX;
        }
        if initial != usize::MAX {
            selection.select_item(initial as u32, true);
        }
        show_volume(if initial == usize::MAX { u32::MAX } else { initial as u32 });
        let weak = Rc::downgrade(self);
        selection.connect_selection_changed({
            let (selection, volumes, show_volume) = (selection.clone(), volumes.clone(), show_volume.clone());
            move |_, _, _| {
                let single = selection.clone().downcast::<gtk::SingleSelection>().ok();
                let index = single.map_or(u32::MAX, |s| s.selected());
                show_volume(index);
                if let (Some(v), Some(dialog)) = (volumes.get(index as usize), weak.upgrade()) {
                    dialog.state.borrow_mut().volume_id = v.id.clone();
                }
            }
        });
        let volumes_page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        volumes_page.append(&notice(
            &tr("ℹ️ Related volumes and mounts"),
            &format!("{scope}\n{}\n\n{}", tr("Related volumes may span several drives. Mount points cover only the application mount namespace. No filesystem is probed, mounted or unlocked."), presentation.notes),
        ));
        volumes_page.append(&gtk::ScrolledWindow::builder().child(&table.view).min_content_height(160).vexpand(true).build());
        volumes_page.append(&gtk::ScrolledWindow::builder().child(&details).min_content_height(160).vexpand(true).build());
        let copy_selected = copy_button(&tr("Copy selected volume"), {
            let (selection, volumes) = (selection.clone(), volumes.clone());
            move || {
                let index = selection.clone().downcast::<gtk::SingleSelection>().map_or(u32::MAX, |s| s.selected());
                volumes.get(index as usize).map(|v| storage_rows_text(std::slice::from_ref(v))).unwrap_or_default()
            }
        });
        let copy_all = copy_button(&tr("Copy all volumes"), {
            let volumes = volumes.clone();
            move || storage_rows_text(&volumes)
        });
        volumes_page.append(&button_row(&[&copy_selected, &copy_all]));
        add(tr("Volumes"), &volumes_page);

        // Health: pick a drive, then show its groups and ATA table.
        let health = presentation.health.clone();
        let names: Vec<&str> = health.iter().map(|h| h.name.as_str()).collect();
        let drive = gtk::DropDown::from_strings(&names);
        drive.set_hexpand(true);
        let previous = self.state.borrow().health_id.clone();
        let initial = health
            .iter()
            .position(|h| h.id == previous)
            .or_else(|| health.iter().position(|h| !presentation.preferred_health_drive.is_empty() && h.id == presentation.preferred_health_drive))
            .unwrap_or(0);
        drive.set_selected(initial as u32);
        let groups = gtk::Box::new(gtk::Orientation::Vertical, 8);
        groups.set_margin_start(8);
        groups.set_margin_end(8);
        let show_health = {
            let (groups, health) = (groups.clone(), health.clone());
            move |index: u32| {
                while let Some(child) = groups.first_child() {
                    groups.remove(&child);
                }
                let Some(entry) = health.get(index as usize) else {
                    groups.append(&plain_label(&tr("Cached health unavailable.")));
                    return;
                };
                let mut sections: Vec<(String, Vec<qldm_core::storage_presentation::StorageDisplayField>)> = Vec::new();
                if !entry.fields.is_empty() {
                    sections.push((tr("Health"), entry.fields.clone()));
                }
                sections.extend(entry.health_groups.iter().map(|g| (g.title.clone(), g.fields.clone())));
                for (title, fields) in sections {
                    let frame = gtk::Frame::new(Some(&title));
                    frame.set_child(Some(&storage_fields_grid(&fields)));
                    groups.append(&frame);
                }
                if !entry.ata_attributes.is_empty() {
                    let frame = gtk::Frame::new(Some(&tr("ATA SMART attributes")));
                    let headers = [tr("ID"), tr("Name"), tr("Current"), tr("Worst"), tr("Threshold"), tr("Interpreted value"), tr("Flags")];
                    let (table, _) = TextTable::new(&headers, true, &[0, 2, 3, 4, 6]);
                    table.set_rows(
                        entry
                            .ata_attributes
                            .iter()
                            .map(|a| {
                                CellRow::new(
                                    vec![
                                        a.id.clone(),
                                        a.name.clone(),
                                        a.current.clone(),
                                        a.worst.clone(),
                                        a.threshold.clone(),
                                        a.interpreted.clone(),
                                        a.flags.clone(),
                                    ],
                                    &a.source,
                                )
                            })
                            .collect(),
                    );
                    let height = (entry.ata_attributes.len() as i32 * 34).clamp(120, 420);
                    frame.set_child(Some(&gtk::ScrolledWindow::builder().child(&table.view).min_content_height(height).build()));
                    groups.append(&frame);
                }
            }
        };
        show_health(initial as u32);
        let weak = Rc::downgrade(self);
        drive.connect_selected_notify({
            let (health, show_health) = (health.clone(), show_health.clone());
            move |d| {
                show_health(d.selected());
                if let (Some(h), Some(dialog)) = (health.get(d.selected() as usize), weak.upgrade()) {
                    dialog.state.borrow_mut().health_id = h.id.clone();
                }
            }
        });
        let health_page = gtk::Box::new(gtk::Orientation::Vertical, 4);
        health_page.append(&notice(
            &tr("ℹ️ Cached drive health"),
            &format!(
                "{}\n\n{}",
                tr("Cached UDisks2 health only, separate from device operational status. Reload does not request SMART updates or self-tests. Missing evidence is unavailable; no reported warning does not guarantee health. Related drives are shown separately."),
                presentation.notes
            ),
        ));
        let drive_label = gtk::Label::with_mnemonic(&tr("_Drive:"));
        drive_label.set_xalign(0.0);
        drive_label.set_margin_start(8);
        drive_label.set_mnemonic_widget(Some(&drive));
        health_page.append(&drive_label);
        drive.set_margin_start(8);
        drive.set_margin_end(8);
        health_page.append(&drive);
        health_page.append(&scrolled(&groups));
        let copy_selected = copy_button(&tr("Copy selected drive health"), {
            let (drive, health) = (drive.clone(), health.clone());
            move || health.get(drive.selected() as usize).map(|h| storage_rows_text(std::slice::from_ref(h))).unwrap_or_default()
        });
        let copy_all = copy_button(&tr("Copy all drive health"), move || storage_rows_text(&health));
        health_page.append(&button_row(&[&copy_selected, &copy_all]));
        add(tr("Health"), &health_page);
    }

    // ---- events ----

    /// Refreshes the Events tab from the monitor's history (called on every inventory snapshot).
    pub fn update_events(&self, history: &DeviceEventsSnapshot, monitor_note: &str) {
        let (removed, displayed) = {
            let state = self.state.borrow();
            (state.removed, state.displayed_events.clone())
        };
        let update = update_events(history, monitor_note, &self.device, removed, &displayed);
        self.state.borrow_mut().event_scope = update.scope.clone();
        self.events_notice.set_tooltip_text(Some(&update.scope));
        if events_unchanged(&update.events, &displayed) {
            return; // Unrelated events must not reset selection or scroll.
        }
        let selected = self.selected_event_rows().first().and_then(|i| displayed.get(*i)).cloned();
        let restore =
            selected.as_ref().and_then(|s| update.events.iter().position(|e| e.sequence == s.sequence && e.elapsed_ms == s.elapsed_ms && e.action == s.action));
        let rows: Vec<CellRow> = update.events.iter().map(|e| CellRow::new(event_row(e).to_vec(), "")).collect();
        self.events_table.set_rows(rows);
        self.state.borrow_mut().displayed_events = update.events;
        let count = self.state.borrow().displayed_events.len();
        if let Some(row) = restore.or((count > 0).then_some(0)) {
            self.events_selection.select_item(row as u32, true);
        }
        self.show_event();
    }

    fn selected_event_rows(&self) -> Vec<usize> {
        let count = self.state.borrow().displayed_events.len();
        (0..count).filter(|i| self.events_selection.is_selected(*i as u32)).collect()
    }

    fn show_event(&self) {
        let rows = self.selected_event_rows();
        let text = rows.first().and_then(|i| self.state.borrow().displayed_events.get(*i).map(event_text)).unwrap_or_default();
        self.events_details.buffer().set_text(&text);
    }

    fn events_copy(&self, selected_only: bool) -> String {
        let state = self.state.borrow();
        let picked: Vec<&DeviceEvent> = if selected_only {
            self.selected_event_rows().iter().filter_map(|i| state.displayed_events.get(*i)).collect()
        } else {
            state.displayed_events.iter().collect()
        };
        events_copy_text(&state.event_scope, &picked)
    }
}

/// Glue for callers that hold the dialog only weakly.
pub fn close_on_destroy(dialog: &Rc<PropertiesDialog>, callback: impl Fn() + 'static) {
    dialog.window().connect_close_request(move |_| {
        callback();
        glib::Propagation::Proceed
    });
}
