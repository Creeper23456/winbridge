<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# Venus Vulkan driver for Windows guests

A Vulkan driver for the Windows guest built from Mesa's Venus (`src/virtio/vulkan`) with a
WinBridge renderer backend. Venus encodes the whole Vulkan API into a command stream; a host
process (`crates/winbridge-venus`) runs that stream on virglrenderer's `vkr` and the host GPU.
The design and its measured evidence are in `docs/adr/0001-venus-as-serialization-layer.md`.

```
Windows app (DXVK, ...) -> vulkan-1.dll -> vulkan_virtio.dll (Venus + vn_renderer_winbridge.c)
        -> two lanes in an ivshmem device the process owns (WBVN, wbvn_proto.h, wbvn_ring.h)
           + a wake-up byte over virtio-serial org.winbridge.venus.N when a side sleeps
        -> winbridge-venus (host) -> virglrenderer vkr / render server -> host Vulkan driver
```

## Layout

| Path | Content |
|---|---|
| `guest/vn_renderer_winbridge.c`, `guest/wbvn_proto.h`, `guest/wbvn_ring.h` | The `vn_renderer` backend, the WBVN wire protocol with the slot layout, and the guest's ring code (the same layout as `crates/winbridge-shm`). Copy them into `src/virtio/vulkan/` of the Mesa tree. |
| `guest/venus-probe.c` | Loads the driver directly and runs instance, device, submit and a buffer copy through the GPU. |
| `tests/vk/featprobe.c` | Creates a device with every feature the driver reports as supported (what an application that enables all it can query does) and, if that fails, names the features that do not. |
| `tests/ivshmem-probe.c` | Asks the ivshmem driver for a slot's mapping the way the driver does, with several output-buffer sizes; built for x64 and x86 it shows what the driver wants from each (24 bytes from a 32-bit caller, 32 from a 64-bit one). |
| `tests/ring_peer.c` | The guest end of the ring interoperability test of `crates/winbridge-venus`. |
| `tests/d3d/` | The D3D programs (`tri11`, `teapot11` for D3D11, `tri10` for D3D10 and `tri9` for D3D9) the driver is accepted with, run through DXVK. |
| `tests/gl/gltri.cpp` | The same triangle through WGL: a 3.3 core context with GLSL, or `--legacy` with `glBegin`. Run it with Mesa's `opengl32.dll` next to it (see OpenGL below). |
| `patches/0001`-`0013` | Patches to Mesa 26.3.0 (`0a6fa60abb73`). Apply in order (`build/build.sh` does). |
| `patches/virglrenderer/` | Patches to virglrenderer for the host: `0001` and `0002` are needed on NVIDIA 550, `0003` is the command trace. |
| `tools/compare-command-traces.py` | Compares command traces of the Windows and the Linux driver. |
| `build/` | The reproducible build of `vulkan_virtio.dll` from Linux: `build.sh`, the pins, the compiler flags. See `build/README.md`. |
| `../scripts/install-venus-icd.ps1` | Installs and registers the driver in the guest. |

## Build

`WBVN_ACCEPT_MS_LICENSE=1 build/build.sh` cross-compiles the driver with `clang-cl` and `lld-link` (LLVM 19) against the MSVC runtime and Windows SDK that it downloads with xwin, and leaves `vulkan_virtio.dll`, `opengl32.dll` and `libgallium_wgl.dll` for x64 in `out/` and for x86 in `out32/` of the work directory (`WBVN_ARCHS=x64` or `x86` builds one). Nothing is built inside the guest. The DLLs use the static CRT and import only system libraries (KERNEL32, USER32, GDI32, SETUPAPI for the driver, and an api-set). What the script needs and pins is in `build/README.md`.

## OpenGL

OpenGL runs on the same driver through Zink, Mesa's OpenGL-on-Vulkan driver. `build/build.sh` also builds Mesa's Windows OpenGL for x64 and x86: `opengl32.dll` (a small stub) loads `libgallium_wgl.dll` (WGL, the GLSL compiler, Zink). Both are self-contained (static CRT and zlib, system DLLs only). Zink loads the system's `vulkan-1.dll`, finds the driver through the registry like any Vulkan program, and creates its window surface through the driver's software WSI. No patch to Mesa was needed for it.

Put `opengl32.dll` and `libgallium_wgl.dll` next to the program's `.exe`: the loader searches the program's directory before System32, so only that program uses Mesa, and no other file is needed (the self-contained build was run this way with `gltri`; a copy of FurMark 2, which had said "OpenGL 1.1 detected", ran with OpenGL 4.6 compatibility profile on the Venus driver when the same two DLLs and their runtime files sat next to it). A 32-bit program takes the DLLs from `out32/` (`gltri` x86 ran, ADR 3.10); it needs a 32-bit `vulkan-1.dll`, which the guest had in `SysWOW64`. `GALLIUM_DRIVER` need not be set (Zink is the only driver in the build).

It cannot be registered for all programs this way: `OpenGLDriverName`, `OpenGLVersion` and `OpenGLFlags` on the adapter's class key (`HKLM\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}\<n>`, the way vendor drivers register an ICD) made no difference for the virtio-gpu display-only adapter, with two value pairs: programs without the Mesa files kept `GDI Generic` 1.1. Replacing `C:\Windows\System32\opengl32.dll` is not done: it belongs to TrustedInstaller and the same Windows partitions also boot on the real GPU. `install-venus-icd.ps1` does not install the OpenGL DLLs.

Programs that leave the swap interval at its default of 1 are paced to about 14.7 ms per frame (see section 3.9 of the ADR); a program that asks for interval 0 is not.

## Run

Host: a *slot* is one 64 MiB `ivshmem-plain` device plus one virtio-serial port. Create the
backing file once (`/dev/shm/winbridge-venus0`, mode 0600, exactly 64 MiB) and start the VM with

```
-object memory-backend-file,id=wbv0,mem-path=/dev/shm/winbridge-venus0,size=64M,share=on
-device ivshmem-plain,memdev=wbv0
-chardev socket,id=wv0,path=SOCK,server=on,wait=off
-device virtserialport,chardev=wv0,name=org.winbridge.venus.0
```

then `winbridge-venus --shm /dev/shm/winbridge-venus0 --socket SOCK --virgl-lib LIBVIRGLRENDERER
--render-server VIRGL_RENDER_SERVER`. The process serves the slot for ever: each guest that claims
it gets a fresh Venus context, and the slot is reset when the guest closes it, exits or stops
showing signs of life for 10 s. The bridge sets `VKR_DEBUG=opaque,nofenceexport` when `VKR_DEBUG` is
unset; both flags come from the virglrenderer patches and are needed on NVIDIA 550.

The slot's number (`--slot N`, default 0) is written into it and names the serial port, so slot N
needs the port `org.winbridge.venus.N`. A guest Vulkan process maps one device for itself, so one
slot serves one Vulkan instance at a time; a second one fails to initialise cleanly (within
`WBVN_CONNECT_MS`, 5 s by default).

Guest: the ivshmem driver must be installed (`docs/IVSHMEM-TRANSPORT.md` section 8), then run
`powershell -ExecutionPolicy Bypass -File scripts/install-venus-icd.ps1 -SourceDll vulkan_virtio.dll` from an elevated
PowerShell. Applications then find the driver through the Vulkan loader. `WBVN_SPIN_US` (default
150) is how long the driver's reader polls for the host before it sleeps.

The slots' serial ports are created for administrators only. The elevated `winbridge-agent` opens them
to the interactive users when it starts (`org.winbridge.venus.N`, live, not persistent), so a
Vulkan process with a normal token can use them; without the agent only an elevated process can.

The driver itself needs no Visual C++ runtime: the DLLs are built with the static CRT (3.8 of the ADR
describes the 0xC0000135 that the dynamic build ran into). A program that links the runtime
dynamically, such as the D3D test programs, still needs `VCRUNTIME140.dll` (and `MSVCP140.dll` with
`VCRUNTIME140_1.dll`) in the guest: install the Visual C++ Redistributable, or put the DLLs of
Microsoft's `Microsoft.VC.<version>.CRT.Redist.X64.base` package next to that program.

## Measuring

`WBVN_STATS=FILE` in the guest's environment makes the driver append one line of counters per second to FILE (submits, renderer waits with a histogram, ring waits, writes, lock waits, reader and pump activity), a line of totals at the end, and at start-up the channel's ping round trips and the length of `Sleep(1)`. `winbridge-venus --stats` logs the host's side once a second: the serve loop by phase, messages and bytes by kind, `submit_cmd`, fence latency with a histogram and the time spent writing. `--stats` also works with `--replay`, which feeds a recording made with `--record` to the host without a guest. Section 3.7 of the ADR shows what these found.

## Licences

The driver is a combined work. The WinBridge files in `guest/` are GPL-3.0-or-later. The Mesa and
venus-protocol code it links is MIT and keeps its own copyright and licence headers; MIT is
compatible with distributing the whole under the GPL. The patches under `patches/` change MIT
files and are offered under the same MIT licence as the files they modify. See `NOTICE.md` for the
other components linked into `vulkan_virtio.dll`.

## Checking the command stream against Mesa's Linux driver

`patches/virglrenderer/0003` adds `VKR_CMD_TRACE=path`: virglrenderer then appends one line per
decoded command (source, type, length, FNV-1a hash). Run `venus-probe basic` once through the
Windows driver with `winbridge-venus` and once on Linux against Mesa's `libvulkan_virtio.so`
through `virgl_test_server` (same Mesa commit, same `VN_PERF`, the same physical device), then
`tools/compare-command-traces.py LINUX_TRACE WINDOWS_TRACE`. `basic` skips the host-visible memory
test, which Mesa's vtest client cannot map on NVIDIA 550. Ring commands must match line for line.
