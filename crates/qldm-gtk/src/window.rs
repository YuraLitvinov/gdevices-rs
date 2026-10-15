//! The main window: menus, toolbar, filter bar, device tree and status line, ported from
//! `src/ui/mainwindow.cpp`. The tree content comes from [`qldm_core::tree`]; this module owns
//! only presentation and the glue to the background workers.
//!
//! Rules carried over from the C++ window:
//! * selection and expansion are restored by node key, device path and generation, so a replaced
//!   device is never silently replaced by a neighbour;
//! * refreshes that change nothing leave the tree untouched;
//! * a deep search is cancelled and restarted whenever the inventory or filter changes;
//! * closing waits (without blocking the UI) for the enumeration worker to stop.
use crate::properties::PropertiesDialog;
use crate::row::Row;
use crate::settings::{self, WindowSettings};
use crate::system_dialog::SystemDialog;
use gtk::prelude::*;
use gtk::{gio, glib};
use qldm_core::events::DeviceEventsSnapshot;
use qldm_core::i18n::{arg, arg2, tr};
use qldm_core::model::Inventory;
use qldm_core::monitor::Session;
use qldm_core::properties::collect_device_properties;
use qldm_core::search::{DeepSearchWorker, SearchBatch};
use qldm_core::system::collect_system_properties;
use qldm_core::tree::{visible_device_count, Filter, TreeModel, TreeNode, View};
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// What was selected in the unfiltered tree, by identity rather than position.
#[derive(Clone, Default)]
struct Selected {
    key: String,
    path: String,
    generation: u64,
}

struct State {
    model: TreeModel,
    filter: Filter,
    /// The (filtered) tree currently shown.
    shown: Option<TreeNode>,
    /// `<view id>:<node key>` of nodes the user expanded.
    expanded: HashSet<String>,
    selected: Selected,
    scan_note: String,
    busy: bool,
    requested: u64,
    initializing: bool,
    closing: bool,
    events: DeviceEventsSnapshot,
    search_revision: u64,
    search_request: u64,
    search_pending: bool,
    search_busy: bool,
    search_done: usize,
    search_total: usize,
    search_unavailable: usize,
    search_cancel: Arc<AtomicBool>,
    search_worker: Option<DeepSearchWorker>,
    filter_timer: Option<glib::SourceId>,
}

/// Bookkeeping of the single open Properties window and its serialized property reads.
#[derive(Default)]
struct PropsState {
    dialog: Option<Rc<PropertiesDialog>>,
    request: u64,
    busy: bool,
    pending: Option<qldm_core::model::Device>,
    cancel: Option<Arc<AtomicBool>>,
}

/// The one System Information window, retained (even when closed) so results never target a
/// destroyed window, plus its single in-flight read.
#[derive(Default)]
struct SystemState {
    dialog: Option<Rc<SystemDialog>>,
    busy: bool,
    cancel: Option<Arc<AtomicBool>>,
}

enum SearchEvent {
    Batch(SearchBatch),
    Finished(DeepSearchWorker),
}

pub struct Ui {
    window: gtk::ApplicationWindow,
    list: gtk::ListView,
    store: gio::ListStore,
    tree_model: gtk::TreeListModel,
    selection: gtk::SingleSelection,
    filter_bar: gtk::Box,
    filter_entry: gtk::SearchEntry,
    deep_check: gtk::CheckButton,
    status: gtk::Label,
    progress: gtk::ProgressBar,
    properties_action: gio::SimpleAction,
    filter_action: gio::SimpleAction,
    internal_action: gio::SimpleAction,
    view_action: gio::SimpleAction,
    session: RefCell<Option<Session>>,
    state: RefCell<State>,
    props: RefCell<PropsState>,
    system: RefCell<SystemState>,
    /// Set while the window itself changes selection or expansion, so handlers stay quiet.
    quiet: Cell<bool>,
}

thread_local! {
    /// The live window state. Callbacks only hold weak references, so something must own it for
    /// as long as the window exists; it is released when the window is destroyed.
    static LIVE: RefCell<Vec<Rc<Ui>>> = const { RefCell::new(Vec::new()) };
}

type Handler = Box<dyn Fn(&Rc<Ui>)>;

fn action(name: &str) -> String {
    format!("win.{name}")
}

fn menu_item(label: &str, name: &str) -> gio::MenuItem {
    gio::MenuItem::new(Some(label), Some(&action(name)))
}

fn build_menu() -> gio::Menu {
    let bar = gio::Menu::new();
    let file = gio::Menu::new();
    file.append_item(&menu_item(&tr("System _Information"), "system-info"));
    let quit = gio::Menu::new();
    quit.append_item(&menu_item(&tr("_Quit"), "quit"));
    file.append_section(None, &quit);
    bar.append_submenu(Some(&tr("_File")), &file);

    let act = gio::Menu::new();
    act.append_item(&menu_item(&tr("_Refresh"), "refresh"));
    act.append_item(&menu_item(&tr("_Properties"), "properties"));
    bar.append_submenu(Some(&tr("_Action")), &act);

    let view = gio::Menu::new();
    let views = gio::Menu::new();
    for v in View::ALL {
        let item = gio::MenuItem::new(Some(&v.label()), None);
        item.set_action_and_target_value(Some(&action("view")), Some(&v.id().to_variant()));
        views.append_item(&item);
    }
    view.append_section(None, &views);
    let toggles = gio::Menu::new();
    toggles.append_item(&menu_item(&tr("_Filter"), "filter"));
    toggles.append_item(&menu_item(&tr("Show _all devices"), "show-all"));
    view.append_section(None, &toggles);
    let expand = gio::Menu::new();
    expand.append_item(&menu_item(&tr("Expand all"), "expand-all"));
    expand.append_item(&menu_item(&tr("Collapse all"), "collapse-all"));
    view.append_section(None, &expand);
    bar.append_submenu(Some(&tr("_View")), &view);

    let help = gio::Menu::new();
    help.append_item(&menu_item(&tr("_About"), "about"));
    bar.append_submenu(Some(&tr("_Help")), &help);
    bar
}

fn trimmed_text(entry: &gtk::SearchEntry) -> String {
    entry.text().trim().to_string()
}

impl Ui {
    pub fn build(app: &gtk::Application) -> Rc<Ui> {
        let saved = settings::load();
        let store = gio::ListStore::new::<Row>();
        let tree_model = gtk::TreeListModel::new(store.clone(), false, false, |item| {
            let row = item.downcast_ref::<Row>()?;
            let children = row.children();
            if children.is_empty() {
                return None;
            }
            let children_store = gio::ListStore::new::<Row>();
            children_store.extend_from_slice(&children);
            Some(children_store.upcast())
        });
        let selection = gtk::SingleSelection::new(Some(tree_model.clone()));
        selection.set_autoselect(false);
        selection.set_can_unselect(true);

        let factory = gtk::SignalListItemFactory::new();
        let list = gtk::ListView::new(Some(selection.clone()), Some(factory.clone()));
        list.set_vexpand(true);
        list.set_single_click_activate(false);

        let filter_entry = gtk::SearchEntry::builder().hexpand(true).placeholder_text(tr("Device name")).build();
        let deep_check = gtk::CheckButton::with_label(&tr("Deep search"));
        deep_check.set_tooltip_text(Some(&tr(
            "Search device identifiers, generic properties, resources and native storage metadata. Specialized hardware-tab information is not fully indexed. Refresh updates cached metadata.",
        )));
        let filter_label = gtk::Label::with_mnemonic(&tr("_Filter:"));
        filter_label.set_mnemonic_widget(Some(&filter_entry));
        let filter_bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        filter_bar.set_margin_top(6);
        filter_bar.set_margin_bottom(6);
        filter_bar.set_margin_start(6);
        filter_bar.set_margin_end(6);
        filter_bar.append(&filter_label);
        filter_bar.append(&filter_entry);
        filter_bar.append(&deep_check);
        filter_bar.set_visible(false);

        let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        toolbar.set_margin_start(4);
        toolbar.set_margin_end(4);
        for (icon, name, tip) in [
            ("dialog-information-symbolic", "system-info", tr("Show System Information")),
            ("view-refresh-symbolic", "refresh", tr("Re-enumerate existing kernel devices; does not rescan buses.")),
            ("document-properties-symbolic", "properties", tr("Properties")),
        ] {
            let button = gtk::Button::from_icon_name(icon);
            button.set_action_name(Some(&action(name)));
            button.set_tooltip_text(Some(&tip));
            button.add_css_class("flat");
            toolbar.append(&button);
        }

        let status = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .margin_start(6)
            .margin_end(6)
            .margin_top(3)
            .margin_bottom(3)
            .build();
        let progress = gtk::ProgressBar::new();
        progress.set_valign(gtk::Align::Center);
        progress.set_size_request(120, -1);
        progress.set_margin_end(6);
        let status_bar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        status_bar.append(&status);
        status_bar.append(&progress);

        let menu_bar = gtk::PopoverMenuBar::from_model(Some(&build_menu()));
        let layout = gtk::Box::new(gtk::Orientation::Vertical, 0);
        layout.append(&menu_bar);
        layout.append(&toolbar);
        layout.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        layout.append(&filter_bar);
        layout.append(&gtk::ScrolledWindow::builder().child(&list).vexpand(true).build());
        layout.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        layout.append(&status_bar);

        let mut title = tr("QLinuxDeviceManager");
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            title.push_str(&tr(" — Administrative mode"));
        }
        let window =
            gtk::ApplicationWindow::builder().application(app).title(title).default_width(saved.width).default_height(saved.height).child(&layout).build();
        if saved.maximized {
            window.maximize();
        }

        let properties_action = gio::SimpleAction::new("properties", None);
        properties_action.set_enabled(false);
        let filter_action = gio::SimpleAction::new_stateful("filter", None, &false.to_variant());
        let internal_action = gio::SimpleAction::new_stateful("show-all", None, &false.to_variant());
        let view_action = gio::SimpleAction::new_stateful("view", Some(glib::VariantTy::STRING), &View::Type.id().to_variant());

        let state = State {
            model: TreeModel::default(),
            filter: Filter::default(),
            shown: None,
            expanded: View::ALL.iter().map(|v| format!("{}:computer", v.id())).collect(),
            selected: Selected::default(),
            scan_note: String::new(),
            busy: false,
            requested: 0,
            initializing: true,
            closing: false,
            events: DeviceEventsSnapshot::default(),
            search_revision: 1,
            search_request: 0,
            search_pending: false,
            search_busy: false,
            search_done: 0,
            search_total: 0,
            search_unavailable: 0,
            search_cancel: Arc::new(AtomicBool::new(false)),
            search_worker: Some(DeepSearchWorker::new()),
            filter_timer: None,
        };
        let ui = Rc::new(Ui {
            window,
            list,
            store,
            tree_model,
            selection,
            filter_bar,
            filter_entry,
            deep_check,
            status,
            progress,
            properties_action,
            filter_action,
            internal_action,
            view_action,
            session: RefCell::new(None),
            state: RefCell::new(state),
            props: RefCell::new(PropsState::default()),
            system: RefCell::new(SystemState::default()),
            quiet: Cell::new(false),
        });
        LIVE.with(|live| live.borrow_mut().push(ui.clone()));
        let released = Rc::downgrade(&ui);
        ui.window.connect_destroy(move |_| {
            if let Some(ui) = released.upgrade() {
                LIVE.with(|live| live.borrow_mut().retain(|u| !Rc::ptr_eq(u, &ui)));
            }
        });
        ui.wire_factory(&factory);
        ui.wire_actions(app);
        ui.wire_signals();
        ui.start_session();
        ui.update_status();
        ui.window.present();
        ui
    }

    fn weak(self: &Rc<Self>) -> Weak<Ui> {
        Rc::downgrade(self)
    }

    // ---- rows ----

    fn wire_factory(self: &Rc<Self>, factory: &gtk::SignalListItemFactory) {
        let weak = self.weak();
        factory.connect_setup(move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().expect("ListItem");
            let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            content.append(&gtk::Image::new());
            let label = gtk::Label::new(None);
            label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            content.append(&label);
            let expander = gtk::TreeExpander::new();
            expander.set_child(Some(&content));
            // Right click: select the row under the pointer and offer Properties.
            let gesture = gtk::GestureClick::builder().button(3).build();
            let (weak_ui, weak_item) = (weak.clone(), item.downgrade());
            gesture.connect_pressed(move |gesture, _, x, y| {
                let (Some(ui), Some(item)) = (weak_ui.upgrade(), weak_item.upgrade()) else { return };
                let Some(widget) = gesture.widget() else { return };
                ui.context_menu(item.position(), &widget, x, y);
            });
            expander.add_controller(gesture);
            item.set_child(Some(&expander));
        });
        factory.connect_bind(|_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().expect("ListItem");
            let Some(list_row) = item.item().and_downcast::<gtk::TreeListRow>() else { return };
            let Some(row) = list_row.item().and_downcast::<Row>() else { return };
            let expander = item.child().and_downcast::<gtk::TreeExpander>().expect("TreeExpander");
            expander.set_list_row(Some(&list_row));
            expander.set_tooltip_text(Some(&row.tooltip()));
            let content = expander.child().and_downcast::<gtk::Box>().expect("Box");
            if let Some(image) = content.first_child().and_downcast::<gtk::Image>() {
                let theme = gtk::IconTheme::for_display(&image.display());
                let icon = row.icon();
                image.set_icon_name(Some(if theme.has_icon(&icon) { &icon } else { "applications-system-symbolic" }));
            }
            if let Some(label) = content.last_child().and_downcast::<gtk::Label>() {
                label.set_text(&row.label());
                // Ancestors kept only for connection context read as secondary.
                if row.is_context() {
                    label.add_css_class("dim-label");
                } else {
                    label.remove_css_class("dim-label");
                }
            }
        });
    }

    fn context_menu(self: &Rc<Self>, position: u32, anchor: &gtk::Widget, x: f64, y: f64) {
        let Some(row) = self.row_at(position) else { return };
        if !Self::has_properties(&row) {
            return;
        }
        self.quiet.set(true);
        self.selection.set_selected(position);
        self.quiet.set(false);
        self.update_status();
        let menu = gio::Menu::new();
        menu.append_item(&menu_item(&tr("_Properties"), "properties"));
        let popover = gtk::PopoverMenu::from_model(Some(&menu));
        popover.set_parent(anchor);
        popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        popover.set_has_arrow(false);
        popover.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        popover.popup();
    }

    fn row_at(&self, position: u32) -> Option<Row> {
        self.tree_model.row(position)?.item().and_downcast::<Row>()
    }

    fn selected_row(&self) -> Option<Row> {
        let position = self.selection.selected();
        (position != gtk::INVALID_LIST_POSITION).then(|| self.row_at(position)).flatten()
    }

    fn has_properties(row: &Row) -> bool {
        row.key() == "computer" || row.target().is_some()
    }

    // ---- actions and signals ----

    fn wire_actions(self: &Rc<Self>, app: &gtk::Application) {
        let window = &self.window;
        let simple = |name: &str, handler: Handler| {
            let a = gio::SimpleAction::new(name, None);
            let weak = self.weak();
            a.connect_activate(move |_, _| {
                if let Some(ui) = weak.upgrade() {
                    handler(&ui);
                }
            });
            window.add_action(&a);
        };
        simple("refresh", Box::new(|ui| ui.refresh()));
        simple("system-info", Box::new(|ui| ui.open_system_information()));
        simple("quit", Box::new(|ui| ui.window.close()));
        simple("expand-all", Box::new(|ui| ui.set_all_expanded(true)));
        simple("collapse-all", Box::new(|ui| ui.set_all_expanded(false)));
        simple("about", Box::new(|ui| ui.show_about()));
        window.add_action(&self.properties_action);
        let weak = self.weak();
        self.properties_action.connect_activate(move |_, _| {
            if let Some(ui) = weak.upgrade() {
                ui.open_properties();
            }
        });

        window.add_action(&self.filter_action);
        let weak = self.weak();
        self.filter_action.connect_activate(move |a, _| {
            let on = a.state().and_then(|s| s.get::<bool>()).unwrap_or(false);
            a.change_state(&(!on).to_variant());
        });
        let weak2 = self.weak();
        self.filter_action.connect_change_state(move |a, value| {
            let Some(ui) = weak2.upgrade() else { return };
            let show = value.and_then(|v| v.get::<bool>()).unwrap_or(false);
            a.set_state(&show.to_variant());
            ui.filter_bar.set_visible(show);
            if show {
                ui.filter_entry.grab_focus();
                ui.filter_entry.select_region(0, -1);
            } else {
                ui.quiet.set(true);
                ui.filter_entry.set_text("");
                ui.quiet.set(false);
                ui.apply_filter();
                ui.list.grab_focus();
            }
        });
        let _ = weak;

        window.add_action(&self.internal_action);
        self.internal_action.connect_activate(|a, _| {
            let on = a.state().and_then(|s| s.get::<bool>()).unwrap_or(false);
            a.change_state(&(!on).to_variant());
        });
        let weak = self.weak();
        self.internal_action.connect_change_state(move |a, value| {
            let Some(ui) = weak.upgrade() else { return };
            let show = value.and_then(|v| v.get::<bool>()).unwrap_or(false);
            a.set_state(&show.to_variant());
            ui.remember_tree();
            ui.cancel_search();
            ui.state.borrow_mut().model.set_show_internal(show);
            ui.state.borrow_mut().search_revision += 1;
            ui.apply_filter();
        });

        window.add_action(&self.view_action);
        let weak = self.weak();
        self.view_action.connect_activate(move |a, parameter| {
            let Some(ui) = weak.upgrade() else { return };
            let Some(id) = parameter.and_then(|p| p.get::<String>()) else { return };
            let Some(view) = View::ALL.iter().copied().find(|v| v.id() == id) else { return };
            a.set_state(&id.to_variant());
            ui.remember_tree();
            ui.cancel_search();
            ui.state.borrow_mut().model.set_view(view);
            ui.state.borrow_mut().search_revision += 1;
            ui.apply_filter();
        });

        app.set_accels_for_action(&action("refresh"), &["F5"]);
        app.set_accels_for_action(&action("filter"), &["<Control>f"]);
        app.set_accels_for_action(&action("system-info"), &["<Alt>i"]);
        app.set_accels_for_action(&action("quit"), &["<Control>q"]);
    }

    fn wire_signals(self: &Rc<Self>) {
        let weak = self.weak();
        self.selection.connect_selection_changed(move |_, _, _| {
            if let Some(ui) = weak.upgrade() {
                if !ui.quiet.get() {
                    ui.update_status();
                }
            }
        });
        // Double click (or a keyboard activation not handled below): branches toggle, leaves open
        // properties.
        let weak = self.weak();
        self.list.connect_activate(move |_, position| {
            let Some(ui) = weak.upgrade() else { return };
            let Some(list_row) = ui.tree_model.row(position) else { return };
            if list_row.is_expandable() {
                list_row.set_expanded(!list_row.is_expanded());
            } else if ui.properties_action.is_enabled() {
                ui.open_properties();
            }
        });
        // Enter always opens properties, even on a branch.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = self.weak();
        keys.connect_key_pressed(move |_, key, _, state| {
            let Some(ui) = weak.upgrade() else { return glib::Propagation::Proceed };
            use gtk::gdk::ModifierType as M;
            let plain = !state.intersects(M::CONTROL_MASK | M::ALT_MASK | M::SHIFT_MASK | M::SUPER_MASK | M::META_MASK);
            if plain && matches!(key, gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter) && ui.list.has_focus() {
                if ui.properties_action.is_enabled() {
                    ui.open_properties();
                }
                return glib::Propagation::Stop;
            }
            if key == gtk::gdk::Key::Escape && ui.filter_bar.is_visible() {
                ui.filter_action.change_state(&false.to_variant());
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.window.add_controller(keys);

        let weak = self.weak();
        self.filter_entry.connect_search_changed(move |_| {
            let Some(ui) = weak.upgrade() else { return };
            if ui.quiet.get() {
                return;
            }
            ui.cancel_search();
            let previous = ui.state.borrow_mut().filter_timer.take();
            if let Some(source) = previous {
                source.remove();
            }
            let weak = ui.weak();
            let source = glib::timeout_add_local_once(Duration::from_millis(150), move || {
                if let Some(ui) = weak.upgrade() {
                    ui.state.borrow_mut().filter_timer = None;
                    ui.apply_filter();
                }
            });
            ui.state.borrow_mut().filter_timer = Some(source);
        });
        let weak = self.weak();
        self.filter_entry.connect_stop_search(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.filter_action.change_state(&false.to_variant());
            }
        });
        let weak = self.weak();
        self.deep_check.connect_toggled(move |check| {
            let Some(ui) = weak.upgrade() else { return };
            ui.filter_entry.set_placeholder_text(Some(&if check.is_active() { tr("All device metadata") } else { tr("Device name") }));
            ui.apply_filter();
        });
        let weak = self.weak();
        self.window.connect_close_request(move |_| match weak.upgrade() {
            Some(ui) => ui.close_requested(),
            None => glib::Propagation::Proceed,
        });
        let weak = self.weak();
        glib::timeout_add_local(Duration::from_millis(100), move || match weak.upgrade() {
            Some(ui) if ui.state.borrow().initializing && ui.progress.is_visible() => {
                ui.progress.pulse();
                glib::ControlFlow::Continue
            }
            _ => glib::ControlFlow::Break,
        });
    }

    // ---- enumeration session ----

    fn start_session(self: &Rc<Self>) {
        let (sender, receiver) = async_channel::unbounded::<Inventory>();
        let session = Session::start(move |inventory| {
            let _ = sender.send_blocking(inventory);
        });
        *self.session.borrow_mut() = Some(session);
        let weak = self.weak();
        glib::spawn_future_local(async move {
            while let Ok(inventory) = receiver.recv().await {
                let Some(ui) = weak.upgrade() else { break };
                ui.accept_inventory(inventory);
            }
        });
    }

    fn refresh(self: &Rc<Self>) {
        if self.state.borrow().closing {
            return;
        }
        let requested = self.session.borrow().as_ref().map(|s| s.request_refresh());
        if let Some(requested) = requested {
            let mut state = self.state.borrow_mut();
            state.busy = true;
            state.requested = requested;
        }
        self.update_status();
    }

    fn accept_inventory(self: &Rc<Self>, inventory: Inventory) {
        if let Some(session) = self.session.borrow().as_ref() {
            session.acknowledge();
        }
        let (manual, closing) = {
            let mut state = self.state.borrow_mut();
            if state.closing {
                return;
            }
            state.initializing = false;
            state.events = inventory.events.clone();
            let manual = state.busy && inventory.request >= state.requested;
            state.busy = inventory.request < state.requested;
            (manual, state.closing)
        };
        let _ = closing;
        self.progress.set_visible(false); // A failed initial scan is a completed attempt too.
                                          // Update before marking a removed dialog so its final removal observation stays in the
                                          // read-only snapshot. Future instances cannot enter that tab.
        let dialog = self.props.borrow().dialog.clone();
        if let Some(dialog) = &dialog {
            dialog.update_events(&inventory.events, &inventory.monitor_note);
        }
        if !inventory.error.is_empty() {
            if inventory.identity_lost || !inventory.removed_paths.is_empty() {
                self.state.borrow_mut().model.set_inventory(Vec::new(), &HashSet::new(), true);
                if let Some(dialog) = &dialog {
                    dialog.reconcile_instance(None);
                }
                self.invalidate_properties_requests();
            }
            self.apply_filter();
            self.status.set_text(&inventory.error);
            return;
        }
        self.remember_tree();
        let changed = self.state.borrow_mut().model.set_inventory(inventory.devices, &inventory.removed_paths, inventory.identity_lost);
        if changed || manual {
            self.cancel_search();
            self.state.borrow_mut().search_revision += 1;
        }
        if let Some(dialog) = &dialog {
            let current = self.state.borrow().model.device(&dialog.device().path, dialog.device().generation).cloned();
            dialog.reconcile_instance(current.as_ref());
            if dialog.is_removed() {
                self.invalidate_properties_requests();
            }
        }
        let mut note = inventory.monitor_note;
        if inventory.skipped > 0 {
            if !note.is_empty() {
                note.push_str(" — ");
            }
            note.push_str(&arg(tr("%1 records unavailable or removed during enumeration"), &inventory.skipped.to_string()));
        }
        self.state.borrow_mut().scan_note = note;
        if changed || manual {
            self.apply_filter();
        } else {
            let start = {
                let state = self.state.borrow();
                state.search_pending && !state.search_busy && !state.busy
            };
            if start {
                self.start_search();
            }
            self.update_status();
        }
    }

    // ---- tree state ----

    fn view_prefix(&self) -> String {
        format!("{}:", self.state.borrow().model.view().id())
    }

    /// Records expansion and selection of the unfiltered tree before it is rebuilt.
    fn remember_tree(&self) {
        if self.state.borrow().filter.active() {
            return; // Preserve the unfiltered expansion and selection.
        }
        let prefix = self.view_prefix();
        let mut expanded = Vec::new();
        for position in 0..self.tree_model.n_items() {
            let Some(list_row) = self.tree_model.row(position) else { continue };
            let Some(row) = list_row.item().and_downcast::<Row>() else { continue };
            if list_row.is_expandable() {
                expanded.push((format!("{prefix}{}", row.key()), list_row.is_expanded()));
            }
        }
        let selected = self.selected_row().map(|row| {
            let (path, generation) = row.target().unwrap_or_default();
            Selected { key: row.key(), path, generation }
        });
        let mut state = self.state.borrow_mut();
        for (id, is_expanded) in expanded {
            if is_expanded {
                state.expanded.insert(id);
            } else {
                state.expanded.remove(&id);
            }
        }
        state.selected = selected.unwrap_or_default();
    }

    /// Shows the (filtered) tree and restores expansion and selection by identity.
    fn rebuild_view(&self) {
        let (shown, expand_all, expanded, selected, prefix) = {
            let mut state = self.state.borrow_mut();
            let Some(root) = state.model.root().cloned() else { return };
            let shown = if state.filter.active() { state.filter.apply(&root) } else { root };
            state.shown = Some(shown.clone());
            let prefix = format!("{}:", state.model.view().id());
            (shown, state.filter.active(), state.expanded.clone(), state.selected.clone(), prefix)
        };
        self.quiet.set(true);
        let tooltip = |node: &TreeNode| self.state.borrow().model.tooltip(node);
        let row = Row::from_node(&shown, &tooltip);
        self.store.remove_all();
        self.store.append(&row);
        // Expanding a row inserts its children after it, so one forward pass reaches them all.
        let mut position = 0;
        while position < self.tree_model.n_items() {
            if let Some(list_row) = self.tree_model.row(position) {
                if let Some(row) = list_row.item().and_downcast::<Row>() {
                    if list_row.is_expandable() && (expand_all || expanded.contains(&format!("{prefix}{}", row.key()))) {
                        list_row.set_expanded(true);
                    }
                }
            }
            position += 1;
        }
        self.restore_selection(&shown, &selected);
        self.quiet.set(false);
        self.update_status();
    }

    fn restore_selection(&self, shown: &TreeNode, selected: &Selected) {
        if selected.key.is_empty() && selected.path.is_empty() {
            self.selection.set_selected(gtk::INVALID_LIST_POSITION);
            return;
        }
        // Prefer the exact node; fall back to the device's first node (a view change may have
        // removed the specific row), and never select a replacement or neighbour.
        let current = self.state.borrow().model.generation_of(&selected.path);
        let alive = selected.path.is_empty() || current == Some(selected.generation);
        let mut trail: Option<Vec<String>> = None;
        fn find(node: &TreeNode, exact: &dyn Fn(&TreeNode) -> bool, ancestors: &mut Vec<String>) -> Option<Vec<String>> {
            for child in &node.children {
                if exact(child) {
                    return Some(ancestors.clone());
                }
                ancestors.push(child.key.clone());
                let found = find(child, exact, ancestors);
                ancestors.pop();
                if found.is_some() {
                    return found;
                }
            }
            None
        }
        let target = selected.clone();
        if selected.key == "computer" {
            trail = Some(Vec::new());
        } else if alive {
            let exact = |n: &TreeNode| n.key == target.key && n.generation == target.generation && (n.path == target.path || n.owner_path == target.path);
            let device = |n: &TreeNode| !target.path.is_empty() && n.path == target.path && n.generation == target.generation;
            trail = find(shown, &exact, &mut Vec::new()).or_else(|| find(shown, &device, &mut Vec::new()));
        }
        let Some(ancestors) = trail else {
            self.selection.set_selected(gtk::INVALID_LIST_POSITION);
            return;
        };
        // Make the path to the selection visible, then locate it.
        let mut position = 0;
        while position < self.tree_model.n_items() {
            if let Some(list_row) = self.tree_model.row(position) {
                if let Some(row) = list_row.item().and_downcast::<Row>() {
                    if ancestors.contains(&row.key()) && list_row.is_expandable() {
                        list_row.set_expanded(true);
                    }
                }
            }
            position += 1;
        }
        for position in 0..self.tree_model.n_items() {
            let Some(row) = self.row_at(position) else { continue };
            let is_target = if selected.key == "computer" {
                row.key() == "computer"
            } else {
                row.key() == selected.key && row.generation() == selected.generation && row.target().is_some_and(|(p, _)| p == selected.path)
                    || (!row.path().is_empty() && row.path() == selected.path && row.generation() == selected.generation && !self.exact_exists(selected))
            };
            if is_target {
                if self.selection.selected() != position {
                    self.selection.set_selected(position);
                    self.list.scroll_to(position, gtk::ListScrollFlags::NONE, None);
                }
                return;
            }
        }
        self.selection.set_selected(gtk::INVALID_LIST_POSITION);
    }

    /// Whether the exact node (not just the device) of a remembered selection still exists.
    fn exact_exists(&self, selected: &Selected) -> bool {
        (0..self.tree_model.n_items())
            .filter_map(|p| self.row_at(p))
            .any(|r| r.key() == selected.key && r.generation() == selected.generation && r.target().is_some_and(|(p, _)| p == selected.path))
    }

    fn set_all_expanded(&self, expand: bool) {
        self.quiet.set(true);
        if expand {
            let mut position = 0;
            while position < self.tree_model.n_items() {
                if let Some(list_row) = self.tree_model.row(position) {
                    if list_row.is_expandable() {
                        list_row.set_expanded(true);
                    }
                }
                position += 1;
            }
        } else {
            for position in (0..self.tree_model.n_items()).rev() {
                if let Some(list_row) = self.tree_model.row(position) {
                    if list_row.is_expandable() {
                        list_row.set_expanded(false);
                    }
                }
            }
        }
        self.quiet.set(false);
        self.update_status();
    }

    fn update_status(&self) {
        let (closing, initializing, busy) = {
            let state = self.state.borrow();
            (state.closing, state.initializing, state.busy)
        };
        if closing {
            return;
        }
        let selected = self.selected_row();
        self.properties_action.set_enabled(selected.as_ref().is_some_and(Self::has_properties));
        if initializing || busy {
            self.status.set_text(&if initializing { tr("Initializing…") } else { tr("Refreshing devices…") });
            return;
        }
        let state = self.state.borrow();
        let shown = state.shown.as_ref().map_or(0, visible_device_count);
        let mut message = arg2(tr("%1 shown / %2 discovered"), &shown.to_string(), &state.model.inventory_count().to_string());
        if state.filter.active() && self.deep_check.is_active() {
            message += &arg2(tr(" — Deep search: %1/%2"), &state.search_done.to_string(), &state.search_total.to_string());
            if state.search_unavailable > 0 {
                message += &arg(tr("; %1 property reads unavailable"), &state.search_unavailable.to_string());
            }
            let node = selected.as_ref().and_then(|row| {
                let path = row.path();
                state.shown.as_ref().and_then(|root| find_node(root, &row.key(), &path, row.generation()))
            });
            if let Some(reason) = node.and_then(|n| state.filter.match_reason(n)) {
                message += &arg(tr(" — Matched: %1"), &reason);
            }
        }
        if !state.scan_note.is_empty() {
            message += &format!(" — {}", state.scan_note);
        }
        if let Some(row) = &selected {
            if !row.path().is_empty() {
                message += &format!(" — {}", row.path());
            }
        }
        self.status.set_text(&message);
    }

    // ---- filter and deep search ----

    fn cancel_search(&self) {
        let mut state = self.state.borrow_mut();
        state.search_request += 1; // Reject queued batches from every obsolete request.
        state.search_pending = false;
        state.search_cancel.store(true, Ordering::Relaxed);
    }

    fn apply_filter(self: &Rc<Self>) {
        let timer = self.state.borrow_mut().filter_timer.take();
        if let Some(source) = timer {
            source.remove();
        }
        self.cancel_search();
        if self.state.borrow().closing {
            return;
        }
        let query = trimmed_text(&self.filter_entry);
        let deep = self.deep_check.is_active();
        if !self.state.borrow().filter.active() && !query.is_empty() {
            self.remember_tree();
        }
        {
            let mut state = self.state.borrow_mut();
            state.filter.set_query(&query, deep);
            state.search_done = 0;
            state.search_total = 0;
            state.search_unavailable = 0;
        }
        self.rebuild_view();
        if !query.is_empty() && deep {
            let start = {
                let mut state = self.state.borrow_mut();
                state.search_total = state.model.visible_count();
                state.search_pending = true;
                !state.search_busy && !state.busy
            };
            if start {
                self.start_search();
            }
        }
        self.update_status();
    }

    fn start_search(self: &Rc<Self>) {
        let (records, query, revision, request, cancel, mut worker) = {
            let mut state = self.state.borrow_mut();
            if state.closing || state.busy || !state.search_pending || state.filter_timer.is_some() {
                return;
            }
            let Some(worker) = state.search_worker.take() else { return };
            state.search_pending = false;
            state.search_busy = true;
            state.search_cancel = Arc::new(AtomicBool::new(false));
            (state.model.search_records(), trimmed_text(&self.filter_entry), state.search_revision, state.search_request, state.search_cancel.clone(), worker)
        };
        let (sender, receiver) = async_channel::unbounded::<SearchEvent>();
        std::thread::spawn(move || {
            let batches = sender.clone();
            worker.run(&records, &query, revision, request, &cancel, move |batch| {
                let _ = batches.send_blocking(SearchEvent::Batch(batch));
            });
            let _ = sender.send_blocking(SearchEvent::Finished(worker));
        });
        let weak = self.weak();
        glib::spawn_future_local(async move {
            while let Ok(event) = receiver.recv().await {
                let Some(ui) = weak.upgrade() else { break };
                match event {
                    SearchEvent::Batch(batch) => ui.accept_search_batch(batch),
                    SearchEvent::Finished(worker) => {
                        ui.search_finished(worker);
                        break;
                    }
                }
            }
        });
    }

    fn accept_search_batch(&self, batch: SearchBatch) {
        {
            let mut state = self.state.borrow_mut();
            if state.closing || batch.request != state.search_request || !state.filter.active() || !self.deep_check.is_active() {
                return;
            }
            state.filter.add_matches(&batch.matches);
            state.search_done = batch.done;
            state.search_total = batch.total;
            state.search_unavailable = batch.unavailable;
        }
        self.rebuild_view();
    }

    fn search_finished(self: &Rc<Self>, worker: DeepSearchWorker) {
        let (closing, pending) = {
            let mut state = self.state.borrow_mut();
            state.search_worker = Some(worker);
            state.search_busy = false;
            (state.closing, state.search_pending)
        };
        if closing {
            self.window.close();
        } else if pending {
            self.start_search();
        }
    }

    // ---- dialogs (placeholders until the properties/system dialog works) ----

    fn open_properties(self: &Rc<Self>) {
        if self.state.borrow().closing {
            return;
        }
        let Some(row) = self.selected_row() else { return };
        if row.key() == "computer" {
            self.open_system_information();
            return;
        }
        let Some((path, generation)) = row.target() else { return };
        let Some(device) = self.state.borrow().model.device(&path, generation).cloned() else { return };
        let existing = self.props.borrow().dialog.clone();
        if let Some(existing) = existing {
            if existing.device().path == device.path && existing.device().generation == device.generation {
                existing.present();
                return;
            }
            existing.window().close();
        }
        let weak = self.weak();
        let dialog = PropertiesDialog::new(
            &self.window,
            device,
            Box::new(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.request_properties();
                }
            }),
        );
        {
            let state = self.state.borrow();
            dialog.update_events(&state.events, &state.scan_note);
        }
        let (weak, closed) = (self.weak(), Rc::downgrade(&dialog));
        dialog.window().connect_close_request(move |_| {
            if let (Some(ui), Some(closed)) = (weak.upgrade(), closed.upgrade()) {
                let mut props = ui.props.borrow_mut();
                if props.dialog.as_ref().is_some_and(|d| Rc::ptr_eq(d, &closed)) {
                    props.dialog = None;
                    props.pending = None;
                    props.request += 1;
                    if let Some(cancel) = &props.cancel {
                        cancel.store(true, Ordering::Relaxed);
                    }
                }
            }
            glib::Propagation::Proceed
        });
        self.props.borrow_mut().dialog = Some(dialog.clone());
        dialog.present(); // Modeless: tree Refresh stays available while properties are open.
        self.request_properties();
    }

    /// Reads the open dialog's device. Reads are serialized: a request made while one runs
    /// cancels it and only the latest pending request survives.
    fn request_properties(self: &Rc<Self>) {
        let dialog = {
            let state = self.state.borrow();
            let props = self.props.borrow();
            match &props.dialog {
                Some(d) if !state.closing && !d.is_removed() => d.clone(),
                _ => return,
            }
        };
        let busy = {
            let mut props = self.props.borrow_mut();
            props.pending = Some(dialog.device().clone());
            props.request += 1;
            if props.busy {
                if let Some(cancel) = &props.cancel {
                    cancel.store(true, Ordering::Relaxed);
                }
            }
            props.busy
        };
        dialog.set_busy(true);
        if !busy {
            self.start_properties_read();
        }
    }

    fn start_properties_read(self: &Rc<Self>) {
        let (device, request, cancel) = {
            let mut props = self.props.borrow_mut();
            let Some(device) = props.pending.take() else { return };
            props.busy = true;
            let cancel = Arc::new(AtomicBool::new(false));
            props.cancel = Some(cancel.clone());
            (device, props.request, cancel)
        };
        let (sender, receiver) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let mut result = collect_device_properties(&device, true, Some(&cancel));
            result.request = request;
            let _ = sender.send_blocking(result);
        });
        let weak = self.weak();
        glib::spawn_future_local(async move {
            let Ok(result) = receiver.recv().await else { return };
            let Some(ui) = weak.upgrade() else { return };
            ui.accept_properties(result);
        });
    }

    fn accept_properties(self: &Rc<Self>, result: qldm_core::properties::DeviceProperties) {
        let (dialog, pending, current, closing) = {
            let mut props = self.props.borrow_mut();
            props.busy = false;
            (props.dialog.clone(), props.pending.is_some(), props.request, self.state.borrow().closing)
        };
        if closing {
            self.window.close();
            return;
        }
        let Some(dialog) = dialog else {
            self.props.borrow_mut().pending = None;
            return;
        };
        if pending && !dialog.is_removed() {
            self.start_properties_read();
            return;
        }
        self.props.borrow_mut().pending = None;
        if result.request == current {
            dialog.accept_result(result);
        }
    }

    /// Drops queued and in-flight property reads (the device they were for is gone).
    fn invalidate_properties_requests(&self) {
        let mut props = self.props.borrow_mut();
        props.pending = None;
        props.request += 1;
        if let Some(cancel) = &props.cancel {
            cancel.store(true, Ordering::Relaxed);
        }
    }

    fn open_system_information(self: &Rc<Self>) {
        if self.state.borrow().closing {
            return;
        }
        let existing = self.system.borrow().dialog.clone();
        let dialog = match existing {
            Some(dialog) => dialog,
            None => {
                let weak = self.weak();
                let dialog = SystemDialog::new(
                    &self.window,
                    Box::new(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.request_system_information();
                        }
                    }),
                );
                // Closing hides it; the dialog is reused with its last snapshot.
                dialog.window().set_hide_on_close(true);
                self.system.borrow_mut().dialog = Some(dialog.clone());
                self.request_system_information();
                dialog
            }
        };
        dialog.present();
    }

    fn request_system_information(self: &Rc<Self>) {
        let dialog = {
            let state = self.state.borrow();
            let mut system = self.system.borrow_mut();
            let Some(dialog) = system.dialog.clone() else { return };
            if state.closing || system.busy {
                return;
            }
            system.busy = true; // One request at a time, including its queued delivery.
            system.cancel = Some(Arc::new(AtomicBool::new(false)));
            dialog
        };
        dialog.set_busy(true);
        let cancel = self.system.borrow().cancel.clone().expect("just set");
        let (sender, receiver) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = sender.send_blocking(collect_system_properties(Some(&cancel)));
        });
        let weak = self.weak();
        glib::spawn_future_local(async move {
            let Ok(result) = receiver.recv().await else { return };
            let Some(ui) = weak.upgrade() else { return };
            ui.system.borrow_mut().busy = false;
            if ui.state.borrow().closing {
                ui.window.close();
                return;
            }
            let dialog = ui.system.borrow().dialog.clone();
            if let Some(dialog) = dialog {
                dialog.accept_result(result);
            }
        });
    }

    pub fn system_dialog(&self) -> Option<Rc<SystemDialog>> {
        self.system.borrow().dialog.clone()
    }

    fn show_about(&self) {
        let about = gtk::AboutDialog::builder()
            .transient_for(&self.window)
            .modal(true)
            .program_name(tr("QLinuxDeviceManager"))
            .version(qldm_core::app::VERSION)
            .website(qldm_core::app::REPOSITORY)
            .comments(tr("A read-only Linux hardware viewer.\nCategories are application groupings, not native Linux device classes.\nDevice presence does not establish hardware health."))
            .license_type(gtk::License::Gpl30)
            .build();
        about.present();
    }

    // ---- hooks for the UI smoke test ----

    /// Text of every row currently listed (expanded rows only), with its depth.
    pub fn listed_rows(&self) -> Vec<(u32, String)> {
        (0..self.tree_model.n_items())
            .filter_map(|p| self.tree_model.row(p))
            .filter_map(|r| r.item().and_downcast::<Row>().map(|row| (r.depth(), row.label())))
            .collect()
    }

    pub fn status_text(&self) -> String {
        self.status.text().to_string()
    }

    pub fn is_initializing(&self) -> bool {
        self.state.borrow().initializing
    }

    pub fn is_busy(&self) -> bool {
        let state = self.state.borrow();
        state.busy || state.initializing
    }

    /// Types into the filter bar as a user would (the 150 ms debounce still applies).
    pub fn set_filter(&self, text: &str, deep: bool) {
        self.filter_action.change_state(&true.to_variant());
        self.deep_check.set_active(deep);
        self.filter_entry.set_text(text);
    }

    pub fn clear_filter(&self) {
        self.filter_action.change_state(&false.to_variant());
    }

    pub fn expand_all(&self) {
        self.set_all_expanded(true);
    }

    pub fn collapse_all(&self) {
        self.set_all_expanded(false);
    }

    pub fn activate_view(&self, id: &str) {
        self.view_action.activate(Some(&id.to_variant()));
    }

    pub fn set_show_all(&self, show: bool) {
        self.internal_action.change_state(&show.to_variant());
    }

    /// Selects the first listed row whose label contains `text`; returns whether one was found.
    pub fn select_label(&self, text: &str) -> bool {
        for position in 0..self.tree_model.n_items() {
            if self.row_at(position).is_some_and(|r| r.label().contains(text)) {
                self.selection.set_selected(position);
                self.update_status();
                return true;
            }
        }
        false
    }

    pub fn select_root(&self) {
        self.selection.set_selected(0);
        self.update_status();
    }

    pub fn properties_enabled(&self) -> bool {
        self.properties_action.is_enabled()
    }

    /// Opens Properties for the selected row, as Enter or the Properties action would.
    pub fn open_selected_properties(self: &Rc<Self>) {
        self.open_properties();
    }

    pub fn properties_dialog(&self) -> Option<Rc<PropertiesDialog>> {
        self.props.borrow().dialog.clone()
    }

    pub fn request_refresh(self: &Rc<Self>) {
        self.refresh();
    }

    pub fn window(&self) -> &gtk::ApplicationWindow {
        &self.window
    }

    // ---- shutdown ----

    fn close_requested(self: &Rc<Self>) -> glib::Propagation {
        {
            let mut state = self.state.borrow_mut();
            if state.closing && self.session.borrow().is_none() {
                return glib::Propagation::Proceed;
            }
            if state.closing {
                return glib::Propagation::Stop; // Already waiting for the worker.
            }
            state.closing = true;
            state.search_cancel.store(true, Ordering::Relaxed);
            state.search_pending = false;
        }
        if let Some(cancel) = &self.system.borrow().cancel {
            cancel.store(true, Ordering::Relaxed);
        }
        let open_dialog = self.props.borrow().dialog.clone();
        if let Some(dialog) = open_dialog {
            dialog.window().close();
        }
        settings::save(&WindowSettings { width: self.window.default_width(), height: self.window.default_height(), maximized: self.window.is_maximized() });
        self.progress.set_visible(false);
        let session = self.session.borrow_mut().take();
        let Some(mut session) = session else { return glib::Propagation::Proceed };
        // A kernel metadata read already in progress cannot be cancelled, so the worker is
        // joined off the UI thread and the window closes when it is done.
        session.request_stop();
        self.window.set_sensitive(false);
        self.status.set_text(&tr("Waiting for device metadata reads to finish…"));
        let (sender, receiver) = async_channel::bounded::<()>(1);
        std::thread::spawn(move || {
            session.join();
            let _ = sender.send_blocking(());
        });
        let weak = self.weak();
        glib::spawn_future_local(async move {
            let _ = receiver.recv().await;
            if let Some(ui) = weak.upgrade() {
                ui.window.destroy();
            }
        });
        glib::Propagation::Stop
    }
}

fn find_node<'a>(node: &'a TreeNode, key: &str, path: &str, generation: u64) -> Option<&'a TreeNode> {
    node.children.iter().find_map(|c| if c.key == key && c.path == path && c.generation == generation { Some(c) } else { find_node(c, key, path, generation) })
}
