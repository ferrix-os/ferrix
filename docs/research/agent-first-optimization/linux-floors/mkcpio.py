#!/usr/bin/env python3
"""Write a newc cpio: /dev, /dev/console (5,1), /proc, /sys, and /init."""
import sys, os
out, init = sys.argv[1], sys.argv[2]
ino = [1]
def entry(f, name, mode, data=b"", rmaj=0, rmin=0):
    hdr = "070701" + "".join("%08X" % v for v in [ino[0], mode, 0, 0, 1, 0, len(data), 0, 0, rmaj, rmin, len(name) + 1, 0])
    ino[0] += 1
    b = hdr.encode() + name.encode() + b"\0"
    b += b"\0" * ((4 - len(b) % 4) % 4)
    b += data + b"\0" * ((4 - len(data) % 4) % 4)
    f.write(b)
with open(out, "wb") as f:
    for d in ["dev", "proc", "sys"]:
        entry(f, d, 0o040755)
    entry(f, "dev/console", 0o020600, rmaj=5, rmin=1)
    entry(f, "init", 0o100755, open(init, "rb").read())
    entry(f, "TRAILER!!!", 0)
