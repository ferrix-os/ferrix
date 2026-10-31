# A cheap restart after a fatal machine check

Design v2, 2026-10-31 (v1 e0effb1b2; consultant OK IF (1)-(12) for slice
8a, 8b NOT YET (B1), ledger 5467, carried here). The customer's item 8 of
the machine-check follow-ups (2026-10-30: "cheap restart after a crash:
fast boot, pstore crash log, 'left from before' record surfaced"). It
builds on `docs/MACHINE-CHECK.md` (§3.1 the left records, §3.4 the retired
set), `docs/POSTMORTEM.md` (PM3's pstore region, PM4's event log, S1's stop
word) and `ferrix.panic=reset` (`panic/reset.rs`, BACKLOG's panic-reboot
row).

**This is slice 8a, the warm reset.** 8b, bad frames across a power cycle
in the event log, is a design of its own (B1: one record per frame in the
event log's ring would push every panic record out; the set must live
outside the ring, as an amendment to PM4) and a BACKLOG row. Making the
restart faster -- the 10 s hold, a reset by default -- stays the
customer's R1.

## 1. What is there today

* **A fatal machine check** stops the kernel through the panic path
  (`trap::fatal`, catalogue FX-9005), whose report prints the banks. With
  `ferrix.panic=reset` (off by default) the machine resets 10 s later.
* **The record**: pstore keeps the log's tail and S1's stop word, here 1,
  "a panic". Code 7 is the lockup detector's (POSTMORTEM §6.2, reserved by
  the lockup branch).
* **The banks** keep a fatal record across a warm reset on hardware where
  firmware leaves them (many PC firmwares clear them), and in QEMU (its
  banks lie past `end_reset_fields`). The next kernel's poll prints it as
  left from before this boot and retires its frame (L.x86_64.222) -- long
  after the boot's allocations: the frame may be the kernel's own, and a
  restart can stop on the same page again.
* **The retired set** is RAM: forgotten at every boot.

## 2. What it does (8a)

1. **Stop word 8, "a machine check"** (L.console.58): `panic::begin_report`
   stores 8 instead of 1 when the first report's catalogue entry is
   `MACHINE_CHECK`, on the ring-0 stop and any other path that reports it;
   1, 7 and 8 never replace each other -- the first report's word stands.
   The loader says `boot N ended: a machine check`.
2. **The bad-frame table** in the pstore header page, **header version 3**
   (L.console.59): bytes 128..648, a count word and 64 entries, each the
   frame's address and its complement (the stop word's rule; an entry
   whose halves do not match is ignored). The layout lives in
   `ferrix_bootinfo::pstore`; `encode` zeroes the whole table; `fresh`
   refuses version 2 and any non-zero table byte; `validate` reads versions
   1, 2 and 3; a loader and kernel of different versions fail closed (no
   table is read or written where the header is not version 3, shown by
   test-ab-boot). The event log's identity CRC covers bytes 0..32 as
   before, which v3 states. V-14 names the table.
3. **The kernel writes it** (L.mm.109): `mm::retire_frame` records each
   frame it newly retires -- `Claimed` and `Marked`; not `Full`, whose frame
   the kernel could not keep out of use this boot either, so recording it
   would claim what the boot itself does not do, and not `Already` -- by
   an atomic `fetch_add` on the count to claim a slot, then the entry's
   store, then one cache-line write-back (`flush_cache_lines` of the
   entry), listed in MEMORY-AND-TIMING; past 64 the entry is dropped and
   said once. The kernel reads only its own count. Stage 2's checked
   retirements write a check-owned copy of the header, never the live
   table (the check requires the live count unchanged).
4. **The machine check's stop writes its own records** (L.x86_64.254, (5)):
   before its report, on the IST stack, for each bank holding an
   uncorrected record with a usable page address (the rules of (9)), the
   stop writes the page into the table by the same lock-free claim and
   store. Taken because many PC firmwares clear the banks at reset: the
   table is then the only memory of the frame across the restart, and H.FAIL.12
   need not depend on the banks surviving. Atomics and two stores, as the
   paranoid entry's rules allow.
5. **The loader keeps each page out** ((7), outside the item): before it
   touches the pstore region it reads the banks ((8), (9)); then it reads
   the table of a valid version-3 header; for each frame named by either it
   calls `AllocatePages(AllocateAddress)` with a Ferrix memory type of its
   own (0x8000_0007), before it stages the kernel or the boot info, so
   nothing of the loader's lands there; a refusal is one boot-log line. The
   map hands the page to the kernel as a new **`MemKind::BadFrame` (16,
   appended)**, distinct from `Defective` (11, firmware's); every match on
   `MemKind` names it. **It is not RAM to the kernel** (`is_ram` false, as
   `Defective`): the direct map does not translate it, so a speculative
   read cannot reach a poisoned line; the cost is splitting the large
   direct-map pages around at most 64 pages. The loader's line: `bad frame
   <f> kept out (<table|left>)`.
6. **The loader never reads a poisoned region page** ((8)): a bank record
   naming the pstore header page skips the region that boot (said); one
   naming a ring page skips the text (said); otherwise poison in the region
   would loop the restart inside firmware.
7. **The bank read** ((9)): read-only, only banks below `MCG_CAP`'s count,
   only on a vendor it names (Intel, AMD); Intel's rule (UC, `ADDRV`,
   `MISCV` with a page-sized or finer grain) and AMD's (UC, `ADDRV`, the
   poison code); pure decoding in `ferrix-mce-decode`-style host-tested
   functions shared with the kernel's rules where they can be.
8. **The kernel leaves them out** (L.mm.110): the frame allocator's
   initialisation gives no `BadFrame` page to its free lists (they are not
   RAM), enters each in the retired set before the first allocation (a
   direct insert, `mm::keep_out`, since `retire_frame` answers `NotCovered`
   for a frame it does not manage), writes each into the new table again
   (so a frame stays out across consecutive warm resets with no event log),
   and says `mm   N frames left out as bad`. `retire_frame` tests `Already`
   before `NotCovered`, so the poll's later retirement of the same left
   record says `already retired` (its line), not `not retired`.
9. **The restart's cost, measured**: from the stop to the next boot's
   `FERRIX-BOOT-OK` under `ferrix.panic=reset`, its parts named (the 10 s
   hold, firmware, the loader, the boot); QEMU in the gate, the N150 a
   BACKLOG row.

## 3. Assumption of use (the loader)

The loader is outside the item (A.FIRMWARE). The kernel only ever leaves
memory out on its word -- the risk is RAM lost, never wrong data -- and
relies on it as **AoU (new)**: the loader marks `BadFrame` exactly the
frames the version-3 table and the banks name, never one more, and never
drops one; it reads no pstore page a bank names. The gate's lines are its
evidence, not the item's.

## 4. What it does not do

* **8b**: nothing survives a power cycle; BACKLOG row.
* **No default reset**, no shorter hold: R1.
* **No guess from text**: the loader reads the table and the banks only.
* **Arm**: no banks, and nothing retires frames there today; the table and
  the kind work for any retirement that comes.

## 5. How it is checked

* **Host tests**: the table's encoding, a damaged entry ignored, the 64
  bound, `fresh`'s refusals, `validate` of versions 1-3, the bank decoding
  rules.
* **Stage 2 check**: a `BadFrame` region in a check map is never in the
  free lists and is in the retired set; a checked retirement writes a
  check-owned header copy, the live count unchanged; `retire_frame` of a
  kept-out frame answers `Already`.
* **`test-mce-restart`** (x86-64, TCG with Intel's records; without
  `-no-reboot`; `ferrix.panic=reset`): a fatal record at the victim's page
  (test-mce's victim, `PCC`); required: the stop's report; the reset; the
  next boot's `boot N ended: a machine check`; `bad frame <f> kept out
  (table)` and `(left)`; `mm   1 frames left out as bad`; `<f>` in
  `/proc/ferrix-mce-retired` before any program allocated; the poll's
  `left from before this boot` and `already retired`; the restart's time
  and parts said. Under KVM if injection survives its reset (QEMU keeps
  the banks under KVM too: `kvm_put_msrs` restores them), else said why.
* **Negative controls**, each FIRED on its text through `gate.sh
  control`: the allocator's leave-out removed (stage 2); the table write
  removed (no `(table)`: the judge); the loader's bank read removed (no
  `(left)`: the judge); code 1 stored for 8 (the loader's line: the
  judge); `fresh` accepting a non-empty table (the host test).

## 6. Requirements (reserved first, released by the code landing)

* **H.FAIL.12**: after a warm reset, the kernel shall hand out no frame the
  boot before retired, nor the frame of an uncorrected record the machine
  check named, where the loader marked it left out (AoU §3). Parent
  O.FAILSAFE, ASR-6.
* **L.console.58**: stop word 8 at a machine check's first report.
* **L.console.59**: the version-3 table's format and `fresh`'s refusals.
* **L.mm.109**: `retire_frame` records each newly retired frame.
* **L.mm.110**: `BadFrame` pages are never in the free lists, are in the
  retired set before the first allocation, and are written into the new
  table.
* **L.x86_64.254**: the stop writes its uncorrected records' pages into
  the table before its report.

## 7. Docs

POSTMORTEM §5.1 (code 8), §3.1 (header version 3); MACHINE-CHECK §3.1,
§3.4; SAFETY-MANUAL (the AoU); MEMORY-AND-TIMING (the write-back);
VULNERABILITY-ANALYSIS V-14 (the table); BACKLOG (the N150 restart time;
8b).

## 8. Where it stands

* Design v1 (e0effb1b2) reviewed 2026-10-31, ledger 5467: **OK IF
  (1)-(12)** for 8a, 8b NOT YET (B1). v2 carries the split (8a only), code
  8 (Q5), (1) the ids, (2)-(4) §2.1-2.3, (5) taken (§2.4), (6) §2.8, (7)
  §2.5, (8) §2.6, (9) §2.7, (10)-(11) §5, (12) §7.
