<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# WinBridge Shared-Memory Transport (ivshmem) Design

Status (2026-11-16): **design plus a measured feasibility probe; the transport itself is not implemented.** `crates/winbridge-shm` (ring, lanes, scheduler, hostile-peer hardening; host-only tests) exists, and `experiments/shmprobe/RESULTS.md` holds the Windows driver and BAR measurements from a disposable build VM. Order of work: (1) vkcube and the DXVK triangle on the current virtio-serial path (`docs/GFX-DESIGN.md` M2/M3; M2 is done), (2) this transport, (3) the Venus port to Windows. Statements marked **[UNVERIFIED]** are not settled by a measurement yet; everything marked (measured) comes from `experiments/shmprobe/RESULTS.md`. Sizes are starting values.

**Direction (user decision, 2026-11-16):** the measurements show the shared-memory path is two orders of magnitude faster than virtio-serial (6.6/7.7 GB/s end to end, 0.2 us ping-pong), so shared memory becomes the primary transport for control and bulk traffic as well as graphics, once the end-to-end test in the build VM confirms it with real agent code. virtio-serial remains only the handshake, the wake-up (kick) and the fallback. Shared memory stays opt-in (`shm = true`) until that test and a deployment to the user's guest have passed.

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
| `wb-gfx` | the elevated guest broker only (Vulkan clients reach it through the broker, section 8a) | Vulkan commands, returned frames, resource pool | 256 MiB |
| virtio-serial `org.winbridge.0` | the agent | handshake and version negotiation, wake-up kick, fallback when shared memory is unavailable | - |

Why two devices: `\\.\Global\org.winbridge.0` needs administrator rights today, which is what stops an unprivileged guest process from feeding fake host messages to the elevated agent. The driver allows **only one mapping per device** (section 8), so in the guest both devices end up opened by the elevated agent/broker anyway and the old argument "graphics clients need their own, user-accessible device" no longer applies. The remaining reasons for two devices are independent failure and capacity domains and simpler per-lane sizing; a single larger device (for example 512 MiB) is the simpler alternative and is an **open decision** (section 11).

QEMU side (sketch, per device; sizes as above):

```
-object memory-backend-file,id=wb-ctl,mem-path=/dev/shm/winbridge-ctl,size=256M,share=on
-device ivshmem-plain,memdev=wb-ctl
```

The backing files are created by the launcher with mode 0600 for the VM's user. `ivshmem-plain` is PCI 1af4:1110 with the memory in BAR2 (measured: the guest sees 1AF4:1110, subsystem 1AF4:1100, "PCI standard RAM Controller"). shm pages are committed on first touch, so a large BAR costs host RAM only for the part actually used.

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
- Memory ordering (measured, build VM): the cached mapping is coherent with the host in both directions. Keep `publish_fence()` (`sfence`) in the ring producer: with a cached mapping only non-temporal stores were reordered (147,815 index-before-data observations without it, none with it); scalar and `rep movsb` stores never were. On a write-combined mapping every store kind can be reordered without a fence.
- **Cache mode: use cached (`cacheMode = 1`) for every device and both directions (measured).** Guest bandwidth in MiB/s: cached write 13,000-24,000 and read 20,000-33,000; write-combined write about 23,900 but read 81 (scalar), 328 (AVX2 plain loads), about 7,200 (`MOVNTDQA` SSE), about 15,000 (`MOVNTDQA` AVX2); non-cached 10-330 both ways. Ring end to end with 1 M verified mixed records per direction: cached 6,628 MiB/s guest to host and 7,722 MiB/s host to guest; write-combined 4,942 and **208**; non-cached about 12. So the "guest reads are slow" problem is solved by the cached mapping and there is no per-device trade-off. One mode per device: never map the same pages with two cache modes at once.

## 5. Wake-up (doorbell)

`ivshmem-plain` has no interrupt vectors (measured: `vectors` is 0), and the doorbell path was not tested (`ivshmem-doorbell` needs the external `ivshmem-server`). Plan: adaptive polling. A consumer spins for a short, bounded time after traffic, then sleeps on a timer (order of 1 ms) and is woken by a one-byte kick over virtio-serial when a producer finds the ring empty and the consumer asleep. Measured ring ping-pong with cached mapping and spinning on both sides: p50 0.20 us, p99.9 0.40 us, max 75 us (hypervisor scheduling noise); spinning holds one core at 100% per side, while a 1 ms sleeping consumer costs about 1% of a core and a round trip of about 1.06 ms. Busy rings therefore pay no kick; idle rings must not spin.

## 6. Sizing: is 256 MiB enough?

Bandwidth is not the question (memory copies run at GB/s, far above the 253 MiB/s of virtio-serial); **capacity** is.

`wb-ctl` (256 MiB, about half used): 4 KiB header, control lanes 1 MiB each way, bulk lanes 64 MiB each way (blocks of 1 MiB, an 8 GiB file streams through the ring with back-pressure and is never resident). Plenty; 128 MiB would also do. The user chose 256 MiB; the unused part costs nothing until touched.

`wb-gfx` (256 MiB, shared by all Vulkan clients): per client roughly a 4-8 MiB command ring and a frame ring sized from the encoded-frame budget when the swapchain is created (raw 1080p BGRA is 8.3 MB per frame, so raw frames would not fit more than a few; encoded tiles are far smaller). The tight spot is the **host-visible resource pool**: DXVK allocates upload and staging memory in blocks of hundreds of MB, and zero-copy import with `VK_EXT_external_memory_host` needs those ranges to live inside the BAR. Options: (a) a managed arena with sub-allocation and streaming uploads through the ring for what does not fit; (b) make the `wb-gfx` size configurable (512 MiB or 1 GiB are the next power-of-two steps, guest address space is not a problem for a 64-bit BAR). Start with 256 MiB as decided and treat the pool size as a measured parameter once DXVK runs; do not assume it is enough for 4K texture-heavy games.

## 7. Security

- The host treats every byte of both regions as hostile: copy a record out of the ring, then validate the copy; no pointers or host addresses inside records; all offsets and lengths bounds-checked against the lane; index values sanity-checked (an index that moves backwards or past the other side's, closes the lane); no parsing in place (no time-of-check/time-of-use).
- **Device security (measured, and different from the first assumption):** the driver's default DACL on the device object gives Everyone read and write, so a standard user (only in `Users`) can open both devices and map the BAR, and because only one mapping is allowed per device, an unprivileged process could also hold the mapping and lock the agent out. The elevated agent must therefore call `SetKernelObjectSecurity` on each device interface at startup (SYSTEM and Administrators only); measured: afterwards a standard user gets access denied while SYSTEM and an elevated admin still map. The hardening does **not** survive a reboot; a per-device registry `Security` value (REG_BINARY and REG_SZ) had no effect after reboot, so the agent re-applies it every boot, before it maps. Until the DACL is applied the devices must not be trusted: the agent maps first only after hardening, and treats a failed hardening as "shared memory unavailable" (fall back to virtio-serial). The agent keeps validating paths and sizes exactly as today.
- Per-client slices of `wb-gfx` are owned by the broker (section 8a): a crashing client's slice is reclaimed on channel loss, and unprivileged processes never map the BAR, so one client cannot scribble over another through the device. Isolation between clients rests on the broker's per-client sections and on per-slice validation by both the broker and the host.
- A malicious guest cannot make the host exceed the region: the host maps the files it created, with fixed sizes.

## 8. Windows driver

Shared memory needs a **kernel-mode driver in the guest**. The user accepted installing a third-party one; we write no driver ourselves. Facts below marked (source) were read from the driver source in `virtio-win/kvm-guest-drivers-windows` (directory `ivshmem`, fetched through the GitHub API on 2026-11-15) or from the binaries inside the Looking Glass host installer; (research) means a secondary report that was not confirmed.

**Verified from source and binaries**

- **Source and license (source):** BSD-3-Clause (`ivshmem/LICENSE`), compatible with our GPL-3.0-or-later project. Looking Glass's `looking-glass-host-setup.exe` (build of 2025-03-06) ships `ivshmem.sys`, `ivshmem.inf`, `ivshmem.cat`; the `.sys` identifies itself as "Red Hat IVSHMEM Driver" by Red Hat, Inc. The current virtio-win ISO (0.1.302 build in the user's VM directory) does **not** contain an `ivshmem` directory (it has `viosock`), so the package has to come from Looking Glass's installer or a build of the source.
- **Signature (binary):** the catalog `ivshmem.cat` is signed by **"Microsoft Windows Hardware Compatibility Publisher"** (chain: Microsoft Windows Third Party Component CA 2014, Microsoft Root Certificate Authority 2010), i.e. a Microsoft (WHCP/attestation) signature, not a test certificate. This contradicts the research report that the driver is not WHQL signed. **Still to confirm on Windows**: `Get-AuthenticodeSignature` on `.cat`/`.sys` and a real install without Code 52 (the catalog inspection here was done with openssl and does not prove the hashes cover the shipped `.sys`).
- **Interface (source, `Public.h`):** device interface `GUID_DEVINTERFACE_IVSHMEM` `{df576976-569d-4672-95a0-f57e4ea0b210}`; IOCTLs (`FILE_DEVICE_UNKNOWN`, buffered, `FILE_ANY_ACCESS`): `REQUEST_PEERID` 0x800, `REQUEST_SIZE` 0x801, `REQUEST_MMAP` 0x802, `RELEASE_MMAP` 0x803, `RING_DOORBELL` 0x804, `REGISTER_EVENT` 0x805, `REQUEST_KMAP` 0x806. `REQUEST_MMAP` takes a cache mode: `IVSHMEM_CACHE_NONCACHED` 0, `CACHED` 1, `WRITECOMBINED` 2 (`Queue.c` maps the pages with `MmMapLockedPagesSpecifyCache` using `MmNonCached`, `MmCached` or `MmWriteCombined`).
- **One mapping per device (source, `Queue.c`):** the device context holds a single `shmemMap`; a second `REQUEST_MMAP` returns `STATUS_DEVICE_ALREADY_ATTACHED`. A commit "Allow multiple processes to map the same shared memory" was merged and then reverted on 2025-09-28. **Consequence: only one process in the guest can map each ivshmem device.** Unprivileged Vulkan clients cannot each map `wb-gfx`; see section 8a.
- **Interrupts (source, `ivshmem.inf`):** the INF enables MSI (`MSISupported=1`), so doorbells work with `ivshmem-doorbell`; `ivshmem-plain` has no interrupt vectors, as before.
- **Device security (measured):** the default DACL is **not** restricted; see section 7. The agent hardens it at runtime.

**Research only (unverified)**

- Looking Glass documents disabling Secure Boot for the driver. Measured: the package installs and passes the cached pattern test in a build VM with Secure Boot ON (secure OVMF with Microsoft keys, `Confirm-SecureBootUEFI = True`) as well as with Secure Boot not active, so that advice does not apply to this package. HVCI was not tested.
- The first expectation that guest reads are slow is confirmed only for the write-combined and non-cached mappings (see section 4); the cached mapping reads at 20,000-33,000 MiB/s.
- No official signed virtio driver offers raw shared-memory rings on Windows (`virtio-pmem` has no Windows driver, `viofs` DAX is Linux-only); `viosock` ships in the ISO and is signed, but it is a socket with kernel transitions and copies and was not measured.

**Measured on the user's LTSC guest** (2026-11-15, read-only query): `Confirm-SecureBootUEFI` fails with "variable is currently undefined" (0xC0000100), i.e. Secure Boot is not active in this QEMU/OVMF setup; VBS/HVCI is off (`Win32_DeviceGuard` VBS status 0); test-signing is not enabled. The same Windows installation also boots on bare metal, where the driver would never start because the device does not exist [INFERENCE].

What must still be checked before committing to it:

1. ~~Install and signature test on a build VM~~ Done (measured): `Get-AuthenticodeSignature` reports Valid for `ivshmem.cat` and `ivshmem.sys` (WHQL, "Microsoft Windows Hardware Compatibility Publisher"; the certificate expired in 2023 but the signature is timestamped); `pnputil /add-driver ivshmem.inf /install` succeeded on every boot and both devices showed Status OK, ProblemCode 0, in a VM that matches the LTSC guest (Secure Boot not active, VBS 0, test-signing off) and with Secure Boot ON.
2. **HVCI / memory integrity** compatibility if the user ever enables it: not tested.
3. ~~Cache modes~~ Done (measured, section 4): cached for every device.
4. **Redistribution**: BSD-3-Clause allows redistribution with notice; ship the package as a separate, explicit agent step (an `--install-ivshmem` style command, never silent, with uninstall, never replacing a driver the user installed), taken from a pinned, hash-checked release. The user removed `virtio-win.iso`, and it has no ivshmem anyway.
5. **EDR and Windows Update:** drivers that expose physical-memory mappings to user mode are flagged by some security products; feature updates may replace or block the driver. The agent must detect a missing or failed driver at runtime.
6. **Failure mode:** if the driver is missing, rejected, or the device is absent, the agent reports it honestly in status and everything runs over virtio-serial exactly as today.

This is a change of the earlier "no kernel driver" decision for the GPU path; it is limited to a third-party, Microsoft-signed (per the catalog), transport-only driver.

## 8a. Consequence of the single mapping: the guest broker

Because each device can be mapped by one process only, **the elevated agent (or a broker process it owns) is the only guest component that touches the BARs.** Vulkan clients in unprivileged processes talk to the broker over ordinary guest memory: per-client named shared-memory sections plus events or pipes, with ACLs restricting each section to its client and the broker. The broker multiplexes the clients onto the lanes. This changes the earlier picture:

- **Trust split by device is no longer needed for access control.** Both devices are opened only by the elevated broker, **after the agent has hardened each device's DACL at startup (section 7; the default is open to Everyone)**, so unprivileged processes never see the BARs. Whether to keep two devices (independent failure and capacity domains, simpler per-lane sizing) or use one larger device is an open decision; one device of 512 MiB is the simpler alternative.
- **One extra guest-side memcpy** (client section to BAR, and BAR to client section for returned frames) in cached ordinary RAM, which is fast; zero-copy is therefore end to end only between broker and host. `VK_EXT_external_memory_host` import still works on the host side from the BAR.
- **The broker replaces the single-handle graphics-port broker** already identified in `docs/GFX-DESIGN.md` risk 10 and must never run on the agent's control reader loop: separate threads, bounded queues, and a hard rule that a stuck client cannot stall control traffic.
- The broker is a guest-side attack surface: it parses client input, so it validates client sections the same way the host validates the BAR.


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

- Two devices or one larger device now that only the broker maps them (2, 8a).
- HVCI / memory integrity compatibility (8); the doorbell path (`ivshmem-doorbell`, not needed while polling plus kick works).
- Whether the DACL hardening can be applied early enough on every boot (before any unprivileged process maps the device), and what the agent does between boot and hardening (7).
- Whether 256 MiB for `wb-gfx` holds once DXVK runs (6).
- Whether to relax the guest-input lock during transfers once lanes exist (3).
- Polling cost on a 4-vCPU guest and the right spin/sleep thresholds (5).
