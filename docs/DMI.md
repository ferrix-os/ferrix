# DMI: the firmware's SMBIOS tables, as Linux shows them

Slice D1 of [N150.md](N150.md) §8. Linux shows the firmware's SMBIOS
tables to programs in two places: the identity strings in
`/sys/class/dmi/id/`, and the raw tables in `/sys/firmware/dmi/tables/`.
Wine builds Windows' view of the firmware from them, so on a Ferrix without
them Windows code sees empty firmware strings where Linux on the same box
shows the maker's. This document is the design, reviewed by the
certification consultant before any code (N150.md §8 and the consultant's
constraints there, ledger line 1285), and then what was built.

Every fact about SMBIOS and Linux below was checked against sources on
nazuna, never written from memory: Linux 6.18.54
(`~/crp123i-port/clean/linux-6.18.54`: `drivers/firmware/dmi_scan.c`,
`drivers/firmware/dmi-id.c`, `include/linux/efi.h`, `include/linux/sysfs.h`,
`lib/uuid.c`, `lib/vsprintf.c`), and QEMU at 0b836b518c
(`~/Documents/qemu/qemu/hw/arm/virt.c`, `hw/i386/pc.c`).

## 1. What Linux shows, and to whom

`/sys/class/dmi/id/` (dmi-id.c:41-63, 186-223): one file per identity
field the table has, plus `modalias`. A field's file exists when the
structure that holds it is long enough to hold it, whatever the string
says; a string index of 0, or a string of spaces only, reads as an empty
line (dmi_scan.c:47-65, 186-200).

| File | Mode | Source (SMBIOS type, offset) |
|---|---|---|
| `bios_vendor`, `bios_version`, `bios_date` | 0444 | 0: 4, 5, 8 |
| `bios_release`, `ec_firmware_release` | 0444 | 0: 20-21, 22-23 as `major.minor`; absent when both bytes are 0xFF |
| `sys_vendor`, `product_name`, `product_version` | 0444 | 1: 4, 5, 6 |
| `product_serial` | **0400** | 1: 7 |
| `product_uuid` | **0400** | 1: 8-23; absent when all 0x00 or all 0xFF; first three fields little-endian from SMBIOS 2.6 (`%pUl`), big-endian before (`%pUb`), lower case |
| `product_sku`, `product_family` | 0444 | 1: 25, 26 |
| `board_vendor`, `board_name`, `board_version` | 0444 | 2: 4, 5, 6 |
| `board_serial` | **0400** | 2: 7 |
| `board_asset_tag` | 0444 | 2: 8 |
| `chassis_vendor` | 0444 | 3: 4 |
| `chassis_type` | 0444 | 3: 5, the byte `& 0x7F` in decimal |
| `chassis_version` | 0444 | 3: 6 |
| `chassis_serial` | **0400** | 3: 7 |
| `chassis_asset_tag` | 0444 | 3: 8 |
| `modalias` | 0444 | `dmi:bvn…:bvr…:bd…:br…:efr…:svn…:pn…:pvr…:rvn…:rn…:rvr…:cvn…:ct…:cvr…:sku…:`, each value with bytes ≤ space, ≥ 127 and `:` dropped (dmi-id.c:65-142) |

The first structure of a type fills its fields; a later one of the same
type does not overwrite them (dmi_scan.c:192). A file reads the value and a
newline, cut to one page (dmi-id.c:22-30).

`/sys/firmware/dmi/tables/` (dmi_scan.c:764-816): `smbios_entry_point`,
the entry point's bytes as firmware wrote them (its own length, at most
32), and `DMI`, the structure table, cut after the last structure the walk
reached. Both are `BIN_ATTR_SIMPLE_ADMIN_RO`, mode **0400**
(sysfs.h:379-380): on Linux only root reads the raw tables, so a program
that is not root, Wine among them, reads `/sys/class/dmi/id`.

**Privacy.** The serial numbers and the UUID identify the one machine, so
Linux lets only root read them, and so does Ferrix, with the same modes.
Every other field names a model, not a machine.
`docs/certification/VULNERABILITY-ANALYSIS.md` carries the entry (§5).

## 2. Where the table is

UEFI firmware publishes the entry points in its configuration table
(efi.h:365-366):

* `SMBIOS3_TABLE_GUID` f2fd1544-9794-4a2c-992e-e5bbcf20e394: the 64-bit
  entry point, anchor `_SM3_`, length at byte 6 (accepted 24 to 32), a
  checksum over that length, version at bytes 7-9, the table's maximum
  length (32 bits) at 12, its address (64 bits) at 16. No structure count:
  the walk ends at type 127 (dmi_scan.c:649-671, 131-139).
* `SMBIOS_TABLE_GUID` eb9d2d31-2d88-11d3-9a16-0090273fc14d: the 32-bit
  entry point, anchor `_SM_`, length at byte 5 (accepted 30 to 32), a
  checksum over that length, version at bytes 6-7; and at byte 16 the
  `_DMI_` header with its own checksum over 15 bytes, the table length (16
  bits) at 22, address (32 bits) at 24 and structure count at 28
  (dmi_scan.c:581-643).

Linux prefers the 3.x entry point and falls back to the 2.x one when the
3.x is absent or does not decode (dmi_scan.c:678-720). So does Ferrix.
One difference, deliberate: Linux still accepts a 2.x entry point whose
`_SM_` checksum fails when the `_DMI_` header after it checks, as "legacy
DMI" (dmi_scan.c:609-635). Ferrix refuses it, because the constraint is
that a bad checksum means no files.

## 3. The design

### 3.1 The loader copies; the kernel never maps firmware memory

The UEFI loader (`src/boot/common/uefi`, in the reference configuration)
looks both GUIDs up next to the ACPI ones. While boot services still run,
firmware's memory is identity mapped, so the loader can read the entry
point and the table where they are. It:

1. reads at most 32 bytes at the entry point and validates them with
   `ferrix_smbios::EntryPoint::parse` (anchor, length range, checksums);
2. refuses a table longer than `SMBIOS_TABLE_MAX` (1 MiB) or of length 0;
3. copies the entry point (padded to 32 bytes) and then the table into one
   allocation of the loader's own `FERRIX_BOOT_INFO` memory type, which the
   kernel's map reports as `MemKind::BootInfo` and which nothing reclaims
   or writes;
4. passes the copy's physical address and length in two new `BootInfo`
   fields, `smbios` and `smbios_len` (version 8), or zeros.

It prints one line: `smbios 3.0 entry point, table of N bytes copied`, or
why there is none.

The alternative, handing the kernel firmware's own address, was considered
and refused: OVMF keeps the table in memory the UEFI map does not call RAM,
so the direct map does not cover it, and the kernel would need a second
exception like `rsdp_region` that maps a firmware-chosen range whatever the
memory map says. The copy needs no new mapping rule: it is RAM the loader
allocated, mapped by the direct map's ordinary rule, exactly as the device
tree copy is (`discovery/fdt.rs`). The Pixel 7 loader passes zeros.

### 3.2 The kernel validates the copy once

`discovery/smbios.rs`, once at boot, before any program runs:

* fails closed when `smbios` is 0;
* refuses a copy not wholly inside one `MemKind::BootInfo` region of the
  map, or outside the direct map (the address is one it was handed, not one
  it computed), or longer than 32 + `SMBIOS_TABLE_MAX`;
* re-parses the entry point from the copy's first 32 bytes, with the same
  checks as the loader, and refuses a table length that is not the copy's
  length less 32;
* walks the table as Linux's `dmi_decode_table` does (dmi_scan.c:88-145):
  stop at the count (2.x) or type 127 (3.x), at a structure shorter than its
  4-byte header, or at the table's end; a structure whose string set does
  not end before the table does is not decoded; the table is cut after the
  last structure reached;
* keeps the result in a write-once static: borrowed slices of the copy (the
  entry point, the cut table) and the decoded identity fields.

Any refusal leaves the static empty and prints why; then
`/sys/class/dmi` and `/sys/firmware/dmi` do not exist. Never an empty
string or an invented value in place of a table that did not validate.

The parser, `ferrix-smbios` (`src/lib/platform/smbios`), is `no_std`,
allocation-free in its parse, has no `unsafe` and no dependency, and takes
byte slices only, so it is host-tested against hostile tables: bad
checksums, lengths past the end, structures shorter than their header,
unterminated strings, string indices past the set, counts larger than the
table, a 2.x `_DMI_` header with a bad checksum, an entry point length out
of range.

### 3.3 sysfs shows it, read-only

`fs/sysfs.rs` (load ring) adds, only while the static holds a table:

* `/sys/class/dmi/id`, a link to `/sys/devices/virtual/dmi/id` as on
  Linux, which holds the field files of §1 that the table has, with
  Linux's modes, `modalias`, `uevent` (`MODALIAS=…`) and a `subsystem` link;
* `/sys/firmware/dmi/tables/smbios_entry_point` and `DMI`, 0400, the bytes
  unaltered: the entry point as firmware wrote it (its own length, its
  table address still firmware's), and the cut table.

Values pass through unaltered: the strings' bytes as the table holds them,
not re-encoded. What Linux formats (release, UUID, chassis type, the
modalias filter) is formatted as Linux formats it. Every file refuses a
write with `EACCES`, as kernfs does for a file without a `store`.

### 3.4 AArch64 and ARMv7-A

The loader code is shared, so the same path runs on both: Ferrix shows
whatever SMBIOS table the firmware it boots on installs. QEMU's `virt`
machine builds a 3.x table and offers it to firmware over `fw_cfg`
(virt.c:1694-1717); whether the firmware xtask boots on `virt` installs it
in the configuration table is what the AArch64 and ARMv7-A boots show, and
is recorded in §6 rather than assumed. A machine whose firmware installs
none (the DK1's U-Boot, the Pixel 7's bootloader, which is not UEFI) has no
`/sys/class/dmi` and no `/sys/firmware/dmi`, as Linux on it has none.

## 4. Checks

* `ferrix-smbios` host tests: QEMU's own 2.x and 3.x tables as built
  (fixtures), each hostile case of §3.2 refused or walked within bounds,
  the identity fields and their formats against Linux's.
* A boot check (`fs/sysfs/check.rs`): when the kernel holds a table, the
  files exist with the modes of §1, read what the parse decoded, refuse a
  write; when it holds none, `/sys/class/dmi` does not exist. Its boot
  line names `sys_vendor` and `product_name`.
* xtask boots x86_64 with `-smbios type=1,manufacturer=…,product=…`, and
  test-boot fails unless the boot line carries exactly those strings.
* A negative control: the kernel's checksum test inverted, run through
  `gate.sh control`, fires on the missing line.

## 5. What it is not

Nothing here hides, alters or invents a value: no hypervisor masking, no
made-up vendor. A VM shows QEMU's strings, the box shows its maker's. The
`dmi-sysfs` entries (`/sys/firmware/dmi/entries/`) and the in-kernel uses of
DMI (quirk tables, `dmesg`'s `DMI:` line) are not built.

## 6. Where it stands

2026-10-15: design written, sent to the certification consultant.
