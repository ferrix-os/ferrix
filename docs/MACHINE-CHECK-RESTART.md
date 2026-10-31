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
   own (0x8000_0008; 0x8000_0007 is already the loader's SMBIOS copy), before it stages the kernel or the boot info, so
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

* **8b**: nothing survives a power cycle in 8a; slice 8b is §9.
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
* As built: the loader's type for a kept-out page is 0x8000_0008
  (0x8000_0007 is its SMBIOS copy); the bank read and its keep-outs come
  first, before the boot info area, and the table's inside `pstore::take`,
  where a page already the loader's is a refusal line; boot 1's checks hold
  the table (`mm::hold_table`, released by the checks' undo), so the stage 2
  and stage 12 retirements never reach it; test-mce-restart judges boot 2 up
  to its poll's `already retired` line, not `FERRIX-BOOT-OK`, since stage
  12's write check needs the fresh writable disk xtask makes per QEMU start,
  and measures the restart to boot 2's poll (13.6 s locally, 11.0 s of it to
  the loader); boots after boot 2 keep the frame out from the table the
  attach writes again.
* Code reviewed 2026-10-31 at 76777731e, ledger 5487: **OK IF (1)-(6)**:
  (1) the stop records only usable addresses (`retirable`'s rules, nothing
  on a vendor with no rules); (2) test-mce-restart-retired, a recovered
  retirement through `retire_frame` kept out of boots 2 and 3 from the table
  alone, with its two controls; (3) the residual of the loader's order named
  (a table frame the boot info area or firmware holds is not kept out, said
  as `is in use`); (4) stage 2's live-table comparison from before
  `check_retired`; (5)-(6) the landing. Rulings: (a) the restart measured to
  boot 2's poll, the N150 row owing it to `FERRIX-BOOT-OK`; (b) KVM not owed
  (TCG only; KVM injection across a reset not tried, a BACKLOG line); (c)
  the postmortem cases leaving the machine check to this gate; (d) the late
  `&raw const` token; (e) a frame above 2^32 answering Full. The loader reads
  only the boot processor's banks; another processor's core-scoped record is
  kept out through the table its stop writes (C4).

* Design v3 (§9, cd59d740b) reviewed 2026-10-31, ledger 5511: **OK IF
  (1)-(11)** for 8b; Q1 (a), H.FAIL.13, reserved in a docs-only landing;
  AoU-39 amended, no new AoU, no low-level id.

## 9. Slice 8b: a bad frame stays out across a power cycle (design v3)

Design v3, 2026-10-31 (cd59d740b; B1 of ledger 5467). The consultant's
verdict **OK IF (1)-(11)**, ledger 5511; v3.1 carries (1), (3), (4), (5)
and (7) here, the rest at code. 8a's table lives in
RAM and is gone after a power cut, a clean power-off or a cold boot. 8b
keeps the bad-frame set on disk, in PM4's event-log partition, written by
the loader of a boot that is not failing -- never by the kernel, which
still writes nothing but RAM (PM4's E1) -- and read by every boot's
loader, before its own allocations.

### 9.1 Where the set lives: an extension of each header copy

Not as records in the ring (B1: 64 frames would push the box's 31 panic
records out, and later panics would overwrite them). Each header copy is
4 KiB, of which one block is written and 64 bytes used (POSTMORTEM §4.1).
The set goes into the same block, after the header, as an **extension**:

```
header copy (one block, as PM4):
  0   "FXEVENTS" ... 40 CRC-32C of 0..40       (PM4's header, version 1, unchanged)
  64  "FXBADFRM"   72 version 1   76 count (0..=64)   80 reserved, 0
  84  64 frame numbers, u32 each (4 KiB pages), the first `count` strictly
      ascending, the rest 0
  340 CRC-32C of the header's 0..40 continued over 64..340 (the header's
      bytes too: an extension belongs to its copy's generation and cannot be
      paired with the other copy's; not over 40..44, see §9.8)
```

* **PM4's header stays version 1** and its bytes unchanged, so every PM4
  reader (an older inspection tool, an older loader) still reads the log.
  A copy whose bytes 64..72 are zero has no extension: an empty set. A
  copy with the magic and a bad CRC, version, count, order or non-zero
  tail is **invalid as a whole** (`Header::decode` refuses it), so
  `select` falls back to the other copy -- never to "no frames".
* **Atomic enough**: 344 bytes fit one 512-byte block, so a copy is one
  block write, as today; a disk that tears inside a block fails the CRC
  and the other copy holds.
* **Order and dedupe** are part of the format: strictly ascending means
  no frame twice, and the decoder checks it.
* **The 64 bound** is the pstore table's (§2.2): the loader's own count of
  kept-out pages (`badframes::MOST`) grows from 96 to 160 (banks 32,
  table 64, log 64).

### 9.2 Who writes it (PM4's EL3 amended)

* **EL3 (amended)**: the loader writes the log (a) a record, on a boot
  whose pstore region holds a valid record with a panic report (PM4,
  unchanged); and (b) **both header copies alone**, on a boot whose
  pstore table or machine-check banks name a frame not already in the
  log's set. A boot whose set is unchanged writes nothing: a healthy
  machine's disk is never written by the loader.
* **The set only grows** in Ferrix: no loader or kernel path removes a
  frame. A frame the loader could not keep out ("in use") still goes into
  the set: the set records what the evidence named, the map what could be
  done about it.
* **No fresh header over damage** (ledger 5511 (3)): `append` and
  `add_bad_frames` write a fresh header only when neither copy carries
  `FXEVENTS`. Copies that carry it and both fail to decode -- damage, or an
  extension version a later loader left behind (an A/B rollback) -- get no
  write, said once: `events   header damaged or newer: nothing written, its
  bad frames not read`. A copy without the extension's magic but with a
  non-zero byte in 64..344 is invalid, not an empty set. (POSTMORTEM
  §4.3's "a fresh header when neither is valid" is amended to this.)
* **The write** (`ferrix_eventlog::add_bad_frames`, host-tested): read the
  header (a fresh one, both copies, only as above);
  merge the new frames (sorted, deduplicated, at most 64); if nothing
  changed, no write; else copy A with generation + 1, flushed, then copy
  B, flushed (PM4's `write_header`). `append` carries the set through its
  own header write (a record never drops the set).
* **The region rule** (ledger 5511 (4)). The pstore table is the set's
  only other copy, so, as with a record (E2): **`add_bad_frames` failed**
  -> the region is kept as it is, not handed over (§3.3, 4's path), said
  every boot, and the next boot tries again; a failure that persists turns
  pstore off until the log is repaired (§9.5, a BACKLOG row). **The log
  absent, unusable or damaged** (as above) -> the region is renewed, said:
  there is no disk copy to wait for, and the kernel's attach rewrites every
  kept-out page into the new table, so the frames still cross warm resets.
  The decision is a host-tested pure function the loader calls
  (`ferrix_eventlog::may_renew(record, frames)`), with its control. A torn write: the set only grows, so the older copy is a subset
  of the newer; falling back to it loses only this boot's additions, which
  the un-renewed table still holds.
* **Full**: a 65th frame is not added, said (`events   bad frame <f> not
  kept in the event log: its 64 are`); it still stays out across warm
  resets through 8a's table (the kernel's attach rewrites it), and the
  region is renewed (not a failure).
* **Where there is no log** (the bare FAT image, `ferrix.pstore=off`, an
  unusable or damaged partition): nothing is written; 8a's warm-reset path
  only.
* **A frame at or above 16 TiB** never reaches the set: the kernel's
  `record_bad_frame` and the banks' `left_page` refuse a frame that is not a
  u32 already (8a), so every frame the loader holds fits the set (§9.8).

### 9.3 Who reads it

Every boot's loader, with the log present, **right after the bank read and
before the boot info area** (moving `events::open` out of `pstore::take`
into `boot_info_and_pstore`): each frame of the set is kept out as 8a keeps
one, said `badframe <f> kept out (log)` (or `(log, as already)` when the
banks named it first), whether or not pstore is on. The kernel needs no
change: a `BadFrame` page is left out (L.mm.110) and written into the new
table at attach, whatever kept it out. The GPT scan's buffers are freed
before the keep-out (they are today), the header is read into the stack
block, and the append's buffers are allocated only after it. A log present
but unreadable says `events   its bad frames not kept out this boot`.
The summary line gains the set:
`events   N records; ...; 2 bad frames kept`.

### 9.4 Who clears it (B1: the clear tool's authority)

Only a person who is root on a host with the disk, after replacing the
memory: `ferrix-eventlog clear-bad-frames DEVICE` (the inspection Linux
image carries it) writes both copies with an empty set and generation + 1,
and prints the frames it dropped. `ferrix-eventlog list` prints the set;
`append` carries it. Raw-device access is root's (V-15). Ferrix never
clears it, and no Ferrix interface asks to.

### 9.5 Residuals (named)

* **A power cut or clean power-off before any warm reset** loses a frame
  the kernel retired in that run: only a loader writes the disk, and only
  a warm reset carries the table to one. The frame is retired again when
  its error recurs. (A kernel disk write is ruled out by E1.)
* **An older writer** -- a loader from before 8b (an A/B rollback) or an
  inspection tool from before 8b -- rewrites a copy without the extension
  on its first header write and so drops the set (the records survive).
  The integrator ships the tool with the loader (AoU-39's amendment).
* **A forged set** (raw disk write, outside Secure Boot's reach) costs at
  most 64 pages; a frame planted at the pstore region's place costs the
  region (`allocate_at` refuses: no region that boot, said), never the
  boot: nothing else the loader allocates is at a fixed address.
  VULNERABILITY-ANALYSIS V-14 amended.
* **A log that keeps refusing writes** keeps the pstore region off every
  boot (the region rule): the post-mortem record is lost until the log is
  repaired, said every boot; BACKLOG row.
* **Another boot disk, or a reinstall** starts with an empty set.
* **The tool's commands unrun**: no gate runs `ferrix-eventlog list` or
  `clear-bad-frames` as a binary; the gate calls the functions they wrap.
* **The banks' early write unchecked** (ledger 5531 C1): the write of the
  banks' frames right after the pstore switch is not part of the region
  rule; if it fails and the region then has no valid header to keep, the
  region is renewed with those frames not on disk. They still stay out
  across warm resets (the kernel's attach), so this is the log-refusing-
  writes residual in another form. The early "already held" test in
  `keep_frames` relies on nothing else writing the log during a boot,
  which holds today.
* **A 4 KiB-block disk** writes the header copy as one 4 KiB block: the
  same layout, still one block.

### 9.6 How it is checked

* **Host tests (`ferrix-eventlog`)**: the extension's encoding, a copy
  with no extension read as empty, every refused field (CRC, version,
  count > 64, order, duplicates, a non-zero tail) making the copy invalid
  and the other copy taken; an extension moved to the other copy refused
  (the CRC binds the header); the merge's dedupe, order and bound;
  unchanged input writes nothing (0 block writes); `append` keeps the set;
  a torn `add_bad_frames` -- the device stopped after every number of block
  writes -- leaves the old set or the new, never fewer frames than the old;
  `clear` empties it; the range check unchanged (no write outside the
  partition).
* **`test-mce-restart` mode `PowerCycle`** (x86-64 TCG, the disk in a GPT
  with the log as `test-postmortem` builds it), TCG only: boot 1 recovers
  an SRAR and retires the victim's frame, then a warm reset (8a's Retired
  mode); boot 2's loader says `kept out (table)` and `bad frame <f> kept in
  the event log (1 of 64)`; the QEMU process is ended (the power cut); a
  second process boots cold: boot 3's loader says `kept out (log)`, no
  `(table)` and no `(left)`; its kernel `1 frames left out as bad`; a warm
  boot 4 writes nothing (the header's generation, read by the host tool
  before and after, unchanged); the host's `ferrix-eventlog list` on the
  image prints the frame; after the host's `clear-bad-frames`, a cold boot
  5's loader keeps nothing out. The gate's host side reads and clears the
  set by the library functions the tool calls (`read_header`,
  `clear_bad_frames`) on the disk file; no gate runs the binary's `list`
  or `clear-bad-frames`, thin wrappers over them (§9.5). **The counted
  retry** (ledger 5564): an attempt is inconclusive only when boot 2's
  loader says exactly `badframe <page> could not be kept out (table): it is
  in use` and still `kept in the event log (1 of 64)`, or cold boot 3's says
  `could not be kept out (log): it is in use` beside its set's summary line,
  with every other check of those boots met; it is said with its page and
  boot and made again from a fresh disk and injection, at most 3 attempts,
  and all 3 inconclusive fail. The pass line says how many were (`N of M
  attempts inconclusive (firmware held the page)`). A pure classifier
  (`after_boot_2`, `after_boot_3`) decides, host-tested with its control.
* **Negative controls** (`gate.sh control`): the loader's log read removed
  (boot 3: no `(log)`); `add_bad_frames` not called (boot 2: no line);
  `append` not carrying the set (host test); the CRC not covering the
  header (host test, the swapped extension); the order check removed (host
  test, the duplicate); the renewal moved before `add_bad_frames` (the
  `may_renew` host test with its control); plus (ledger 5511 (3), (8))
  a fresh header written over damaged copies (host test: both copies
  damaged and an unknown extension version each give 0 writes) and a
  write on an unchanged set (host test: 0 block writes).

### 9.7 Requirements and docs

* **Q1 for the review**: the kernel is unchanged and the loader is outside
  the item. Either (a) a new **H.FAIL.13** "after a power cycle, the kernel
  shall hand out no frame a boot before retired, where the loader marked it
  left out", criterion L.mm.110 and the `PowerCycle` gate, with no new
  low-level requirement; or (b) H.FAIL.12's statement widened to "after a
  warm reset or a power cycle". The design proposes (a), keeping 8a's
  landed claim as it is.
* **AoU-39 amended** (or AoU-40): the loader also marks exactly the log's
  set, never drops a frame, and writes the set before it renews the pstore
  header; the integrator ships the clear tool and the loader together and
  clears the set only after replacing memory.
* Docs: POSTMORTEM §4.1 (the extension), §4.2 (EL3 amended), §4.4 (the
  tool's commands); SAFETY-MANUAL (the AoU); VULNERABILITY-ANALYSIS V-14
  and V-15; MEMORY-AND-TIMING (one block written twice on a boot that adds
  frames); BACKLOG (8b's row closed by the landing; the residuals' rows).

### 9.8 As built

* **The extension's CRC** covers the header's checked bytes 0..40, then the
  extension's 64..340 -- not 0..340 as v3 said. A CRC-32C run over bytes and
  their own stored CRC (bytes 40..44) ends in the same state whatever the
  bytes, so a CRC over 0..340 bound nothing: the host test
  `an_extension_moved_to_the_other_copy_is_refused` caught it, an extension
  spliced onto the other copy decoding as valid.
* **PM4 headers stay readable both ways**: an empty set leaves bytes 44..344
  zero, byte for byte a PM4 header; the decoder also requires 44..64 zero.
* **Frames above 16 TiB** never reach the loader: the kernel's
  `record_bad_frame` and the banks' `left_page` refuse a frame that is not a
  u32 already (8a), so the set's u32 needs no check of its own.
* **The order in the loader**: `events::open` and `events::keep_out` run in
  `boot_info_and_pstore` after the bank read and before `boot_info_area`;
  `pstore::take` gets the opened log. The banks' frames are written to the
  log as soon as pstore is on (so a boot that ends with no region still
  keeps them), the table's before the renewal, and `may_renew` decides from
  both writes.
* **A damaged log with a panic record**: `append` refuses it (`Damaged`), so
  the record stays in RAM, not handed over, as PM4 keeps one for an
  unusable log.
* **The summary**: `events   N bad frame(s) kept out across power cycles`,
  a line of its own and only when the set is not empty, so boots without a
  set print what they printed.
* **The gate's host side** reads and clears the set by the library functions
  the tool calls (`read_header`, `clear_bad_frames`) on the disk file.
* **The counted retry** (ledger 5564, after m8b-d10 on 8f1a1d67e): that
  run's victim page 0x1e286000, near the top of the guest's RAM, was held
  by firmware when boot 2's loader read the table and again on boots 3 and
  4 (`could not be kept out (table)` then `(log): it is in use`), though
  boot 2 kept it in the log. AoU-39's order residual, made likelier by the
  GPT disk (firmware binds more drivers there); the gate now retries such an
  attempt as §9.6 says, and BACKLOG's 8b row (e) owes the read order.
* `test-mce-restart-power` passed locally on the first run: boot 2 `kept out
  (table)` and `kept in the event log (1 of 64)`, cold boot 3 `kept out
  (log)` only, warm boot 4 `(log)` and `(table, as already)`, the
  generation unchanged, boot 5 after the clear keeping nothing out.
* Code review 2026-10-31, ledger 5531: **OK IF (1)-(6)** (rustfmt only on
  the fix, these docs, the rows and controls, the commit body, coverage).
* Gate ruling 2026-10-31, ledger 5564: **OK IF (1)-(8)** (the counted retry, its classifier and controls, these docs, BACKLOG (e), the rows owed).
* Switch design v4 (§10, 5dde60e0c) reviewed 2026-10-31, ledger 5579: **OK IF (1)-(11)**; §10 is v4.1, written to it.
* Switch code reviewed 2026-10-31, ledger 5596: **OK IF (1)-(5)** (test_off and test_reset trace no requirement; the commit body; the rows and controls; these lines).

## 10. The switch: off, reset or persist (design v4.1)

Design v4 (5dde60e0c) reviewed 2026-10-31, ledger 5579: **OK IF (1)-(11)**;
v4.1 is written to those decisions. The customer (2026-10-31): "make that
configurable ... max should have that fixed at build time while e.g.
desktop has this at runtime configurable", choosing three modes, with only
`desktop` choosing at run time. Why it matters: an error an overclock
causes is not a defective DIMM, and under `persist` it would keep good RAM
out until root clears the set on a host (§9.4).

### 10.1 The modes

* **`persist`** (8a + 8b, today's behaviour): bad frames stay out across a
  warm reset (the pstore table) and across a power cycle (the event log's
  set).
* **`reset`** (8a alone): the loader keeps out the frames the banks and the
  table name. It writes nothing to the event log's set. A frame kept out
  from the log alone is released, and the set on disk is left as it is, so
  going back to `persist` restores it. A table frame stays out across
  warm reset after warm reset, since the kernel writes every `BadFrame`
  into the new table (L.mm.110), as 8a has it.
* **`off`**: nothing is kept out across a restart, and nothing is written
  to the set. The running kernel still retires a bad frame for the rest of
  its boot (MACHINE-CHECK §3.4).

The kernel does not change. It leaves out exactly the `BadFrame` pages the
loader hands it; the switch is the loader's alone.

### 10.2 Fixed at build time, or a boot word

* **The image key `bad_frames = persist | choice`** (an image key, not a
  flag: loader content, like `slot_signing`; flags select kernel code alone,
  BUILD-FLAGS rule 6 b).
  * `persist` allows `sc4`, `present`, `sc2` and `none`; `max`, `fast` and
    `present` name it.
  * `choice` allows `none` alone; `desktop` names it.
  * BUILD-FLAGS.md §3.1 lists the key, and certification-item.json's
    `reference_configuration` gains `bad_frames="persist"`.
  * **An ad-hoc build takes its base profile's key**: `loader_env` reads it
    from the resolved base profile, so the image manifest and the loader
    agree. The choice-mode gates run with `--kernel-profile desktop`; every
    other gate, H.FAIL.12/13's among them, stays on a `persist` loader.
* **Into the loader**: `loader_env` adds `FERRIX_BAD_FRAMES`. The loader's
  `build.rs` refuses a build without it or with an unknown value, and emits
  a `cfg` declared with `rustc-check-cfg`. **A `persist` loader compiles no
  read of the word and no release code**: it says `badframes persist
  (fixed by this build)` on every boot, and nothing about the word.
* **The boot word `ferrix.badframes = persist | reset | off`**, registered
  in `build-flags.toml` with the `choice` loader as its only reader, and
  permitted by `desktop` alone. It is read through
  `ferrix_bootinfo::option_in`, as `pstore::switch` reads its word. Absent,
  it means `persist`; a value not understood is said as `... is not
  understood, so persist`.

### 10.3 Order in a `choice` loader

1. As `persist` does: keep out the banks' pages, then the log's set (§9.3),
   then the boot info area with the command line.
2. **Parse the mode once**, after the boot info area and before
   `pstore::take`, and hand it to `take`.
   * Under `reset` and `off`, both `keep_frames` calls are skipped, and the
     region rule gets `NotNeeded`, never `Failed`, so the region renews.
   * Under `off`, the table's frames are not kept out either.
3. **Release after `take` returns**, before the kernel is staged. Until
   then the bank-named frames stay held: `names_bank_page` answers from them
   all through `take`, and the pstore region's `allocate_at` cannot land on
   a frame just freed.
4. **What is released** is decided by a pure, host-tested function of the
   mode and the kept set:
   * `reset`: every kept frame not named by the banks or the table
     (`Kept::named()`), so a log frame the table also names stays out;
   * `off`: every kept frame;
   * `persist`: none.

   Each freed frame is said `badframe <a> released
   (ferrix.badframes=<mode>)`. A FreePages failure is said, and that frame
   stays out.
5. The mode is said once: `badframes <mode> (ferrix.badframes)`, or `...
   (the word absent)`.

### 10.4 Settings

* **Every image's Settings page reads `bad_frames` from the image
  manifest** (the 3623 pattern). On a `persist` image it shows "bad frames:
  kept across restarts, fixed by this build" and offers no control.
* **On desktop the word is set by B5's `set-boot-word`.** Its list becomes
  `present`'s list plus the boot words whose registry entry says
  `settable = true` and that the image's profile permits.
  * `ferrix.badframes` is the only settable word.
  * check-build-flags refuses `settable` on a test-only or claimless word.
  * A BACKLOG row for B5's owner says this, and B5's design also decides
    what a slot boot does with the word: its command line comes from the
    slot, not `CMDLINE.TXT`.
  * Until B5 lands, root edits `CMDLINE.TXT` on the ESP.

### 10.5 Claims

* **H.FAIL.12 and H.FAIL.13** are scoped to "an image built
  `bad_frames=persist`, whatever its command line". Their docs say that a
  `choice` image gives them only with the word absent or `persist`. Both
  criteria add the strings check (§10.6).
* **AoU-39** gains these points:
  * the integrator uses an image whose manifest says `bad_frames=persist`;
  * "never dropping one" is a `persist` loader's promise;
  * `off`'s residual: the loader hands the kernel a page a bank's
    uncorrected record named, and only the kernel's poll at bring-up
    retires it, possibly after the allocator gave it out.
* POSTMORTEM §4 and V-14 say that a `choice` loader set to `reset` or `off`
  keeps fewer frames.
* No new id: the kernel is unchanged, and the item-hits of the code landing
  show 0 core and 0 item.

### 10.6 How it is checked

* **check-build-flags**: every profile names `bad_frames`; `choice` at a
  certified level and the key unset are refused (self-tests);
  `ferrix.badframes` is permitted by desktop alone (a self-test refuses it
  elsewhere); `settable` is refused on a test-only or claimless word.
* **The loader's build** refuses a missing or unknown `FERRIX_BAD_FRAMES`.
* **The strings check**, on every architecture's loader, both ways: no
  `ferrix.badframes` in a `persist` loader, and the text present in a
  `choice` loader.
* **Host tests**: the release set for each mode (a log frame the table
  names stays out under `reset`); the word's parse (absent, unknown).
* **Gates**, on `--kernel-profile desktop`, test time kept first:
  * `reset`: boot 2 keeps the frame out from the table, writes nothing to
    the log (the generation unchanged), and the region renews; cold boot 3
    keeps nothing out, and the set on disk is unchanged.
  * `off`: boot 2 releases the frame, and its kernel leaves nothing out.
  * The word on a `persist` build: appended to the existing PowerCycle run
    if its boot-line check allows it, else one more run; the commit states
    the boots and time added.
* **Negative controls** (`gate.sh control`, each FIRED):
  * (i) `off`'s release removed;
  * (ii) `reset` writing the log;
  * (iii) a `persist` build forced to compile the parser (the strings check);
  * (iv) `reset`'s release set taking a table-named frame (host test);
  * (v) `reset` handing the region rule `Failed` (the region kept).
* BACKLOG's 8b row (e), the loader's read order, gains one sentence: the
  release stays after every keep-out and every region read, wherever (e)
  moves the reads.

### 10.7 As built

* **The pure part lives in `ferrix-eventlog`** (`badframes`: `Mode`, the
  word's parse `choice`, the release predicate `released`), not in
  `ferrix-bootinfo` as a first draft had it: `ferrix-bootinfo` is compiled
  into the kernel, and the switch is the loader's alone (0 core, 0 item).
  check-build-flags names the file among its library readers, as it names
  `pstore::switch`.
* **The lines**: `badframe mode persist (fixed by this build)` (every
  `persist` loader, every boot); on a `choice` loader `badframe mode <m>
  (ferrix.badframes)`, `... (the word absent)`, or `badframe
  ferrix.badframes=<v> is not understood, so persist`; `badframe <a>
  released (ferrix.badframes=<m>)`, or `not released ...: firmware refused`.
* **"Claimless" in 5579 (8b)** is taken as a free-form word (`*`, which
  B5's closed list never takes): check-build-flags refuses `settable` on a
  `test_only` word and on one whose values hold `*`, and refuses
  `ferrix.badframes` in a profile whose `bad_frames` is not `choice`.
* **The strings check** is a step of `cargo xtask check` per architecture:
  this run's (`persist`) loader holds no `ferrix.badframes` and says `fixed
  by this build`; a `choice` build in a target directory of its own lints
  clean (`clippy -D warnings`) and holds the word.
* **The `off` gate** (`test-mce-restart-off`) uses 8a's fatal record: its
  page is in the banks after the reset, so boot 2's loader keeps it out
  first (`left`) and then releases it, which shows the release; a
  recovered retirement leaves nothing in the banks and, under `off`, the
  table is not read, so nothing would be kept to release.
* **The word on a `persist` build** is appended to `PowerCycle`'s image
  (`ferrix.badframes=off`), which also requires `badframe mode persist
  (fixed by this build)` on boot 2: no boot or time added.
* **The controls** (on c4bab0fe7, each `control: FIRED`): sw-d1, `off`'s
  release removed: "boot 2's loader did not release the page"; sw-d2,
  `reset` writing the log: "boot 2's loader wrote the event log under
  reset"; sw-d3, a `persist` build compiled with the parser: the strings
  step's own "the bad_frames = persist loader carries `ferrix.badframes`";
  sw-d4, `reset`'s release taking a table-named frame: host test
  `reset_releases_the_log_alone_and_off_releases_all`; sw-d5, `reset`
  handing the region rule `Failed`: "boot 2's loader did not renew the
  pstore region".
* **Test time**: the per-architecture check step adds about 112 s to a cold
  check (26 s x86-64, 72 s aarch64, 14 s armv7a); `test-mce-restart-reset`
  ran 319 s (4 boots) and `-off` 75 s (2 boots); `PowerCycle` gained no boot.
