<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# ADR 0001: Venus as the Vulkan serialization layer on Windows guests

- Status: **Proposed** (evidence gathered 2026-11-16; the Windows port runs a buffer-copy probe through NVIDIA 550 in the build VM; acceptance in section 5 is not complete)
- Context owner: 3D acceleration (`docs/GFX-DESIGN.md`)
- Related: `docs/IVSHMEM-TRANSPORT.md` (shared-memory transport), `docs/GFX-DESIGN.md` (current hand-written ICD)

## 1. Context

The guest ICD (`winbridge-gfx-icd`) is hand-written. It forwards Vulkan calls to the host dispatcher and emulates host-visible memory with command-driven readback (`docs/GFX-DESIGN.md`, "M3 result"). It renders a DXVK d3d11 triangle (avg 4.3 ms per frame in the build VM) but every new command has to be written, wired and tested by hand; known gaps are listed in that document.

Mesa's Venus driver (`src/virtio/vulkan`, MIT) generates the encoding of the whole Vulkan API from `vk.xml` (the venus-protocol repository), and virglrenderer's `vkr` (host side) decodes and executes it. The user's intent (2026-11-16): **use Venus only to solve the serialization problem.** Memory sharing, transport and presentation stay WinBridge's own (ivshmem lanes, readback into the guest window; no overlay).

## 2. Decision (proposed)

1. Guest ICD = Mesa Venus (`vn_*` object tracking, sync, encoder) with a **new WinBridge `vn_renderer` backend** (`venus/guest/vn_renderer_winbridge.c`, protocol `wbvn_proto.h`), replacing the POSIX-only `vn_renderer_virtgpu.c` and `vn_renderer_vtest.c`. Transport: a virtio-serial port `org.winbridge.venus.0`; moving it onto the ivshmem lanes is future work (serial measured 220-390 MiB/s).
2. Host = `crates/winbridge-venus` on virglrenderer `vkr` (render-server process), fed from the mirrored ring.
3. **Memory (amended 2026-11-16, replaces "no blob").** Shmem (command rings, command-stream pools) and `VkDeviceMemory` are *mirrored* between guest and host by copy, not shared: the guest copy is a `VirtualAlloc(MEM_WRITE_WATCH)` region diffed against a snapshot, pushed at submit/flush; GPU writes come back when a fence retires. Host-visible `VkDeviceMemory` is a mappable HOST3D blob on the host, mapped by the bridge. NVIDIA 550 exports dma-bufs it cannot mmap, so the host runs a virglrenderer patch (`VKR_DEBUG=opaque`, `venus/patches/virglrenderer/0001`) that exports an opaque fd instead. Guest event, semaphore and query feedback are disabled (`VN_PERF=no_event_feedback,no_semaphore_feedback,no_query_feedback`) because they would poll mapped device memory.
4. **Sync fences are created without sync-fd export** (`VKR_DEBUG=nofenceexport`, patch `0002`). Evidence: replaying a recorded guest session (`--record`/`--replay`) segfaulted at a fixed offset in `libnvidia-glcore.so.550.163.01` in the `vkr-queue` thread (`vkWaitForFences`) on every run; with `VK_EXPORT_FENCE_CREATE_INFO` removed the same replay and the VM probe both pass. The bridge sets `VKR_DEBUG=opaque,nofenceexport` when the variable is unset. Cause inside the driver is not known.
5. The existing hand-written ICD stays as the fallback until Venus passes the acceptance in section 5.
6. Toolchain: **`clang-cl` (`--target=x86_64-pc-windows-msvc`)**, decided by the user on 2026-11-16. The only reason to prefer `cl` was Windows compatibility and familiarity; `clang-cl` produces the same ABI, links the same CRT and SDK and emits PDBs, and code signing does not depend on the compiler. Native `cl` is dropped. Result: Mesa 26.3.0 plus eight local patches (`venus/patches/`) cross-compiles clean and the resulting `vulkan_virtio.dll` loads and runs in the build VM.
7. **Presentation uses Mesa's software WSI** (`wsi_common_win32.cpp`, the GDI path lavapipe and dzn use), enabled for Venus by patch `0007`: `VK_KHR_win32_surface` and `VK_KHR_swapchain` are exposed, swapchain images are linear host-visible images that the guest reads through the mirrored mapping and blits to the window. The backend reports `has_implicit_fencing = true` (no sync files exist); the software WSI waits on a fence before it reads an image and a bo is brought up to date when that fence retires. Fence imports with fd `-1` mean "already signaled", as upstream defines.
8. **Only cached host-visible memory types are exposed** (patch `0008`). The host reads mirrored memory after every fence; on NVIDIA 550 the write-combined and BAR host-visible types are uncached for CPU reads, and one fence retire that scanned about 10 MiB of them took about 490 ms (measured through host-side phase timing in a replay of a recorded session). Non-cached types keep their index but lose `HOST_VISIBLE` and `HOST_COHERENT`.

## 3. Evidence

### 3.1 Venus over vtest on a Linux host (Mesa 26.3.0, virglrenderer 1.3.0, built in a user prefix; no system packages installed)

| Host Vulkan driver | Result of a 256x256 offscreen triangle with a host-visible readback buffer |
|---|---|
| lavapipe (CPU) | Pass. 8193 distinct colours, centre pixel (127, 65, 63). Command stream recorded (916 bytes client to server, 314 bytes server to client). |
| NVIDIA RTX 3060 Ti, driver 550.163.01 | Fail. Default: the client blocks in `vkWaitForFences` after `vkQueueSubmit`. With `VKR_DEBUG=udmabuf`: `failed to look up object 10 of type 8`, `vkBindBufferMemory2 resulted in CS error`. The same program without Venus renders correctly on this GPU. |

Root cause of the failure (investigated by reading source and by a separate import probe):

- `vkr_device_memory.c` imports a udmabuf fd into the host driver for any allocation with `HOST_VISIBLE`. On driver 550.163.01 the import succeeds for the non-host-visible memory types and fails with `VK_ERROR_OUT_OF_DEVICE_MEMORY` (-2) for every host-visible type. `vkGetMemoryFdPropertiesKHR` on the fd returns -13.
- The Venus guest submits `vkAllocateMemory` asynchronously (`vn_device_memory.c`), so the application sees success; the later `vkBindBufferMemory2` then references an object the host never created. With `VN_PERF=no_async_mem_alloc` the error surfaces directly as `vkAllocateMemory -> -2`.
- Mesa's `docs/drivers/venus.rst` lists NVIDIA 570.86 or later as the tested proprietary driver. This is consistent with the failure; a driver upgrade was **not** tried.
- The stall without `VKR_DEBUG` is explained in the investigation as a consequence of dma-buf export on driver 550 [INFERENCE]; it was not isolated further.

### 3.2 Venus without host-visible memory on the same NVIDIA 550 host (measured)

A test program (instance, device, queue; 256x256 RGBA8 image in a `DEVICE_LOCAL` type; barrier; `vkCmdClearColorImage`; `vkQueueSubmit` with a fence; `vkWaitForFences`) ran through Venus over vtest on NVIDIA 550.163.01: `vkWaitForFences` returned 0 immediately, client exit code 0, no server errors, both with the default server and with `VKR_DEBUG=udmabuf`.

Limits of this test: it clears an image and does not copy back or sample; it proves allocation, bind, submit and fence completion without blob, not a full rendering path.

### 3.3 Windows toolchain inventory (compile only, first error per file, 193 files of the Venus ICD and its Mesa dependencies; no link, nothing run)

| Configuration | Clean of 193 |
|---|---|
| MSVC `cl /c`, Mesa flags as is | 28 (first error in most `vn_*` files: C11 atomics disabled) |
| MSVC `cl /c` with `/experimental:c11atomics` | 28 (26 `vn_*` files stop at the GNU `__attribute__` in `ralloc.h`; 63 files at `pthread.h`; 35 at `endian.h`) |
| `clang-cl` (MSVC and SDK headers) | 28 |
| `clang-cl` + case-insensitive include overlay | 38 |
| `clang-cl` + overlay + Mesa's native `_WIN32` endian path | 63 |
| `clang-cl` + overlay + endian + native Win32 threads | 77 |

- The same 28 files compile with both compilers; `clang-cl` adds 10 that need no header edits because it accepts GNU `__attribute__`, `packed` and `aligned`.
- Backends: `vn_renderer_virtgpu.c` needs DRM (no Windows equivalent); `vn_renderer_vtest.c` is POSIX sockets; neither is reusable as is.
- Remaining errors after those switches (for example `_Interlocked*` intrinsics in `u_atomic.h`) were not worked through; the estimates of what a header patch set would unlock are [INFERENCE].

### 3.4 Venus in the build VM (measured 2026-11-16)

Mesa Venus `vulkan_virtio.dll` installed with `scripts/install-venus-icd.ps1`, host `winbridge-venus` on virglrenderer with the two patches, NVIDIA RTX 3060 Ti, driver 550.163.01:

| Test | Result |
|---|---|
| `venus-probe.exe`: instance, device, empty submit, buffer copy through the GPU | Pass, exit 0 |
| DXVK 3.1.1 d3d11 triangle (`tri11.exe`, 640x360, 120 frames), three runs | Pass 3 of 3; frame **bit-identical** to the frame the hand-written ICD produced (0 of 230400 pixels differ); avg 37.3, 41.0, 43.2 ms per frame, first frame 330-420 ms |
| Hand-written ICD, same program | avg 4.48 ms per frame |

The Venus path is about nine times slower per frame than the hand-written ICD. Causes not yet separated: round trips through the pump thread, fence retire and copy-back of the swapchain image, serial throughput. Defects found and fixed on the way, each with its evidence in the commit history: `Sleep(1)` lasting 15.6 ms (timer resolution), uncached memory scan (8), sync-file semantics (7), ring tail pushed ahead of its commands (the guest read the tail after the write-watch list, so a tail could arrive without the page behind it; one in several runs ended in `vkUpdateDescriptorSets resulted in CS error`).

### 3.5 Command streams compared with Mesa's Linux driver (measured 2026-11-16)

`venus-probe basic` (instance, enumerate, properties, device, queue, fence, an empty submit, waits, destroy) ran once through the Windows driver and once through Mesa's Linux `libvulkan_virtio.so` (same Mesa commit, same `VN_PERF`, same physical device, vtest). virglrenderer logged every decoded command with `VKR_CMD_TRACE` (type, length, FNV-1a hash); `venus/tools/compare-command-traces.py` compared the two traces:

| Stream | Linux | Windows | Result |
|---|---|---|---|
| Ring commands | 34 | 34 | 34 identical in type, length and content hash |
| Context commands | 4 | 4 (plus 37 `vkNotifyRingMESA` issued by the host bridge) | Same types and lengths; the hashes of `vkCreateRingMESA`, `vkSubmitVirtqueueSeqnoMESA` and `vkDestroyRingMESA` differ |

The three differing commands carry the ring id, which Mesa sets to the address of the ring object (`ring->id = (uintptr_t)ring`), and the shmem resource id, which each backend assigns; both are process-specific by construction [INFERENCE from the source; the differing bytes were not dumped on the Linux side].

Limit: this covers the basic call sequence only. A draw, memory and presentation have no Linux counterpart in this run (Mesa's vtest client cannot map host-visible memory on NVIDIA 550). For those, the evidence is that virglrenderer's strict decoder accepted every command in the DXVK runs and the frame matched the hand-written ICD bit for bit (3.4).

### 3.6 Teapot-class scene on both paths (measured 2026-11-16)

`crates/winbridge-gfx-probe/d3d11/teapot11.cpp` draws a procedural pot (lathe body, lid, knob, spout, handle) and a ground plane with a depth buffer, two indexed draws, a sampled 64x64 checker texture with a wrap sampler and a constant buffer updated between the draws (640x360, frame 59 of a fixed rotation), through DXVK 3.1.1 d3d11:

| Path | Result |
|---|---|
| Venus | Pass, exit 0; avg 47.9 ms per frame, first frame 399 ms. The frame was inspected: the handle, spout and lid occlude correctly, the texture is sampled and the surfaces are lit (107620 non-background pixels, 1454 distinct colours). |
| Hand-written ICD | Fail: `sc->Present` returns `E_FAIL`; the dispatcher logs `unknown VkSampler handle id 36` and closes the session, DXVK reports `VK_ERROR_DEVICE_LOST` from `vkCreateDescriptorPool`. The cause was not investigated. |

There is no second frame to compare against, so the pixel check for this scene is the visual one; a bit-for-bit comparison is only available where both paths render, as for the triangle (3.4). The frame was not compared against any third renderer.

## 4. Options considered

| Option | Effect | Cost / risk |
|---|---|---|
| A. Keep the hand-written ICD, add commands as needed | No new dependency | Every Vulkan command and extension by hand; coverage gap stays |
| B. Venus encoder + WinBridge backend, no blob (this ADR) | Whole API encoded by generated code; fits the ivshmem design; works on host driver 550 | Port work; guest memory-type design (6); toolchain choice |
| C. Venus with upstream blob/dma-buf memory | Upstream behaviour | Needs host driver 570.86 or later (upgrade is the user's call); needs a dma-buf equivalent inside a VM guest that does not exist here |
| D. Re-implement Venus natively without Mesa code | No third-party code in the guest | Largest effort; conformance with Mesa would need continuous proof |

## 5. Acceptance before anything is pushed

1. Command-stream consistency with Mesa's own driver: the same Vulkan call sequence must produce the same Venus byte stream, and the same stream through virglrenderer must render the same image. **Done for the basic call sequence** (section 3.5); draw and presentation streams are covered by the decoder acceptance and the pixel result in 3.4, not by a Linux comparison.
2. The DXVK d3d11 triangle runs through the new path in the build VM and matches the current ICD's result. **Done** (section 3.4).
3. A teapot-class test (depth, textures, constant buffers) renders on both paths. **Venus passes; the hand-written ICD fails on a sampler handle** (section 3.6), the kind of coverage gap this ADR is meant to close.
4. Mesa and venus-protocol copyright and licence headers are preserved; nothing in the repository carries private paths or host names.

## 6. Open design items

- **Memory types exposed to the guest (decided).** Cached host-visible types are exposed and the others are hidden by clearing their host-visible bits (section 2.8); the mirrored design in section 2.3 makes mapping work. Cost: copies at submit and fence, so large frequently written buffers are slower than true sharing.
- **Where mirrored memory is synchronised:** guest writes are pushed at submit and at `vkFlushMappedMemoryRanges`; host writes are scanned and returned when a fence retires. Bos above 2 MiB are not host-scanned.
- **Synchronisation primitives** of the new `vn_renderer` (timeline counters over the ring, wait in the guest) and how the wake-up interacts with the poll-and-kick scheme in `docs/IVSHMEM-TRANSPORT.md` (idle wake-up latency is acceptable; steady state must not degrade).
- **Toolchain:** decided, `clang-cl` cross-compiling from Linux (`lld-link`) against MSVC and Windows SDK headers and libraries copied from a Windows install; nothing is built inside the guest. The sysroot currently lives outside the repository and a reproducible build script is still to be written.
- **Host driver:** 550.163.01 works for the serialization-only path. Upgrading to 570.86 or later is not required by this decision.

## 7. Not verified

- Venus on a driver of 570.86 or later (not tried).
- Frame time of the Venus path (about 40 ms against 4.5 ms) is measured but its causes are not separated; bos above 2 MiB are not host-scanned.
- Idle CPU cost of the guest pump thread (spins 4000 iterations, then `Sleep(1)`).
- Container tooling: docker was unusable without root, so the Linux side of the comparison ran natively in a user prefix, not in a container.
