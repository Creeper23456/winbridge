#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Example launcher: QEMU with the D-Bus display and the WinBridge virtio-serial port.
# QEMU's `-display dbus,addr=...` connects to a D-Bus *bus*, so a private bus daemon is started
# first (it exits when this script does). The viewer connects to the same address.
#
# Usage: launch-qemu-winbridge.sh /path/to/windows11.qcow2 [extra qemu args...]
#        winbridge-host --socket /tmp/winbridge.sock --dbus-addr unix:path=/tmp/qemu-dbus.sock

set -euo pipefail

IMAGE="${1:?usage: $0 IMAGE.qcow2 [qemu args...]}"
shift
WINBRIDGE_SOCK="${WINBRIDGE_SOCK:-/tmp/winbridge.sock}"
DBUS_SOCK="${DBUS_SOCK:-/tmp/qemu-dbus.sock}"
BUS_CONF="$(mktemp)"

rm -f "$WINBRIDGE_SOCK" "$DBUS_SOCK"

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
trap 'kill "$BUS_PID" 2>/dev/null || true; rm -f "$BUS_CONF" "$DBUS_SOCK"' EXIT
for _ in $(seq 1 50); do [ -S "$DBUS_SOCK" ] && break; sleep 0.1; done

echo "Guest image:         $IMAGE"
echo "WinBridge channel:   $WINBRIDGE_SOCK"
echo "D-Bus display bus:   $DBUS_SOCK"

qemu-system-x86_64 \
    -enable-kvm \
    -cpu host \
    -smp 4 \
    -m 8G \
    -drive file="$IMAGE",format=qcow2,if=virtio \
    -vga none \
    -device virtio-gpu-pci,edid=on \
    -display dbus,addr="unix:path=$DBUS_SOCK" \
    -chardev socket,id=wb0,path="$WINBRIDGE_SOCK",server=on,wait=off \
    -device virtio-serial-pci \
    -device virtserialport,chardev=wb0,name=org.winbridge.0 \
    -nic user,model=virtio-net-pci \
    "$@"
