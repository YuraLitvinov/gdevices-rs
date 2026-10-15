use gtk::prelude::*;
use gtk::{gio, glib};
use qldm_gtk::window::Ui;

const APP_ID: &str = "io.github.birdie-github.QLinuxDeviceManager";

fn main() -> glib::ExitCode {
    qldm_core::i18n::init();
    let app = gtk::Application::builder().application_id(APP_ID).flags(gio::ApplicationFlags::default()).build();
    app.connect_activate(|app| {
        // A second launch just raises the existing window.
        if let Some(existing) = app.active_window() {
            existing.present();
            return;
        }
        Ui::build(app);
    });
    app.run()
}
