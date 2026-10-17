# Post-mortem: what a failed boot leaves behind

Version 1, written 2026-10-11 by postmortem. The customer's ask, after
installing Ferrix on the WD SN720 of a PC that has never booted it: "add some
minimal script for debugging to ferrix, so that we can have post mortem
analysis". A first bare-metal boot may stop with no picture (no framebuffer
driver yet), and a panic halts. After the reset into Ubuntu, something has to
say what happened.

Three pieces, each useful without the next:

| | What | Where | Item? |
|---|---|---|---|
| PM1 | `ferrix-postmortem.sh`, a host script that reads a Ferrix disk read-only and prints what is there | `tools/common/debug/` | no |
| PM2 | The loader's boot log, `FERRIX/BOOTLOG.TXT` on the ESP | `src/boot/common/uefi` | outside the TOE, A.FIRMWARE |
| PM3 | The kernel's log across a reset (a minimal pstore), copied by the next boot's loader to `FERRIX/LASTKERNEL.TXT` | loader, `ferrix-bootinfo`, kernel console | **yes**: console ring, panic path |

## 1. PM1: the host script

`tools/common/debug/ferrix-postmortem.sh [--image FILE]` on the host (Ubuntu):

* finds the disk by **GPT partition name** `ferrix-esp` and `ferrix-root`
  (`/dev/disk/by-partlabel/`), never by device name; `--image` takes a disk
  image instead (a loop device, read-only, `losetup -r -P`);
* mounts the ESP (`vfat`, `ro`) and the root (`btrfs`, `ro,norecovery`) under
  a fresh directory in `$XDG_RUNTIME_DIR`, through `sudo` or `pkexec`;
* prints `FERRIX/BOOTLOG.TXT`, `BOOTLOG.PRV`, `LASTKERNEL.TXT` and
  `LASTKERNEL.PRV` when present, `CMDLINE.TXT`, the ESP's file list;
* decodes the A/B control record from the partition of type
  `c29aa8c5-…` (both copies, `cargo xtask bootctl-read`, which uses
  `ferrix-slots` on the host, read-only `O_RDONLY`) and each slot header;
* lists the newest files under the root's `/var/log` and prints their tails;
* unmounts and detaches on every exit path (`trap`).

It never opens a device for writing: every `mount` is `ro`, `norecovery`
keeps btrfs from replaying its log tree, the loop device is `-r`, and the
record decoder opens `O_RDONLY`.

## 2. PM2: the loader's boot log

The loader's console (`console.rs`) also appends every line to a static
buffer of 32 KiB (truncated, never wrapped: the first lines say the most).
Before `exit_boot_services` -- or on the error path, before returning to
firmware -- the loader writes it to `FERRIX/BOOTLOG.TXT` on its own volume
through `SimpleFileSystem`, after copying the previous `BOOTLOG.TXT` to
`BOOTLOG.PRV` (FAT 8.3; one previous boot is kept, each file at most
32 KiB). Lines it adds for the log: the firmware vendor and revision, the
memory map summary (descriptors, conventional MiB), and `handing over to the
kernel`. The slot plan, verification, command line and framebuffer lines are
the console's own.

**A logging failure never stops a boot**: every file call's status is
ignored after it is said on the console; a read-only or full volume, a
firmware without FAT write, all boot as before. It writes nothing but these
two files on the loader's own volume (the ESP), and only through firmware's
file system.

## 3. PM3: the kernel's log across a reset

Version 2 of this section, 2026-10-15 (pm3), replacing version 1's. What
changed and why is in §3.8; the consultant's version-1 conditions (P1-P10,
ledger line 744) are kept, each named where it is met.

The shape is Linux's ramoops: a fixed piece of RAM that the kernel writes its
log into as it goes, that the firmware of the next boot leaves alone because
nothing asked it to clear RAM, and that the next boot's loader finds again,
checks, and reports. Nothing is written to storage by the kernel, nothing at
panic time but a cache write-back.

### 3.1 The region

One region of RAM, a 4 KiB header page and a ring of 1 MiB:

```
  0  magic      "FXPSTORE"
  8  version    u32 = 1
 12  header     u32 = 4096       the ring starts here
 16  capacity   u64 = 1 MiB      bytes of ring, a power of two
 24  sequence   u64              the generation: which boot wrote it (previous + 1, or 1)
 32  crc        u32              CRC-32C of bytes 0..32
 36  reserved   u32 = 0
 64  written    u64              bytes ever recorded (the kernel's only header write)
 72..4096  zero
4096 ring       byte n at 4096 + n % capacity
```

The region is `4096 + capacity` bytes, page aligned. It is **valid** when
magic, version, header, capacity (a power of two from 4 KiB to 16 MiB, and
`len - 4096`) and CRC match and `written` is below 2^48. Its text is the last
`min(written, capacity) - 1` bytes, in order (the place being claimed is
always in flight, as in the in-kernel ring), every byte that is not printable
ASCII, tab or newline shown as `?`. The format, the CRC, validation and the
extraction are one host-tested module, `ferrix-bootinfo`'s `pstore`, which
both the loader and the kernel compile.

What the CRC does not cover is the ring. A firmware that wrote into the
region on its way up (§3.4) leaves a valid header over damaged text; the
loader's report says the text is unchecked, and the printable filter keeps
what it prints readable. A sealed text checksum written by the panic path
was considered and left out: it is a second header write and 1 MiB of
reading in the panic path, and it would protect only a panic's record, not a
hang's or a watchdog's.

### 3.2 Where the region is: chosen, not remembered

Version 1 kept the address in a non-volatile UEFI variable. Version 2 drops
the variable and **derives the address from the memory map the same way
every boot**:

> the lowest 2 MiB-aligned address at least 64 MiB above the lowest
> `EfiConventionalMemory` address of the map rounded down to 2 MiB, below
> the direct map's end,
> whose whole region lies inside one `EfiConventionalMemory` descriptor at
> the moment the loader looks.

The loader looks before it allocates anything but the RAM-measuring buffer
it already frees (`measure_direct_map`) and the boot info area, right after
the command line is read (§3.3), and takes the region with
`AllocatePages(AllocateAddress)` of memory type `0x8000_0006`
(`FERRIX_PSTORE`, beside the five Ferrix types already in use).

**Version 2.1 (2026-10-17, panic-reboot):** the rule is first tried with
**1 GiB** in place of 64 MiB (`PREFERRED_ABOVE_LOWEST`), and only a map with
no room there (a 512 MiB board) takes 64 MiB. Linux's EFI stub on a firmware
without an RNG protocol decompresses its kernel at 16 MiB, past 72 MiB for
Ubuntu's 7.0, before `memmap=` is read, so the N150 box's inspection Linux
overwrote a region at 64 MiB; at 1 GiB it keeps it with `nokaslr
memmap=2M$0x40000000`. Still derived from the map alone.

Why chosen: EDK2 allocates top-down, so its boot-time allocations sit at the
top of low RAM and the bottom of RAM above its own early work area stays
conventional from boot to boot of the same machine and firmware; the rule
then lands on the same address every time without anything stored. The
first draft said 16 MiB; OVMF rewrites 8 to 27 MiB on every reset (its
MEMFD and the firmware volumes it decompresses: measured 2026-10-15 by
writing a marker into every MiB of a 512 MiB guest over gdb, resetting,
and reading them back; 27 to 480 MiB survived), so the floor is 64 MiB. A
variable, by contrast, writes a PC's NV store (flash) on the first boot and
every time the address is lost, and U-Boot's UEFI (the DK1) keeps variables
only in a file. If the choice ever moves (a firmware update, a different RAM
fit), the record of the boot before is lost and the new address simply has
no valid header: the loss is one record, said in the log, never garbage
taken for a record.

Kept out of the kernel's free memory: the allocation is in the final memory
map as type `0x8000_0006`, which the loader's map translation turns into a
new `MemKind::Pstore` (value 14, appended; existing values unchanged). It is
RAM (`is_ram`, so the direct map covers it), not free at boot, not
reclaimable; every match on kinds names it (P4).

**Off switch** (P7): `ferrix.pstore=off` on the command line (the ESP's
`CMDLINE.TXT` or the slot's) makes the loader skip §3.3 entirely: it neither
reads nor writes the region and hands over `pstore_len = 0`. No option, or
exactly `ferrix.pstore=on`, is on; any other value is said and taken as
**off** (the consultant's Q4: a misspelt switch keeps no log nobody asked
for). The reference configuration has it **on** (the customer's ask;
ITEM.md row and certification-item.json say so). Every loader line that
leaves the boot without a region ends `no region this boot`.

### 3.3 The loader

After the command line is read (`boot_info_area`), before the kernel is
staged:

1. `ferrix.pstore=off` -> the line `pstore   off (ferrix.pstore=off)`, and
   `pstore_len = 0`.
2. Choose the address (§3.2) from a fresh memory map; none -> one line,
   `pstore_len = 0`. `AllocatePages(AllocateAddress)` it; refused -> one line,
   `pstore_len = 0`.
3. Read the header. Valid -> the previous kernel's record is reported (§3.5);
   otherwise one line saying why (`nothing there` for a wrong magic, which is
   every cold boot; `damaged` for a right magic with a wrong CRC or bound).
4. When the boot disk has an event log (§4) and the record holds a panic
   report: append it there first, durably (§4.3). If that append fails, the
   region is **not** renewed: it stays allocated (so the kernel never uses
   its pages), is not handed over, and the boot goes without a pstore, said
   on the console; the record waits in RAM for the next boot or the
   inspection Linux (the consultant's E2). The same holds when a log may be
   there and cannot be used (`ferrix_eventlog::classify`, fail closed):
   more than one partition of the log's type, a damaged table or entry
   array, a log entry outside the usable blocks or overlapping another, a
   partition too small or a block size the log is not kept on, or a failed
   read of the disk or its table. Only a disk with no GPT signature (or a
   boot volume that is a whole disk) and a table with no partition of the
   log's type count as absent, and keep PM3's behaviour: the record is
   reported, then renewed.
   Then write a fresh header: sequence = previous + 1 (or 1), `written` = 0.
   The ring is not cleared (1 MiB of writes for nothing: only `written`
   decides what is text).
5. Clean the header's cache lines on Arm (the switch turns caches off), and
   hand `pstore_phys`, `pstore_len` to the kernel in the boot info.

Every failure is one line and the boot goes on with `pstore_len = 0`.

### 3.4 What survives a reset, and what does not

* **QEMU**: a reset (`system_reset` from QMP, the guest's own reset through
  the ACPI reset register, PSCI `SYSTEM_RESET`) restarts firmware and keeps
  RAM, under TCG and under KVM. `-no-reboot` turns a reset into QEMU's exit,
  and RAM goes with the process: so every boot that should find a record
  runs without `-no-reboot`, as `--reset` boots already do (`-action
  shutdown=pause`; a power-off then only pauses QEMU).
* **A PC**: a warm reset (`0xCF9 = 0x06`, the keyboard controller, the ACPI
  reset register, a watchdog) keeps DRAM powered and is the case this is
  for. A full reset (`0xCF9 = 0x0E`) or a power cycle drops DRAM refresh;
  memory training on the way up can clear or scramble it. Then nothing
  valid is found and the loader says `nothing there`. The claim is only
  "when the region survived, its text is reported", never "it survives".
* **Firmware writing into it**: a firmware that uses the region's RAM on its
  way up damages the text or the header; §3.1's checks keep a damaged header
  out and the report calls the text unchecked.
* **Caches**: what the kernel wrote may still be in a cache a reset
  discards. The panic path writes the region back (§3.6); a hang or a
  watchdog reset gets no write-back, and loses what was still in a cache.

### 3.5 What the loader does with a record it found

Always, on the console (so into the boot log that PM2 writes and
netboot-pm sends, with no extra plumbing):

```
  pstore   at 0x1000000, 1 MiB: boot 7's kernel log, 48211 bytes, 48211 kept (unchecked)
  pstore   boot 7's last report: panicked: init exited, and ferrix.onexit=panic asks for a panic
```

The second line is the last `FERRIX-PANIC ` line of the text with that
marker left off (so no gate's log reader takes a loader line for a panic of
this boot), or `did not panic; its last line: ...` when there is none.
Each is cut at 160 columns, every byte that is not printable shown as `?`,
and every other `FERRIX-` in it made `FERRIX_` (Q6). The quoted line is
**untrusted text** (L1): whatever the previous boot's programs printed to
the console, or RAM firmware scribbled on.

Then the whole text, by where the loader was started from -- **not yet**:
PM3 lands without either (the consultant's ruling on deviation (c), ledger
line 1238), and each comes to the consultant as its own review before its
landing (P10, line 1202 C3):

* **A disk boot** (ESP or slot): `FERRIX/LASTKERNEL.TXT` on the ESP, the
  previous one moved to `LASTKERNEL.PRV`, headed by the two lines above,
  through PM2's `Services::replace_file`. Owed with PM2 (branch
  `postmortem`), which is not on main: PM3 lands the extraction and the
  console lines, and the file write is one call that lands with or after PM2.
* **A netboot** (iPXE with `REPORT.TXT`): the last 32 KiB of the text sent as
  a second report, name `LASTKERNEL.TXT`, through netboot-pm's generic
  `(name, bytes)` sender (its 64 KiB per-report limit). Owed with
  netboot-pm (branch `netboot-pm`, a456c7706), not on main; the consultant's
  line 1202 C3 makes it PM3's review, not a free reuse.

PM1's host script prints `LASTKERNEL.TXT` already.

### 3.6 The kernel

* **One ring algorithm** (P2): `console/log.rs`'s `Ring` keeps its storage
  and count, and its `record_byte` becomes a function over a slice of
  `AtomicU8` (power-of-two length) and an `AtomicU64` count. The static log
  and the region both go through it, so they cannot diverge; host tests of
  the shared code cover a wrap and concurrent writers.
* **The mirror**: `log::record` also records into the region when one is
  attached -- the same bytes, so `println_unlogged!`'s layout lines (KASLR
  slide, trace addresses) are not there either. Lock-free, no allocation.
* **Attaching** (P3, fail closed): once, early in boot, single-processor and
  before interrupts are on. The boot info's `pstore_phys/len` must lie wholly
  inside one `MemKind::Pstore` entry of the map, `len - 4096` a power of two
  from 4 KiB to 16 MiB, page aligned, inside the direct map, and the header
  must validate with `written = 0` (as the loader left it). Anything else ->
  one line (`pstore   not attached: ...`) and no mirror; nothing is written
  before the check. On attach the in-kernel log so far (everything from the
  first line, under 128 KiB at that point) is copied in, so the region holds
  the boot from its start, then the region is published to `record`. With
  `pstore_len = 0`: `pstore   none (the loader gave no region)`.
* **The panic write-back** (P5): in `conclude`, after `smp::stop_others`, the
  backtrace and the explanation, after `console::drain` and before
  `screen::draw` (the step most likely to fault): clean by address over the
  region alone on every architecture -- `clflush` on x86-64 (the IOMMU's
  `clean_for_walker`; version 2's first draft had `wbinvd`, which takes no
  address and costs every cache's size), `clean_to_poc` on AArch64 and
  ARMv7-A. No lock, no allocation. Bounded: `len / line` instructions,
  16 448 at 64-byte lines, 32 896 at the Cortex-A7's 32. Stated in
  MEMORY-AND-TIMING §2.2o. Then one unlogged line, `pstore    written back
  for the next boot`, and a drain: the point from which a reset keeps the
  whole report (Q7). The same write-back runs before `ferrix.onexit=reset`'s
  reset. An abridged report (a second
  processor) writes nothing back: the first report does, last.
  If `ferrix.panic=reset` (bm-nvme, AB-BOOT §6.4) lands, its wait and reset
  come after this write-back (R12); whichever lands second places it.
* **Never free** (P4): the region is `MemKind::Pstore`; `is_free_at_boot`
  and `is_reclaimable` match kinds positively, and the boot check asserts no
  frame of the region is on a free list after the reclaim of boot memory.

### 3.7 Requirements, checks, the gate and its controls

Requirement ids (P6), reserved on main in a docs-only landing before the
code: `L.console.42` (the mirror records exactly what the log records,
layout lines excluded), `L.console.43` (attaching fails closed, P3),
`L.console.44` (the panic write-back, P5), `L.mm.75` (Pstore is never free,
P4).

* **Host tests**: `ferrix-bootinfo::pstore` -- header encode/validate and
  every refusal (each field, CRC, bound, version), extraction across a wrap
  and with `written` below and above capacity, the printable filter, the
  address choice over sample maps (none, an occupied 64 MiB, an OVMF-shaped map, a map whose
  lowest RAM is at 1 GiB as on Arm); the shared ring code (a wrap,
  concurrent writers); BootInfo v8 refusing v7 and v9 (P8).
* **Boot checks**: `log_check` gains a region case (a ring written through
  the shared code reads back through `pstore`'s extraction); the P4
  assertion, said on every boot that attached.
* **Gate `cargo xtask test-postmortem`** (P9), x86-64 under KVM and AArch64:
  one QEMU, no `-no-reboot`, a QMP port, the image's command line
  `ferrix.onexit=panic`. Boot 1 runs to init's exit and panics on purpose
  (FX-1501); the gate waits for the report's `stopped` line, then QMP
  `system_reset`; it requires the loader banner a second time, the
  loader's `pstore   at ...: boot 1's kernel log` line and `boot 1's last
  report: panicked: <boot 1's panic message, exactly>`, and boot 2's kernel
  attaching boot 2. A third check: boot 1's kernel attached (`pstore
  mirroring`), and its loader said `nothing there` (cold).
* **Controls** (Q15; gate.sh control, each must FIRE; the tags and
  verdicts are in `~/.local/share/ferrix/handover/pm3.md`):
  1. the mirror removed (`console::pstore::record` records nothing) ->
     test-postmortem: `missing from the region's end`;
  2. placement's Pstore clause removed -> the boot's placement check:
     `took a region inside the kernel image's entry`;
  3. `MemKind::Pstore` free at boot -> the never-free check's line;
  4. the header's CRC check removed -> host test `every_field_is_checked`
     (host tests print no guest line, so gate.sh cannot judge them: run
     with a private target, the output kept in the handover);
  5. unlogged lines mirrored -> test-postmortem: `holds an unlogged line`;
  6. `ferrix.pstore=off` ignored by the loader -> test-postmortem's switch,
     caught by off_problem's first check, the loader's line: `the loader did
     not say \`pstore   off (ferrix.pstore=off)` (pm3-ctl-off2; pm3-ctl-off
     shows the sabotage took effect, its --expect-line named the later arm;
     ledger line 1335);
  7. `0x8000_0006` translated as Usable -> stage 1: `does not describe the
     pstore region as Pstore`;
  8. the quotation's `FERRIX-` defusing removed -> host test
     `quoted_markers_are_defused` (as 4);
  9. the copy of the log so far at attach removed -> test-postmortem: `does
     not start with the kernel's first lines`;
  10. the write-back is stated not observable under QEMU (its caches are
     the host's); the hardware run is a BACKLOG row.
* Rows owed (Q14): check; build --arch all --release; test-boot x86_64 kvm
  and tcg, aarch64, armv7a, armv7a --smp 2; test-boot --reset x86_64 and
  aarch64; test-ab-boot; test-postmortem x86_64 kvm and aarch64.

### 3.8 What changed from version 1

* The NV variable is gone (§3.2): fewer firmware writes, works on U-Boot,
  and P1's restatement loses the variable half. Off switch is the command
  line option only.
* The ring is a power of two (header page + 1 MiB) so the shared code keeps
  its mask.
* The kernel copies its log so far into the region on attach.
* The loader reports a found record on the console first, so it reaches
  the serial port, BOOTLOG.TXT (PM2) and the netboot server (netboot-pm)
  even before the LASTKERNEL file and the netboot send are wired.
* The test uses `ferrix.onexit=panic` (an existing deliberate panic) and a
  QMP reset, since no kernel resets on a panic yet.

## 4. PM4: the event log, across a power cycle

Version 1, 2026-10-17 (n150), the design of
`~/.local/share/ferrix/n150-linux/panic-log-design.md` option E; the
consultant's verdict **OK IF (E1)-(E12)**, ledger line 2153. The customer
chose the partition's place on the N150 box: the gap before its ESP.

PM3's record lives in RAM and is lost to a power cycle. PM4 keeps a
panicked boot's record on disk, written by **the next boot's loader**, on a
machine that is not failing, through firmware's own Block I/O -- never by
the panicked kernel, which writes nothing but RAM. With `ferrix.panic=reset`
(H.FAIL.5) the machine resets itself, RAM survives the warm reset, and the
loader that comes up 10 to 20 seconds later appends the record before it
gives the region a fresh header. When the boot manager starts the
inspection Linux instead, its init appends the same record with
`ferrix-eventlog append`, from `/dev/mem`.

### 4.1 The partition and its format

A GPT partition of type **Ferrix event log**,
`0a0323e0-d79d-498d-a4d6-dbea6af3bab0`, name `ferrix-events`. On the N150
box: LBA 40-2047, 2008 sectors in the gap before the ESP (LBA 2048). The
format is `ferrix-eventlog` (`src/lib/proto/eventlog`: `no_std`,
`#![forbid(unsafe_code)]`, host-tested), which the loader, the Linux tool
and xtask share:

```
  0       header copy A   (4 KiB; 64 bytes used)
  4096    header copy B
  8192    record 0        (32 KiB each)
  ...     record n-1      n = (bytes - 8192) / 32 KiB, at most 4096: 31 on the box

header:  0 "FXEVENTS"  8 version 1  12 record length  16 records  20 next slot
         24 generation  32 appended (the next record's index)  40 CRC-32C of 0..40
record:  0 "FXEVTREC"  8 version 1  12 kind (1 = panic)  13 flags (truncated, unchecked)
         16 index  24 count  28 text length  32 first boot  40 last boot
         48 identity  56 key (CRC-32C of the panic line)  64 first time  72 last time
         92 CRC-32C of 0..92 and the text   96 the text
```

The header's current copy is the valid one, or of two the higher
generation. A record belongs to the log when it is valid and its index is
below the header's `appended`; records are ordered by index, the log's own
count, not by the pstore's boot sequence (it starts again at 1 after every
cold boot) nor by firmware's clock (informative only, Unix seconds) (E7).

**The text** (E5, C2): the report first -- from the last `FERRIX-PANIC`
line to the end of the kernel log -- then `-- the log before the report --`
and as much of the log before it as fits, from a line's start; every byte
as `pstore::shown` prints it and every `FERRIX-` made `FERRIX_`. A record
holds at most 32 672 bytes of the 1 MiB ring: the report and the log's
tail, never the whole ring (flag `truncated` says it was cut). Every record
carries `unchecked`: the pstore ring has no checksum, so firmware may have
written into it (§3.1). The pstore header and ring are parsed as untrusted:
`pstore::validate` clamps the count to the ring and every place is bounded
by the text's and the buffer's lengths; a host test feeds 300 hostile
regions.

**Kinds** (E6): this version logs one kind, a previous boot's record that
holds a panic report. A record without one (a clean reboot, or a hang a
warm reset ended) and no record at all (a cold boot, a power cut, the
first boot after an install, after a Linux boot or after
`ferrix.pstore=off`) log nothing: telling them apart needs the kernel's
clean-shutdown mark, an item change with its own id and control, which is
left for its own review. Nothing is ever logged as "power loss" or "hang".

### 4.2 What writes it, and nothing else (E1)

* **EL1** The loader writes only blocks of the boot disk's one partition of
  the event log's type, through that disk's Block I/O. `classify` decides
  (host test `a_present_but_unusable_log_is_never_taken_for_absent`): no
  GPT signature or no partition of the type -> absent, no write, nothing
  said; more than one, a damaged table, an entry outside the usable blocks
  or overlapping another, a partition too small, a failed read -> unusable:
  no write, said, and the pstore record kept in RAM (§3.3, 4).
* **EL2** Every block address it reads or writes comes from
  `Geometry::lba`, which refuses an offset that is not a whole block or a
  block not wholly inside the partition (host test
  `no_address_outside_the_partition`, which also appends 40 records and
  counts 0 writes outside LBA 40-2047).
* **EL3** It writes only on a boot whose pstore region holds a valid record
  with a panic report (and, on that boot, a fresh header if the partition
  has none).
* **EL4** The inspection Linux's `ferrix-eventlog append` writes by the same
  functions to the partition device named `ferrix-events`, and never writes
  the pstore region.

### 4.3 The append (E2, E3, E4, E8)

`ferrix_eventlog::append`, in this order: read the header (two copies; a
fresh header, both copies, when neither is valid); read the newest record
through it. If its identity is this pstore record's (the boot sequence and
the CRC-32C of the whole pstore header, `written` among it), nothing is
written (E3): a reset during the header writes, the loader after Linux or
Linux after the loader never duplicate. If it is the same panic (kind and
key), its count goes up by one and it is rewritten in place, then flushed:
a reset loop is one record with a count, and the first record of the run
is not pushed out by its copies (E8). Otherwise the record goes to the
header's next slot, flushed, then header copy A, flushed, then copy B,
flushed; the ring overwrites its oldest. `Ok` only when all of it is
durable, and only then does the loader renew the pstore header (§3.3, 4).

Torn writes (E4; the box's SATA disk has a write cache and no FUA): a new
record is invisible until a header copy counts it, so a stop at any block
leaves the previous newest; a torn header copy falls back to the other; a
torn in-place collapse leaves a record that fails its CRC, and the log's
newest valid record is the one before -- never a record that is not whole.
Host test `a_torn_append_leaves_the_newest_or_the_previous_record` stops
the device after every number of block writes of an append and of a
collapse.

### 4.4 Readers

The loader says on every boot with a log: `events   N records; newest
#I: boot B panicked, C time(s): FERRIX_PANIC ...`, and `events   boot B's
panic kept as record #I (slot S)` when it appends. `ferrix-eventlog list
DEVICE [--text]` (a static Linux binary, also in the inspection image)
prints every record, oldest first, with the whole text on `--text`. Root
only: it reads the raw partition (VULNERABILITY-ANALYSIS V-15).

### 4.5 The gate and its controls (E12)

`cargo xtask test-postmortem` (x86-64): the image's ESP in a GUID partition
table with the log at LBA 40-2047, as on the box. Boot 1 panics and resets
by itself (`ferrix.panic=reset`); boot 2's loader says it kept the record;
the QEMU process is ended (the power cut); a second process boots the disk
cold: boot 3's loader finds no pstore and quotes boot 1's panic from the
log, and the host reads the record back with `ferrix-eventlog`, its text
starting with boot 1's panic line. Host tests in `ferrix-eventlog`: the
formats and every refused field, the CRC across head and text, the text's
composition and truncation, 300 hostile regions, idempotence, the collapse,
the ring's wrap, torn appends, torn header copies, and the range check.
Controls (`fleet/gate.sh control`, each FIRED): `ev-ctl-order`, the append
moved after the pstore header's renewal (the gate: boot 2 keeps nothing);
and with `--expect-host` on `check`: `ev2-ctl-crc`, the record's CRC check
removed (`a_record_round_trips_and_a_changed_byte_is_refused`);
`ev2-ctl-range`, the range check removed
(`no_address_outside_the_partition`); `ev2-ctl-identity`, the identity check
removed (`the_same_pstore_record_is_appended_once`); `ev2-ctl-classify`, a
table with two logs answered as absent
(`a_present_but_unusable_log_is_never_taken_for_absent`).

## 5. Where it stands

2026-10-17, PM4: code verdict **OK IF (1)-(6)** (consultant ledger line
2195), after the design's OK IF (E1)-(E12) (line 2153). E1's deviation,
accepted: the loader writes through the boot disk's whole-disk Block I/O,
every address bounded by `Geometry::lba`, not the partition's own handle,
as AB-BOOT's control record does.

2026-10-11:

* **Design verdict: OK IF P1-P10** (consultant ledger
  `~/.local/share/ferrix/cert-consultant/reviews.md` line 744). PM1 outside
  the item; PM2 outside the TOE (no code review, but P1's doc restatements:
  A.FIRMWARE, the A/B AoU (AoU-29, renumbered from a second AoU-28 on 2026-10-12) and
  line 727's "writes nothing but one control block" must name the
  BOOTLOG/LASTKERNEL files and the NV variable, with PM2's commit;
  test-ab-boot's p1-hash rule must hold with logging on). PM3 is CORE:
  one ring algorithm shared with `console/log.rs` (P2), fail-closed region
  check (P3), Pstore never free with no wildcard matches and a boot check
  (P4), panic write-back after other CPUs stop, region-only clean on Arm,
  bound in MEMORY-AND-TIMING (P5), requirement ids reserved first plus
  four controls (P6), a VA row and an off switch by boot option or absent
  variable, ITEM.md row (P7), BootInfo v8 refused on mismatch (P8),
  test-postmortem with a QMP reset on x86-64 and AArch64, BACKLOG row for
  the first SN720 PC run (P9), code review before land.sh take (P10).
* **PM1 landed-ready** (this commit): `tools/common/debug/ferrix-postmortem.sh`
  and `ferrix-slots-decode.py`. The decoder is Python, not `ferrix-slots`,
  so the customer's host needs no build; checked by hand against a record
  built to §3 (CRC-32C check value, a failed-trial record). Not yet run
  against a loop-mounted image (needs root); `shellcheck` is not installed.
  Owed: a host test that decodes records and headers `ferrix-slots` wrote.
* **PM2 in progress**, uncommitted / WIP on branch `postmortem`:
  `bootlog.rs` (console tee into a 32 KiB buffer, `save` writes
  BOOTLOG.TXT and moves the old one to BOOTLOG.PRV), `Services::replace_file`
  and `firmware_vendor`, FileProtocol `write`/`delete` bindings. Still to do:
  call `bootlog::firmware` after `Services::new`, a memory-map summary line,
  `handing over to the kernel` then `bootlog::save` before
  `console::shutdown()`, `save` on `efi_main`'s error path (a fresh
  `Services`), build, test-ab-boot with BOOTLOG checks, P1's doc edits.
* **2026-10-15, branch `netboot-pm`:** the console tee landed there, as
  `bootlog.rs` (the 32 KiB buffer, the `handing over to the kernel` line),
  for a netbooted loader to send its log to `cargo xtask netboot`
  (`docs/NETBOOT.md` §6; consultant ledger 2026-10-15, netboot-pm). The ESP
  write (`save`, `replace_file`, the FileProtocol write bindings) is still
  PM2's, on branch `postmortem`, and still owes P1's doc restatements.
* **PM3** (§3, version 2, 2026-10-15, branch `pm3`): design verdict
  **OK IF (Q1)-(Q16)**, ledger line 1238. Ids `L.console.42-44` and
  `L.mm.75` reserved on branch `pm3-reserve` (docs only, to land first).
  Code on `pm3`: the format and its host tests (`ferrix-bootinfo::pstore`),
  BootInfo v8 and `MemKind::Pstore`, the loader's region and report, the
  kernel's mirror, attach, placement check, never-free check, panic and
  reset write-back, the audit record `Config::Pstore`, `cargo xtask
  test-postmortem`, and `test-boot --reset`'s record check (Q8). Rows and
  controls: `~/.local/share/ferrix/handover/pm3.md`.
