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
- **Direct Input Injection:** Key press/release events and absolute mouse motion/clicks are injected via `org.qemu.Display1.Keyboard` and `org.qemu.Display1.Mouse` without requiring synthetic emulated USB devices or window grabs.
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

---

## 4. DPI Synchronization & Limitations

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
