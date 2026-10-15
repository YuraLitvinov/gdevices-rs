//! Text content of the specialised Properties tabs (Network, Camera, Display, USB, PCIe,
//! Battery), ported from the `rebuild*` methods of `src/ui/propertiesdialog.cpp` without any
//! toolkit types. Each builder returns grouped caption/value/source fields (the source is the
//! tooltip) plus the exact text the tab's Copy button places on the clipboard.
use crate::display::DisplayProperties;
use crate::i18n::{arg, arg2, format_g, format_integer, tr};
use crate::model::{Attribute, Device, ReadState};
use crate::properties::DeviceProperties;
use crate::propertytext::property_read_value;
use serde_json::Value;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReportField {
    pub caption: String,
    pub value: String,
    pub source: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReportGroup {
    pub title: String,
    pub fields: Vec<ReportField>,
}

/// A table of equal-width text rows (the camera's mode list).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReportTable {
    pub title: String,
    pub headers: Vec<String>,
    /// Display cells per row.
    pub rows: Vec<Vec<String>>,
    /// Tooltip per row.
    pub sources: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub groups: Vec<ReportGroup>,
    pub tables: Vec<ReportTable>,
    /// What the Copy button puts on the clipboard.
    pub copy_text: String,
}

#[derive(Default)]
struct Builder {
    report: Report,
    copy: Vec<String>,
}

impl Builder {
    fn group(&mut self, title: String) {
        if !self.copy.is_empty() {
            self.copy.push(String::new());
        }
        self.copy.push(title.clone());
        self.report.groups.push(ReportGroup { title, fields: Vec::new() });
    }

    fn field(&mut self, caption: impl Into<String>, value: impl Into<String>, source: impl Into<String>) {
        let (caption, value) = (caption.into(), value.into());
        self.copy.push(format!("{caption}: {value}"));
        self.report.groups.last_mut().expect("a group was opened").fields.push(ReportField { caption, value, source: source.into() });
    }

    fn finish(mut self) -> Report {
        self.report.copy_text = self.copy.join("\n");
        self.report
    }
}

fn read(value: &Attribute) -> String {
    property_read_value(value)
}

fn attribute(state: ReadState, error: i32) -> Attribute {
    Attribute { state, value: String::new(), error }
}

// ---------------------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------------------

pub fn network_applicable(device: &Device) -> bool {
    device.subsystem == "net" || device.category == "network"
}

fn ethernet_value(value: &Attribute) -> String {
    match value.state {
        ReadState::Unsupported => return tr("Query unsupported by the kernel/device"),
        ReadState::Error => match value.error {
            libc::EPROTO => return tr("Malformed ethtool reply"),
            libc::ETIMEDOUT => return tr("Query timed out"),
            libc::EOVERFLOW => return tr("Reply exceeds collection limits"),
            libc::EAGAIN => return tr("Link changed during collection; reload properties"),
            _ => {}
        },
        _ => {}
    }
    read(value)
}

fn speed_text(mbps: u64) -> String {
    if mbps >= 1000 {
        arg(tr("%1 Gbps"), &format_g(mbps as f64 / 1000.0, 6))
    } else {
        arg(tr("%1 Mbps"), &format_integer(mbps))
    }
}

pub fn network_report(snapshot: &DeviceProperties) -> Option<Report> {
    let device = &snapshot.device;
    if !network_applicable(device) {
        return None;
    }
    let mut b = Builder::default();
    if snapshot.network.is_empty() {
        b.group(tr("Network interfaces"));
        let value = if snapshot.network_state.state == ReadState::Available { tr("No network interfaces observed") } else { read(&snapshot.network_state) };
        b.field(tr("Interfaces"), value, device.path.clone());
    }
    for interface in &snapshot.network {
        let ethernet = &interface.ethernet;
        if ethernet.applicable {
            b.group(arg(tr("%1 — Ethernet link"), &interface.name));
            let source = format!("{}\n{}", interface.path, ethernet.source);
            let disconnected = ethernet.carrier.state == ReadState::Available && ethernet.carrier.value == "0";
            let link = if ethernet.carrier.state == ReadState::Available {
                if disconnected {
                    tr("Disconnected")
                } else {
                    tr("Connected")
                }
            } else {
                ethernet_value(&ethernet.carrier)
            };
            b.field(tr("Link status"), link, source.clone());
            let operational = |value: &Attribute| {
                if value.state == ReadState::NotApplicable && disconnected {
                    tr("Not applicable — link disconnected")
                } else {
                    ethernet_value(value)
                }
            };
            let mut speed = operational(&ethernet.speed);
            if ethernet.speed.state == ReadState::Available {
                speed = match ethernet.speed.value.parse::<u64>() {
                    Err(_) => tr("Malformed ethtool reply"),
                    Ok(0) | Ok(0xffff_ffff) => tr("Unavailable (driver reports unknown speed)"),
                    Ok(mbps) => speed_text(mbps),
                };
            }
            b.field(
                tr("Link speed"),
                speed,
                format!("{source}\n{}", tr("Driver-reported operational speed, not measured throughput; it may be forced when autonegotiation is disabled.")),
            );
            let mut duplex = operational(&ethernet.duplex);
            if ethernet.duplex.state == ReadState::Available {
                duplex = match ethernet.duplex.value.as_str() {
                    "0" => tr("Half duplex"),
                    "1" => tr("Full duplex"),
                    "255" => tr("Unavailable (driver reports unknown duplex)"),
                    other => arg(tr("Unrecognized duplex value: %1"), other),
                };
            }
            b.field(tr("Duplex"), duplex, source.clone());
            let mut autoneg = ethernet_value(&ethernet.autoneg);
            if ethernet.autoneg.state == ReadState::Available {
                autoneg = match ethernet.autoneg.value.as_str() {
                    "0" => tr("Disabled"),
                    "1" => tr("Enabled"),
                    other => arg(tr("Unrecognized autonegotiation value: %1"), other),
                };
            }
            b.field(tr("Autonegotiation"), autoneg, source.clone());
            b.field(
                tr("Supported modes / capabilities"),
                ethernet_value(&ethernet.supported_modes).replace('\n', ", "),
                format!("{source}\n{}", tr("Kernel-named supported-mask entries include link modes and media, pause or autonegotiation capabilities. Unnamed bits retain their numeric index.")),
            );
            let mut partner = operational(&ethernet.partner_modes);
            if ethernet.partner_modes.state == ReadState::NotApplicable && !disconnected {
                partner = tr("Not applicable — autonegotiation disabled");
            } else if ethernet.partner_modes.state == ReadState::Unavailable {
                partner = tr("Unavailable — no partner advertisement reported");
            }
            b.field(tr("Link-partner advertised modes"), partner.replace('\n', ", "), source.clone());
        }
        b.group(arg(tr("%1 — Link"), &interface.name));
        let no_mac = interface.link_type.state == ReadState::Available && interface.link_type.value != "1";
        let current = if no_mac { attribute(ReadState::NotApplicable, 0) } else { interface.current_address.clone() };
        b.field(tr("Hardware MAC address"), read(&interface.permanent_address), tr("Driver permanent address: ETHTOOL_GPERMADDR"));
        b.field(tr("Current MAC address"), read(&current), format!("{}/address", interface.path));
        let override_value = if no_mac {
            tr("Not applicable")
        } else if current.state != ReadState::Available {
            read(&current)
        } else if interface.permanent_address.state == ReadState::Available {
            if current.value.eq_ignore_ascii_case(&interface.permanent_address.value) {
                tr("None")
            } else {
                current.value.clone()
            }
        } else if interface.assignment_type.state == ReadState::Available && matches!(interface.assignment_type.value.as_str(), "1" | "2" | "3") {
            current.value.clone()
        } else {
            tr("Unavailable; permanent MAC could not be determined")
        };
        b.field(tr("Overridden MAC address"), override_value, format!("{0}/address; {0}/addr_assign_type; ETHTOOL_GPERMADDR", interface.path));
        b.field(tr("MTU"), read(&interface.mtu), format!("{}/mtu", interface.path));
        if !ethernet.applicable && ethernet.scope.state != ReadState::NotApplicable {
            b.field(tr("Ethernet details"), ethernet_value(&ethernet.scope), ethernet.source.clone());
        }
        b.group(arg(tr("%1 — IP addresses"), &interface.name));
        let source = tr("getifaddrs: application network namespace");
        if interface.addresses.is_empty() {
            let value = if interface.address_state.state == ReadState::Available { tr("None assigned") } else { read(&interface.address_state) };
            b.field(tr("IP addresses"), value, source.clone());
        }
        for address in &interface.addresses {
            b.field(
                if address.family == "IPv4" { tr("IPv4 address") } else { tr("IPv6 address") },
                format!("{}/{}", address.address, read(&address.netmask)),
                source.clone(),
            );
        }
    }
    Some(b.finish())
}

// ---------------------------------------------------------------------------------------
// Camera
// ---------------------------------------------------------------------------------------

fn camera_error_text(error: i32) -> String {
    match error {
        libc::EBUSY => return tr("Device busy"),
        libc::ETIMEDOUT => return tr("Query timed out"),
        libc::EAGAIN => return tr("Previous camera query has not exited; retry later"),
        libc::EOVERFLOW => return tr("Enumeration or report limit reached"),
        libc::EPROTO => return tr("Malformed driver response"),
        _ => {}
    }
    let state = match error {
        libc::EACCES | libc::EPERM => ReadState::PermissionDenied,
        libc::ENODEV | libc::ENOENT | libc::ENXIO => ReadState::Removed,
        libc::EINVAL | libc::ENOTTY | libc::EOPNOTSUPP => ReadState::Unsupported,
        _ => ReadState::Error,
    };
    read(&attribute(state, error))
}

/// `1/30` as text.
fn fraction_text(f: &Value) -> String {
    format!("{}/{}", f[0].as_i64().unwrap_or(0), f[1].as_i64().unwrap_or(0))
}

/// Frames per second of an interval fraction.
fn fps(f: &Value) -> f64 {
    f[1].as_i64().unwrap_or(0) as f64 / f[0].as_i64().unwrap_or(1) as f64
}

/// Four-character code of a V4L2 pixel format with unprintable bytes as `\xNN`.
pub fn fourcc(code: u32) -> String {
    [0u32, 8, 16, 24]
        .iter()
        .map(|shift| {
            let c = (code >> shift) & if *shift == 24 { 127 } else { 255 };
            if (32..=126).contains(&c) {
                (c as u8 as char).to_string()
            } else {
                format!("\\x{c:02x}")
            }
        })
        .collect()
}

pub fn camera_applicable(snapshot: &DeviceProperties) -> bool {
    snapshot.camera.applicable
}

pub fn camera_report(snapshot: &DeviceProperties) -> Option<Report> {
    let data = &snapshot.camera;
    if !data.applicable {
        return None;
    }
    let mut b = Builder::default();
    let report = &data.report;
    let empty = Vec::new();
    let modes = report.get("modes").and_then(Value::as_array).unwrap_or(&empty);
    b.group(tr("Capture device"));
    let query_source = format!("{}: VIDIOC_QUERYCAP via native ioctl passthrough", data.node);
    b.field(
        tr("Device node"),
        if data.node.is_empty() { tr("Unavailable") } else { data.node.clone() },
        format!("{} / udev devnode; verified sysfs dev and /sys/dev/char binding", snapshot.device.path),
    );
    let mut status = if data.state.state == ReadState::Available {
        tr("Driver-reported snapshot")
    } else if data.state.error != 0 {
        camera_error_text(data.state.error)
    } else {
        read(&data.state)
    };
    if data.state.state != ReadState::Available && !modes.is_empty() {
        status = arg(tr("Incomplete enumeration: %1"), &status);
    }
    b.field(tr("Query status"), status, format!("{}: read-only capability enumeration; two-second worker wait budget", data.node));
    for (key, caption) in [("card", tr("Device name")), ("driver", tr("Driver")), ("bus", tr("Bus information"))] {
        if let Some(value) = report.get(key) {
            let text = value.as_str().unwrap_or("");
            b.field(caption, if text.is_empty() { tr("Unavailable") } else { text.to_string() }, query_source.clone());
        }
    }
    b.group(tr("Collection scope"));
    b.field(
        tr("Coverage"),
        tr("This capture node and current media pipeline. No capture or configuration changes. Reported modes do not guarantee throughput or compatibility with other simultaneous streams."),
        query_source,
    );
    if !modes.is_empty() {
        let title = tr("Format, size and frame rate combinations");
        let headers = vec![tr("Capture queue"), tr("Pixel format"), tr("Resolution / range"), tr("Frame rates / intervals")];
        let mut table = ReportTable { title: title.clone(), headers: headers.clone(), ..Default::default() };
        b.copy.push(String::new());
        b.copy.push(title);
        for (index, row) in modes.iter().enumerate() {
            let code = row["format"].as_u64().unwrap_or(0) as u32;
            let code_text = fourcc(code);
            let description = row["description"].as_str().unwrap_or("");
            let flags = row["flags"].as_u64().unwrap_or(0) as u32;
            // The complete driver report goes to Copy, concise names to the table.
            let mut format = format!("{code_text} (0x{code:08x})");
            if code & 0x8000_0000 != 0 {
                format += &tr(" (big endian)");
            }
            if !description.is_empty() {
                format += &format!(" — {description}");
            }
            if flags & 1 != 0 {
                format += &tr(" (compressed)");
            }
            if flags & 2 != 0 {
                format += &tr(" (emulated, reported by driver)");
            }
            let mut display_format = if code_text == "MJPG" {
                "MJPEG".to_string()
            } else if code_text == "YUYV" {
                "YUYV 4:2:2".to_string()
            } else if description == code_text || description.starts_with(&format!("{code_text} ")) {
                description.to_string()
            } else if description.is_empty() {
                code_text.clone()
            } else {
                format!("{code_text} — {description}")
            };
            if code & 0x8000_0000 != 0 {
                display_format += &tr(" (big endian)");
            }
            if flags & 1 != 0 && code_text != "MJPG" {
                display_format += &tr(" (compressed)");
            }
            if flags & 2 != 0 {
                display_format += &tr(" (emulated, reported by driver)");
            }
            let kind = row["size_kind"].as_str().unwrap_or("");
            let int = |key: &str| row[key].as_i64().unwrap_or(0).to_string();
            let size = if let Some(error) = row.get("size_error") {
                camera_error_text(error.as_i64().unwrap_or(0) as i32)
            } else if kind == "discrete" {
                arg2(tr("%1 × %2"), &int("width"), &int("height"))
            } else {
                let mut size = argn4(tr("%1–%2 × %3–%4 pixels"), &int("width"), &int("max_width"), &int("height"), &int("max_height"));
                size +=
                    &if kind == "continuous" { tr(" (continuous)") } else { arg2(tr(" (steps: %1 × %2 pixels)"), &int("step_width"), &int("step_height")) };
                size
            };
            let interval = &row["intervals"];
            let rates = if kind != "discrete" {
                tr("Not enumerated: intervals depend on the exact size")
            } else if let Some(error) = interval.get("error") {
                camera_error_text(error.as_i64().unwrap_or(0) as i32)
            } else if interval["kind"] == "discrete" {
                interval["values"]
                    .as_array()
                    .map(|values| values.iter().map(|f| arg2(tr("%1 fps (%2 s)"), &format_g(fps(f), 8), &fraction_text(f))).collect::<Vec<_>>().join("\n"))
                    .unwrap_or_default()
            } else {
                let (min, max) = (&interval["min"], &interval["max"]);
                let mut rates =
                    argn4(tr("%1–%2 fps; intervals %3–%4 s"), &format_g(fps(max), 8), &format_g(fps(min), 8), &fraction_text(min), &fraction_text(max));
                rates +=
                    &if interval["kind"] == "continuous" { tr(" (continuous)") } else { arg(tr(" (interval step: %1 s)"), &fraction_text(&interval["step"])) };
                rates
            };
            let queue = if row["queue"] == "multi" { tr("Multiple planes") } else { tr("Single plane") };
            let values = [queue, display_format, size, rates];
            let mut copy_values = values.clone();
            copy_values[1] = format;
            let source = format!(
                "{}: VIDIOC_ENUM_FMT / VIDIOC_ENUM_FRAMESIZES / VIDIOC_ENUM_FRAMEINTERVALS\n{}",
                data.node,
                tr("Intervals queried only for this pixel format and exact discrete size. Ranges are not expanded.")
            );
            b.copy.push(String::new());
            b.copy.push(arg(tr("Mode %1"), &(index + 1).to_string()));
            for (header, value) in headers.iter().zip(&copy_values) {
                b.copy.push(format!("{header}: {value}"));
            }
            table.rows.push(values.to_vec());
            table.sources.push(source);
        }
        b.report.tables.push(table);
    }
    Some(b.finish())
}

/// Four placeholders, as chained `QString::arg` calls.
fn argn4(text: String, a: &str, b: &str, c: &str, d: &str) -> String {
    crate::i18n::argn(text, &[a, b, c, d])
}

// ---------------------------------------------------------------------------------------
// Display
// ---------------------------------------------------------------------------------------

pub fn display_applicable(device: &Device) -> bool {
    device.subsystem == "drm" && device.sysname.contains('-')
}

fn session_value(data: &DisplayProperties, key: &str) -> String {
    let value = if data.status.state == ReadState::Available {
        data.values.get(key).cloned().unwrap_or_default()
    } else {
        attribute(data.status.state, data.status.error)
    };
    if value.state != ReadState::Available {
        return read(&value);
    }
    match key {
        "resolution" => value.value.replace('x', "×"),
        "depth" => arg(tr("%1 bits per pixel"), &value.value),
        "refresh" => arg(tr("%1 Hz"), &format!("{:.2}", value.value.parse::<f64>().unwrap_or(0.0))),
        "rotation" => {
            if value.value == "0" {
                tr("0°")
            } else {
                arg(tr("%1° counterclockwise"), &value.value)
            }
        }
        "reflection" => match value.value.as_str() {
            "xy" => tr("X and Y axes"),
            "x" => tr("X axis"),
            "y" => tr("Y axis"),
            "vertical" => tr("Vertical axis"),
            _ => tr("None"),
        },
        "scale" => {
            if value.value == "custom" {
                return tr("Custom transform");
            }
            let axes: Vec<&str> = value.value.split(' ').collect();
            let (x, y) = (axes.first().and_then(|a| a.parse::<f64>().ok()).unwrap_or(0.0), axes.get(1).and_then(|a| a.parse::<f64>().ok()).unwrap_or(0.0));
            let mut text =
                if axes.first() == axes.get(1) { arg(tr("%1×"), &format_g(x, 6)) } else { arg2(tr("X: %1×, Y: %2×"), &format_g(x, 6), &format_g(y, 6)) };
            match data.values.get("scale_kind").map(|k| k.value.as_str()) {
                Some("derived") => text += &tr(" (derived)"),
                Some("integer") => text += &tr(" (integer buffer scale)"),
                _ => {}
            }
            text
        }
        _ => value.value,
    }
}

pub fn display_report(snapshot: &DeviceProperties) -> Option<Report> {
    if !display_applicable(&snapshot.device) {
        return None;
    }
    let data = &snapshot.display;
    let monitor = &snapshot.monitor;
    let mut b = Builder::default();
    if monitor.applicable {
        let hardware = |key: &str| read(&monitor.values.get(key).cloned().unwrap_or_default());
        let source_of = |key: &str| monitor.sources.get(key).cloned().unwrap_or_default();
        b.group(tr("Monitor hardware (reported)"));
        let descriptor = &monitor.descriptor;
        let status = match (descriptor.state, descriptor.error) {
            (ReadState::Available, _) => {
                if monitor.values.contains_key("warnings") {
                    tr("Decoded with conformance warnings")
                } else {
                    tr("Decoded by libdisplay-info")
                }
            }
            (ReadState::Unsupported, _) if descriptor.value.is_empty() => tr("EDID decoding unavailable: build with libdisplay-info support"),
            (ReadState::Unsupported, _) => arg(tr("Unsupported EDID version: %1"), &descriptor.value),
            (ReadState::Error, libc::EPROTO) => tr("Malformed EDID (invalid header, length, checksum or parser rejection)"),
            (ReadState::Error, libc::EAGAIN) => tr("Monitor report changed during collection; reload properties"),
            _ => read(descriptor),
        };
        b.field(tr("Descriptor"), status, source_of("descriptor"));
        let hardware_field = |b: &mut Builder, caption: String, key: &str| b.field(caption, hardware(key), source_of(key));
        if monitor.values.contains_key("version") {
            hardware_field(&mut b, tr("EDID version"), "version");
        }
        if descriptor.state == ReadState::Available {
            hardware_field(&mut b, tr("Manufacturer"), "manufacturer");
            hardware_field(&mut b, tr("Manufacturer code"), "manufacturer_code");
            hardware_field(&mut b, tr("Model"), "model");
            hardware_field(&mut b, tr("Product code"), "product");
            hardware_field(&mut b, tr("Serial number"), "serial");
            let size = monitor.values.get("size").cloned().unwrap_or_default();
            let dimensions: Vec<&str> = size.value.split(' ').collect();
            b.field(
                tr("Image dimensions"),
                if size.state == ReadState::Available {
                    arg2(tr("%1 × %2 cm"), dimensions.first().unwrap_or(&""), dimensions.get(1).unwrap_or(&""))
                } else {
                    read(&size)
                },
                source_of("size"),
            );
            if monitor.values.contains_key("warnings") {
                hardware_field(&mut b, tr("Conformance warnings"), "warnings");
            }
            b.group(tr("Monitor input (reported)"));
            let digital = monitor.values.get("digital").cloned().unwrap_or_default();
            b.field(
                tr("Signal type"),
                if digital.state == ReadState::Available {
                    if digital.value == "1" {
                        tr("Digital")
                    } else {
                        tr("Analog")
                    }
                } else {
                    read(&digital)
                },
                source_of("digital"),
            );
            if digital.state == ReadState::Available && digital.value == "1" {
                hardware_field(&mut b, tr("Input interface"), "interface");
                let depth = monitor.values.get("depth").cloned().unwrap_or_default();
                b.field(
                    tr("Declared color depth"),
                    if depth.state == ReadState::Available { arg(tr("%1 bits per color component"), &depth.value) } else { read(&depth) },
                    source_of("depth"),
                );
            }
        }
        b.group(tr("Kernel-reported modes"));
        hardware_field(&mut b, tr("Mode names"), "modes");
        b.field(
            tr("Scope"),
            tr("Cached mode names; refresh rates are not exposed here. The list may include driver additions or restrictions and does not guarantee monitor capabilities."),
            source_of("modes"),
        );
    }
    if data.status.state == ReadState::Available && data.status.value == "disconnected" {
        b.group(tr("Desktop session"));
        b.field(tr("Configuration"), tr("Disconnected"), data.sources.get("configuration").cloned().unwrap_or_default());
        return Some(b.finish());
    }
    let source = |key: &str| data.sources.get(key).cloned().unwrap_or_default();
    b.group(tr("Desktop session"));
    b.field(tr("Backend"), if data.backend.is_empty() { tr("Unavailable") } else { data.backend.clone() }, "");
    b.field(tr("Output"), if data.output.is_empty() { tr("Unavailable") } else { data.output.clone() }, snapshot.device.path.clone());
    b.field(
        tr("Configuration"),
        if data.status.state == ReadState::Available {
            if data.status.value == "inactive" { tr("No active mode") } else { tr("Reported by desktop server") }
        } else {
            read(&data.status)
        },
        tr("Read-only session query. Unavailable can mean missing runtime libraries, an output not exposed by this session, or a protocol without usable output names."),
    );
    b.group(tr("Active display mode"));
    b.field(tr("Resolution"), session_value(data, "resolution"), source("resolution"));
    b.field(tr("Desktop color depth"), session_value(data, "depth"), source("depth"));
    b.field(tr("Refresh rate"), session_value(data, "refresh"), source("refresh"));
    b.group(tr("Orientation and scale"));
    b.field(tr("Rotation"), session_value(data, "rotation"), source("rotation"));
    b.field(tr("Reflection"), session_value(data, "reflection"), source("reflection"));
    b.field(tr("Output scale"), session_value(data, "scale"), source("scale"));
    Some(b.finish())
}

// ---------------------------------------------------------------------------------------
// USB
// ---------------------------------------------------------------------------------------

pub fn usb_applicable(device: &Device) -> bool {
    device.subsystem == "usb" && device.devtype == "usb_device"
}

fn usb_class_text(attribute: &Attribute) -> String {
    if attribute.state != ReadState::Available {
        return read(attribute);
    }
    let Some(code) = u32::from_str_radix(&attribute.value, 16).ok().filter(|c| *c <= 255) else {
        return read(&self::attribute(ReadState::Error, libc::EINVAL));
    };
    let name = match code {
        0x01 => "Audio",
        0x02 => "Communications",
        0x03 => "Human interface device",
        0x07 => "Printer",
        0x08 => "Mass storage",
        0x09 => "Hub",
        0x0a => "CDC data",
        0x0b => "Smart card",
        0x0e => "Video",
        0xe0 => "Wireless controller",
        0xef => "Miscellaneous",
        0xfe => "Application-specific",
        0xff => "Vendor-specific",
        _ => "",
    };
    let hex = format!("0x{code:02x}");
    if name.is_empty() {
        hex
    } else {
        arg2(tr("%1 (%2)"), &tr(name), &hex)
    }
}

pub fn usb_report(snapshot: &DeviceProperties) -> Option<Report> {
    let device = &snapshot.device;
    if !usb_applicable(device) {
        return None;
    }
    let data = &snapshot.usb;
    let mut b = Builder::default();
    let value = |key: &str| read(&data.values.get(key).cloned().unwrap_or_default());
    let source = |key: &str| format!("{}/{key}", device.path);
    b.group(tr("Connection"));
    let version = snapshot.values.get("sysfs/version").cloned().unwrap_or_default();
    b.field(
        tr("USB version"),
        read(&version),
        format!("{}\n{}", source("version"), tr("USB specification version reported by the device descriptor; not the port's maximum capability.")),
    );
    let speed = snapshot.values.get("sysfs/speed").cloned().unwrap_or_default();
    let mut speed_text = read(&speed);
    if speed.state == ReadState::Available {
        let mbps = speed.value.parse::<f64>().ok();
        if speed.value.eq_ignore_ascii_case("unknown") {
            speed_text = read(&Attribute::default());
        } else if !mbps.is_some_and(|m| m.is_finite() && m > 0.0) {
            speed_text = read(&attribute(ReadState::Error, libc::EINVAL));
        } else {
            let mbps = mbps.expect("checked");
            speed_text = if mbps >= 1000.0 { arg(tr("%1 Gbps"), &format_g(mbps / 1000.0, 6)) } else { arg(tr("%1 Mbps"), &format_g(mbps, 6)) };
        }
    }
    b.field(
        tr("Connection speed"),
        speed_text,
        format!("{}\n{}", source("speed"), tr("Kernel-reported link rate, not measured throughput. For root hubs this describes the USB bus.")),
    );
    b.group(tr("Power declarations"));
    let attributes = data.values.get("bmAttributes").cloned().unwrap_or_default();
    let (mut supply, mut wake) = (read(&attributes), read(&attributes));
    if attributes.state == ReadState::Available {
        match u32::from_str_radix(&attributes.value, 16).ok().filter(|f| *f <= 255 && f & 0x80 != 0) {
            None => {
                supply = read(&attribute(ReadState::Error, libc::EINVAL));
                wake = supply.clone();
            }
            Some(flags) => {
                supply = if flags & 0x40 != 0 { tr("Self-powered") } else { tr("Bus-powered") };
                wake = if flags & 0x20 != 0 { tr("Supported") } else { tr("Not supported") };
            }
        }
    }
    b.field(tr("Power source"), supply, source("bmAttributes"));
    b.field(tr("Declared maximum current"), value("bMaxPower"), source("bMaxPower"));
    b.field(tr("Remote wakeup capability"), wake, source("bmAttributes"));
    b.group(tr("Configuration"));
    let active = data.values.get("bConfigurationValue").cloned().unwrap_or_default();
    b.field(
        tr("Active configuration"),
        if active.state == ReadState::Available && (active.value.is_empty() || active.value == "0") { tr("Unconfigured") } else { read(&active) },
        source("bConfigurationValue"),
    );
    b.field(tr("Available configurations"), value("bNumConfigurations"), source("bNumConfigurations"));
    let description = data.values.get("configuration").cloned().unwrap_or_default();
    b.field(
        tr("Description"),
        if description.state == ReadState::Available && description.value.is_empty() { tr("Not provided") } else { read(&description) },
        source("configuration"),
    );
    b.field(tr("Interface count"), value("bNumInterfaces"), source("bNumInterfaces"));
    if data.interface_state.state != ReadState::Available || data.interfaces.is_empty() {
        b.group(tr("Interfaces"));
        b.field(
            tr("Interfaces"),
            if data.interface_state.state == ReadState::Available { tr("None observed") } else { read(&data.interface_state) },
            device.path.clone(),
        );
    }
    for interface in &data.interfaces {
        b.group(arg(tr("Interface %1"), &interface.name));
        let get = |key: &str| interface.values.get(key).cloned().unwrap_or_default();
        if let Some(state) = interface.values.get("state") {
            b.field(tr("Status"), read(state), interface.path.clone());
            continue;
        }
        let entry = |b: &mut Builder, key: &str, caption: String| b.field(caption, read(&get(key)), format!("{}/{key}", interface.path));
        entry(&mut b, "bInterfaceNumber", tr("Number (hex)"));
        entry(&mut b, "bAlternateSetting", tr("Alternate setting"));
        let label = get("interface");
        b.field(
            tr("Description"),
            if label.state == ReadState::Available && label.value.is_empty() { tr("Not provided") } else { read(&label) },
            format!("{}/interface", interface.path),
        );
        b.field(tr("Function class"), usb_class_text(&get("bInterfaceClass")), format!("{}/bInterfaceClass", interface.path));
        entry(&mut b, "bInterfaceSubClass", tr("Subclass (hex)"));
        entry(&mut b, "bInterfaceProtocol", tr("Protocol (hex)"));
        let driver = get("driver");
        b.field(
            tr("Bound driver"),
            if driver.state == ReadState::Available && driver.value.is_empty() { tr("Unbound") } else { read(&driver) },
            format!("{}/driver", interface.path),
        );
    }
    Some(b.finish())
}

// ---------------------------------------------------------------------------------------
// PCIe
// ---------------------------------------------------------------------------------------

pub fn pcie_applicable(snapshot: &DeviceProperties) -> bool {
    snapshot.device.subsystem == "pci" && snapshot.pcie_applicable
}

fn pcie_link_value(snapshot: &DeviceProperties, key: &str, speed: bool) -> String {
    let value = snapshot.values.get(&format!("sysfs/{key}")).cloned().unwrap_or_default();
    if value.state == ReadState::Unsupported {
        return tr("Query unsupported by the kernel/device");
    }
    if value.state != ReadState::Available {
        return read(&value);
    }
    let raw = value.value.clone();
    let malformed = || arg(tr("Malformed reported value: %1"), &if raw.is_empty() { tr("(empty)") } else { raw.clone() });
    if raw.is_empty() {
        return malformed();
    }
    if speed {
        if raw.eq_ignore_ascii_case("Unknown speed") || raw.eq_ignore_ascii_case("unknown") {
            return arg(tr("Unavailable (%1)"), &raw);
        }
        // Parse the kernel's C-locale rate, including its usual .0 suffix. Unfamiliar formats
        // and future rates remain visible verbatim.
        let rate_text = raw.strip_suffix(" PCIe").unwrap_or(&raw);
        let Some(rate_text) = rate_text.strip_suffix(" GT/s") else { return raw };
        let Some(rate) = rate_text.parse::<f64>().ok().filter(|r| r.is_finite() && *r > 0.0) else { return malformed() };
        for (i, known) in [2.5, 5.0, 8.0, 16.0, 32.0, 64.0, 128.0].iter().enumerate() {
            if rate == *known {
                return arg2(tr("%1 (PCIe Gen %2)"), &raw, &(i + 1).to_string());
            }
        }
        return raw;
    }
    let Some(lanes) = raw.parse::<u64>().ok().filter(|_| !raw.starts_with('-')) else { return malformed() };
    // max_link_width exposes PCIE_LNK_WIDTH_UNKNOWN (0xff) verbatim: a kernel sentinel, not a
    // 255-lane capability. Actual PCIe link width fields are six bits wide.
    if key == "max_link_width" && lanes == 255 {
        return arg(tr("Unavailable (kernel reports unknown width: %1)"), &raw);
    }
    if lanes > 63 {
        return malformed();
    }
    if lanes == 0 {
        return arg(tr("Unavailable (reported width: %1)"), &raw);
    }
    arg2(tr("x%1 (%2 lanes)"), &lanes.to_string(), &format_integer(lanes))
}

pub fn pcie_report(snapshot: &DeviceProperties) -> Option<Report> {
    if !pcie_applicable(snapshot) {
        return None;
    }
    let mut b = Builder::default();
    let kind = &snapshot.pcie_type;
    let number = kind.value.parse::<u32>().ok().filter(|_| kind.state == ReadState::Available);
    let mut type_text = read(kind);
    if kind.state == ReadState::Error && kind.error == libc::EPROTO {
        type_text = tr("Malformed PCI capability data");
    } else if kind.state == ReadState::Error && kind.error == libc::EMSGSIZE {
        type_text = tr("Incomplete PCI configuration data");
    }
    if let Some(n) = number {
        type_text = match n {
            0 => tr("PCIe endpoint"),
            1 => tr("Legacy PCIe endpoint"),
            4 => tr("Root port"),
            5 => tr("Upstream switch port"),
            6 => tr("Downstream switch port"),
            7 => tr("PCIe-to-PCI/PCI-X bridge"),
            8 => tr("PCI/PCI-X-to-PCIe bridge"),
            9 => tr("Root Complex Integrated Endpoint"),
            10 => tr("Root Complex Event Collector"),
            _ => arg(tr("Unrecognized PCIe device type: %1"), &kind.value),
        };
    }
    b.group(tr("Connection"));
    b.field(tr("Connection type"), type_text, snapshot.pcie_type_source.clone());
    if snapshot.pcie_type_requires_root {
        b.field(tr("Access"), tr("Run the application as root or with sudo to read this device's PCIe connection type."), snapshot.pcie_type_source.clone());
    }
    // Both root-complex types lack conventional PCIe link registers. This identifies connection
    // topology, not GPU integration or performance.
    if matches!(number, Some(9 | 10)) {
        b.field(tr("PCIe link"), tr("Not applicable — integrated into the root complex; no external PCIe link."), snapshot.pcie_type_source.clone());
    } else {
        let source = |key: &str, scope: &str| format!("{}\n{scope}", snapshot.sources.get(&format!("sysfs/{key}")).cloned().unwrap_or_default());
        let current_scope = tr("Current link reported for this device; an on-demand reading, not measured throughput or a fault verdict.");
        b.group(tr("Current link"));
        b.field(tr("Link speed"), pcie_link_value(snapshot, "current_link_speed", true), source("current_link_speed", &current_scope));
        b.field(tr("Link width"), pcie_link_value(snapshot, "current_link_width", false), source("current_link_width", &current_scope));
        let capability_scope =
            tr("Maximum capability reported for this device, not the slot or entire upstream path. Speed and width maxima are separate declarations.");
        b.group(tr("Device capability"));
        b.field(tr("Maximum link speed"), pcie_link_value(snapshot, "max_link_speed", true), source("max_link_speed", &capability_scope));
        b.field(tr("Maximum link width"), pcie_link_value(snapshot, "max_link_width", false), source("max_link_width", &capability_scope));
        b.group(tr("Interpretation"));
        b.field(tr("Upstream path"), tr("Not assessed; upstream links may impose additional limits."), "");
        b.field(
            tr("Current vs. maximum"),
            tr("A lower current link speed or width is not automatically a fault. Power management and topology can explain differences."),
            "",
        );
    }
    Some(b.finish())
}

// ---------------------------------------------------------------------------------------
// Battery
// ---------------------------------------------------------------------------------------

pub fn battery_applicable(snapshot: &DeviceProperties) -> bool {
    snapshot.device.subsystem == "power_supply" && snapshot.battery.applicable
}

pub fn battery_report(snapshot: &DeviceProperties) -> Option<Report> {
    if !battery_applicable(snapshot) {
        return None;
    }
    let data = &snapshot.battery;
    let device = &snapshot.device;
    let mut b = Builder::default();
    let source = |key: &str| format!("{}/{key}", device.path);
    let read_value = |value: &Attribute| {
        if value.state == ReadState::Unsupported || value.error == libc::EOPNOTSUPP || value.error == libc::ENOSYS {
            tr("Query unsupported by the kernel/device")
        } else {
            read(value)
        }
    };
    let get = |key: &str| data.values.get(key).cloned().unwrap_or_default();
    let text_value = |key: &str| {
        let value = get(key);
        if value.state != ReadState::Available {
            return read_value(&value);
        }
        if value.value.is_empty() {
            return tr("Not reported");
        }
        if value.value == "Unknown" {
            return tr("Unknown (reported)");
        }
        if key == "status" {
            match value.value.as_str() {
                "Charging" => return tr("Charging"),
                "Discharging" => return tr("Discharging"),
                "Not charging" => return tr("Not charging"),
                "Full" => return tr("Full"),
                _ => {}
            }
        }
        value.value
    };
    // Sysfs numeric battery properties are decimal integers, in micro-units. Signed current and
    // power readings keep their sign, and neither is ever derived from the other.
    let number_value = |key: &str, divisor: f64, unit: &str, signed: bool| {
        let value = get(key);
        if value.state != ReadState::Available {
            return read_value(&value);
        }
        let number = value.value.parse::<i64>().ok().filter(|n| (signed || *n >= 0) && !(key == "capacity" && *n > 100));
        let Some(number) = number else {
            return arg(tr("Malformed reported value: %1"), &if value.value.is_empty() { tr("(empty)") } else { value.value.clone() });
        };
        if number == 0 && (key == "cycle_count" || key.ends_with("_full") || key.ends_with("_full_design")) {
            return tr("Unavailable (reported value: 0)");
        }
        let formatted = if divisor == 1.0 { format_integer_signed(number) } else { format!("{:.3}", number as f64 / divisor) };
        if unit.is_empty() {
            formatted
        } else {
            format!("{formatted} {unit}")
        }
    };
    b.group(tr("Current state"));
    if data.state.state != ReadState::Available {
        let mut state = read_value(&data.state);
        if data.state.error == libc::EAGAIN {
            state = tr("Battery type or presence changed during collection; reload properties.");
        } else if data.state.error == libc::EINVAL && data.values.contains_key("present") {
            state = arg(tr("Malformed battery presence value: %1"), &get("present").value);
        }
        b.field(tr("Collection status"), state, format!("{}\n{}", source("type"), source("present")));
        if data.values.contains_key("type") {
            b.field(tr("Supply type"), text_value("type"), source("type"));
        }
    } else {
        let present = get("present");
        let presence = if present.state == ReadState::Available {
            if present.value == "1" {
                tr("Present")
            } else {
                tr("Absent")
            }
        } else {
            read_value(&present)
        };
        b.field(tr("Battery presence"), presence, source("present"));
        b.field(tr("Status"), text_value("status"), source("status"));
        b.field(tr("Charge percentage"), number_value("capacity", 1.0, "%", false), source("capacity"));
        b.field(tr("Voltage"), number_value("voltage_now", 1_000_000.0, "V", false), source("voltage_now"));
        b.field(
            tr("Current"),
            number_value("current_now", 1_000_000.0, "A", true),
            format!(
                "{}\n{}",
                source("current_now"),
                tr("Driver-reported instantaneous current; the reported sign is preserved. Charging state is reported separately.")
            ),
        );
        b.field(
            tr("Power"),
            number_value("power_now", 1_000_000.0, "W", true),
            format!("{}\n{}", source("power_now"), tr("Driver-reported instantaneous power; not calculated from voltage and current.")),
        );
        let retention = |prefix: &str| {
            let full = get(&format!("{prefix}_full"));
            let design = get(&format!("{prefix}_full_design"));
            if full.state == ReadState::NotApplicable || design.state == ReadState::NotApplicable {
                return read_value(&attribute(ReadState::NotApplicable, 0));
            }
            if full.state != ReadState::Available || design.state != ReadState::Available {
                return tr("Unavailable — requires compatible full and design readings");
            }
            let (Some(full), Some(design)) = (full.value.parse::<i64>().ok().filter(|v| *v > 0), design.value.parse::<i64>().ok().filter(|v| *v > 0)) else {
                return tr("Unavailable — requires positive full and design readings");
            };
            arg(tr("%1 % (estimate)"), &format!("{:.1}", 100.0 * full as f64 / design as f64))
        };
        let has_capacity = |prefix: &str| {
            ["_full_design", "_full", "_now"].iter().any(|suffix| {
                let value = get(&format!("{prefix}{suffix}"));
                value.state != ReadState::Unavailable || value.error != libc::ENOENT
            })
        };
        let (energy, charge) = (has_capacity("energy"), has_capacity("charge"));
        for (present, prefix, title, divisor, unit) in
            [(energy, "energy", tr("Energy capacity"), 1_000_000.0, "Wh"), (charge, "charge", tr("Charge capacity"), 1000.0, "mAh")]
        {
            if !present {
                continue;
            }
            b.group(title);
            b.field(tr("Design capacity"), number_value(&format!("{prefix}_full_design"), divisor, unit, false), source(&format!("{prefix}_full_design")));
            b.field(
                tr("Last full-charge capacity"),
                number_value(&format!("{prefix}_full"), divisor, unit, false),
                format!(
                    "{}\n{}",
                    source(&format!("{prefix}_full")),
                    tr("Firmware-reported learned full capacity; it can depend on temperature, age and calibration.")
                ),
            );
            b.field(tr("Remaining capacity"), number_value(&format!("{prefix}_now"), divisor, unit, false), source(&format!("{prefix}_now")));
            b.field(
                tr("Estimated capacity retention"),
                retention(prefix),
                format!(
                    "{}\n{}\n{}",
                    source(&format!("{prefix}_full")),
                    source(&format!("{prefix}_full_design")),
                    tr("100 × full / design using matching units; not a complete battery health verdict. Values above 100% are not clamped.")
                ),
            );
        }
        if !energy && !charge {
            b.group(tr("Capacity"));
            b.field(tr("Reported capacities"), tr("Unavailable — no energy or charge capacity attributes exposed"), device.path.clone());
            b.field(tr("Estimated capacity retention"), tr("Unavailable — requires compatible full and design readings"), device.path.clone());
        }
        b.group(tr("Battery information"));
        b.field(
            tr("Cycle count"),
            number_value("cycle_count", 1.0, "", false),
            format!("{}\n{}", source("cycle_count"), tr("Reported full charge/discharge cycles. The kernel ABI defines zero as unavailable.")),
        );
        b.field(tr("Chemistry"), text_value("technology"), source("technology"));
        b.field(tr("Manufacturer"), text_value("manufacturer"), source("manufacturer"));
        b.field(tr("Model"), text_value("model_name"), source("model_name"));
        b.field(tr("Serial number"), text_value("serial_number"), source("serial_number"));
    }
    Some(b.finish())
}

fn format_integer_signed(n: i64) -> String {
    if n < 0 {
        format!("-{}", format_integer(n.unsigned_abs()))
    } else {
        format_integer(n as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera::CameraProperties;
    use crate::display::DisplayProperties;
    use crate::ethernet::EthernetProperties;
    use crate::monitor_edid::MonitorProperties;
    use crate::network::{NetworkAddress, NetworkProperties};
    use crate::properties::{BatteryProperties, UsbInterfaceProperties};
    use serde_json::json;

    fn available(value: &str) -> Attribute {
        Attribute { state: ReadState::Available, value: value.into(), error: 0 }
    }
    fn field<'a>(report: &'a Report, caption: &str) -> &'a ReportField {
        report.groups.iter().flat_map(|g| &g.fields).find(|f| f.caption == caption).unwrap_or_else(|| panic!("missing field {caption}"))
    }
    fn snapshot(subsystem: &str) -> DeviceProperties {
        DeviceProperties {
            device: Device { subsystem: subsystem.into(), path: "/sys/devices/x".into(), sysname: "x-1".into(), ..Default::default() },
            state: ReadState::Available,
            ..Default::default()
        }
    }

    #[test]
    fn ethernet_links_are_worded_from_raw_driver_values() {
        let mut s = snapshot("net");
        let ethernet = EthernetProperties {
            applicable: true,
            scope: available(""),
            carrier: available("1"),
            speed: available("2500"),
            duplex: available("1"),
            autoneg: available("1"),
            supported_modes: available("10baseT/Half\n1000baseT/Full"),
            partner_modes: available("1000baseT/Full"),
            source: "ethtool".into(),
        };
        s.network.push(NetworkProperties {
            name: "eno1".into(),
            path: "/sys/devices/x/net/eno1".into(),
            link_type: available("1"),
            permanent_address: available("aa:bb:cc:dd:ee:01"),
            current_address: available("aa:bb:cc:dd:ee:02"),
            assignment_type: available("3"),
            mtu: available("1500"),
            addresses: vec![NetworkAddress { family: "IPv4", address: "192.168.1.5".into(), netmask: available("255.255.255.0") }],
            address_state: available(""),
            ethernet,
        });
        let report = network_report(&s).unwrap();
        assert_eq!(field(&report, "Link status").value, "Connected");
        assert_eq!(field(&report, "Link speed").value, "2.5 Gbps");
        assert_eq!(field(&report, "Duplex").value, "Full duplex");
        assert_eq!(field(&report, "Autonegotiation").value, "Enabled");
        assert_eq!(field(&report, "Supported modes / capabilities").value, "10baseT/Half, 1000baseT/Full");
        assert_eq!(field(&report, "Overridden MAC address").value, "aa:bb:cc:dd:ee:02", "current differs from the permanent address");
        assert_eq!(field(&report, "IPv4 address").value, "192.168.1.5/255.255.255.0");
        assert!(report.copy_text.starts_with("eno1 — Ethernet link\nLink status: Connected"));
        assert!(report.copy_text.contains("\n\neno1 — Link\n") && report.copy_text.contains("\n\neno1 — IP addresses\n"));

        // Unknown driver values stay explicit; a disconnected link makes speed inapplicable.
        s.network[0].ethernet.speed = available("4294967295");
        s.network[0].ethernet.duplex = available("255");
        s.network[0].ethernet.autoneg = available("7");
        assert_eq!(field(&network_report(&s).unwrap(), "Link speed").value, "Unavailable (driver reports unknown speed)");
        assert_eq!(field(&network_report(&s).unwrap(), "Duplex").value, "Unavailable (driver reports unknown duplex)");
        assert_eq!(field(&network_report(&s).unwrap(), "Autonegotiation").value, "Unrecognized autonegotiation value: 7");
        s.network[0].ethernet.carrier = available("0");
        s.network[0].ethernet.speed = attribute(ReadState::NotApplicable, 0);
        s.network[0].permanent_address = Attribute::default();
        s.network[0].assignment_type = available("0");
        let report = network_report(&s).unwrap();
        assert_eq!(field(&report, "Link status").value, "Disconnected");
        assert_eq!(field(&report, "Link speed").value, "Not applicable — link disconnected");
        assert_eq!(field(&report, "Overridden MAC address").value, "Unavailable; permanent MAC could not be determined");
        s.network.clear();
        s.network_state = available("");
        assert_eq!(field(&network_report(&s).unwrap(), "Interfaces").value, "No network interfaces observed");
        assert!(network_report(&snapshot("usb")).is_none());
        assert_eq!(speed_text(100), "100 Mbps");
    }

    #[test]
    fn camera_modes_keep_format_size_and_rate_together() {
        let mut s = snapshot("video4linux");
        s.camera = CameraProperties {
            applicable: true,
            state: available(""),
            node: "/dev/video0".into(),
            report: json!({
                "card": "USB2.0 HD UVC WebCam", "driver": "uvcvideo", "bus": "usb-0000:08:00.4-3", "error": 0,
                "modes": [
                    {"queue": "single", "format": 1196444237u32, "description": "Motion-JPEG", "flags": 1, "size_kind": "discrete",
                     "width": 1280, "height": 720, "intervals": {"kind": "discrete", "values": [[1, 30], [1, 15]]}},
                    {"queue": "single", "format": 1448695129u32, "description": "YUYV 4:2:2", "flags": 0, "size_kind": "stepwise",
                     "width": 160, "height": 120, "max_width": 640, "max_height": 480, "step_width": 16, "step_height": 8},
                ],
            }),
        };
        let report = camera_report(&s).unwrap();
        assert_eq!(field(&report, "Device name").value, "USB2.0 HD UVC WebCam");
        assert_eq!(field(&report, "Query status").value, "Driver-reported snapshot");
        let table = &report.tables[0];
        assert_eq!(table.rows[0], ["Single plane", "MJPEG", "1280 × 720", "30 fps (1/30 s)\n15 fps (1/15 s)"]);
        assert_eq!(table.rows[1][1], "YUYV 4:2:2");
        assert_eq!(table.rows[1][2], "160–640 × 120–480 pixels (steps: 16 × 8 pixels)");
        assert_eq!(table.rows[1][3], "Not enumerated: intervals depend on the exact size");
        assert!(
            report.copy_text.contains("Mode 1\nCapture queue: Single plane\nPixel format: MJPG (0x47504a4d) — Motion-JPEG (compressed)"),
            "{}",
            report.copy_text
        );
        assert_eq!(fourcc(1196444237), "MJPG");
        assert_eq!(fourcc(0x0000_0001), "\\x01\\x00\\x00\\x00");

        s.camera.state = attribute(ReadState::Error, libc::ETIMEDOUT);
        assert_eq!(field(&camera_report(&s).unwrap(), "Query status").value, "Incomplete enumeration: Query timed out");
        assert!(camera_report(&snapshot("usb")).is_none());
    }

    #[test]
    fn display_reports_hardware_and_session_state_separately() {
        let mut s = snapshot("drm");
        s.device.sysname = "card1-eDP-1".into();
        s.monitor = MonitorProperties { applicable: true, descriptor: available(""), ..Default::default() };
        for (k, v) in [
            ("version", "1.4"),
            ("manufacturer", "Dell Inc."),
            ("manufacturer_code", "DEL"),
            ("model", "U2720Q"),
            ("product", "0xa0b1"),
            ("serial", "S1"),
            ("size", "60 34"),
            ("digital", "1"),
            ("interface", "DisplayPort"),
            ("depth", "10"),
            ("modes", "3840x2160\n1920x1080"),
        ] {
            s.monitor.values.insert(k.into(), available(v));
        }
        s.display = DisplayProperties { backend: "Wayland".into(), output: "eDP-1".into(), status: available("exposed"), ..Default::default() };
        for (k, v) in [("resolution", "2880x1800"), ("refresh", "120"), ("rotation", "90"), ("reflection", "none"), ("scale", "2 2"), ("scale_kind", "derived")]
        {
            s.display.values.insert(k.into(), available(v));
        }
        let report = display_report(&s).unwrap();
        assert_eq!(field(&report, "Descriptor").value, "Decoded by libdisplay-info");
        assert_eq!(field(&report, "Image dimensions").value, "60 × 34 cm");
        assert_eq!(field(&report, "Declared color depth").value, "10 bits per color component");
        assert_eq!(field(&report, "Resolution").value, "2880×1800");
        assert_eq!(field(&report, "Refresh rate").value, "120.00 Hz");
        assert_eq!(field(&report, "Rotation").value, "90° counterclockwise");
        assert_eq!(field(&report, "Output scale").value, "2× (derived)");
        assert_eq!(field(&report, "Desktop color depth").value, "Unavailable", "Wayland exposes no desktop depth");
        s.display.values.insert("scale".into(), available("2 1.5"));
        assert_eq!(field(&display_report(&s).unwrap(), "Output scale").value, "X: 2×, Y: 1.5× (derived)");
        s.display = DisplayProperties { status: available("disconnected"), ..Default::default() };
        s.display.sources.insert("configuration".into(), "/sys/devices/x/status".into());
        assert_eq!(field(&display_report(&s).unwrap(), "Configuration").value, "Disconnected");
        assert!(display_report(&snapshot("usb")).is_none());
    }

    #[test]
    fn usb_power_class_and_speed_are_decoded_conservatively() {
        let mut s = snapshot("usb");
        s.device.devtype = "usb_device".into();
        s.values.insert("sysfs/version".into(), available(" 2.00"));
        s.values.insert("sysfs/speed".into(), available("480"));
        for (k, v) in [
            ("bmAttributes", "e0"),
            ("bMaxPower", "500mA"),
            ("bConfigurationValue", "1"),
            ("bNumConfigurations", "1"),
            ("configuration", ""),
            ("bNumInterfaces", "1"),
        ] {
            s.usb.values.insert(k.into(), available(v));
        }
        s.usb.interface_state = available("");
        let mut interface = UsbInterfaceProperties { name: "1-2:1.0".into(), path: "/sys/devices/x/1-2:1.0".into(), ..Default::default() };
        for (k, v) in [
            ("bInterfaceNumber", "00"),
            ("bAlternateSetting", "0"),
            ("bInterfaceClass", "03"),
            ("bInterfaceSubClass", "01"),
            ("bInterfaceProtocol", "01"),
            ("interface", ""),
            ("driver", "usbhid"),
        ] {
            interface.values.insert(k.into(), available(v));
        }
        s.usb.interfaces.push(interface);
        let report = usb_report(&s).unwrap();
        assert_eq!(field(&report, "Connection speed").value, "480 Mbps");
        assert_eq!(field(&report, "Power source").value, "Self-powered");
        assert_eq!(field(&report, "Remote wakeup capability").value, "Supported");
        assert_eq!(field(&report, "Description").value, "Not provided");
        assert_eq!(field(&report, "Function class").value, "Human interface device (0x03)");
        assert_eq!(field(&report, "Bound driver").value, "usbhid");
        s.usb.values.insert("bmAttributes".into(), available("20"));
        assert_eq!(field(&usb_report(&s).unwrap(), "Power source").value, "Invalid data or query (errno 22)", "bit 7 must be set");
        s.values.insert("sysfs/speed".into(), available("unknown"));
        assert_eq!(field(&usb_report(&s).unwrap(), "Connection speed").value, "Unavailable");
        s.values.insert("sysfs/speed".into(), available("20000"));
        assert_eq!(field(&usb_report(&s).unwrap(), "Connection speed").value, "20 Gbps");
        assert_eq!(usb_class_text(&available("ff")), "Vendor-specific (0xff)");
        assert_eq!(usb_class_text(&available("42")), "0x42", "unfamiliar classes keep their code only");
        s.usb.interfaces.clear();
        assert_eq!(field(&usb_report(&s).unwrap(), "Interfaces").value, "None observed");
    }

    #[test]
    fn pcie_links_name_generations_and_sentinels() {
        let mut s = snapshot("pci");
        s.pcie_applicable = true;
        s.pcie_type = available("4");
        s.pcie_type_source = "/sys/devices/x/config".into();
        for (k, v) in [("current_link_speed", "16.0 GT/s PCIe"), ("current_link_width", "8"), ("max_link_speed", "32.0 GT/s PCIe"), ("max_link_width", "255")] {
            s.values.insert(format!("sysfs/{k}"), available(v));
        }
        let report = pcie_report(&s).unwrap();
        assert_eq!(field(&report, "Connection type").value, "Root port");
        assert_eq!(field(&report, "Link speed").value, "16.0 GT/s PCIe (PCIe Gen 4)");
        assert_eq!(field(&report, "Link width").value, "x8 (8 lanes)");
        assert_eq!(field(&report, "Maximum link speed").value, "32.0 GT/s PCIe (PCIe Gen 5)");
        assert_eq!(field(&report, "Maximum link width").value, "Unavailable (kernel reports unknown width: 255)");
        assert!(report.groups.iter().any(|g| g.title == "Interpretation"));
        s.values.insert("sysfs/current_link_speed".into(), available("Unknown speed"));
        assert_eq!(field(&pcie_report(&s).unwrap(), "Link speed").value, "Unavailable (Unknown speed)");
        s.values.insert("sysfs/current_link_speed".into(), available("3.5 GT/s"));
        assert_eq!(field(&pcie_report(&s).unwrap(), "Link speed").value, "3.5 GT/s", "unfamiliar rates stay verbatim");
        s.values.insert("sysfs/current_link_width".into(), available("-1"));
        assert!(field(&pcie_report(&s).unwrap(), "Link width").value.starts_with("Malformed reported value"));
        s.pcie_type = available("9");
        let integrated = pcie_report(&s).unwrap();
        assert!(field(&integrated, "PCIe link").value.starts_with("Not applicable — integrated"));
        assert!(integrated.groups.iter().all(|g| g.title != "Current link"));
        s.pcie_type = attribute(ReadState::PermissionDenied, libc::EACCES);
        s.pcie_type_requires_root = true;
        assert!(field(&pcie_report(&s).unwrap(), "Access").value.contains("Run the application as root"));
        s.pcie_applicable = false;
        assert!(pcie_report(&s).is_none());
    }

    #[test]
    fn battery_values_keep_units_signs_and_estimates_honest() {
        let mut s = snapshot("power_supply");
        s.battery = BatteryProperties { applicable: true, state: available(""), ..Default::default() };
        for (k, v) in [
            ("type", "Battery"),
            ("present", "1"),
            ("status", "Discharging"),
            ("capacity", "87"),
            ("voltage_now", "12345000"),
            ("current_now", "-1500000"),
            ("power_now", "18500000"),
            ("energy_full_design", "56000000"),
            ("energy_full", "49000000"),
            ("energy_now", "42630000"),
            ("cycle_count", "212"),
            ("technology", "Li-ion"),
            ("manufacturer", "ASUSTeK"),
            ("model_name", "A32-K55"),
            ("serial_number", "Unknown"),
        ] {
            s.battery.values.insert(k.into(), available(v));
        }
        // Real collection records absent attributes as ENOENT, which means "not exposed".
        for key in ["charge_full_design", "charge_full", "charge_now"] {
            s.battery.values.insert(key.into(), attribute(ReadState::Unavailable, libc::ENOENT));
        }
        let report = battery_report(&s).unwrap();
        assert_eq!(field(&report, "Charge percentage").value, "87 %");
        assert_eq!(field(&report, "Voltage").value, "12.345 V");
        assert_eq!(field(&report, "Current").value, "-1.500 A", "sign preserved");
        assert_eq!(field(&report, "Power").value, "18.500 W");
        assert_eq!(field(&report, "Design capacity").value, "56.000 Wh");
        assert_eq!(field(&report, "Estimated capacity retention").value, "87.5 % (estimate)");
        assert_eq!(field(&report, "Cycle count").value, "212");
        assert_eq!(field(&report, "Serial number").value, "Unknown (reported)");
        assert!(report.groups.iter().all(|g| g.title != "Charge capacity"), "no charge attributes were exposed");
        s.battery.values.insert("energy_full".into(), available("0"));
        let report = battery_report(&s).unwrap();
        assert_eq!(field(&report, "Last full-charge capacity").value, "Unavailable (reported value: 0)");
        assert_eq!(field(&report, "Estimated capacity retention").value, "Unavailable — requires positive full and design readings");
        s.battery.values.insert("capacity".into(), available("101"));
        assert!(field(&battery_report(&s).unwrap(), "Charge percentage").value.starts_with("Malformed reported value"));
        s.battery.values.clear();
        s.battery.state = attribute(ReadState::Error, libc::EAGAIN);
        assert!(field(&battery_report(&s).unwrap(), "Collection status").value.starts_with("Battery type or presence changed"));
        s.battery.applicable = false;
        assert!(battery_report(&s).is_none());
    }
}
