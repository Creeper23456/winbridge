<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# WinBridge Shared-Memory Transport (ivshmem) Design

Status (2026-11-15): **design only, nothing is implemented.** Decided by the user the same day. Order of work: (1) vkcube and the DXVK triangle on the current virtio-serial path (`docs/GFX-DESIGN.md` M2/M3), (2) this transport, (3) the Venus port to Windows. Statements marked **[UNVERIFIED]** are recalled from memory or inferred and must be measured or checked before they are relied on. Sizes are starting values, not measurements.

## 1. Why

The control channel (`org.winbridge.0`: clipboard, drag-and-drop, status, files up to 8 GiB) and the graphics channel (`org.winbridge.gfx.0`: Vulkan commands and returned frames) are today two virtio-serial ports. They share one virtio device and QEMU's main-loop I/O, and each is a byte stream with one copy through the chardev socket. Measured (`docs/GFX-DESIGN.md` 3.6): host-to-guest 216-253 MiB/s, about 30 us per small write, at least 76 us per synchronous round trip.

The scenario that forces the redesign: a 3D program is presenting frames while the user pastes a large file. With a single FIFO per direction the file would sit in front of frames and Pings (head-of-line blocking); even with separate ports they compete for the same device, host thread and guest CPU. Today the transfer also locks guest input (`docs/ARCHITECTURE.md`, transfer progress), which hides the problem rather than solving it.

Goals:

- Full-duplex, much higher bandwidth, lower latency than virtio-serial.
- **Lanes with priorities** so bulk data can never delay control traffic or frames.
- Frames, commands and bulk data cross the boundary with at most one copy.
- A hostile or buggy guest process can never make the host read or write outside the shared region, and cannot impersonate the host to the elevated agent.

## 2. Topology

Two `ivshmem-plain` devices, split by trust domain, plus the existing virtio-serial retained as a very small side channel.

| Device | Opened by | Carries | BAR2 |
|---|---|---|---|
| `wb-ctl` | the elevated guest agent only | control, clipboard, drag-and-drop, file/clipboard bulk | 256 MiB |
| `wb-gfx` | Vulkan clients (unprivileged processes, via the ICD) and the agent | Vulkan commands, returned frames, resource pool | 256 MiB |
| virtio-serial `org.winbridge.0` | the agent | handshake and version negotiation, wake-up kick, fallback when shared memory is unavailable | - |

Why two devices: `\\.\Global\org.winbridge.0` needs administrator rights today, which is what stops an unprivileged guest process from feeding fake host messages to the elevated agent. Shared memory has no such barrier: any process that can open the device can write the rings. Graphics clients are ordinary user processes and need access, so graphics gets its own device; the control rings live on a device only an administrator can open. A guest-local attacker with access to `wb-gfx` can then corrupt graphics data but cannot reach the agent's control path.

QEMU side (sketch, per device; sizes as above):

```
-object memory-backend-file,id=wb-ctl,mem-path=/dev/shm/winbridge-ctl,size=256M,share=on
-device ivshmem-plain,memdev=wb-ctl
```

The backing files are created by the launcher with mode 0600 for the VM's user. `ivshmem-plain` is PCI 1af4:1110 with the memory in BAR2 **[UNVERIFIED: recalled]**. shm pages are committed on first touch, so a large BAR costs host RAM only for the part actually used.

## 3. Lanes and scheduling

Each device holds, per direction (guest-to-host TX, host-to-guest RX), several independent single-producer single-consumer rings. A lane has its own credit counter; a full lane back-pressures only itself.

| Lane | Device | Direction | Priority | Rule |
|---|---|---|---|---|
| control | ctl | both | highest | small messages (Ping, status, clipboard metadata, transfer progress); always drained first; reserved space that bulk cannot use |
| bulk | ctl | both | lowest | file and clipboard payload in blocks of at most 1 MiB; takes leftover bandwidth; every block is a scheduling point |
| commands | gfx | guest to host | high | Vulkan command batches, latency sensitive |
| frames | gfx | host to guest | high | encoded frame tiles; newest frame wins, stale frames are dropped (`docs/GFX-DESIGN.md` bandwidth-adaptive presentation) |

Rules:

1. A lane never blocks another lane. Each consumer thread serves exactly one lane (or round-robins lanes of equal priority) and always serves higher priority first.
2. Bulk is chunked and credit-controlled: at most N blocks in flight. A 3D program and an 8 GiB paste therefore share the memory bandwidth, not the queue.
3. Starvation guard: bulk gets a guaranteed minimum (for example one block per 50 ms) so a transfer always makes progress and the existing 30 s no-progress watchdog stays meaningful.
4. The transfer progress bar and guest-input lock stay as they are. Relaxing the input lock is a separate decision, to be made after the lanes are measured.

## 4. Ring design

- SPSC ring buffers in the BAR: a 64-byte-aligned header per ring (head, tail, flags, each index written by exactly one side and padded to its own cache line), then a power-of-two data area. Records carry a length and a lane tag; existing framing (`winbridge-gfx-proto` frames, `winbridge-proto` messages) is reused as record payload, only the transport changes. The `MessageTransport` boundary from `docs/GFX-DESIGN.md` 12a is the seam; the control protocol needs the same seam (host `serial_client.rs`, agent `channel.rs`) before migration.
- A region header page holds magic, protocol version, capabilities and the lane directory. Version and capabilities are negotiated over virtio-serial first; a peer without the driver or with a mismatching version falls back to virtio-serial. The `bincode` wire format is positional, so host and agent must still be rebuilt and deployed together.
- Memory ordering: x86 TSO plus compiler fences is enough for ordinary memory. If the guest maps the BAR write-combining, the producer must execute `sfence` after writing record bytes and before publishing the new index; with a cached mapping this is not needed. Which one applies is chosen when the driver maps the BAR (section 8).
- Direction asymmetry: on a write-combined or uncached mapping guest writes are fast but guest **reads are slow** (research reports anecdotes of tens of MB/s for scalar reads; **[UNVERIFIED: not measured by us]**). Everything the guest consumes at volume (returned frames, bulk file data from the host) is exposed to that. Candidate mitigations, in the order to try: (a) map the BAR **cached** (the driver reportedly takes a per-mapping cache mode: non-cached, cached, write-combined; QEMU backs the BAR with ordinary host RAM, so hardware coherency should apply **[UNVERIFIED]**); never map the same physical pages with two different cache modes at once (conflicting memory types are undefined on x86), so the cache mode is one choice per device; (b) streaming loads (`MOVNTDQA`) on a WC mapping; (c) a bounce copy into ordinary RAM in large blocks; (d) encoding that shrinks the bytes read (`experiments/codecbench`). The real numbers must be measured before any lane layout is frozen.

## 5. Wake-up (doorbell)

`ivshmem-plain` has no interrupt. `ivshmem-doorbell` has one but needs the external `ivshmem-server` process and a driver that handles the vectors **[UNVERIFIED: whether the chosen driver does]**. Plan: adaptive polling. A consumer spins for a short, bounded time after traffic, then sleeps on a timer (order of 1 ms) and is woken by a one-byte kick over virtio-serial when a producer finds the ring empty and the consumer asleep. Busy rings therefore pay no kick; idle rings do not burn a core.

## 6. Sizing: is 256 MiB enough?

Bandwidth is not the question (memory copies run at GB/s, far above the 253 MiB/s of virtio-serial); **capacity** is.

`wb-ctl` (256 MiB, about half used): 4 KiB header, control lanes 1 MiB each way, bulk lanes 64 MiB each way (blocks of 1 MiB, an 8 GiB file streams through the ring with back-pressure and is never resident). Plenty; 128 MiB would also do. The user chose 256 MiB; the unused part costs nothing until touched.

`wb-gfx` (256 MiB, shared by all Vulkan clients): per client roughly a 4-8 MiB command ring and a frame ring sized from the encoded-frame budget when the swapchain is created (raw 1080p BGRA is 8.3 MB per frame, so raw frames would not fit more than a few; encoded tiles are far smaller). The tight spot is the **host-visible resource pool**: DXVK allocates upload and staging memory in blocks of hundreds of MB, and zero-copy import with `VK_EXT_external_memory_host` needs those ranges to live inside the BAR. Options: (a) a managed arena with sub-allocation and streaming uploads through the ring for what does not fit; (b) make the `wb-gfx` size configurable (512 MiB or 1 GiB are the next power-of-two steps, guest address space is not a problem for a 64-bit BAR). Start with 256 MiB as decided and treat the pool size as a measured parameter once DXVK runs; do not assume it is enough for 4K texture-heavy games.

## 7. Security

- The host treats every byte of both regions as hostile: copy a record out of the ring, then validate the copy; no pointers or host addresses inside records; all offsets and lengths bounds-checked against the lane; index values sanity-checked (an index that moves backwards or past the other side's, closes the lane); no parsing in place (no time-of-check/time-of-use).
- `wb-ctl` is opened only by the elevated agent. The driver's default device security reportedly grants only SYSTEM and Administrators **[UNVERIFIED: read from the INF/driver, not tested]**, which is exactly what `wb-ctl` wants. The agent keeps validating paths and sizes exactly as today.
- `wb-gfx` slices are per client; a crashing client's slice is reclaimed by the host on channel loss. Whether one unprivileged process can scribble over another client's slice depends on whether the driver maps the whole BAR to every opener **[UNVERIFIED]**; the blast radius is limited to graphics data and is mitigated by per-slice validation and the broker in the agent if needed.
- A malicious guest cannot make the host exceed the region: the host maps the files it created, with fixed sizes.

## 8. Windows driver

Shared memory needs a **kernel-mode driver in the guest**. The user accepted installing a third-party one; we write no driver ourselves. Everything below was gathered by a research pass over secondary sources and a web search; **my own attempt to fetch the driver's `Public.h` from GitHub failed (network error), so none of it is confirmed from the primary source yet.** Treat every item as **[UNVERIFIED]** until the driver source or a test in the build VM confirms it.

What research reports:

- **Source and license:** the ivshmem driver lives in `virtio-win/kvm-guest-drivers-windows` (directory `ivshmem`), under BSD-3-Clause; Looking Glass bundles a prebuilt copy in its installer (Looking Glass itself is GPL-2.0, its own indirect-display driver is separate). BSD-3-Clause is compatible with our GPL-3.0-or-later project.
- **User-mode interface:** device interface `GUID_DEVINTERFACE_IVSHMEM` `{df576976-569d-4672-95a0-f57e4ea0b210}`; IOCTLs to request the peer id, the BAR2 size, to map BAR2 into the caller (`IOCTL_IVSHMEM_REQUEST_MMAP`, with a cache mode parameter: non-cached, cached, write-combined), to ring a doorbell and to register an event. Mappings are per file handle and released when the handle closes. Several `ivshmem-plain` devices should appear as several device interface instances, enumerated with SetupAPI; a client tells `wb-ctl` from `wb-gfx` by reading the region header magic and the directory's role field, not by index.
- **Doorbell and interrupts:** `ivshmem-plain` has no interrupts; doorbell registration only works with `ivshmem-doorbell` plus the external `ivshmem-server`. The polling-plus-serial-kick plan of section 5 stands.
- **Access control:** default security is SYSTEM and Administrators only. That suits `wb-ctl`. `wb-gfx` must be opened by unprivileged Vulkan processes, so its device security has to be widened after installation. Editing the INF would invalidate the signed catalog, so the approach to try is for the elevated agent to set the device's security descriptor after install (SetupAPI device registry property for the security descriptor), leaving the signed package untouched. If that does not work, graphics clients would have to talk to a broker in the agent (more copies; avoid if possible).
- **Signing:** reported: the virtio-win ivshmem builds are **not WHQL signed** (test-signed or signed with Red Hat's own certificate), WHQL packages are not publicly distributed, and Looking Glass documents disabling Secure Boot in the guest firmware to load it; with Secure Boot enabled the symptom is Device Manager **Code 52** (signature invalid). Windows 11 24H2 and later feature updates may also tighten driver blocklists.
- **HVCI / memory integrity:** reported to reject older builds; not a constraint here today (see below).
- **Alternatives (research verdict):** no official signed virtio driver provides raw shared-memory rings on Windows: `virtio-pmem` has no Windows driver, `viofs` DAX is not supported on Windows, `viosock` is a socket with kernel transitions and copies (no advantage over the current virtio-serial for our purpose, **not measured**), ACPI NVDIMM needs a filesystem on top. So the ivshmem driver is the only candidate.

Measured on the user's LTSC guest (2026-11-15, read-only query): `Confirm-SecureBootUEFI` fails with "variable is currently undefined" (0xC0000100), i.e. Secure Boot is **not active** in this QEMU/OVMF setup; VBS/HVCI is off (`Win32_DeviceGuard` VBS status 0); test-signing is not enabled (`bcdedit` shows no `testsigning`). So the Secure Boot blocker does not apply today, but a driver still needs a signature that chains to a trusted root; a test-signed or self-signed package would still fail with Code 52 unless test-signing were turned on, which we do not want. The same Windows installation also boots on bare metal (possibly with Secure Boot), where the driver is never started because the device does not exist [INFERENCE].

What must be checked before committing to it:

1. **The signature of the exact package we would ship**: inspect the `.cat`/`.sys` signer (`signtool verify /kp`, `Get-AuthenticodeSignature`), install it in a build VM configured like the LTSC guest (same OVMF build, Secure Boot state, test-signing off) and confirm Device Manager shows no Code 52. Record the result; if it fails, the options are a package signed by Microsoft (attestation signing needs an EV certificate and a Partner Center account, a significant commitment) or enabling test-signing (rejected).
2. **HVCI / memory integrity** compatibility if the user ever enables it.
3. **Cache modes**: which modes the mapping IOCTL accepts and the real read speeds (section 4).
4. **Device security**: whether the agent can widen the ACL for `wb-gfx` without breaking the signature; whether a second device is a second driver instance.
5. **Redistribution**: BSD-3-Clause allows redistribution with notice; ship the driver package as a separate, explicit step of the agent (an `--install-ivshmem` style command, never silent, with uninstall, never replacing a driver the user installed). The user already removed `virtio-win.iso` from the VM, so the package must be supplied another way.
6. **EDR and Windows Update:** drivers that expose physical-memory mappings to user mode are flagged by some security products; feature updates may replace or block the driver. The agent must detect a missing or failed driver at runtime.
7. **Failure mode:** if the driver is missing, signature-rejected, or the device is absent, the agent reports it honestly in status and everything runs over virtio-serial exactly as today.

This is a change of the earlier "no kernel driver" decision for the GPU path; it is limited to a third-party, signed, transport-only driver.

## 9. Verification plan

Do these on the disposable build VM, never on the user's LTSC VM, before any lane layout is frozen:

1. Two `ivshmem-plain` devices in the Build VM; install the driver; Device Manager shows no error (for example code 52).
2. Microbenchmarks (extend `experiments/chanbench`): guest write MB/s, guest read MB/s for plain loads and `MOVNTDQA`, host read/write, ping-pong round trip with spin and with sleep plus kick, CPU use of both polling modes.
3. Contention test: a frame stream (frames lane) and an 8 GiB bulk stream at the same time; record frame latency p50/p99 and bulk throughput with bulk off and on. Pass criterion: frames and Pings are not delayed by bulk beyond a stated bound.
4. Hostile-guest test: a guest process writes garbage into each lane; the host must close the lane without reading out of bounds (run under ASan/Miri-style checks where possible).

## 10. Migration

1. Introduce the transport seam in the control protocol (host and agent), as already done for graphics.
2. Add ivshmem as a second implementation of the seam for the graphics lanes; virtio-serial remains the fallback.
3. Move control and bulk to `wb-ctl`; keep virtio-serial for handshake, wake-up and degraded mode.
4. Capability and version negotiation lets mixed host/agent versions fall back instead of garbling frames.

## 11. Open questions

- Per-handle mapping and ACLs of the chosen driver (7, 8.4).
- Real BAR read speed in the guest and the best mitigation (4).
- Whether 256 MiB for `wb-gfx` holds once DXVK runs (6).
- Whether to relax the guest-input lock during transfers once lanes exist (3).
- Polling cost on a 4-vCPU guest and the right spin/sleep thresholds (5).
