<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# WinBridge

**English** | [简体中文](README.zh-CN.md)

**A Linux GTK4 viewer and guest agent for Windows 11 on QEMU/KVM.**

## Origin

I am working on the Axolotl engine and the JBP project, and money is very tight right now...

I sold two 16 GB memory sticks to pay for development. As a result, continuing on Windows was hardly an option, so I carried on in Debian, whose memory management is more flexible.

But the first problem of moving into the Linux ecosystem showed up right away: how do I check that the Bevy backend of the VN engine works in a Windows VM, and how do I check the Minecraft instances that some mods need to launch?

I had no choice but to use a Windows virtual machine. A Windows VM has no 3D graphics acceleration, so it is painfully slow. That is why this project was built.

## Introduction

WinBridge shows a Windows 11 guest desktop in a GTK4 window and offers a better interactive experience than virt-manager. It offers a Windows virtualization experience similar to VMware Workstation or Parallels Desktop, with these features:

- Two-way clipboard copy
- Quick USB device passthrough
- Vulkan 3D acceleration
- File transfer by drag and drop
- Absolute and relative mouse coordinates

It is lightweight: on the host you only need QEMU, not the heavy virt-manager. On the guest, you only need to prepare it as required, and it stays very light.

This project does not modify QEMU on the host, and it does not need unsigned drivers installed in Windows. This makes it basically work out of the box. On the host, you only need the QEMU package from your distribution. For the guest, I recommend:

- A modern Windows version. I have only tested Windows 11 LTSC 24H2, Chinese edition.
- All the VirtIO drivers installed, including the VirtIO display driver that has no 3D acceleration.
- QemuGA, the QEMU Guest Agent, installed.
- The ivshmem driver installed. It is Red Hat's IVSHMEM driver. Take `ivshmem.inf`, `ivshmem.cat` and `ivshmem.sys` from the Looking Glass Windows host installer (`looking-glass-host-setup.exe`, since B6 it bundles the driver, see the [Looking Glass documentation](https://looking-glass.io/docs/B7/install_host/)), put them in one folder and run `winbridge-agent.exe --install-ivshmem <folder>` from an elevated prompt. The command checks the signatures first, is safe to run twice, and `--uninstall-ivshmem` removes it again (see [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)).
- The ivshmem device and the serial device configured with the VM parameters from this repository.

Install the Agent built from this project in the guest, and user-mode Vulkan forwarding and rendering are enabled.

I would really like someone to make this project work on a Hackintosh through Metal2Vulkan. Pull requests from anyone are very welcome.

For this project, I hope everyone will actively submit pull requests to improve its features. You can also share your views and feature suggestions in the issue discussions. But this is not my main project: I may only join the discussions and will not add more new features unless someone submits a pull request. Still, I am glad to exchange ideas about the project with you in more depth.

I will review pull requests and give my suggestions for changes if a request is not good enough. Please also keep to a friendly and respectful community consensus. For issues, please be polite and provide detailed logs wherever possible.

Almost all the code of this project was written by AI; I took part only in the architecture design. So, unlike some open-source organizations that resist AI, I warmly welcome pull requests with AI-generated code, as long as the quality is good.

The quality requirement is strict. It covers these dimensions:

- Maintainability
- Robustness
- Is it the better engineering solution, or was it added as a temporary stopgap?
- Does it affect existing stable features, and if so, how large is the effect?

What follows is Claude taking over, to introduce the project itself:

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
- **Shared memory (optional, `shm = true`)**: an `ivshmem` device carries the control and bulk lanes (clipboard, drops, file streams) with priorities, so a large paste cannot queue control messages behind it. virtio-serial stays the handshake and the fallback ([design and measurements](docs/IVSHMEM-TRANSPORT.md)).
- **Display path**: when QEMU offers it (`Listener.Unix.Map`), the viewer maps the guest's scanout memory read-only and draws from it, so no pixel is sent or copied; GTK redraws are paced by updates, not by the compositor (see [Speed and diagnostics](#speed-and-diagnostics)).

## 3D acceleration

Programs in the guest can use the host GPU. WinBridge ports Mesa's **Venus**, a Vulkan serialization layer, to Windows: the guest driver `vulkan_virtio.dll` encodes every Vulkan call into a command stream, the stream crosses to the host through shared memory, and `winbridge-venus` runs it on the host GPU with virglrenderer's Venus renderer. The design, the alternatives that were rejected and every measurement are in [ADR 0001](docs/adr/0001-venus-as-serialization-layer.md); build, install and run are in [`venus/README.md`](venus/README.md).

- **What runs**: Vulkan programs; Direct3D 9, 10 and 11 through DXVK; OpenGL 4.6 through Zink; 32-bit programs with the x86 build of the same DLLs; FurMark 2's Vulkan mode and its OpenGL mode. Vulkan needs nothing per program. OpenGL and Direct3D need their DLLs loaded: put them next to a program, or install the **global hook** (`scripts/install-gfx-hook.ps1`, an AppInit_DLLs module) that gives them to every program that is not part of Windows, except a deny list, without touching the program's folder. Tested with the hook: the OpenGL and Direct3D test programs, x64 and x86, and a copy of FurMark 2 (OpenGL 4.6, about 230 fps). Not reached: sandboxed programs, so browsers need `--use-angle=vulkan` (Edge was run that way), and everything when Secure Boot is on. Every program that creates a DXGI factory takes one of the Venus slots while it runs; see [`venus/README.md`](venus/README.md) and ADR 3.11 before installing it with few slots.
- **Measured** (Windows 11 LTSC guest, 4 vCPU, RTX 3060 Ti host, normal user token): a D3D11 triangle takes 1.9 ms a frame, the teapot scene 2.0 ms at 1280x720 (about 500 frames a second); a 16-minute run showed no leak and no host warning; FurMark's Vulkan mode runs at about 100 FPS. Direct3D 10 (2.35 ms) and 9, and the 32-bit builds (2.5 to 3.2 ms for the triangles and the teapot), ran too.
- **No broker**: every Vulkan process gets one *slot*, a 64 MiB `ivshmem-plain` device plus a virtio-serial wake-up port, and one `winbridge-venus` process serves it. The launch script example starts them with `WINBRIDGE_VENUS=N`.
- **Host needs**: a virglrenderer with the patches in `venus/patches/virglrenderer` (two of them are needed on NVIDIA 550) and its render server.
- **Build from Linux**: `WBVN_ACCEPT_MS_LICENSE=1 venus/build/build.sh` cross-compiles the driver, the OpenGL DLLs and the x86 builds with `clang-cl`; nothing is built inside the guest. `scripts/install-venus-icd.ps1` registers the driver (it needs the ivshmem driver).
- **Cost**: the host's `winbridge-venus` busy-polls one core on purpose. During the teapot scene at 720p a 15 s sample showed the viewer at 45% of a core, QEMU at 202% and the Venus thread at 100%.
- **Limits**: one Vulkan process per slot; its Vulkan instances share it (DXVK and FurMark's Vulkan mode create two). A program that leaves the OpenGL swap interval at 1 is paced to about 14.7 ms a frame. Applications other than the test programs and FurMark were not run. Without `winbridge-venus` a Vulkan program waits up to 30 s for the host. Venus reports its own `driverID` (`VK_DRIVER_ID_MESA_VENUS`, 22), as upstream Mesa does. The GeeXLab 2021 ray-tracing demo (`Vulkan_Raytracing_Demo`, Geeks3D) indexes a 10-entry table with that value and crashes in `plugin_gxc_vulkan2_x64.dll`; with the ID forced to an NVIDIA one in a throwaway build the demo ran its hardware ray tracing on the host GPU at 30 FPS. The ID was left as upstream has it.

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
| 3D: Vulkan, Direct3D 9/10/11 (DXVK), OpenGL (Zink), x64 and x86, FurMark 2 Vulkan | works in the LTSC VM with the test programs and FurMark; other applications not run |
| Shared-memory transport (`shm = true`) for control and bulk traffic | works; in the build VM 2.0-3.4 GiB/s host to guest against 239 MiB/s over virtio-serial, and control round trips stay at p50 135-150 us while a bulk stream saturates it (serial: p50 4.2 ms) |
| Display from shared-memory scanout, no copy in the viewer | works (framebuffer copy 12.5 ms -> 0.0 ms; about 60 display updates a second with GDI and with Vulkan programs) |
| Folder drag and drop as one zip, both directions | host -> guest verified in the LTSC VM with the headless `folderdnd` driver (nested, empty and non-ASCII entries and a 5 MiB file arrived with identical SHA-256; names Windows cannot hold were left out and listed; a second drop landed as `name (1)`); the GTK drop gesture and guest -> host not run |

Known limitations:

- **DPI is capped by the resolution.** Windows only offers scale levels that leave a usable effective resolution (about 700 px of height or more). At 1920x888 the maximum was 125%, so a 200% request fails there. On a HiDPI display, where the canvas is large in physical pixels, 200% is reachable. **200% was not tested end to end** (no HiDPI monitor available); 100% and the 125% limit were.
- **Integer scale factors only.** GTK on X11 reports an integer `scale_factor`. Fractional Linux scaling is not seen.
- **Windows scale levels are discrete** (100, 125, 150, 175, 200 ...); the request is snapped to the nearest.
- **Clipboard: text, HTML, RTF, images and files, both directions.** Text, HTML, RTF and images (items up to 48 MiB) sync at copy time. Copied files do **not**: nothing is transferred until you paste (Ctrl+V), then a progress bar appears at the top of the VM canvas, input to the VM is locked while it runs (the guest keeps running), and the files arrive in the paste target (up to 8 GiB per paste). Virtual file sources (for example archive managers or mail attachments) are not supported on either side. *Unit-tested for the shared logic and the guest's delayed rendering inside the build VM; not yet tested end to end.*
- **Drag and drop works in both directions.** Host -> guest: the pointer position decides. Receiving starts when you release the button, into a private staging folder in the guest, and the files are then **moved** to the Explorer folder or desktop under the pointer (anything else lands in `Downloads\WinBridge`); dropped text is saved as a `.txt` file. Guest -> host: drag from a Windows application and move the pointer out of the canvas with the button held; the Linux application under the pointer receives the files, and where they end up is its choice (the viewer fetches them from the guest only at the drop; limit 8 GiB). A folder travels as one zip (pack, send, unpack, delete fill the progress bar); what the other system cannot create is left out and listed in a notice. A progress bar and input lock show during every transfer. Only files and folders that exist on disk and text can be dragged out; virtual files (archives, mail attachments, browser downloads) cannot. Existing files and folders are never overwritten. Handing an application a file path while the file is still streaming is not implemented. *Not yet verified end to end through the viewer; guest -> host depends on the guest's OLE drag asking the probe window within 700 ms, which has not been observed. Folder drops from the host to the guest were run in the VM through the viewer's protocol with a headless driver (`folderdnd`): the nested, empty and non-ASCII entries and a 5 MiB file arrived with identical SHA-256, names Windows cannot hold were left out and listed, and a second drop landed as `name (1)`. The GTK drop gesture and the guest -> host zip have not been run there (the zip code is unit-tested).*
- **Fractional/very large resolutions** need enough virtio-gpu video memory.
- **The guest display runs at 64 Hz, not at the host monitor's rate.** The viewer asks for the lowest rate among the host's monitors (at most 60 Hz); the virtio-gpu display-only driver keeps 64 Hz and refuses (`Guest did not take the refresh rate 60000 mHz ... it runs at 64000 mHz` in the viewer log). A GDI program gets about 60 display updates a second, and so do programs that present through the Venus driver (measured with the driver's 1 ms timer resolution; an earlier build asked for 0.5 ms, which cut the whole desktop to about 33 updates a second, see the LTSC notes in `docs/adr/0001-venus-as-serialization-layer.md`).
- **Mica and acrylic are off until DWM is told otherwise.** DWM composes on the display-only virtio-gpu adapter with the Basic Render Driver (`dxdiag`: WDDM 1.3, Direct3D 0/4) and paints the opaque fallback colours: a test window that asked for Mica or acrylic (`DwmSetWindowAttribute` succeeded and read the value back) came out flat grey. Setting `HKLM\SOFTWARE\Microsoft\Windows\Dwm\ForceEffectMode` to 2 (a value DWM reads; the name is in `uDWM.dll` and `dwmcore.dll`, that 2 means "on" was found by trying) and restarting DWM (`Stop-Process dwm`, Windows starts it again, or sign out) gave acrylic a real blur and Mica the wallpaper-tinted material in the same test; DWM used 0.13 s of CPU in 8 idle seconds with it on. Not measured: DWM's cost while windows move, any effect on the 60 display updates a second, and a reboot. Delete the value and restart DWM to undo.

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
   -smp 8,sockets=1,cores=8,threads=1
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

   The default is **8 vCPUs** in one socket (`WINBRIDGE_VCPUS=N` changes it). Windows composes its desktop on the CPU (WARP, see "Limits" below): while a window moves that took about 2 busy cores of a 4-vCPU guest, so 4 leave the programs little. 8 is the setting the launch script and the example VM use; the effect of 8 against 4 on frame times has not been measured. The guest sees one socket because Windows client editions use at most two.

2. Start the viewer: `winbridge-host --socket /tmp/winbridge.sock --dbus-addr unix:path=/tmp/qemu-dbus.sock --qmp-socket /tmp/winbridge-qmp.sock`. The header-bar **Devices** menu inserts/ejects CD/DVD images and passes host USB devices to Windows. USB hotplug works in both directions and reattaches checked devices after reconnection. Passthrough detaches a device from its Linux host driver while attached. Keyboards and mice are never offered for passthrough, with no override: any device with a keyboard or mouse HID interface is filtered out, including composite devices. The host user needs access to `/dev/bus/usb/*`; run `scripts/install-host-udev.sh` (use `--dry-run` to preview) to grant it.
3. Install the agent in the guest with an elevated PowerShell: `scripts\install-agent.ps1 -SourceBinary .\winbridge-agent.exe`. The script checks the file first (a 64-bit Windows program, the WinBridge agent, runs), warns when the VM has no working virtio serial device, restricts the install folder to SYSTEM and Administrators (the service runs what is in it), keeps the previous binary as `winbridge-agent.previous.exe`, and **restores the previous state and exits with 1** if the service does not start or the agent does not come up in the console session. Running it again updates. `-Status` shows the state (service, binary hash, processes, logs, shared-memory devices) and changes nothing; `-Uninstall` removes the service (files stay; `-Purge` also deletes the binaries and `%ProgramData%\WinBridge`); `-InstallDir` and `-StartTimeoutSeconds` change the folder and the wait.

The agent **runs as SYSTEM**. The installer registers the service `WinBridgeAgent` (starts at boot, LocalSystem), which starts the agent in the console session with the logged-on user's environment, already at the logon screen, and starts it again when it ends or when another user logs on. Opening `\\.\Global\org.winbridge.0` needs administrator rights; a limited-token process retries silently forever. Code that touches the user's files runs as the user (the threads impersonate the logged-on user), so dropped and pasted files belong to the user and land in the user's profile. The service logs to `%ProgramData%\WinBridge\service.log`, the agent to `%LOCALAPPDATA%\WinBridge\agent.log` of the logged-on user. The viewer and the agent can be restarted independently of each other.

The guest needs the virtio-win drivers (`viogpudo`, `vioser`). Run the agent with `RUST_LOG=info` to see what it does.

### Speed and diagnostics

- `--shm` (or `shm = true`) turns on the shared-memory transport. It needs the ivshmem device (`WINBRIDGE_SHM=1` in `scripts/launch-qemu-winbridge.sh`), the ivshmem driver in the guest and the agent installed with it; without them nothing changes.
- The viewer sets `GDK_DEBUG=no-vsync` before GTK starts. On the measured X11 GNOME session (monitors at 75 and 60 Hz) GTK's frame clock ran at a median of 33 ms and a guest that delivered 60 updates a second was shown at 30. `--vsync` restores GTK's pacing.
- `--stats` logs once a second what the display path costs (updates, time in the handler, draws, framebuffer copy time, how far apart the updates arrive) and, while you use the mouse or keyboard, how many input calls went to QEMU and how long QEMU took to answer them.
- `winbridge-venus --stats` logs its loop and how many bytes of guest memory it compared, split into rings, other shared memory and device memory.
- Examples that play the viewer against a real agent without GTK: `shme2e` (transport throughput and latency) and `folderdnd` (one folder drop), in `crates/winbridge-host/examples/`. The viewer must not be connected while they run.

### Launch script and lifecycle

You manage how the VM starts. Point the viewer at a script and it runs it once, unchanged, as its child:

```bash
winbridge-host --launch-script ~/vm/start-windows.sh
```

or in `~/.config/winbridge/config.toml` (keys: `launch_script`, `launch_args`, `socket`, `dbus_addr`, `qmp_socket`, `usb_bus`, `grab_keyboard`, `shm`; command-line options win; unknown keys are an error):

```toml
launch_script = "/home/me/vm/start-windows.sh"
```

The script gets `WINBRIDGE_SOCK`, `WINBRIDGE_DBUS_ADDR` and `WINBRIDGE_QMP_SOCK` in its environment; its output goes to `~/.local/state/winbridge/launch.log`. [`scripts/launch-qemu-winbridge.sh`](scripts/launch-qemu-winbridge.sh) is an example that honors them. Without a launch script the viewer attaches to a VM you started yourself. A script that fails before the VM is reachable shows an error with the log path; a clean exit after the VM ran closes the viewer.

The viewer lives and dies with the VM. Closing the window asks to **Shut down VM** (graceful ACPI powerdown; nothing is killed), **Minimize to tray**, or **Cancel**; there is no "close viewer only". The tray icon (StatusNotifierItem) shows/hides the window and can shut the VM down; on GNOME it needs the AppIndicator/KStatusNotifierItem extension, otherwise "Minimize to tray" only minimizes. QEMU serves one QMP client at a time, so the viewer connects per command and leaves the socket free for other tools. Starting profiles through virt-manager/libvirt is not implemented.

## Input

All mouse buttons (left, middle, right, back, forward) and the scroll wheel (both axes) are forwarded. **Capture is scoped by the pointer, strictly:** keyboard and mouse go to the guest only while the window is active and the pointer is over the VM canvas. On the title bar or header bar, outside the window, or with another window focused, nothing is captured, and any key or button the guest still holds is released; key events outside that scope propagate normally. Over the canvas the Linux pointer is hidden and only the Windows cursor is shown. Mouse events reach QEMU through one ordered queue, and every button press or release is preceded by the exact pointer position of the same event, so a click lands where the pointer is.

**Releasing the mouse by hand.** Ctrl+Alt+G, or a tap of Right Ctrl (pressed and released with no other key in between), releases the mouse and keyboard while the pointer is over the canvas, for example when the guest hangs. The shortcuts are handled entirely on the host: they work without `--grab-keyboard`, with a hung guest and during a file transfer, the guest never sees the G, and Ctrl/Alt already sent to the guest are released first. Right Ctrl is only held back until the next key, so guest shortcuts that use it as a modifier keep working. While released the Linux pointer is shown, nothing is forwarded, and the header says `Mouse released`. The release lasts until you press either shortcut again, click the canvas (that click is not sent to the guest), or move the pointer out of the canvas and back in. The header names the shortcuts (`Ctrl+Alt+G or Right Ctrl: release mouse`) only while the guest agent does not answer its keepalive for about 5 seconds (or the agent channel is down while the VM runs), and in Game mode.

By default the viewer does not take an active keyboard grab, because a grab takes every key from the rest of the session, including input methods of other applications (Flatpak apps stopped accepting input in testing). Keys that reach the focused canvas are forwarded, but desktop shortcuts such as Super (Windows key) and Alt+Tab stay with the desktop. Start with `--grab-keyboard` (or `grab_keyboard = true` in `config.toml`) to opt in: while the pointer is over the canvas, the viewer then holds the keyboard on X11 and inhibits system shortcuts on Wayland, so those shortcuts reach Windows.

**Game mode (relative mouse), automatic.** There is no button. While a program that is not part of Windows uses the 3D acceleration (it holds the Venus driver; the guest agent reports the list of such programs and ends it when the last one ends), the canvas switches to relative motion for games that need raw deltas (first-person cameras). While the pointer is over the canvas it is then hidden and kept at the canvas centre, and movement is sent as `RelMotion` deltas. A small badge (a mouse icon and "Game mode") shows for a moment each time the pointer goes into it. Release with Ctrl+Alt+G or Right Ctrl (same rules as above); focus loss, a transfer, shutdown and a stopped VM release it too. *Not yet run end to end in the VM.* Requirements and limits, stated plainly:

- QEMU delivers pointer input to one device at a time, and refuses `RelMotion` while that device is absolute (the tablet). So when game mode starts the viewer makes a relative device current with the monitor command `mouse_set` over the QMP socket (`info mice` lists the devices), and makes the tablet current again when it ends and once at startup. The machine's PS/2 mouse is relative and always present on the q35 machine type; a `-device usb-mouse` works too (`scripts/launch-qemu-winbridge.sh` adds it when started with `WINBRIDGE_REL_MOUSE=1`). If no relative device can be chosen, or QEMU still refuses relative motion, game mode stays off until the programs end and the header says why. That Windows takes motion from the PS/2 mouse has not been verified here.
- Without a relative device, a game that re-centres the cursor itself (most first-person games) cannot work through the tablet: the host cannot follow the guest's own cursor warps.
- It needs **X11**. GTK4 has no pointer-lock API on Wayland; there game mode does not start and the header says why, instead of faking it. On X11 the pointer is warped back to the canvas centre once it strays; there is no pointer or keyboard grab, so a very fast flick can still leave the canvas, which releases the capture as usual.
- Game mode follows the 3D programs and nothing else, and the capture is scoped by the pointer like every other: with the pointer off the canvas, or after a manual release, the guest pointer behaves as always. If QEMU refuses relative motion, it stays off until those programs end and the header says why.

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
