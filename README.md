<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# WinBridge

**A Linux GTK4 viewer and guest agent for Windows 11 on QEMU/KVM.**

WinBridge shows a Windows 11 guest desktop in a GTK4 window and keeps the two sides in sync: the guest resolution follows the window size (no letterboxing), the guest DPI scale follows the Linux scale factor, the clipboard (text, HTML, RTF, images, files) is shared both ways, and files/text can be dragged between the two sides. The guest cursor replaces the Linux pointer while it is over the canvas.

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
| Clipboard HTML, RTF, images, files; drag from Windows to Linux | implemented, **not yet verified end to end** |
| Guest cursor instead of Linux pointer | the Linux pointer is hidden over the canvas; a guest hardware-cursor sprite is shown if the guest sends one |

Known limitations:

- **DPI is capped by the resolution.** Windows only offers scale levels that leave a usable effective resolution (about 700 px of height or more). At 1920x888 the maximum was 125%, so a 200% request fails there. On a HiDPI display, where the canvas is large in physical pixels, 200% is reachable. **200% was not tested end to end** (no HiDPI monitor available); 100% and the 125% limit were.
- **Integer scale factors only.** GTK on X11 reports an integer `scale_factor`. Fractional Linux scaling is not seen.
- **Windows scale levels are discrete** (100, 125, 150, 175, 200 ...); the request is snapped to the nearest.
- **Clipboard: text, HTML, RTF, images and files, both directions.** Text, HTML, RTF and images (items up to 48 MiB) sync at copy time. Copied files do **not**: nothing is transferred until you paste (Ctrl+V), then a progress bar appears at the top of the VM canvas, input to the VM is locked while it runs (the guest keeps running), and the files arrive in the paste target (up to 8 GiB per paste). Virtual file sources (for example archive managers or mail attachments) are not supported on either side. *Unit-tested for the shared logic and the guest's delayed rendering inside the build VM; not yet tested end to end.*
- **Drag and drop works in both directions.** Host -> guest: the pointer position decides. Receiving starts when you release the button, into a private staging folder in the guest, and the files are then **moved** to the Explorer folder or desktop under the pointer (anything else lands in `Downloads\WinBridge`); dropped text is saved as a `.txt` file. Guest -> host: drag from a Windows application and move the pointer out of the canvas with the button held; the Linux application under the pointer receives the files, and where they end up is its choice (the viewer fetches them from the guest only at the drop; limit 8 GiB). A progress bar and input lock show during every transfer. Only files that exist on disk and text can be dragged out; virtual files (archives, mail attachments, browser downloads) cannot. Existing files are never overwritten. Handing an application a file path while the file is still streaming is not implemented. *Not yet verified end to end; guest -> host depends on the guest's OLE drag asking the probe window within 700 ms, which has not been observed.*
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
   -qmp unix:/tmp/winbridge-qmp.sock,server=on,wait=off
   -device qemu-xhci,id=xhci
   -drive if=none,id=cd0,media=cdrom,readonly=on
   -device ide-cd,drive=cd0,id=cdrom0
   -device virtio-gpu-pci,edid=on
   -device virtio-serial-pci
   -chardev socket,id=wb0,path=/tmp/winbridge.sock,server=on,wait=off
   -device virtserialport,chardev=wb0,name=org.winbridge.0
   ```

2. Start the viewer: `winbridge-host --socket /tmp/winbridge.sock --dbus-addr unix:path=/tmp/qemu-dbus.sock --qmp-socket /tmp/winbridge-qmp.sock`. The header-bar **Devices** menu inserts/ejects CD/DVD images and passes host USB devices to Windows. USB hotplug works in both directions and reattaches checked devices after reconnection. Passthrough detaches a device from its Linux host driver while attached. Keyboards and mice are never offered for passthrough, with no override: any device with a keyboard or mouse HID interface is filtered out, including composite devices. The host user needs access to `/dev/bus/usb/*`; run `scripts/install-host-udev.sh` (use `--dry-run` to preview) to grant it.
3. Install the agent in the guest with an elevated PowerShell: `scripts\install-agent.ps1 -SourceBinary .\winbridge-agent.exe`.

The agent **must run in the logged-in user's session with administrator rights** (the installer registers a highest-privilege task at logon). Opening `\\.\Global\org.winbridge.0` needs administrator rights; a limited-token process retries silently forever. The viewer and the agent can be restarted independently of each other.

The guest needs the virtio-win drivers (`viogpudo`, `vioser`). Run the agent with `RUST_LOG=info` to see what it does.

### Launch script and lifecycle

You manage how the VM starts. Point the viewer at a script and it runs it once, unchanged, as its child:

```bash
winbridge-host --launch-script ~/vm/start-windows.sh
```

or in `~/.config/winbridge/config.toml` (keys: `launch_script`, `launch_args`, `socket`, `dbus_addr`, `qmp_socket`, `usb_bus`; command-line options win; unknown keys are an error):

```toml
launch_script = "/home/me/vm/start-windows.sh"
```

The script gets `WINBRIDGE_SOCK`, `WINBRIDGE_DBUS_ADDR` and `WINBRIDGE_QMP_SOCK` in its environment; its output goes to `~/.local/state/winbridge/launch.log`. [`scripts/launch-qemu-winbridge.sh`](scripts/launch-qemu-winbridge.sh) is an example that honors them. Without a launch script the viewer attaches to a VM you started yourself. A script that fails before the VM is reachable shows an error with the log path; a clean exit after the VM ran closes the viewer.

The viewer lives and dies with the VM. Closing the window asks to **Shut down VM** (graceful ACPI powerdown; nothing is killed), **Minimize to tray**, or **Cancel**; there is no "close viewer only". The tray icon (StatusNotifierItem) shows/hides the window and can shut the VM down; on GNOME it needs the AppIndicator/KStatusNotifierItem extension, otherwise "Minimize to tray" only minimizes. QEMU serves one QMP client at a time, so the viewer connects per command and leaves the socket free for other tools. Starting profiles through virt-manager/libvirt is not implemented.

## Input

All mouse buttons (left, middle, right, back, forward) and the scroll wheel (both axes) are forwarded. **Capture is scoped by the pointer, strictly:** keyboard and mouse go to the guest only while the window is active and the pointer is over the VM canvas. On the title bar or header bar, outside the window, or with another window focused, nothing is captured, and any key or button the guest still holds is released; key events outside that scope propagate normally. Over the canvas the Linux pointer is hidden and only the Windows cursor is shown. Mouse events reach QEMU through one ordered queue, and every button press or release is preceded by the exact pointer position of the same event, so a click lands where the pointer is.

**Releasing the mouse by hand.** Ctrl+Alt+G, or a tap of Right Ctrl (pressed and released with no other key in between), releases the mouse and keyboard while the pointer is over the canvas, for example when the guest hangs. The shortcuts are handled entirely on the host: they work without `--grab-keyboard`, with a hung guest and during a file transfer, the guest never sees the G, and Ctrl/Alt already sent to the guest are released first. Right Ctrl is only held back until the next key, so guest shortcuts that use it as a modifier keep working. While released the Linux pointer is shown, nothing is forwarded, and the header says `Mouse released`. The release lasts until you press either shortcut again, click the canvas (that click is not sent to the guest), or move the pointer out of the canvas and back in. The header names the shortcuts (`Ctrl+Alt+G or Right Ctrl: release mouse`) only while the guest agent does not answer its keepalive for about 5 seconds (or the agent channel is down while the VM runs), and in Game mode.

By default the viewer does not take an active keyboard grab, because a grab takes every key from the rest of the session, including input methods of other applications (Flatpak apps stopped accepting input in testing). Keys that reach the focused canvas are forwarded, but desktop shortcuts such as Super (Windows key) and Alt+Tab stay with the desktop. Start with `--grab-keyboard` (or `grab_keyboard = true` in `config.toml`) to opt in: while the pointer is over the canvas, the viewer then holds the keyboard on X11 and inhibits system shortcuts on Wayland, so those shortcuts reach Windows.

**Game mode (relative mouse), off by default.** The **Game mode** toggle in the header switches the canvas to relative motion for games that need raw deltas (first-person cameras). While the pointer is over the canvas it is then hidden and kept at the canvas centre, and movement is sent as `RelMotion` deltas. Release with Ctrl+Alt+G or Right Ctrl (same rules as above); focus loss, a transfer, shutdown and a stopped VM release it too. Requirements and limits, stated plainly:

- The guest needs a **relative** pointing device. QEMU's D-Bus display documents that `RelMotion` returns an error while `Mouse.IsAbsolute` is true, so a guest with only the absolute `usb-tablet` cannot take relative motion. Add one to the QEMU command line, for example `-device usb-mouse` (`scripts/launch-qemu-winbridge.sh` adds it when started with `WINBRIDGE_REL_MOUSE=1`; your own launch script needs the same device). If QEMU refuses relative motion, Game mode switches itself off and the header says why. Which device `IsAbsolute` reports when both a tablet and a mouse exist depends on QEMU's active input handler; this has not been verified here.
- Without a relative device, a game that re-centres the cursor itself (most first-person games) cannot work through the tablet: the host cannot follow the guest's own cursor warps.
- It needs **X11**. GTK4 has no pointer-lock API on Wayland, and the button is disabled there with an explanation instead of faking it. On X11 the pointer is warped back to the canvas centre once it strays; there is no pointer or keyboard grab, so a very fast flick can still leave the canvas, which releases the capture as usual.
- Game mode does not change the default absolute behaviour; it is never active unless you switch it on.

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
