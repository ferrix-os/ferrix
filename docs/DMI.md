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
`docs/certification/VULNERABILITY-ANALYSIS.md` carries the entry, under
"T.IDENTIFY — a program that is not root fingerprints the machine from its
firmware".

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
Linux corrects some firmware's 2.x version (dmi_scan.c:596-608): 2.31 and
2.33 are read as 2.3, 2.51 as 2.6. So does Ferrix, and the corrected
version decides one thing only, the UUID's byte order (§1).

Two differences, deliberate:

* Linux still accepts a 2.x entry point whose `_SM_` checksum fails when
  the `_DMI_` header after it checks, as "legacy DMI" with a 15-byte entry
  point (dmi_scan.c:609-635). Ferrix refuses it, because the constraint is
  that a bad checksum means no files; Linux's 15-byte legacy entry point is
  never shown.
* Linux has no cap on the table's length. Ferrix refuses a table longer
  than 1 MiB, and then falls back to the 2.x entry point if there is one,
  as it does on any refusal of the 3.x one.

## 3. The design

### 3.1 The loader copies; the kernel never maps firmware memory

The UEFI loader (`src/boot/common/uefi`, in the reference configuration)
looks both GUIDs up next to the ACPI ones. While boot services still run,
firmware's memory is identity mapped, so the loader can read the entry
point and the table where they are. It:

1. reads at most 32 bytes at the entry point and validates them with
   `ferrix_smbios::EntryPoint::parse` (anchor, length range, both 2.x
   checksums);
2. refuses a table longer than `ferrix_smbios::TABLE_MAX` (1 MiB) or of
   length 0, a table address whose end overflows or that the loader cannot
   address (the 32-bit ARMv7-A loader, above 4 GiB), and an entry point or
   table that overlaps what the UEFI memory map calls device memory
   (`EfiMemoryMappedIO`, `EfiMemoryMappedIOPortSpace`);
3. copies the entry point (padded to 32 bytes) and then the table into one
   allocation of a memory type of its own, `FERRIX_SMBIOS`, which the
   kernel's map reports as `MemKind::Smbios` and which nothing reclaims or
   writes, allocated below the direct map's end like everything else it
   hands over;
4. passes the copy's physical address and length in two new `BootInfo`
   fields, `smbios` and `smbios_len` (version 8), or zeros.

A refusal never stops the boot: the loader prints one line naming it,
tries the 2.x entry point after a refused 3.x one, and passes zeros. When it
copies, it prints `smbios 3.0 entry point (64-bit), table of N bytes
copied`: the version and the length, nothing from the table.

What the loader does here is an assumption of the kernel's requirement
(`L.discovery.5`), not a requirement of its own: no requirement names a
loader unit, so `L.discovery.6`, reserved for it, was released unwritten.
The kernel does not rely on it: it checks the copy again.

`BootInfo` version 8 is refused by a version 7 kernel and the other way
round, as every version change is (`BootInfoError::VersionMismatch`), so a
loader and a kernel go together; an A/B update carries both.

The alternative, handing the kernel firmware's own address, was considered
and refused: OVMF keeps the table in memory the UEFI map does not call RAM,
so the direct map does not cover it, and the kernel would need a second
exception like `rsdp_region` that maps a firmware-chosen range whatever the
memory map says. The copy needs no new mapping rule: it is RAM the loader
allocated, mapped by the direct map's ordinary rule, exactly as the device
tree copy is (`discovery/fdt.rs`). The Pixel 7 loader passes zeros.

### 3.2 The kernel validates the copy once

`discovery/smbios.rs` (item ring), once at boot, before any program runs:

* `range`, a pure function of the offered address and length, the memory
  map and the direct map's bounds, with checked arithmetic: nothing offered
  is "none handed over"; a copy no longer than 32 bytes or longer than 32 +
  1 MiB, one whose end overflows, one not wholly inside one
  `MemKind::Smbios` region, or one not wholly inside the direct map is
  refused by name;
* then the one `from_raw_parts` of the copy, a `'static` shared borrow
  (`SAFETY: (BOOT-DATA)`, as `discovery/fdt.rs`); no copy, no allocation;
* `decode` re-parses the entry point from the copy's first 32 bytes, with
  the same checks as the loader, and refuses a table length that is not the
  copy's length less 32;
* walks the table as Linux's `dmi_decode_table` does (dmi_scan.c:88-145):
  stop at the count (2.x) or type 127 (3.x), at a structure shorter than its
  4-byte header, or at the table's end; a structure whose string set does
  not end before the table does is not decoded; the table is cut after the
  last structure reached;
* keeps the result in a write-once static: borrowed slices of the copy (the
  entry point, the cut table) and the decoded identity fields.

It prints one line: `smbios 3.0, N structures in M bytes: <sys_vendor>
<product_name>`, those two strings with every byte outside printable ASCII
(and the backslash) escaped as `\xNN`, or `smbios refused: <why>`, or
`smbios none handed over`. Any refusal leaves the static empty and the boot
goes on; then
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
  Linux's modes and owner root, `modalias`, `uevent` (`MODALIAS=dmi:…:`,
  what Linux's `uevent_show` prints for this device) and a `subsystem` link
  to `/sys/class/dmi`;
* `/sys/firmware/dmi/tables/smbios_entry_point` and `DMI`, 0400, the bytes
  unaltered: the entry point as firmware wrote it (its own length, its
  table address still firmware's), and the cut table. `stat` gives their
  real lengths, as Linux's binary attributes do; the text files say a page.

`/sys/firmware` itself appears only with a table (Ferrix has nothing else
to put there). Linux's `power/` directory under `dmi/id` is not shown.

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

* `ferrix-smbios` host tests (`src/lib/platform/smbios/src/tests.rs`):
  tables laid out as QEMU's `hw/smbios/smbios.c` lays out types 0, 1, 3
  and 127; each hostile case of §3.2 refused or walked within bounds (bad
  checksums, a bad `_DMI_` header, entry point lengths just outside both
  ranges, an entry point cut at every byte, a table at the cap and one byte
  over, a length past the end, a structure shorter than its header,
  unterminated strings, a count larger than the table, every truncation and
  every single-byte change of a good table); Linux's version fixups, the
  2.x count and count-0 walks, the 3.x stop at type 127, first-wins, a
  0xFF/0xFF release, all-0x00 and all-0xFF UUIDs, strings of spaces, string
  index 0 and past the set, bytes passed through, the modalias and the page
  cut.
* `discovery::smbios_check` (boot, FX-0909): `range` and `decode` fed a
  forged machine -- none, outside the region, straddling its end, below
  the direct map, overflowing, holding no table, a table one byte longer
  than the entry point says, a bad checksum -- each refused by its name,
  and a good copy accepted.
* The sysfs boot check (`fs::sysfs::check::check_dmi`, FX-0890): with a
  table, every field file the table has and `modalias` and both table
  files have Linux's mode and uid 0, read what the parse decoded, refuse a
  write with `EACCES`; a user (1000) and the global user a namespaced root
  runs as (100000) get `EACCES` on the 0400 files and read the 0444 ones;
  fields the table lacks are absent. With none, `/sys/class/dmi`,
  `/sys/firmware` and `/sys/devices/virtual/dmi` do not exist.
* Every QEMU boot xtask makes gives the machine `-smbios` with a vendor, a
  product, and marker serials and UUID (`tools/common/xtask/src/smbios.rs`).
  A test boot fails unless the kernel accepted a table on every
  architecture, unless on x86-64 its line names exactly that vendor and
  product, and if any marker appears anywhere in the serial log.
* Negative controls through `gate.sh control`, each to read FIRED (§6).

## 5. What it is not

Nothing here hides, alters or invents a value: no hypervisor masking, no
made-up vendor. A VM shows QEMU's strings, the box shows its maker's. The
`dmi-sysfs` entries (`/sys/firmware/dmi/entries/`) and the in-kernel uses of
DMI (quirk tables, `dmesg`'s `DMI:` line) are not built.

## 6. Where it stands

2026-10-15: design reviewed by the certification consultant, OK IF (1)-(23)
(ledger line 1339; its predecessor's constraints at line 1285). Requirement
ids L.discovery.2-6 and H.BOOT.16 reserved on branch `dmi-d1-reserve`, to
land alone first (condition 13). The code is on branch `dmi-d1`; what it
has run and what is left is in the handover,
`~/.local/share/ferrix/handover/dmi-d1.md`, until it lands.
