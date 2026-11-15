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
Build inside the Windows build VM (`/mnt/VM/Windows11Build`) targeting `x86_64-pc-windows-msvc`:
```bash
cargo build --release -p winbridge-agent
```

## Safety and Privacy Rules
- Never commit private host paths, proxy credentials, personal tokens, or machine hostnames.
- Do not run unconstrained recursive filesystem scans.
- Use headless mock loopback or socket tests for unit and protocol verification.
