# sysfs — the device tree, fed by the services that own it ✅  ·  *26 points, spent*

Placed after stage 12 without a number of its own. `mount -t sysfs` was
`ENODEV` by design, and a boot check required it to be, until the customer
asked for sysfs on 2026-09-24 in the shape the rest of the system has: the
drivers are processes, `devmgr` matches and starts them, and the kernel's
cores accept what they publish. So sysfs is an in-kernel view like procfs and
cgroupfs, storing nothing, and every fact in it comes from whoever owns it;
a write to a driver's `bind` or `unbind` is a request to `devmgr`, which
decides. `docs/SYSFS.md` is the design; `docs/DEVMGR.md` §7 the protocol.

**Exit:** a boot check walks a whole sysfs on all three architectures, after
`devmgr` has started its drivers, and holds it against enumeration, the
cores and `devmgr`; and from a shell, what libdrm reads to find a card is
there, and the card's driver unbound and bound through sysfs goes and comes
back.

**Done (2026-09-24, 26 points).** In one landing:

* `src/lib/fs/common/sysfs`, every format and parse, pure, host-tested against Linux's
  and fuzzed (`sysfs_names`): PCI identifiers, `modalias` and `uevent`,
  processor lists, input bitmaps in words of the kernel's `long`, connector
  names, kernfs's relative links, and the name a `bind` write gives.
* The view, `src/kernel/src/fs/sysfs.rs`: `devices` with PCI roots, functions
  nested behind their bridges, `platform` for device tree nodes, `system/cpu`
  and `virtual`; `bus/pci` and `bus/platform` with `devmgr`'s drivers;
  `class` for block, drm, input, mem, net and tty; `dev/char` and
  `dev/block`; `block`; and `fs/cgroup` to mount cgroup2 on. Mounted on
  `/sys` at boot and again at the pivot; `/proc/filesystems` lists it.
* The tie from each published object to its device node, which no core had:
  a disk's registration records its node and the serial its driver
  reported, a net ring its interface's node, a card, a render node and an
  input device theirs. Enumeration keeps the revision, the subsystem ids and
  each bridge's secondary bus.
* `devmgr` announces its drivers and reports each binding (DRIVER, BOUND,
  UNBOUND), watches its channel for BIND and UNBIND, and answers with DONE:
  an unbind stops the driver and quiesces the device, a bind starts it as at
  boot.
* The boot check (FX-0890), the last one: 447 names in 90 directories and
  105 links on a desktop-shaped x86-64 machine, 13 device nodes, 7 bound as
  `devmgr` says; 37 nodes on ARMv7-A, its 32 virtio-mmio transports on the
  platform bus. A sabotaged `vendor` fails it with FX-0890. The stage 8 call
  check mounts sysfs by syscall number; `devpts` is its `ENODEV` now.
* `cargo xtask test-sysfs`, x86-64: a shell reads the card's vendor and
  device through `/sys/dev/char/226:0/device`, its `drm` directory, a
  connector, an input device, `eth0`, `vda` and the processors; a write to a
  value is refused at `openat`; the GPU's driver is unbound and bound
  through sysfs, the card gone and back, and a second of each refused.

**Still to do**, none of it on a path anything needs yet (`docs/SYSFS.md`
§6): uevents over `NETLINK_KOBJECT_UEVENT` (3 points), a function's
`resource` (2) and `config` (3), `/sys/firmware/devicetree` on the DK1 (3),
a processor's `topology`.

---

