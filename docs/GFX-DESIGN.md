<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# WinBridge GPU Forwarding Design (DXVK over virtio-serial)

Status (2026-11-15): M0, M1 and M2 are done and verified in the Build VM (sections 3.6, 3.7, 10). A Windows ICD forwards a generated subset of Vulkan to a host dispatcher; the Windows loader lists the WinBridge device; the probe triangle is bit-identical to an llvmpipe render; LunarG `vkcube` renders a rotating textured cube into its window. **Not done: M3 (DXVK / D3D11), compression, more than one Vulkan process per VM (port broker), reconnect.** The injector (`gfx_inject.rs`) has never run in a VM; it is off by default and installs nothing while the hook DLL, which does not exist, is missing.

## 1. Decision

- **Build our own forwarding layer.** Generate the Vulkan command serializer (guest) and dispatcher (host) from the Khronos registry `vk.xml`, restricted to the set DXVK 2.x needs.
- **gfxstream: reference only.** It is Apache-2.0 and compatible with GPL-3.0-or-later, but the Windows guest encoder is not in the mirror we checked, and the host side is coupled to Android/emulator infrastructure and shared-memory (hostmem/virtio-gpu) assumptions we do not have on virtio-serial.
- **gpu-mesh: do not depend on it.** The repository has no license file and no Cargo license field, despite a README "MIT" claim. Its transport is TCP-only and its server enables only Vulkan 1.2 device features, so DXVK's requirements are not met.

## 2. Scope

In scope: a guest user-mode Vulkan ICD (no kernel driver, no testsigning) for DXVK-based D3D9/10/11 applications; a host offscreen renderer on the Linux GPU; a byte-stream protocol over a dedicated virtio-serial port; presentation into the guest HWND by readback.

Out of scope: kernel drivers, D3D12, host-side display of guest frames (frames appear only in the guest window), video decode, audio.

## 3. Evidence

### 3.1 Host GPU capability probe (`experiments/vkcaps`)

Built with ash 0.38 (standalone crate, own `[workspace]`), run with `cargo run --release`. The probe creates a Vulkan 1.3 instance without a surface and checks each DXVK-required feature and extension on every physical device.

| Device | apiVersion | driverVersion | DXVK required (71 checks) | Optional DXVK exts | Device exts |
|---|---|---|---|---|---|
| NVIDIA GeForce RTX 3060 Ti (DISCRETE) | 1.3.277 | 550.163.01 (build 0) | 71/71 | 15/24 | 219 |
| llvmpipe (LLVM 19.1.7, CPU) | 1.4.305 | raw 0x1 | 71/71 | 12/24 | 156 |

- NVIDIA optional extensions missing: `VK_KHR_maintenance7`, `maintenance8`, `maintenance9`, `maintenance10`, `maintenance11`, `VK_KHR_present_id2`, `VK_KHR_present_wait2`, `VK_KHR_unified_image_layouts`, `VK_KHR_incremental_present`.
- The first probe run printed the NVIDIA driver as `550.163.64`. The NVIDIA packing was decoded wrongly (low 14 bits taken as one field). Corrected to major `>>22`, minor `>>14 & 0xff`, patch `>>6 & 0xff`, build `& 0x3f`. The corrected output matches `nvidia-smi` (550.163.01).
- Required set: taken from DXVK's `VP_DXVK_requirements.json`. Extension entries cross-checked against DXVK source (`src/dxvk/dxvk_device_info.cpp`): `khrLoadStoreOpNone`, `khrMaintenance6`, `extDepthClipEnable.depthClipEnable` are all enabled with `required = true`.
- Consequence: the host GPU is the gate. The ICD must refuse device creation when the host device lacks any required feature. Both NVIDIA and llvmpipe pass, so llvmpipe can serve as the headless CI backend.

### 3.2 gfxstream (github.com/google/gfxstream)

- License: Apache-2.0 (verified in `LICENSE`).
- Host decoder: present in `host/vulkan/cereal` (generated dispatch/deepcopy code, ~87k lines of C/C++). Name matches for `VK_KHR_dynamic_rendering`, `VK_KHR_transform_feedback`/`VK_EXT_transform_feedback`, `maintenance6`, `depth_clip_enable` and `load_store_op_none` occur in that tree. I did not verify that the host implements the full DXVK feature set, nor that each name is advertised as supported.
- Windows guest encoder: **not verified**. `guest/vulkan_enc` is absent from the GitHub mirror. The googlesource (AOSP) clone did not produce a checkout (`/tmp/gfxstream` does not exist), and I did not retry it in this spike.
- Transport: the byte-stream abstraction is `host/iostream/include/gfxstream/host/iostream.h`: class `IOStream` with pure virtual `allocBuffer`, `commitBuffer`, `writeFully`, `readFully`, `readRaw`, `getDmaForReading`, `unlockDma`, `onSave`, `onLoad`. Because the DMA hooks are pure virtual, a virtio-serial adapter must implement them itself (for example, return null and do nothing). This is useful prior art for a byte-stream adapter.
- Memory: gfxstream's design relies on host-shared memory (hostmem / virtio-gpu blobs) [INFERENCE: from general knowledge of the project, not checked in this clone]. That mechanism does not exist on virtio-serial, so we need shadow memory (section 7).

### 3.3 gpu-mesh (github.com/rover-001/gpu-mesh)

- License: the README says "Licensed under the MIT License". The shallow clone has no LICENSE or COPYING file and no `license` field in any `Cargo.toml`. Treat as unlicensed until the author clarifies.
- Windows ICD: code paths exist. `crates/icd/src/dispatch/wsi.rs` has `cfg(target_os = "windows")` presentation branches; `crates/icd/src/shadow.rs` uses write-watch on Windows. I found no evidence of a Windows test run. The repo also contains an IddCx kernel driver directory, which we do not need.
- Transport: `crates/icd/src/client.rs` holds a `std::net::TcpStream` directly. `crates/wire/src/framing.rs` is generic over `AsyncRead`/`AsyncWrite` and has sync variants, with a length prefix, a compression flag, and zstd (level 1) above a size threshold. Framing is reusable; the ICD client is TCP-bound.
- Server: `crates/server/src/engine/vulkan.rs:166` builds `PhysicalDeviceVulkan12Features`. No `Vulkan13Features` struct, and no `robustness2`, `dynamic_rendering`, `synchronization2` or `maintenance5` references, were found anywhere in `crates/server`. DXVK's requirements are therefore not met by this code [INFERENCE from grep: not compiled or run].
- Size: wire 3,253 + icd 8,143 + server 4,619 ≈ 16.0k lines.

### 3.4 Vulkan registry

- `/usr/share/vulkan/registry/vk.xml`: SPDX `Apache-2.0 OR MIT`, `VK_HEADER_VERSION` 309, 619 extensions. Usable as a generator input.

### 3.5 Host tooling

- `vulkaninfo` is not installed and not on PATH. Debian's `vulkan-tools` 1.4.304 is available (`apt-cache policy`) but was deliberately not installed. The probe uses ash instead.
- Vulkan ICDs present: NVIDIA (`nvidia_icd.json`), lavapipe, Mesa virtio/gfxstream. The gfxstream ICD from Debian's Mesa targets virtio-gpu guests, so it is not the host renderer [INFERENCE].
- Toolchain: cargo/rustc 1.100.0, gcc/clang/ld present; crates.io reachable.

### 3.6 M0: throughput and latency of the virtio-serial port (`experiments/chanbench`)

Setup: QEMU 10.0.13 (`-machine q35,accel=kvm`), host AMD Ryzen 9 5900X on Linux 6.12, guest Windows 11 LTSC 2024 (build 26100) in the disposable `Windows11Build` VM with the virtio-win serial driver. The VM's existing virtio-serial bus got one more port:

```
-chardev socket,id=gfx0,path=/tmp/gfx0.sock,server=on,wait=off \
-device virtserialport,chardev=gfx0,name=org.winbridge.gfx.0
```

`chanbench guest` (release build, compiled inside the VM) ran as SYSTEM and opened `\\.\Global\org.winbridge.gfx.0` overlapped; `chanbench host --socket /tmp/gfx0.sock --mib 256 --iters 5000` (release build) connected to the chardev socket. One thread per side, blocking I/O, no other traffic, host-side wall clock stopped when the receiver acknowledged. A transfer is at most 256 MiB or 262,144 chunks, whichever is smaller (so 16 B..256 B chunks move 4–64 MiB); 16 MiB warm-up each way. Two independent boots, each from the `clean` snapshot; cells are run 1 / run 2.

Throughput, MiB/s:

| Write size | host→guest | guest→host |
|---:|---:|---:|
| 16 B | 8.7 / 8.7 | 0.54 / 0.53 |
| 64 B | 36.6 / 34.2 | 2.1 / 2.1 |
| 256 B | 129.1 / 122.4 | 8.2 / 8.3 |
| 1 KiB | 221.2 / 220.2 | 33.5 / 32.0 |
| 4 KiB | 238.8 / 237.3 | 135.3 / 132.5 |
| 16 KiB | 253.1 / 246.5 | 464.3 / 434.0 |
| 64 KiB | 250.4 / 243.1 | 1,427 / 1,281 |
| 1 MiB | 216.5 / 222.6 | 4,873 / 4,650 |
| 4 MiB | 100.7 / 99.7 | 4,395 / 4,895 |

Round trip (host write, guest echo, host read; 5,000 iterations per size), µs:

| Payload | p50 | p90 | p99 | p99.9 | max |
|---:|---:|---:|---:|---:|---:|
| 0 B | 45.4 / 46.3 | 52.7 / 58.6 | 79.6 / 93.6 | 149 / 159 | 440 / 253 |
| 16 B | 76.8 / 76.5 | 95.0 / 98.5 | 167.9 / 164.4 | 196 / 195 | 233 / 2,703 |
| 64 B | 76.5 / 76.2 | 94.2 / 96.4 | 165.5 / 161.0 | 217 / 209 | 292 / 260 |
| 256 B | 76.7 / 76.2 | 92.8 / 101.4 | 165.5 / 166.8 | 194 / 209 | 606 / 250 |
| 1 KiB | 75.6 / 77.3 | 94.0 / 106.5 | 163.0 / 168.5 | 217 / 216 | 322 / 312 |
| 4 KiB | 89.0 / 93.4 | 110.6 / 120.2 | 181.0 / 181.5 | 235 / 211 | 4,122 / 257 |
| 16 KiB | 140.0 / 138.6 | 182.2 / 185.1 | 236.9 / 246.8 | 355 / 393 | 1,868 / 972 |
| 64 KiB | 334.7 / 334.1 | 403.2 / 413.9 | 532.1 / 540.3 | 650 / 680 | 1,220 / 1,025 |

The host→guest direction (the frame direction) is flat at about 216–253 MiB/s from 1 KiB to 1 MiB writes and halves at 4 MiB writes. The guest→host direction is dominated by a per-call cost of about 30 µs until the writes are large. Run-to-run spread was within 12% for every throughput cell and every RTT p50, with larger differences in the p99.9/max outliers.

### 3.7 Guest-side facts found in the Build VM

- **The port has one owner.** A second `CreateFileW` on `\\.\Global\org.winbridge.gfx.0` while the first handle is open fails with `ERROR_ACCESS_DENIED` (os error 5), observed on every connection of both boots. Several Vulkan processes cannot each open the port; a broker (the agent owns the port, processes reach it through a named pipe) is needed. It is not built: the ICD opens the port itself, so one Vulkan process per VM at a time.
- After the host side of the benchmark exited, the guest's `ReadFile` failed with `ERROR_NO_SYSTEM_RESOURCES` (1450) in every reopen cycle until the host came back. A dead host must be treated as a failed read, not as a hang: the ICD wraps every transfer in a 30 s wait that cancels the I/O (implemented; the timeout path was not exercised by a test).
- **No `vulkan-1.dll` is installed in the VM.** The tests use LunarG's `VulkanRT-1.3.296.0-Components.zip` (sdk.lunarg.com; sha256 `e0429f91cf356fe99e381f7d22e568ac81945669f1be1772d933a0d189230452`; `x64/vulkan-1.dll` is loader 1.3.296, plus `vulkaninfo.exe`), copied next to the test programs. The 1.4.x runtime URLs I tried returned 404. A guest without a GPU driver has no loader: the product must ship one (its license, `VulkanRT-License.txt`, was not reviewed here) [INFERENCE for stock Windows; verified only in the Build VM].
- **`VK_DRIVER_FILES` is ignored by a SYSTEM process** (open question 1 of the original design). `vulkaninfo.exe --summary` run through the guest agent (`whoami`: `nt authority\system`) with `VK_DRIVER_FILES` pointing at the ICD manifest printed `Registry lookup failed to get ICD manifest files`, `vkCreateInstance: Found no drivers!` and failed with `ERROR_INCOMPATIBLE_DRIVER`. After adding the manifest path as a DWORD value 0 under `HKLM\SOFTWARE\Khronos\Vulkan\Drivers`, the same loader found the ICD (and `winbridge-gfx-probe.exe` listed the device and passed). The behaviour matches the loader's documented rule that elevated processes ignore the override environment variables [INFERENCE: the rule is from memory, not re-read]. An unelevated process was not tested.

## 4. Components

| Component | Location | Runs on | Role |
|---|---|---|---|
| Guest ICD | `crates/winbridge-gfx-icd` (cdylib) | Windows 11 guest, x86_64-pc-windows-msvc, crt-static | Exports the Vulkan loader ICD entry points and forwards calls; shadows host-visible memory. Surface, swapchain and present: not built |
| Guest DXVK injection | `crates/winbridge-agent/src/gfx_inject.rs` (owned by GfxInject agent) | Guest agent (admin) | Detects VM; would wire DXVK and the ICD into allow-listed executables. Has never run in a VM; installs nothing while its hook DLL is missing |
| Channel | `\\.\Global\org.winbridge.gfx.0` (virtio-serial) | Guest ↔ host | Dedicated port; independent of `org.winbridge.0`; one owner at a time |
| Framing and codec | `crates/winbridge-gfx-proto` | Both | Frame header, fragmentation, opcodes, byte codec |
| Generated serializers | `crates/winbridge-gfx-vk` | Both | Generated request/reply serializers and the host executor (output of `vkgen`) |
| Host renderer | `crates/winbridge-gfx` | Linux host | Owns VkInstance/VkDevice (ash); executes forwarded calls. Offscreen rendering and readback: not built |
| Codegen | `crates/winbridge-gfx-vkgen` (Rust, reads `vk.xml`) | Build time | Emits the serializers, the dispatcher and the ICD shims for the subset in `subset.rs` |
| Test program | `crates/winbridge-gfx-probe` | Guest and host | Loader-level probe and the host loopback tests |
| Benchmark | `experiments/chanbench` | Host and guest | Port throughput and round-trip numbers (section 3.6) |
| Probe (done) | `experiments/vkcaps` | Host | Capability check; not part of the product |

The host viewer (`winbridge-host`) does not display gfx output. Frames are only presented into the guest HWND, so the viewer and the gfx channel do not share code paths.

## 5. Data flow

```
Guest (Windows 11)                                   Host (Linux, NVIDIA)
------------------------------------------           -----------------------------------
DXVK d3d11.dll  (side-loaded in app dir)
   | Vulkan calls
   v
winbridge-gfx-icd (Vulkan ICD)
   - handles = guest-assigned 64-bit IDs
   - serialize, batch until submit/present/sync
   - shadow memory (VirtualAlloc + write-watch)
   | frames: 16 B header + payload (zstd opt.)
   |  --- \\.\Global\org.winbridge.gfx.0 ----->  winbridge-gfx (QEMU chardev socket, server=on)
   |                                              - decode, map IDs -> ash objects
   |                                              - execute on NVIDIA VkDevice (offscreen)
   |                                              - present: copy to readback buffer
   |                                              - diff 64 KiB tiles, zstd changed tiles
   | <---------- FRAME (dirty tiles) ------------
   v
present thread: decompress into DIB section,
BitBlt into HWND (WM_PAINT-safe)
```

Command submission is asynchronous. Only queries, `vkWaitForFences` with a timeout, and map/invalidate are synchronous round trips.

## 6. Presentation by readback

- The ICD implements `VK_KHR_win32_surface` and `VK_KHR_swapchain` itself. A swapchain maps to N host images with the HWND's client size, format `B8G8R8A8_UNORM`.
- `vkQueuePresent` causes the host to copy the image into a host-visible readback buffer, split it into 64 KiB-aligned tiles of rows, hash each tile against the previous frame, and send only changed tiles (zstd, level 1, matching `wire/framing.rs`).
- The guest writes changed tiles into a DIB section and calls `BitBlt` on a dedicated present thread.
- Bandwidth is the main constraint. Raw BGRA sizes:
  - 1280×720: 3,686,400 B/frame ≈ 110.6 MB/s at 30 fps, ≈ 221 MB/s at 60 fps.
  - 1920×1080: 8,294,400 B/frame ≈ 249 MB/s at 30 fps, ≈ 498 MB/s at 60 fps.
  - These are raw; tile diffing and zstd reduce them for static UI but not for full-screen motion.
  - Measured on the real port (section 3.6): host→guest peaks at 243–253 MiB/s with 16–64 KiB writes (single stream, two boots); the M0 gate in section 10 is decided from that.
- Resize: the guest sends the new HWND size; the host recreates images on `VK_ERROR_OUT_OF_DATE_KHR`.

## 7. Memory model

- Device-local allocations live only on the host. The guest sees an opaque handle.
- Host-visible allocations: the guest's `vkMapMemory` returns a pointer into a shadow buffer allocated with `VirtualAlloc(MEM_WRITE_WATCH)`. The shadow is the same size as the allocation.
- Coherence: the ICD advertises `HOST_COHERENT` but does not rely on hardware coherence. It flushes dirty 64 KiB chunks (from `GetWriteWatch`) before every `vkQueueSubmit`, `vkQueuePresent`, and `vkFlushMappedMemoryRanges`. After a fence signal, `vkMapMemory`, or `vkInvalidateMappedMemoryRanges`, the host sends back dirty ranges written by the GPU.
- DXVK allocates in large blocks (hundreds of MB) for upload and staging. Shadowing these costs guest RAM and copy bandwidth. Mitigations: lazy shadow commit (pages committed on first map), chunk-granular dirtiness, and preferring device-local memory for DXVK's default heaps when the host reports them. This is a known risk (section 11).
- Pointers inside Vulkan structs never cross the wire. The generated serializer walks `pNext` chains (from `structextends` in `vk.xml`) and replaces host-side objects with IDs.

## 8. Protocol framing proposal

Transport: any ordered byte stream. The host connects to the QEMU chardev socket (`server=on`), as `chanbench` does. The guest opens the port overlapped (`FILE_FLAG_OVERLAPPED`) and wraps reads and writes in a `ByteStream` trait. The framing does not depend on the transport.

Frame header, 16 bytes, all little-endian:

| Offset | Size | Field | Notes |
|---|---|---|---|
| 0 | 4 | magic | ASCII `WBGF` |
| 4 | 1 | version | 1 |
| 5 | 1 | flags | bit0 zstd payload; bit1 reply expected; bit2 more fragments follow |
| 6 | 2 | opcode | see below |
| 8 | 4 | seq | request ID; replies echo it |
| 12 | 4 | length | payload bytes after header; max 64 MiB (as `winbridge-proto`) |

Opcodes (initial set):

- `0x0001 HELLO`: both ways; carries protocol version, identity string, feature bitmap (no bits defined), receive window (advertised, not enforced). Version mismatch closes the channel. A second `HELLO` on a live connection means a new guest process: the host destroys the previous one's objects.
- `0x0002 PING` / `0x0003 PONG`: liveness.
- `0x0010 VK_CALL`: guest → host; serialized Vulkan call, payload starts with the command id (u16). In M1 always sent with `reply expected`.
- `0x0011 VK_REPLY`: host → guest; return value and out-parameters.
- `0x0020 MEM_UPLOAD`: guest → host; memory id, offset, data (`MemUpload`); the host answers with an empty frame of the same opcode and seq.
- `0x0021 MEM_DOWNLOAD`: M1 form: guest → host request (`MemRead`: memory id, offset, length) answered by a frame with the same opcode and seq that carries the bytes. The host→guest push described originally is not implemented.
- `0x0030 PRESENT`, `0x0031 FRAME`: not implemented.
- `0x00F0 ERROR`: host → guest, UTF-8 message; the host closes the session after sending it.

Implemented in `crates/winbridge-gfx-proto`: header, fragmentation (a message larger than the writer's fragment size, 64 KiB by default, is sent as frames with the same opcode, seq and flags; all but the last carry bit2; the reader reassembles up to 256 MiB), bad magic/version/flags/length rejection, and a poisoned reader after the first framing error (no resync). Bit0 (zstd) is reserved and rejected by the reader: compression is not implemented.

Flow control (design): the host advertises a receive window in `HELLO`; the guest blocks before exceeding it. M1 advertises the window and does not enforce it (every call waits for its reply, so at most one request is in flight, except fragments of one message). Since virtio-serial is ordered and reliable, there is no retransmission or resync: a bad magic or length closes the channel. The ICD then reports `VK_ERROR_DEVICE_LOST` (and `VK_ERROR_INCOMPATIBLE_DRIVER` from `vkCreateInstance` if the host cannot be reached).

Relation to existing code: `winbridge-proto` uses an 8-byte header (magic + length). Our header is separate, because it carries a sequence number and opcode for a different message class. `chanbench` uses a 16-byte layout, but it is a benchmark and not a precedent.

## 9. Vulkan subset and codegen

- Generate only the commands and structs reachable from DXVK's required set plus the swapchain/sync commands. Unlisted commands return `VK_ERROR_EXTENSION_NOT_PRESENT` or `VK_ERROR_FEATURE_NOT_PRESENT` as appropriate.
- Input: `vk.xml` at build time (committed generated output, plus a `vkgen --check` mode that fails on drift).
- Output: guest serializer and shims (`winbridge-gfx-icd`), host dispatcher (`winbridge-gfx`). The generated codec for both sides lives in `winbridge-gfx-vk` (features `guest` and `host`); `winbridge-gfx-proto` holds the framing and the byte codec.

Implemented in M1 (`crates/winbridge-gfx-vkgen`):
- Parameter shapes supported: handles (guest-assigned 64-bit ids, dispatchable ones are `DispatchableHeader` objects), scalars, `const T*` structures, strings, out structures with or without a `pNext` chain, out handles, and `count + array` enumerations of plain structures or handles. Anything else makes the generator panic with the command and parameter named; nothing is skipped silently.
- Structures that are plain data are copied as bytes; others are walked member by member (nested structures, length-prefixed arrays checked against their count member, string arrays). `pNext` chains are serialized node by node for the structures in `CHAIN_ALLOW`, filtered by `structextends`, and rebuilt from an arena on the host. Output chains travel as a list of structure types up and as the bodies down. Struct layouts rely on `repr(C)` of ash 0.38 (Vulkan 1.3.281 headers) being identical on both sides; a compile-time assertion checks the 16-byte `sType`/`pNext` header offsets.
- The generated code is `unsafe` and uses ash types (`ash 0.38.0+1.3.281`): structures added to `vk.xml` after 1.3.281 are not available. Promoted structures are named by their ash (`...KHR`) alias.
- The wire command id is the index in `COMMANDS` (`subset.rs`); extending the list at its end keeps old ids, reordering breaks the protocol (bump `PROTOCOL_VERSION`).

Effort, own build (estimate, [INFERENCE]):

| Part | Estimate |
|---|---|
| `vkgen` (XML parse, pNext walk, emitters) | 1.5–2.5k lines |
| Guest ICD (loader entry points, surface/swapchain, shadow memory, present) | 4–6k lines |
| Host renderer (dispatch to ash, ID tables, readback, tile diff) | 3–5k lines |
| Tests (framing, loopback, llvmpipe render checksums) | ~1k lines |
| **Total** | **~10–15k lines** |

Reuse comparison:

- gfxstream host: ~87k generated lines, but Android/emulator-coupled and built on hostmem. Reusing it means inheriting its dependency surface and memory model.
- gpu-mesh: ~16k lines, closer in size, but TCP-only, Vulkan 1.2 only, and unlicensed.

## 10. Milestones and acceptance

Host-only tests run on this machine (llvmpipe and NVIDIA). VM tests run on a disposable Windows build VM that is rolled back to a clean snapshot after each run and is booked with a lock shared with other jobs. Never use a VM that holds real user data, and never touch a running qemu.

**M0 — Transport and numbers**
- Build: `winbridge-gfx-proto` framing. `chanbench` was extended before the run (it was not run unchanged): bulk write sizes 16 B..4 MiB instead of 64 KiB..4 MiB, a cap of 262,144 chunks per transfer, echo sizes 0 B..64 KiB, p90/p99.9/mean columns, a chunks-per-second column, and a probe that tries to open the port a second time while it is open (section 3.7).
- Host acceptance (no VM): framing round-trip, fragmentation, bad-magic close, loopback tests over `UnixStream`.
- VM acceptance: `chanbench` guest and host run on `org.winbridge.gfx.0`, recording throughput for payload sizes 16 B to 64 KiB and RTT percentiles. Results go into this doc.
- Gate: sustained throughput that supports the chosen resolution and frame rate. At 1280×720 @ 30 fps raw is ~110 MB/s; if the measured figure is below ~110 MB/s with typical tile-diff savings, lower the target (resolution or fps) before M1. The gate is decided by measurement, not assumed.

**M0 result (measured 2026-11-15; evidence in section 3.6): gate passed for 1280×720 @ 30 fps raw, not for 1080p60 raw.**

- Frames flow host→guest (readback). Single-stream host→guest throughput with 1 KiB..64 KiB writes was 220–253 MiB/s over two boots. Raw BGRA needs 105.5 MiB/s (720p30), 210.9 MiB/s (720p60), 237.3 MiB/s (1080p30) and 474.6 MiB/s (1080p60).
- 720p30 has 2.1–2.4× headroom before tile diffing, so the target for M2/M3 is **1280×720 @ 30 fps**. 720p60 (1.04–1.2×) and 1080p30 (0.93–1.07×) need the tile-diff/zstd savings to be real; 1080p60 raw needs 1.9× the measured ceiling and is out of reach.
- The default fragment size is 64 KiB (1 MiB fragments measured 10–13% slower host→guest).
- Guest→host is not the bottleneck for writes of 16 KiB and up (430–4,900 MiB/s), but a small write costs about 30 µs (33–35k writes/s from 16 B to 4 KiB). Commands MUST be batched into frames of at least 16 KiB.
- A synchronous round trip costs at least 76 µs for 16 B–1 KiB (p99 about 165 µs), so at most about 13k synchronous calls/s. The M1 ICD makes every call synchronous (189 µs p50 measured through the ICD, section 10 M1 result): the asynchronous batching of section 5 is a prerequisite for M2, not an optimisation.
- Not measured: both directions at once, CPU use of either side, behaviour under host load, other QEMU versions.

**M1 — Loader sees the device**
- Build: ICD with `vk_icdNegotiateLoaderICDInterfaceVersion`, `vk_icdGetInstanceProcAddr`, `vk_icdGetPhysicalDeviceProcAddr`; `vkEnumeratePhysicalDevices`, `vkCreateDevice`, buffer create/map/write/read.
- Host acceptance (no VM): loopback test driving the ICD call sequence against llvmpipe, asserting the 71 DXVK-required checks pass on the created device.
- VM acceptance: the Windows Vulkan loader on the guest lists the "WinBridge" device; a checksum over a mapped buffer uploaded and read back through the host matches.

**M1 result (2026-11-15): host acceptance and VM acceptance passed for the scope below.**

What exists:
- `crates/winbridge-gfx-vkgen` reads `/usr/share/vulkan/registry/vk.xml` (VK_HEADER_VERSION 309) and writes `crates/winbridge-gfx-vk/src/generated.rs` (struct and command serializers, host executor, `dispatch`) and `crates/winbridge-gfx-icd/src/shims.rs`. `cargo run -p winbridge-gfx-vkgen -- --check` fails on drift. Generated subset, 25 commands: `vkCreateInstance`, `vkDestroyInstance`, `vkEnumeratePhysicalDevices`, `vkGetPhysicalDevice{Properties,Properties2,Features,Features2,FormatProperties,FormatProperties2,MemoryProperties,MemoryProperties2,QueueFamilyProperties,ImageFormatProperties,SparseImageFormatProperties}`, `vkEnumerateDeviceExtensionProperties`, `vkCreateDevice`, `vkDestroyDevice`, `vkGetDeviceQueue`, `vkDeviceWaitIdle`, `vkCreateBuffer`, `vkDestroyBuffer`, `vkGetBufferMemoryRequirements`, `vkAllocateMemory`, `vkFreeMemory`, `vkBindBufferMemory`. The last two physical-device commands are there because the Vulkan loader refuses an ICD without them (`Failed to find required entrypoints`). The ICD adds by hand `vkMapMemory`, `vkUnmapMemory`, `vkFlushMappedMemoryRanges`, `vkInvalidateMappedMemoryRanges`, `vkEnumerateInstance{ExtensionProperties,LayerProperties,Version}` and the three `vk_icd*` exports.
- `crates/winbridge-gfx`: host dispatcher (`winbridge-gfx --socket PATH [--device auto|INDEX|NAME]`, library `winbridge_gfx`). One `Session` per connection owns the id→handle tables, the driver instance/device objects, and the host mappings of host-visible memory; a repeated `HELLO` or the end of the connection destroys what the guest left behind.
- `crates/winbridge-gfx-icd`: the ICD (cdylib `winbridge_gfx_icd.dll`, also builds as a Linux `.so` that talks to a Unix socket named by `WINBRIDGE_GFX_SOCKET`, which is how the host test runs the same code under the Linux loader). Device name is reported as `WinBridge: <host GPU>`.
- `crates/winbridge-gfx-probe`: loader-level test program (also the VM acceptance binary) and the host loopback tests.

Host acceptance (no VM), run here on `NVIDIA GeForce RTX 3060 Ti` and `llvmpipe (LLVM 19.1.7, 256 bits)`:
- `cargo test -p winbridge-gfx-proto`: 17 tests (framing, fragmentation, bad magic, truncation, loopback over `UnixStream`, codec).
- `cargo test -p winbridge-gfx-vk`: 3 tests (`device_create_info_with_feature_chain_round_trips`, `unknown_chain_nodes_are_dropped_not_misparsed`, `truncated_and_inconsistent_input_is_rejected`).
- `cargo test -p winbridge-gfx-probe`: 3 tests. `loader_lists_winbridge_device_and_buffer_round_trips_on_every_host_gpu` runs the probe as a child process whose Linux Vulkan loader sees only the WinBridge ICD (`VK_DRIVER_FILES`), once per host device: exactly one device is listed, named `WinBridge: <host GPU>`; `dxvk_checks: 71/71` through the ICD; a device is created with the six DXVK extensions and the whole feature chain; an 8 MiB mapped buffer is written, flushed, the guest shadow wiped, invalidated, and its FNV-1a checksum compares equal; a partial flush of 8 KiB leaves exactly that range changed; no memory type reports `HOST_COHERENT`. `buffer_larger_than_one_upload_chunk_round_trips` does the same with 20 MiB (five 4 MiB requests). `dispatcher_rejects_protocol_violations_and_closes` checks garbage, a call before `HELLO`, an unknown command id, an unknown handle id and a wrong protocol version.

VM acceptance (Build VM, Windows loader `vulkan-1.dll` 1.3.296, release build of the ICD and probe made inside the VM in 17 s, ICD DLL 395,264 bytes, host dispatcher on the NVIDIA GPU and then on llvmpipe, ICD registered under `HKLM\SOFTWARE\Khronos\Vulkan\Drivers`; commands run as SYSTEM through the guest agent):
- the loader lists `device[0]: WinBridge: NVIDIA GeForce RTX 3060 Ti (type DISCRETE_GPU, vendor 0x10de, api 1.3.277)` (resp. `WinBridge: llvmpipe (LLVM 19.1.7, 256 bits)`, type CPU);
- `dxvk_checks: 71/71` on both; device created with 6 extensions;
- 8 MiB buffer checksum written `0x1953fd726605bdfd`, read back `0x1953fd726605bdfd`; partial flush expected and read back `0x26f975a4f1d96ba5`; `RESULT: PASS` on both devices.
- Through the ICD over the real port (NVIDIA, 2,000 `vkGetBufferMemoryRequirements` calls): p50 188.7 µs, p90 231.0 µs, p99 305.4 µs, max 834 µs (llvmpipe: 186.4 / 233.4 / 323.3 / 1,903 µs). One 8 MiB `vkFlushMappedMemoryRanges` ran at 565 MiB/s (llvmpipe 501), one 8 MiB `vkInvalidateMappedMemoryRanges` at 142 MiB/s (llvmpipe 237). For comparison, the same probe over the Unix-socket loopback on the host (debug build) measured 28.0 µs p50 per call. The 189 µs is about 110 µs above the raw echo round trip of section 3.6; it was not profiled.

What M1 does not do (each of these is a fact about the code as tested):
- (Superseded by the M2 result: calls that return nothing are batched; command buffers, queue submission, images, pipelines, swapchain and surface exist; host-visible memory is coherent by write-watch. M1 as tested did none of this.)
- No instance extensions are exposed; the device extension list is the host's raw list although most extension commands are not implemented.
- Unknown `pNext` structures are dropped by the serializer (as the specification allows); only the structures in `CHAIN_ALLOW` of `vkgen` are forwarded.
- One Vulkan process per VM (risk 10); the ICD can only be found by elevated processes through the registry (section 3.7); no reconnect after a dead channel (every later call fails with `VK_ERROR_DEVICE_LOST`, `vkCreateInstance` with `VK_ERROR_INCOMPATIBLE_DRIVER`).
- The release of leftovers on a repeated `HELLO` is implemented but not covered by a test.

**M2 — vkcube presents**
- VM acceptance: LunarG's Windows `vkcube` runs against the ICD and renders into the HWND. Host-side frame checksum equals a reference offscreen render of the same frame on llvmpipe, within a stated tolerance (exact match expected for deterministic output).

**M2 result (2026-11-15): vkcube renders in the Build VM; exact-checksum acceptance holds for the triangle test, not for vkcube.**

- Build: commands, images, render passes, framebuffers, pipelines, shaders, descriptors, fences and queue submit through the generated set; asynchronous batching (`OP_VK_BATCH`, 0.73–0.84 µs per recorded command, measured on the host loopback); `VK_KHR_surface`, `VK_KHR_win32_surface` (the ICD owns its Win32 surface objects: the loader's own surface object had no usable `VkIcdSurfaceBase` when handed to the ICD, so creating them in the ICD is what works) and `VK_KHR_swapchain`. Presentation is readback of the host image into the guest window (GDI `SetDIBitsToDevice`) with raw BGRA frames; no tile diff yet. Messages `OP_SC_*` in `winbridge-gfx-proto`.
- Transport seam: `MessageTransport` / `StreamTransport` (section 12a).
- Host test `presented_triangle_matches_the_llvmpipe_reference` (`cargo test -p winbridge-gfx-probe`): the probe's triangle (render pass + graphics pipeline, SPIR-V from `crates/winbridge-gfx-probe/shaders/tri.wgsl` via naga) presented through the ICD and the dispatcher is bit-identical to a direct llvmpipe render (320×240, FNV-1a `0x1d6064a832042f25`, 0 of 76,800 pixels differ). On the NVIDIA host the frame differs by at most 1 per channel (rounding), asserted with a 2-per-channel tolerance.
- VM, same triangle on the llvmpipe host device: the frame the ICD wrote in the VM is byte-identical to that reference (`cmp`). VM, NVIDIA host device, 640×360 window in the interactive session, 900 presents: `frame_ms` p50 1.59 ms, max 24.78 ms (this measures the readback call, not display); screenshot taken with `vm shot` shows the triangle in the window.
- VM, `vkcube.exe` (LunarG SDK 1.3.296, `--suppress_popups`), host NVIDIA: a window "Vulkan Cube" shows the textured rotating cube; two screenshots 6 s apart show different rotation angles and the process was still running at the end of the run. An earlier run with `--c 3000` exited with code 0. Exact frame checksum against llvmpipe is not possible for vkcube (it animates in real time); it was verified by screenshots only.
- Conditions the VM run needed: the ICD process must be elevated (an unelevated `builder` process gets `Access is denied` opening the port, as predicted in section 3.7; the test task runs with `/rl highest`), and the ICD is registered under `HKLM\SOFTWARE\Khronos\Vulkan\Drivers`.
- Not done: no frame-rate measurement of vkcube, no tile diff or compression, one Vulkan process per VM, no reconnect after a dead host.

**M3 — DXVK d3d11 triangle**
- Build: DXVK 2.x (master `71bf4bad` as cloned) with a small D3D11 triangle test app built on the guest side.
- VM acceptance: triangle renders in the HWND; pixel comparison against a reference image; WM_SIZE resize works; 60-second run with no device loss. D3D9 and D3D10 smoke tests follow as M3b.

**M4 — Injection and product integration**
- Build: `gfx_inject.rs` (GfxInject agent) wires DXVK and the ICD into allow-listed executables; launcher and fallback path.
- VM acceptance: launching an allow-listed EXE gets the gfx path automatically; uninstalling the allow-list entry reverts; host restart and channel reconnect recover; 30-minute soak with frame pacing recorded.

## 11. Risks

1. **Bandwidth and latency.** Measured in M0 (section 3.6): about 220–253 MiB/s host→guest, 76 µs minimum synchronous round trip. 720p30 raw fits, 1080p60 raw does not. Mitigation: tile diff; zstd; target resolution or fps reduction; batching of commands (small writes cost about 30 µs each).
2. **Shadow memory cost.** DXVK's large upload blocks multiply guest RAM and copy traffic. Mitigation: lazy commit, device-local preference, measured in M1/M3.
3. **Coherence correctness.** Host-visible memory is offered as `HOST_COHERENT` by a write-watch shadow (`crates/winbridge-gfx-icd/src/shadow.rs`, `MEM_WRITE_WATCH` on Windows) flushed at queue submit and invalidated after waits; a non-coherent type is offered as well. The probe round-trips checksums on both types on NVIDIA and llvmpipe (host and VM). Not tested: applications that read GPU results without a fence, DXVK's staging patterns.
4. **Injection mechanism.** IFEO's `Debugger` value redirects a launch; it does not inject a DLL by itself. The launcher must start the process suspended, then arrange DXVK and the ICD. Preferred: side-load DXVK DLLs from the app directory and select the ICD via the loader's `VK_DRIVER_FILES` override set by the launcher, instead of registering a global ICD. Measured since: the Windows loader ignores `VK_DRIVER_FILES` in a SYSTEM (high-integrity) process (section 3.7), so the override can only be relied on for unelevated launches, which was not tested. Owned by GfxInject; not validated here.
5. **Anti-cheat and antivirus.** Injected DLLs and IFEO entries are flagged by some games and AV products. Out of scope for the protocol; a per-app allow-list is the mitigation.
6. **Device loss and host crashes.** A host Vulkan device loss must become `VK_ERROR_DEVICE_LOST` in the guest; the ICD must recreate the host device on reconnect. Not designed in detail yet.
7. **Host GPU variance.** NVIDIA and llvmpipe pass the required set; other vendors may not. The host refuses device creation with `VK_ERROR_FEATURE_NOT_PRESENT` instead of degrading silently (risk 12).
8. **License.** gfxstream code, if copied, needs Apache-2.0 notices. gpu-mesh is unusable until its license is clarified.
9. **Unverified Windows ICD in gfxstream and gpu-mesh.** Neither was run in this spike. Our design does not depend on either.
10. **The port has one owner** (measured, section 3.7). Two Vulkan processes in the guest cannot both open `org.winbridge.gfx.0`; the second `CreateFileW` fails with access denied. A broker in the agent with per-process pipes is required before more than one Vulkan application can run; the M1 ICD opens the port directly.
11. **ICD entry-point coverage (was: the ICD claims more than it implements).** The generated set now covers 206 of the 215 core 1.0–1.3 commands (`cargo run -p winbridge-gfx-vkgen -- --survey`); the other nine have shapes the generator does not handle (`vkMapMemory`, `vkUpdateDescriptorSetWithTemplate`, ...; the ones the ICD needs are written by hand). Observed in the Build VM: LunarG `vulkaninfo --summary` ran to `VULKANINFO_EXIT=0` listing the device (the earlier 0xC0000005 crash came with 34 entry points), and `vkcube` ran (M2 result). Not covered: extension commands beyond `VK_KHR_swapchain`/`VK_KHR_win32_surface`, ray tracing, mesh shaders.
12. **Refusal gate.** `vkCreateDevice` on the host evaluates the 71 DXVK checks and returns `VK_ERROR_FEATURE_NOT_PRESENT` when one fails (`crates/winbridge-gfx/src/session.rs`). Found in the VM with vkcube: the host instance used the application's `apiVersion` (1.0), which hides the Vulkan 1.1+ feature structs, so the gate rejected a capable GPU. The host instance now asks for min(loader, 1.3); regression test `device_is_created_for_an_api_1_0_instance` (fails without the fix, checked). The refusal path itself (a GPU that really lacks a feature) has no test.

## 12. Reuse vs own

Recommendation: **own implementation**, generated from `vk.xml` for the DXVK subset, with gfxstream's `IOStream` interface and dispatch structure as design reference. Revisit only if the gfxstream guest encoder is obtained and its license and transport are a clean fit.

## 12a. Future transport: ivshmem

Decision (user, 2026-11-15): stay on virtio-serial until a real application presents, then M3; ivshmem is a later step. No code exists for it. What is in place today is the boundary that makes it a swap: `winbridge_gfx_proto::frame::MessageTransport` (send one message, block for the next whole message). The ICD link (`HostLink`) and the host dispatcher (`serve`) use only that trait. Overlapped handles, the 30 s I/O timeout, the 1 MiB `ReadFile` cap and the 64 KiB fragmenting live in `StreamTransport` and `crates/winbridge-gfx-icd/src/transport.rs`. Opcodes, codecs and the dispatcher do not change with the transport.

The ivshmem design (devices by trust domain, lanes, sizing, driver issues) is in `docs/IVSHMEM-TRANSPORT.md`; it is not duplicated here.

Out of scope (user decision, 2026-11-15): any overlay or host-side display of guest frames. Presentation is only readback into the guest window; with ivshmem the return path is a frame ring read by the guest.

Frame compression is bandwidth-adaptive, not canvas-size-based: per-tile mode (skip unchanged, lossless for text-UI tiles, block compression for picture tiles), a host controller that follows the measured link throughput, guest decode and blit time reported back in the frame ack, and a bounded number of frames in flight (credit-based, the newest frame wins, stale frames are dropped). The codec is not chosen yet: it waits for the `experiments/codecbench` data. The frame sink stays pluggable. Not implemented.

## 13. Open questions

- ~~Confirm the guest loader honours the `VK_DRIVER_FILES` override in the Windows VM.~~ Answered in section 3.7 (measured): a process running as SYSTEM, i.e. at high integrity, does not (`ERROR_INCOMPATIBLE_DRIVER`, "Found no drivers"); registering the manifest under `HKLM\SOFTWARE\Khronos\Vulkan\Drivers` works. Not yet measured: the override in an unelevated process, which is what DXVK-using games are; it decides whether M4 can avoid a global registry entry.
- Per-process access to the single-owner port (broker design, section 11 risk 10).
- The refusal gate exists (section 11 risk 12) but only the pass path is tested; a host GPU that really lacks a feature is untested.
- Decide whether IFEO is used only as a launch trigger (with a suspended-launch helper) or for direct injection; this belongs to the GfxInject design.
- Obtain a license statement from gpu-mesh's author if its code is ever to be referenced beyond ideas.
- Retry the AOSP googlesource clone of gfxstream to verify the guest Vulkan encoder.
