<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# WinBridge

**Seamless Linux GTK4 Viewer & Windows 11 QEMU/KVM Guest Bridge**

WinBridge is an open-source suite that bridges a Linux GTK4 viewer window to a Windows 11 guest virtual machine running on QEMU/KVM. It provides high-performance canvas synchronization, hardware input, dynamic resolution resizing without letterboxing, per-monitor DPI synchronization, bidirectional clipboard sharing, and drag-and-drop file/text transfer.

---

## Features

1. **Canvas & Resolution Sync**:
   - Out-of-process GTK4 viewer displays the guest desktop using QEMU's high-speed D-Bus display interface (`-display dbus`).
   - Dragging or resizing the GTK window automatically drives the Windows guest desktop resolution to match in real-time, eliminating letterboxing.
2. **DPI Synchronization**:
   - The Windows guest DPI scale automatically tracks the Linux monitor and window scale factor (`100%`, `125%`, `150%`, `200%`, etc.).
   - Applied dynamically via the Windows Connecting and Configuring Displays (CCD) API.
3. **Bidirectional Clipboard Sharing**:
   - Real-time text and media clipboard sharing between host and guest.
   - Built-in echo loop prevention.
4. **Drag-and-Drop Sharing**:
   - Drop files directly onto the Linux GTK window to transfer them into the Windows guest (`Downloads\WinBridge` folder).
   - Supports plain text and file streaming.
5. **Robust Wire Protocol**:
   - Clean, versioned, length-prefixed framing over a dedicated VirtIO-serial channel (`org.winbridge.0`).

---

## Workspace Architecture

- **`winbridge-proto`**: Shared length-prefixed protocol, serialization types, and sync/async framing codecs.
- **`winbridge-host`**: Linux GTK4 application interfacing with QEMU D-Bus display and the virtio-serial bridge.
- **`winbridge-agent`**: Windows guest agent compiled for `x86_64-pc-windows-msvc`, executing Win32 display changes, DPI updates, and clipboard/drag-and-drop actions.
- **`docs/`**:
  - [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md): Architectural design, D-Bus display comparison, threading model.
  - [`docs/PROTOCOL.md`](docs/PROTOCOL.md): Wire framing, message variants, and error handling.
- **`scripts/`**:
  - `launch-qemu-winbridge.sh`: Example launcher demonstrating required QEMU arguments.
  - `install-agent.ps1`: PowerShell helper to install and start the agent in the Windows guest.

---

## Quick Start

### 1. Building the Host Viewer (Linux)

Install GTK4 development libraries:
```bash
# Debian / Ubuntu
sudo apt install libgtk-4-dev pkg-config
```

Build the host binary:
```bash
cargo build --release -p winbridge-host
```

### 2. Building the Guest Agent (Windows)

On a Windows development machine or cross-compilation environment with Rust and MSVC:
```powershell
cargo build --release -p winbridge-agent
```
The output binary will be located at `target\release\winbridge-agent.exe`.

### 3. Running QEMU with WinBridge

Launch QEMU with the D-Bus display backend and a VirtIO-serial channel:

```bash
qemu-system-x86_64 \
    -enable-kvm -m 8G -smp 4 \
    -drive file=/path/to/windows11.qcow2,format=qcow2 \
    -vga none -device virtio-gpu-pci \
    -display dbus,p2p=yes \
    -chardev socket,path=/tmp/winbridge.sock,server=on,wait=off,id=winbridge0 \
    -device virtio-serial-pci \
    -device virtserialport,chardev=winbridge0,name=org.winbridge.0
```

### 4. Running the Host Viewer

```bash
./target/release/winbridge-host --socket /tmp/winbridge.sock
```

### 5. Running the Guest Agent (Windows Guest)

Copy `winbridge-agent.exe` into the Windows guest and run:
```powershell
.\winbridge-agent.exe
```
Or install it as a persistent scheduled task using `scripts\install-agent.ps1`.

---

## License

This project is licensed under the **GNU General Public License v3.0 or later** ([GPL-3.0-or-later](LICENSE)).

```
WinBridge - Linux GTK4 Viewer & Windows 11 QEMU/KVM Guest Bridge
Copyright (C) 2026 Creeper23456 <Creeper23456@outlook.com>

This program is free software: you can redistribute it and/or modify
it under the terms of the GNU General Public License as published by
the Free Software Foundation, either version 3 of the License, or
(at your option) any later version.

This program is distributed in the hope that it will be useful,
but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
GNU General Public License for more details.
```
