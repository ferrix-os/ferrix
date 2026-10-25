# AHCI: the driver for a SATA drive

Version 1, 2026-10-16. Written for the N150 box, whose one drive turned out
to be SATA on the chipset's AHCI controller (`8086:54d3`) rather than the
NVMe drive `docs/N150.md` had planned for. It booted the box's root on
2026-10-15, from the unlanded branch `ahci` (4a0db8a97).

**Evidence on `main`: QEMU q35 only.** `test-n150-sata` is a q35 test of the
N150's disk configuration, not the box. The driver's fixes from the box
-- the C-bit fix and BSY wait (§3), the COMRESET of a link firmware left
down -- landed with it (`ahci-land`). The kernel's bring-up the box also
needs (the 2 KiB ABAR mapped as its page, decoding turned on, the
console's and the clock's fallbacks, x2APIC) is on `main` since the n150k landing (ledger 1884),
evidenced on QEMU only and not yet run on the box with checks on (BACKLOG). Outside the certified item, as every driver process is
(`tools/common/data/certification-item.json`).

## 1. What it is

| Part | Where | What |
|---|---|---|
| The logic | `src/lib/drivers/block/ahci` (`ferrix-ahci`) | The host bus adapter as logic over registers, memory and a clock: the BIOS handoff, AHCI enable, finding the drive, IDENTIFY DEVICE, polled reads, writes and flushes, recovery and shutdown. `no_std`, no `unsafe`, host tests against a model HBA (`src/tests/`) |
| The process | `src/user/system/native/drivers/block/ahci` (`ahci`) | The glue: ABAR (BAR 5) through `ferrix-driver`'s PCI bus, pinned memory, the monotonic clock, the console lines. Serves the block ring through `ferrix-driver`'s `block` subsystem, as `nvme` and `blk` do |
| The match | `devmgr`'s PCI table | Class code `01:06:01` (mass storage, SATA, AHCI 1.0), any vendor, drives `ahci` as a disk |
| The gate | `cargo xtask test-n150-sata` | `test-n150` with its disk a SATA drive on q35's AHCI controller (`docs/N150.md` §9) |

The kernel is not changed for it: the disk reaches the block core over a
block ring, so the partition scan, btrfs and everything above them are the
same code as for a virtio or NVMe disk. `ferrix-pci` learned that an AHCI
controller's ABAR is BAR slot 5, sized and mappable whole.

## 2. What it does

1. **Bind.** `devmgr` starts `ahci` on the controller's class code. The
   process maps ABAR and pins two pages of control memory (command list,
   received-FIS area and command table in the first, IDENTIFY's data in
   the second) and the ring's data region (128 pages). The first pin turns
   bus mastering on; the IOMMU holds the controller to those pages.
2. **Handoff and enable.** The BIOS/OS handoff (AHCI 1.3.1 §10.6.3) when
   the HBA has one, waited for at most 2 s; then `GHC.AE` on with
   interrupts off. An HBA with staggered spin-up has every implemented
   port spun up.
3. **Find the drive.** The first implemented port with an ATA disk. A link
   the firmware left down or offline, or a device it left busy, is given a
   COMRESET (`PxSCTL.DET` to 1 for at least 1 ms) first, as Linux does; an
   empty port is not waited for. A port with an ATAPI or other device is
   passed over.
4. **IDENTIFY.** Model, serial, sector count and size from IDENTIFY
   DEVICE's words; LBA48 is required. A refused IDENTIFY says which words
   it was judged on.
5. **Serve.** One command at a time, in slot 0, polled to completion: READ
   DMA EXT, WRITE DMA EXT and FLUSH CACHE EXT. No interrupts (the vector is
   bound, as every PCI driver's is, and never asked for) and no native
   command queuing. A request longer than one command table holds is
   split; segments must be even in address and length, and below 4 GiB
   when the HBA lacks 64-bit addressing. A running command is looked at
   every 20 us for its first 100 ms, then every millisecond; every other
   wait polls every 5 us for a millisecond, then every millisecond.
6. **Read ahead.** Once two reads follow each other on the disk, the
   driver launches a READ DMA EXT of the next 512 KiB into one of two
   pinned windows and answers the read without waiting for it (§5). A
   later read the windows hold is a copy; any other command waits for the
   window first; a write drops a window it lands on.
7. **Recover.** A command that fails or times out (10 s) stops the port's
   command list, clears the errors, overrides a busy task file and starts
   it again (AHCI 1.3.1 §6.2.2.1). It is then issued again, up to three
   times in all, unless the drive said the medium could not take or give
   the sector (`UNC`): libata retries every failure of a read or write but
   a medium error the same way (`ata_eh_worth_retry`). A command that
   timed out has the link COMRESET before its second try and gets no third,
   so a hung drive is answered in about 20 s, inside the 30 s the kernel
   waits for a request before it answers EIO itself; once it has timed out
   after the reset, the drive is taken to have stopped answering and each
   command is issued once until one completes. Only the last attempt's
   failure is answered, and a request that needed another try is said on
   the console, done or not (2026-10-25: on the N150, one failed write
   aborted btrfs's transaction and left `/` read-only). Registers that read
   all ones mean the HBA left the bus.
8. **Shut down.** Wait for a window being read, flush the drive's cache,
   stop the port.

On the console: one line when the function is found, before a register is
written, with the HBA's registers as firmware left them; one when the drive
is up, naming its model, serial and size; one per failed request and one
per request that was issued again, up to 16 together; one at shutdown.

## 3. What the box taught it

Seen on the branch `ahci` before it landed; the fix is on `main` with
the driver (`ahci-land`).

QEMU completes a command at once, which hid one bug the box did not: every
command header set the C bit (clear BSY on R_OK), so a real HBA cleared
`PxCI` as soon as the command FIS was sent, before IDENTIFY's PIO data
landed. The driver read an empty page and refused the drive as having no
LBA48; READ DMA EXT would have come back stale the same way. The C bit is
gone, a command waits for the task file's BSY and DRQ to clear after
`PxCI`, and the model HBA refuses the C bit.

## 4. What it does not do

* Only the first ATA disk on the first controller is served.
* No interrupts, no NCQ, one command in flight: correctness first. A disk
  with a slow seek holds every request behind it, and so does a window
  being read ahead, for at most one 512 KiB command.
* No hot plug, no port multipliers, no ATAPI, no TRIM.
* Register offsets, bits and layouts are from the Serial ATA AHCI 1.3.1
  specification, opcodes and IDENTIFY's words from ATA8-ACS. No Linux or
  QEMU code was taken.

## 5. Throughput (2026-10-23)

On the N150 box `busybox dd` of an uncached 1.6 GB file read 48.9 MB/s at
`bs=1M` and 41.5 MB/s at `bs=64k`, with a game loading beside it, where a
SATA SSD gives 450 to 550. Measured in QEMU (`test-n150-sata` with a
256 MiB file, release build, KVM), by counting the driver's commands and
timing each layer:

* Every command was 128 KiB or less, one or two PRDT entries: the kernel
  fills a file's page cache 128 KiB at a time (`MAX_FILL_RUN`), and each
  fill is one synchronous ring request. Queue depth 1, always.
* Of about 300 us a request, the driver held it 140 and the kernel's ring
  and fill path the rest; the drive was idle for all of the latter.
* A command still running after its first millisecond was polled every
  millisecond: anything slower than that, or held up by a busy processor,
  lost up to a millisecond more.

The two fixes are the driver's, with no kernel line: commands are polled
every 20 us for 100 ms, and sequential reads are read ahead 512 KiB at a
time (§2, steps 5 and 6). `dd` of an uncached 256 MiB file, `bs=1M`, before
and after:

| QEMU | idle | four processors busy |
|---|---|---|
| as QEMU completes (instantly) | 342 / 381 -> 910 / 1500 MB/s | 239 -> 658 / 804 MB/s |
| each command held to a 500 MB/s SSD's time (60 us + 2 ns a byte) | 219 / 263 -> 439 / 444 MB/s | 209 / 213 -> 398 / 410 MB/s |

QEMU's figures are only relative; the box's are owed. The kernel's 128 KiB
fill, depth 1, is still the bound for a reader the driver cannot predict.
