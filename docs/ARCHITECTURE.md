<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# WinBridge Architecture

WinBridge is an open-source bridge between a Linux GTK4 viewer window and a Windows 11 guest virtual machine running under QEMU/KVM. It provides seamless canvas rendering, bidirectional input, dynamic resolution synchronization, Per-Monitor DPI scaling synchronization, bidirectional clipboard sharing, and file/text drag-and-drop.

```
+------------------------------------------------------------------------------------+
|                                    Linux Host                                      |
|                                                                                    |
|  +------------------------------------------------------------------------------+  |
|  |                         winbridge-host (GTK4 + Tokio)                        |  |
|  |                                                                              |  |
|  |  +------------------+  +------------------+  +----------------------------+  |  |
|  |  | GtkDrawingArea   |  | Input Handler    |  | Serial Channel Controller  |  |  |
|  |  | (Cairo Scanout)  |  | (Keys/Mouse)     |  | (Resolution, DPI, Dnd)     |  |  |
|  |  +--------^---------+  +--------+---------+  +-------------+--------------+  |  |
|  +-----------|---------------------|--------------------------|-----------------+  |
|              | P2P D-Bus Listener  | D-Bus Input              | Unix Stream Socket |
|              | (/org/qemu/...)     | (/org/qemu/...)          | (/tmp/winbridge...) |
|  +-----------v---------------------v--------------------------+-----------------+  |
|  |                              QEMU / KVM                                      |  |
|  |                                                                              |  |
|  |   -display dbus,addr=unix:path=...  (a D-Bus bus, not p2p)                   |  |
|  |   -device virtio-gpu-pci                                                     |  |
|  |   -device virtio-serial-pci                                                  |  |
|  |   -device virtserialport,chardev=winbridge0,name=org.winbridge.0             |  |
|  +------------------------------------------------------------+-----------------+  |
+---------------------------------------------------------------|--------------------+
                                                                | VirtIO Serial
+---------------------------------------------------------------v--------------------+
|                                 Windows 11 Guest                                   |
|                                                                                    |
|  +------------------------------------------------------------------------------+  |
|  |                          winbridge-agent (MSVC x64)                          |  |
|  |                                                                              |  |
|  |  +----------------------+  +---------------------+  +---------------------+  |  |
|  |  | VirtIO Serial Client |  | Display & DPI Sync  |  | Clipboard & DnD     |  |  |
|  |  | (\\.\Global\         |  | (ChangeDisplay-     |  | (Clipboard format   |  |  |
|  |  |  org.winbridge.0)    |  |  SettingsEx, CCD)   |  |  listener, Shell)   |  |  |
|  |  +----------------------+  +---------------------+  +---------------------+  |  |
|  +------------------------------------------------------------------------------+  |
+------------------------------------------------------------------------------------+
```

---

## 1. Display Architecture: QEMU D-Bus Display vs. Alternatives

### The Problem with Stock `-display gtk`
Under Windows 11 with the Red Hat `virtio-gpu` (`viogpudo.sys`) display driver, host-initiated resolution resize events from QEMU's built-in GTK interface are ignored. The `viogpudo` driver does not dynamically adjust the active display mode in response to host window resizing, leading to letterboxing, distortion, or static resolutions.

### Why QEMU `-display dbus`?
QEMU 7.0+ introduces `-display dbus`, exporting the VM's graphical console, input devices, and audio over standard D-Bus interfaces (`org.qemu.Display1`).
- **P2P Zero-Overhead Scanout:** The host registers a dedicated listener socketpair via `org.qemu.Display1.Console.RegisterListener(fd)`. QEMU connects directly to the listener over a private Unix socket and transmits dirty rectangle updates (`Update`) and frame resizes (`Scanout`).
- **Direct Input Injection:** Key press/release events and mouse motion/clicks are injected via `org.qemu.Display1.Keyboard` and `org.qemu.Display1.Mouse` without requiring synthetic emulated USB devices or window grabs. The default is absolute positioning (`SetAbsPosition`, needs an absolute device such as `usb-tablet` in the guest); the opt-in game mode uses `RelMotion` (see "Input Pipeline, Capture and Game Mode").
- **Out-of-Process UI Independence:** Isolating the viewer from QEMU's internal GUI thread prevents crashes in the guest or viewer from affecting VM process stability.

---

## 2. Agent Channel Protocol & VirtIO-Serial

To drive guest desktop resolution, DPI scaling, clipboard exchange, and drag-and-drop transfers, WinBridge implements a paravirtualized serial communication channel:
- **Host Configuration:**
  ```bash
  -chardev socket,path=/tmp/winbridge.sock,server=on,wait=off,id=winbridge0
  -device virtserialport,chardev=winbridge0,name=org.winbridge.0
  ```
- **Guest Device Path:** Windows VirtIO Serial (`vioserial.sys`) exposes the port under the DOS device namespace as `\\.\Global\org.winbridge.0`.
- **Wire Framing:**
  - 4-byte Magic: `b"WB01"`
  - 4-byte Payload Length: `u32` (little-endian)
  - Variable-length Payload: Bincode-serialized strongly-typed `Message` enum.
  - Maximum frame size is capped at 64 MB to safeguard against memory exhaustion.

---

## 3. Threading & Event Loop Model

### Host Architecture (`winbridge-host`)
1. **GTK Main Thread:** Runs the GTK4 application main loop (`glib::MainContext`), manages the window, handles pointer motion and key controllers, renders the Cairo scanout surface in a `GtkDrawingArea`, and processes drag-and-drop events via `GtkDropTarget`.
2. **Tokio Async Worker:**
   - Connects to `/tmp/winbridge.sock` to service the guest agent channel.
   - Dispatches `SetResolution`, `SetDpiScale`, and file drop streams to the guest.
   - Receives clipboard updates and completion acknowledgments from the agent.
3. **P2P D-Bus Listener Service:**
   - Handles `org.qemu.Display1.Listener` methods (`Scanout`, `Update`).
   - Copies pixel buffers into a thread-safe `Arc<Mutex<Framebuffer>>` and calls `drawing_area.queue_draw()`.

### Guest Agent Architecture (`winbridge-agent`)
1. **VirtIO-Serial Worker Thread:**
   - Synchronously reads and frames messages from `\\.\Global\org.winbridge.0`.
   - Dispatches requests to the appropriate Win32 subsystem.
2. **Hidden Message Window Thread:**
   - Creates a message-only HWND (`HWND_MESSAGE`) and registers with `AddClipboardFormatListener`.
   - Dispatches `WM_CLIPBOARDUPDATE` messages while checking suppression flags to eliminate recursive echo loops.
3. **Display & DPI Executor:**
   - Calls Win32 `ChangeDisplaySettingsExW` and CCD API `DisplayConfigSetDeviceInfo` on demand.
4. **Drop Receiver:**
   - Maps host drop coordinates into guest pixels and hit-tests the root window. Files go directly to the Desktop or an Explorer filesystem folder when applicable; other applications receive a Shell `CF_HDROP` OLE drag staged under Downloads.
   - Uses short-lived single-threaded apartments for `IShellWindows` lookup and OLE drag-and-drop. Mouse release is injected on every drag exit path; rejected OLE drops remain in the staging directory. Existing files are preserved with numeric suffixes. Native text drops into guest applications and directory drops into guest applications are not implemented (guest-to-host drags are described below).

---

## Design Rules

1. **Input capture is scoped by the pointer, strictly.** The viewer captures keyboard and mouse only while the window is active and the pointer is over the VM canvas (`EventControllerMotion` enter/leave on the drawing area). The title bar, header bar, areas outside the window, and other focused windows are never captured: no grab, no shortcut inhibition, no key forwarding, and held guest keys and buttons are released on leaving. No toggle key is ever required to capture; Ctrl+Alt+G and a Right Ctrl tap only release by hand (see below). The active X11 keyboard grab / Wayland shortcut inhibition is opt-in (`--grab-keyboard`, `grab_keyboard` in `config.toml`), since it also takes keys from other applications' input methods.
2. **Keyboards and mice are never passed through** over USB, with no override.
3. **The user owns the VM lifecycle.** The viewer executes the configured launch script unchanged, shuts down only through ACPI, and never kills QEMU or the script.
4. **Guest state is detected, not assumed.** The viewer polls QEMU in the background; a guest shut down from the inside, or a QEMU that exited, is shown in the header and lets the window close without a shutdown dialog.

## Input Pipeline, Capture and Game Mode

**Ordered pipeline (`input.rs`).** Every pointer and key event for the guest is pushed onto one `InputQueue` per D-Bus connection, drained by one Tokio task that awaits each call before the next. Motion, buttons, scroll notches, keys, the capture-release events and the guest-drag probe's nudge all use it. Earlier versions sent each event from its own `tokio::spawn`; tasks on the multi-threaded runtime are unordered and QEMU's button events carry no position, so a press could overtake the position event before it and the click landed where the pointer had been a moment earlier (a right click on the desktop hit the Recycle Bin icon). Now each button press and release is queued as `Abs(x, y)` of the GTK event followed by the button. Only motion is coalesced (adjacent absolute positions keep the last, adjacent relative deltas add up). Buttons and keys are never merged or dropped. If the queue backs up past 1024 entries, absolute positions that a later event supersedes are dropped, except one directly in front of a button. D-Bus call failures are logged once per kind; each call has a 2 s timeout so one wedged call cannot stall the queue.

**Capture state machine (`capture.rs`, `router.rs`).** `CaptureState` holds window-active, pointer-inside, blocked (shutdown, stopped VM, transfer, power dialog), the manual-release override and the game-mode switch; the mode is `Off`, `Absolute` or `Relative`. `InputRouter::apply` is the only place the mode changes on the guest side: entering `Off` releases held keys and buttons, drops the keyboard grab and shows the Linux pointer; entering `Absolute` takes the keyboard grab (if enabled), moves the guest pointer under the Linux pointer and shows only the guest cursor (hotspot included, re-applied on every capture so no stale shape survives). One exception on pointer *leave*: a held left button stays down until its physical release, because a guest-to-host drag-and-drop needs it while the pointer is outside; focus loss, a transfer, or the manual release still release it at once. The manual release is an override of `Off` that ends when the pointer leaves the canvas (so it ends on re-entry), when either shortcut is used again, or on a canvas click; that click is swallowed together with its release.

**Release shortcuts (`keyfilter.rs`).** `KeyFilter` sits in front of the guest keyboard. Ctrl+Alt+G (matched by keyval, so layout-aware, with GDK's Ctrl+Alt modifier state) is swallowed and releases any Ctrl/Alt it had forwarded; because those keys were already forwarded, the guest sees a short Ctrl/Alt pulse before the release. A Right Ctrl press is held back; the next key forwards the held press first, a release without another key is a tap, which is swallowed and toggles the release. Everything runs on the host, independent of the guest and of `--grab-keyboard`, and also while a transfer blocks input.

**Mouse buttons (`buttons.rs`).** Button presses and releases come from one `EventControllerLegacy` on the canvas, not from GTK gestures (a gesture follows one button at a time and lost the first button's release once a second was pressed). `HeldButtons` records each forwarded press and yields exactly one release per press in any order; a release of a button the guest does not hold (a swallowed recapture click) is dropped.

**Absolute position rounding (`abs_wire_value` in `input.rs`).** Measured with `GetCursorPos` in the guest, the cursor lands exactly one pixel below the `SetAbsPosition` value on both axes. This is inferred, not read from QEMU's source: QEMU scales into the tablet's 0..0x7fff range over the full extent and the guest's driver floors when scaling back. Every absolute position is therefore sent as `pixel + 1` in the single conversion `InputRouter::wire_pos`, which also serves button prefixes and the drag probe's nudge. QEMU rejects `x >= extent` (`Invalid mouse position`, verified on the VM), so the value is capped at `extent - 1`: pixels `0..=extent-2` are exact and the last column and row are unreachable through QEMU's absolute pointer (the last pixel reads `extent - 2`). Widget coordinates also get a 0.01 px epsilon before flooring, because GTK's single-precision transforms can deliver 97.99999 for pixel 98. The window corners are intercepted by the compositor's resize grips unless the window is maximized or fullscreen.

**Guest agent liveness.** The viewer pings the agent once a second (`Ping`/`Pong`); any frame from the agent counts as an answer. The header shows the shortcut hint only while nothing was heard for more than 5 s (not during transfers, which keep the channel busy) or while the serial channel is down with the display connected. When the agent answers again after such a silence, the viewer resends the current canvas resolution and DPI (through the usual resize debounce) and queries the status. The header label is ellipsized and width-capped so no message can resize the window; transient notices (for example a refused game mode) show a short text for about 8 s with the full text in the tooltip.

**Game mode (relative mouse).** Off by default; toggled in the header. In `Relative` mode the Linux pointer is hidden, `RelativeTracker` turns motion events into deltas in guest pixels (with sub-pixel carry) and the pointer is warped to the canvas centre with `XWarpPointer` once it is more than an eighth of the canvas away; the warp's own motion event is recognised and ignored. Buttons are sent without the absolute position prefix. Limits: X11 only (GTK4 offers no pointer lock under Wayland, so the toggle is disabled there); no grab, so a very fast flick can leave the canvas and release the capture; the guest must have a relative pointing device, because QEMU's D-Bus Mouse interface documents that `RelMotion` fails while `IsAbsolute` is true and `SetAbsPosition` fails while it is false. With only a `usb-tablet`, games that depend on raw relative input cannot work. Adding `-device usb-mouse` gives the guest a relative device; which of the two QEMU reports through `IsAbsolute` when both exist was not verified. When QEMU refuses `RelMotion`, game mode turns itself off and the header names the reason.

---

## Clipboard and Guest-to-Host Drag-and-Drop

Both reuse the file-stream mechanism of the protocol (`winbridge-proto::files`: `scan`, `send_stream`, `Receiver`), so path sanitizing, collision numbering and size limits are written once and unit-tested.

- **Clipboard.** `winbridge-proto::clip` holds `ClipBundle` (text, HTML, RTF, PNG, BMP, files), the sender (`ClipBundle::send`: offer and non-file items, files are only listed as `FileList`; `clip::send_files` answers a `ClipFilesRequest`) and the receiver (`Assembler`: applies a snapshot only when every offered non-file format arrived; `handle_files` reassembles the file stream of a request). Host: `clipboard.rs` reads the GTK clipboard (`read_text`, MIME reads for HTML/RTF, `read_texture`, `FileList`) 150 ms after the last change, sends the offer from a blocking task and remembers `(clip_id, paths)` of the newest local offer. Agent: `clipboard.rs` reads and writes the Windows formats (`CF_UNICODETEXT`, `HTML Format` with CF_HTML offsets, `Rich Text Format`, registered `PNG`, `CF_DIB`, `CF_HDROP` with `Preferred DropEffect`). Echo prevention is structural, not hash based: the agent tags its own writes with the format `WinBridge.Origin`; the host ignores changes while `Clipboard::is_local()`, which also covers content set through our own lazy provider. **Files move at paste, not at copy.** Host copy, guest paste: the guest sends `ClipFilesRequest { clip_id }`; the host streams that snapshot's files if `clip_id` is its newest offer. Guest copy, host paste: `apply` puts text, HTML, RTF and image providers on the clipboard as before; when the offer listed files it publishes only a `LazyFilesProvider` (a `ContentProvider` subclass advertising `gdk::FileList`, `text/uri-list` and `x-special/gnome-copied-files`, with no storable formats so a clipboard manager cannot trigger a transfer). The first request for `text/uri-list` or `x-special/gnome-copied-files` sends `ClipFilesRequest`, and the answer arrives through `write_mime_type_future` without blocking the main loop; concurrent requests share one fetch, later requests for the same `clip_id` reuse the staged paths, and a new offer discards the old state. The synchronous `FileList` value is only answered once the files are staged; before that it starts the fetch and reports an error. Cancel, timeout and stream errors fail the waiting requests and call `Assembler::cancel_files`.
- **Guest-to-host drag.** The host tracks the left button; when the pointer leaves the canvas with it held, it sends `GuestDragProbe` at the clamped edge position. The agent (`dragout.rs`) shows a 120 px, alpha-1, topmost, no-activate window under the guest cursor for 700 ms; it is an OLE `IDropTarget` registered with `RegisterDragDrop`. A running drag calls `DragEnter` with its `IDataObject`; the agent reads `CF_HDROP`/`CF_UNICODETEXT`, answers `GuestDragOffer` and keeps accepting with `DROPEFFECT_COPY` (never move). The host then starts a GTK drag (`gdk::Drag::begin`) with a `GuestDragProvider`: a `ContentProvider` subclass whose `write_mime_type_future` sends `GuestDragFetch` only when a Linux application asks for `text/uri-list` or `x-special/gnome-copied-files`, waits for the staged files and returns their `file://` URIs. When the GTK drag ends, the host releases the guest's left button itself and sends `GuestDragEnd`. Staged files live in `$XDG_CACHE_HOME/winbridge/drag-in` and are purged at startup.
- **Transfer progress and input lock.** `transfer.rs` is a small hub shared by Tokio threads and the GTK main thread: each running transfer has a label, direction, bytes done/total, a last-progress time and an optional abort hook. Every change schedules one coalesced refresh on the GTK main loop. While at least one transfer is active, `ui.rs` shows a bar (label with percentage, `ProgressBar`, **Cancel**) in a `gtk4::Overlay` at the top of the canvas, calls `release_input()`, ignores key forwarding and makes the canvas insensitive (it keeps redrawing guest frames). Sensitivity is `!shutting_down && !vm_stopped && !input_locked`, so the shutdown and VM-stopped flows keep working; when the lock ends, keyboard capture is re-evaluated from the pointer position as before. A transfer without progress for 30 s is ended by a watchdog task and input is restored; **Cancel** ends all transfers. Cancel and timeout run the owner's abort hook, and failures (including a failed `DropResult`) are shown in an error dialog. The transfer kinds feed the hub as follows: a host-to-guest drop counts bytes handed to the bounded link queue (so it paces with the link) and ends with the guest's last `DropResult` (one is sent per dropped file; the first failure ends it); a host copy is streamed by `serial_client::send_clip_files` on a blocking task when the guest sends `ClipFilesRequest` for the newest remembered offer (other ids are logged and ignored); the guest-to-host drag fetch and the lazy clipboard fetch count received bytes.
- **Limits.** Files are never transferred at copy time, so a paste (or a drop) waits for the whole stream, and nothing hands an application a path while the files are still streaming. A cancel or a stall abandons the host side only: there is no protocol message that tells the sender to stop, so the other side may keep streaming until its stream ends (the receiver discards it). Only on-disk files and text can leave the guest by drag. The probe relies on the guest source polling the window under the cursor; the 700 ms window and the X11/Wayland behaviour of `gdk::Drag::begin` from a non-event callback are not verified.

## 4. CD/DVD Media and USB Device Hotplug

The header-bar **Devices** popover controls QEMU through the QMP Unix socket (`/tmp/winbridge-qmp.sock` by default). Every command uses its own short-lived connection (greeting/capabilities handshake, one command, close), because QEMU serves a single QMP client at a time. Connection attempts retry for up to 3 seconds, and a command is never re-sent once it was written. QMP and host permission errors are displayed in the popover.

`blockdev-change-medium` inserts ISO images or host `/dev/srN` optical drives into the read-only `cdrom0` IDE CD-ROM; `eject` removes the medium. Host drive entries appear only when a corresponding device node exists.

USB passthrough uses `device_add`/`device_del` on the runtime-created `usb-host` devices. Unless `--usb-bus <id>` names a controller, `device_add` omits the `bus` argument and QEMU attaches the device to its default USB bus; no controller id is assumed. The viewer polls `/sys/bus/usb/devices` every 1.5 seconds, excluding hubs and root hubs, and updates its list while the popover is open or closed. Hotplug therefore works in both directions: checking a row attaches a device to Windows, unchecking it returns it to the host, unplugging cleans up the guest device, and reconnecting a still-checked vendor:product attaches it again. A USB device is detached from its Linux host driver while passed through.

Keyboards and mice are never offered for passthrough, and there is no override. A device is filtered out when any of its interfaces has class 03 (HID) with protocol 01 (keyboard) or 02 (mouse), so composite devices such as gaming keyboards and wireless-mouse dongles are excluded too. The filter applies to enumeration, and `device_add` re-checks sysfs immediately before attaching, so a device that re-enumerated as an input device is refused with a message. This prevents passing through the device that controls the host.

Access: the host user needs read/write access to `/dev/bus/usb/*`. `scripts/install-host-udev.sh` installs `70-winbridge-usb.rules`, which grants the active seat user access to USB device nodes (`--dry-run` shows the commands; `--uninstall` removes the rule). The rule skips any device with a keyboard or mouse interface, based on `ID_USB_INTERFACES` on the USB device node. It does not cover `/dev/input/*` or `/dev/hidraw*`, so OS-level protection of input devices is partial: the viewer filter is the primary guard, and the udev rule only limits what the USB node path grants. When `device_add` fails with a permission error, the viewer points to this script.

### Lifecycle, launch script and tray

The viewer's lifetime is the VM's lifetime. How the VM starts is the user's business: WinBridge does not build QEMU arguments.

- **Launch script.** With `--launch-script <PATH>` (or `launch_script` in `config.toml`), the viewer runs that script once at startup as a child in its own process group, with the script's directory as working directory. `launch_args` are passed through unchanged. Output is appended to `$XDG_STATE_HOME/winbridge/launch.log` (default `~/.local/state/winbridge/launch.log`). The script receives the resolved endpoints as `WINBRIDGE_SOCK`, `WINBRIDGE_DBUS_ADDR` and `WINBRIDGE_QMP_SOCK` and is expected to start QEMU with them. Without a launch script the viewer only attaches to an already running VM.
- **Exit handling.** A script that exits non-zero, or exits before QEMU was reachable, shows an error with the log path and keeps the window open. A zero exit after the VM was reachable means the VM is gone and the viewer quits.
- **Config file.** `$XDG_CONFIG_HOME/winbridge/config.toml` (default `~/.config/winbridge/config.toml`) accepts `launch_script`, `launch_args`, `socket`, `dbus_addr`, `qmp_socket` and `usb_bus`. Command-line options override the file; unknown keys are an error.
- **Close = shut down.** Closing the window asks: **Shut down VM** (default), **Minimize to tray**, **Cancel**. There is no "close viewer only". Shutdown sends the QMP `system_powerdown` (ACPI, never forced), then polls `query-status` once a second. The viewer quits when the status is `shutdown` or QMP is unreachable twice in a row, and the launch script (if any) has exited. After 120 s without power-off it asks again (keep waiting / minimize); it never kills QEMU or the script. If QMP cannot be reached at close time, **Quit anyway** is offered only when QEMU is confirmed gone.
- **Tray.** A StatusNotifierItem (via `ksni`) offers Show window, Shut down VM (same flow as above) and a status line; left click toggles the window. **Minimize to tray** hides the window and holds the application open. If no `org.kde.StatusNotifierWatcher` is on the session bus (GNOME needs the AppIndicator/KStatusNotifierItem extension), the window is only minimized and a one-time notice explains why. If the watcher disappears while the window is hidden, the window is shown again.
- **QMP is single-client.** QEMU serves one QMP connection at a time, so the viewer opens a short-lived connection per command; `./vm shot`-style tools and the viewer can coexist.
- **Not implemented.** Profile-based launching (for example starting a virt-manager/libvirt profile) is a deliberately deferred idea.

---

## 5. DPI Synchronization & Limitations

### Implementation
- The GTK4 viewer monitors window/monitor scale factor changes (`widget.scale_factor()`, e.g. 1.0x -> 100%, 1.25x -> 125%, 1.5x -> 150%, 2.0x -> 200%).
- The host transmits `Message::SetDpiScale { scale_percent }` to the agent.
- The agent updates scaling using the Windows Connecting and Configuring Displays (CCD) API:
  - Retrieves active path and source ID via `QueryDisplayConfig`.
  - Queries relative DPI boundaries using `DisplayConfigGetDeviceInfo` with type `-3` (`DISPLAYCONFIG_DEVICE_INFO_GET_DPI_SCALE`).
  - Calculates relative offset from baseline: `scaleRel = target_index - baseline_index`.
  - Applies scaling dynamically via `DisplayConfigSetDeviceInfo` with type `-4` (`DISPLAYCONFIG_DEVICE_INFO_SET_DPI_SCALE`).
  - Writes registry keys `AppliedDPI` and `LogPixels` under `HKCU\Control Panel\Desktop\WindowMetrics` and broadcasts `WM_SETTINGCHANGE`.

### Honest Technical Limitations
1. **Discrete Hardware Tiers:** Windows CCD scaling only accepts standard discrete scaling steps: `[100%, 125%, 150%, 175%, 200%, 225%, 250%, 300%, 350%, 400%, 450%, 500%]`. Arbitrary fractional scales from Linux (e.g., 115% or 133%) are snapped to the nearest supported hardware tier.
2. **Legacy GDI Applications:** While the Windows 11 shell, Taskbar, Settings, Explorer, and modern Per-Monitor V2 applications resize instantaneously without sign-out, legacy GDI applications that do not declare DPI awareness will retain their initial font cache until restarted.
3. **Driver Mode Recognition:** In virtual machines using `virtio-gpu`, arbitrary non-standard resolutions must be permitted by the driver; QEMU should be configured with `edid=on` or appropriate resolution caps to allow smooth transitions.

---

## 6. Injection (IFEO) and GPU path

The graphics hook is the planned route for GPU-accelerated Windows applications. Its guest-side delivery is the Image File Execution Options (IFEO) mechanism in `crates/winbridge-agent/src/gfx_inject.rs`. **Status: the hook DLL `winbridge_gfx_hook.dll` is not built yet, so nothing is injected today.** With no DLL beside the agent, `run_debugger` starts each target without the hook.

**The injection is off by default.** The agent only touches IFEO when started with `--enable-gfx`; a plain start never writes registry entries (`--uninstall-gfx` removes whatever an earlier run created). It has never run on a real guest, and with no hook DLL it cannot accelerate anything, so enabling it today only adds risk.

### Allow-list
- The allow-list is `%ProgramData%\WinBridge\gfx-apps.txt`: one bare `.exe` name per line, `#` comments, no paths or wildcards. It is re-read about every 5 seconds.
- It is created with defaults only when missing, so an existing install keeps its list.
- Defaults: the shell processes (`explorer.exe`, `SystemSettings.exe`, `ApplicationFrameHost.exe`, `ShellExperienceHost.exe`, `StartMenuExperienceHost.exe`, `SearchHost.exe`, `TextInputHost.exe`) and `dwm.exe`. The test programs `vkcube.exe` and `dxvk-test.exe` are commented out.
- `dwm.exe` is the Desktop Window Manager. A fault in its hook can black-screen the session. It is in the default list and carries a warning comment in the template.

### Arming: VM gate
Redirects are armed only when `gate_open()` holds: not safe mode, and `is_virtual_machine()` is true. The gate is evaluated on every launch and is not cached.

`is_virtual_machine()` evaluates five signals and needs at least 3 to fire:
1. **cpuid-hypervisor**: CPUID.1:ECX bit 31 is set, and the hypervisor vendor leaf (0x40000000) reads `KVMKVMKVM` or `TCGTCGTCGTCG`.
2. **firmware**: the raw SMBIOS table (`RSMB`) or the `HARDWARE\DESCRIPTION\System\BIOS` strings contain `QEMU`, `Standard PC`, `SeaBIOS`, `EDK II`, or `OVMF` (case-insensitive).
3. **virtio-pci**: a `VEN_1AF4` device is enumerated under `Enum\PCI`.
4. **winbridge-port**: `\\.\Global\org.winbridge.0` exists. Access denied or a sharing violation also counts as present, since the port exists but is held elsewhere.
5. **no-display-gpu**: no `VEN_10DE`, `VEN_1002`, or `VEN_8086` device has the `Display` class.

The hypervisor bit alone is not enough: Hyper-V and VBS set it on bare metal, so the cpuid-hypervisor signal also needs the KVM or TCG vendor leaf. `EDK II` also appears in some physical firmware, so the firmware signal is one of five and never decides alone. A signal that cannot be read does not fire, which keeps the hook disarmed when detection fails.

### Bare-metal safety
- When the gate is closed, the agent removes every WinBridge redirect it wrote, in both registry views, and clears the recovery record. This is best-effort: a redirect that cannot be removed (for example, from a non-elevated agent) is logged, and the launch continues.
- Every launch starts the target with `DEBUG_ONLY_THIS_PROCESS` and detaches it, armed or not. Bare metal therefore never blocks a program from starting, even if a redirect survives.
- Backstop: if the debugger-created process turns out to be this agent, or its image cannot be checked, the start is terminated and the launch fails with exit code 1. This fires only when the bypass fails, and it stops a recursion that would otherwise be unbounded.

### Injection sequence
1. Windows starts `winbridge-agent.exe --ifeo-debug <target command line>` because of the IFEO `Debugger` value. `main` dispatches this before flag parsing, so target arguments are never read as agent options.
2. The agent sets `DebugSetProcessKillOnExit(FALSE)` before the target is created. If the agent faults, the target is detached and not killed.
3. The agent creates the target with `DEBUG_ONLY_THIS_PROCESS`. It writes nothing to the registry for this start. [INFERENCE] The flag keeps the loader from consulting the target's IFEO `Debugger` value, so the start does not re-enter the agent. This comes from secondhand summaries of Windows behaviour and must be confirmed on the VM.
4. The agent runs debug events to the initial loader breakpoint. For an armed target it injects the hook with `VirtualAllocEx`, `WriteProcessMemory`, and a remote `LoadLibraryW` thread. It then continues the breakpoint and calls `DebugActiveProcessStop`. An unarmed target is detached at the same point, without injection.
5. The agent waits for the target to exit and returns its real exit code. It holds no lock while waiting, so a session ending during a long-running target is not blocked. A target that ends within 3 seconds with an NTSTATUS error code counts as an early crash for the guard.

The detach step is not yet verified on a VM. The remote thread's create event may still be pending when the debugger detaches. This must be validated on the LTSC VM before the hook ships. The debug-flag bypass is likewise unverified on a VM.

### Crash guard
If a target exits with an NTSTATUS error code within 3 seconds of launch, that is one early crash. Three in a row disable the redirect for that executable, recorded as `disabled` in `%ProgramData%\WinBridge\gfx-guard.txt`. Removing that line re-arms it. A normal exit clears the count.

### Recovery and uninstall
- `HKLM\SOFTWARE\WinBridge\Gfx` has one empty subkey per armed executable. It is written before arming and cleared only after every redirect is removed.
- `winbridge-agent.exe --uninstall-gfx` removes every redirect the agent wrote and clears the record.
- Offline recovery from WinPE: load the `SYSTEM` and `SOFTWARE` hives. For each subkey of `HKLM\SOFTWARE\WinBridge\Gfx`, delete the `Debugger` value under `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\<exe>`. Repeat under `WOW6432Node`.
- When an interactive session ends (WM_ENDSESSION), the agent removes all its redirects and clears the record. A non-interactive service session does not receive this message, so recovery then depends on `--uninstall-gfx` or the offline steps above.


