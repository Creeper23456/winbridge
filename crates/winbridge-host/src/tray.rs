// SPDX-License-Identifier: GPL-3.0-or-later

//! StatusNotifierItem tray icon: show/hide the viewer window and shut the VM down.
//!
//! ksni runs on the Tokio runtime, so menu callbacks hop to the GTK main thread through
//! `glib::idle_add_once` before touching the viewer.

use crate::with_viewer;
use gtk4::glib;
use ksni::menu::StandardItem;
use ksni::{MenuItem, TrayMethods};
use tracing::{info, warn};

pub struct VmTray {
    pub status: &'static str,
}

impl ksni::Tray for VmTray {
    fn id(&self) -> String {
        "org.winbridge.viewer".into()
    }

    fn title(&self) -> String {
        "WinBridge".into()
    }

    fn icon_name(&self) -> String {
        "computer".into()
    }

    /// Left click toggles the window.
    fn activate(&mut self, _x: i32, _y: i32) {
        glib::idle_add_once(|| with_viewer(|v| v.tray_toggle()));
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            StandardItem {
                label: self.status.into(),
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Show window".into(),
                activate: Box::new(|_| {
                    glib::idle_add_once(|| with_viewer(|v| v.tray_show()));
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Shut down VM".into(),
                icon_name: "system-shutdown".into(),
                activate: Box::new(|_| {
                    glib::idle_add_once(|| with_viewer(|v| v.tray_shutdown()));
                }),
                ..Default::default()
            }
            .into(),
        ]
    }

    /// A hidden window would be unreachable without the tray, so bring it back.
    fn watcher_offline(&self, _reason: ksni::OfflineReason) -> bool {
        warn!("StatusNotifierWatcher went away; showing the window");
        glib::idle_add_once(|| with_viewer(|v| v.tray_show()));
        true
    }
}

/// Registers the tray. Without a StatusNotifierWatcher on the session bus `spawn` fails and the
/// viewer keeps no tray handle, so "Minimize to tray" only minimizes.
pub fn start() {
    tokio::spawn(async {
        match (VmTray { status: "VM running" }).spawn().await {
            Ok(handle) => {
                info!("Tray icon registered");
                glib::idle_add_once(move || with_viewer(|v| v.set_tray(handle)));
            }
            Err(e) => warn!("No tray icon ({}); Minimize to tray will only minimize", e),
        }
    });
}
