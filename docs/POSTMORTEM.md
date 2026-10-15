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

> the lowest 2 MiB-aligned address at least 16 MiB above the lowest
> `EfiConventionalMemory` address of the map, below the direct map's end,
> whose whole region lies inside one `EfiConventionalMemory` descriptor at
> the moment the loader looks.

The loader looks before it allocates anything but the RAM-measuring buffer
it already frees (`measure_direct_map`) and the boot info area, right after
the command line is read (§3.3), and takes the region with
`AllocatePages(AllocateAddress)` of memory type `0x8000_0006`
(`FERRIX_PSTORE`, beside the five Ferrix types already in use).

Why chosen: EDK2 allocates top-down, so its boot-time allocations sit at the
top of low RAM and the bottom of RAM above the first 16 MiB stays
conventional from boot to boot of the same machine and firmware; the rule
then lands on the same address every time without anything stored. A
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
reads nor writes the region and hands over `pstore_len = 0`. Any other value
is said and taken as on. The reference configuration has it **on** (the
customer's ask; ITEM.md row and certification-item.json say so).

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
4. Write a fresh header: sequence = previous + 1 (or 1), `written` = 0. The
   ring is not cleared (1 MiB of writes for nothing: only `written` decides
   what is text).
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
  pstore   at 0x1000000, 1 MiB: boot 7's kernel log, 48211 bytes, 48210 kept
  pstore   boot 7's last report: panicked: init exited, as ferrix.onexit=panic asks
```

The second line is the last `FERRIX-PANIC ` line of the text with that
marker left off (so no gate's log reader takes a loader line for a panic of
this boot), or `did not panic; its last line: ...` when there is none.
Each is cut at 160 columns.

Then the whole text, by where the loader was started from:

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
  `screen::draw` (the step most likely to fault): `wbinvd` on x86-64, clean
  by VA to the point of coherency over the region only on AArch64 and
  ARMv7-A (the existing `clean_to_poc`). No lock, no allocation. Bounded:
  `wbinvd` is one instruction whose time is the cache's size; the Arm clean
  is `capacity / line` (16 384 at 64-byte lines) instructions. Stated in
  MEMORY-AND-TIMING beside the panic path. An abridged report (a second
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
  address choice over sample maps (none, an occupied 16 MiB, a map whose
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
* **Controls** (gate.sh control, each must FIRE):
  1. the mirror removed (`log::record` stops calling the region) ->
     `test-postmortem` fails on `boot 1's last report` lacking the panic
     line ("did not panic" or empty) -- the gate's own line;
  2. the attach check removed with a boot info naming a non-Pstore region
     -> the boot check's line (P3);
  3. Pstore made free at boot -> the P4 assertion's line;
  4. the loader's CRC check removed -> the host test of a corrupted header;
  5. the panic write-back is stated not observable under QEMU (its caches
     are the host's, coherent); the hardware run is a BACKLOG row (first
     boot of the SN720 PC, warm reset, the customer's OK for the boot).
* Rows owed: `cargo xtask gate-rows --since main` decides; expected test-boot's
  four arches (every boot now attaches), test-ab-boot (loader), check, and
  the two test-postmortem rows.

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

## 4. Where it stands

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
* **PM3 design version 2** (§3, 2026-10-15, branch `pm3`): for review.
