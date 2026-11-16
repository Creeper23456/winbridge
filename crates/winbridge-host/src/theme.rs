// SPDX-License-Identifier: GPL-3.0-or-later

//! Light/dark appearance that follows the desktop.
//!
//! The desktop publishes its preference through the XDG settings portal
//! (`org.freedesktop.appearance` `color-scheme`: 0 no preference, 1 dark, 2 light). GTK reads
//! it by itself on Wayland only, so the viewer reads it here (works on X11 too) and mirrors it
//! into `gtk-application-prefer-dark-theme`. There is deliberately no switch in the UI.
//! Without a portal the GTK defaults stay in force.

use gtk4::gio::{self, prelude::*};
use gtk4::{glib, Settings};
use tracing::{debug, info};

const BUS_NAME: &str = "org.freedesktop.portal.Desktop";
const OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";
const INTERFACE: &str = "org.freedesktop.portal.Settings";
const NAMESPACE: &str = "org.freedesktop.appearance";
const KEY: &str = "color-scheme";
/// `color-scheme` value meaning "prefer dark".
const PREFER_DARK: u32 = 1;

/// Applies the desktop's current colour scheme and keeps following it for the rest of the run.
pub fn follow_desktop() {
    glib::spawn_future_local(async {
        let proxy = match gio::DBusProxy::for_bus_future(
            gio::BusType::Session,
            gio::DBusProxyFlags::NONE,
            None,
            BUS_NAME,
            OBJECT_PATH,
            INTERFACE,
        )
        .await
        {
            Ok(proxy) => proxy,
            Err(e) => {
                debug!("no settings portal, keeping the GTK default appearance: {e}");
                return;
            }
        };

        match proxy.call_future("Read", Some(&(NAMESPACE, KEY).to_variant()), gio::DBusCallFlags::NONE, -1).await {
            Ok(reply) => apply(scheme_of(&reply)),
            Err(e) => debug!("the portal has no {KEY}: {e}"),
        }

        // The handler owns the proxy, which keeps the subscription alive for the whole run.
        let keep = proxy.clone();
        proxy.connect_g_signal(move |_, _sender, signal, params| {
            let _ = &keep;
            if signal != "SettingChanged" {
                return;
            }
            let (Some(namespace), Some(key)) =
                (params.try_child_value(0).and_then(|v| v.str().map(str::to_owned)), params.try_child_value(1).and_then(|v| v.str().map(str::to_owned)))
            else {
                return;
            };
            if namespace == NAMESPACE && key == KEY {
                if let Some(value) = params.try_child_value(2) {
                    apply(scheme_of(&value));
                }
            }
        });
    });
}

/// The `uint32` inside a portal answer, however many `variant` layers wrap it (`Read` wraps it
/// twice, `SettingChanged` once).
fn scheme_of(value: &glib::Variant) -> Option<u32> {
    if value.type_() == glib::VariantTy::VARIANT {
        return value.as_variant().and_then(|inner| scheme_of(&inner));
    }
    if value.is_container() && value.n_children() == 1 {
        return scheme_of(&value.child_value(0));
    }
    value.get::<u32>()
}

fn apply(scheme: Option<u32>) {
    let Some(settings) = Settings::default() else { return };
    let dark = scheme == Some(PREFER_DARK);
    if settings.is_gtk_application_prefer_dark_theme() != dark {
        info!("desktop colour scheme: {}", if dark { "dark" } else { "light" });
        settings.set_gtk_application_prefer_dark_theme(dark);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheme_is_found_under_every_portal_wrapping() {
        let plain = 1u32.to_variant();
        let once = glib::Variant::from_variant(&plain);
        let twice_in_tuple = (glib::Variant::from_variant(&once),).to_variant();
        assert_eq!(scheme_of(&plain), Some(1));
        assert_eq!(scheme_of(&once), Some(1));
        assert_eq!(scheme_of(&twice_in_tuple), Some(1));
        assert_eq!(scheme_of(&"dark".to_variant()), None);
    }
}
