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

#### `ClipboardAnnounce`
- **Direction:** Bidirectional
- **Description:** Informs peer that new clipboard content is available in the specified formats.
- **Fields:**
  - `formats: Vec<ClipboardFormat>`: Available formats (`Text`, `Png`, `Bmp`, `FileList`).

#### `ClipboardRequest`
- **Direction:** Bidirectional
- **Description:** Requests data for a specific announced format.
- **Fields:**
  - `format: ClipboardFormat`: Desired format.

#### `ClipboardPayload`
- **Direction:** Bidirectional
- **Description:** Carries raw data for the requested clipboard format.
- **Fields:**
  - `format: ClipboardFormat`: Format of payload data.
  - `data: Vec<u8>`: Byte array containing formatted data (e.g., UTF-8 string bytes, PNG image bytes).

#### Echo-Loop Prevention Rule:
Both endpoints record the SHA-256 hash or exact byte content of recently applied clipboard data. Incoming clipboard updates matching the locally applied hash are discarded immediately.

---

### 2.6 Drag-and-Drop Sharing

#### `DropFileStart`
- **Direction:** Host $\to$ Agent
- **Description:** Initiates a file transfer dropped on the host GTK window.
- **Fields:**
  - `transfer_id: u32`: Unique identifier for the transfer session.
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
- **Description:** Directly drops text content onto the guest desktop/focused window.
- **Fields:**
  - `text: String`: Dropped UTF-8 text string.

#### `DropResult`
- **Direction:** Agent $\to$ Host
- **Description:** Final acknowledgment for a drop operation.
- **Fields:**
  - `success: bool`: `true` if saved successfully; `false` otherwise.
  - `message: String`: Human-readable status (e.g. `Saved to C:\Users\User\Downloads\WinBridge\report.pdf`).
