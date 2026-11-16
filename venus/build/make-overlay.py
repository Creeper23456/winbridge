#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Writes a clang VFS overlay that makes include directories case-insensitive.

MSVC finds an include by name without regard to case; a Linux file system does not. Mesa has
`#include <BaseTsd.h>` where the SDK file is `basetsd.h`, and the SDK has `"DriverSpecs.h"` for
`driverspecs.h`. The overlay lists every file under each given directory and sets
`"case-sensitive": false`, so clang looks names up the way cl does. Nothing is copied or written
under the sysroot. Symbolic links are skipped: the overlay already answers for any case.

usage: make-overlay.py OUT.json DIR...
"""
import json
import sys
from pathlib import Path


def node(path: Path):
    if path.is_dir():
        kids = [node(c) for c in sorted(path.iterdir()) if not c.is_symlink()]
        return {"type": "directory", "name": path.name, "contents": kids}
    return {"type": "file", "name": path.name, "external-contents": str(path)}


def main() -> int:
    if len(sys.argv) < 3:
        print(__doc__, file=sys.stderr)
        return 2
    out = Path(sys.argv[1])
    roots = [Path(d).resolve() for d in sys.argv[2:]]
    overlay = {
        "version": 0,
        "case-sensitive": False,
        "use-external-names": False,
        "roots": [
            {"type": "directory", "name": str(r),
             "contents": [node(c) for c in sorted(r.iterdir()) if not c.is_symlink()]}
            for r in roots
        ],
    }
    out.write_text(json.dumps(overlay, indent=1), encoding="utf-8")
    files = out.read_text(encoding="utf-8").count('"type": "file"')
    print(f"overlay: {files} files in {len(roots)} directories -> {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
