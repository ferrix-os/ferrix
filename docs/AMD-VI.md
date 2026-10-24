# AMD-Vi: the AMD IOMMU, isolated as VT-d isolates

Session: amd-vi (subagent of the PO's session), 2026-10-24. Branch:
`feature/amd-vi` from `develop` (40f063bad). Status: **BUILT** on `feature/amd-vi` (see *As built*, before the
verdicts). The certification consultant's design review is needed before
any code (`docs/CONVENTIONS.md`, *Changes to the certified item*); its
verdict goes in the last section.

Sources: AMD I/O Virtualization Technology (IOMMU) Specification, #48882
(rev 3.10); Linux 6.18 `drivers/iommu/amd/{init.c,iommu.c,io_pgtable.c,
amd_iommu_types.h}`; QEMU 10.2.1 `hw/i386/amd_iommu.{c,h}` and
`hw/i386/acpi-build.c` (`build_amd_iommu`), which the boot rows run; the
VT-d design this follows, `~/.local/share/ferrix/n150-linux/vtd-design.md`
(never landed in the tree; its verdicts are ledger 2174, 2388, 2392);
`src/kernel/src/iommu.rs`, `iommu/{vtd,remapping,check,domain_check}.rs`,
`src/lib/kernel/paging/src/vtd*`, `src/lib/platform/acpi/src/dmar.rs`,
`docs/INTEL-GPU.md` §4.4, and nazuna's own firmware (§9).

---

## 0. What this is, and what it is not

**Why.** Ferrix's drivers run in ring 3; isolation is the microkernel's
premise. Without an IOMMU a DMA-capable device reaches all of memory, and
the kernel says so (degraded trusted mode, `ARCHITECTURE.md` §7,
VULNERABILITY-ANALYSIS V-03). On an AMD machine today:

* `iommu::units` finds no unit (it reads only the DMAR and the IORT), so
  every PCI function is *bypassing* and every domain is untranslated;
* `remapping::interrupts_isolated` is false, so `device_isolation` never
  sets `DEVICE_ISOLATION_INTERRUPTS`, and devmgr's `gpu::hand_over`
  answers `Unisolated`: the NVIDIA driver cannot run on nazuna;
* the NICs, disks and USB hosts run, untranslated (devmgr gates only
  `Plan::Gpu`, `main.rs:561`).

**What.** A second x86-64 IOMMU driver behind the *same* kernel interface
VT-d serves (§6): a function behind an AMD-Vi unit gets a translated
`Domain` from `domain_for`, its messages are routed through its own
interrupt remapping table, and `function_isolated` answers true on a machine
whose units all remap and where no function bypasses them. devmgr, the pin
module, the quarantine, `device_isolation` and nvrm change in nothing.

**Kept as small and generic as VT-d's.** The kernel gains a mechanism and no
policy: it parses the IVRS, programs the unit, and answers the same five
questions it answers for VT-d (is this function placed, give it a domain,
pin, unpin, route its interrupts). Which device gets a driver, and whether
an untranslated device may run at all (§8), stays devmgr's.

**Not built**, each said where it matters below: guest translation (v2
tables, GT, PASID, PPR), ATS and device IOTLBs, x2APIC destinations
(`XTEn`, 128-bit IRTEs), guest virtual APIC (`GAEn`), the hardware
exclusion range, SNP, performance counters, IOMMU event interrupts, and
more than one PCI segment (x86-64 enumerates segment 0 only, as for VT-d).

---

## 1. What the specification asks, and what Ferrix uses of it

### 1.1 The IVRS (ACPI, `ferrix_acpi::ivrs`, new)

`IVRS` = 36-byte header, `IVinfo` (u32 at 36), 8 reserved, then blocks
from byte 48, each `type, flags, length:u16`:

| block | header | used |
|---|---|---|
| IVHD 10h | 24 bytes: `devid` (the unit's own PCI function), `cap_offset`, `base` (u64, MMIO), `segment`, `info`, `feature_reporting` (u32) | yes |
| IVHD 11h | 40 bytes: as 10h to `info`, then `attributes` (u32; bit 0 `HATDis`), `efr` (u64, the EFR image), `efr2` (u64) | yes |
| IVHD 40h | as 11h, and may hold ACPI-HID entries (0xF0) | yes |
| IVMD 20h/21h/22h | 32 bytes: `flags` (0 unity, 1 IR, 2 IW, 3 exclusion), `devid`, `aux` (21h: none; 22h: last devid), `start` u64, `length` u64 | yes (§4) |
| anything else | skipped by its length | -- |

**One IVHD type per unit** (Linux `get_highest_supported_ivhd_type`):
firmware describes each unit once per type it supports (QEMU emits 10h
*and* 11h for its one unit; nazuna's Linux says "Using global IVHD EFR", so
11h or 40h). The type used is the highest of 10h, 11h and 40h among the
blocks naming the first unit's `devid`; blocks of other types are ignored,
so no unit is counted twice and no device entry is read twice.

**Device entries**, after the IVHD header to its `length`. An entry's length
is `4 << (type >> 6)` for types below 0x80 (Linux `ivhd_entry_length`), and
for 0xF0 (ACPI HID) `22 + uid_length`; any other type at 0x80 or above has
no known length, so the block's remaining entries **cannot be read**.

| type | name | meaning for placement |
|---|---|---|
| 00h | pad | none |
| 01h | ALL | every devid on the segment is behind this unit (a catch-all, as `INCLUDE_PCI_ALL`) |
| 02h | select | `devid` behind this unit |
| 03h / 04h | start / end of range | `start..=end` behind this unit |
| 42h | alias select | `devid` behind this unit, **arriving as** `source` (bytes 5-6) |
| 43h / 04h | alias range | `start..=end` arriving as `source` |
| 46h / 47h | extended select / range | as 02h / 03h; the extended byte (ATS) is ignored |
| 48h | special | `handle` (byte 4), `devid` (5-6), `variety` (7: 1 I/O APIC, 2 HPET): the source ID that interrupt source arrives with |
| F0h | ACPI HID | a non-PCI device (AMDI0040 and the like); no PCI function, skipped by length |

The `flags` byte of an entry (`INITPass`, `EIntPass`, `NMIPass`, `Lint0/1`,
`SysMgt`) is **not** applied: every pass bit in every device table entry is
0 (§3.3). Linux applies them; Ferrix refuses the passed interrupt types
outright, which is the stricter side and costs nothing it uses.

What cannot be followed is said, not guessed, as for the DMAR
(`iommu.rs`, module docs): a block whose entries cannot be read to its end,
an end-of-range with no start, a range with `end < start`, and a block
whose length runs past the table make every function it might name
**unresolved**, never behind no unit. The parser keeps that as a
`Truncated` mark on the block, as `dmar.rs` keeps `path_is_whole`.

### 1.2 The unit's registers (`iommu/amdvi.rs`, new)

IVHD gives no length. **The kernel keeps 512 KiB (0x80000) from every
driver's apertures** -- Linux's `MMIO_REG_END_OFFSET`, covering the
performance-counter pages a 11h/40h unit has -- and maps the first 16 KiB,
which hold every register this driver touches:

| offset | register | use |
|---|---|---|
| 0x0000 | device table base | base 51:12, `Size` 8:0 = 511 (2 MiB, 65,536 entries) |
| 0x0008 | command buffer base | base, `ComLen` 59:56 = 8 (256 entries, 4 KiB) |
| 0x0010 | event log base | base, `EventLen` 59:56 = 8 (256 entries, 4 KiB) |
| 0x0018 | control | `IommuEn` 0, `EventLogEn` 2, `Coherent` 10, `CmdBufEn` 12, `InvTimeOut` 7:5 = 1 ms; everything else 0 |
| 0x0020 / 0x0028 | exclusion base / limit | written 0: `ExEn` 0 (§4.3) |
| 0x0030 | extended features (EFR) | read: `HATS`, `IASup`, `GASup`, `XTSup`, `HESup`, `PCSup` |
| 0x2000 / 0x2008 | command head / tail | |
| 0x2010 / 0x2018 | event head / tail | |
| 0x2020 | status | `EventOverflow` 0, `EventLogRun` 3, `CmdBufRun` 4 |

**Which units** (`ferrix_paging::amdvi::unit::accept`, pure, host-tested,
from the EFR, the IVHD type and its attributes, as `vtd::unit::accept` is
from VER/CAP/ECAP):

* the EFR is the IVHD image for 11h/40h (Linux prefers it: "Using global
  IVHD EFR"), the MMIO register for 10h;
* `HATDis` set (an 11h/40h attribute: no host translation; QEMU sets it
  for `dma-translation=off`): **remapping only**, translation off, said;
* `HATS` must allow a three-level walk -- every value does (00b is four
  levels, 01b five, 10b six; 11b is reserved and refused);
* a unit with `XTSup` is accepted and used with xAPIC destinations only
  (`XTEn` 0); `GASup` is accepted with `GAEn` 0, so IRTEs are the 32-bit
  format (§3);
* a unit that firmware left enabled (`IommuEn`, `CmdBufEn` or `EventLogEn`
  set at open) is turned off before any table is written: control cleared,
  `CmdBufRun` and `EventLogRun` waited for to read 0 (bounded, the gate's
  patience), as `vtd::stop_firmware` does. A unit that does not stop is
  refused. QEMU never leaves one on; the path is argued, and a host test
  covers the decision.

### 1.3 The device table

256 bits per devid; 65,536 entries; one 2 MiB block (order 9, `MAX_ORDER`
is 10), allocated once at bring-up, zeroed, never freed. **Every entry is
written blocked before `IommuEn`**:

* `V`=1, `TV`=1, `Mode`=0, `IR`=`IW`=0: DMA translation valid and every
  access refused (Linux writes `V|TV` for every devid for the same
  reason). `V`=0 would *pass DMA through untranslated*, and a devid the
  table does not cover does the same on some parts, which is why the whole
  64 K is allocated rather than Linux's `last_bdf`;
* `IV`=1, `IntCtl`=00b (target abort), `IntTabLen`=0, root 0: every fixed
  or arbitrated interrupt from the devid is aborted; `IV`=0 would pass it
  through unremapped. All pass bits 0, so NMI, INIT, ExtInt, LINT0/1 are
  aborted too;
* `I` (IOTLB) 0, so an ATS request is refused (AoU-12: ATS is never
  enabled, now on AMD too), `SE`, `SA`, `IoCtl` 0, `GV` 0, `GLX`/`GCR3` 0.

**Attach** writes a function's entry: `Mode`=3 (three levels, 39 bits; the
same `TRANSLATED_BITS` as VT-d), the root, `IR`=`IW`=1, `DomainID`, then
`INVALIDATE_DEVTAB_ENTRY` + completion wait. Words are written high to low
with the root and `Mode` in word 0 last, so the entry is never valid with a
stale domain ID (the order VT-d's `install` keeps). **Detach** writes it
back to blocked, invalidates the entry, then `INVALIDATE_IOMMU_PAGES` for
the domain, then gives back the root table, as VT-d's `detach` does.

Domain IDs: 1 to 65,535, as VT-d's identifiers (`tables.identifiers`).

### 1.4 Page tables: AMD v1 host tables, three levels

A new `Encoding`, `AmdV1` (`ferrix_paging::amdvi`), with `ROOT_LEVEL =
Level::GIGABYTE`, `VIRT_BITS = 39`, `PHYS_BITS = 52` (masked to the 39
TRANSLATED_BITS by the domain, as for VT-d), mapped through the same
`mm::map_io::<E>`, `unmap_io`, `translate_io` VT-d uses:

* PDE: `PR` 0, `NextLevel` 11:9 = the AMD level of the table it points to,
  address 51:12, `IR` 61, `IW` 62 both set (permissions are ANDed down the
  walk, QEMU `fetch_pte`);
* PTE (4 KiB, AMD level 1): `PR`, `NextLevel` 0, address, `IR`/`IW` from
  `MapFlags`, `FC` 60 0;
* no 2 MiB or 1 GiB leaves (`supports_block` false): a domain maps 4 KiB
  pins only, and no `NextLevel=7` page-size encoding is ever written.

`NextLevel` is the one field no existing encoding has: a table
descriptor's value depends on its level. `Encoding::table_descriptor` gains
a sibling with a default, `table_descriptor_at(table, level, flags)`
calling `table_descriptor`, and the mapper's one call site
(`ferrix_paging/src/lib.rs:695`) calls it. Every existing encoding is
byte-identical (the default), which a host test pins for each.

Tables are written through the direct map. **The unit is coherent**
(`Control.Coherent` 1, and AMD's units snoop), so `Unpublished::new(true)`
is used and nothing is cleaned; there is no AMD counterpart of `ECAP.C`.

### 1.5 The command buffer and completion

One 4 KiB ring, 256 slots of 16 bytes, written only under the unit's queue
lock (the unit `Gate` VT-d uses, `iommu/gate.rs`). The commands used:

| code | command | when |
|---|---|---|
| 01h | `COMPLETION_WAIT`, `S`=1, store address, store data | ends every batch |
| 02h | `INVALIDATE_DEVTAB_ENTRY` (devid) | after any DTE write |
| 03h | `INVALIDATE_IOMMU_PAGES` (domain, `S`=1, address `0x7FFF_FFFF_FFFF_F000`, `PDE`=1) | after unmap, after map (below), at detach |
| 05h | `INVALIDATE_INTERRUPT_TABLE` (devid) | after any IRTE write |
| 08h | `INVALIDATE_IOMMU_ALL` | once after `IommuEn`, if `IASup`; else 02h for every devid and 05h for every devid with a table |

**Completion** is the VT-d queue's discipline: the wait stores a sequence
number of the unit's own to a kernel word, and the kernel waits for *that*
number, up to the unit's patience (100 ms, `PATIENCE_NANOS`), so a late
completion of an earlier wait is never taken for this one. A failed wait is
the number not stored in time, or `CmdBufRun` read clear, or an
`ILLEGAL_COMMAND_ERROR` (05h) / `COMMAND_HARDWARE_ERROR` (06h) event seen
while waiting. It answers `Err`, and as on VT-d every caller treats that as
"the unit may still reach it": an unpin hands the pin back, a detach keeps
the root. The unit is then marked **failed**: every later command fails at
once, nothing is released from its domains until reboot, and the end-of-boot
audit counts it stray (FX-1007), as `vtd`'s `IQE`/`ITE` do (`Cause::Queue`).

`Wait::Unwritten` (check R7) is a `COMPLETION_WAIT` with `S`=0: nothing is
stored, the short patience runs out, the unpin fails and keeps its pages.
`Domain::queued()` answers true for an AMD-Vi domain too, so R7 runs on
both drivers.

**Flush after map: always.** VT-d flushes after a map only in caching
mode; an AMD unit may cache not-present entries (`NpCache`, a PCI
capability bit this kernel does not read), so every map is followed by
`INVALIDATE_IOMMU_PAGES` and a wait. A pin costs one more command; nothing
reads a stale "not present".

### 1.6 The event log, and faults

A 4 KiB log of 256 16-byte events, `EventLogEn` 1, its interrupt off
(`EventIntEn` 0), read by polling exactly where VT-d's fault records are
read: `Domain::take_fault`, the probe, and `audit_faults` last in boot.
`Unit::take_fault` reads the event at head, advances head, and decodes it
(`ferrix_paging::amdvi::event::decode`, pure, host-tested):

| event | becomes |
|---|---|
| 02h `IO_PAGE_FAULT`, `I`=0 | `Cause::Access`: `stream` = devid, `page` = address & !0xFFF, `write` = `RW` (flags bit 5) |
| 02h `IO_PAGE_FAULT`, `I`=1, address in 0xFEE0_0000-0xFEEF_FFFF | `Cause::Interrupt { reason: AMDVI_INTERRUPT, index: 0 }` |
| any other code (01h illegal DTE, 03h/04h hardware errors, 05h/06h command errors, 07h IOTLB timeout, 08h invalid device request, ...) | `Cause::AmdVi(code)`: counted stray, as an `SMMUv3` event of another type is |
| `EventOverflow` in status | `Cause::Lost`, then the bit is cleared (write 1) and the log restarted (`EventLogEn` toggled, as Linux does) |

**One QEMU deviation, carried in the decoder (decision D6).** QEMU 10.2's
`amdvi_page_fault` sets `I` on *every* DMA page fault and puts the access in
flags bits 1:0 (`PERM_WRITE` lands in bit 1, `NX`) instead of `RW`. A DMA
fault and an interrupt fault are told apart by the address, which the
specification also allows: an interrupt request's fault carries the
interrupt address. So the rule is: `I` set **and** the address in the
interrupt window is an interrupt fault; any other 02h is a DMA fault, and
`write` is `RW`, or flags bit 1 when `I` is set outside the window (QEMU's
encoding, which no hardware produces: on hardware `I` with a DMA address is
not a combination the specification defines). The host test holds both
encodings. Without this the probe's `Fault::write_to(stream, PROBE_PAGE)`
would read as "a fault other than the probe's" under QEMU.

QEMU also logs **no event for a refused interrupt** (`amdvi_mem_ir_write`,
"TODO: log the event"), so on QEMU the interrupt checks show non-delivery
only (§5, R1-R3). On hardware the refused request's event is read, must be
the provoked one, and any other is stray.

QEMU's event `devid` is `as->devfn` only (the bus is dropped,
`amdvi_page_fault`). The boot rows' devices are on bus 0, where that is the
devid; the probe stays on a bus-0 device, said in the check's comment.

`xtask`'s whole-run fault audit (`dma_faults.rs`, which reads QEMU's
`vtd_dmar_fault` trace) gains QEMU's `amdvi_page_fault` trace for the
AMD-Vi rows, held to the kernel's "on purpose" lines the same way.

---

## 2. Placement: which function is behind which unit

`iommu::discover` gets a third firmware source beside the DMAR and the
IORT. The decision is the one `place_dmar` makes, through the same
`ferrix_pci::topology::place` (so the report and the domains cannot
disagree, `L.iommu.45`):

* `Explicit::Own(unit)` for a function whose devid a select, range or
  extended entry of the unit's IVHD names, **and** that enumeration places
  as its own requester (`topology::claims(false, f, f, bridges) == Own`:
  every bridge on the way a PCIe port);
* `Explicit::Unfollowed` (→ unresolved) for a devid an alias entry names
  (its DMA arrives as another devid; a second function behind the same
  bridge would share it, so it is never given a domain -- stricter than
  Linux, which shares the domain), for one enumeration finds under an
  aliasing bridge, and for any devid on a segment with a truncated block;
* `CatchAll::One(unit)` for a segment whose unit has an ALL (01h) entry;
  two units with ALL on one segment are `Several`, said, and unresolved;
* the stream is the devid, `function.requester_id()`.

The IOMMU's own PCI function (the IVHD `devid`, 00:00.2 on nazuna, QEMU's
`amd-iommu` device) is placed like any other function and gets a blocked
entry; its own table walks and log writes are not translated by itself.

`Report` gains `amdvi: usize`; the summary line gains `, N AMD-Vi units`
**only when N > 0**, so every VT-d, SMMU and Arm boot line is byte-identical
(§6.3).

---

## 3. Interrupt remapping

### 3.1 Per-function tables

Unlike VT-d's one table per unit with a source-ID check in each entry, an
AMD unit finds a message's table through the *sending devid's* DTE. So
isolation between functions is structural: a function can only name
entries in its own table.

* A function's table is made the first time `route` hands it a vector
  (`msi_allocate`), never before, never freed: 8 KiB (order 1), zeroed,
  128-byte aligned; its DTE gets `IV`=1, `IntCtl`=10b (remapped),
  `IntTabLen`=9 (512 entries) and the root, then
  `INVALIDATE_DEVTAB_ENTRY`. 512 entries is the largest every unit takes
  without `NumIntRemapSup` (Linux `MAX_IRQS_PER_TABLE_512`); the
  allocation is 8 KiB, not 2 KiB, because QEMU indexes the table with
  `data[10:0]` and does not check `IntTabLen`, so a forged index up to 2047
  must land in zeroes (`RemapEn` 0) rather than in the next object.
* **The entry index is the vector's slot among the sixty-four** (`vector -
  0x40`), as on VT-d: vectors are never given back, so an entry is written
  once and never rewritten (check `check_entry_never_rewritten` holds for
  AMD too).
* **32-bit IRTE** (`GAEn` 0): `RemapEn` 1, `SupIOPF` 0, `IntType` 000b
  fixed, `RqEoi` 0, `DM` 0 physical, `GuestMode` 0, `Destination` 15:8 the
  APIC ID (`remap::destination`'s rule: none above 255, so a processor
  above it gets no vector, as VT-d's `EIME`=0 does), `Vector` 23:16.
  Written, then `INVALIDATE_INTERRUPT_TABLE` + wait, **before** the
  message is programmed.
* **The message**: address 0xFEE0_0000 (destination bits ignored by the
  unit), data = the index; delivery mode in data 10:8 is then 000b fixed.
  `Route::Remapped` becomes `Route::Remapped(Remapper)`, an enum of
  `VtD(&vtd::Unit)` and `AmdVi(&amdvi::Unit)`, and `remap_message` answers
  `(address, data)` instead of the address alone; `msi.rs` takes both.

128-bit IRTEs, `XTEn`, `GAEn` and the guest virtual APIC are not built; the
obligation "x2APIC work must move to the 128-bit format" is written beside
`remap::destination`, as VT-d's `EIME` one is.

### 3.2 The console's I/O APIC line

Converted as VT-d's is (`remapping.rs` steps 1-4), with three differences:

1. **Its source** is the special (48h, variety 1) entry whose `handle` is
   the line's MADT I/O APIC ID; its devid's table is made for it. No such
   entry on any unit: remapping is not enabled, said, exactly as "no DMAR
   scope names I/O APIC N" (QEMU: handle 0, devid 0x00a0).
2. **The redirection entry** keeps compatibility form, with the *index* in
   its vector field (7:0) and delivery mode fixed; the unit reads the
   I/O APIC's message data as the index. `LiveLine::convert` gets the
   entry's form as an argument (`Form::VtdRemappable | Form::AmdIndexed`),
   and `ferrix_paging::amdvi::remap::redirection_entry` builds the AMD one.
3. **Edge-triggered lines only.** A level-triggered line's EOI is matched
   by the I/O APIC against the entry's vector field, which now holds the
   index, so it would need directed EOI by index (Linux's
   `eoi_ioapic_pin`). Not built: a level line is not converted, remapping
   is not enabled, and the line says why. COM1 is edge.

### 3.3 When interrupts are isolated

An AMD unit's `UnitState` is `{ remapping: true, compatibility_blocked:
true }` once its device table is live with `IV`=1 in every one of the
65,536 entries and `IommuEn` is on: there is no compatibility format to
leave open, and no devid outside the table. `remap::interrupts_isolated`
then holds on the same terms as VT-d's G4: every unit remaps, no function
bypasses. `function_isolated(f)` is `interrupts_isolated() && f is placed
behind a unit that remaps`. Check R9 (no vector minted in compatibility
form before remapping) runs unchanged before `IommuEn`.

---

## 4. Unity maps and exclusion ranges (AMD's RMRRs)

### 4.1 The rule

An IVMD names memory firmware's devices keep using. It is AMD's RMRR, and
it gets K2's treatment (`vtd-design.md` §3.9(b), ledger 2388), with the
same `ferrix_paging::vtd::rmrr::identity` rules moved to a shared module
(`ferrix_paging::unity`, bodies byte-identical, `rmrr` re-exporting it):
whole pages, inside the 39-bit reach, over no RAM the memory map gives, and
no frame the allocator holds. Then:

* **IVMD 21h (one devid) or 22h (a devid range)** naming functions placed
  behind the unit: each such function gets an identity domain holding
  exactly the IVMD's pages at their own addresses, attached before
  `IommuEn` with the rest of the device table, never detached, claimed by
  `domain_for` as an RMRR's is (`IdentityDomain`, `L.iommu.73`-`78`). The
  permissions are the IVMD's `IR`/`IW`.
* **IVMD 20h (all peripherals), an IVMD that cannot be honoured, or one
  naming a devid this cannot place**: the unit is held to **remapping
  only**: translation off (`Mode`=0, `IR`=`IW`=1 in every entry, so DMA is
  untranslated and reaches all of memory), interrupts still remapped, and
  its line says which IVMD held it and why, as VT-d's `RmrrPlan::Hold`
  does (`L.iommu.67`). An all-peripherals unity map would need every
  domain to carry it; that is a later slice, not guessed at now.
* **Exclusion ranges** (IVMD flag 3) are treated as unity maps with
  `IR`=`IW`=1, as Linux has since 5.x, and the hardware exclusion
  registers are written 0 (`ExEn` 0): the hardware range lets *every*
  device with `EX` set bypass translation, which no domain could then
  bound.

`IdentityDomain::attached` and `claim` become generic over the two units
(an `Attached` enum); the VT-d bodies are unchanged inside their arm.

### 4.2 What QEMU can show

QEMU's IVRS has no IVMD. As K2's check does on VT-d
(`domain_check::check_rmrr_identity`), the unity check makes a synthetic
one at check time, on the translating unit -- and goes further than K2,
because the AMD row has a device that can do the DMA: QEMU's `edu`.

* `attach_identity` is refused on a translating unit unless boot checks
  run (K2's condition 2);
* the check takes a frame, keeps it out of the allocator for the boot, and
  gives `edu` an identity domain of that one page;
* `edu` DMAs into it (its DMA engine, `0x80`-`0x98`): the bytes land at
  the page's own address -- **the unity map holds**;
* `edu` DMAs to the page beside it: the unit faults it and the event is
  read and was provoked;
* pins over the page and unpins naming it are refused, the domain is never
  detached and never claimed twice (K2's steps, reused).

---

## 5. Checks, negative controls, gates

### 5.1 Boot checks on the AMD-Vi row (`test-boot --amd-vi`)

The machine is the x86-64 machine with `-device amd-iommu,intremap=on` in
place of `intel-iommu` (`xtask` `AMD_IOMMU`, chosen by `--amd-vi`), and
`edu` attached for the row. Every stage 10 check that runs on the VT-d row
runs here through the same `Domain` API, plus:

| check | what it shows | negative control (`gate.sh control --expect`) |
|---|---|---|
| A1 blocked by default | at bring-up every DTE reads blocked (`V`,`TV`,`IV` set, `Mode` 0, `IR`=`IW`=0, `IntCtl` 0), sampled every 4,096th devid and every devid PCI enumeration found; `edu` before its domain DMAs and nothing lands | write `V`=0 for unattached entries: A1's readback FIRED, and the edu write lands |
| A2 translation | virtio-rng's entropy read lands through its translated domain (the VT-d row's check, unchanged) | `Mode` 0 for attached entries: the probe's write lands, FIRED |
| A3 out-of-domain DMA faults and is logged | the probe (`virtio.rs`) writes page 0x1000 outside its domain; the event log holds exactly that `IO_PAGE_FAULT` (stream, page, a write), provoked; nothing stray at the audit | PTE `PR` written for the probe page: "the device wrote into a page its domain does not map", FIRED |
| A4 unpin waits | pin/unpin/quarantine/budget checks (`check_iommu`, `check_budget`) on an AMD domain; R7: `Wait::Unwritten` fails, the pin keeps its pages | drop the `INVALIDATE_IOMMU_PAGES` from `flush`: the R7 check sees the release, FIRED |
| A5 commands complete | N commands queued and each waited by sequence number; a planted bad command (opcode 0) gives `ILLEGAL_COMMAND_ERROR`, the unit is marked failed in a check-only unit copy (as `check_planted_completion_error`) | wait ignores the sequence: FIRED |
| A6 interrupts remapped | `edu`'s minted MSI is delivered through its own table's entry; the console line converted (index in RTE) and serviced once | `IntCtl`=01b pass for edu: R1 delivered, FIRED |
| A7 R1-R3 | a message with an unwritten index, an NMI-type message, and one naming another function's index each arrive nowhere (and on hardware, are the provoked event) | `IntTabLen`/table shared between two functions: R3 delivered, FIRED |
| A8 unity map holds | §4.2 | identity domain made without the page: edu's write to it does not land and faults, FIRED |
| A9 isolation mark | R10 (`check_isolation_mark`) on AMD: `device_isolation` answers both bits for edu; forced unisolated, the mark refused | `function_isolated` false on AMD: the bit missing, FIRED |

Each control is run with `gate.sh control … --expect` on the landing patch
and its log cited in the commit (FIRED counts).

### 5.2 Host tests

* `ferrix_acpi::ivrs`: QEMU 10.2's IVRS bytes exactly (10h and 11h for one
  unit at 0xfed80000, the select/range entries of the row's bus, the
  I/O APIC special entry); **nazuna's IVRS bytes exactly** (§9) as a
  fixture; a 40h block with an ACPI-HID entry; alias select and range;
  extended entries; every truncation (`end < start`, unknown variable
  type, length past the table, a lone end-of-range); one-type-per-unit;
  IVMD 20h/21h/22h with exclusion; the fuzz target `acpi_tables` gains IVRS.
* `ferrix_paging::amdvi`: `AmdV1` walks (map/unmap/resolve at 0 and at
  2^39-4K, PDE `NextLevel` per level, `IR`/`IW` on PDEs, leaf flags round
  trip, no blocks); `table_descriptor_at` default byte-identical for every
  existing encoding; DTE encode/decode (blocked, attached, remapped,
  remapping-only); every command's encoding against the specification's
  layout; IRTE 32-bit; the redirection entry; the event decoder with both
  the specification's and QEMU's encodings; `accept` (HATS 11b refused,
  `HATDis` remapping only, EFR source by IVHD type).
* `ferrix_pci::topology` placement for the IVHD cases (own, aliased,
  catch-all, several), as `L.iommu.45`'s tests do for the DMAR.

### 5.3 Gates

* `cargo xtask check` (host tests, traceability, item boundary, coverage
  carry: `carry-coverage.py` then `gen-coverage-justification.py --check`);
* `test-boot` on x86-64 (KVM; the unchanged VT-d machine: boot lines
  compared with develop's, identical), aarch64, and armv7a `--smp 2`;
* **new row** `test-boot --amd-vi` (KVM, and TCG once): A1-A9 and every
  existing stage 10 check;
* `test-net --amd-vi`, the disk tests (`--boot-virtio`, `--boot-nvme`,
  `--boot-sata`) under `--amd-vi`, and the GPU-isolation check: `test-nvrm`'s
  handed-over boot needs the patched QEMU's NVIDIA stand-in; if it cannot
  take `amd-iommu`, A9 carries the `device_isolation` evidence and the
  devmgr hand-over's decision is host-tested in `devmgr-proto`
  (`gpu_tests.rs`) with `INTERRUPTS_ISOLATED` set, unchanged.

---

## 6. The shared interface, which stays identical

Every one of these is a place where an AMD-Vi arm is added and nothing that
uses it changes. Where it is a type, its variants grow; no signature,
meaning or VT-d/SMMU body changes.

1. **`iommu::Domain`**: `pin`, `unpin`, `unpin_waiting`, `translated`,
   `resolve`, `stream`, `take_fault`, `provoke`, `provoke_interrupt`,
   `pinned_pages`, `queued`, `pin_budget`, `id`, `detach_translation` --
   unchanged signatures; `Translation` gains `AmdVi { unit, attached,
   changing: Gate, identity }`, and each `match` gains its arm.
   `TRANSLATED_BITS` stays 39 and the identity-mapped addressing stays
   (no I/O address allocator).
2. **`iommu::domain_for(Address) -> Option<Domain>`** and
   `DeviceNode::domain()`: unchanged; an AMD-placed function gets an AMD
   domain, an IVMD's function its identity domain, under the same
   claim-once rule.
3. **`DomainError`, `Pinned`, `Wait`**: unchanged.
4. **`Fault` and `Cause`**: `Cause` gains `AmdVi(u8)` (another event code);
   `Lost` and `Queue` are reused (their Display texts name SMMUv3 and VT-d,
   so AMD gets its own Display line through a `unit` word, and the VT-d and
   SMMU texts are byte-identical); `Interrupt { reason, index }` is reused
   with one new reason, `AMDVI_INTERRUPT` (0x80, outside VT-d's 0x20-0x26),
   named "an AMD-Vi unit refused the interrupt request".
5. **Fault bookkeeping**: `PROVOKED`, `PROVOKED_INTERRUPTS`, `STRAY`,
   `count_if_stray`, `audit_faults`, `report_unchecked_faults`, the audit
   record's `DMA_FAULT`/`INTERRUPT_FAULT`: unchanged; `audit_faults` drains
   AMD units too.
6. **`object::pin`** (budgets, the quarantine and its release at HELLO,
   F-38's dead-driver rule): untouched. It depends only on `unpin` not
   returning before the unit has forgotten the pages, which §1.5 keeps.
7. **`remapping`**: `route`, `function_isolated`, `interrupts_isolated`,
   `any_remapping`, `console_vectors`, `stray_deliveries`, R9, `STATES`:
   unchanged meaning; `Route::Remapped` carries a `Remapper`.
8. **`device_isolation`** (`syscall/native.rs`), its two bits, and the
   isolated-interrupts mark (`L.device.27`, `L.device.28`): untouched.
   **devmgr, `devmgr-proto::gpu::hand_over`, nvrm: no change** -- the
   hand-over reads `INTERRUPTS_ISOLATED`, which AMD-Vi now sets.
9. **`iommu::units`, the apertures**: an AMD unit's 512 KiB is kept from
   drivers as a VT-d unit's 4 KiB is.
10. **Boot lines**: every existing line is byte-identical on the four
    boots (VT-d x86-64, aarch64, armv7a, the AMD row's unchanged lines);
    new lines appear only on machines with an AMD unit. The summary line
    gains its AMD count only then.
11. **`ferrix.vtd=off`** keeps its meaning (VT-d only). O1 asks whether
    AMD gets its own switch.

---

## 7. Requirements (reserve **L.iommu.79-93**)

Develop's highest is L.iommu.78, and no reservation holds L.iommu. The
reservation lands first, docs-only, and is released in the writing commit.

| id | name | statement in short | check |
|---|---|---|---|
| 79 | ivrsReadAsWritten | IVHD 10h/11h/40h, their device entries and IVMDs are read as §1.1 says; what cannot be read to its end marks its block truncated | host (QEMU, nazuna, synthetic) |
| 80 | oneIvhdTypePerUnit | only the highest of 10h/11h/40h naming the first unit is used | host |
| 81 | anAmdUnitIsAccepted | §1.2's rule: HATS 11b refused, HATDis remapping only, EFR from the IVHD for 11h/40h | host; boot (QEMU accepted) |
| 82 | firmwareLeftOnIsStopped | a unit left enabled is stopped, or refused | host (decision); argued |
| 83 | everyDeviceEntryBlocked | all 65,536 DTEs blocked (DMA and interrupts) before `IommuEn` | boot A1 |
| 84 | aDomainIsAModeThreeTable | attach writes `Mode` 3, root, domain ID, IR/IW, invalidates the entry | host (DTE, AmdV1); boot A2 |
| 85 | commandsWaitBySequence | every batch ends in a stored completion wait, waited by its own number; a failed wait fails the unit | host (encoding); boot A5, R7 |
| 86 | unmapsAreForgottenBeforeRelease | unpin invalidates the domain's pages and waits before frames go; map is followed by the same | boot A4 |
| 87 | everyEventIsRead | the log is read head to tail; each event decoded as §1.6 | host (both encodings); boot A3 |
| 88 | aFunctionsInterruptsUseItsOwnTable | a function's vectors go through its own 512-entry table, index = slot, invalidated before the message | boot A6 |
| 89 | noOtherInterruptIsDelivered | unwritten index, other function's index, NMI/INIT/ExtInt: not delivered | boot A7 |
| 90 | theConsoleLineByItsSpecialEntry | converted only by the I/O APIC's special entry, edge only, index in the RTE | boot A6 |
| 91 | unityMapsAsIdentityDomains | IVMD 21h/22h and exclusion ranges honoured as K2's identity domains; otherwise remapping only | host; boot A8 |
| 92 | exclusionRegistersOff | `ExEn` 0, base and limit 0, read back | boot line |
| 93 | aliasedIsUnresolved | an alias entry's devid, or one under an aliasing bridge, is unresolved | host |

Changed statements: **L.iommu.8** (refusal reasons gain the AMD ones, or a
sibling), **L.iommu.46** (catch-all: ALL entries), **L.iommu.67** (held to
remapping only by an IVMD), **L.iommu.73-78** (identity domains: "an RMRR
or an IVMD"), **L.device.28** (no vector: "no unit that remaps"; unchanged
text if it already says that). SAFETY-MANUAL AoU-12 (ATS never enabled)
gains the DTE `I` bit. If O1 is taken, one more id (94).

---

## 8. The NIC gap: should every DMA-capable device be gated on isolation?

**Today**: only `Plan::Gpu` goes through `hand_over`; a NIC, disk, USB host
or sound card is started whatever `device_isolation` answers. On a machine
with no IOMMU (or with `ferrix.vtd=off`) they run with DMA to all of memory
and the kernel announces degraded trusted mode at the first pin.

**Why the GPU is gated and the others are not**: the GPU executes command
buffers a *client* hands it (nvrm passes user work to the device), so
without isolation an unprivileged program can DMA anywhere. A NIC driver
takes no client programs; its DMA is only its own driver's, which runs as
a trusted component in degraded mode. The remaining risk is the device
itself (a malicious or buggy NIC firmware, a Thunderbolt peer) and a
compromised driver.

**Options, for the customer** (this design builds none of them; the
consultant's view is asked in D9):

* **(a) As today.** Untranslated non-GPU devices run, announced. Nazuna and
  every AMD box keep working before AMD-Vi lands, and after it on a
  firmware with IOMMU off.
* **(b) Gate every DMA-capable device by default, with a named opt-out.**
  devmgr refuses to start a driver for a function whose `device_isolation`
  lacks `DMA_TRANSLATED` unless `ferrix.degraded=allow` (or a per-class
  list) is on the command line; refused in a release image's default line,
  like `ferrix.intelgpu`. Matches the premise; costs boot-ability on
  machines without a working IOMMU (the N150 today has VT-d; a firmware
  with IOMMU disabled in setup would lose its disk and network).
* **(c) Gate by class.** Bus masters that can be driven from outside the
  box (Thunderbolt/USB4 hosts, external PCIe) and client-programmable
  devices (GPU, future accelerators) gated; internal NIC/disk/USB run
  degraded, announced.

**Recommendation**: (b) for release images and (a) for development images,
which is (b) with the opt-out on by default in dev builds. Policy lives in
devmgr (ring 3); the kernel already exports everything it needs
(`device_isolation`). The customer decides.

---

## 9. Nazuna (AMD Ryzen, Granite Ridge, B650)

What the host shows without root (`journalctl -k`, `lspci -tvnn`,
`/sys/class/iommu/ivhd0`):

* one unit, `ivhd0`, the PCI function **00:00.2** ([1022:14d9]);
* "AMD-Vi: Using global IVHD EFR:0x246577efa2254afa, EFR2:0x0": an IVHD
  11h or 40h carries the EFR. Decoded: `PPRSup`, `NXSup`, `GTSup`,
  `IASup`, `GASup`, `PCSup`, `GAvAPIC`; **`XTSup` clear** (no x2APIC
  support), so the 32-bit IRTE format with xAPIC destinations is exactly
  what this unit offers; `HATS`, from bits 11:10, allows three levels;
* Linux: "Interrupt remapping enabled", every function in an IOMMU group:
  the IVHD covers them all;
* the functions this landing is for: **01:00.0/.1** the RTX 3060 behind
  root port 00:01.1, **03:00.0/.1** the RTX 3090 behind 00:01.3,
  **06:00.0** the SN720 behind the chipset switch (00:02.1 → 04:00.0 →
  05:00.0 → 06), **08:00.0** the RTL8111H behind 05:09.0. Every bridge on
  each path is a PCIe port (no PCIe-to-PCI bridge in `lspci -t`), so each
  is placed as its own requester, unless the IVHD names an alias.

**Still owed:** the IVRS bytes. `/sys/firmware/acpi/tables/IVRS` is
root-only, `dmesg` is restricted, and pkexec timed out without an answer.
The customer was asked for one `pkexec cp` (the PO relays it). With the
bytes: the fixture test (`ivrs::tests::nazuna`), the IVHD type, any
IVMD (a unity map would decide whether the unit translates or is held to
remapping only), the I/O APIC and HPET special entries, and the expected
boot lines below confirmed.

**Expected boot lines on nazuna** (bare metal, checks skipped), if the IVRS
is as the journal suggests and holds no IVMD:

```
  iommu    amd-vi unit 0x<base> (00:00.2, IVHD 40h): translating (three-level), commands and event log on, remapping on (512 entries per function, 32-bit format), exclusion range off; 65536 device table entries blocked by default
  iommu    0 VT-d units, 0 SMMUv3s, 1 AMD-Vi units; <n> PCI functions behind one, 0 of them untranslated, 0 bypassing, 0 unresolved
  iommu    <k> faults recorded in an unchecked boot, ...
```

No boot word is needed: AMD-Vi comes up wherever an IVRS describes a unit,
as VT-d does wherever a DMAR does. If O1 is taken, `ferrix.amdvi=off` is
the bare-metal preparation's way back (a GRUB entry with it), and the
image should carry that entry beside the default one.

---

### 9.1 nazuna's IVRS, read (2026-10-24, after the review)

The bytes arrived (200 bytes, SHA-256 9fb713bc...0e44; fixture
`ivrs::tests::NAZUNA`). One unit, 00:00.2, at **0xF7500000**, described
twice, as an IVHD 10h and an 11h with the same entries; the 11h is read. Its
EFR image is the journal's, and its attributes do not set `HATDis`. **No
IVMD**, so nothing holds the unit to remapping only. Entries:

* devids 0x0003-0xFFFE, each as itself: every function on every bus,
  among them 01:00.0/.1 (the 3060), 03:00.0/.1 (the 3090), 06:00.0 (the
  SN720) and 08:00.0 (the RTL8111H);
* an alias range 0xFF00-0xFFFF arriving as 00:14.5 (bus 0xFF holds no
  function), left unresolved by D4;
* special entries: the HPET (handle 0, devid 00:14.0), I/O APIC 0x20 at
  00:14.0 and I/O APIC 0x21 at 00:00.1. The I/O APIC entry's flags 0xD7
  (INIT, ExtInt, NMI, LINT0 and LINT1 pass, SysMgt 01b) are not applied;
  Ferrix routes no NMI, INIT or ExtInt through an I/O APIC.

**Not named: 00:00.0, 00:00.1 and 00:00.2.** 00:00.1 enumerates as no
function. 00:00.0 is the root complex: a host bridge (class 0600), no
capabilities, bus master off. 00:00.2 is the unit itself (class 0806). As
written, §2 counts both as *bypassing*, and then `interrupts_isolated`
(no function bypasses) is false on nazuna, so no GPU is handed over.

**Decision D10, asked of the consultant.** Neither function is a
requester whose DMA a domain could bound:

* **the unit's own function** (the IVHD `devid`) is the unit: its table
  walks, log writes and completion stores are its own, never translated by
  itself. It is placed as *the unit*, counted behind it, and never given a
  domain or a vector (`domain_for` answers `None`, as for a held identity
  domain), so no driver can pin into it;
* **a host bridge (class 06h, subclass 00h) on bus 0 that no entry names**
  is the root complex, which issues no DMA of its own. It is counted apart,
  as *not a requester*, on the report line ("..., 0 bypassing, 1 root
  complex, 0 unresolved" -- only when non-zero, so VT-d and Arm lines stay
  byte-identical), and is also never given a domain or a vector.

Every other function no entry names stays *bypassing*, and the machine's
interrupts are then not isolated. The expected nazuna line becomes:
`1 AMD-Vi units; N PCI functions behind one, 0 of them untranslated, 0
bypassing, 1 root complex, 0 unresolved`.

## 10. Files, and kernel lines

| file | change | product lines (est.) |
|---|---|---|
| `src/lib/platform/acpi/src/ivrs.rs` | new: IVRS parser (§1.1) | +380 |
| `src/lib/platform/acpi/src/lib.rs` | `ivrs()` accessor | +15 |
| `src/lib/kernel/paging/src/amdvi.rs` + `amdvi/{dte,command,remap,event,unit}.rs` | new: encodings and decisions (pure) | +470 |
| `src/lib/kernel/paging/src/lib.rs` | `table_descriptor_at` default + call site | +12 |
| `src/lib/kernel/paging/src/unity.rs` | the RMRR identity rules moved, `vtd::rmrr` re-exporting | ±0 (moved) |
| **kernel** `src/kernel/src/iommu/amdvi.rs` | new: the unit (open, tables, commands, events, attach/detach, map/unmap/flush, IRTEs, identity) | +780 |
| **kernel** `src/kernel/src/iommu.rs` | `Kind::AmdVi`, placement, `Programmed`, `bring_up_amdvi`, `Translation::AmdVi` arms, `IdentityDomain` generic, audit | +260 |
| **kernel** `src/kernel/src/iommu/remapping.rs` | `Remapper`, AMD bring-up and console source, `remap_message` (address, data) | +110 |
| **kernel** `src/kernel/src/arch/x86_64/msi.rs`, `mod.rs` | message data; the line's form | +15 |
| **kernel** `src/kernel/src/iommu/domain_check.rs`, `check.rs`, `device/check.rs` | A1, A5, A8; R1-R3 on AMD (non-delivery on QEMU) | +260 (check code) |
| **kernel total** | product +1,165, check +260; nothing removed | |
| `tools/common/xtask` | `--amd-vi`, `AMD_IOMMU`, the AMD trace in `dma_faults.rs`, expected lines | +150 |
| `docs/sysml/16-iommu-requirements.sysml`, reservations, traceability, SAFETY-MANUAL AoU-12, VULNERABILITY-ANALYSIS V-03, `docs/N150.md` analogue for nazuna | | |

New `unsafe`: the device table, IRT, command and log rings are written
through the direct map like VT-d's tables (`read_entry`/`write_entry`
pattern), one `SAFETY: (DMA)` per accessor, traced in
`unsafe-trace-baseline.json`; MMIO goes through `Mmio`. No new syscall, no
new object, no new capability.

---

## 11. Risks

1. **First run on hardware.** QEMU's model differs from silicon in exactly
   the places §1.6 lists; the bare-metal boot (checks skipped) reads every
   event at the end, and a wrong DTE refuses DMA (fail-safe) -- but the
   SN720 is the root disk, so a refused disk stops the boot at mount. O1 is
   the way back.
2. **Firmware's DMA at hand-over.** Blocking every devid at `IommuEn` stops
   anything firmware left running (USB legacy, a NIC's PXE ring); those
   faults are counted. In a checked boot they fail FX-1007; on nazuna's
   unchecked boot they are reported.
3. **The 2 MiB device table** is one order-9 allocation at stage 10; on a
   fragmented boot it may fail, and the unit is then left alone, said.
4. **QEMU shows no interrupt fault**, so A7's evidence on QEMU is
   non-delivery only; the event path for interrupts runs first on hardware.
5. **An IVMD on nazuna** would hold the unit to remapping only until the
   all-peripherals slice is built; the GPU hand-over still passes (it gates
   interrupts), but DMA stays untranslated, and the line says so.

---

## 12. Decisions asked of the consultant

* **D1** the device table blocked by default, all 64 K entries (§1.3).
* **D2** `Mode` 3 / 39 bits, 4 KiB only, `table_descriptor_at` (§1.4).
* **D3** completion by stored sequence; failed unit = `Cause::Queue`
  (§1.5); always flush after map.
* **D4** placement through `topology::place`; aliases unresolved (§2).
* **D5** per-function 512-entry tables in 8 KiB, 32-bit IRTEs, xAPIC only,
  edge-only console conversion, `Route::Remapped(Remapper)` (§3).
* **D6** the event decoder carrying QEMU's encoding, told apart by address
  (§1.6).
* **D7** IVMD as K2 identity domains, IVMD 20h and unhonourable ones hold
  the unit to remapping only, exclusion ranges as unity maps and the
  registers off (§4); the check made with `edu`'s DMA (§4.2).
* **D8** the shared-interface list (§6) as the review's boundary: is
  anything missing?
* **D9** the NIC gap (§8): which option the consultant supports for the
  customer's decision.
* **O1** `ferrix.amdvi=off`, a recovery configuration like
  `ferrix.vtd=off` (L.iommu.71), for nazuna's first boots.
* The ids L.iommu.79-93 (94 with O1).

---

## As built (2026-10-24)

Built as reviewed, with the conditions of ledger 3867 and 3923, and these
differences, each for the consultant's code review:

* **QEMU patch 0007** (`tools/common/data/qemu/`, landed on its own first).
  QEMU 10.2.1's amd-iommu event log cannot be read at all: the address is
  lost to a shift by 64 in `amdvi_setevent_bits` (undefined; the built
  binary writes 0), the requester's bus is dropped, `I` is set on every DMA
  fault, and a refused interrupt request logs nothing. §1.6's QEMU branch
  of the decoder (condition 10) stays, unused by the patched build; the
  AMD-Vi rows need the patched build, which every x86-64 boot already
  requires (`(ferrix-cfi`).
* **The checks on `edu`** (`iommu/amdvi_check.rs`) run in stage 10's MSI
  check, before `edu`'s domain is made, because that is the only point a
  device with a DMA engine has no domain: A1's blocked write, A8's unity
  map, and condition 3's stale write after an unpin. A8's identity domain
  is then `edu`'s own domain for every check after it, given to its node
  by a check-only path (`DeviceNode::give_check_domain`, refused unless
  boot checks run). `edu`'s DMA mask is widened to 32 bits on every x86-64
  machine (`xtask`'s `attach_rng`), since QEMU's default 28 bits reaches no
  frame the allocator hands out.
* **A5** is the boot's command and wait counts, with exactly R7's wait
  failing; the planted illegal command is not built (QEMU logs it but
  never stops its buffer, so the failure path cannot be shown on it).
* **The RMRR rules are used in place** (`ferrix_paging::vtd::rmrr`), not
  moved to a shared module: no body moves, so no coverage is carried for
  them.
* **A remapping-only unit's entries are untranslated** (`Mode` 0, `IR` and
  `IW` set), not blocked: its functions' domains are untranslated and must
  reach memory, as a VT-d unit with `TE` off lets them.
* **`ferrix.amdvi` has no audit configuration item** (VT-d's `Vtd` has one,
  `L.iommu.71`): its boot line says when it was taken. Adding one bumps the
  audit record's format and is left for the consultant to ask for.
* **Interrupt tables** are 512 entries (2 KiB) in a frame of their own
  (condition 13).
* **D10** is `ferrix_acpi::ivrs::exempt`, host-tested on nazuna's bytes,
  and applies to IVRS placement alone.

**The code review** (2026-10-24, ledger 4108): OK IF (1)-(7), met so:
(1) `vmo_pin` takes the domain before bus mastering, and `enable_dma`
refuses a D10-exempt function; stage 10 shows the exempt function refused
a pin and bus mastering (`L.iommu.99`). (2) `Config::AmdVi` = 9 joins the
audit configuration, `BOOT_RECORDS` 11, H.AUD.2 restated "eleven", and
`L.iommu.98` names the audit item. (3) `L.iommu.87`'s criterion says the
failed-unit half is argued from the code. (4) `L.iommu.95` stays
unverified, with its BACKLOG row naming a QEMU IVMD patch. (5)-(7): the
landing commit's boot-line diffs, rows, controls and record.

**Which check catches each control** (pool round av4 on 50e0ca78c, logs
`~/.local/share/ferrix/logs/queue/av4-ctl-*.log`; under the 2026-10-01
ruling a control may fire on an earlier check than the one it targets, and
this table says which):

| control | sabotage | fired on |
|---|---|---|
| a1 | unattached entries written with `V`=0 | `check_edu`'s blocked write: "edu's write with no domain was not recorded as its fault" -- the first check to DMA through an unattached entry, before A1's readback in `check_tables` runs |
| a2 | `Mode` 0 for attached entries | virtio-rng's out-of-domain probe (A3's check, the VT-d row's): "the device wrote into a page its domain does not map" -- an attached, untranslated entry passes the probe's write, before A2's own line |
| a3 | every event decoded as other than `IO_PAGE_FAULT` | `check_edu`'s blocked write, as a1: its fault is not recorded |
| a6 | `REMAPPED` IntCtl changed | `check_forged`'s own delivery of edu's minted message: "edu raised its MSI and nothing arrived" |
| a7 | one function's interrupt table looked up for every devid | the same delivery, before R3: edu's message names an entry in another table, so it arrives nowhere |
| a8 | identity domains built without their page | `check_edu`'s IVMD step: "an IVMD identity domain does not map exactly its page" |
| a9 | `function_isolated` false on AMD-Vi | xtask's R10 line: "check R10 never showed the isolated-interrupts mark on AMD-Vi" |
| unpin | unpin's flush without `INVALIDATE_IOMMU_PAGES` | `check_edu`'s stale-DMA step: "edu's write after the unpin was not refused" |
| r7 | the completion wait taken as written | `object::pin::check`: "frames released after a failed invalidation" |
| exen | `EXCLUSION_LIMIT` written 0x1000 | `check_tables`: "its exclusion registers do not read 0" |
| exempt | `enable_dma` not refusing a D10-exempt function | `check_exempt_pin`: "an exempt function was made a bus master" |
| h1-h4 | the IVRS reader, the event decoder, `accept`, `table_descriptor_at` | their host tests, under `cargo xtask check` |

**What a QEMU boot prints** (`test-boot --amd-vi`, KVM):

```
  iommu    amd-vi unit 0xfed80000: the console's I/O APIC line converted: vector 0x40 retired, 0x41 through entry 1 of devid 0x00a0's table
  iommu    amd-vi unit 0xfed80000 (devid 0x0010): translating (three-level), commands and event log on, remapping on (512 entries per function, 32-bit format), exclusion range off; 65536 device table entries blocked by default
  iommu    edu on AMD-Vi: its write with no domain refused and recorded; an IVMD identity domain of page 0x1e443000 took its write there, refused the page beside and a pin over it; a page pinned took its write and, unpinned, refused the same write
  remap    edu's forged messages refused on AMD-Vi: one naming entry 0xfc of its own table, never written, and an NMI each faulted 0x80 and arrived nowhere, one naming pci 0000:00:03.0's entry faulted 0x80 and arrived nowhere; its own remapped message delivered
  iommu    0 VT-d units, 0 SMMUv3s, 1 AMD-Vi units; 16 PCI functions behind one, 0 of them untranslated, 0 bypassing, 0 unresolved
  iommu    amd-vi: 15 sampled device table entries no function holds read blocked, every function with no domain blocked, exclusion registers 0; 1615 commands issued and 54 completion waits stored by their sequence, 1 failed as check R7 made it
  iommu    0 DMA faults recorded that no check provoked, 0 of them unit events other than a refused access, across 1 units that translate or remap (1 translating); 0 late faults from the out-of-domain probe
```

**What nazuna should print** (bare metal, `ferrix.checks=skip`; no boot
word is needed):

```
  iommu    amd-vi unit 0xf7500000 (devid 0x0002): translating (three-level), commands and event log on, remapping on (512 entries per function, 32-bit format), exclusion range off; 65536 device table entries blocked by default
  iommu    0 VT-d units, 0 SMMUv3s, 1 AMD-Vi units; N PCI functions behind one, 0 of them untranslated, 0 bypassing, 1 root complex, 0 unresolved
```

and, if a console line is routed through I/O APIC 0x20, its conversion
through devid 0x00a0's table. `ferrix.amdvi=off` is the recovery word for
a GRUB entry beside the default one.

**Kernel lines** (as `git diff --stat` counts them, comments included;
code lines without comments in brackets): `iommu/amdvi.rs` +917 (627),
`iommu/ivrs.rs` +373 (284), `iommu.rs` +324, `iommu/remapping.rs` +209,
`arch/x86_64/{mod,msi}.rs` +50, `device.rs` +27, `object/pin/check.rs` +14,
`service_check.rs` +1; check code `iommu/amdvi_check.rs` +265,
`iommu/domain_check.rs` +98, `device/check.rs` +79. Library code linked
into ring 0 (condition 11): `ferrix_acpi::ivrs` 649 (419), `ferrix_paging`
`amdvi*` 708 (359) plus `table_descriptor_at` +13. Against §10's +1,165
product estimate, which counted code: about 1,300 code lines, within the
25 % condition 11 allows; the comments roughly double the raw count.

## Consultant verdict

2026-10-24, design review before code (consultant subagent for amd-vi;
ledger `~/.local/share/ferrix/cert-consultant/reviews.md` line 3867):
**OK IF (1)-(13)**. Code may start. This is an internal review, not
independent verification (F-27).

Found in QEMU 10.2.1 (`hw/i386/amd_iommu.c`): `dma-remap` defaults to off
(:2599), so nothing is translated. With it on, an address space switches to
translation only on an `INVALIDATE_DEVTAB_ENTRY` that names its devid with
Mode != 0 (:1142-1205). A Mode-0 entry passes everything, whatever IR/IW
say (:1163). With `dma-translation=off`, HATS reads 11b (:120). So §1.3's
blocked entry and §1.2's accept order, as written, cannot be shown on the
row.

1. **Blocked, as QEMU enforces it.** The row runs `-device
   amd-iommu,intremap=on,dma-remap=on`.
   * A translating unit's blocked DTE is V=1, TV=1, Mode=3, IR=IW=0,
     DomainID 0 (never handed out), with the interrupt half as §1.3. Its
     root is one kernel-held zeroed table page that every blocked entry
     shares.
   * A HATDis unit's blocked DTE stays Mode 0, IR=IW=0, root 0.
   * Bring-up issues `INVALIDATE_DEVTAB_ENTRY` for every devid enumeration
     found and for the unit's own, as well as `INVALIDATE_IOMMU_ALL`.
   * §1.3 and §1.6 say why. A1 shows `edu`'s blocked write faulting, and
     reads its event as provoked.
2. **Accept order.** HATDis is decided before HATS, and a HATDis unit's HATS
   is not read. A host test runs on QEMU's exact HATDis EFR.
3. **Unpin is shown by a stale DMA.**
   * The check: `edu` DMAs through a pinned page, the page is unpinned, and
     `edu` DMAs again. That second DMA faults (provoked) and nothing lands.
   * Its control: drop `INVALIDATE_IOMMU_PAGES` from unpin's flush, and the
     second DMA lands (FIRED).
   * R7's own control sabotages the wait instead.
   * Flush-after-map is shown by a host test plus an argument, because QEMU
     caches no negative entry. Its requirement says so.
4. **DTE discipline.**
   * One per-unit lock covers both halves of a DTE: the DMA half
     (attach/detach) and the interrupt half (first vector).
   * Neither write touches the other half's words (host test).
   * The function's table is allocated, zeroed, and its IRTE written and
     invalidated before IV/IntCtl/root are set.
   * An allocation failure in `msi_allocate` answers `Err` before a vector
     is taken.
5. **Fail closed.**
   * Every IVHD unit enters remapping's `STATES`, accepted or refused, and a
     refused one enters as not remapping.
   * An IVRS that is present but unreadable, and a lower-type IVHD that
     names a unit absent at the chosen type, are each said on their own line
     and place nothing behind a unit.
6. **§6 completed in the as-built.**
   * The `Remapper` goes through `programmed_unit_for`, `function_isolated`
     and `route`.
   * Also covered: discover's third arm; the placement line's `Kind`;
     `arch/x86_64/mod.rs`'s redirection entry; the panic catalog's
     VT-d-named entries.
   * `dma_faults.rs`: QEMU's trace carries only the address, so faults are
     counted, not attributed, and the doc says so.
   * MEMORY-AND-TIMING: the tables, the rings, the shared blocked root, the
     bring-up invalidation cost, and lock holds.
   * Also updated: the unsafe trace, AoU-12, V-03.
7. **Ids.** Reserve **L.iommu.79-98** first, in a docs-only landing, and
   release them in the writing commit:

   | ids | names |
   |---|---|
   | 79-83 | ivrsReadAsWritten, unreadableIvrsIsUnresolved, oneIvhdTypePerUnit, anAmdUnitIsAccepted, firmwareLeftOnIsStopped |
   | 84-89 | everyDeviceEntryBlocked, aDomainIsAModeThreeTable, commandsWaitBySequence, aFailedWaitFailsTheUnit, unpinForgetsBeforeRelease, mapIsFlushed |
   | 90-93 | everyEventIsRead, aFunctionsInterruptsUseItsOwnTable, noOtherInterruptIsDelivered, theConsoleLineByItsSpecialEntry |
   | 94-98 | ivmdAsIdentityDomains, unhonourableIvmdHoldsRemappingOnly, exclusionRegistersOff, aliasedIsUnresolved, amdviOff |

   * L.iommu.8, 67 and 73-78 stay VT-d's.
   * L.iommu.46 is extended only if its host test gains the IVHD cases.
   * L.device.28 is checked against the AMD route.
8. **Controls.** Each runs as `gate.sh control --expect` and reads FIRED on
   the landing patch:
   * A1 with (1)'s form;
   * A2, A3 and A5-A9;
   * (3)'s control;
   * ExEn written 1;
   * host controls for the truncation mark, the decoder's QEMU branch,
     (2)'s order, and `table_descriptor_at`'s default.
9. **Gates.**
   * `cargo xtask check`, including coverage carry and the justification
     check.
   * test-boot x86_64 kvm (VT-d) and `--vtd-aw48`, with boot lines diffed
     against the merge base: identical.
   * aarch64 tcg, and armv7a tcg `--smp 2`.
   * `--amd-vi` on kvm, and once on tcg.
   * `test-net --amd-vi`, and the three disk boots under `--amd-vi`.
   * check-docs.
   * test-nvrm as §5.3 says.
10. **Decoder.** The QEMU branch applies only when I=1 and the address is
    outside 0xFEE00000-0xFEEFFFFF; write comes from flags bit 1. It is
    documented as a QEMU model deviation, and the probe's comment names the
    bus-0 limit.
11. **Kernel lines.** The per-file figure counts the ring-0-linked library
    files too. More than 25% over §10's estimate is explained.
12. **Hardware.** No nazuna claim until both exist:
    * the IVRS fixture test;
    * a bare-metal boot of the landing hash under checks=skip, in a BACKLOG
      hardware row with §9's lines.

    nazuna is the gate host, so that boot is the PO's or customer's call.
    O1 lands with the driver.
13. **Table size.** Correct §3.1's reason. QEMU remaps only fixed and
    arbitrated messages, so a forged index is at most 511, and 2 KiB covers
    it. Allocate 2 KiB, or keep 8 KiB and give the true reason.

**Decisions.**

* **D1** OK, as changed by (1).
* **D2** OK. NextLevel is the level of the table pointed to, host-tested per
  level.
* **D3** OK.
* **D4** OK. Aliases stay unresolved, plus (5).
* **D5** OK if (4) and (13).
* **D6** OK, verified in QEMU, if (10).
* **D7** OK, with K2's conditions carried over.
* **D8** §6 plus (6).
* **O1** Yes: `ferrix.amdvi=on|off`, mirroring L.iommu.71 exactly.
* **D9 / §8** This is the customer's decision; the consultant's view is
  below.

The consultant's view on D9: (b) for claimed images and (a), announced, for
development images.
* The gate checks DMA_TRANSLATED as well as INTERRUPTS_ISOLATED for every
  bus master.
* The opt-out is named, and said when used.
* The policy lives in devmgr, with its own design and review.
* Most urgent: the GPU gate checks INTERRUPTS_ISOLATED only (`gpu.rs:136`).
  A held unit or a HATDis unit therefore starts a client-programmable GPU
  with untranslated DMA.

**D10 addendum (§9.1), 2026-10-24** (ledger line 3923): **OK IF (1)-(5)**.
This is an internal review, not independent verification (F-27).

The customer has decided D9: every bus master is gated (ledger 3905;
DMA-GATE.md 3906).

1. **The unit's own function** is exempt only when its address is the IVHD
   devid **and** its class is 08h/06h. It then gets `domain_for` None and
   `route` Err. Otherwise it is bypassing.
2. **The root complex** is exempt only when all of these hold: bus 0, class
   06h/00h, named by no entry, and bus master enable read clear at
   enumeration.
   * With BME set it is bypassing.
   * It never gets a domain, a vector or a driver, and the kernel never sets
     its BME.
   * Host tests: NAZUNA, plus synthetic cases for BME set, bus != 0, another
     class, and a named host bridge.
3. **IVRS only.** Both exemptions apply to IVRS placement only. The DMAR
   and IORT lines stay byte-identical.
4. **Ids.** Reserve L.iommu.99 (theUnitsOwnFunctionIsTheUnit) and
   L.iommu.100 (anUnnamedHostBridgeIsTheRootComplex) first.
5. **Provenance.**
   * The fixture's comment gives the SHA-256, who read the bytes and how,
     and the date.
   * `nazuna-IVRS.bin` is committed beside the fixture, or removed.
   * §9's expected line is updated.

**Final review** (2026-10-25, feature/amd-vi at 50e0ca78c; ledger line
4169): **OK IF (1)-(3)**. (1) test-boot aarch64 PASSED on the landing
patch; (2) the table above says which check catches each control; (3) the
commit message cites 3867, 3923, 4108 and 4169. Still owed after landing:
the nazuna bare-metal run and a QEMU IVMD patch for `L.iommu.95`
(`docs/BACKLOG.md`).
