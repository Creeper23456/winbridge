<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# Venus Vulkan driver for Windows guests

A Vulkan driver for the Windows guest built from Mesa's Venus (`src/virtio/vulkan`) with a
WinBridge renderer backend. Venus encodes the whole Vulkan API into a command stream; a host
process (`crates/winbridge-venus`) runs that stream on virglrenderer's `vkr` and the host GPU.
The design and its measured evidence are in `docs/adr/0001-venus-as-serialization-layer.md`.

```
Windows app (DXVK, ...) -> vulkan-1.dll -> vulkan_virtio.dll (Venus + vn_renderer_winbridge.c)
        -> virtio-serial org.winbridge.venus.0 (WBVN, see wbvn_proto.h)
        -> winbridge-venus (host) -> virglrenderer vkr / render server -> host Vulkan driver
```

## Layout

| Path | Content |
|---|---|
| `guest/vn_renderer_winbridge.c`, `guest/wbvn_proto.h` | The `vn_renderer` backend and the WBVN wire protocol. Copy them into `src/virtio/vulkan/` of the Mesa tree. |
| `guest/venus-probe.c` | Loads the driver directly and runs instance, device, submit and a buffer copy through the GPU. |
| `patches/0001`-`0008` | Patches to Mesa 26.3.0 (`0a6fa60abb73`). Apply with `git am`, in order. |
| `patches/virglrenderer/` | Patches to virglrenderer for the host: `0001` and `0002` are needed on NVIDIA 550, `0003` is the command trace. |
| `tools/compare-command-traces.py` | Compares command traces of the Windows and the Linux driver. |
| `../scripts/install-venus-icd.ps1` | Installs and registers the driver in the guest. |

## Build

The driver is cross-compiled from Linux with `clang-cl` and `lld-link` (LLVM 19) against the MSVC
runtime and Windows SDK headers and libraries copied from a Windows install. Nothing is built
inside the guest. Configure Mesa with the Vulkan driver `virtio` on the Windows platform, with a
meson cross file that points the compiler, linker and resource tools at those wrappers, then
`ninja src/virtio/vulkan/vulkan_virtio.dll`. The result imports only KERNEL32, USER32, GDI32,
the UCRT api-sets and VCRUNTIME140.

## Run

Host: start the VM with `-chardev socket,id=wv0,path=SOCK,server=on,wait=off -device
virtserialport,chardev=wv0,name=org.winbridge.venus.0`, then `winbridge-venus --socket SOCK
--virgl-lib LIBVIRGLRENDERER --render-server VIRGL_RENDER_SERVER`. The bridge sets
`VKR_DEBUG=opaque,nofenceexport` when `VKR_DEBUG` is unset; both flags come from the virglrenderer
patches and are needed on NVIDIA 550.

Guest: run `powershell -ExecutionPolicy Bypass -File scripts/install-venus-icd.ps1 -SourceDll vulkan_virtio.dll` from an elevated
PowerShell. Applications then find the driver through the Vulkan loader.

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
