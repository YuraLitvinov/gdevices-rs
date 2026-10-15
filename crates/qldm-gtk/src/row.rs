//! GObject row used as the item type of the device tree model. A row mirrors one
//! [`qldm_core::tree::TreeNode`]: the node identity (key, path, generation) is what selection and
//! expansion are restored by, never a row position.
use gtk::glib;
use gtk::subclass::prelude::*;
use qldm_core::tree::TreeNode;
use std::cell::{Cell, RefCell};

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Row {
        pub label: RefCell<String>,
        pub icon: RefCell<String>,
        pub key: RefCell<String>,
        pub path: RefCell<String>,
        pub owner_path: RefCell<String>,
        pub tooltip: RefCell<String>,
        pub generation: Cell<u64>,
        pub context: Cell<bool>,
        pub children: RefCell<Vec<super::Row>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Row {
        const NAME: &'static str = "QldmRow";
        type Type = super::Row;
    }

    impl ObjectImpl for Row {}
}

glib::wrapper! {
    pub struct Row(ObjectSubclass<imp::Row>);
}

impl Row {
    /// Builds the row (and, recursively, its children) for a tree node; `tooltip` renders the
    /// tooltip text of a node.
    pub fn from_node(node: &TreeNode, tooltip: &dyn Fn(&TreeNode) -> String) -> Self {
        let row: Self = glib::Object::new();
        let imp = row.imp();
        *imp.label.borrow_mut() = node.label.clone();
        *imp.icon.borrow_mut() = if node.key == "computer" { "computer".into() } else { qldm_core::model::category(&node.category).icon.into() };
        *imp.key.borrow_mut() = node.key.clone();
        *imp.path.borrow_mut() = node.path.clone();
        *imp.owner_path.borrow_mut() = node.owner_path.clone();
        *imp.tooltip.borrow_mut() = tooltip(node);
        imp.generation.set(node.generation);
        imp.context.set(node.context);
        *imp.children.borrow_mut() = node.children.iter().map(|c| Row::from_node(c, tooltip)).collect();
        row
    }

    pub fn label(&self) -> String {
        self.imp().label.borrow().clone()
    }
    pub fn icon(&self) -> String {
        self.imp().icon.borrow().clone()
    }
    pub fn key(&self) -> String {
        self.imp().key.borrow().clone()
    }
    pub fn path(&self) -> String {
        self.imp().path.borrow().clone()
    }
    pub fn owner_path(&self) -> String {
        self.imp().owner_path.borrow().clone()
    }
    pub fn tooltip(&self) -> String {
        self.imp().tooltip.borrow().clone()
    }
    pub fn generation(&self) -> u64 {
        self.imp().generation.get()
    }
    pub fn is_context(&self) -> bool {
        self.imp().context.get()
    }
    pub fn children(&self) -> Vec<Row> {
        self.imp().children.borrow().clone()
    }

    /// The device this row acts on (a resource row acts on its owner), with its generation.
    pub fn target(&self) -> Option<(String, u64)> {
        let path = if self.path().is_empty() { self.owner_path() } else { self.path() };
        (!path.is_empty()).then(|| (path, self.generation()))
    }
}
