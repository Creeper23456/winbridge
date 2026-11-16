<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# WinBridge Wire Protocol (Version 1)

This document specifies the wire format and message semantics used over the VirtIO-serial communication channel (`org.winbridge.0`) between `winbridge-host` (Linux) and `winbridge-agent` (Windows 11).

---

## 1. Framing Specification

All communication occurs as discrete, length-prefixed binary frames:

```
+-------------------+----------------------+----------------------------------------+
|  Magic (4 bytes)  | Length (4 bytes, LE) |        Payload (N bytes, Bincode)      |
+-------------------+----------------------+----------------------------------------+
|   'W' 'B' '0' '1' |     u32 (N bytes)    |       Serialized Message Variant       |
+-------------------+----------------------+----------------------------------------+
```

### Fields:
1. **Magic Bytes (4 bytes):** Literal ASCII `WB01` (`0x57, 0x42, 0x30, 0x31`), representing WinBridge Protocol Version 1.
2. **Payload Length (4 bytes):** 32-bit unsigned integer in little-endian byte order indicating the exact length $N$ of the serialized message body.
3. **Payload ($N$ bytes):** Serialized representation of the `Message` enum encoded with `bincode`.
4. **Maximum Frame Size:** Enforced at $64 \text{ MB}$ (`0x04000000` bytes). Frames with length header exceeding this limit are discarded, and the connection is reset.

---

## 2. Message Definitions

### 2.1 Keepalive and Health Checks

#### `Ping`
- **Direction:** Host $\to$ Agent or Agent $\to$ Host
- **Fields:**
  - `seq: u64`: Monotonically increasing sequence number.
  - `timestamp_ms: u64`: UNIX timestamp in milliseconds.

#### `Pong`
- **Direction:** Host $\to$ Agent or Agent $\to$ Host (reply to `Ping`)
- **Fields:**
  - `seq: u64`: Echoes the sequence number of the corresponding `Ping`.
  - `timestamp_ms: u64`: Echoes the original timestamp for RTT calculation.

---

### 2.2 Display Resolution Synchronization

#### `SetResolution`
- **Direction:** Host $\to$ Agent
- **Description:** Sent when the GTK viewer window is resized by the user.
- **Fields:**
  - `width: u32`: Target horizontal resolution in pixels.
  - `height: u32`: Target vertical resolution in pixels.

#### `ResolutionResult`
- **Direction:** Agent $\to$ Host
- **Description:** Reports the result of invoking `ChangeDisplaySettingsExW`.
- **Fields:**
  - `width: u32`: The width that was applied or requested.
  - `height: u32`: The height that was applied or requested.
  - `success: bool`: `true` if display settings were accepted; `false` otherwise.
  - `error: Option<String>`: Diagnostic error message if failed.

---

### 2.3 Per-Monitor DPI Synchronization

#### `SetDpiScale`
- **Direction:** Host $\to$ Agent
- **Description:** Sent when the GTK window is moved across monitors with different DPI or when the host desktop scale factor changes.
- **Fields:**
  - `scale_percent: u32`: Integer percentage (e.g. `100`, `125`, `150`, `175`, `200`, `250`).

#### `DpiScaleResult`
- **Direction:** Agent $\to$ Host
- **Description:** Reports the result of applying display scaling via Windows CCD API (`DisplayConfigSetDeviceInfo`).
- **Fields:**
  - `scale_percent: u32`: Applied scale percentage.
  - `success: bool`: `true` if successful; `false` otherwise.
  - `error: Option<String>`: Diagnostic error message if failed.

### 2.3.1 Refresh Rate

#### `SetRefreshRate`
- **Direction:** Host $\to$ Agent
- **Description:** The refresh rate the virtual screen should run at. The viewer sends the lowest rate among the monitors connected to the Linux host, at most 60 Hz, when the agent first answers and again whenever that value changes (monitor plugged in, removed or switched to another mode). A host whose monitors report no rate sends nothing.
- **Fields:**
  - `millihertz: u32`: Rate in millihertz (`60000` = 60 Hz, `59940` = 59.94 Hz).

#### `RefreshRateResult`
- **Direction:** Agent $\to$ Host
- **Description:** Answer to `SetRefreshRate`. The agent changes the rate of the active display path through `SetDisplayConfig`, validates the configuration first and then reads the rate back. A driver can accept a configuration and keep its own rate: the answer is a failure then, with the rate it reports in `error`.
- **Fields:**
  - `millihertz: u32`: The requested rate.
  - `success: bool`: `true` only if the display configuration reports the requested rate afterwards.
  - `error: Option<String>`: Diagnostic message if failed.

Measured (2026-11-16, Windows 11 LTSC guest, `Red Hat VirtIO GPU DOD controller` 100.101.104.28500): the display path reports 64 Hz, `SetDisplayConfig` validates and applies a 60 Hz request without an error, and the path still reports 64 Hz afterwards, so the answer is a failure on this driver. QEMU's `virtio-gpu-pci` has no refresh-rate property, and its D-Bus `SetUIInfo` carries none.

---

### 2.4 Status Query

#### `QueryStatus`
- **Direction:** Host $\to$ Agent
- **Description:** Solicits the agent's current operating metrics and environment information.

#### `StatusReply`
- **Direction:** Agent $\to$ Host
- **Fields:**
  - `current_width: u32`: Active screen width in pixels.
  - `current_height: u32`: Active screen height in pixels.
  - `current_dpi: u32`: Current active DPI percentage (e.g., `100`).
  - `os_description: String`: Guest OS build information.
  - The agent appends the capability token described in section 3.2 to `os_description` (for example `Windows 11 x64 [wb-shm=1:0badcafe]`). Hosts that do not know it treat the whole string as free text.

---

### 2.5 Bidirectional Clipboard Exchange

One snapshot of everything the clipboard holds. The sender announces it and sends each non-file format. Copied files are **not** sent with the snapshot: the offer only lists `FileList` and is followed by a `ClipFilesManifest` (names and sizes); the receiver fetches the files with `ClipFilesRequest` at the moment the user pastes. The receiver applies the snapshot once offer, manifest and all non-file formats arrived; a failed or oversized part drops the snapshot. A new offer replaces an unfinished one and invalidates the files of the previous one.

#### `ClipboardOffer`
- **Direction:** Bidirectional
- **Fields:**
  - `clip_id: u32`: Snapshot id; also the `stream_id` of its file stream. The agent draws ids from one counter shared with guest drag-and-drop, so they never collide.
  - `formats: Vec<ClipboardFormat>`: What follows: `Text` (UTF-8), `Html` (UTF-8 document, without the Windows CF_HTML header), `Rtf`, `Png` (PNG file), `Bmp` (BMP file, a DIB plus file header), `FileList` (files were copied; they are fetched on paste with `ClipFilesRequest`).

#### `ClipboardItem`
- **Direction:** Bidirectional
- **Fields:** `clip_id: u32`, `format: ClipboardFormat`, `data: Vec<u8>` (at most 48 MiB; larger items are skipped).

#### `ClipFilesRequest`
- **Direction:** Bidirectional (sent by the side that is pasting)
- **Fields:** `clip_id: u32`. The offering side answers with a file stream (section 2.7) whose `stream_id` is `clip_id`. A request for an id other than the newest offer is ignored. There is no message that cancels a running stream; the receiver discards what it no longer wants.

#### `ClipFilesManifest`
- **Direction:** Bidirectional (sent by the offering side)
- **Fields:** `clip_id: u32`, `entries: Vec<ManifestEntry>` where `ManifestEntry { rel_path: String, is_dir: bool, size: u64 }` (`rel_path` uses `/` separators and starts with the top-level name, exactly as the later `FileEntry` messages). Sent right after a `ClipboardOffer` that lists `FileList`, before the `ClipboardItem`s. It lets the receiver present the files (names, sizes, folders) without transferring anything: the guest agent publishes them as virtual files (`FileGroupDescriptorW` + `FileContents` streams on the OLE clipboard), so Explorer's paste-state probing never causes a transfer. A snapshot that offers files is complete only after offer, manifest and items have arrived. The first read of a file's content sends `ClipFilesRequest`; the bytes of that stream are served to the reading application as they arrive.

#### Rules
- Copied files are offered alone (no text or picture of them). Without files, every available format is sent.
- An image is sent as `Png` (keeps alpha) and as a `Bmp` for legacy consumers; the agent stores the `Png` in the registered `PNG` format and the `Bmp` as `CF_DIB`. From Windows the registered `PNG` is preferred, otherwise `CF_DIB` is wrapped into a `Bmp`.
- The agent converts `Html` to and from the `HTML Format` header (`StartHTML`..`EndHTML` offsets); RTF uses the registered `Rich Text Format`.
- Echo-loop prevention: the agent tags content it writes with the private format `WinBridge.Origin` and ignores such content; the host ignores changes while `Clipboard::is_local()` is true, which is the case for content the viewer set itself.
- Files are staged on the receiver (host: `$XDG_CACHE_HOME/winbridge/clip-in`, guest: `%TEMP%\WinBridge\clip`) and removed when the next snapshot arrives or the program starts.

---

### 2.6 Drag-and-Drop Sharing

File and text drops include guest-screen pixel coordinates, not GTK widget coordinates. The agent hit-tests the window under that point with `WindowFromPoint` and `GetAncestor(GA_ROOT)`:

- A desktop window selects the user's known Desktop folder.
- An Explorer window selects its current location when `IShellWindows` identifies a filesystem directory.
- Any other window selects `%USERPROFILE%\Downloads\WinBridge` for staging. After all files arrive, the agent starts a real OLE drag with a Shell `CF_HDROP` data object and injects mouse input at the original point. The application must accept OLE file drops for this to succeed; rejected or failed drops remain in the staging directory.

Every destination file is created with create-new semantics. An existing name is never overwritten; the agent appends ` (1)`, ` (2)`, and so on before the extension. Drops are regular files, not recursive directory transfers.

#### `DropBegin`
- **Direction:** Host $\to$ Agent
- **Description:** Starts one multi-file operation and fixes its destination before transfer begins.
- **Fields:**
  - `transfer_id: u32`: Unique identifier shared with each file's `operation_id`.
  - `x: u32`, `y: u32`: Guest-screen pixel coordinates of the drop.
  - `file_count: u32`: Number of files in this operation.

#### `DropFileStart`
- **Direction:** Host $\to$ Agent
- **Description:** Initiates one file transfer within a preceding `DropBegin` operation.
- **Fields:**
  - `transfer_id: u32`: Unique identifier for this file transfer.
  - `operation_id: u32`: The `transfer_id` from `DropBegin`.
  - `filename: String`: Basename of the file (sanitized, path components stripped).
  - `total_bytes: u64`: Total file size in bytes.

#### `DropFileChunk`
- **Direction:** Host $\to$ Agent
- **Description:** Transmits a contiguous block of file content.
- **Fields:**
  - `transfer_id: u32`: Matching session identifier.
  - `offset: u64`: Byte offset within the target file.
  - `data: Vec<u8>`: Chunk payload (typically 64 KB – 1 MB chunks).

#### `DropFileFinish`
- **Direction:** Host $\to$ Agent
- **Description:** Signals completion of file streaming.
- **Fields:**
  - `transfer_id: u32`: Matching session identifier.
  - `success: bool`: Whether all chunks were streamed without error.
  - `error: Option<String>`: Failure details if cancelled.

#### `DropText`
- **Direction:** Host $\to$ Agent
- **Description:** Saves dropped UTF-8 text in the staging directory and reports its path. Coordinates are transported for consistency, but text is not currently delivered as a native application text drop.
- **Fields:**
  - `text: String`: Dropped UTF-8 text string.
  - `x: u32`, `y: u32`: Guest-screen pixel coordinates of the drop (currently unused).

#### `DropResult`
- **Direction:** Agent $\to$ Host
- **Description:** Acknowledgment for a drop operation. The agent answers every `DropFileFinish` (a non-final file with `success: true` and a "waiting for remaining files" message, the last one with the saved path) and every `DropText`; a failed step answers `success: false`. The host ends its progress bar after as many successful results as files were dropped, or at the first failure, which it shows to the user.
- **Fields:**
  - `success: bool`: `true` if saved successfully; `false` otherwise.
  - `message: String`: Human-readable status (e.g. `Saved to C:\Users\User\Downloads\WinBridge\report.pdf`).

---

### 2.7 File Streams

One mechanism moves files and directories in both directions: clipboard files and files dragged out of the guest. A stream is `FilesBegin`, one `FileEntry` per entry (each followed by its `FileChunk` messages), then `FilesEnd`. `rel_path` uses `/` separators and starts with a top-level name; the receiver sanitizes every component (no `..`, no absolute paths, characters invalid on Windows replaced by `_`), numbers colliding top-level names, and writes below one staging directory only. Symbolic links are skipped. Chunks must arrive in order; a size mismatch fails the stream.

#### `FilesBegin`
- **Fields:** `stream_id: u32`, `entries: u32`, `total_bytes: u64`. A stream above the receiver's limit is rejected (clipboard and guest drag: 8 GiB).

#### `FileEntry`
- **Fields:** `stream_id: u32`, `index: u32`, `rel_path: String`, `is_dir: bool`, `size: u64`.

#### `FileChunk`
- **Fields:** `stream_id: u32`, `index: u32`, `offset: u64`, `data: Vec<u8>` (256 KiB chunks).

#### `FilesEnd`
- **Fields:** `stream_id: u32`, `success: bool`, `error: Option<String>`. `success: false` without a preceding `FilesBegin` tells the receiver that the sender could not deliver at all.

---

### 2.8 Guest-to-Host Drag-and-Drop

A drag that starts in Windows and ends on a Linux application. The host notices that the pointer left the canvas with the left button held and probes the guest; the agent places a nearly invisible `IDropTarget` window under the guest cursor for 700 ms and, if an OLE drag is running, reads it. Only files that exist on disk (`CF_HDROP`) and text (`CF_UNICODETEXT`) can be taken; virtual files have no path to read. The files are not sent with the offer: the host fetches them when a Linux application accepts the drop, so they are staged only when needed and the Linux target decides where they finally go.

#### `GuestDragProbe`
- **Direction:** Host $\to$ Agent
- **Fields:** `x: u32`, `y: u32` (guest pixels at the screen edge where the pointer left).

#### `GuestDragOffer`
- **Direction:** Agent $\to$ Host
- **Fields:** `drag_id: u32`, `names: Vec<String>` (top-level file names, empty for a text drag), `total_bytes: u64`, `text: Option<String>`.

#### `GuestDragNone`
- **Direction:** Agent $\to$ Host. No drag was found within the probe time.

#### `GuestDragFetch`
- **Direction:** Host $\to$ Agent
- **Fields:** `drag_id: u32`. The agent answers with a file stream whose `stream_id` equals `drag_id`. The agent keeps the last offered drag so the fetch can follow the end of the guest-side drag.

#### `GuestDragEnd`
- **Direction:** Host $\to$ Agent
- **Fields:** `drag_id: u32`. The host drag finished (dropped or cancelled); the agent hides its probe window. The host releases the guest's left button itself over the display channel.

---

## 3. Shared-memory transport (optional)

Design and measurements: `docs/IVSHMEM-TRANSPORT.md`. This section is the wire-level contract. The transport is **opt-in** (`shm = true` in the viewer's `config.toml` or `--shm`; the launch script adds the device with `WINBRIDGE_SHM=1`) and **off by default**: without it, every message in section 2 travels over virtio-serial exactly as described there, byte for byte.

### 3.1 What moves

Every `Message` keeps its encoding (section 1 frame: magic, length, `bincode` payload). On the shared-memory transport the **whole frame** is the payload of one ring record, so a record decodes with the same code as a serial frame (`winbridge_proto::decode_frame`, which requires the record to hold exactly one frame). The virtio-serial channel stays in use for the handshake, for the four negotiation messages below, for wake-up kicks and as the fallback.

### 3.2 Capability gating

`bincode` is positional: a message variant an agent does not know cannot be decoded by it. The four `Shm*` variants were therefore **appended at the end of `Message`** (the order of existing variants must never change), and the host sends `ShmOffer` **only to an agent that announced the capability**. The agent announces it in the free-text `os_description` of its `StatusReply` (the first reply after it starts, and every reply to `QueryStatus`) as a trailing token:

```
Windows 11 x64 [wb-shm=1:0badcafe]
```

`1` is the shared-memory protocol generation (`caps::SHM_PROTOCOL`), `0badcafe` a random id of the agent process. An old agent never emits the token and an old host only displays the text, so mixed versions fall back to serial without any new message. A host that sees a different id in a later reply knows the agent restarted and offers again. (`winbridge_proto::caps::announce` / `parse`.)

### 3.3 Negotiation messages (always on the serial channel)

| Message | Direction | Fields | Meaning |
|---|---|---|---|
| `ShmOffer` | Host $\to$ Agent | `shm_version: u32`, `region_len: u64` | The host created the region (`winbridge-shm` layout version, length of the BAR). The host keeps using serial until `ShmReady`. |
| `ShmReady` | Agent $\to$ Host | `ok: bool`, `reason: String` | `true`: the agent mapped the device (cached, DACL hardened), attached and validated the header and started its lane threads. `false`: `reason` says why; the session stays on serial. |
| `ShmKick` | either | none | Wake the peer's ring consumer: sent by a producer that found the ring empty and the consumer's `sleeping` flag set. Carries no data and is coalesced. |
| `ShmFallback` | either | `reason: String` | The sender leaves the rings (corruption, stalled peer, shutdown, serial reconnect, agent restart); both ends return to serial. |

Sequence: the agent's `StatusReply` carries the token; the host (enabled, backing file present, correct size, owned by the user, not world-writable, not a symlink) maps the file, creates the region (`Region::create`, a **new generation on every serial session**; the file content persists across guest reboots, so an old region is never reused) and sends `ShmOffer`; the agent calls `ivshmem::map_device(region_len)`, `Region::attach` and replies; on `ok` both sides switch their outgoing path to the lanes. The host waits **5 s** for `ShmReady` and then stays on serial (and sends `ShmFallback` so a late agent leaves again). `ok = false`, a missing capability, a missing or unsuitable backing file, or a link that ends later all leave the rest of the session on serial; the next serial session (reconnect) offers again. A repeated `ShmOffer` makes the agent leave its current link and map again.

### 3.4 Region and lanes

One `ivshmem-plain` device, 256 MiB, backing file `/dev/shm/winbridge-ctl` (mode 0600). Header page and ring layout are defined by `winbridge-shm`; the high half of the header capability word holds the session generation.

| Lane id | Direction | Priority | Ring data | Record cap |
|---|---|---|---|---|
| 1 | host $\to$ guest | control | 1 MiB | 256 KiB |
| 2 | guest $\to$ host | control | 1 MiB | 256 KiB |
| 3 | host $\to$ guest | bulk | 64 MiB | 56 MiB |
| 4 | guest $\to$ host | bulk | 64 MiB | 56 MiB |

Record tag `0` is a message frame, tag `1` a **bulk credit** (control lanes only). Bulk records may carry flag `0x0001` (end of a scheduling block); the flag bit `0x8000` is reserved by the ring. Any other tag or flag is an invalid record.

Scheduling (one writer thread per direction): control is strict priority; bulk is sent in blocks of at most 1 MiB with a credit window of 16 blocks in flight. The last record of a block carries the end flag, and the consumer returns one credit per block through the reverse control lane (tag `1`, `winbridge_shm::sched::encode_credit`). A bulk lane that has pending data and credit and was not served for 50 ms goes first (minimum progress), so a transfer and its 30 s no-progress watchdog stay meaningful. A control record therefore waits at most one bulk record.

### 3.5 Lane mapping

`Message::is_bulk()` decides. **Bulk lane:** `ClipboardOffer`, `ClipboardItem`, `ClipFilesManifest` (a clipboard snapshot is one ordered stream; items can be tens of MiB), `FilesBegin`, `FileEntry`, `FileChunk`, `FilesEnd`, `DropBegin`, `DropFileStart`, `DropFileChunk`, `DropFileFinish`. **Control lane:** everything else (`Ping`, `Pong`, resolution, DPI, status, `DropText`, `DropResult`, `ClipFilesRequest`, all `Guest*` drag messages). The four `Shm*` messages never use a lane.

Messages of one stream always share one lane, so their order is kept. **The order between a control message and a bulk message is not guaranteed** on this transport and no code may rely on it (for example a `ClipFilesRequest` can overtake the `ClipboardOffer` it refers to; both sides treat an unknown or stale `clip_id` as an error they already handle). A control message larger than the control record cap (for example a huge `DropText`) is sent over serial.

### 3.6 Validation, limits, fallback

Every byte of the region is untrusted. `winbridge-shm` checks indexes, record lengths and the lane directory (a violation closes the lane: `LaneError::Corrupt`). On top of that each reader copies the record out and checks: tag, flags, exactly one frame, a decodable message, and that the message belongs on this lane (`is_bulk` against the lane class; negotiation messages never). An invalid record is logged and counted; the link ends after **8** of them. Credit returned beyond what is outstanding ends the link.

The link also ends when a lane stays full or credit stays outstanding for **10 s** (the peer stopped reading) or the peer closes it. The end of a link always follows the same steps: the detecting side sends `ShmFallback` over serial, followed by the messages it had queued for the lanes but not yet written, and routes everything over serial from then on. The receiver of `ShmFallback` stops writing, keeps draining the rings for up to 2 s and holds back messages that arrive over serial until the drain is over, so records written before the fallback are dispatched before later serial traffic. Records inside a corrupt ring are lost; a file stream that loses data ends through its existing error path (offset check, `FilesEnd { success: false }`, the 30 s watchdog), it never hangs on the transport. An agent restart or a serial reconnect also leaves the lanes (`ShmFallback`) and the next session negotiates again.

### 3.7 Threads and back-pressure

Per end: one writer thread, one reader thread per consumer lane, a small kick thread. A reader hands decoded messages to the owner's dispatch queue (separate bounded queues for control lane, serial and bulk lane, served in that priority order); a full queue blocks only that lane's reader, so a slow consumer of file data back-pressures the bulk ring and never the serial reader or the control lane. The serial reader runs on its own thread in the agent and never waits for a lane.

One exception keeps keepalives independent of the dispatcher: the agent answers a `Ping` that arrives on the control lane on the lane's own reader thread (`Endpoint::set_fast_path`), because the dispatch loop can be inside a slow disk write for a file chunk (measured without it, build VM, 1 GiB host-to-guest stream: Pong round trip p50 0.19 ms but p99 21.6 ms and max 41.7 ms; the cause is presumably write throttling of the guest disk **[INFERENCE]**). Other control messages wait for the dispatcher, but they are served before any queued bulk message, so they wait for at most the chunk being handled.
