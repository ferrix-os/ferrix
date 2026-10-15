#!/usr/bin/env python3
"""Copy FERRIX/{KERNEL.ELF,INITRD.IMG,CMDLINE.TXT} and EFI/BOOT/BOOTX64.EFI out of a FAT ESP image
(the one `cargo xtask n150-install` puts on the N150's disk) into
OUT/FERRIX/, and write OUT/SHA256SUMS, for the box's `esp` script
(G:/GameLab/iso/ferrix/n150/esp) to fetch over HTTP.

    esp-files.py ESP.IMG OUT [--cmdline-from OTHER_ESP.IMG]

`--cmdline-from` takes CMDLINE.TXT from another ESP image (the one the box
was written with), so a fresh build keeps the box's command line. A file
absent from the image is skipped and said so. Reads FAT12/16/32, 8.3 names.
"""
import hashlib
import os
import struct
import sys

NAMES = ["FERRIX/KERNEL.ELF", "FERRIX/INITRD.IMG", "FERRIX/CMDLINE.TXT", "EFI/BOOT/BOOTX64.EFI"]


class Fat:
    def __init__(self, path):
        self.f = open(path, "rb")
        b = self.f.read(512)
        self.bps, self.spc, self.rsv, self.nfats, self.nroot = struct.unpack_from("<HBHBH", b, 11)
        tot16, = struct.unpack_from("<H", b, 19)
        fatsz16, = struct.unpack_from("<H", b, 22)
        tot32, fatsz32 = struct.unpack_from("<II", b, 32)
        self.fatsz = fatsz16 or fatsz32
        total = tot16 or tot32
        self.rootsecs = (self.nroot * 32 + self.bps - 1) // self.bps
        self.data = self.rsv + self.nfats * self.fatsz + self.rootsecs
        clusters = (total - self.data) // self.spc
        self.kind = 12 if clusters < 4085 else 16 if clusters < 65525 else 32
        self.root_cluster = struct.unpack_from("<I", b, 44)[0] if self.kind == 32 else None
        self.f.seek(self.rsv * self.bps)
        self.fat = self.f.read(self.fatsz * self.bps)

    def next(self, c):
        if self.kind == 32:
            n = struct.unpack_from("<I", self.fat, c * 4)[0] & 0x0FFFFFFF
            return None if n >= 0x0FFFFFF8 else n
        if self.kind == 16:
            n = struct.unpack_from("<H", self.fat, c * 2)[0]
            return None if n >= 0xFFF8 else n
        v = struct.unpack_from("<H", self.fat, c + c // 2)[0]
        n = v >> 4 if c & 1 else v & 0xFFF
        return None if n >= 0xFF8 else n

    def chain(self, c):
        out = []
        while c is not None and c >= 2:
            self.f.seek((self.data + (c - 2) * self.spc) * self.bps)
            out.append(self.f.read(self.spc * self.bps))
            c = self.next(c)
        return b"".join(out)

    def root(self):
        if self.kind == 32:
            return self.chain(self.root_cluster)
        self.f.seek((self.rsv + self.nfats * self.fatsz) * self.bps)
        return self.f.read(self.rootsecs * self.bps)

    @staticmethod
    def entries(raw):
        for i in range(0, len(raw), 32):
            e = raw[i:i + 32]
            if e[0] == 0:
                return
            if e[0] == 0xE5 or e[11] & 0x08:
                continue
            base = e[0:8].decode("latin-1").rstrip()
            ext = e[8:11].decode("latin-1").rstrip()
            name = base + ("." + ext if ext else "")
            hi, = struct.unpack_from("<H", e, 20)
            lo, = struct.unpack_from("<H", e, 26)
            size, = struct.unpack_from("<I", e, 28)
            yield name.upper(), e[11], (hi << 16) | lo, size

    def lookup(self, path):
        raw = self.root()
        parts = path.upper().split("/")
        for i, part in enumerate(parts):
            for name, attr, cluster, size in self.entries(raw):
                if name == part:
                    if i == len(parts) - 1:
                        return self.chain(cluster)[:size]
                    raw = self.chain(cluster)
                    break
            else:
                return None
        return None


def main():
    argv = sys.argv[1:]
    other = None
    if "--cmdline-from" in argv:
        i = argv.index("--cmdline-from")
        other = argv[i + 1]
        del argv[i:i + 2]
    if len(argv) != 2:
        sys.exit(__doc__)
    image, out = argv
    fat = Fat(image)
    sums = []
    for name in NAMES:
        src = Fat(other) if (other and name == "FERRIX/CMDLINE.TXT") else fat
        data = src.lookup(name)
        target = os.path.join(out, *name.split("/"))
        os.makedirs(os.path.dirname(target), exist_ok=True)
        if data is None:
            print(f"esp-files: no {name} in the image: skipped")
            if os.path.exists(target):
                os.remove(target)
            continue
        with open(target, "wb") as f:
            f.write(data)
        digest = hashlib.sha256(data).hexdigest()
        sums.append(f"{digest}  {name}\n")
        print(f"esp-files: {name} {len(data)} bytes {digest}")
        if name == "FERRIX/CMDLINE.TXT":
            print(f"esp-files: CMDLINE.TXT = {data.decode('utf-8', 'replace').strip()!r}")
    with open(os.path.join(out, "SHA256SUMS"), "w", newline="\n") as f:
        f.writelines(sums)
    print(f"esp-files: {out}/SHA256SUMS written ({len(sums)} files)")


if __name__ == "__main__":
    main()
