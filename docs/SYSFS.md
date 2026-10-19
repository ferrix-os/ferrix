# sysfs: the device tree, fed by the services that own it

Version 1. Written on 2026-09-24, when sysfs was built; the customer chose
the shape that day: an in-kernel view like procfs and cgroupfs, fed by the
services that own each fact, with `bind` and `unbind` going to `devmgr` to
decide. `docs/ARCHITECTURE.md` §7 and §8 are the architecture; `docs/DEVMGR.md`
§7 is the protocol it added.

## 1. What this is, and what it is not

It is `/sys` as Linux programs read it: `mount -t sysfs`, `/sys/devices`,
`/sys/bus`, `/sys/class`, `/sys/dev`, `/sys/block` and `/sys/fs/cgroup`,
in Linux's formats. libdrm finds a card's PCI identity through
`/sys/dev/char/226:<minor>/device` (`docs/GPU.md` §6), `lspci` lists
functions from `/sys/bus/pci/devices/*/uevent`, glibc counts processors in
`/sys/devices/system/cpu`, btop reads an interface's `statistics`, and init
mounts cgroup2 on `/sys/fs/cgroup` (`docs/INIT.md` C1).

It is **not** where any of that is decided. Ferrix's drivers are processes,
`devmgr` is the one that matches devices to drivers and starts them, and the
kernel's cores accept what the drivers publish (`ARCHITECTURE.md` §7). sysfs
keeps that split: it stores nothing, and every directory and file is computed,
when it is looked at, from whoever owns what it shows. A write that would
change something -- `bind`, `unbind` -- is not done in the kernel at all; it
is a request to `devmgr`.

It is also not a ring-3 filesystem. `ARCHITECTURE.md` §1 keeps filesystems in
the kernel, and a served filesystem would put an IPC round trip under every
`stat` of `/sys` and a hung server under every reader. The services feed the
kernel over the channels they already have; readers never wait on a service.

## 2. Who owns each fact

| What sysfs shows | Owner | How the kernel knows it |
|---|---|---|
| Device nodes, their bus, parent bridge, vendor, device, class, revision, subsystem ids | the kernel's enumeration | `device::devices()`, read once at boot; enumeration now keeps the revision, the subsystem ids and each bridge's secondary bus |
| Disks: name, number, size, sector size, `ro`, serial | the `blk` driver, through the block ring | the devfs registry; each registration now records its device node and the serial HELLO carried (`devfs::Origin`) |
| Interfaces: name, index, address, MTU, flags, counters | the `net` driver and the net core | the net core's stack, copied out; `net_ring::node_of` records the node each ring's interface came from |
| Cards, connectors, modes; render nodes | the `gpu` or `ltdc` driver, through the display and render cores | `display::card`, `display::drm::connectors` (the same function the connector ioctl uses), `render::renderer`; each records its node |
| Input devices: name, ids, capability bitmaps, serial | the `input` or `usbhid` driver, through the input core | the input session HELLO declared; each device records its node |
| Which drivers exist, and which drives which device | `devmgr` | DRIVER, BOUND and UNBOUND on its bootstrap channel (§5); `src/kernel/src/discovery/devmgr.rs` keeps what it was told |
| Processors: possible, present, online | the kernel | `smp::topology()` |
| `null`, `zero`, `tty`, `console` and the other static nodes | the kernel | devfs's table |

The tie between a published object and its device node did not exist before
sysfs: each core knew its devices only by the channel they came on. It is one
index per object, recorded where the core accepts its driver's HELLO, and
nothing else reads it.

## 3. The tree

```
/sys
├── block/<disk> -> ../devices/.../block/<disk>
├── bus/
│   ├── pci/      devices/<slot> -> …   drivers/<driver>/{bind, unbind, <slot> -> …}
│   └── platform/ devices/<name> -> …   drivers/<driver>/{bind, unbind, <name> -> …}
├── class/
│   ├── block/  drm/ (and version)  input/  mem/  net/  tty/
├── dev/
│   ├── block/<major>:<minor> -> …
│   └── char/<major>:<minor> -> …
├── devices/
│   ├── pci<segment>:<bus>/<slot>/          vendor device subsystem_vendor subsystem_device
│   │   │                                   class revision modalias uevent subsystem driver
│   │   ├── <slot>/                         (a function behind a bridge, nested)
│   │   ├── block/<disk>/                   dev size ro removable range serial uevent queue/
│   │   ├── drm/card<N>/                    dev uevent card<N>-<type>-<M>/{status,enabled,modes}
│   │   ├── drm/renderD<N>/                 dev uevent
│   │   ├── input/input<N>/                 name phys uniq properties uevent id/ capabilities/
│   │   │                                   event<N>/{dev,uevent}
│   │   └── net/<interface>/                address addr_len broadcast carrier flags ifindex
│   │                                       iflink mtu operstate type uevent statistics/
│   ├── platform/<address>.<node>/          uevent subsystem driver, and its class devices
│   ├── system/cpu/                         online possible present offline kernel_max cpu<N>/
│   └── virtual/{mem,tty,net,block}/        the devices no node backs: null…, lo
└── fs/cgroup/                              empty, for cgroup2
```

Names follow Linux: a PCI function is its slot, `0000:00:02.0`; a device tree
node is its unit address and node name, `5a001000.display-controller`; a
connector is `card0-Virtual-1`. There is no `virtio<N>` layer between a
function and its class devices: Ferrix's drivers drive the PCI function
directly, so a card's `device` link leads to the function, whose `subsystem`
is `pci`, and libdrm reads the vendor and device it needs there.

Every link is relative, as kernfs spells it, so a sysfs mounted anywhere --
the boot check mounts one under `/tmp` -- still leads where it should.

Directories whose names never change once the machine is up -- `/sys`,
`bus`, `class`, `dev`, `devices`, `fs` and a few more -- let the VFS remember
their lookups, which is what lets cgroup2 be mounted on `/sys/fs/cgroup`.
Every directory that follows the drivers asks afresh on each walk, as `/proc`
and `/dev` do. Listings are in the order of a hash of each name and resume
after the last hash given, so a device coming or going between two
`getdents64` calls neither repeats nor hides a name that stayed. Inode numbers
are hashes of paths.

The kernel mounts a sysfs on `/sys` at boot and again inside the btrfs root
after the pivot, as it does `/proc` and `/dev`.

## 4. The kernel's half

`src/kernel/src/fs/sysfs.rs` is the view: a directory is a `Dir`, its names are
computed by `entries`, a file is rendered at open by `render`, and a link's
target is `path_of` the directory it names. `src/lib/fs/common/sysfs` holds every format
and every parse, pure, host-tested and fuzzed: the identifiers, the processor
lists, the uevent lines, the input bitmaps in words of the kernel's `long`
(64 bits on the 64-bit pair, 32 on ARMv7-A), the connector names, the relative
link targets, and the name a write to `bind` gives.

A read never blocks on a service and never takes a lock across rendering:
each core's state is copied out first.

## 5. `bind` and `unbind`

A driver's directory has two write-only files. What Linux checks before it
asks a driver is checked in the kernel: the name written must be a device on
the driver's bus (`ENODEV`), and for `unbind` the device must be this
driver's (`ENODEV`). Then the kernel sends `devmgr` a request and the writer
waits, bounded, for the answer:

```
writer            kernel                              devmgr
echo slot > unbind
  ── UNBIND{device, driver, token} ─────────────────▶ kill the driver's job
                                                      TERMINATED on its port
                                                      quiesce the device
  ◀────────────────────────────────── UNBOUND{device}
  ◀────────────────────────────── DONE{token, done}
write returns

echo slot > bind
  ── BIND{device, driver, token} ───────────────────▶ start the driver as at boot
  ◀──────────────────────── PUBLISHED{location} ──── (the core, on HELLO)
  ◀──────────────────────────── BOUND{device, driver}
  ◀────────────────────────────── DONE{token, done}
write returns
```

| DONE's answer | errno | when |
|---|---|---|
| done | 0 | bound, or unbound and quiesced |
| no device | `ENODEV` | `devmgr` never started a driver on it, or the driver does not take it, or (unbind) it has none |
| busy | `EBUSY` | (bind) the device has a driver |
| failed | `EIO` | the driver did not publish, or the device would not quiesce |
| no answer in 20 s | `ETIMEDOUT` | Linux has no such case; a write here cannot wait for ever |

`devmgr` decides everything past the name: whether its table matches the
driver to the device, whether the device is up, how to start it (the disk
name a `blk` driver had, the START a display driver had). An unbound driver
is not restarted by the death that follows; a bound one is started the way
`docs/DEVMGR.md` §4 restarts one, without counting against the restart
budget. Requests that arrive while `devmgr` waits for a driver to publish are
kept and answered after.

Whether a driver *can* come back is its core's business, as it is for a
restart: a disk's ring releases its claim at the quiesce, a display's does, and
the net, input and serial cores do not yet, so a bind there may answer `EIO`.
`cargo xtask test-sysfs` unbinds and binds the GPU.

## 6. What is left out, and what each would take

* **uevents.** The `uevent` files say what an event would carry; none is sent.
  A `NETLINK_KOBJECT_UEVENT` socket opens and joins its group since
  2026-09-30, so libudev's monitor is made, but nothing is ever queued on it
  (`src/kernel/src/net/netlink/mod.rs`). A write to `uevent` is refused
  rather than accepted and dropped. Sending one on each BOUND, UNBOUND and
  publication is a broadcast to those sockets and a few lines per core,
  3 points. One rule for it (os-9f, 2026-09-30): a program's send must never
  reach a listener looking like the kernel's -- sender port 0, root's
  credentials -- because udev trusts exactly that, and forged uevents are a
  known escalation (CVE-2009-1185). The `CAP_NET_ADMIN` refusal of a send
  stays, judged in the first user namespace once N4 lands, never by a child
  namespace's capability.
* **A PCI function's `resource`, `config`, `irq`, `enable`.** Enumeration keeps
  apertures, not BARs by index, and nothing maps configuration space after
  boot. `resource` is keeping the sized BARs, 2 points; `config` is a read of
  configuration space on open, which `tools/common/check/check-device-access.py` must be
  told about, 3.
* **A processor's `topology/`.** Firmware's tables do not say siblings and
  packages reliably enough to print; `cpufreq` has no source at all.
* **`/sys/kernel`, `/sys/power`, `/sys/module`, and `/sys/firmware` but for
  `dmi/tables`** (and `/sys/class/dmi/id`, which [DMI.md](DMI.md) adds
  while firmware gave an SMBIOS table the kernel accepted). Nothing true
  to put there yet. `/sys/firmware/devicetree/base` on the DK1 is the flattened
  tree as files, 3 points, when a program needs it.
* **Writable attributes other than `bind`/`unbind`**, and `chmod`/`chown` in
  sysfs: every other file is read-only, and a mount with `ro` is not enforced
  on `bind`/`unbind`.
* **A `virtio<N>` bus layer** between a function and its class devices, which
  Linux has and Ferrix's drivers do not.

## 7. How it is checked

* **`src/lib/fs/common/sysfs`'s host tests** pin each format against what a Linux printed:
  a virtio-gpu's `uevent` and `modalias`, a keyboard's `capabilities/ev` in
  64- and 32-bit words, `DEVMODE`'s octal, kernfs's link spelling to an
  ancestor.
* **`src/tests/fuzz/fuzz_targets/sysfs_names.rs`**: every parse round-trips, a written
  name stays inside what was written, and a link never climbs above its mount.
* **The boot check**, the last one, after `devmgr` has started its drivers
  (`src/kernel/src/fs/sysfs/check.rs`, FX-0890): a sysfs under `/tmp` walked whole,
  three names a listing so every listing resumes; every name looks up as the
  kind and number listed, every file reads, every link leads to a directory in
  the mount; then every device node is on its bus with enumeration's ids, every
  bound device's `driver` link and `uevent` say what `devmgr` said, every disk
  has its size, `ro`, number and node, `lo` is interface 1, processor lists are
  firmware's, `/sys/dev/char/1:3` is `null`, writes, `mkdir`, `unlink` and
  creation are refused as kernfs refuses them, and cgroup2 mounts on
  `fs/cgroup`. It prints the `sysfs` boot line. The stage 8 call check mounts
  and unmounts a sysfs by syscall number, and `mount -t devpts` is its `ENODEV`.
* **`cargo xtask test-sysfs`**, x86-64: a shell beside a card, a keyboard, a
  tablet and a network adapter reads what libdrm reads, a connector, an input
  device, the adapter, a disk and the processors; a write to a value is
  refused at `openat`; the card's driver is unbound and bound through sysfs,
  with the card gone and back, and a second of each refused.

## 8. The landing

One landing, 26 points, measured in the points ledger:

| part | points |
|---|---|
| `src/lib/fs/common/sysfs`, its tests and fuzzer | 5 |
| The view and its boot check | 8 |
| The cores' tie to their device nodes; enumeration's revision, subsystem ids and bridges | 3 |
| `devmgr`: DRIVER, BOUND, UNBOUND, BIND, UNBIND, DONE, both ends | 5 |
| `cargo xtask test-sysfs` | 3 |
| This document and the rest | 2 |
