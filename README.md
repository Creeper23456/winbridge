<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# WinBridge

**A Linux GTK4 viewer and guest agent for Windows 11 on QEMU/KVM.**

WinBridge shows a Windows 11 guest desktop in a GTK4 window and keeps the two sides in sync: the guest resolution follows the window size (no letterboxing), the guest DPI scale follows the Linux scale factor, the text clipboard is shared both ways, and files/text can be dropped onto the window. The guest cursor replaces the Linux pointer while it is over the canvas.

It exists because QEMU's own GTK window cannot drive the resolution of a Windows guest using the virtio-gpu (`viogpudo`) driver: the driver learns the new size but nothing applies it. WinBridge asks QEMU for the new size and applies it itself from inside the guest.

## How it works

```
 GTK4 viewer (winbridge-host)                       Windows guest
 ┌──────────────────────────┐   -display dbus   ┌───────────────────────────┐
 │ canvas, input, cursor    │◄─────────────────►│ QEMU virtio-gpu           │
 │ window size + scale ─────┼─ SetUIInfo ──────►│ driver builds a custom mode│
 │                          │   virtio-serial   │ winbridge-agent           │
 │ clipboard, drops, DPI ───┼──org.winbridge.0─►│ SetDisplayConfig, DPI,    │
 └──────────────────────────┘                   │ clipboard, file receiver  │
                                                └───────────────────────────┘
```

- **Canvas, input, cursor**: QEMU `-display dbus`; the viewer registers a listener on `org.qemu.Display1.Console_0`. QEMU connects to a D-Bus *bus*, so you need a bus daemon (the launcher script starts a private one).
- **Resolution**: the viewer calls `SetUIInfo` with the canvas size in physical pixels (logical size x scale factor), then asks the agent to apply it with `SetDisplayConfig` (the same call the virtio-win helper uses). `ChangeDisplaySettings` is only a fallback: Windows offers it a fixed mode list.
- **DPI**: sent after the resolution (Windows caps the scale by the resolution). The agent reads the effective DPI back and reports the real outcome.
- **Channel**: versioned, length-prefixed frames on a dedicated virtio-serial port, `org.winbridge.0` ([protocol](docs/PROTOCOL.md), [architecture](docs/ARCHITECTURE.md)). It does not touch `org.qemu.guest_agent.0`.

## Status

Verified end to end (Windows 11 LTSC 2024 guest, virtio-win 0.1.285 GPU driver, QEMU 10.0):

| Feature | Result |
|---|---|
| Canvas shown in the GTK window | works |
| Guest resolution follows window size | works (e.g. 1044x661, 1280x677, 1920x888) |
| DPI sync | works up to what Windows allows; a request above the limit is reported as failed |
| Clipboard, host -> guest and guest -> host, text | works, no echo loop |
| Drag and drop of files onto the window | works (3 MiB file, SHA-256 identical) |
| Drag and drop of text onto the window | works (UTF-8 verified) |
| Guest cursor instead of Linux pointer | the Linux pointer is hidden over the canvas; a guest hardware-cursor sprite is shown if the guest sends one |

Known limitations:

- **DPI is capped by the resolution.** Windows only offers scale levels that leave a usable effective resolution (about 700 px of height or more). At 1920x888 the maximum was 125%, so a 200% request fails there. On a HiDPI display, where the canvas is large in physical pixels, 200% is reachable. **200% was not tested end to end** (no HiDPI monitor available); 100% and the 125% limit were.
- **Integer scale factors only.** GTK on X11 reports an integer `scale_factor`. Fractional Linux scaling is not seen.
- **Windows scale levels are discrete** (100, 125, 150, 175, 200 ...); the request is snapped to the nearest.
- **Clipboard: text only.** The protocol defines PNG/BMP formats but the viewer and agent do not sync images.
- **Drag and drop is host -> guest only.** Dropped files go to `%USERPROFILE%\Downloads\WinBridge`.
- **Fractional/very large resolutions** need enough virtio-gpu video memory.

## Build

### Host viewer (Linux)

```bash
sudo apt install libgtk-4-dev pkg-config   # Debian / Ubuntu
cargo build --release -p winbridge-host
```

### Guest agent (Windows, `x86_64-pc-windows-msvc`)

```powershell
cargo build --release -p winbridge-agent
```

The agent is linked with the static CRT (`.cargo/config.toml`), so it runs on a stock Windows without the Visual C++ redistributable. Type-checking from Linux works with `rustup target add x86_64-pc-windows-msvc && cargo check --target x86_64-pc-windows-msvc -p winbridge-agent`.

## Run

1. Start QEMU with the D-Bus display and the WinBridge port. [`scripts/launch-qemu-winbridge.sh`](scripts/launch-qemu-winbridge.sh) starts a private bus and shows the arguments:

   ```
   -display dbus,addr=unix:path=/tmp/qemu-dbus.sock
   -device virtio-gpu-pci,edid=on
   -device virtio-serial-pci
   -chardev socket,id=wb0,path=/tmp/winbridge.sock,server=on,wait=off
   -device virtserialport,chardev=wb0,name=org.winbridge.0
   ```

2. Start the viewer: `winbridge-host --socket /tmp/winbridge.sock --dbus-addr unix:path=/tmp/qemu-dbus.sock`.
3. Install the agent in the guest with an elevated PowerShell: `scripts\install-agent.ps1 -SourceBinary .\winbridge-agent.exe`.

The agent **must run in the logged-in user's session with administrator rights** (the installer registers a highest-privilege task at logon). Opening `\\.\Global\org.winbridge.0` needs administrator rights; a limited-token process retries silently forever. The viewer and the agent can be restarted independently of each other.

The guest needs the virtio-win drivers (`viogpudo`, `vioser`). Run the agent with `RUST_LOG=info` to see what it does.

## License

GPL-3.0-or-later; see [LICENSE](LICENSE).

```
WinBridge - Linux GTK4 viewer and Windows 11 guest agent for QEMU/KVM
Copyright (C) 2026 Creeper23456

This program is free software: you can redistribute it and/or modify
it under the terms of the GNU General Public License as published by
the Free Software Foundation, either version 3 of the License, or
(at your option) any later version.

This program is distributed in the hope that it will be useful,
but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
GNU General Public License for more details.
```
