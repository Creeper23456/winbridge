// SPDX-License-Identifier: GPL-3.0-or-later

//! Runs the configured launch script as the viewer's child and reports how it ended.

use crate::config::{launch_log_path, Config, LaunchSpec};
use crate::qmp::QmpClient;
use gtk4::glib;
use serde_json::json;
use std::fs::{self, OpenOptions};
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::sleep;
use tracing::{info, warn};

/// Interval between QMP probes while waiting for QEMU to become reachable.
const REACH_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How the launch script ended, as seen by the viewer.
#[derive(Debug, Clone)]
pub enum LaunchOutcome {
    /// Exit status 0 after QEMU was reachable: the VM has gone and the viewer should quit.
    VmGone,
    /// Exited before QEMU was reachable, or with a failure status. Carries a short reason.
    Failed(String),
}

/// Spawns the launch script once and watches it in the background.
///
/// The script gets its own process group, so the viewer's Ctrl-C handling does not reach it
/// directly. Its stdout and stderr are appended to the launch log.
pub fn start(spec: &LaunchSpec, config: &Config) -> Result<(), String> {
    let log_path = launch_log_path();
    if let Some(dir) = log_path.parent() {
        fs::create_dir_all(dir)
            .map_err(|e| format!("creating {}: {}", dir.display(), e))?;
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("opening {}: {}", log_path.display(), e))?;
    let log_err = log
        .try_clone()
        .map_err(|e| format!("duplicating log handle: {}", e))?;

    let script_dir = spec.script.parent().unwrap_or(Path::new("/"));
    let mut command = Command::new(&spec.script);
    command
        .args(&spec.args)
        .current_dir(script_dir)
        .env("WINBRIDGE_SOCK", &config.socket)
        .env("WINBRIDGE_QMP_SOCK", &config.qmp_socket)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_err)
        .process_group(0);
    if let Some(dbus) = &config.dbus_addr {
        command.env("WINBRIDGE_DBUS_ADDR", dbus);
    }

    let mut child = command
        .spawn()
        .map_err(|e| format!("starting {}: {}", spec.script.display(), e))?;
    info!("Launch script started: {}", spec.script.display());

    let exited = Arc::new(AtomicBool::new(false));
    let reached = Arc::new(AtomicBool::new(false));

    // QEMU counts as reachable once its QMP socket answers a status query.
    let qmp = QmpClient::new(config.qmp_socket.clone());
    {
        let exited = exited.clone();
        let reached = reached.clone();
        tokio::spawn(async move {
            while !exited.load(Ordering::SeqCst) {
                if qmp.execute("query-status", json!({})).await.is_ok() {
                    info!("QEMU is reachable over QMP");
                    reached.store(true, Ordering::SeqCst);
                    break;
                }
                sleep(REACH_POLL_INTERVAL).await;
            }
        });
    }

    tokio::spawn(async move {
        let status = child.wait().await;
        exited.store(true, Ordering::SeqCst);
        let was_reached = reached.load(Ordering::SeqCst);
        let outcome = match status {
            Ok(status) => classify(status, was_reached, &log_path),
            Err(e) => LaunchOutcome::Failed(format!("waiting for the launch script failed: {}", e)),
        };
        warn_if_failed(&outcome);
        glib::idle_add_once(move || {
            crate::with_viewer(|viewer| viewer.on_launch_exit(outcome.clone()));
        });
    });

    Ok(())
}

fn classify(status: ExitStatus, reached: bool, log_path: &Path) -> LaunchOutcome {
    match (status.success(), reached) {
        (true, true) => LaunchOutcome::VmGone,
        (true, false) => LaunchOutcome::Failed(format!(
            "The launch script exited before the VM was reachable. See {}",
            log_path.display()
        )),
        (false, _) => LaunchOutcome::Failed(format!(
            "The launch script stopped ({}). See {}",
            status,
            log_path.display()
        )),
    }
}

fn warn_if_failed(outcome: &LaunchOutcome) {
    if let LaunchOutcome::Failed(reason) = outcome {
        warn!("{}", reason);
    }
}
