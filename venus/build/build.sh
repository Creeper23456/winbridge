#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Cross-builds the Venus guest ICD (vulkan_virtio.dll) on Linux with clang-cl and lld-link.
# See README.md for what it needs and what has been verified.
#
#   WBVN_ACCEPT_MS_LICENSE=1 ./build.sh            all stages
#   ./build.sh mesa build                          only these stages
#
# Stages, in order: tools venv sysroot overlay toolchain mesa build.
# Environment:
#   WBVN_BUILD_DIR   work directory (default: ~/.cache/winbridge-venus-build)
#   WBVN_SYSROOT     use this clang-cl -winsysroot style tree instead of fetching one with xwin
#   LLVM_BIN         directory of clang-cl, lld, llvm-lib, llvm-rc (default /usr/lib/llvm-<major>/bin)
#   WBVN_ACCEPT_MS_LICENSE=1   accept the Microsoft Software License Terms for the CRT and SDK
#                    that the sysroot stage downloads; it will not download without it

set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd "$here/../.." && pwd)
# shellcheck source=pins.env
. "$here/pins.env"

work=${WBVN_BUILD_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/winbridge-venus-build}
llvm=${LLVM_BIN:-/usr/lib/llvm-$LLVM_MAJOR/bin}
sysroot=${WBVN_SYSROOT:-$work/sysroot}

say() { printf '==> %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

# Where the sysroot keeps the CRT and the SDK; set by locate_sysroot.
crt="" sdk="" sdkver=""

locate_sysroot() {
    local msvc="$sysroot/VC/Tools/MSVC"
    [ -d "$msvc" ] || die "$sysroot is not a -winsysroot tree (no VC/Tools/MSVC)"
    crt=$(find "$msvc" -mindepth 1 -maxdepth 1 -type d | sort | tail -n 1)
    sdk="$sysroot/Windows Kits/10"
    [ -d "$sdk/Include" ] || die "$sdk/Include is missing"
    sdkver=$(basename "$(find "$sdk/Include" -mindepth 1 -maxdepth 1 -type d | sort | tail -n 1)")
    [ -n "$crt" ] && [ -n "$sdkver" ] || die "cannot find the CRT or SDK version directories in $sysroot"
}

stage_tools() {
    local t
    for t in clang clang-cl lld llvm-lib llvm-rc; do
        [ -x "$llvm/$t" ] || die "$llvm/$t is missing (install clang-$LLVM_MAJOR, lld-$LLVM_MAJOR, llvm-$LLVM_MAJOR)"
    done
    for t in git curl ninja python3 flex bison; do
        command -v "$t" >/dev/null || die "$t is not installed"
    done
}

stage_venv() {
    if [ -x "$work/venv/bin/meson" ] && [ "$("$work/venv/bin/meson" --version)" = "$MESON_VERSION" ]; then
        return
    fi
    say "python environment with meson $MESON_VERSION"
    rm -rf "$work/venv"
    if command -v uv >/dev/null; then
        uv venv "$work/venv"
        # shellcheck disable=SC2086
        uv pip install --python "$work/venv/bin/python" $PY_PACKAGES
    else
        python3 -m venv "$work/venv"
        # shellcheck disable=SC2086
        "$work/venv/bin/pip" install $PY_PACKAGES
    fi
}

check_hashes() {
    local base=$1 list=$2 sum file
    while read -r sum file; do
        [ -n "$sum" ] || continue
        [ -e "$base/$file" ] || die "$base/$file is missing"
        [ "$(sha256sum "$base/$file" | cut -d' ' -f1)" = "$sum" ] || die "$base/$file does not match the pinned hash"
    done <<<"$list"
}

stage_sysroot() {
    if [ -n "${WBVN_SYSROOT:-}" ]; then
        say "using the sysroot $WBVN_SYSROOT"
    else
        [ -n "${WBVN_ACCEPT_MS_LICENSE:-}" ] || die "the sysroot stage downloads the MSVC CRT and the Windows SDK from Microsoft; set WBVN_ACCEPT_MS_LICENSE=1 to accept their license terms"
        command -v cargo >/dev/null || die "cargo is needed to install xwin"
        say "xwin $XWIN_VERSION"
        cargo install xwin --version "$XWIN_VERSION" --locked --root "$work/xwin"
        say "MSVC $CRT_VERSION and SDK $SDK_VERSION from the Visual Studio $VS_MANIFEST_VERSION channel"
        rm -rf "$sysroot"
        "$work/xwin/bin/xwin" --accept-license --cache-dir "$work/xwin-cache" \
            --manifest-version "$VS_MANIFEST_VERSION" --crt-version "$CRT_VERSION" \
            --sdk-version "$SDK_VERSION" --arch x86_64 --variant desktop \
            splat --use-winsysroot-style --preserve-ms-arch-notation --output "$sysroot"
    fi
    locate_sysroot
    check_hashes "$crt/lib/x64" "$CRT_LIB_SHA256"
    check_hashes "$sdk/Lib/$sdkver" "$SDK_LIB_SHA256"
    say "sysroot ok: CRT $(basename "$crt"), SDK $sdkver"
}

stage_overlay() {
    locate_sysroot
    say "case-insensitive include overlay"
    python3 "$here/make-overlay.py" "$work/overlay.json" "$crt/include" \
        "$sdk/Include/$sdkver/ucrt" "$sdk/Include/$sdkver/shared" "$sdk/Include/$sdkver/um"
}

# The compiler, linker and resource compiler as meson sees them, with the sysroot and the overlay
# built in, and the cross file that names them.
stage_toolchain() {
    locate_sysroot
    say "compiler wrappers and cross file"
    mkdir -p "$work/bin"
    cat >"$work/bin/clang-cl" <<EOF
#!/bin/bash
exec "$llvm/clang-cl" --target=x86_64-pc-windows-msvc -winsysroot "$sysroot" \\
  -Xclang -ivfsoverlay -Xclang "$work/overlay.json" -fuse-ld=lld "\$@"
EOF
    cat >"$work/bin/clang" <<EOF
#!/bin/bash
exec "$llvm/clang" "\$@"
EOF
    cat >"$work/bin/lld-link" <<EOF
#!/bin/bash
exec "$llvm/lld" -flavor link \\
  -libpath:"$crt/lib/x64" \\
  -libpath:"$sdk/Lib/$sdkver/ucrt/x64" \\
  -libpath:"$sdk/Lib/$sdkver/um/x64" "\$@"
EOF
    cat >"$work/bin/llvm-rc" <<EOF
#!/bin/bash
exec "$llvm/llvm-rc" \\
  --preprocessor="$work/bin/clang" \\
  --preprocessor-arg="--target=x86_64-pc-windows-msvc" \\
  --preprocessor-arg="-winsysroot" --preprocessor-arg="$sysroot" \\
  --preprocessor-arg="-ivfsoverlay" --preprocessor-arg="$work/overlay.json" \\
  /I "$crt/include" \\
  /I "$sdk/Include/$sdkver/ucrt" /I "$sdk/Include/$sdkver/shared" /I "$sdk/Include/$sdkver/um" "\$@"
EOF
    ln -sf "$llvm/llvm-lib" "$work/bin/llvm-lib"
    chmod +x "$work/bin/clang-cl" "$work/bin/clang" "$work/bin/lld-link" "$work/bin/llvm-rc"
    cat >"$work/cross.ini" <<EOF
[binaries]
c = '$work/bin/clang-cl'
cpp = '$work/bin/clang-cl'
ar = '$work/bin/llvm-lib'
strip = '/bin/true'
windres = '$work/bin/llvm-rc'
c_ld = '$work/bin/lld-link'
cpp_ld = '$work/bin/lld-link'
python = '$work/venv/bin/python3'
python3 = '$work/venv/bin/python3'

[host_machine]
system = 'windows'
cpu_family = 'x86_64'
cpu = 'x86_64'
endian = 'little'

[built-in options]
c_args = ['/FI$here/windows_compat.h', '-DVK_USE_PLATFORM_WIN32_KHR', '-D_ALLOW_COMPILER_AND_STL_VERSION_MISMATCH']
cpp_args = ['/FI$here/windows_compat.h', '-DVK_USE_PLATFORM_WIN32_KHR', '-D_ALLOW_COMPILER_AND_STL_VERSION_MISMATCH']
c_link_args = ['/ignore:4217']
cpp_link_args = ['/ignore:4217']
b_lundef = true
EOF
}

# Mesa at the pinned revision, our patches in order, and the two files of the WinBridge backend.
stage_mesa() {
    local src=$work/mesa p
    say "Mesa $MESA_REV"
    if [ ! -d "$src/.git" ]; then
        git init -q "$src"
        git -C "$src" remote add origin "$MESA_URL"
    fi
    if [ "$(git -C "$src" rev-parse -q --verify HEAD 2>/dev/null || true)" != "$MESA_REV" ]; then
        git -C "$src" fetch -q --depth 1 origin "$MESA_REV"
        git -C "$src" checkout -q -f FETCH_HEAD
    fi
    # untracked files are removed, ignored ones (the downloaded subprojects) are kept
    git -C "$src" reset -q --hard "$MESA_REV"
    git -C "$src" clean -q -fd
    for p in "$repo"/venus/patches/0*.patch; do
        say "patch $(basename "$p")"
        git -C "$src" apply --whitespace=nowarn "$p"
    done
    cp "$repo/venus/guest/vn_renderer_winbridge.c" "$repo/venus/guest/wbvn_proto.h" \
        "$repo/venus/guest/wbvn_ring.h" "$src/src/virtio/vulkan/"
}

stage_build() {
    local src=$work/mesa build=$work/build
    export PATH="$work/bin:$PATH"
    say "configure"
    local setup=(setup)
    [ -f "$build/build.ninja" ] && setup=(setup --reconfigure)
    # The static CRT and zlib make the DLLs depend on system libraries only: the guest needs no Visual C++
    # runtime, and opengl32.dll with libgallium_wgl.dll work from any program directory without other files.
    "$work/venv/bin/meson" "${setup[@]}" "$build" "$src" --cross-file "$work/cross.ini" \
        --buildtype debugoptimized -Db_vscrt=mt -Ddefault_library=static \
        -Dvulkan-drivers=virtio -Dgallium-drivers=zink -Dplatforms=windows \
        -Dopengl=true -Degl=disabled -Dglx=disabled -Dgbm=disabled -Dllvm=disabled \
        -Dvalgrind=disabled -Dlibunwind=disabled -Dbuild-tests=false -Dmicrosoft-clc=disabled \
        -Dtools= -Dxmlconfig=disabled
    say "build"
    ninja -C "$build" src/virtio/vulkan/vulkan_virtio.dll src/gallium/targets/libgl-gdi/opengl32.dll \
        src/gallium/targets/wgl/libgallium_wgl.dll
    mkdir -p "$work/out"
    cp "$build/src/virtio/vulkan/vulkan_virtio.dll" "$build/src/virtio/vulkan/vulkan_virtio.pdb" \
        "$build/src/gallium/targets/libgl-gdi/opengl32.dll" "$build/src/gallium/targets/libgl-gdi/opengl32.pdb" \
        "$build/src/gallium/targets/wgl/libgallium_wgl.dll" "$build/src/gallium/targets/wgl/libgallium_wgl.pdb" \
        "$work/out/"
    say "done: $work/out"
}

mkdir -p "$work"
stages=("$@")
[ ${#stages[@]} -gt 0 ] || stages=(tools venv sysroot overlay toolchain mesa build)
for s in "${stages[@]}"; do
    case $s in
        tools | venv | sysroot | overlay | toolchain | mesa | build) "stage_$s" ;;
        *) die "unknown stage $s" ;;
    esac
done
