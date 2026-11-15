#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Example script showing required QEMU arguments for WinBridge

set -euo pipefail

IMAGE="${1:-/var/lib/libvirt/images/windows11.qcow2}"
WINBRIDGE_SOCK="/tmp/winbridge.sock"
DBUS_SOCK="/tmp/qemu-dbus.sock"

echo "=== Starting QEMU with WinBridge support ==="
echo "Guest Image: $IMAGE"
echo "Serial Channel Socket: $WINBRIDGE_SOCK"
echo "D-Bus Display Socket:  $DBUS_SOCK"

# Clean up stale sockets
rm -f "$WINBRIDGE_SOCK" "$DBUS_SOCK"

exec qemu-system-x86_64 \
    -enable-kvm \
    -cpu host \
    -smp 4 \
    -m 8G \
    -drive file="$IMAGE",format=qcow2,if=virtio \
    -vga none \
    -device virtio-gpu-pci,edid=on,xres=1920,yres=1080 \
    -display dbus,addr="unix:path=$DBUS_SOCK" \
    -chardev socket,path="$WINBRIDGE_SOCK",server=on,wait=off,id=winbridge0 \
    -device virtio-serial-pci \
    -device virtserialport,chardev=winbridge0,name=org.winbridge.0 \
    -nic user,model=virtio-net-pci
