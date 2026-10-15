//! Small building blocks shared by the Properties tabs: selectable wrapped labels, grouped
//! caption/value grids with source tooltips, a plain text table, and copy buttons.
use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use qldm_core::report::ReportGroup;
use qldm_core::storage_presentation::StorageDisplayField;
use std::cell::RefCell;

/// A selectable, wrapping plain-text label.
pub fn plain_label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_wrap(true);
    label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    label.set_xalign(0.0);
    label.set_yalign(0.0);
    label.set_selectable(true);
    label.set_hexpand(true);
    label.set_natural_wrap_mode(gtk::NaturalWrapMode::Word);
    label
}

fn tooltip(source: &str) -> Option<&str> {
    (!source.is_empty()).then_some(source)
}

/// One captioned row; the source is the tooltip of both cells.
pub fn add_field(grid: &gtk::Grid, row: i32, caption: &str, value: &str, source: &str) {
    let name = gtk::Label::new(Some(&format!("{caption}:")));
    name.set_xalign(0.0);
    name.set_yalign(0.0);
    name.set_wrap(true);
    name.add_css_class("dim-label");
    let text = plain_label(value);
    for widget in [name.upcast_ref::<gtk::Widget>(), text.upcast_ref()] {
        widget.set_tooltip_text(tooltip(source));
    }
    grid.attach(&name, 0, row, 1, 1);
    grid.attach(&text, 1, row, 1, 1);
}

pub fn field_grid() -> gtk::Grid {
    let grid = gtk::Grid::new();
    grid.set_column_spacing(12);
    grid.set_row_spacing(6);
    grid.set_margin_top(6);
    grid.set_margin_bottom(6);
    grid.set_margin_start(8);
    grid.set_margin_end(8);
    grid
}

/// Report groups as titled frames, in one scrollable column.
pub fn groups_box(groups: &[ReportGroup]) -> gtk::Box {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 8);
    column.set_margin_top(8);
    column.set_margin_bottom(8);
    column.set_margin_start(8);
    column.set_margin_end(8);
    for group in groups {
        let frame = gtk::Frame::new(Some(&group.title));
        let grid = field_grid();
        for (row, field) in group.fields.iter().enumerate() {
            add_field(&grid, row as i32, &field.caption, &field.value, &field.source);
        }
        frame.set_child(Some(&grid));
        column.append(&frame);
    }
    column
}

/// Storage-style fields in a plain grid.
pub fn storage_fields_grid(fields: &[StorageDisplayField]) -> gtk::Grid {
    let grid = field_grid();
    for (row, field) in fields.iter().enumerate() {
        add_field(&grid, row as i32, &field.label, &field.value, &field.source);
    }
    grid
}

pub fn scrolled(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    gtk::ScrolledWindow::builder().child(child).hexpand(true).vexpand(true).build()
}

pub fn copy_to_clipboard(widget: &impl IsA<gtk::Widget>, text: &str) {
    widget.display().clipboard().set_text(text);
}

pub fn copy_button(label: &str, text: impl Fn() -> String + 'static) -> gtk::Button {
    let button = gtk::Button::with_label(label);
    button.connect_clicked(move |b| copy_to_clipboard(b, &text()));
    button
}

pub fn notice(text: &str, tip: &str) -> gtk::Label {
    let label = plain_label(text);
    label.set_margin_start(8);
    label.set_margin_end(8);
    label.set_margin_top(6);
    label.set_tooltip_text(tooltip(tip));
    label
}

mod cells_imp {
    use super::*;

    #[derive(Default)]
    pub struct CellRow {
        pub cells: RefCell<Vec<String>>,
        pub tooltip: RefCell<String>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for CellRow {
        const NAME: &'static str = "QldmCellRow";
        type Type = super::CellRow;
    }

    impl ObjectImpl for CellRow {}
}

glib::wrapper! {
    /// One table row of text cells.
    pub struct CellRow(ObjectSubclass<cells_imp::CellRow>);
}

impl CellRow {
    pub fn new(cells: Vec<String>, tooltip: &str) -> Self {
        let row: Self = glib::Object::new();
        *row.imp().cells.borrow_mut() = cells;
        *row.imp().tooltip.borrow_mut() = tooltip.to_string();
        row
    }

    pub fn cell(&self, column: usize) -> String {
        self.imp().cells.borrow().get(column).cloned().unwrap_or_default()
    }

    pub fn cells(&self) -> Vec<String> {
        self.imp().cells.borrow().clone()
    }

    pub fn tooltip(&self) -> String {
        self.imp().tooltip.borrow().clone()
    }
}

/// A read-only text table with resizable columns.
pub struct TextTable {
    pub view: gtk::ColumnView,
    pub store: gtk::gio::ListStore,
}

impl TextTable {
    /// `right_aligned` lists columns whose text is right aligned (numbers).
    pub fn new(headers: &[String], multi: bool, right_aligned: &[usize]) -> (Self, gtk::SelectionModel) {
        let store = gtk::gio::ListStore::new::<CellRow>();
        let selection: gtk::SelectionModel = if multi {
            gtk::MultiSelection::new(Some(store.clone())).upcast()
        } else {
            let single = gtk::SingleSelection::new(Some(store.clone()));
            single.set_autoselect(false);
            single.set_can_unselect(true);
            single.upcast()
        };
        let view = gtk::ColumnView::new(Some(selection.clone()));
        view.set_show_row_separators(true);
        view.set_reorderable(false);
        for (column, header) in headers.iter().enumerate() {
            let factory = gtk::SignalListItemFactory::new();
            let right = right_aligned.contains(&column);
            factory.connect_setup(move |_, item| {
                let item = item.downcast_ref::<gtk::ListItem>().expect("ListItem");
                let label = gtk::Label::new(None);
                label.set_xalign(if right { 1.0 } else { 0.0 });
                label.set_yalign(0.0);
                label.set_wrap(true);
                label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
                label.set_margin_start(4);
                label.set_margin_end(4);
                item.set_child(Some(&label));
            });
            factory.connect_bind(move |_, item| {
                let item = item.downcast_ref::<gtk::ListItem>().expect("ListItem");
                let (Some(row), Some(label)) = (item.item().and_downcast::<CellRow>(), item.child().and_downcast::<gtk::Label>()) else { return };
                label.set_text(&row.cell(column));
                let tip = row.tooltip();
                label.set_tooltip_text(tooltip(&tip));
            });
            let col = gtk::ColumnViewColumn::new(Some(header), Some(factory));
            col.set_resizable(true);
            col.set_expand(column + 1 == headers.len());
            view.append_column(&col);
        }
        (Self { view, store }, selection)
    }

    pub fn set_rows(&self, rows: Vec<CellRow>) {
        self.store.remove_all();
        self.store.extend_from_slice(&rows);
    }
}
