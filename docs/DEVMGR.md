# devmgr: what the kernel starts it with, and what it does

Version 1. Written by os-5b (stage 10), the shape decided by the product owner
os-23 on 2026-09-13; the object side reviewed by stage 9, the start shape by
stage 11. `docs/ARCHITECTURE.md` §7 is the architecture; this is the protocol
between the kernel and `devmgr`, and between `devmgr` and the drivers it
starts. The kernel's half of it — `device_info`, `device_quiesce`, bus
mastering at the first pin, START — is on develop; `devmgr` the program, on
`ferrix-rt`, is `src/user/system/native/devmgr`, started by `src/kernel/src/discovery/devmgr.rs`; the
messages are `src/lib/proto/devmgr-proto`.

## 1. What devmgr is, and what it is not

`devmgr` is a native program in the initramfs, `/sbin/devmgr`, started by the
kernel once, after the boot checks and before `init`. It matches devices to
drivers and starts each driver in a job of its own with exactly what
ARCHITECTURE §7 says a driver gets: its device, the channel to the subsystem
it serves, and the means to claim that device's memory, interrupts and DMA.
It watches each driver and, when one dies, makes its device safe before
anything of it is reused.

It is **not** a driver, and it drives nothing: it never maps a device's
registers, never pins memory for one, never touches configuration space. It
is also **not** the enumerator: the kernel found the devices and keeps their
nodes; `devmgr` is handed them. And it does not read files. A native program
has native calls only — handles, channels, VMOs, ports, processes — and no
descriptors, so the kernel reads the driver images out of the initramfs and
hands them over as memory (§2). That is also what makes the rule in §5 hold
by construction.

## 2. Kernel → devmgr: the bootstrap channel

The kernel creates a channel, keeps one end, and starts `/sbin/devmgr` with
the other as its bootstrap handle -- at bring-up, or, under
`ferrix.devmgr=init`, when pid 1 asks with its starter (`devmgr_start`,
`docs/INIT.md` §7.3), in the job pid 1 names, which then holds the drivers'
jobs too; either way the channel goes between the kernel and `devmgr` alone,
and this section is the same -- (`Handle(1)`, which `ferrix-rt`'s
`Bootstrap` adopts; `docs/BLOCK-RING.md` §6.4 says the same of a driver's).
Before starting it the kernel has written one message on its end:

```
DEVICES  kernel -> devmgr, 24 + 32 x drivers bytes
0   4  type = 1
4   4  length
8   4  devices      how many devices in this message; each is two handles
12  4  drivers      how many (name, image) pairs follow; 0 unless first
16  4  more         how many devices still come in later DEVICES messages
20  4  flags        bit 0: the first message, which carries the job and
                    the images
24  32 name[0]      the first driver's program name, NUL-padded, as
                    process_create takes it (PROCESS_NAME_MAX)
56  32 name[1]      ...
handles: [job (first only), device 0, device 0 again, ..., image 0 ...
          (first only)]
```

* **The job** comes first: a root job of `devmgr`'s own, with every right a
  job carries, under which it makes a job per driver. A native program is
  given no job otherwise, and `process_create` needs one.

* **The devices** are every node `device::devices()` publishes, in that order,
  each as two handles. The first has `DEVICE_RIGHTS | SET_LIMIT`
  (`TRANSFER | MANAGE | SET_LIMIT`) and is the handle the driver will be
  given in START, narrowed to exactly `DEVICE_RIGHTS`. The second stays
  with `devmgr` for the quiesce in §4, because a handle given away is gone. It
  has `DEVICE_RIGHTS | SET_LIMIT | DUPLICATE`, so a driver started again after
  a death gets a duplicate of it, narrowed the same way, and `devmgr` still
  has one for the next quiesce. `SET_LIMIT` is for the device's pin budget
  (§3.1), which is `devmgr`'s and never a driver's.
  `device_info` (0x1049) on either says what the device is.
* **The drivers** are every program in one fixed initramfs directory,
  `/lib/drivers/`, in the order of `/lib/drivers/MANIFEST`, one name per
  line, which `xtask` writes when it builds the image: the kernel has no
  directory listing of its own to spend on this. For each, the kernel reads
  the file into a VMO it creates for the purpose — anonymous memory, filled
  by the kernel from the initramfs — and hands the VMO with
  `READ | TRANSFER`. The name is the file's name. `process_create` (0x1030)
  takes exactly this: a job, an image VMO with `READ`, a name.
* A channel message carries at most `CHANNEL_MAX_HANDLES` (64) handles, and
  a device takes two: ARMv7-A's machine publishes 36 nodes, its 32
  virtio-mmio transports among them. So the kernel sends as many DEVICES
  messages as it takes — the first with the job and the images, the rest
  with devices only — each saying in `more` how many devices are still to
  come, and `devmgr` reads until that is zero.

`devmgr` answers on the same channel once it has done what §3 says:

```
REPORT   devmgr -> kernel, 16 bytes
0   4  type = 2
4   4  length = 16
8   4  started      drivers started and whose disks the kernel accepted
12  4  failed       devices that matched a driver and got none
```

The kernel waits for REPORT with a deadline, prints one line for the boot log
— `devmgr   N devices, M drivers, K started, F failed` — and goes on to
`init`. `xtask test-boot` requires that line on the machines it configures
with a disk, with `started` at least 1 and `failed` 0, so a `devmgr` that
starts nothing fails the boot rather than leaving `/dev/vda` quietly absent.
The kernel keeps its end open: a later message from `devmgr` is a report of
a driver's death or of its restart (§4), printed the same way.

## 3. What devmgr does with them

1. `device_info` on every device. The identity decides the driver, by a table
   in `devmgr` and nowhere else: vendor `0x1AF4` with device `0x1042` or
   `0x1001` is virtio-blk, driven by `blk`; stage 17 adds virtio-gpu
   (`0x1050`) and virtio-input (`0x1052`) to the same table, and their
   drivers to the same directory, with no change to the kernel. A device
   nobody drives is left alone, and its handle closed. A device tree node
   the kernel publishes for a binding it knows reports
   `DEVICE_TREE_BLOCKS` and the binding's number in place of a PCI identity,
   its register windows in the blocks; a second, smaller table matches those:
   `TREE_STM32_HDMI`, the DK board's HDMI output, is a card driven by `ltdc`
   (`docs/DISPLAY.md` §6). A third matches PCI functions that are not virtio
   transports: vendor `0x10DE` with base class `03`, an NVIDIA display
   controller, is a GPU driven by `nvrm` (NVIDIA's N1b, §3.2); and QEMU's
   `pci-testdev` (`1B36:0005`) is a GPU driven by `nvrm-test`, an image only
   `cargo xtask test-nvrm` puts in the driver directory, since QEMU emulates
   no NVIDIA function.
2. Each device keeps the kernel's default pin budget, 66560 pages, unless
   its kind's rule says otherwise (§3.1). Devices of one kind are named in
   PCI order — `vda`, `vdb`, … for disks, as `docs/BLOCK-RING.md` §6.1
   decides — so names are stable on a given machine.
3. For each match, in that order:
   * `block_ring_create` (0x1048) on the device: the driver's end of the
     ring's control channel. For a device whose subsystem has no ring yet,
     the kernel has no call and `devmgr` starts nothing.
   * `job_create`: a job of the driver's own, under `devmgr`'s.
   * `process_create` in it, from the driver's image VMO, named after the
     driver and the disk (`blk:vda`).
   * A channel pair; on `devmgr`'s end, START (`docs/BLOCK-RING.md` §6.4)
     built from the `DeviceInfo` — the blocks, the multiplier, the MSI-X
     table size, the PCI device identifier, the location, the name — with
     handles `[device, control]`, exactly `DEVICE_RIGHTS` and
     `CONTROL_RIGHTS`. `devmgr` gives the device away here and keeps no
     handle to it: the driver holds it, and the kernel's node outlives both.
   * `object_wait_async` on the process handle for `TERMINATED`, on
     `devmgr`'s one port, with the disk's index as the key.
   * `process_start` (0x1031) with the other end of the pair as the
     bootstrap.
4. `devmgr` does not wait for the driver's HELLO — that is between the driver
   and the kernel — but its REPORT counts a driver as started only once the
   kernel has published the disk. `devmgr` cannot look in `/dev`, so the
   kernel tells it: after accepting a HELLO for a device it handed `devmgr`,
   the kernel writes on the bootstrap channel:

```
PUBLISHED  kernel -> devmgr, 16 bytes
0   4  type = 3
4   4  length = 16
8   4  location   the device's PCI address word
12  4  reserved   zero
```

   `devmgr` starts one driver, waits for its PUBLISHED, then starts the
   next, so disks register in PCI order and no two drivers race to be `vda`;
   then it sends REPORT. A native program has no clock, so the deadline is
   the kernel's patience for REPORT: a driver that never publishes holds
   `devmgr` there, and the boot fails saying so. A driver whose start fails
   outright counts as failed.

### 3.1 Pin budgets

A device's pin budget bounds what its drivers' pins hold: a pin past the
budget is refused `LIMIT_REACHED`, and one that would take the device's
quarantined, kept and live pages past twice it `QUARANTINE_FULL`
(`docs/NVIDIA.md` §12.2, `src/kernel/src/object/pin.rs`). The budget is the
node's, and survives every driver's death and `devmgr`'s own restart.
`devmgr` sets it with `device_set_limit` (0x1057), which needs `SET_LIMIT`,
before the driver starts: the kernel refuses a change under live pins.

| Kind | Budget |
|---|---|
| disk, network, display, input, sound, port, host, engine, gadget | the kernel's default, 66560 pages (one driver's worst case); not set |
| GPU under `nvrm` (NVIDIA's N1b, §3.2) | `max(ceiling / 2, 1 GiB)`, cut to the room |

The GPU's rule (`ferrix_devmgr_proto::budget`, the customer's decision of
2026-10-02) reads the ceiling and the room with `device_get_limit` (0x1058):
the ceiling is a quarter of RAM, on twice every raised budget, and the room
what other devices' raised budgets leave. It wants an eighth of RAM and at
least 1 GiB; when twice that is more than the room, the floor yields and the
budget is half the room. Every outcome is a line, and a GPU that cannot have
enough is not started -- there is no fallback to the default:

```
devmgr   gpu 01:00.0: pin budget 1024 MiB (an eighth of RAM, at least 1 GiB)
devmgr   gpu 01:00.0: pin budget 512 MiB, cut from 1024 MiB: the 1 GiB floor yields to the kernel's ceiling (4096 MiB of RAM)
devmgr   gpu 01:00.0 not started: its pin budget of 128 MiB is under the 256 MiB nvrm needs
devmgr   gpu 01:00.0 not started: the kernel refused its pin budget of 512 MiB (past the ceiling)
```

256 MiB is `NVRM_MIN_PIN_PAGES`, until NVIDIA's N1d measures what `nvrm`
needs. The last line is for a `NO_MEMORY` from `device_set_limit`, when
another device took room between the read and the set; any other refusal
of the budget is `… not started: the kernel refused its pin budget of 512
MiB`. The `Gpu` kind applies the rule as the last step of its hand-over
(§3.2).

### 3.2 The `Gpu` kind

A GPU runs firmware of its own, NVIDIA's GSP, which is not its driver's to
vouch for, and its driver pins more than any other. So `devmgr` hands one to
`nvrm` only once three things hold, in this order, each through the device
handle it keeps, which alone has `SET_LIMIT` (`ferrix_devmgr_proto::gpu`,
host-tested):

1. **The mark.** `device_set_limit(DEVICE_LIMIT_ISOLATED_INTERRUPTS, 1)`,
   set-once (`docs/NVIDIA.md` §12.3). From then on the kernel refuses the
   device vectors and pins whenever the machine's interrupts are not
   isolated, whatever `devmgr` decides next. A refused mark stops the
   launch: `devmgr   gpu 01:00.0 not started: the kernel refused its
   isolated-interrupts mark`.
2. **Isolation.** `device_isolation` must have bit 1, interrupts isolated.
   Without it the GPU is not handed over at all, and the budget is not
   raised: `devmgr   gpu 01:00.0 not started: its interrupts are not
   isolated (device_isolation 0x0)`. This is where F-57's closure is
   verified for the `ferrix-3060` domain at N1's first boot.
3. **The budget**, as §3.1 says, set before the driver starts.

A GPU handed over is said in two lines, then started as a port's driver is,
with its device and START over the bootstrap channel:

```
devmgr   gpu 01:00.0: marked for isolated interrupts, device_isolation 0x2 (interrupts isolated); handing it to nvrm
devmgr   gpu 01:00.0: pin budget 1024 MiB (an eighth of RAM, at least 1 GiB)
```

A GPU not handed over counts as failed in REPORT; its device handles are
closed and the node stays unstarted. A GPU publishes nothing yet (the
forwarding core is NVIDIA's N1e), so it is taken at its word as an engine
is, and it is not started again when it dies (§4) until `nvrm` can tear
down a GSP it did not boot (NVIDIA's N2). A BIND of it runs the whole
hand-over again.

**How `devmgr` says it.** The kernel prints only what the bootstrap
protocol carries, and the hand-over's lines are `devmgr`'s own decisions.
So `devmgr` writes them to standard error, which every process has open on
the console from its start, as `gc400` writes its findings; nothing in the
kernel changed for it.

**Starting a ferrousli program.** `nvrm` is a static Linux-personality
program, linked against ferrousli, because RM needs threads, futexes and
clocks (`docs/NVIDIA.md` §4.1). `devmgr` starts it as it starts every
driver -- `process_create` from the image the kernel read out of the
initramfs, in a job of its own, and `process_start` with its bootstrap
channel -- so the job, the process handle, the death watch and §5's
image-as-memory rule hold for it unchanged. A native start gives a program
its bootstrap handle and an empty stack, so `nvrm`'s entry
(`src/user/system/linux/drivers/nvrm/src/start.c`) builds the `argc`,
`argv`, environment and auxiliary vector ferrousli's `__libc_start_main`
reads, and calls it as `crt1.o` would. From `main` on it is an ordinary
ferrousli program that also makes native calls.

**RM's core is not in `nvrm`'s image.** The core is 13 MB, too large for
the kernel's read of a driver's image (§5). So `nvrm` reads it at run time
from the NVIDIA volume, at `/data/usr/lib/ferrix/nvrm-core`, and never
from the initramfs (`docs/NVIDIA.md` §4.1, "The core"). It reads the file
with `read(2)` into anonymous memory of its own, so nothing maps the file.
The volume is mounted after REPORT, so `nvrm` waits for the file, at most
60 s. devmgr does not wait on `nvrm`, and its REPORT counts the GPU as
started once `nvrm` is. The sha256 pin that `nvrm` checks the core against
protects the product. It is not an argument for the certified item.

`cargo xtask test-nvrm` (x86-64, KVM and TCG) boots the three outcomes
QEMU can show: handed over at 4 GiB, refused for a budget too small at
512 MiB, and refused for interrupts not isolated on a VT-d unit with
`intremap=off`.

## 4. When a driver dies

A `TERMINATED` packet on `devmgr`'s port names the disk. The kernel's ring
has already failed every outstanding read with `EIO` and unpublished the node
(`docs/BLOCK-RING.md` §6.3), and the driver's handles have closed: its pins
are gone from any translated domain, and kept, leaked, in an untranslated
one. Nothing has reset the device, and until something does the kernel
refuses a new ring for it (`ALREADY_BOUND`). `devmgr`:

1. calls `device_quiesce` (0x104A) on the device — bus mastering off, so the
   device reaches nothing, and the ring's claim released. `TERMINATED` fires
   when the driver's handles close, which queues `PEER_CLOSED` for the ring's
   task but does not wait for it, so the quiesce may arrive before the ring
   has ended: the kernel then waits, bounded, for the ring to let the device
   go, since the driver's end of the channel is provably closed. The net
   ring, display, render, input and sound cores are waited for the same way
   (`src/kernel/src/claim.rs`), so a quiesced device has no claim left on it and
   a driver started again gets its channel. Only a driver still holding its end gets `BAD_STATE`, which
   `devmgr` never retries; a core that has not let go within the kernel's
   patience answers `TIMED_OUT`, which `devmgr` retries until it succeeds,
   since the device must not stay on;
2. writes on the bootstrap channel a DIED message, which the kernel prints:

```
DIED     devmgr -> kernel, 16 bytes
0   4  type = 4
4   4  length = 16
8   4  location
12  4  status     the driver's exit code, or 128 and the signal that killed it
```

   A `TERMINATED` packet carries no status, so `devmgr` reads it from the
   process handle (`process_status`, `docs/INIT.md` K6): `137` for a
   `SIGKILL`, `1` for a driver that exited 1. One whose status cannot be read
   is reported as `137`, as every death was before K6;
3. starts a **display**, **sound**, **network**, **input** or **disk**
   driver again, once the quiesce
   succeeded: in a new job, on a duplicate of its kept device handle, with
   the START it was first given, and waits for PUBLISHED as at boot. The
   kernel numbers cards and render nodes lowest-free, so the card comes back
   as the `card<N>` or `controlC<N>` it was, and a compositor that waits for
   it (hyprix does) opens it again; a program holding a dead sound card gets
   `EBADFD`. A dead driver's pins on a translated domain stay mapped, their
   frames held, until the core accepts the next driver's HELLO, which it
   sends after resetting the device (`src/kernel/src/object/pin.rs`, finding
   F-38): QEMU writes a dead driver's buffers late, and those writes must
   not reach frames the allocator has handed on. A core that restarts a new
   kind calls `object::pin::quarantine_release` where it accepts a HELLO. A
   driver that publishes gets RESTARTED, which the kernel prints:

```
RESTARTED devmgr -> kernel, 16 bytes
0   4  type = 5
4   4  length = 16
8   4  location
12  4  restarts   how many times this device's driver was started again
```

   Whether to start it again is the service manager's own restart policy
   (`src/lib/init/restart`, `docs/INIT.md` §5.4): `Restart=always` for a kind
   that is started again and `Restart=no` for the rest, no delay, and a start
   limit of eight. A native program has no clock, so the limit is a count,
   not a rate: eight restarts per device, after which the device stays
   quiesced, as does one whose restarted driver dies before it publishes.

What each kind gets back (2026-09-27, T0 of the live kernel update plan):

* A **network** interface is parked with its index, name and addresses until
  the next driver's HELLO takes it up (`docs/NET-RING.md`), so a socket
  bound to it sees only the carrier go and come back.
* An **input** device takes the lowest free `event<N>`: a device whose
  driver dies alone comes back under its number, but when two die together
  the one that comes back first takes the lower.
* A **disk** is parked too: the block ring keeps its node published and its
  requests queued, and replays them to the next driver for the same
  location, so a filesystem mounted on it never sees the death
  (`docs/BLOCK-RING.md` §6.3). Replaying the dead driver's in-flight writes
  is safe because a block write is idempotent at its LBA and the replay keeps
  the queue's epoch and barrier order. A disk no driver takes up within 30
  seconds answers new requests with EIO, and each waiting request keeps its
  own 30-second patience. Every disk driver killed under a btrfs root leaves
  `/` writable, and `btrfs check` clean.

Each core gives a dead driver's quarantined pins back only once it has
accepted the next driver's HELLO, which that driver sends after resetting
the device (`object::pin::quarantine_release`). Every other kind is not
started again: the serial port (`vport`) has no core to wait for, and the
USB host (`usbhid`), the GPU engine (`gc400`), the gadget (`usbdev`) and a GPU
under `nvrm` (§3.2) stay quiesced after their driver dies. `devmgr` itself stays fatal until the
kernel can offer a new `devmgr` the devices again (`docs/INIT.md` L12).

It began as a bug. With no restart, `kill -9` of the `gpu` driver under the
desktop took the card away for good, the compositor ended on `ENODEV`, and
the compositor was init, so the machine powered off.
`cargo xtask test-restart` (a shell kills it twice; `--boot input`, `net`,
`blk` or `all` for the other kinds, the disk's on a btrfs root) and
`cargo xtask test-compositor --boot restart` (a script first stops it for
longer than the kernel's five-second reply wait, which hyprix must ride out
by dropping frames, then kills it twice under hyprix) are the gates. For sound, `cargo xtask test-audio`'s restart boot
kills `snd` twice under a running stream, requires the quarantine to have
caught a late write, and plays a second on the third driver's card.

`device_quiesce` needs `MANAGE`, and `devmgr` gave one device handle away in
START; the quiesce goes through the second handle §2 gave it for exactly
this.

### 4.1 A new version of a driver, without a reboot

The customer's question of 2026-10-05: can a driver be replaced by a new
version while the machine runs? For the five kinds §4 starts again the hard
part exists: the device comes back with its state kept when its driver
dies. An update is that restart onto a different image, with a way back.
This section is the design (po10-drv, 2026-10-07); "as built" notes follow
each landing. The other kinds and `devmgr`'s own restart are outside it, as
they are outside §4.

**Where the new image comes from.** Every image `devmgr` has comes from the
initramfs, through the kernel (§2), and `devmgr` reads no files. An update's
image comes from root at run time, through a helper and not through the
kernel:

* `drvupdated` (`src/user/system/native/drvupdated`) is a native program an
  image may carry in `/lib/drivers`, listed in `MANIFEST` like a driver, so
  the kernel hands it to `devmgr` as one more image. No table entry names it,
  so it drives no device and is no `DRIVER` in sysfs. When the images
  include it, `devmgr` starts it after REPORT, in a job of its own, with a
  channel whose other end `devmgr` keeps on its port. An image without it has
  no updates: whether the image carries the helper is the opt-in, the way
  `nvrm-test` opts in to the test GPU. Until D3 (below) has its review,
  only the images `cargo xtask test-restart --update` boots carry it. If
  the helper dies, `devmgr` says so on its line
  and does not start it again.
* The helper binds the abstract `AF_UNIX` name `\0ferrix.devmgr.update`.
  The name is abstract because `devmgr` and what it starts keep the
  initramfs root (`docs/INIT.md` §7.3). The helper takes one connection at
  a time and closes any peer whose `SO_PEERCRED` uid is not 0, as `vport`
  does.
* The client, `/bin/drvupdate <driver> <image> [<location>]`, sends a
  48-byte header and then the image's bytes. The header holds a magic, the
  driver's name, the device's PCI address word (or any, which means every
  device that driver drives) and the length. The helper copies the bytes
  into a VMO it creates, up to 8 MiB, and writes UPDATE to `devmgr` with the
  VMO. It writes `devmgr`'s answer back to the client as one line.
* `devmgr` copies that VMO into one of its own before it reads anything in
  it. Any handle the helper kept cannot change the bytes between `devmgr`'s
  checks and `process_create`, and §5 holds as it does at boot: the new
  driver's image is anonymous memory, filled before the driver exists, and
  nothing maps a file. For a disk's driver this means the whole image is in
  memory before the old driver stops.

The kernel is not the way in because that path would add a kernel
interface into the certified item. A sysfs `update` file whose write makes
the kernel read an image would need one, and the kernel would read a file
from a root that is not the initramfs. The helper needs nothing new: an
abstract socket, `SO_PEERCRED`, VMOs and channels all exist. `devmgr` does
not take the socket itself because its loop waits on one port, and a port
does not watch a descriptor. Slow or hostile I/O on a socket would then
hold up the quiesce of a driver that died. The helper does the I/O and
hands `devmgr` one finished message.

**What `devmgr` checks before it stops anything.** An update leaves the
running driver alone unless all three hold:

1. The device is one the driver named in UPDATE drives by the table. Its
   kind is one §4 starts again, and its driver is up (published). If not,
   the answer is `no device`, `not restarted` or `busy`.
2. The image loads. `devmgr` calls `process_create` on its copy in a scratch
   job. The kernel's loader refuses anything that is not a native program it
   can start, and the answer is then `refused`. `devmgr` kills the scratch
   job either way.
3. The console line names the image: its length and its FNV-1a-64
   fingerprint, so the log shows which bytes drive the device. This proves
   integrity, not authenticity. Whether an update must also carry a
   signature, and whose key verifies it, is the customer's question
   (below).

**The swap, and the rule for going back.**

1. `devmgr` kills the running driver's job and quiesces the device, as for
   a death (§4, step 1), and sends UNBOUND. It sends no DIED, because no
   driver died that `devmgr` did not stop.
2. It starts the new image in a new job, on a duplicate of its kept device
   handle, with the START the device was first given. It then waits for
   PUBLISHED with a deadline of 15 s (`devmgr` reads the monotonic clock
   through the Linux call `await_settled` already uses).
3. **Published**: from now on the device's image is the new copy, so a
   restart after a death or a BIND starts it. `devmgr` sends BOUND, answers
   `updated` and prints `devmgr   gpu 00:02.0 updated: N bytes, fnv64 X
   (was M bytes, fnv64 Y)`.
4. **Not published**, because the new driver exited or the deadline passed:
   `devmgr` kills its job, quiesces the device and starts the old image
   again, as a restart does, then waits for PUBLISHED. It answers `rolled
   back` and prints the same pair of images. If the old image does not
   publish either, the device stays quiesced, as when a restarted driver
   dies before it publishes (§4), and the answer is `failed`.
5. An update counts against no restart budget, as a BIND does not. Requests
   that arrive while it runs wait in the inbox, as during any other wait.
   `devmgr` runs one update at a time.

**What the kernel changes: nothing.** Every call an update makes is one a
restart makes today. The kernel sees a driver die after a quiesce and a new
HELLO for the same device, and the bootstrap protocol keeps its messages.

**Still open, for the customer.** Is root's word enough for D3, or must an
update image carry a signature, and if so whose key (one built into
`devmgr`, or one in the image's configuration)? The first kind lands with
the uid-0 gate, the loader's refusal and the fingerprint line, and no
signature.

**Its gate** is `cargo xtask test-restart --update` (D4). On the display
kind, with `/bin/blank` holding the card, the gate updates the driver four
times:

1. to bytes that are not a program: `refused`, and the card never goes;
2. to a native program that never publishes: `rolled back`, and `card0`
   comes back from the old image;
3. to the `gpu` driver built as version `next`, which prints `gpu: version
   next` once at its start: `updated`, `card0` comes back, the line
   appears, and the fingerprint is the carried file's;
4. then a `kill -9` of the updated driver: it is started again from the new
   image, and its version line appears a second time.

The shell must answer after each step.

## 5. No driver faults on the disk it serves

The decision of 2026-09-13 (`docs/BACKLOG.md`): a driver serving a disk must
never take a page fault that fills through that disk, or it waits on its own
completion forever. Under this design it holds by construction rather than by
`devmgr` checking anything:

* a driver's image is an anonymous VMO the kernel filled from the initramfs
  before the driver existed; `process_create` reads it into the new process's
  memory, and nothing maps the file;
* a native program has no `mmap` of files — it maps VMOs, its own or ones it
  was handed — and the driver's are the ring VMO, the data VMO and its DMA
  memory, all anonymous;
* `devmgr` itself is started the same way, before any mount of a disk.

What is left to enforce is the future: a pivot onto btrfs must not re-exec or
remap a running driver from the new root, and a driver started after the
pivot must still get its image from the initramfs copy the kernel keeps.
`devmgr` is the one process that starts drivers, so it is where that rule
lives when the pivot exists.

**`nvrm` is the one exception to "every image from the initramfs."** Its
own image comes from the initramfs like every driver's. But RM's core
(§3.2) is read from the NVIDIA volume after `nvrm` has started. The rule
still holds in substance:

* `nvrm` serves no disk. It is never a disk's driver, and the volume is
  served by the disk's own driver.
* The core is copied with `read(2)` into anonymous memory. No page of it is
  filled from the file afterwards, so `nvrm` takes no fault through any
  disk.

A driver that serves a disk must never load code this way.

## 6. What this settles for stage 10's exit

The exit criterion needs a sector read through a ring-3 driver. Stage 11's
boot check starts `blk` from the check itself with a kernel-driven parent,
sending the same START (`block_ring::start_for`), so the driver is proven
before `devmgr` exists; `devmgr` then replaces that parent for the running
system, with no change to the driver, and the boot check's line above is what
`xtask` reads. `devmgr` is the last program on stage 10's list; the BAR trust
row is beside it, not on the path.

## 7. Drivers and bindings, for sysfs

`devmgr` owns two facts sysfs shows (`docs/SYSFS.md` §2): which drivers it
can start, and which drives which device. It tells the kernel both on the
bootstrap channel, and the kernel keeps what it was told
(`src/kernel/src/discovery/devmgr.rs`). A device is named by its place among the devices
the DEVICES messages carried, in order, and a driver by its place among
their names: a device tree node has no PCI address to be told apart by.

```
DRIVER   devmgr -> kernel, 16 bytes
0   4  type = 6
4   4  length = 16
8   4  driver     its place among DEVICES' names
12  4  bus        1 PCI, 2 platform (a device tree node)

BOUND    devmgr -> kernel, 16 bytes
0   4  type = 7
4   4  length = 16
8   4  device     its place among DEVICES' devices
12  4  driver

UNBOUND  devmgr -> kernel, 16 bytes
0   4  type = 8
4   4  length = 16
8   4  device
12  4  reserved   zero
```

DRIVER comes once for each driver in the table whose image the initramfs
carries, before any driver is started; BOUND once a driver has published, or,
for a port or a bus host, which publish nothing, once it has started; UNBOUND
once a driver has died or been unbound and its device quiesced, beside DIED.
All of them may come before REPORT, and the kernel takes them there too.

A write to a driver's `bind` or `unbind` in sysfs is a request, which `devmgr`
answers:

```
BIND     kernel -> devmgr, 16 bytes
0   4  type = 9
4   4  length = 16
8   4  device
12  2  driver
14  2  token      echoed in DONE

UNBIND   kernel -> devmgr, 16 bytes
0   4  type = 10
4   4  length = 16
8   4  device
12  2  driver     the driver whose unbind was written
14  2  token

DONE     devmgr -> kernel, 16 bytes
0   4  type = 11
4   4  length = 16
8   4  token
12  4  answer     0 done, 1 no device, 2 busy, 3 failed
```

`devmgr` watches its channel for requests for the life of the machine, beside
its drivers' deaths. UNBIND kills the driver's job; its death is quiesced as
§4 says and answered with DONE instead of DIED, and nothing is started again.
BIND starts the driver the way it was started at boot -- a disk's `blk` with
the name it had -- and answers once it has published; a bind does not count
against the restart budget. A request that arrives while `devmgr` waits for a
driver to publish is kept, and answered after. `docs/SYSFS.md` §5 is the
kernel's half and the errnos.
