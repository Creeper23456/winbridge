// SPDX-License-Identifier: GPL-3.0-or-later

//! CD/DVD media and host USB passthrough controls backed by QEMU QMP.

use crate::qmp::{QmpClient, QmpError};
use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Box as GtkBox, ApplicationWindow, Button, CheckButton, FileDialog, FileFilter, Label,
    MenuButton, Orientation, Popover, Separator,
};
use serde_json::json;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

const REFRESH_INTERVAL: Duration = Duration::from_millis(1500);

/// USB interface class HID, with boot-protocol keyboard (01) and mouse (02).
const HID_CLASS: u16 = 0x03;
const HID_KEYBOARD_PROTOCOL: u16 = 0x01;
const HID_MOUSE_PROTOCOL: u16 = 0x02;

/// USB device class of hubs, including root hubs.
const HUB_CLASS: u16 = 0x09;

#[derive(Clone)]
struct UsbDevice {
    sysname: String,
    bus: u16,
    address: u16,
    vendor: u16,
    product: u16,
    manufacturer: String,
    name: String,
}

impl UsbDevice {
    fn signature(&self) -> u32 {
        ((self.vendor as u32) << 16) | self.product as u32
    }

    fn qmp_id(&self) -> String {
        format!("wbusb-{}-{}", self.bus, self.address)
    }

    fn description(&self) -> String {
        let names = [self.manufacturer.as_str(), self.name.as_str()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let names = if names.is_empty() {
            "USB device".to_string()
        } else {
            names
        };
        format!(
            "{} [{:04x}:{:04x}] — bus {}, address {}",
            names, self.vendor, self.product, self.bus, self.address
        )
    }
}

#[derive(Default)]
struct DeviceState {
    /// Eligible devices seen by the last enumeration, keyed by sysname.
    current: BTreeMap<String, UsbDevice>,
    /// Devices passed through, keyed by sysname.
    attached: HashMap<String, UsbDevice>,
    /// Sysnames with a `device_add` in flight.
    adding: HashSet<String>,
    /// QMP ids with a `device_del` in flight.
    deleting: HashSet<String>,
    /// Signatures (vendor:product) the user has checked; reattached after reconnection.
    wanted: HashSet<u32>,
}

struct UsbRow {
    check: CheckButton,
    description: String,
    _handler: glib::SignalHandlerId,
}

struct Devices {
    window: ApplicationWindow,
    qmp: QmpClient,
    usb_bus: Option<String>,
    medium: Label,
    error: Label,
    usb_list: GtkBox,
    empty_usb: Label,
    drives_box: GtkBox,
    drives: RefCell<Vec<PathBuf>>,
    rows: RefCell<HashMap<String, UsbRow>>,
    state: RefCell<DeviceState>,
}

/// Creates the header-bar device menu and starts host hotplug monitoring.
/// `usb_bus` names the QEMU USB controller for passthrough; `None` lets QEMU pick the default bus.
pub fn devices_button(
    window: &ApplicationWindow,
    qmp_socket: PathBuf,
    usb_bus: Option<String>,
) -> MenuButton {
    let (button, devices) = Devices::new(window, qmp_socket, usb_bus);
    // The refresh timer holds the only other reference, so the controller lives with the window.
    drop(devices);
    button
}

impl Devices {
    fn new(window: &ApplicationWindow, qmp_socket: PathBuf, usb_bus: Option<String>) -> (MenuButton, Rc<Self>) {
        let content = GtkBox::new(Orientation::Vertical, 8);
        content.set_margin_top(12);
        content.set_margin_bottom(12);
        content.set_margin_start(12);
        content.set_margin_end(12);
        content.set_width_request(340);

        let cd_title = Label::new(Some("CD/DVD image"));
        cd_title.set_xalign(0.0);
        cd_title.add_css_class("heading");
        content.append(&cd_title);
        let medium = Label::new(Some("Current medium: checking…"));
        medium.set_xalign(0.0);
        medium.set_wrap(true);
        medium.set_width_chars(42);
        content.append(&medium);

        let insert = Button::with_label("Insert ISO…");
        content.append(&insert);
        let drives_box = GtkBox::new(Orientation::Vertical, 6);
        content.append(&drives_box);
        let eject = Button::with_label("Eject");
        content.append(&eject);
        content.append(&Separator::new(Orientation::Horizontal));

        let usb_title = Label::new(Some("USB passthrough"));
        usb_title.set_xalign(0.0);
        usb_title.add_css_class("heading");
        content.append(&usb_title);
        let usb_hint = Label::new(Some(
            "Checked devices are detached from the host and attached to Windows.",
        ));
        usb_hint.set_xalign(0.0);
        usb_hint.set_wrap(true);
        usb_hint.set_width_chars(42);
        content.append(&usb_hint);
        let usb_list = GtkBox::new(Orientation::Vertical, 4);
        content.append(&usb_list);
        let empty_usb = Label::new(Some("No eligible host USB devices found"));
        empty_usb.set_xalign(0.0);
        content.append(&empty_usb);
        let usb_footer = Label::new(Some("Keyboards and mice are never passed through"));
        usb_footer.set_xalign(0.0);
        usb_footer.set_wrap(true);
        usb_footer.set_width_chars(42);
        usb_footer.add_css_class("dim-label");
        content.append(&usb_footer);
        let error = Label::new(None);
        error.set_xalign(0.0);
        error.set_wrap(true);
        error.set_width_chars(42);
        error.add_css_class("error");
        content.append(&error);

        let popover = Popover::new();
        popover.set_child(Some(&content));
        popover.set_position(gtk4::PositionType::Bottom);
        let button = MenuButton::new();
        button.set_icon_name("drive-removable-media-usb-symbolic");
        button.set_tooltip_text(Some("Devices"));
        button.set_popover(Some(&popover));

        let devices = Rc::new(Self {
            window: window.clone(),
            qmp: QmpClient::new(qmp_socket),
            usb_bus,
            medium,
            error,
            usb_list,
            empty_usb,
            drives_box,
            drives: RefCell::new(Vec::new()),
            rows: RefCell::new(HashMap::new()),
            state: RefCell::new(DeviceState::default()),
        });

        insert.connect_clicked({
            let devices = devices.clone();
            move |_| devices.choose_iso()
        });
        eject.connect_clicked({
            let devices = devices.clone();
            move |_| devices.eject()
        });
        devices.refresh();
        devices.query_medium();
        glib::timeout_add_local(REFRESH_INTERVAL, {
            let devices = devices.clone();
            move || {
                devices.refresh();
                glib::ControlFlow::Continue
            }
        });
        (button, devices)
    }

    fn report(&self, message: impl Into<String>) {
        self.error.set_text(&message.into());
    }

    fn choose_iso(self: &Rc<Self>) {
        let filter = FileFilter::new();
        filter.set_name(Some("CD/DVD images (*.iso, *.img)"));
        filter.add_pattern("*.iso");
        filter.add_pattern("*.img");
        let filters = gio::ListStore::new::<FileFilter>();
        filters.append(&filter);
        let dialog = FileDialog::new();
        dialog.set_title("Insert CD/DVD image");
        dialog.set_filters(Some(&filters));
        dialog.set_default_filter(Some(&filter));
        let devices = self.clone();
        dialog.open(Some(&self.window), gio::Cancellable::NONE, move |result| match result {
            Ok(file) => {
                if let Some(path) = file.path() {
                    devices.change_medium(path);
                }
            }
            Err(e) if e.matches(gio::IOErrorEnum::Cancelled) => {}
            Err(e) => devices.report(format!("Cannot open image chooser: {e}")),
        });
    }

    fn change_medium(self: &Rc<Self>, path: PathBuf) {
        self.medium.set_text(&format!("Current medium: {}", path.display()));
        let qmp = self.qmp.clone();
        let devices = self.clone();
        glib::spawn_future_local(async move {
            let result = qmp
                .execute(
                    "blockdev-change-medium",
                    json!({
                        "id": "cdrom0",
                        "filename": path.to_string_lossy(),
                        "format": "raw",
                        "read-only": true,
                    }),
                )
                .await;
            glib::idle_add_local_once(move || match result {
                Err(e) => {
                    devices.report(e);
                    devices.query_medium();
                }
                Ok(_) => devices.report(""),
            });
        });
    }

    fn eject(self: &Rc<Self>) {
        let qmp = self.qmp.clone();
        let devices = self.clone();
        glib::spawn_future_local(async move {
            let result = qmp.execute("eject", json!({"id": "cdrom0", "force": true})).await;
            glib::idle_add_local_once(move || match result {
                Ok(_) => {
                    devices.medium.set_text("Current medium: empty");
                    devices.report("");
                }
                Err(e) => {
                    devices.report(e);
                    devices.query_medium();
                }
            });
        });
    }

    fn query_medium(self: &Rc<Self>) {
        let qmp = self.qmp.clone();
        let devices = self.clone();
        glib::spawn_future_local(async move {
            let result = qmp.execute("query-block", json!({})).await;
            glib::idle_add_local_once(move || match result {
                Ok(value) => {
                    let name = value.as_array().and_then(|blocks| {
                        blocks.iter().find_map(|block| {
                            if block.get("device")?.as_str()? == "cdrom0" {
                                Some(block.pointer("/inserted/image/filename")?.as_str()?.to_owned())
                            } else {
                                None
                            }
                        })
                    });
                    devices.medium.set_text(&match name {
                        Some(name) => format!("Current medium: {name}"),
                        None => "Current medium: empty".to_string(),
                    });
                }
                Err(e) => devices.report(e),
            });
        });
    }

    fn refresh(self: &Rc<Self>) {
        let current = enumerate_usb();
        let optical = optical_drives();
        {
            let mut drives = self.drives.borrow_mut();
            if *drives != optical {
                *drives = optical;
                while let Some(child) = self.drives_box.first_child() {
                    self.drives_box.remove(&child);
                }
                for path in drives.iter() {
                    let button =
                        Button::with_label(&format!("Use host optical drive {}", path.display()));
                    button.connect_clicked({
                        let devices = self.clone();
                        let path = path.clone();
                        move |_| devices.change_medium(path.clone())
                    });
                    self.drives_box.append(&button);
                }
                self.drives_box.set_visible(!drives.is_empty());
            }
        }

        let removed = {
            let mut state = self.state.borrow_mut();
            let removed = state
                .attached
                .iter()
                .filter(|(sysname, device)| {
                    current.get(*sysname).is_none_or(|new| new.qmp_id() != device.qmp_id())
                })
                .map(|(_, device)| device.qmp_id())
                .collect::<Vec<_>>();
            state.attached.retain(|sysname, device| {
                current
                    .get(sysname)
                    .is_some_and(|new| new.qmp_id() == device.qmp_id())
            });
            state.current = current;
            removed
        };
        self.render_usb();
        for id in removed {
            self.remove_qmp_usb(id);
        }
        self.reattach_wanted();
    }

    /// Brings the checkbox rows in line with `state.current`. Rows are rebuilt only when the
    /// description changes; checked state follows the user's wanted set.
    fn render_usb(self: &Rc<Self>) {
        let devices = self.state.borrow().current.clone();
        let mut rows = self.rows.borrow_mut();
        let stale = rows
            .keys()
            .filter(|sysname| !devices.contains_key(*sysname))
            .cloned()
            .collect::<Vec<_>>();
        for sysname in stale {
            if let Some(row) = rows.remove(&sysname) {
                self.usb_list.remove(&row.check);
            }
        }
        for (sysname, device) in devices {
            let description = device.description();
            if rows
                .get(&sysname)
                .is_some_and(|row| row.description == description)
            {
                continue;
            }
            if let Some(row) = rows.remove(&sysname) {
                self.usb_list.remove(&row.check);
            }
            let label = Label::new(Some(&description));
            label.set_xalign(0.0);
            label.set_max_width_chars(48);
            label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            let check = CheckButton::new();
            check.set_child(Some(&label));
            check.set_tooltip_text(Some(&description));
            let active = self.state.borrow().wanted.contains(&device.signature());
            check.set_active(active);
            let handler = check.connect_toggled({
                let devices = self.clone();
                let device = device.clone();
                move |check| {
                    if check.is_active() {
                        devices.attach_usb(device.clone());
                    } else {
                        devices.detach_usb(device.clone());
                    }
                }
            });
            self.usb_list.append(&check);
            rows.insert(
                sysname,
                UsbRow {
                    check,
                    description,
                    _handler: handler,
                },
            );
        }
        self.empty_usb.set_visible(rows.is_empty());
    }

    fn attach_usb(self: &Rc<Self>, device: UsbDevice) {
        let mut state = self.state.borrow_mut();
        state.wanted.insert(device.signature());
        if state.attached.contains_key(&device.sysname)
            || state.deleting.contains(&device.qmp_id())
            || state.adding.contains(&device.sysname)
        {
            return;
        }
        state.adding.insert(device.sysname.clone());
        state.attached.insert(device.sysname.clone(), device.clone());
        drop(state);
        self.add_qmp_usb(device);
    }

    fn detach_usb(self: &Rc<Self>, device: UsbDevice) {
        let mut state = self.state.borrow_mut();
        state.wanted.remove(&device.signature());
        state.attached.remove(&device.sysname);
        if state.adding.contains(&device.sysname) || !state.deleting.insert(device.qmp_id()) {
            return;
        }
        drop(state);
        self.remove_qmp_usb(device.qmp_id());
    }

    fn remove_qmp_usb(self: &Rc<Self>, id: String) {
        {
            let mut state = self.state.borrow_mut();
            if !state.deleting.insert(id.clone()) {
                return;
            }
        }
        let qmp = self.qmp.clone();
        let devices = self.clone();
        glib::spawn_future_local(async move {
            let result = qmp.execute("device_del", json!({ "id": id })).await;
            glib::idle_add_local_once(move || {
                // This operation owns the `deleting` entry it inserted above.
                devices.state.borrow_mut().deleting.remove(&id);
                match result {
                    Ok(_) => devices.report(""),
                    Err(e) => devices.report(e),
                }
            });
        });
    }

    /// Attaches devices the user wants but that are not attached yet, such as after QEMU
    /// restarts or after a device re-enumerates.
    fn reattach_wanted(self: &Rc<Self>) {
        let attach = {
            let mut state = self.state.borrow_mut();
            let attach = state
                .current
                .values()
                .filter(|device| {
                    state.wanted.contains(&device.signature())
                        && !state.attached.contains_key(&device.sysname)
                        && !state.adding.contains(&device.sysname)
                        && !state.deleting.contains(&device.qmp_id())
                })
                .cloned()
                .collect::<Vec<_>>();
            for device in &attach {
                state.adding.insert(device.sysname.clone());
                state.attached.insert(device.sysname.clone(), device.clone());
            }
            attach
        };
        for device in attach {
            self.add_qmp_usb(device);
        }
    }

    fn add_qmp_usb(self: &Rc<Self>, device: UsbDevice) {
        // Re-checked against sysfs right before attaching: a device that re-enumerated as a
        // keyboard or mouse since the last refresh must never reach QEMU.
        if input_device_sysnames().contains(&device.sysname) {
            self.refuse_input_device(device);
            return;
        }
        let mut arguments = json!({
            "driver": "usb-host",
            "hostbus": device.bus,
            "hostaddr": device.address,
            "id": device.qmp_id(),
        });
        // Named only when configured; otherwise QEMU chooses its default USB bus.
        if let Some(bus) = &self.usb_bus {
            arguments["bus"] = json!(bus);
        }
        let qmp = self.qmp.clone();
        let devices = self.clone();
        glib::spawn_future_local(async move {
            let result = qmp.execute("device_add", arguments).await;
            glib::idle_add_local_once(move || {
                // This operation owns the `adding` entry inserted by attach or reattach.
                let mut state = devices.state.borrow_mut();
                state.adding.remove(&device.sysname);
                let current = state
                    .attached
                    .get(&device.sysname)
                    .is_some_and(|attached| attached.qmp_id() == device.qmp_id());
                if current && result.is_err() {
                    state.attached.remove(&device.sysname);
                }
                drop(state);
                match result {
                    Ok(_) => devices.report(""),
                    Err(e) => devices.report(usb_add_error(e)),
                }
            });
        });
    }

    /// Drops a device that turned out to be a keyboard or mouse: no `device_add` is sent, the
    /// row disappears, and the device stays out of later enumerations.
    fn refuse_input_device(self: &Rc<Self>, device: UsbDevice) {
        {
            let mut state = self.state.borrow_mut();
            state.wanted.remove(&device.signature());
            state.attached.remove(&device.sysname);
            state.adding.remove(&device.sysname);
            state.current.remove(&device.sysname);
        }
        self.render_usb();
        self.report(format!(
            "Keyboards and mice are never passed through: {}",
            device.description()
        ));
    }
}

/// Adds a hint to host permission failures so the user knows which script fixes them.
fn usb_add_error(error: QmpError) -> String {
    let message = error.to_string();
    if message.contains("Permission denied") || message.contains("Could not open") {
        format!("{message}\nRun scripts/install-host-udev.sh to give your user access to USB devices.")
    } else {
        message
    }
}

fn read_trimmed(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|value| value.trim().to_string())
}

fn read_number(path: &Path, radix: u32) -> Option<u16> {
    u16::from_str_radix(&read_trimmed(path)?, radix).ok()
}

/// Sysnames of USB devices with a keyboard or mouse interface. Composite devices count:
/// interface entries are named `<device>:<config>.<interface>`, so the device sysname is the prefix.
fn input_device_sysnames() -> HashSet<String> {
    let mut result = HashSet::new();
    let Ok(entries) = fs::read_dir("/sys/bus/usb/devices") else {
        return result;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some((device, _)) = name.split_once(':') else {
            continue;
        };
        let path = entry.path();
        let is_hid = read_number(&path.join("bInterfaceClass"), 16) == Some(HID_CLASS);
        let is_input = matches!(
            read_number(&path.join("bInterfaceProtocol"), 16),
            Some(HID_KEYBOARD_PROTOCOL | HID_MOUSE_PROTOCOL)
        );
        if is_hid && is_input {
            result.insert(device.to_string());
        }
    }
    result
}

/// Lists USB devices that may be passed through. Hubs and root hubs are excluded, and so are
/// devices with any keyboard or mouse interface; those are never offered.
fn enumerate_usb() -> BTreeMap<String, UsbDevice> {
    let mut result = BTreeMap::new();
    let Ok(entries) = fs::read_dir("/sys/bus/usb/devices") else {
        return result;
    };
    let input_devices = input_device_sysnames();
    for entry in entries.flatten() {
        let sysname = entry.file_name().to_string_lossy().into_owned();
        // Interface entries contain ':'; only device entries carry the fields below.
        if sysname.contains(':') {
            continue;
        }
        let path = entry.path();
        let (Some(bus), Some(address), Some(vendor), Some(product)) = (
            read_number(&path.join("busnum"), 10),
            read_number(&path.join("devnum"), 10),
            read_number(&path.join("idVendor"), 16),
            read_number(&path.join("idProduct"), 16),
        ) else {
            continue;
        };
        if read_number(&path.join("bDeviceClass"), 16) == Some(HUB_CLASS) {
            continue;
        }
        if input_devices.contains(&sysname) {
            continue;
        }
        result.insert(
            sysname.clone(),
            UsbDevice {
                sysname,
                bus,
                address,
                vendor,
                product,
                manufacturer: read_trimmed(&path.join("manufacturer")).unwrap_or_default(),
                name: read_trimmed(&path.join("product")).unwrap_or_default(),
            },
        );
    }
    result
}

fn optical_drives() -> Vec<PathBuf> {
    let mut drives = fs::read_dir("/sys/class/block")
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.strip_prefix("sr")
                .filter(|suffix| !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()))
                .map(|_| PathBuf::from("/dev").join(&name))
                .filter(|path| path.exists())
        })
        .collect::<Vec<_>>();
    drives.sort();
    drives.dedup();
    drives
}
