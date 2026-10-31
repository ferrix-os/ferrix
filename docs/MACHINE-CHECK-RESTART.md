# A cheap restart after a fatal machine check

Design v1, 2026-10-31. The customer's item 8 of the machine-check follow-ups
(2026-10-30: "cheap restart after a crash: fast boot, pstore crash log,
'left from before' record surfaced"). It builds on `docs/MACHINE-CHECK.md`
(§3.1 the left records, §3.4 the retired set), `docs/POSTMORTEM.md` (PM3's
pstore region, PM4's event log, S1's stop word) and `ferrix.panic=reset`
(`panic/reset.rs`, BACKLOG's panic-reboot row).

## 1. What is there today

* **A fatal machine check** stops the kernel through the panic path
  (`trap::fatal`, catalogue FX-9005), whose report prints the banks
  (`mce::report`). With `ferrix.panic=reset` (off by default, a stop-gap
  awaiting the customer's R1) the machine resets 10 s after the report.
* **The record of it**: the pstore region keeps the kernel log's tail and
  S1's stop word, here `1`, "a panic": nothing says it was a machine check.
  The next loader copies the record into the event log (`Kind::Panic`),
  which outlives a power cycle (PM4), and says the stop on its boot line.
* **The banks** keep a fatal record across a warm reset on hardware (and in
  QEMU, whose banks lie past `end_reset_fields`). The next kernel's
  bring-up hands each one to its poll as **left from before this boot**,
  which prints it and retires its frame (L.x86_64.222) -- in the poll task,
  long after the boot's allocations: the frame may already be the kernel's
  own, and a kernel consuming it stops again. A restart can loop on the
  same bad page.
* **The retired set** is RAM: every frame retired is forgotten at the next
  boot, warm or cold, and handed out again until it fails again.

## 2. What it does

1. **A stop of its own.** The machine check's stop writes S1's stop word
   **7, "a machine check"** (from the reserved range), instead of 1, when
   its report is the first; the loader says "the last boot stopped on a
   machine check", and the event log records it as `Kind::MachineCheck`
   (new) with the report's text, as it does a panic.
2. **Bad frames remembered across a reset.** As the kernel retires a frame
   it also writes the frame's address into a **bad-frame table in the
   pstore header page** (bytes 128..640: a count and up to 64 addresses,
   each with its complement, the stop word's rule; written with release
   order, no lock, never read back by the kernel). The loader, at the next
   boot, before it gives the region a fresh header, reads the table.
3. **Bad frames remembered across a power cycle.** The loader appends each
   table entry it has not recorded before to the event log as a
   `Kind::BadFrame` record (address, the boot that found it, the source:
   retired or left), bounded at 64 bad frames in the log; past that it says
   so and records no more.
4. **Bad frames kept out of the next boot from its start.** The loader
   reads every `BadFrame` record in the event log, adds the frames the
   banks name (§2.5), and **marks each page `Bad`** in the memory map it
   hands the kernel (a type of its own in `ferrix-bootinfo`'s map). The
   kernel's frame allocator never takes a `Bad` page into its free lists,
   says how many it left out (`mm   N frames left out as bad`), and lists
   them in the retired set at once (so `/proc/ferrix-mce-retired` shows
   them and the counts stay one set).
5. **The left record acted on before anything allocates.** The loader reads
   the machine-check banks (x86-64, `MCG_CAP`'s count, `MCi_STATUS`,
   `MCi_ADDR`, `MCi_MISC`; reads only, never writes or clears) and, for an
   uncorrected record with `ADDRV` and a page-granular `MISC` (Intel's
   rule; AMD's `ADDRV` with the poison code), marks that page `Bad` too and
   records it with source `left`. The kernel's bring-up still reads,
   prints and clears the banks as today (L.x86_64.222), so the left record
   is surfaced as before, and now also kept out of use.
6. **The restart's cost, measured.** No new switch and no default changed:
   a fatal machine check takes `ferrix.panic`'s action like any panic, and
   whether that is a reset by default stays the customer's R1. The design
   measures and states the restart: from the stop to the next boot's
   `FERRIX-BOOT-OK` under `ferrix.panic=reset` on QEMU (and the N150 as a
   BACKLOG row), and names the parts (the 10 s hold, firmware, the loader,
   the boot).

## 3. What changes where

* **Kernel** (item): the stop word 7 at the machine check's report
  (`panic::begin_report` given the catalogue entry, or the machine check's
  `stop` writing it first); the bad-frame table's write in `mm::retire_frame`
  (`console`'s pstore module owns the region); `Bad` pages left out by the
  frame allocator's initialisation and entered in the retired set.
* **Loader** (outside the item, A.FIRMWARE): the table's read, the event
  log's two new kinds, the banks' read, the `Bad` type in the map, its
  boot-log lines.
* **`ferrix-bootinfo`**: the map's `Bad` type; the pstore header's table
  layout (version 3).
* **`ferrix-eventlog`**: `Kind::MachineCheck`, `Kind::BadFrame`, their text,
  the 64-frame bound, deduplication by address.

## 4. What it does not do

* **No default reset.** A fatal machine check halts unless `ferrix.panic=
  reset` is set, as every panic does; the customer's R1 decides that.
* **No bad-frame removal.** A frame once recorded stays out for good (an
  operator who replaced a DIMM clears the event log's bad frames with
  `ferrix-eventlog --clear-bad`, a ring-3 tool on the host or a later
  slice).
* **No guess at a frame from text.** The loader reads the table and the
  banks, never the report's text.
* **Not across a disk change**: the event log is the boot disk's.
* **Arm**: no banks; the table and the `Bad` map type work there for
  frames the kernel retired, which on Arm is none today.

## 5. How it is checked

* **Host tests** (`ferrix-eventlog`, `ferrix-bootinfo`): the table's
  encoding, a torn or damaged entry ignored, deduplication, the 64 bound,
  the `Bad` type's round trip.
* **Stage 2 check**: a `Bad` page in a boot map is never in the free lists
  and is in the retired set; the bad-frame table's write of a retirement
  (into a check-owned copy of the header).
* **`test-mce-restart`** (x86-64, TCG with Intel's records, no
  `-no-reboot`, `ferrix.panic=reset`): a fatal machine check after the
  victim's line at the victim's page; required: the stop's report, the
  reset, then the next boot's loader line `the last boot stopped on a
  machine check`, its line `bad frame <f> kept out (retired, left)`, the
  kernel's `1 frames left out as bad`, `<f>` in `/proc/ferrix-mce-retired`
  before any program ran, the poll's `left from before this boot` line, and
  the restart's time from the stop to `FERRIX-BOOT-OK` said. **Cold mode**:
  the same image booted again from a powered-off QEMU (the pstore region
  gone, the event log kept): `bad frame <f> kept out (event log)` and `<f>`
  still left out.
* **Negative controls**: the allocator's `Bad` test removed (the stage 2
  check fires); the table's write removed (no `retired` source: the judge
  fails); the loader's bank read removed (the restart's line lacks `left`:
  the judge fails); stop word 1 written instead of 7 (the loader's line
  says a panic: the judge fails).

## 6. Requirements

To be ruled on by the consultant: kernel ids for the stop word 7, the
table's write and the allocator's `Bad` pages (L.x86_64 / L.mm / L.console
ranges, reserved first); the loader's part is outside the item and its
evidence is the gate. Possibly an H id: "a frame the kernel found bad is
kept out of use across a reset and a power cycle", whose environment
includes the loader.

## 7. Where it stands

* Design v1, for the consultant's review.
