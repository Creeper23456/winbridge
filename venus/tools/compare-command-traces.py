#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Compares two VKR_CMD_TRACE files: the commands virglrenderer decoded for the Windows driver
and for Mesa's own Linux driver running the same Vulkan calls.

Each line is `SOURCE TYPE LENGTH FNV1A64`, with SOURCE `R` for the ring, `C` for the context's
own submits and `N` for nested command streams. Ring commands must match line for line. Context
commands carry the ring id (an address) and shmem resource ids, which differ between processes
and backends by design, so they are compared by type and length; the Windows host also issues
vkNotifyRingMESA (type 190) itself when it finds a ring idle, and those are left out.

Usage: compare-command-traces.py LINUX_TRACE WINDOWS_TRACE
"""

import sys

NOTIFY_RING = "190"


def load(path):
    with open(path) as f:
        return [tuple(line.split()) for line in f if line.strip()]


def main(argv):
    if len(argv) != 3:
        print(__doc__)
        return 2
    linux, windows = load(argv[1]), load(argv[2])
    failed = False

    for source in ("R", "N"):
        a = [c[1:] for c in linux if c[0] == source]
        b = [c[1:] for c in windows if c[0] == source]
        same = sum(x == y for x, y in zip(a, b))
        print(f"{source}: {len(a)} linux, {len(b)} windows, {same} identical (type, length, hash)")
        if len(a) != len(b) or same != len(a):
            failed = True
            for i, (x, y) in enumerate(zip(a, b)):
                if x != y:
                    print(f"  first difference at {i}: linux {x} windows {y}")
                    break

    a = [c[1:3] for c in linux if c[0] == "C"]
    b = [c[1:3] for c in windows if c[0] == "C" and c[1] != NOTIFY_RING]
    print(f"C: {len(a)} linux, {len(b)} windows (without host notifies), types and lengths equal: {a == b}")
    failed = failed or a != b
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
