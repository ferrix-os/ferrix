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

### 3.1 The region

A **pstore region** of 1 MiB of RAM:

```
  0  magic      "FXPSTORE"
  8  version    u32 = 1
 12  header     u32 = 4096       data starts here
 16  capacity   u64 = len - 4096 bytes of ring
 24  sequence   u64              the boot that writes it (loader: previous + 1)
 32  crc        u32              CRC-32C of bytes 0..32
 36  reserved   u32 = 0
 64  written    u64              bytes ever recorded (the kernel's only header write)
 72..4096  zero
4096 ring       byte n at 4096 + n % capacity
```

A region is **valid** when magic, version, header, capacity (= len - 4096)
and CRC match and `written` is below 2^48. The text is the last
`min(written, capacity)` bytes, in order.

### 3.2 The loader

1. First thing after `Services::new`, before any other allocation: read the
   UEFI variable `FerrixPstore` (Ferrix's vendor GUID, boot-service +
   runtime access, non-volatile): the region's physical address. If set,
   `AllocatePages(AllocateAddress)` at exactly that address, 1 MiB.
2. If that worked and the region is valid: copy its text to
   `FERRIX/LASTKERNEL.TXT` (previous one to `LASTKERNEL.PRV`), headed by one
   line `sequence N, W bytes written, kept K`. Invalid: say so in the boot
   log (`pstore   no valid previous kernel log at …`).
3. If step 1 failed (no variable, or the address is taken this boot):
   allocate 1 MiB anywhere below the direct map's end and set the variable
   to its address -- the only time the variable is written, so an unchanged
   machine writes NV storage once.
4. Write a fresh header (sequence = previous + 1, or 1), zero `written`.
5. Hand the address and length to the kernel in the boot info
   (`pstore_phys`, `pstore_len`, BootInfo version 8) and report the
   allocation as a new memory kind `Pstore` -- RAM (direct-mapped), never
   free, never reclaimed.

Every failure in 1-4 is said in the boot log and the boot goes on with
`pstore_len = 0`. Firmware keeping RAM across a warm reset is not
guaranteed (a cold boot or memory training may clear it): the claim is only
"when the region survived, its text is copied", and the CRC and `written`
bound keep garbage out.

### 3.3 The kernel

* `console/log.rs`: `record` also appends to the region when the boot info
  gave one -- same bytes, same rule (`println_unlogged!`'s layout lines are
  not recorded there either, so KASLR does not leak to the ESP). Lock-free
  as the in-kernel ring: store the byte at `written % capacity`, then
  advance `written` with a compare-exchange; the region is touched only
  through atomics on its direct-map address. `written` is the only header
  field the kernel writes.
* The region's address is taken once at boot from the boot info, checked
  (inside a `Pstore` region of the map, length > 4096, header valid as the
  loader wrote it) before it is used, and otherwise ignored with a line.
* `panic.rs`, before it halts: write the caches back so the bytes are in
  RAM, not lost in a cache a reset discards (`wbinvd` on x86-64; clean by
  VA to PoC over the region on Arm).
* Nothing else changes: no allocation, no lock, no new unsafe beyond the
  one block that turns the direct-map address into a `&[AtomicU8]` and
  `&AtomicU64` (one `// SAFETY:` argued from the checked map entry).

### 3.4 Self-checks and negative controls (proposed)

* Host tests of the header's encode/validate (a new `ferrix-bootinfo`
  module, `pstore`), the text extraction across a wrap, and every refusal.
* The kernel's `log_check` gains a case: a record written to a test region
  reads back the same through the loader's extraction function.
* Gate `cargo xtask test-postmortem` (x86-64, OVMF, KVM, a disk like
  test-ab-boot's): boot 1 a kernel with `ferrix.panic_test` or a panicking
  init; the guest is reset with QMP `system_reset` (RAM kept); boot 2's
  loader writes `LASTKERNEL.TXT` holding `FERRIX-PANIC` from boot 1; the
  host script prints both files from the image.
* Controls (source sabotages, no `cfg`): the panic path's cache write-back
  removed has no effect under QEMU (stated, not claimed); the kernel's
  mirror removed -> `LASTKERNEL.TXT` lacks the panic line; the loader's
  CRC check removed -> a region with a corrupted header is copied (host
  test).

## 4. Where it stands

2026-10-11: this design; PM1 and PM2 in progress on branch `postmortem`;
PM3 waits for the certification consultant's design verdict.
