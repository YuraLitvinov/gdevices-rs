pub mod camera;
pub mod cpuinfo;
pub mod dirfd;
pub mod display;
pub mod display_wayland;
pub mod display_x11;
pub mod efivars;
pub mod enumerate;
pub mod ethernet;
pub mod events;
pub mod events_view;
pub mod functionnames;
pub mod i18n;
pub mod label;
pub mod model;
pub mod modulenames;
pub mod monitor;
pub mod monitor_edid;
pub mod monitorstate;
pub mod naming;
pub mod network;
pub mod presentation;
pub mod properties;
pub mod propertytext;
pub mod report;
pub mod resourceformat;
pub mod resources;
pub mod search;
pub mod secureboot;
pub mod storage;
pub mod storage_presentation;
pub mod sysfs;
pub mod system;
pub mod system_report;
pub mod tree;
pub mod v4l2_sys;

pub use enumerate::scan;

/// Application identity from `project.json`, fixed at build time.
pub mod app {
    pub const NAME: &str = env!("QLDM_NAME");
    pub const VERSION: &str = env!("QLDM_VERSION");
    pub const REPOSITORY: &str = env!("QLDM_REPOSITORY");
}
