#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Installs the udev rule that lets the logged-in seat user open USB devices for WinBridge
# passthrough: copies 70-winbridge-usb.rules to /etc/udev/rules.d/, reloads udev and re-triggers
# USB add events so already-connected devices pick up the ACL.
#
# Security: the rule gives the active seat user read/write access to USB device nodes, except
# keyboards and mice (HID protocol 01/02, composite devices included). Install it only on a
# machine where that user is trusted. The rule does not cover /dev/input/* or /dev/hidraw*.
#
# Usage: install-host-udev.sh [--dry-run] [--yes] [--uninstall]
#   --dry-run    print the commands without running them (no root needed)
#   --yes        skip the confirmation prompt
#   --uninstall  remove the rule and reload udev
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RULE_SRC="$SCRIPT_DIR/70-winbridge-usb.rules"
RULE_DST="/etc/udev/rules.d/70-winbridge-usb.rules"

dry_run=0
assume_yes=0
uninstall=0
for arg in "$@"; do
  case "$arg" in
    --dry-run) dry_run=1 ;;
    --yes) assume_yes=1 ;;
    --uninstall) uninstall=1 ;;
    -h|--help) sed -n '2,13p' "$0"; exit 0 ;;
    *) echo "unknown option: $arg" >&2; exit 2 ;;
  esac
done

# Runs a command as root (via sudo when needed), or only prints it in dry-run mode.
run() {
  local prefix=()
  if [[ $EUID -ne 0 ]]; then
    prefix=(sudo)
  fi
  if (( dry_run )); then
    echo "would run: ${prefix[*]} $*"
  else
    "${prefix[@]}" "$@"
  fi
}

if (( ! dry_run )) && [[ $EUID -ne 0 ]] && ! command -v sudo >/dev/null 2>&1; then
  echo "error: root privileges are required and sudo is not installed" >&2
  exit 1
fi

if (( uninstall )); then
  echo "Removes $RULE_DST and reloads udev. Devices keep their ACL until they are replugged."
else
  echo "Installs $RULE_DST: the active seat user gets access to USB devices, excluding keyboards and mice."
fi

if (( ! dry_run && ! assume_yes )); then
  if [[ ! -t 0 ]]; then
    echo "error: confirmation needs a terminal; rerun with --yes to proceed non-interactively" >&2
    exit 1
  fi
  read -r -p "Proceed? [y/N] " answer
  [[ "$answer" == [yY] ]] || { echo "aborted"; exit 1; }
fi

if (( uninstall )); then
  run rm -f "$RULE_DST"
else
  [[ -f "$RULE_SRC" ]] || { echo "error: missing $RULE_SRC" >&2; exit 1; }
  run install -m 0644 "$RULE_SRC" "$RULE_DST"
fi
run udevadm control --reload
run udevadm trigger --subsystem-match=usb --action=add

if (( dry_run || uninstall )); then
  exit 0
fi

# Verification: one device node should now carry an ACL entry for the current user.
node="$(find /dev/bus/usb -mindepth 2 -maxdepth 2 -type c 2>/dev/null | head -n 1 || true)"
if [[ -z "$node" ]]; then
  echo "no USB device node found; replug a device and check again"
elif getfacl -p "$node" 2>/dev/null | grep -q "^user:$(id -un):"; then
  echo "ok: $node grants access to $(id -un)"
else
  echo "warning: $node has no ACL for $(id -un) yet; replug a device and check with: getfacl $node"
fi
