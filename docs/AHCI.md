# AHCI: the driver for a SATA drive

Version 1, 2026-10-16. Written for the N150 box, whose one drive turned out
to be SATA on the chipset's AHCI controller (`8086:54d3`) rather than the
NVMe drive `docs/N150.md` had planned for. It booted the box's root on
2026-10-15. Outside the certified item, as every driver process is
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
   when the HBA lacks 64-bit addressing.
6. **Recover.** A command that fails or times out (10 s) stops the port's
   command list, clears the errors, overrides a busy task file and starts
   it again (AHCI 1.3.1 §6.2.2.1); the request is answered with the error.
   Registers that read all ones mean the HBA left the bus.
7. **Shut down.** Flush the drive's cache, stop the port.

On the console: one line when the function is found, before a register is
written, with the HBA's registers as firmware left them; one when the drive
is up, naming its model, serial and size; one per failed request, up to
16; one at shutdown.

## 3. What the box taught it

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
  with a slow seek holds every request behind it.
* No hot plug, no port multipliers, no ATAPI, no TRIM.
* Register offsets, bits and layouts are from the Serial ATA AHCI 1.3.1
  specification, opcodes and IDENTIFY's words from ATA8-ACS. No Linux or
  QEMU code was taken.
