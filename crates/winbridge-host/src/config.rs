// SPDX-License-Identifier: GPL-3.0-or-later

//! Viewer configuration: `config.toml` in the XDG config directory, overridden by command-line
//! options. Keys absent from both sources take the defaults below.

use serde::Deserialize;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// Default VirtIO-serial channel socket, shared with the example launch script.
pub const DEFAULT_SOCKET: &str = "/tmp/winbridge.sock";
/// Default QEMU Machine Protocol socket.
pub const DEFAULT_QMP_SOCKET: &str = "/tmp/winbridge-qmp.sock";
/// Default QEMU D-Bus display address when the viewer launches QEMU itself. Matches the
/// example launch script's private bus.
pub const DEFAULT_LAUNCH_DBUS_ADDR: &str = "unix:path=/tmp/qemu-dbus.sock";

/// Command-line values. `None` means the option was not given.
#[derive(Debug, Default)]
pub struct CliOverrides {
    pub launch_script: Option<PathBuf>,
    pub socket: Option<PathBuf>,
    pub dbus_addr: Option<String>,
    pub qmp_socket: Option<PathBuf>,
    pub usb_bus: Option<String>,
    pub grab_keyboard: bool,
}

/// Keys of `config.toml`. Unknown keys are rejected so typos do not pass silently.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct FileConfig {
    launch_script: Option<PathBuf>,
    launch_args: Vec<String>,
    socket: Option<PathBuf>,
    dbus_addr: Option<String>,
    qmp_socket: Option<PathBuf>,
    usb_bus: Option<String>,
    grab_keyboard: bool,
}

/// Script the viewer runs as its child to start the VM.
#[derive(Debug, Clone)]
pub struct LaunchSpec {
    pub script: PathBuf,
    pub args: Vec<String>,
}

/// Fully resolved configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub socket: PathBuf,
    /// `None` connects to the session bus.
    pub dbus_addr: Option<String>,
    pub qmp_socket: PathBuf,
    pub usb_bus: Option<String>,
    /// Actively grab the keyboard while the pointer is over the VM canvas.
    pub grab_keyboard: bool,
    /// `None` attaches to a QEMU that is already running.
    pub launch: Option<LaunchSpec>,
}

/// Loads `config.toml` (when present) and applies command-line overrides on top of it.
pub fn load(cli: CliOverrides) -> Result<Config, String> {
    let file = read_file_config()?;

    let launch_script = cli.launch_script.or(file.launch_script);
    if launch_script.is_none() && !file.launch_args.is_empty() {
        return Err("config.toml: launch_args requires launch_script".to_string());
    }
    let launch = match launch_script {
        Some(script) => {
            let script = fs::canonicalize(&script).map_err(|e| {
                format!("launch script {}: {}", script.display(), e)
            })?;
            Some(LaunchSpec {
                script,
                args: file.launch_args,
            })
        }
        None => None,
    };

    // In launch mode the viewer hands QEMU the D-Bus address, so it needs one even when the
    // session bus is the default in attach mode.
    let dbus_addr = cli
        .dbus_addr
        .or(file.dbus_addr)
        .or_else(|| launch.as_ref().map(|_| DEFAULT_LAUNCH_DBUS_ADDR.to_string()));

    Ok(Config {
        socket: cli
            .socket
            .or(file.socket)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET)),
        dbus_addr,
        qmp_socket: cli
            .qmp_socket
            .or(file.qmp_socket)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_QMP_SOCKET)),
        usb_bus: cli.usb_bus.or(file.usb_bus),
        grab_keyboard: cli.grab_keyboard || file.grab_keyboard,
        launch,
    })
}

/// `$XDG_CONFIG_HOME/winbridge/config.toml`, or `~/.config/winbridge/config.toml`.
pub fn config_file_path() -> Option<PathBuf> {
    xdg_dir("XDG_CONFIG_HOME", ".config").map(|dir| dir.join("winbridge/config.toml"))
}

/// `$XDG_STATE_HOME/winbridge/launch.log`, or `~/.local/state/winbridge/launch.log`.
/// Falls back to a path under the working directory when neither variable is usable.
pub fn launch_log_path() -> PathBuf {
    xdg_dir("XDG_STATE_HOME", ".local/state")
        .unwrap_or_else(|| PathBuf::from("."))
        .join("winbridge/launch.log")
}

fn read_file_config() -> Result<FileConfig, String> {
    let Some(path) = config_file_path() else {
        return Ok(FileConfig::default());
    };
    if !path.exists() {
        return Ok(FileConfig::default());
    }
    let text = fs::read_to_string(&path)
        .map_err(|e| format!("reading {}: {}", path.display(), e))?;
    toml::from_str(&text).map_err(|e| format!("{}: {}", path.display(), e))
}

/// Resolves an XDG base directory. Relative values are ignored, as the spec requires.
fn xdg_dir(variable: &str, home_relative: &str) -> Option<PathBuf> {
    let from_env = env::var_os(variable)
        .map(PathBuf::from)
        .filter(|p| Path::new(p).is_absolute());
    from_env.or_else(|| {
        env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .map(|home| home.join(home_relative))
    })
}
