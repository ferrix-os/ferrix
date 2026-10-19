"""The entries of a `newc` cpio archive, as Ferrix's initramfs is written.

The kernel reads it with `src/lib/fs/cpio`; this is the release tools'
reading of the same format (`070701` headers, eight hex digits a field,
names and data padded to four bytes), enough to list what an image
carries and read a file out of it.
"""

from __future__ import annotations

import stat
from dataclasses import dataclass


@dataclass(frozen=True)
class Entry:
    name: str
    mode: int
    data: bytes

    @property
    def regular(self) -> bool:
        return stat.S_ISREG(self.mode)

    @property
    def link(self) -> bool:
        return stat.S_ISLNK(self.mode)

    @property
    def directory(self) -> bool:
        return stat.S_ISDIR(self.mode)


def entries(archive: bytes) -> list[Entry]:
    found: list[Entry] = []
    offset = 0
    while True:
        header = archive[offset : offset + 110]
        if len(header) < 110 or header[:6] not in (b"070701", b"070702"):
            raise ValueError(f"no cpio header at {offset}")
        fields = [int(header[6 + 8 * i : 14 + 8 * i], 16) for i in range(13)]
        mode, size, name_size = fields[1], fields[6], fields[11]
        name_start = offset + 110
        name = archive[name_start : name_start + name_size - 1].decode()
        data_start = (name_start + name_size + 3) & ~3
        data = archive[data_start : data_start + size]
        offset = (data_start + size + 3) & ~3
        if name == "TRAILER!!!":
            return found
        found.append(Entry(name.removeprefix("./"), mode, data))
