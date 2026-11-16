<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# Building the Venus guest ICD

`build.sh` cross-builds `vulkan_virtio.dll` on Linux with clang-cl and lld-link, against the MSVC CRT and the Windows SDK that it fetches from Microsoft's public channel. Nothing from a Windows machine is needed, and nothing from Microsoft is stored in this repository.

```sh
WBVN_ACCEPT_MS_LICENSE=1 venus/build/build.sh      # all stages; the DLL ends up in $WBVN_BUILD_DIR/out
```

The work directory is `~/.cache/winbridge-venus-build` unless `WBVN_BUILD_DIR` says otherwise. Stages can be run alone (`build.sh mesa build`), and a finished stage is cheap to run again.

## What it needs

- Debian packages `clang-19`, `lld-19`, `llvm-19`, `ninja-build`, `git`, `curl`, `python3`; `cargo` (to install xwin) and, for the Python environment, `uv` or `python3-venv`.
- Network access to gitlab.freedesktop.org (Mesa), github.com and wrapdb.mesonbuild.com (meson's wrapped subprojects), crates.io and download.visualstudio.microsoft.com (xwin and what it downloads), pypi.org.
- Acceptance of the Microsoft Software License Terms for the CRT and the SDK: `WBVN_ACCEPT_MS_LICENSE=1`. The script refuses to download without it.

## What it pins (`pins.env`)

| Item | Pin |
|---|---|
| Mesa | `0a6fa60abb73` (mesa-26.3.0); the patches in `../patches/0*.patch` are applied in order, then `../guest/vn_renderer_winbridge.c` and `wbvn_proto.h` are copied in |
| Wrapped subprojects | whatever that Mesa commit's `subprojects/*.wrap` name: venus-protocol v1.1.3, DirectX-Headers v1.619.1, zlib 1.3.1 |
| Compiler | LLVM 19 (built with Debian's 19.1.7) |
| Python | meson 1.12.1 and the packages listed in `PY_PACKAGES` |
| xwin | 0.10.0, Visual Studio 18 manifest, MSVC 14.52, SDK 10.0.26100 |
| Sysroot check | SHA-256 of `msvcrt`, `oldnames`, `msvcprt`, `vcruntime`, `ucrt` and `kernel32` import libraries, compared after unpacking |

`WBVN_SYSROOT` points the build at an existing `clang-cl -winsysroot` tree instead (`VC/Tools/MSVC/<version>` and `Windows Kits/10/{Include,Lib}/<version>`), for example one made from a Visual Studio install.

## How the pieces fit

- `windows_compat.h` is force-included in every C and C++ file. Mesa uses the lowercase `_interlockedadd` family that MSVC provides as intrinsics and clang-cl does not.
- `make-overlay.py` writes a case-insensitive clang VFS overlay over the include directories, because Mesa and the SDK include `<BaseTsd.h>` and `"DriverSpecs.h"` where the files are lowercase. A copy of the headers or a set of symbolic links would do the same.
- `-D_ALLOW_COMPILER_AND_STL_VERSION_MISMATCH` lets the MSVC 14.52 STL accept clang 19.

## Verified (2026-11-16)

A run from nothing (empty work directory, every stage in order) on the development host, with no change to the script in between:

| Stage | Result |
|---|---|
| `tools`, `venv` | meson 1.12.1 in a `uv` environment |
| `sysroot` | xwin 0.10.0 built and run with the flags in `build.sh`; produced `VC/Tools/MSVC/14.52` and `Windows Kits/10/{Include,Lib}/10.0.26100` (653 MB); the SHA-256 check of the six pinned import libraries passed, which is the only comparison of the downloaded SDK files with the SDK the first DLL was built against (`kernel32` and `ucrt`) |
| `overlay`, `toolchain` | overlay of 2864 headers; wrappers and cross file written |
| `mesa` | Mesa `0a6fa60abb73` fetched from gitlab; patches 0001-0009 applied |
| `build` | meson configured (DirectX-Headers, venus-protocol and zlib downloaded) and ninja linked the DLL: 5,354,496 bytes, exports `vk_icdGetInstanceProcAddr`, `vk_icdGetPhysicalDeviceProcAddr` and `vk_icdNegotiateLoaderICDInterfaceVersion`, imports KERNEL32, USER32, GDI32, VCRUNTIME140 and UCRT api-sets |

About 14 minutes in all, 6 of them compiling xwin. The DLL differs in size by 2.5 KB from the one built with the hand-made sysroot (5,357,056 bytes); the cause was not investigated. It ran in the build VM: the DXVK triangle and teapot frames were bit-identical to those of the hand-built DLL (0 of 230400 pixels differ), 4.32 ms per frame for the triangle (600 frames) and 4.46 ms for the teapot (1200 frames), no host error and no crash dump. The build prints a number of upstream warnings (misleading indentation in `nir_builder.h`, unused functions, one `-Wpointer-sign`); none is in WinBridge code.

Not verified: other LLVM versions, other Linux distributions, a sysroot made from a Visual Studio install through `WBVN_SYSROOT`, and the other SDK libraries the link reads (only six are hash-checked).
