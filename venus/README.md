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
| `tests/ring_peer.c` | The guest end of the ring interoperability test of `crates/winbridge-venus`. |
| `tests/d3d/` | The D3D programs (`tri11`, `teapot11` for D3D11, `tri10` for D3D10 and `tri9` for D3D9) the driver is accepted with, run through DXVK. |
| `patches/0001`-`0011` | Patches to Mesa 26.3.0 (`0a6fa60abb73`). Apply in order (`build/build.sh` does). |
| `patches/virglrenderer/` | Patches to virglrenderer for the host: `0001` and `0002` are needed on NVIDIA 550, `0003` is the command trace. |
| `tools/compare-command-traces.py` | Compares command traces of the Windows and the Linux driver. |
| `build/` | The reproducible build of `vulkan_virtio.dll` from Linux: `build.sh`, the pins, the compiler flags. See `build/README.md`. |
| `../scripts/install-venus-icd.ps1` | Installs and registers the driver in the guest. |

## Build

`WBVN_ACCEPT_MS_LICENSE=1 build/build.sh` cross-compiles the driver with `clang-cl` and `lld-link` (LLVM 19) against the MSVC runtime and Windows SDK that it downloads with xwin, and leaves `vulkan_virtio.dll` in the work directory. Nothing is built inside the guest. The result imports only KERNEL32, USER32, GDI32, the UCRT api-sets and VCRUNTIME140. What the script needs and pins is in `build/README.md`.

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

The driver needs the Visual C++ runtime in the guest (`VCRUNTIME140.dll`; the test programs also
`MSVCP140.dll` and, through it, `VCRUNTIME140_1.dll`). A Windows 11 install does not have it until some
application brings it: without it the loader reports no driver and a program that links it
dynamically stops with 0xC0000135. Install the Visual C++ Redistributable, or put the DLLs of
Microsoft's `Microsoft.VC.<version>.CRT.Redist.X64.base` package next to the application and next to
`vulkan_virtio.dll`.

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
