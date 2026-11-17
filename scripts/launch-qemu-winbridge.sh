#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Example launcher: QEMU with the D-Bus display and the WinBridge virtio-serial port.
# QEMU's `-display dbus,addr=...` connects to a D-Bus *bus*, so a private bus daemon is started
# first (it exits when this script does). The viewer connects to the same address.
#
# Usage: launch-qemu-winbridge.sh IMAGE.qcow2 [extra qemu args...]
# Environment, set by winbridge-host from config.toml when it runs this script:
#   WINBRIDGE_SOCK, WINBRIDGE_QMP_SOCK, WINBRIDGE_DBUS_ADDR (unix:path=PATH)
# Optional: WINBRIDGE_REL_MOUSE=1 adds a relative USB mouse next to the absolute tablet. The viewer's
# game mode (on while a program uses 3D) picks a relative device with `mouse_set`; the machine's own
# PS/2 mouse is one, so this is not required.
# Optional: WINBRIDGE_SHM=1 adds the ivshmem shared-memory device for the optional shared-memory
# transport (see below); WINBRIDGE_SHM_FILE changes its backing file (default /dev/shm/winbridge-ctl).
# Optional: WINBRIDGE_VENUS=N adds N Venus slots for 3D acceleration and starts one winbridge-venus
# per slot (see below and venus/README.md).
# Runs in the foreground so the EXIT trap can stop the private bus daemon.

set -euo pipefail

IMAGE="${1:?usage: $0 IMAGE.qcow2 [qemu args...]}"
shift
WINBRIDGE_SOCK="${WINBRIDGE_SOCK:-/tmp/winbridge.sock}"
WINBRIDGE_QMP_SOCK="${WINBRIDGE_QMP_SOCK:-/tmp/winbridge-qmp.sock}"
WINBRIDGE_DBUS_ADDR="${WINBRIDGE_DBUS_ADDR:-unix:path=/tmp/qemu-dbus.sock}"
case "$WINBRIDGE_DBUS_ADDR" in
    unix:path=*) DBUS_SOCK="${WINBRIDGE_DBUS_ADDR#unix:path=}" ;;
    *) echo "WINBRIDGE_DBUS_ADDR must be unix:path=PATH, got: $WINBRIDGE_DBUS_ADDR" >&2; exit 2 ;;
esac
BUS_CONF="$(mktemp)"

rm -f "$WINBRIDGE_SOCK" "$DBUS_SOCK" "$WINBRIDGE_QMP_SOCK"

cat > "$BUS_CONF" <<EOF
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path=$DBUS_SOCK</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
EOF

dbus-daemon --nofork --nopidfile --config-file="$BUS_CONF" &
BUS_PID=$!
trap 'kill "$BUS_PID" 2>/dev/null || true; rm -f "$BUS_CONF" "$DBUS_SOCK" "$WINBRIDGE_QMP_SOCK"' EXIT
for _ in $(seq 1 50); do [ -S "$DBUS_SOCK" ] && break; sleep 0.1; done

echo "Guest image:         $IMAGE"
echo "WinBridge channel:   $WINBRIDGE_SOCK"
echo "D-Bus display bus:   $DBUS_SOCK"
echo "QMP device control:  $WINBRIDGE_QMP_SOCK"

# The viewer sends absolute positions, which needs an absolute pointing device: usb-tablet.
REL_MOUSE=()
if [ "${WINBRIDGE_REL_MOUSE:-0}" = 1 ]; then
    REL_MOUSE=(-device usb-mouse)
fi

# Optional shared-memory transport (docs/IVSHMEM-TRANSPORT.md): WINBRIDGE_SHM=1 adds an
# ivshmem-plain device backed by a private 256 MiB file. The file is recreated empty on every
# start (mode 0600, this user only); the viewer maps it and refuses a file of another size, owner
# or mode. The guest needs the ivshmem driver and the agent (agent --install-ivshmem), and the
# viewer must run with --shm (or shm = true in config.toml); without them nothing changes and the
# virtio-serial channel carries everything. WINBRIDGE_SHM_FILE overrides the backing file.
SHM_DEVICE=()
if [ "${WINBRIDGE_SHM:-0}" = 1 ]; then
    WINBRIDGE_SHM_FILE="${WINBRIDGE_SHM_FILE:-/dev/shm/winbridge-ctl}"
    rm -f "$WINBRIDGE_SHM_FILE"
    (umask 077 && : > "$WINBRIDGE_SHM_FILE")
    truncate -s 256M "$WINBRIDGE_SHM_FILE"
    echo "Shared memory:       $WINBRIDGE_SHM_FILE (256 MiB, ivshmem-plain)"
    SHM_DEVICE=(
        -object memory-backend-file,id=wbctl,mem-path="$WINBRIDGE_SHM_FILE",size=256M,share=on
        -device ivshmem-plain,memdev=wbctl
    )
fi

# Optional Venus slots (venus/README.md): WINBRIDGE_VENUS=N adds N client slots for 3D
# acceleration. A slot is a 64 MiB ivshmem-plain device backed by /dev/shm/winbridge-venus<I>
# (recreated empty at every start, mode 0600) plus the virtio-serial port org.winbridge.venus.<I>
# that carries wake-up bytes, and gets one winbridge-venus process, stopped when this script ends.
# A guest Vulkan process takes one slot for as long as any of its Vulkan instances lives. Needs WINBRIDGE_VIRGL_LIB (libvirglrenderer.so.1
# with the patches of venus/patches/virglrenderer) and WINBRIDGE_RENDER_SERVER (virgl_render_server);
# WINBRIDGE_VENUS_BIN names the winbridge-venus binary (default: from PATH).
VENUS_DEVICES=()
VENUS_PIDS=()
VENUS_SLOTS="${WINBRIDGE_VENUS:-0}"
if [ "$VENUS_SLOTS" -gt 0 ]; then
    : "${WINBRIDGE_VIRGL_LIB:?WINBRIDGE_VENUS needs WINBRIDGE_VIRGL_LIB}"
    : "${WINBRIDGE_RENDER_SERVER:?WINBRIDGE_VENUS needs WINBRIDGE_RENDER_SERVER}"
    VENUS_BIN="${WINBRIDGE_VENUS_BIN:-winbridge-venus}"
    for ((i = 0; i < VENUS_SLOTS; i++)); do
        slot_file="/dev/shm/winbridge-venus$i"
        slot_sock="/tmp/winbridge-venus$i.sock"
        rm -f "$slot_file" "$slot_sock"
        (umask 077 && : > "$slot_file")
        truncate -s 64M "$slot_file"
        VENUS_DEVICES+=(
            -object memory-backend-file,id=wbv$i,mem-path="$slot_file",size=64M,share=on
            -device ivshmem-plain,memdev=wbv$i
            -chardev socket,id=wv$i,path="$slot_sock",server=on,wait=off
            -device virtserialport,chardev=wv$i,name=org.winbridge.venus.$i
        )
        "$VENUS_BIN" --shm "$slot_file" --socket "$slot_sock" --slot "$i" \
            --virgl-lib "$WINBRIDGE_VIRGL_LIB" --render-server "$WINBRIDGE_RENDER_SERVER" &
        VENUS_PIDS+=($!)
        echo "Venus slot $i:        $slot_file (64 MiB, ivshmem-plain), $slot_sock"
    done
    trap 'kill "$BUS_PID" "${VENUS_PIDS[@]}" 2>/dev/null || true; rm -f "$BUS_CONF" "$DBUS_SOCK" "$WINBRIDGE_QMP_SOCK"' EXIT
fi

# The default pc machine provides the IDE bus used by this removable CD-ROM.
qemu-system-x86_64 \
    -enable-kvm \
    -cpu host \
    -smp 4 \
    -m 8G \
    -drive file="$IMAGE",format=qcow2,if=virtio \
    -vga none \
    -device virtio-gpu-pci,edid=on \
    -display dbus,addr="$WINBRIDGE_DBUS_ADDR" \
    -qmp unix:"$WINBRIDGE_QMP_SOCK",server=on,wait=off \
    -device qemu-xhci,id=xhci \
    -device usb-tablet \
    "${REL_MOUSE[@]}" \
    "${SHM_DEVICE[@]}" \
    -drive if=none,id=cd0,media=cdrom,readonly=on \
    -device ide-cd,drive=cd0,id=cdrom0 \
    -chardev socket,id=wb0,path="$WINBRIDGE_SOCK",server=on,wait=off \
    -device virtio-serial-pci \
    -device virtserialport,chardev=wb0,name=org.winbridge.0 \
    "${VENUS_DEVICES[@]}" \
    -nic user,model=virtio-net-pci \
    "$@"
