//! Drives the real window against this machine's devices. GTK needs the main thread, hence no
//! libtest harness. Skips (exit 0) when no display is available.
use gtk::glib;
use gtk::prelude::*;
use qldm_gtk::window::Ui;
use std::rc::Rc;
use std::time::{Duration, Instant};

fn pump_until(what: &str, limit: Duration, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    let context = glib::MainContext::default();
    while !done() {
        assert!(started.elapsed() < limit, "timed out waiting for {what}");
        context.iteration(false);
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn settle(ms: u64) {
    let started = Instant::now();
    let context = glib::MainContext::default();
    while started.elapsed() < Duration::from_millis(ms) {
        context.iteration(false);
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn check(ok: bool, message: &str) {
    if !ok {
        eprintln!("FAIL: {message}");
        std::process::exit(1);
    }
    println!("ok: {message}");
}

fn main() {
    if gtk::init().is_err() {
        println!("skipped: no display");
        return;
    }
    // Never touch the user's real settings file.
    let config = std::env::temp_dir().join(format!("qldm-ui-smoke-{}", std::process::id()));
    std::env::set_var("XDG_CONFIG_HOME", &config);
    let app =
        gtk::Application::builder().application_id("io.github.birdie-github.QLinuxDeviceManager.UiSmoke").flags(gtk::gio::ApplicationFlags::NON_UNIQUE).build();
    app.register(gtk::gio::Cancellable::NONE).expect("register");
    let ui: Rc<Ui> = Ui::build(&app);

    pump_until("the first inventory", Duration::from_secs(60), || !ui.is_initializing());
    let rows = ui.listed_rows();
    check(
        rows[0].0 == 0 && rows.len() > 1 && rows[1..].iter().all(|(d, _)| *d == 1),
        "the tree starts with the computer expanded and every category collapsed",
    );
    check(!rows[0].1.is_empty(), "the computer root has a caption");

    ui.expand_all();
    let rows = ui.listed_rows();
    for (depth, label) in rows.iter().take(40) {
        println!("{}{}", "  ".repeat(*depth as usize), label);
    }
    let devices: Vec<&String> = rows.iter().filter(|(d, _)| *d >= 2).map(|(_, l)| l).collect();
    check(devices.len() > 5, "categories expand into devices");
    check(
        devices.iter().all(|l| !(l.len() == 12 && l.chars().nth(4) == Some(':') && l.chars().nth(7) == Some(':'))),
        "no device is shown as a bare bus address such as 0000:03:00.0",
    );
    check(rows.iter().any(|(d, l)| *d == 1 && (l.contains("Network") || l.contains("Display") || l.contains("Disk"))), "category captions are readable");
    println!("status: {}", ui.status_text());
    check(ui.status_text().contains("shown /") && ui.status_text().contains("discovered"), "the status line counts devices");

    // Quick filter by displayed name.
    let sample = devices.iter().find(|l| l.len() > 6).map(|l| l.to_string()).expect("a named device");
    let needle: String = sample.chars().take(6).collect();
    ui.set_filter(&needle, false);
    settle(500);
    let filtered = ui.listed_rows();
    check(filtered.iter().any(|(_, l)| *l == sample), "filtering by name keeps the matching device");
    check(filtered.len() < rows.len(), "filtering removes non-matching rows");
    ui.clear_filter();
    settle(300);
    ui.expand_all();
    check(ui.listed_rows().len() >= rows.len() / 2, "clearing the filter restores the tree");

    // Deep search finds a device by metadata that is not in its label.
    ui.set_filter("PCI_CLASS", true);
    settle(200);
    pump_until("deep search progress", Duration::from_secs(60), || ui.status_text().contains("Deep search"));
    pump_until("deep search to finish", Duration::from_secs(120), || {
        let status = ui.status_text();
        status
            .split("Deep search: ")
            .nth(1)
            .and_then(|t| t.split(|c: char| !c.is_ascii_digit() && c != '/').next())
            .is_some_and(|p| p.split_once('/').is_some_and(|(done, total)| done == total && total != "0"))
    });
    check(ui.listed_rows().len() > 1, "deep search lists matching devices by metadata");
    ui.clear_filter();
    settle(300);

    // Every projection keeps one computer root.
    for view in ["connection", "devicesByDriver", "driversByDevice", "driversByType", "resourcesByType", "resourcesByConnection", "type"] {
        ui.activate_view(view);
        settle(150);
        ui.expand_all();
        let rows = ui.listed_rows();
        check(rows.iter().filter(|(d, _)| *d == 0).count() == 1 && rows.len() > 1, &format!("view {view} has one computer root and content"));
    }
    ui.collapse_all();
    check(ui.listed_rows().len() == 1, "collapse all leaves the root");

    // Selecting a device enables Properties; the root opens system information.
    ui.expand_all();
    check(ui.select_label(&sample), "a device row can be selected");
    check(ui.properties_enabled(), "Properties is enabled for a device");
    check(ui.status_text().contains("/sys/"), "the status line shows the selected device path");

    // Properties opens a real tabbed window per device class.
    for (needle, tabs) in [
        ("Wireless Network Adapter", &["General", "Driver", "Network", "PCIe", "Details", "Events"][..]),
        ("UVC WebCam: USB2.0 HD [video0]", &["General", "Driver", "Camera", "Details", "Events"][..]),
        ("WD_BLACK SN770", &["General", "Details", "Events", "Storage", "Volumes", "Health"][..]),
        ("card1-eDP-1", &["General", "Display", "Details", "Events"][..]),
        ("AC power adapter", &["General", "Driver", "Details", "Events"][..]),
    ] {
        ui.expand_all();
        if !ui.select_label(needle) {
            println!("skip: no row {needle:?} on this machine");
            continue;
        }
        ui.open_selected_properties();
        let dialog = ui.properties_dialog().expect("the dialog is open");
        pump_until("properties to load", Duration::from_secs(60), || !dialog.is_loading());
        settle(200);
        let titles = dialog.tab_titles();
        println!("{needle}: {titles:?}");
        check(tabs.iter().all(|t| titles.iter().any(|x| x == t)), &format!("{needle}: tabs {tabs:?} are present"));
        check(dialog.banner_text().is_empty(), &format!("{needle}: properties loaded without a warning banner ({})", dialog.banner_text()));
        check(dialog.show_tab("Details") && dialog.show_tab("Events"), "tabs can be switched");
        dialog.window().close();
        settle(100);
        check(ui.properties_dialog().is_none(), "closing the window releases the dialog");
    }
    // Opening Properties twice for the same instance reuses the window.
    ui.expand_all();
    ui.select_label(&sample);
    ui.open_selected_properties();
    let first = ui.properties_dialog().expect("dialog");
    ui.open_selected_properties();
    check(std::rc::Rc::ptr_eq(&first, &ui.properties_dialog().expect("dialog")), "reopening Properties for the same instance reuses the window");
    pump_until("properties to load", Duration::from_secs(60), || !first.is_loading());
    // A refresh with the dialog open does not reload or retarget it.
    ui.request_refresh();
    pump_until("the refresh to finish", Duration::from_secs(60), || !ui.is_busy());
    settle(200);
    check(!first.is_removed(), "an unchanged inventory leaves the dialog live");
    first.window().close();
    settle(100);

    // The computer root opens System Information with real collected values.
    ui.select_root();
    ui.open_selected_properties();
    let system = ui.system_dialog().expect("the system dialog exists");
    pump_until("system information to load", Duration::from_secs(60), || system.is_loaded());
    settle(200);
    let titles = system.tab_titles();
    println!("system tabs: {titles:?}");
    check(titles == ["Operating System", "Hardware", "Processor", "RAM/Memory"], "System Information has its four tabs");
    for key in ["os", "kernel", "cpu", "memory", "boot", "secure_boot", "uptime", "logical"] {
        let text = system.field_text(key).unwrap_or_default();
        println!("  {key}: {}", text.replace('\n', " | "));
        check(!text.is_empty() && text != "Not collected", &format!("system field {key} is collected"));
    }
    check(system.field_text("kernel").unwrap_or_default().starts_with("Linux "), "the kernel version is reported");
    check(system.field_text("memory").unwrap_or_default().contains("GiB"), "memory is shown in GiB");
    system.window().close();
    settle(100);
    ui.select_root();
    ui.open_selected_properties();
    check(std::rc::Rc::ptr_eq(&system, &ui.system_dialog().expect("dialog")), "reopening System Information reuses the retained window and snapshot");

    // Show all devices adds internal records.
    ui.expand_all();
    check(ui.select_label(&sample), "the device row can be selected again");
    let before = ui.status_text();
    ui.set_show_all(true);
    settle(300);
    let after = ui.status_text();
    println!("before: {before}\nafter:  {after}");
    check(before != after, "show all devices changes the shown count");
    let selected_path = before.rsplit(" — ").next().unwrap_or_default().to_string();
    check(selected_path.starts_with("/sys/") && after.ends_with(&selected_path), "the selection survives the rebuild by path and generation");
    ui.set_show_all(false);

    // A manual refresh settles back to idle.
    ui.request_refresh();
    pump_until("the refresh to finish", Duration::from_secs(60), || !ui.is_busy());
    settle(200);
    check(ui.status_text().contains("shown /"), "refresh returns to the idle status");

    // Closing joins the worker without blocking the UI and destroys the window.
    let started = Instant::now();
    ui.window().close();
    let window = ui.window().clone();
    drop(ui);
    pump_until("the window to be destroyed", Duration::from_secs(20), || !window.is_visible());
    println!("closed in {:?}", started.elapsed());
    check(true, "closing destroys the window");
    let saved = std::fs::read_to_string(config.join("QLinuxDeviceManager/settings.conf")).unwrap_or_default();
    check(saved.contains("window.width") && saved.contains("window.height"), "closing saved the window size");
    let _ = std::fs::remove_dir_all(&config);
}
