# NVMe: the driver for a real SSD

Version 1, 2026-10-10. `docs/INSTALLER.md` §6 plans this driver as slice H1:
*NVMe, behind `blkring`, started by `devmgr` on the PCI class it matches,
gated under QEMU before it meets the reference PC.* This document is what
was built for it, what it decided, where it departs from that plan, what
only QEMU's emulated controller has shown so far, and the checklist for the
first run on a real drive.

**Nothing here has run on a real drive.** Every claim below that says
"shown" was shown on QEMU 10.2.1's `nvme` device; §8 lists what that cannot
show.

## 1. What it is

| Part | Where | What |
|---|---|---|
| The logic | `src/lib/drivers/block/nvme` (`ferrix-nvme`) | The controller as logic over registers and memory: reset and enable, Identify, queues, PRP lists, read, write and flush, the Host Memory Buffer, timeouts, shutdown, quirks. `no_std`, no `unsafe`, 40 host tests against a model of a controller |
| The process | `src/user/system/native/drivers/block/nvme` (`nvme`) | The glue: the BAR, pinned memory, a clock, host memory, the console lines. Serves the block ring through `ferrix-driver`'s `block` subsystem, exactly as `blk` does |
| The bus | `src/user/system/native/driver/src/pci.rs` | A PCI function that is not virtio, for any driver: BARs by `device_aperture`, vector 0, pinned memory, runs of contiguous memory |
| The match | `devmgr`'s `PCI_DRIVERS` | Class code `01:08:02`, any vendor, drives `nvme` as a disk |
| The gates | `cargo xtask test-nvme`, `cargo xtask test-restart --boot nvme` | §7 |

The kernel is not changed. An NVMe disk reaches it as a virtio disk does:
HELLO on a block ring, so the block core, the partition scan, btrfs and
everything above them are the same code. The driver, `devmgr` and
`ferrix-driver` are outside the certified item
(`tools/common/data/certification-item.json`).

## 2. What the driver does, in order

1. **Bind.** `devmgr` starts `nvme` on every function whose class code is
   `01:08:02`. The process checks the class again itself, refuses a
   controller on its not-driven list (§6), maps the aperture that starts
   BAR 0, and pins its queues, its PRP lists and the ring's data region.
   The first pin is what turns bus mastering on (`docs/DEVMGR.md`).
2. **Reset.** Whatever firmware or a dead driver left, `CC.EN` is cleared
   and `CSTS.RDY` waited for, for as long as `CAP.TO` says. Only then are
   `AQA`, `ASQ` and `ACQ` written.
3. **Enable.** `CC` = NVM command set, 4 KiB pages (`MPS` 0), 64-byte
   submission and 16-byte completion entries, `EN`. A controller that
   cannot run 4 KiB pages (`CAP.MPSMIN` > 0) or lacks the NVM command set
   is refused.
4. **Identify Controller.** Entry sizes are checked against `SQES`/`CQES`.
5. **Queues.** Set Features 07h asks for one I/O queue pair; the count
   granted bounds the count used.
6. **Host memory** (§4), **write cache** (§5), **power** (§5).
7. **Namespace.** The first active one (Identify CNS 02h, or namespace 1
   on a controller older than 1.1 or with the Identify quirk). Logical
   blocks of 512 to 4096 bytes are served; a format with metadata or
   end-to-end protection is refused.
8. **I/O queues.** Each completion queue, then its submission queue, 64
   entries, each one page, interrupting through vector 0.
9. **HELLO**, with the namespace's block size, its block count, the most
   blocks a request may carry (the least of `MDTS`, 128 KiB, and what one
   PRP list page names), and the flags: `FUA` always, `FLUSH` if the
   controller has a volatile write cache, `READ_ONLY` if the namespace is
   write protected.

One line on the console says all of it (§9).

### Requests

A ring request becomes one command. Data is not copied: the PRP entries
name the pages of the ring's data region, which the kernel fills and reads.
A transfer within one page uses PRP1; into a second page, PRP2; beyond
that, PRP2 names a list in a page that belongs to the command's slot. SGLs
are not used.

A command's id is its slot and the slot's generation, so the completion of
a command that was aborted or sent again cannot be taken for the one waited
for. A completion entry is read status word first, in one access: its phase
bit says the entry is written.

The interrupt is MSI-X entry 0 (or the function's one MSI message), bound
to the port the ring's bell arrives on, so the serve loop's sleep handshake
(`ferrix-blkserve`) covers both; the driver never sleeps without it.

### When a command does not come back

Every 100 ms while requests are in flight and nothing was heard
(`ferrix-driver`'s watch):

* registers read all ones: the drive left the bus; the driver exits;
* `CSTS.CFS`: the controller is reset, set up again as it was (host memory
  returned with the memory-return bit), and every request in flight sent
  again;
* a completion that has waited 100 ms with no interrupt: taken, and said;
* a command older than 30 s: Abort, within the controller's abort limit;
  still missing 30 s later: reset, as above.

A request is sent again at most three times, after a reset or after a
completion the controller did not mark do-not-retry; then it fails upward
as an I/O error. Three resets in a row with no command succeeding between
them end the driver. When the driver ends for any reason `devmgr` quiesces
the device and starts another, and the kernel's ring parks the disk and
keeps its requests for the next driver (`docs/BLOCK-RING.md`): a mounted
btrfs root survives.

### Shutdown

When the ring ends in order (STOP), or the serve loop stops on a fault, the
driver takes host memory back (Set Features 0Dh with enable clear), writes
`CC.SHN` = 01b and waits for `CSTS.SHST` = 10b for as long as `RTD3E` says
(5 s at least, 30 s at most), then clears `CC.EN` and waits for `CSTS.RDY`
to fall. Only a controller seen disabled has its memory freed; one that
will not disable keeps it pinned and the process exits `Wedged`.

**What does not happen yet: the kernel never sends STOP.** At power-off the
kernel commits every mounted volume, which ends in a Flush command the
drive completes, and then powers off; no driver is told. So today a drive
is never cut with acknowledged data unwritten, but it is cut without the
shutdown notification: it counts an unsafe shutdown in its SMART log, and
may take longer to come ready next time. The same is true of a driver that
is killed: nothing runs in a killed process. Sending STOP to every block
ring from `power::finish` and `reboot`, and waiting for STOPPED, is a kernel
change that waits on the customer's word (`docs/roadmap/stage-10-userspace-drivers.md`,
*Still to do*: no driver's stop path is reachable today); the driver's half
is built and tested against the model. The customer's word came on
2026-10-13 with the native boot of the PC (stage 21); §2.1 is the design.

### 2.1 STOP at power-off and reset (design, for review)

Written 2026-10-13 by bm-nvme. A drive must be told before its power goes:
a real SN720 counts every untold power-off in its SMART log
(`unsafe_shutdowns`) and may take longer to come ready, and a drive that
is mid-way through folding its cache is the case the notification exists
for.

**The order at the end, with this change.** `power::finish` (init exited:
power-off or `ferrix.onexit=reset`) and `reboot(2)`'s `POWER_OFF`, `HALT`,
`RESTART` and `RESTART2`:

1. `sync_disks()`: every registered flush committed, as today. Each commit
   ends in the ring's flush, which the NVMe driver makes a Flush command.
2. **new:** `stop_devices()`: every registered *stop* hook, in registration
   order, each given the same deadline, now + 35 s.
3. the audit record, the console line, `arch::shutdown()` or `arch::reset()`,
   as today.

`ferrix.onexit=panic` sends no STOP: its panic halts (or, under
`docs/AB-BOOT.md` §6.4, resets), and nothing after a panic waits on ring 3.

**The hook.** `power` names nothing above the item, as with flushes. It
gains a second list, `Hooks<Stop, 4>`: `Stop { what: &'static str, stop:
fn(deadline_nanos: u64) -> Stopped }`, `Stopped { asked, stopped }`,
registered at bring-up from `main.rs`'s `register_load` next to the
flushes. The block ring registers the only one. `power` prints one line per
hook: `  power    disks: 2 told to stop, 2 stopped` or `... 1 did not answer
within 35 s`, and powers off either way: a driver that does not answer
never keeps the machine running.

**What the block ring does** (`interfaces/block_ring`, load ring):

1. sets `STOPPING`, after which `BLOCK_RING_CREATE` answers `BAD_STATE`, so a
   driver `devmgr` starts again (block drivers are `Restart=always`) gets no
   ring and touches no device;
2. marks every live ring's task *asked to stop* and nudges it, through the
   same port packet a queued read nudges it with;
3. each task, at the top of its loop, sends STOP (type 4) once on its
   control channel, then keeps serving: the driver completes or fails what
   it holds (BLOCK-RING.md §6.3), resets the device -- for `nvme`, host memory
   back, `CC.SHN` = 01b, `CSTS.SHST` = 10b waited for, `CC.EN` cleared -- and
   answers STOPPED; the ring ends `Ending::Stopped` as it does today, its
   node is unpublished and anything still outstanding fails `EIO`;
4. `stop` waits on the ring's existing `SERVED` queue until every ring that
   was asked has ended (STOPPED counts as stopped; a driver that died
   instead does not, since no reset is promised) or the deadline passes,
   and returns the counts.

A parked disk (its driver died earlier) has no driver to tell and is not
counted. Nothing in the item waits without the deadline.

**The deadline: 35 s.** The driver waits for `CSTS.SHST` for `RTD3E`,
clamped to 5..30 s (Linux clamps its `shutdown_timeout` to at most 60 s,
default 5 s, `drivers/nvme/host/core.c`), then for `CSTS.RDY` to fall; the
kernel gives the slowest drive the driver allows its whole shutdown wait
plus 5 s. QEMU's controller completes the shutdown at once; a virtio disk's
driver resets its device in microseconds.

**The constants**, checked on 2026-10-13 against Linux's
`include/linux/nvme.h` on the gate host
(`/usr/src/linux-headers-7.0.0-29`, lines 230-234 and 253-260) and the
NVMe Base Specification: `CC.SHN` bits 15:14, `NVME_CC_SHN_NORMAL` =
1 << 14 (`ferrix-nvme`'s `regs::CC_SHN_NORMAL`, the same); `CSTS.SHST` bits
3:2, `NVME_CSTS_SHST_CMPLT` = 2 << 2 (`regs::CSTS_SHST_COMPLETE`, the
same); `RTD3E` at Identify Controller bytes 88..92, in microseconds
(`identify.rs`, the same). They are outside the item; the kernel uses
none of them.

**What it changes, and where.**

| Where | Ring | Change |
|---|---|---|
| `src/kernel/src/power.rs` | item | `Stop`, `register_stop`, `stop_devices`, called from `finish` (off, reset) |
| `src/kernel/src/syscall/system.rs` | load | `stop_devices()` after `sync_disks()` in `reboot(2)` |
| `src/kernel/src/main.rs` | item | registers the block ring's stop with the flushes |
| `src/kernel/src/interfaces/block_ring/mod.rs` | load | `STOPPING`, the asked-to-stop flag, STOP sent from the task, `stop_all(deadline)` |
| `docs/BLOCK-RING.md` §6.3 | | the kernel sends STOP at power-off and reset |
| `docs/sysml`, `TRACEABILITY.md` | | a requirement for the power side's order and bound (flushes, then stops, then off; never past the deadline), ids reserved first, as the consultant rules |

**The gate.** `cargo xtask test-nvme` (KVM, x86-64) at its end powers off
as it does today and now also requires: the kernel's `power    disks: N
told to stop, N stopped` with N counting both NVMe drives and the virtio
disks; each `nvme` driver's `the controller said its shutdown is complete`;
and QEMU's own trace showing the shutdown notification set
(`pci_nvme_mmio_shutdown_set`) for each controller. `test-boot` on all four
rows and `test-restart --boot nvme` stay green (their disks are stopped
too; the boot lines gain the one `power` line).

**Negative controls:** `stop_devices()` removed from `finish`: test-nvme
fails on the missing `power    disks:` line and the missing QEMU trace
event; the STOP send removed from the ring's task: it fails on `0 stopped`;
the driver's `CC.SHN` write removed (`ferrix-nvme`): it fails on the
missing trace event while the kernel still counts the ring stopped, which
shows the trace check is what proves the drive was told.

## 3. Decisions, and where they depart from `docs/INSTALLER.md`

| | INSTALLER.md | Built | Why |
|---|---|---|---|
| Driver model | ring 3, behind `blkring`, started by `devmgr` on the PCI class | the same | |
| Gate | `test-install --usb-live`, target on `nvme`, after H4 | `test-nvme` and `test-restart --boot nvme` now; the installer's gate still waits for H3 and H4 | The driver can be shown without a USB stick to boot from |
| Names | `/dev/nvme0n1`, `/dev/nvme0n1p1`, Linux's numbers (§5.2) | `/dev/vdX`, in PCI order with the virtio disks | A ring disk's name is checked by the kernel's block ring and `ferrix-blkring` as `vd` and letters, with virtio-blk's major. Changing that is a change to the kernel's interface and the certified boot checks that read `vda`; it is not needed to boot, since `/` is found by label on any disk. A `docs/BACKLOG.md` row |
| I/O queues | not said | one pair | One process serves one ring from one thread; a queue per processor has nobody to submit on it. The logic takes up to four pairs and spreads requests across them (tested against the model with two); the process asks for one. Worth revisiting only with a serve loop per processor |
| TRIM | not said | none | The block ring has no discard operation. Dataset Management is never sent |
| Namespaces | not said | the first active one per controller | A ring serves one disk per device. Consumer drives have one namespace |

## 4. The Host Memory Buffer

A drive without DRAM keeps its logical-to-physical map in flash and a small
internal cache. With a host memory buffer it caches the map in the host's
RAM instead. Identify says how much it would like (`HMPRE`) and the least
it can use (`HMMIN`), in 4 KiB units, and may bound the pieces (`HMMINDS`,
`HMMAXD`).

* The driver lends up to 128 MiB (Linux's default bound), `HMPRE` if that
  is less, in runs of contiguous device addresses of 4 MiB (the kernel's
  largest contiguous pin), halving the run size while memory is too
  fragmented, down to 256 KiB or `HMMINDS`. At most 32 runs.
* The runs are pinned through the device's IOMMU domain like everything
  else and never mapped into the driver: the controller alone reads and
  writes them. They count against the device's pin budget (260 MiB by
  default).
* Less than `HMMIN`, a `HMMIN` above 128 MiB, or a controller that refuses
  the feature: **no buffer, and the drive is used without.** The
  specification requires a controller to work that way; it is slower,
  mostly on random reads and writes over a large span, and it wears no
  faster. The console line says `host memory NOT in use`.
* Host memory is disabled before shutdown and before its memory is freed,
  and given back after a controller reset with the memory-return bit.

What a DRAM-less drive needs from this driver that a drive with DRAM does
not: this section, and nothing else. QEMU's controller reports `HMPRE` 0,
so **all of it is shown against the model only** (§8).

## 5. Write cache, flush, force unit access, power

* **Volatile write cache.** If Identify says one is present (`VWC` bit 0),
  it is switched on (Set Features 06h, not saved) and the ring is told
  `FLUSH`: the kernel's flush is a Flush command, and a write with force
  unit access carries `FUA`. If none is present a flush completes at once
  with no command, since every completed write is already durable.
* **Power.** Autonomous power state transitions are switched off on every
  controller that has them (Set Features 0Ch with enable clear, not
  saved); the power state itself is never set. The drive stays in the
  state it powers up in, PS0. That costs idle power and removes the class
  of failures Linux's `NO_APST` and `NO_DEEPEST_PS` quirks exist for.
  Enabling APST with a latency bound is a later, measured decision.

## 6. Quirks

Facts from Linux 6.18's `drivers/nvme/host/pci.c` and `core.c`; the table
(`ferrix-nvme`'s `quirks.rs`) and code are Ferrix's own.

| Quirk | Controllers | What the driver does |
|---|---|---|
| Delay before ready | Seagate Nytro `1bb1:0100`, HGST `1c58:0003`, WDC SN200 `1c58:0023`, Memblaze `1c5f:0540`, Samsung PM1725(a) `144d:a821/a822`, and by model the Samsung Portable SSD X5 | waits 2.3 s after every change of `CC.EN` before reading `CSTS.RDY` |
| Identify CNS | QEMU's old Intel identity `8086:5845` | takes namespace 1 without asking for the list |
| Broken MSI | WD/SanDisk PC SN530 `15b7:5008`, SN550 `15b7:5009` | drives them with MSI-X; if a function had only MSI it is refused |
| Queue depth one | Apple `106b:2001` | one command at a time |
| Not driven | Apple `106b:2005` (128-byte entries, shared tags) | refused before a register is written |

Linux's other quirks concern things this driver never does (Write Zeroes,
Dataset Management, namespace identifiers, the subsystem NQN, temperature
thresholds, suspend, APST, DMA pool alignment: every buffer here is a whole
page) and so need no row. Linux's DMI-keyed quirks are all about APST or
suspend.

## 7. Gates

| Gate | What it requires |
|---|---|
| `cargo test -p ferrix-nvme` | 40 tests: register fields, each command's bytes by offset, queue indices and phase, PRP lists up to a chained list, bring-up order, both block sizes, `MDTS`, refused namespaces, retries, abort then reset, a fatal controller, the reset limit, a controller gone, host memory lent, refused and returned, shutdown order, each quirk |
| `cargo xtask test-nvme` (`--boot io`) | x86-64. `/` on an NVMe drive of 512-byte blocks **below a root port and a PCIe switch**, and a second drive of 4096-byte blocks on a root port. Both drivers' lines; `/` installed on the NVMe root; the first block, the last block and 128 KiB in one request (a PRP list) read back as written; 24 blocks written with a flush, read back, and found in the image with their neighbours untouched; Flush commands in QEMU's own trace; both drivers killed twice under a reading loop with every read right; a file on `/` after each; host `btrfs check` |
| `cargo xtask test-nvme --boot fault` | A read QEMU fails (blkdebug, `EIO`) fails in the guest, the driver says so, the blocks beside it still read, and the controller is neither reset nor given up |
| `cargo xtask test-nvme --boot bench` | §10 |
| `cargo xtask test-restart --boot nvme` | The restart gate's rounds with `/` on the NVMe root |

`--accel tcg` runs each under emulation. `cargo xtask run --nvme` boots the
usual system with its root on the NVMe drive.

**A timeout is not injected under QEMU.** QEMU's controller answers every
command or fails it; holding one back means stalling the host's file I/O
for longer than the 30 s timeout, and QEMU's reset then waits for that same
I/O. Abort, reset and requeue are shown against the model
(`a_lost_command_is_aborted_and_then_the_controller_is_reset` and its
neighbours), not on the emulated device.

## 8. What has only been proved on QEMU's emulated controller

Shown on QEMU, and so far nowhere else:

* reset from whatever state firmware left, enable, Identify, one I/O queue
  pair, MSI-X completion;
* reads, writes, flushes, PRP lists, on 512- and 4096-byte blocks;
* a controller's error failing upward;
* a killed driver's successor resetting a controller that was left
  enabled with its queues in quarantined memory, and serving the parked
  disk;
* a function three bridges down (root port, switch upstream, switch
  downstream): enumeration, its IOMMU domain, MSI-X through VT-d's
  interrupt remapping.

Shown only against this tree's own model of a controller, which was written
from the same reading of the specification as the driver:

* the Host Memory Buffer, every part of it;
* command timeout, Abort, controller reset and requeue; `CSTS.CFS`; a
  drive leaving the bus;
* the shutdown notification, and the order host memory, `CC.SHN`, `CC.EN`;
* every quirk;
* more than one I/O queue pair;
* `MDTS` below 128 KiB; a controller without a volatile write cache; a
  write-protected namespace.

Not shown at all, because QEMU is not a drive:

* **timing.** QEMU is ready within microseconds; a real controller takes
  up to `CAP.TO` (often 20 to 60 s worst case) to become ready and to
  disable. The waits are there; their lengths have never been needed;
* **real DMA.** QEMU reads guest memory by address; ordering, caching and
  an IOMMU's real behaviour (AMD-Vi on the target PC, which Ferrix does
  not drive yet, §11) are untested. The driver puts a full fence before
  every doorbell;
* **real MSI-X**, interrupt coalescing, and a drive that completes many
  commands per interrupt;
* **a BAR whose memory decoding firmware left off.** The kernel publishes
  no aperture for such a BAR (`src/kernel/src/device.rs`), and the driver
  then exits with status 3 (`Registers`). OVMF leaves it on. A PC's
  firmware usually does for a drive it enumerated, but that is not
  promised;
* power loss, thermal throttling, firmware bugs, a drive that is slow
  after a long idle, and every quirk no table lists.

## 9. The first run on a real drive

No agent writes to a real disk (`device-testing-no-persistent-writes`):
this is a checklist for a person, with the product owner's word for the
hardware.

### Before Ferrix: read the drive under Linux

With the drive fitted and Linux booted, as root, and keep the output:

```
lspci -nn -vv -s <bus:dev.fn>        # vendor:device, class 0108:02, MSI-X table size and BAR, the bridges above it
nvme id-ctrl /dev/nvmeX              # vid, mn, fr, ver, mdts, oacs, acl, npss, apsta, hmpre, hmmin, hmminds, hmmaxd, sqes, cqes, nn, oncs, vwc, rtd3e
nvme id-ns /dev/nvmeXn1 -H           # nsze, flbas and the LBA format in use (lbads, ms), dps, nsattr
nvme show-regs /dev/nvmeX -H         # CAP: mqes, to, dstrd, mpsmin, css
nvme get-feature /dev/nvmeX -f 0x06 -H   # volatile write cache
nvme get-feature /dev/nvmeX -f 0x0c -H   # APST as the firmware defaults it
nvme get-feature /dev/nvmeX -f 0x07      # queues granted
smartctl -a /dev/nvmeX               # health, unsafe_shutdowns (write the number down), media errors, firmware
```

The fields that decide what the driver does:

| Field | Decides |
|---|---|
| class code, vendor:device | whether `devmgr` starts `nvme`; the quirk row |
| MSI-X capability | the interrupt; a function with neither MSI-X nor MSI is not driven |
| `CAP.MPSMIN` | must be 0 (4 KiB pages), or the drive is refused |
| `CAP.TO` | how long enable and disable may take |
| `CAP.MQES`, `CAP.DSTRD` | queue size (64 unless smaller), doorbell spacing |
| `SQES`, `CQES` | must allow 6 and 4 |
| `VER` | whether the active namespace list is asked for (1.1 and later) |
| `MDTS` | the largest request, up to 128 KiB |
| `VWC` | whether flushes are commands |
| `APSTA` | whether APST is switched off |
| `HMPRE`, `HMMIN`, `HMMINDS`, `HMMAXD` | host memory: 0 and the drive has its own; otherwise §4 |
| `ACL` | Abort commands at once |
| `RTD3E` | how long a shutdown is waited for |
| `FLBAS`, the format's `LBADS` and `MS`, `DPS` | block size 512 to 4096, no metadata, no protection, or refused |
| `NSATTR` | read-only |

### What the driver prints, and what a failure looks like

At first contact, three lines: the first before a register is written, the
other two once the controller is up (QEMU's, as the gate prints them):

```
nvme     0000:05:00.0 1b36:0010 found: BAR 0 maps 8192 bytes, interrupt by MSI-X, quirks 0x0
nvme     0000:05:00.0 1b36:0010 QEMU NVMe Ctrl fw 10.2.1 NVMe 1.4: namespace 1 of 2097152 blocks of 512 (1024 MiB), 1 queue of 64, 256 blocks a request, write cache on, no host memory wanted, quirks 0x0
nvme     0000:05:00.0 said: CAP 0x... (ready within 7500 ms, 2048 entries a queue, doorbells 4 bytes apart), MDTS 7, VWC 1, APSTA 0, ACL 3, NPSS 0, RTD3E 0 us, HMPRE 0 HMMIN 0 HMMINDS 0 HMMAXD 0, SQES 0x66 CQES 0x44, NN 256
```

Compare every number with the Linux output above. A boot that has the
first line and not the second stopped in the reset, the enable or an admin
command, and the line that says which follows. If the lines do not come:

| On the serial log | Means |
|---|---|
| no `nvme` line at all, and the kernel's `devmgr   ... N failed` counts one | `devmgr` matched nothing (the class code is not `01:08:02`, or the image carries no `nvme`), or the process could not be started |
| `pci      the function's BAR is not published ...` | BAR 0 has no aperture: memory decoding is off, or the kernel withheld the range. Exit status 3 |
| `pci      the function's first MSI-X or MSI vector could not be claimed` | no usable interrupt. Exit status 7 |
| `nvme     <address> N pages could not be pinned ...` | no memory, no IOMMU domain, or the pin budget. Exit status 4 |
| `nvme     <address> did not come up: Gone` | the registers read all ones: no decode, or the link is down |
| `... did not come up: PageSize` / `NoNvmCommandSet` / `EntrySize` | the `CAP` and Identify checks above |
| `... did not come up: DisableTimeout` / `EnableTimeout` | `CSTS.RDY` did not follow `CC.EN` within `CAP.TO`; exit status 9 if it could not be disabled (its memory stays pinned), 5 otherwise |
| `... did not come up: AdminTimeout(op)` | the admin queue got no answer for opcode `op` in 60 s: the interrupt is not needed for this (admin commands are polled), so it is the queue memory or DMA: check the IOMMU |
| `... did not come up: Admin(op, status)` | the controller refused opcode `op` |
| `... did not come up: Namespace(...)` / `NoNamespace` | the format |
| the `nvme` lines, then `completions waited 100 ms with no interrupt` | I/O works and the interrupt is late or does not arrive. On hardware: MSI-X or its remapping. Under QEMU it was seen only with the host too loaded to run every virtual processor (load 50), where the processor an interrupt goes to stands still while the driver's watch runs on another |
| `commands timed out: abort sent`, `controller reset (...)`, `request N failed` | §2; each says what it did |

### The order of the first run

1. Boot the live image with `/` in memory (`ferrix.root=tmpfs`), so nothing
   mounts the drive. Read the `nvme` line.
2. Read only: `dd if=/dev/vdX of=/dev/null bs=1M count=1024`, and compare
   `sha256sum` of the first gigabyte with Linux's of the same range.
3. Only on a drive with nothing to lose: write, flush, read back, power off
   from the shell, boot Linux, compare, and read `smartctl -a` again
   (`unsafe_shutdowns` will have gone up by one per Ferrix power-off until
   the kernel sends STOP, §2).
4. Kill the driver from a shell (`kill -9`), and see it come back.

## 10. A 4 KiB read

`docs/BACKLOG.md`, *Block I/O faster than Redox*, sets 80 us at the median
for a 4 KiB read through the block ring and its ring-3 driver. The row's
own method (20,000 samples around `rdtsc`, alternated with Redox's image)
measures the kernel's path with a program this tree does not carry for
Ferrix; the kernel's `seam` boot line measures the first virtio disk only.
`test-nvme --boot bench` therefore measures what can be compared in one
boot: `dd` reading 4096 blocks of 4 KiB from the virtio-blk pattern disk
and from the NVMe scratch drive in turn, five runs each after a warm-up,
each run's time over its reads being its mean read. It includes the system
call and the copy to the reader, which the `seam` line does not.

Measured on nazuna on 2026-10-10 under KVM, four virtual processors, with
the host at a load of 32 (other sessions' builds, and a game), so the
runs scatter:

| Disk | Median of five runs | Least | The runs, us |
|---|---|---|---|
| virtio-blk (`vda`, 512-byte blocks) | 132.6 us | 89.9 us | 89.9, 101.3, 132.6, 201.3, 418.4 |
| NVMe (`vde`, 4096-byte blocks) | 120.2 us | 101.6 us | 101.6, 120.2, 120.2, 138.8, 182.1 |

The kernel's `seam` line for `vda` in the same boot said a median of
222.4 us at depth 1. **Neither driver meets 80 us on a host this loaded,**
and the two are within each other's scatter: the time is the ring's two
copies and the hops between the kernel, the driver and QEMU, which both
drivers share, not the device protocol. They are QEMU's device model both
times: the difference between two drivers' paths, not a drive's speed. The
row stays open; this adds only that NVMe is no slower than virtio-blk
through the same ring.

## 11. The first real drive: WD PC SN720, and the PC around it

The customer's first drive is a WD PC SN720 NVMe, 256 GB, for the ASUS
ProArt B650-CREATOR's third M.2 slot (`M.2_3`), which hangs off the B650
chipset: the drive is behind the chipset's PCIe switch, not on a processor
root port.

What is known without the drive in hand, and how:

* **PCI identity**: `15b7:5002` by the PCI ID database
  (`/usr/share/misc/pci.ids`: "SanDisk Extreme Pro / WD Black 2018/SN750 /
  PC SN720 NVMe SSD"); the SN720 shares it with the WD Black 2018 and
  SN750. To be confirmed by `lspci`.
* **Linux's quirks**: none. Linux 6.18's device table has three
  Western Digital/SanDisk rows (`15b7:5008` SN530, `15b7:5009` SN550,
  `15b7:2001` Skyhawk) and no row for `15b7:5002`; `core.c`'s model table
  has no Western Digital row; no DMI quirk names it. Linux drives it by
  class code with APST on.
* **DRAM and host memory**: not verified here. From memory only, the
  SN720 has its own DRAM and is the OEM sibling of the WD Black
  2018/SN750, so `HMPRE` would be 0 and §4 would not apply. `nvme id-ctrl`
  decides.
* **NVMe version**: from memory 1.3; `VER` decides. Either way the active
  namespace list is used.
* **Power states**: no quirk exists in this Linux tree for the family.
  Reports of trouble in low-power states on some SN750 firmware are not
  reflected there and were not verified. This driver switches APST off, so
  the drive never enters a non-operational state by itself.

What the driver must not assume about it, and does not: where the MSI-X
table is, how many queues it grants, `MDTS`, that a write cache is present,
that the format is 512 bytes, that firmware left it disabled, and that it
sits on a root port. Nothing in `devmgr`'s match, the driver, or
`ferrix-driver`'s PCI bus looks at a function's parent: the match is the
class code, the BAR and vectors come from the function's own node, and the
gate's root drive sits below a root port and a two-port switch on purpose.

The earlier candidate, a WD PC SN520 (`15b7:5003`, or `5004`/`5005` for the
2230 and 2242 sizes, by the same database), also has no row in Linux's
tables. It is DRAM-less; whether it asks for host memory was not verified
(from memory it does not).

### What the first bare-metal boot needs besides this driver

This driver is one slice of `docs/INSTALLER.md` §9's *Real PCs*. On this PC
the rest of the distance is:

* **H0, the first boot on a PC at all**: Ferrix has only booted under QEMU
  and on two Arm boards. ACPI as a real firmware writes it, interrupt
  routing, the timer, and a console (the board has no serial port without
  a header or a card; the firmware's framebuffer is H6).
* **An AMD IOMMU driver.** Ferrix drives Intel's VT-d and Arm's SMMUv3. A
  Ryzen's IOMMU is AMD-Vi, described by the `IVRS` table, and nothing
  drives it. Without it no device has a translated domain, and what the
  kernel does with a driver's pins on such a machine has to be decided,
  not assumed: DMA isolation is a claim of the certified item.
* **A way to boot it**: the live medium on a USB stick needs xHCI and USB
  mass storage (H3, H4; xHCI is being built). Booting from the NVMe drive
  itself needs an EFI system partition on it and the loader's boot entry
  (I8, I9), and Secure Boot off or the key enrolled (I13).
* **A keyboard**: USB HID over xHCI (H3).
* **A screen**: the firmware's framebuffer for the compositor (H6); the
  NVIDIA driver is stage 21 and not needed.
* **Network**: no driver for a PC's Ethernet or Wi-Fi exists (the board has
  Intel 2.5 GbE and Marvell 10 GbE); not needed to boot or to install.
* **STOP at power-off** (§2), so the drive is told before power goes.
* **On Arm, not this PC**: a function below a PCIe bridge has its DMA
  faulted by the SMMUv3 (`docs/BACKLOG.md`, found by this gate), so an Arm
  machine with an NVMe slot behind a root port waits on that.
* **Disk names**, if `/dev/nvme0n1` is wanted (§3).

## 12. Where it stands

2026-10-10: built as above on branch `nvme-driver`; gated on QEMU's
controller under KVM and TCG; never run on hardware. Next, in order: the
first run of §9 once the drive is fitted and the PC boots Ferrix at all
(H0); STOP at power-off; AArch64 in the gate once uutils is built for it.
