<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# Third-party components in vulkan_virtio.dll

`vulkan_virtio.dll` is built from the Mesa 26.3.0 source tree with the patches in `patches/`. The
library list below is the static link line of that build (`ninja -t commands`). Licence texts
travel with the sources; a binary distribution must carry them.

| Component | Where it comes from | Licence |
|---|---|---|
| Mesa (Venus driver, `src/vulkan` runtime and WSI, `src/util`, `src/compiler` NIR/SPIR-V front end) | Mesa 26.3.0 | MIT (per file headers; Mesa's `licenses/` directory) |
| venus-protocol 1.1.3 (generated encoders) | `gitlab.freedesktop.org/virgl/venus-protocol` | MIT |
| BLAKE3 1.8.2 | `src/util/blake3` in Mesa, upstream `BLAKE3-team/BLAKE3` | CC0-1.0 or Apache-2.0 according to upstream [not re-verified here] |
| zlib 1.3.1 | Mesa subproject | zlib licence |
| DirectX-Headers 1.0 (GUIDs and format properties) | Mesa subproject | MIT, Copyright (c) Microsoft Corporation |
| Vulkan headers and Vulkan Video headers | Mesa `include/` | Apache-2.0 or MIT per file header [not re-verified here] |

The DLLs are linked with the static MSVC CRT and UCRT (`/MT`, from the CRT and SDK packages that
`build/build.sh` downloads under Microsoft's licence terms, which it makes you accept). Microsoft's
terms for redistributing those libraries inside a binary were not reviewed here; check them before
distributing the DLLs.

## opengl32.dll and libgallium_wgl.dll (OpenGL through Zink)

The same Mesa 26.3.0 build also links Mesa's OpenGL for Windows: `opengl32.dll` (the `libgl-gdi`
stub) loads `libgallium_wgl.dll`, which holds the WGL frontend, the GLSL compiler, the Mesa state
tracker, NIR and the Zink driver (OpenGL on Vulkan, `src/gallium/drivers/zink`). Mesa's sources are
MIT-licensed per file header [not re-verified file by file here]; zlib 1.3.1 is the same subproject
as above and is linked in statically. The licence texts of Mesa (`licenses/`) travel with the sources.
