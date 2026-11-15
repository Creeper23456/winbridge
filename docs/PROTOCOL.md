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
