<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# AGENTS.md

Guidance for automated agents and developers working on the WinBridge codebase.

## Repository Overview

- **`crates/winbridge-proto`**: Shared length-prefixed wire protocol, message structures, and serialization codecs (sync and async via Tokio).
- **`crates/winbridge-host`**: Linux GTK4 viewer application interfacing with QEMU D-Bus display (`org.qemu.Display1`) and the virtio-serial bridge.
- **`crates/winbridge-agent`**: Windows 11 guest agent compiled for `x86_64-pc-windows-msvc`, handling resolution changes, CCD DPI scaling, clipboard integration, and file reception.
- **`docs/`**: Technical documentation (`ARCHITECTURE.md`, `PROTOCOL.md`).
- **`scripts/`**: Helper scripts for host QEMU launch and guest agent deployment.

## Build Guidelines

### Linux Host
```bash
cargo build --release -p winbridge-proto -p winbridge-host
cargo test -p winbridge-proto
```

### Windows Guest Agent
Build on Windows (or in a Windows build VM) for `x86_64-pc-windows-msvc`:
```bash
cargo build --release -p winbridge-agent
```
From Linux you can type-check it with `cargo check --target x86_64-pc-windows-msvc -p winbridge-agent`
(`rustup target add x86_64-pc-windows-msvc` first). It is linked with the static CRT (`.cargo/config.toml`).

## Things that bit us
- The agent needs administrator rights and the user's session: `\\.\Global\org.winbridge.0` cannot be opened by a limited token, and the agent then retries silently.
- The serial handle must stay overlapped (`FILE_FLAG_OVERLAPPED`); a synchronous handle blocks writes while a read is pending.
- Never write registry DPI values after a CCD DPI change; read the effective DPI back instead.
- zbus maps `set_ui_info` to `SetUiInfo`; QEMU's method is `SetUIInfo` (`#[zbus(name = "SetUIInfo")]`).

## Design Rules (iron laws)
- **Input capture is scoped by the pointer, strictly.** The viewer captures keyboard and mouse only while the window is active **and** the pointer is over the VM canvas. With the pointer on the title bar or header bar, outside the window, or with another window focused, nothing is captured: no keyboard grab, no shortcut inhibition, no key forwarding, and held guest keys are released. A toggle key must never be *required* to capture: Ctrl+Alt+G and a Right Ctrl tap only *release* by hand (an override that ends when the pointer leaves the canvas), and the opt-in relative "Game mode" is off by default and never changes the default absolute behaviour. An active keyboard grab is opt-in (`--grab-keyboard`), because it takes keys from the rest of the session, including other applications' input methods (Flatpak apps). All pointer and key events for the guest go through the one ordered queue in `input.rs`; never send them from separate tasks.
- **Never offer keyboards or mice for USB passthrough**, with no override; that would lock the user out of the host.
- **The VM lifecycle is the user's.** The viewer runs the configured launch script as-is and never kills QEMU or the script; shutdown is a graceful ACPI request.

## Safety and Privacy Rules
- Never commit private host paths, proxy credentials, personal tokens, or machine hostnames.
- Do not run unconstrained recursive filesystem scans.
- Use headless mock loopback or socket tests for unit and protocol verification.
