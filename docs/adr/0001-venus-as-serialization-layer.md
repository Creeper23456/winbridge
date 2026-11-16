<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# ADR 0001: Venus as the Vulkan serialization layer on Windows guests

- Status: **Proposed** (evidence gathered 2026-11-16; the Windows port is not started)
- Context owner: 3D acceleration (`docs/GFX-DESIGN.md`)
- Related: `docs/IVSHMEM-TRANSPORT.md` (shared-memory transport), `docs/GFX-DESIGN.md` (current hand-written ICD)

## 1. Context

The guest ICD (`winbridge-gfx-icd`) is hand-written. It forwards Vulkan calls to the host dispatcher and emulates host-visible memory with command-driven readback (`docs/GFX-DESIGN.md`, "M3 result"). It renders a DXVK d3d11 triangle (avg 4.3 ms per frame in the build VM) but every new command has to be written, wired and tested by hand; known gaps are listed in that document.

Mesa's Venus driver (`src/virtio/vulkan`, MIT) generates the encoding of the whole Vulkan API from `vk.xml` (the venus-protocol repository), and virglrenderer's `vkr` (host side) decodes and executes it. The user's intent (2026-11-16): **use Venus only to solve the serialization problem.** Memory sharing, transport and presentation stay WinBridge's own (ivshmem lanes, readback into the guest window; no overlay).

## 2. Decision (proposed)

1. Guest ICD = Mesa Venus (`vn_*` object tracking, sync, encoder) with a **new WinBridge `vn_renderer` backend** over the ivshmem transport, replacing the POSIX-only `vn_renderer_virtgpu.c` and `vn_renderer_vtest.c`.
2. Host = virglrenderer `vkr` (`vkr_context_submit_cmd`) fed from the ring, replacing the current dispatcher once parity is shown.
3. **No blob / dma-buf resources.** The guest never relies on host-exported dma-buf memory. Only `DEVICE_LOCAL` memory is allocated on the host; host-visible behaviour is provided by guest shadow memory plus explicit transfers through the ivshmem lanes. Detail of the exposed memory types is an open design item (section 6).
4. The existing hand-written ICD stays as the fallback until Venus passes the acceptance in section 5.
5. Toolchain: **`clang-cl` (`--target=x86_64-pc-windows-msvc`)**, decided by the user on 2026-11-16. The only reason to prefer `cl` was Windows compatibility and familiarity; `clang-cl` produces the same ABI, links the same CRT and SDK and emits PDBs, and code signing does not depend on the compiler. It accepts the GNU syntax Mesa uses, so Mesa's shared headers need no patches for it. Native `cl` is dropped.

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

## 4. Options considered

| Option | Effect | Cost / risk |
|---|---|---|
| A. Keep the hand-written ICD, add commands as needed | No new dependency | Every Vulkan command and extension by hand; coverage gap stays |
| B. Venus encoder + WinBridge backend, no blob (this ADR) | Whole API encoded by generated code; fits the ivshmem design; works on host driver 550 | Port work; guest memory-type design (6); toolchain choice |
| C. Venus with upstream blob/dma-buf memory | Upstream behaviour | Needs host driver 570.86 or later (upgrade is the user's call); needs a dma-buf equivalent inside a VM guest that does not exist here |
| D. Re-implement Venus natively without Mesa code | No third-party code in the guest | Largest effort; conformance with Mesa would need continuous proof |

## 5. Acceptance before anything is pushed

1. Command-stream consistency in a Linux container: the same Vulkan call sequence must produce the same Venus byte stream as Mesa's own driver (record with the logging tap used in 3.1), and the same stream through virglrenderer must render the same image.
2. The DXVK d3d11 triangle runs through the new path in the build VM and matches the current ICD's result.
3. A teapot-class test (depth, textures, constant buffers) renders on both paths.
4. Mesa and venus-protocol copyright and licence headers are preserved; nothing in the repository carries private paths or host names.

## 6. Open design items

- **Memory types exposed to the guest.** Venus advertises the host's `HOST_VISIBLE` types. With no blob, either the backend strips them and an emulation layer provides mappable memory through shadow buffers, or the host side must not export. Which of these is the least invasive (guest-side change in `vn_physical_device` or host-side filter in `vkr`) is undecided.
- **Where shadow memory is synchronised:** reuse the command-driven readback already in the current ICD, or explicit transfer commands.
- **Synchronisation primitives** of the new `vn_renderer` (timeline counters over the ring, wait in the guest) and how the wake-up interacts with the poll-and-kick scheme in `docs/IVSHMEM-TRANSPORT.md` (idle wake-up latency is acceptable; steady state must not degrade).
- **Toolchain:** decided, `clang-cl` cross-compiling from Linux (`--target=x86_64-pc-windows-msvc`, `lld-link`) against MSVC and Windows SDK headers and libraries copied from a Windows install; nothing is built inside the guest. Linking has not been tried yet; the first step is a small DLL that links and loads in the build VM.
- **Host driver:** 550.163.01 works for the serialization-only path. Upgrading to 570.86 or later is not required by this decision.

## 7. Not verified

- Linking, exported symbols and any execution of Mesa code on Windows.
- Venus behaviour with host-visible memory on a driver of 570.86 or later (not tried).
- A full render-and-readback path through Venus on NVIDIA 550 (only clear and fence were tested).
- Memory-type stripping or shadow buffering of the Venus guest (design only).
- Container tooling: docker was unusable without root, so the build used a user prefix; the byte-for-byte stream comparison of 5.1 has not been run.
