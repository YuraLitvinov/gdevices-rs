//! The System Information window, ported from `src/ui/systemdialog.cpp`. What each field says
//! comes from `qldm_core::system_report`; this module lays it out in four tabs (Operating System,
//! Hardware with the MOK certificates, Processor, RAM/Memory with the firmware records), with
//! source tooltips, Copy all and Refresh.
use crate::properties::widgets::{add_field, copy_to_clipboard, field_grid, plain_label, scrolled};
use gtk::prelude::*;
use qldm_core::i18n::tr;
use qldm_core::system::SystemProperties;
use qldm_core::system_report::{copy_all, display_value, field_source, memory_blocks, mok_blocks, mok_scope, sections, MemoryBlock, MokBlock};
use std::cell::RefCell;
use std::rc::Rc;

pub struct SystemDialog {
    window: gtk::Window,
    notebook: gtk::Notebook,
    refresh: gtk::Button,
    copy: gtk::Button,
    snapshot: RefCell<Option<SystemProperties>>,
    /// Value label text by field key, for the test hooks.
    texts: RefCell<Vec<(String, String)>>,
}

fn heading(text: &str) -> gtk::Label {
    let label = plain_label(text);
    label.add_css_class("heading");
    label.set_margin_top(6);
    label
}

impl SystemDialog {
    pub fn new(parent: &impl IsA<gtk::Window>, on_refresh: Box<dyn Fn()>) -> Rc<Self> {
        let window = gtk::Window::builder().transient_for(parent).title(tr("System Information")).default_width(680).default_height(650).build();
        let notebook = gtk::Notebook::new();
        notebook.set_scrollable(true);
        notebook.set_vexpand(true);
        let copy = gtk::Button::with_label(&tr("Copy all"));
        copy.set_sensitive(false);
        let refresh = gtk::Button::with_label(&tr("Refresh"));
        let close = gtk::Button::with_label(&tr("Close"));
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        buttons.set_halign(gtk::Align::End);
        buttons.set_margin_top(6);
        buttons.set_margin_bottom(6);
        buttons.set_margin_end(8);
        for b in [&copy, &refresh, &close] {
            buttons.append(b);
        }
        let layout = gtk::Box::new(gtk::Orientation::Vertical, 0);
        layout.append(&notebook);
        layout.append(&buttons);
        window.set_child(Some(&layout));
        let this = Rc::new(Self {
            window: window.clone(),
            notebook,
            refresh: refresh.clone(),
            copy: copy.clone(),
            snapshot: RefCell::new(None),
            texts: RefCell::new(Vec::new()),
        });
        refresh.connect_clicked(move |_| on_refresh());
        close.connect_clicked(move |_| window.close());
        let weak = Rc::downgrade(&this);
        copy.connect_clicked(move |b| {
            if let Some(dialog) = weak.upgrade() {
                if let Some(snapshot) = dialog.snapshot.borrow().as_ref() {
                    copy_to_clipboard(b, &copy_all(snapshot));
                }
            }
        });
        this.rebuild();
        this
    }

    pub fn window(&self) -> &gtk::Window {
        &self.window
    }

    pub fn present(&self) {
        self.window.present();
    }

    pub fn set_busy(&self, busy: bool) {
        self.refresh.set_sensitive(!busy);
        self.refresh.set_label(&if busy { tr("Refreshing…") } else { tr("Refresh") });
    }

    pub fn accept_result(&self, result: SystemProperties) {
        *self.snapshot.borrow_mut() = Some(result);
        self.rebuild();
        self.set_busy(false);
        self.copy.set_sensitive(true);
    }

    pub fn is_loaded(&self) -> bool {
        self.snapshot.borrow().is_some()
    }

    pub fn tab_titles(&self) -> Vec<String> {
        (0..self.notebook.n_pages())
            .filter_map(|n| self.notebook.nth_page(Some(n)))
            .filter_map(|p| self.notebook.tab_label_text(&p).map(|t| t.to_string()))
            .collect()
    }

    /// The text shown for a field key (test hook).
    pub fn field_text(&self, key: &str) -> Option<String> {
        self.texts.borrow().iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    fn rebuild(&self) {
        let current = self.notebook.current_page();
        while self.notebook.n_pages() > 0 {
            self.notebook.remove_page(Some(0));
        }
        self.texts.borrow_mut().clear();
        let snapshot = self.snapshot.borrow();
        for section in sections() {
            let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
            let grid = field_grid();
            let mut row = 0;
            for field in &section.fields {
                let (text, source) = match snapshot.as_ref() {
                    Some(s) => (display_value(s, field.key, &s.values.get(field.key).cloned().unwrap_or_default()), field_source(s, field.key)),
                    None => (tr("Not collected"), String::new()),
                };
                add_field(&grid, row, &field.caption, &text, &source);
                self.texts.borrow_mut().push((field.key.to_string(), text));
                row += 1;
                if field.key == "os" {
                    grid.attach(&gtk::Separator::new(gtk::Orientation::Horizontal), 0, row, 2, 1);
                    row += 1;
                }
            }
            page.append(&grid);
            if let Some(s) = snapshot.as_ref() {
                if section.fields.first().is_some_and(|f| f.key == "manufacturer") {
                    self.append_mok(&page, s);
                }
                if section.fields.first().is_some_and(|f| f.key == "physical_memory") {
                    self.append_memory(&page, s);
                }
            }
            self.notebook.append_page(&scrolled(&page), Some(&gtk::Label::new(Some(&section.title))));
        }
        if let Some(n) = current {
            self.notebook.set_current_page(Some(n));
        }
    }

    fn append_mok(&self, page: &gtk::Box, snapshot: &SystemProperties) {
        let scope = mok_scope(snapshot);
        for block in mok_blocks(snapshot) {
            match block {
                MokBlock::Message(text) => {
                    let label = plain_label(&text);
                    label.set_tooltip_text(Some(&scope));
                    label.set_margin_start(8);
                    page.append(&label);
                    self.texts.borrow_mut().push(("mok".into(), text));
                }
                MokBlock::Certificate { title, fields } => {
                    let frame = gtk::Frame::new(Some(&title));
                    frame.set_margin_start(8);
                    frame.set_margin_end(8);
                    let grid = field_grid();
                    for (row, (caption, text, details)) in fields.iter().enumerate() {
                        add_field(&grid, row as i32, caption.trim_end_matches(':'), text, &format!("{scope}\n\n{details}"));
                    }
                    frame.set_child(Some(&grid));
                    page.append(&frame);
                    self.texts.borrow_mut().push(("mok".into(), fields.iter().map(|f| f.1.clone()).collect::<Vec<_>>().join(" | ")));
                }
            }
        }
    }

    fn append_memory(&self, page: &gtk::Box, snapshot: &SystemProperties) {
        let grid = field_grid();
        for (row, block) in memory_blocks(snapshot).into_iter().enumerate() {
            let row = row as i32;
            match block {
                MemoryBlock::Message { text, tooltip } => {
                    let label = plain_label(&text);
                    label.set_tooltip_text((!tooltip.is_empty()).then_some(tooltip.as_str()));
                    grid.attach(&label, 0, row, 2, 1);
                    self.texts.borrow_mut().push(("memory_devices".into(), text));
                }
                MemoryBlock::Heading(text) => grid.attach(&heading(&text), 0, row, 2, 1),
                MemoryBlock::Field { caption, value, tooltip } => add_field(&grid, row, &caption, &value, &tooltip),
            }
        }
        page.append(&grid);
    }
}
