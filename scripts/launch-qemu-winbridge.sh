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
# Optional: WINBRIDGE_REL_MOUSE=1 adds a relative USB mouse next to the absolute tablet, which the
# viewer's "Game mode" (relative mouse) needs. Without it, game mode is refused by QEMU.
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
    -drive if=none,id=cd0,media=cdrom,readonly=on \
    -device ide-cd,drive=cd0,id=cdrom0 \
    -chardev socket,id=wb0,path="$WINBRIDGE_SOCK",server=on,wait=off \
    -device virtio-serial-pci \
    -device virtserialport,chardev=wb0,name=org.winbridge.0 \
    -nic user,model=virtio-net-pci \
    "$@"
