# Stage 10 — Userspace drivers ✅

ACPI and device-tree enumeration in the kernel; IOMMU domains (VT-d, AMD-Vi,
SMMUv3); `devmgr`; the shared-ring block protocol; and the first driver —
virtio-blk — as a user process.

**Exit:** a boot test that reads a sector from a virtio disk through a driver
running in ring 3, with the IOMMU on and a deliberate out-of-domain DMA
attempt faulting — on x86-64, through VT-d, and on AArch64, through the
`SMMUv3`. The check itself clears VT-d's single fault record before it probes,
and requires every Arm domain to map the `GICv2m` doorbell, since QEMU sends
a device's MSI writes through the SMMU.

**ARMv7-A runs its drivers in degraded trusted mode.** U-Boot 2025.10 resets
when a virtio device offers `VIRTIO_F_ACCESS_PLATFORM`, so the 32-bit
machine's devices bypass its `SMMUv3`: a ring-3 driver there can DMA anywhere
in physical memory, the kernel prints `degraded trusted mode` on the console
at the first pin into an untranslated domain (`docs/ARCHITECTURE.md` §7), and
the exit's out-of-domain fault is checked on x86-64 and AArch64 only. A driver
reading sectors through an untranslated domain may land before then, so stage
11 can go on, but the stage is not done until both 64-bit machines translate
and fault.

**Done — configuration space, ahead of the kernel code that reads it.**
Started while stage 9 is still under way, because the exit criterion needs
stage 9's objects but most of what stands between here and it does not.

* `src/lib/platform/pci` — configuration space as arithmetic over a `ConfigSpace` the
  caller implements, answering as hardware does: all ones for a function that
  is not there. `#![forbid(unsafe_code)]`, no allocation, no recursion.
  * **ECAM**: the window geometry, and an adapter that turns "read these bytes
    at this offset" into configuration space, so the kernel's half is its
    volatile access and nothing else.
  * **Headers**, types 0 and 1, and the class code.
  * **BARs**, decoded and sized. Sizing is where the two classic mistakes live
    — probing a BAR while its function still decodes, so it briefly answers
    for whatever sits at the top of the address space, and sizing or
    restoring only the lower half of a 64-bit BAR — and neither is visible on
    a machine with one device. The tests' fake bus records the first and
    compares every register before and after for the second.
  * **Both capability lists**, walked with a visited set over every offset the
    list could use, so a list that points back at itself ends in an error, not
    a kernel that never finishes enumerating. MSI-X decoded from its
    capability.
  * **The bus walk**: every function reachable from a root bus through bridges
    firmware numbered. A bridge whose numbers make no sense — not above its
    own bus, outside the window, already claimed — is reported and skipped,
    and no arrangement of bridges scans a bus twice. Bus numbers are
    firmware's and are not renumbered, because the number is also where the
    function sits in the ECAM window and in the IOMMU's tables.
  * **virtio's PCI transport**: the vendor capabilities that say where in its
    BARs a modern virtio device keeps each register block, and a check that
    each block fits the sized BAR it names — the kernel will map exactly those
    blocks into a driver, and a block that overhangs its BAR would map
    whatever is next to it.
* The `pci_walk` fuzz target builds configuration spaces from its input and
  requires every walk to end, every function to be found once, and sizing to
  restore every register without probing a decoding function; 5.1 million runs
  found nothing when it landed. Miri runs the host tests in CI.

**One bug, found in review before anything ran.** The first BAR sizing
required the bits that stuck to run all the way to bit 63. A device with a
64-bit BAR that decodes only 40 bits of address reads back zeros above bit 40
— which the specification allows and Linux's sizing expects — and would have
been refused as malformed. The
check now requires one contiguous run, and a test sizes a 40-bit decoder.

**Done — enumeration, in the boot test on all three architectures.**

* **Where configuration space is.** `src/lib/platform/acpi` reads the MCFG, `src/lib/platform/fdt` the
  `pci-host-ecam-generic` nodes. The two disagree about what their address
  means: an MCFG allocation's is where bus *zero* would be, whatever bus it
  starts at, and a device tree's `reg` is its first bus's. Both parsers hand
  over the first bus's, so `src/kernel/src/discovery/pci.rs` never has to remember which it
  read. Where the loader handed over ACPI tables the MCFG is authoritative;
  otherwise the device tree is read. A `bus-range` larger than its window is
  cut to what the window holds, as Linux does.
* **A bus at a time.** An ECAM window is a megabyte per bus, and every machine
  here describes 256 buses. Mapping all of it would take 256 MiB of address
  space — more than half the 32-bit kernel's arena — to reach a handful of
  functions on bus zero, so a bus's megabyte is mapped the first time the walk
  reads from it, and every window is given back afterwards.
* **The check** walks every host, sizes every BAR, walks both capability lists
  of every function and finds its virtio transport. It fails if a described
  host answers with nothing, if a bus cannot be mapped, or if `src/lib/platform/pci`
  refuses anything a device presents. A machine that describes no host passes
  and says so, because the board has no PCI at all.
* **A virtio device on every test machine.** `virtio-rng-pci`, because it needs
  no backend and nothing depends on it, so the check meets a 64-bit BAR,
  MSI-X and virtio's vendor capabilities rather than only host bridges.

The run recorded when it landed: on x86-64, 6 functions from the MCFG with 8
BARs sized, 8 capabilities and one virtio transport; on AArch64, 2 functions
from the MCFG with 3 BARs, 6 capabilities and one virtio transport; on
ARMv7-A the same 2 from the device tree, reaching a window at
`0x40_1000_0000` — above 4 GiB, through LPAE.

**Done — device nodes, and the only way to name a device's memory.**
`src/kernel/src/device.rs`, written to the contract agreed with stage 9: its
`IoMapping` and `Interrupt` take an `Aperture` and a `Vector`, and only this
module can make either — from a sized memory BAR, from a `virtio,mmio` node's
`reg` and GIC interrupts, or afterwards from `DeviceNode::aperture`, which
answers only for a range inside *one* of the device's apertures. "Nothing
outside it" is then a type rather than a comparison a handler can forget.

* **The device tree is an allowlist** — `virtio,mmio` only — because most
  nodes with a `reg` are devices the kernel drives itself, and a node for the
  console would let a driver map it. An aperture overlapping the boot
  framebuffer is withheld; on x86-64 that is the display adapter's BAR, which
  is where a panic is drawn.
* **Not every aperture is whole pages.** QEMU packs its 32 virtio-mmio
  transports 0x200 bytes apart, several to a page, so a page mapping of one
  would hand a driver its neighbours' registers. An `Aperture` says whether it
  is whole pages, and `IoMapping` is to refuse one that is not. On QEMU's Arm
  machines that makes virtio-pci the transport a ring-3 driver can be given.
* **The check** asks every node for each aperture, its last byte, a range
  across each edge, an empty range and one that wraps the address space, and
  for every vector and the one past the end, before anything is published.

The run recorded when it landed: x86-64 publishes 6 nodes with 4 apertures, 1
withheld, and 24 refusals as specified; AArch64 2 nodes with 2 apertures;
ARMv7-A 34 nodes — its 32 virtio-mmio transports among them — with 34
apertures, 32 of them not whole pages, 32 edge-triggered vectors and 170
refusals, at four processors and at two.

**Done — a device driven by DMA, from the boot check.** The kernel does not
drive devices, but it owns everything a driver stands on — the BAR mappings,
the capability locations, and the physical memory a device reads and writes —
and enumeration proves none of that, because it only reads configuration
space. So once at boot `src/kernel/src/discovery/pci/virtio.rs` plays driver for the
simplest device there is, virtio-rng: it maps the common and notification
blocks the capabilities name, turns on bus mastering, gives the device a queue
in a page of its own, asks for 64 bytes, and requires the device to write them
into the page whose physical address it was given. Then it resets the device
— before the pages are freed, so the device holds no address into memory that
is given back — and restores the command register.

* `src/lib/drivers/virtio` gains the PCI transport's common configuration: the status
  protocol, feature negotiation and queue activation, host-tested against a
  device that behaves as virtio 1.2 §4.1.4.3 says. A reset that never finishes
  times out rather than hanging, a device that drops `FEATURES_OK` has refused,
  and `FAILED` is set before either error is returned.
* This is also the harness the next two items need. MSI-X is proven when this
  completion arrives as an interrupt rather than by polling, and an IOMMU
  domain when a descriptor pointing outside it faults.

The run recorded when it landed: 64 bytes read by DMA on x86-64, AArch64 and
ARMv7-A, at four processors and, on ARMv7-A, at two.

**Done — MSI-X tables kept out of a driver's reach.** An MSI-X interrupt is a
write the device makes, to an address and of a value the kernel put in a table
in one of the device's own BARs — so whoever can write the table chooses which
interrupt the device raises, the kernel's included. A PCI device node now cuts
the pages holding its MSI-X table and pending-bit array out of whichever BAR
holds them, mints apertures only from what is left, and records the withheld
ranges; the boot check requires every one of them, and its first and last
byte, to be refused. virtio-rng keeps its table in a BAR of its own, which
therefore stops being an aperture at all.

* `src/lib/platform/pci::msix` is the arithmetic — the withheld and mappable ranges of a
  BAR, rounded out to pages, and the table entry layout — and the messages the
  table will be programmed with: the local APIC's on x86-64, and a `GICv2m`
  frame's on the Arm machines, read from QEMU's own `arm_gicv2m.c` rather than
  remembered — `MSI_SETSPI_NS` takes the GIC identifier itself, and
  `MSI_TYPER` reports the first identifier and the count. The `pci_walk` fuzz
  target now also requires a BAR's mappable and withheld ranges to partition
  it exactly, with every withheld range on page boundaries.
* `src/lib/platform/acpi` reads the MADT's GIC MSI frame entries, and `src/lib/platform/fdt` the
  `arm,gic-v2m-frame` nodes, which is where those frames are described.

The run recorded when it landed: x86-64 publishes 3 apertures where it had 4,
with one MSI-X range withheld; AArch64 and ARMv7-A publish one PCI aperture
each where they had two.

**Done — what a review found.** A read-only review of stage 10 found ten
defects, and all ten are fixed. Two mattered most. A common Intel chipset would
have stopped the boot, because every function's vendor capabilities were read
as virtio's. And an aperture could be minted over RAM, which stopped being
theoretical the day `IoMapping` began mapping apertures for drivers.

* **Apertures are screened against everything the kernel owns:** the memory
  map (less firmware's own MMIO descriptions), every ECAM window, every
  controller window the kernel mapped for itself — `vmap` now records their
  physical addresses — the device tree's console, the framebuffer, and every
  other node's apertures. A function with memory decoding off contributes
  none. The publish check sorts every aperture and compares neighbours, which
  replaces a check that repeated the minting condition and so could not fail.
* **Device tree vectors** are shared peripheral interrupts only, never a line
  the kernel registered, never one another node holds.
* **virtio**: only a virtio device's vendor capabilities are read as virtio's;
  its blocks must lie in memory BARs, and every transport is verified. Freeing
  a virtqueue chain refuses a descriptor already free, since the device can
  move a link between validation and the free.
* **Hosts** whose segment and buses overlap one already accepted are refused;
  a device tree host without `linux,pci-domain` gets a segment of its own.
* **Mapping** refuses a physical range wider than the encoding's descriptors
  hold, which would otherwise have been masked onto low memory.
* **The entropy check** halts the boot only on a completion that cannot be
  right. A device that refuses, stalls or will not reset is reported and
  skipped — libvirt adds a virtio-rng to every guest, and a rate-limited
  backend is not the kernel's fault — and one that will not reset keeps bus
  mastering off and its pages out of the allocator. `xtask test-boot` fails a
  boot that reads no entropy, so on the machines it configures the regression
  is still caught.

One consequence, recorded rather than hidden: EDK2 on AArch64 leaves memory
decoding off for a function no firmware driver binds, so virtio-rng there now
publishes no aperture. A driver for such a device waits on the item below.

**Done — interrupts by message, on every architecture.** The boot check's
virtio-rng request now completes by MSI-X rather than by polling: table entry
0 is programmed with a vector the architecture allocated, the queue is told
to use it, and the used ring is read only once the vector has been delivered
to a kernel handler. That proves the whole message path — table, controller,
vector, dispatch — on each machine, and `xtask test-boot` fails a boot where
no completion arrived by interrupt.

* `arch::msi_allocate` hands out an interrupt number with the address and
  data a device writes to raise it: a local APIC vector from 0x40 to 0x7F on
  x86-64, below the legacy system call gate and the kernel's own vectors, and
  an SPI from the `GICv2m` frame on the Arm machines, found through the MADT
  or the device tree and sized from its `MSI_TYPER`. A `GICv2m` write is a
  pulse, so its SPIs are made edge-triggered; a level-sensitive line loses it.
* **One bug, and only the U-Boot machine could show it.** A GICv2 delivers a
  shared interrupt only to the CPU interfaces its `GICD_ITARGETSR` byte names,
  and QEMU resets that byte to zero on a multiprocessor GIC. The driver had
  never set one, because every interrupt the kernel used before was
  per-core. EDK2 routes what it touches, so AArch64 worked; U-Boot does not,
  so on ARMv7-A the device completed and the interrupt went nowhere. A shared
  interrupt with no target is now given the enabling core's own bit, read from
  the banked target bytes as Linux does.
* The check's vector and its handler are kept for the life of the machine:
  `irq` cannot take a handler back out.

The run recorded when it landed: one completion by MSI-X, 64 bytes, on x86-64
and AArch64 at four processors and on ARMv7-A at four and at two.

**Done — PCI vectors a driver can be given.** A PCI device node's vectors are
its MSI-X table entries. `DeviceNode::vector(i)` mints entry *i*'s the first
time it is asked — from `arch::msi_allocate`, programmed into the entry with
the entry masked — and hands out the same vector every time after. Minting
waits for the ask because a vector is spent for good and a machine has 64 to
give devices. The first mint on a function masks every entry and turns MSI-X
on; it does not turn bus mastering on, which belongs to whatever gives the
device DMA.

* **Masking says where.** A `Vector` carries whether it is a controller line or
  an MSI-X entry, and `Vector::mask` and `unmask` write the entry's own mask
  bit for the second — which the interrupt controller cannot reach — without a
  lock, since the interrupt handler calls them. Stage 9's `Interrupt` now
  masks and unmasks through them, agreed with that stage; its handler finds
  the object before masking, because only the object's vector knows where.
* **The check** mints one function's first entry after publishing — minting
  needs the node's place in the published list — and requires the same vector
  on a second ask, no handler already on its number, an entry that reads back
  masked and then unmasked and masked as told, and nothing past the table.
  Stage 9's interrupt check then claims that vector through `interrupt_create`,
  which on x86-64 it could not do before: the machine had no device vector.

The run recorded when it landed: one MSI-X table and one vector minted on
x86-64 and on ARMv7-A at four processors and at two, and "1 interrupt held
from delivery to acknowledgement" on x86-64 for the first time. AArch64 mints
none: its virtio-rng has memory decoding off, which the next item is for.

**Done — where each device's IOMMU is, and virtio's DMA sent through it.**
`src/kernel/src/iommu.rs` finds every IOMMU firmware describes and places every
PCI function behind one, before any unit is programmed:

* on x86-64, by the DMAR's endpoint scopes;
* on AArch64, by following the IORT's root complex one mapping to its `SMMUv3`;
* on ARMv7-A, by the `arm,smmu-v3` node a host's `iommu-map` names by phandle.

What it cannot follow — a scope through a bridge, a mapping to a node that is
not there — is counted as unresolved, never as bypassing, because a bypassing
function is one no domain will ever be built for. Every unit's register block
is withheld from apertures.

* `src/lib/platform/fdt` reads SMMU nodes, phandles, and `iommu-map` with its mask. It
  masks the requester ID, then takes the first entry that matches, as Linux's
  `of_map_id` does. `src/lib/platform/pci` gives a function's requester ID.
* `src/lib/kernel/paging` gains the two tables a domain is made of: VT-d's second level
  and an `SMMUv3`'s stage 2. Both are three levels over 39 bits of I/O
  address, with bits checked against QEMU's walkers, and they run through the
  same walk tests as the processors' tables. `Mapper::pages_only` holds a
  table to 4 KiB pages for a VT-d unit without superpages.
* **QEMU lets virtio bypass the IOMMU** unless the device is made with
  `iommu_platform=on`. So the test machines' virtio-rng now is, with
  `disable-legacy=on`. The entropy check accepts `VIRTIO_F_ACCESS_PLATFORM`,
  which such a device will not run without. Until a domain is switched on,
  the translation is the identity, and the check reads its 64 bytes as
  before.
* **Not on ARMv7-A.** U-Boot 2025.10's virtio-pci driver fails a heap
  assertion and resets when a device offers `VIRTIO_F_ACCESS_PLATFORM`, with
  or without an SMMU, while the loader is still on boot services. So the
  32-bit machine's virtio-rng bypasses its SMMU, and an out-of-domain fault
  there needs another way in.
* `xtask test-boot` fails a boot that places no function behind an IOMMU, or
  leaves one unresolved.

The run recorded when it landed:

* x86-64 places its 6 functions behind the VT-d unit at `0xfed90000`.
* AArch64 and ARMv7-A place their 2 behind the `SMMUv3` at `0x9050000`.
* Every function arrives as its requester ID.
* With the legacy interface off, AArch64's virtio-rng comes up with memory
  decoding on, so that node now publishes an aperture and mints a vector.

**Done — the domain a device's DMA goes through.** `iommu::Domain` is what a
driver pins its DMA pages into and takes device addresses back from.
`Domain::pin(frames, flags)` gives each page its device address — not
necessarily contiguous — and `Domain::unpin` takes them back, refusing a pin
another domain took. Every device node has one, made the first time it is
asked for. No unit is programmed yet, so every domain is untranslated: the
device address is the physical address, the first pin announces the degraded
trusted mode `docs/ARCHITECTURE.md` §7 requires, and an unpinned frame may be
freed only once its device is known to be quiet. The VT-d and `SMMUv3` domains
go in behind the same two calls.

* **The entropy check gives its device the addresses its domain returns**
  rather than physical ones, so it is already the harness the translated
  domains will be proven on.
* **The boot check** pins two frames through a device node's domain, and
  requires one domain per node, each frame's address, a count that returns to
  where it started, and a refusal of an empty pin and of a pin unpinned by the
  wrong domain.
* **Agreed with stage 9 and stage 11:** `VMO_PIN` (0x1025) pins a range of a
  VMO — held by stage 9's `Vmo::hold` — into a device's domain, owned by a
  `Pin` handle that unpins before it lets the frames go, and 0x1026 writes the
  pages' device addresses. The block ring, drafted by stage 11 and reviewed
  here, copies data through one pinned data VMO and rings doorbells as port
  packets.

The run recorded when it landed: 2 pages pinned and unpinned and 2 refusals, and
the entropy check's 64 bytes through its domain, on x86-64, AArch64 and ARMv7-A.

**Done — VT-d translating, and a function behind it given a translated
domain.** On x86-64 the kernel programs the VT-d unit the DMAR describes before
PCI enumeration, so from the first DMA on, a function behind it reaches only
what its domain maps and a function with no domain reaches nothing.
`src/kernel/src/iommu/vtd.rs` drives the unit in legacy mode, checked against
QEMU's `intel_iommu.c`: a root table per unit, a context table per bus, a
context entry per function naming a domain identifier and its second-level
tables, and register-based invalidation of the context cache and the IOTLB
after every change the unit may have cached. A unit firmware left translating,
or one that needs write-buffer flushing, is left alone and says why.

* **`iommu::domain_for(function)`** attaches a function to the unit its DMAR
  endpoint scope names, and gives an untranslated domain where no unit
  translates. `DeviceNode::domain` and the entropy check both use it, so the
  entropy device's rings and buffer are now reached through VT-d.
* **A translated domain maps each pinned page at its own physical address.**
  Nothing but what was pinned is mapped, which needs no I/O address allocator
  and refuses frames above 39 bits. A failed pin unmaps what it mapped; a
  domain dropped with nothing pinned detaches and gives back its tables.
* **`mm::map_io`, `unmap_io` and `translate_io`** build IOMMU tables beside
  the kernel's, generic over `src/lib/kernel/paging`'s encodings, a page at a time, so a
  unit is never asked to walk a block.
* **The domain check** now also requires a translated domain to resolve each
  pinned page to its frame and to fault it again once unpinned.

The run recorded when it landed: on x86-64, 1 VT-d unit translating, the
entropy check's 64 bytes and its MSI-X completion through a translated domain,
and 2 pages pinned and unpinned through a translated domain with 2 refusals.
AArch64 and ARMv7-A still say degraded trusted mode.

**Done — pinning VMO pages for a device.** `VMO_PIN` (0x1025) pins whole
pages of a VMO into a device's IOMMU domain, and `VMO_PIN_ADDRESSES` (0x1026)
says at which addresses the device reaches them: the DMA grant
`docs/ARCHITECTURE.md` §7 gives a driver. Stage 9's `Vmo::hold` keeps the pages
where they are, and the pin handle — which carries `READ` and nothing else, so
it stays with the driver that made it — owns the hold and the domain's record
of the pages together.

* **Given back in order.** Closing a pin unpins it from the domain and makes
  the unit forget the pages before the holds are released. On an untranslated
  domain the device can still reach the frames, and nothing resets a device
  yet, so they stay held and the console says so the first time.
* **The rules.** The device handle needs `MANAGE`; the VMO needs `READ`, and
  `WRITE` unless the pin is `PIN_READ_ONLY`. A range off a page boundary,
  empty, past the VMO's end or with an unknown option is `INVALID_ARGS`; a
  page already pinned into a translated domain is `ALREADY_BOUND`.
* **The check,** among stage 9's device objects, pins two pages for a PCI
  function, and requires every refusal, the second page's device address to
  lead to the frame holding what was written through the VMO, and — on a
  translated domain — both pages unreachable once the pin is closed.

The run recorded when it landed: 2 VMO pages pinned and found at their device
addresses on every machine, through a translated domain on x86-64.

**Done — the `SMMUv3` translating under ACPI, and a function behind it given a
translated domain.** On AArch64 the kernel programs every `SMMUv3` the IORT
describes before PCI enumeration, so a function an IORT root complex sends to
one reaches only what its stage-2 domain maps, and one with no domain reaches
nothing. `src/kernel/src/iommu/smmuv3.rs` drives the unit, checked against QEMU's
`smmuv3.c` and `smmuv3-internal.h`: a linear stream table of 256 entries, every
entry valid and aborting until a domain is attached; stage 2 per attached
stream, with its own VMID, a 39-bit walk from level 1 over 4 KiB pages, 40
bits of output and faults recorded; the command queue for `CFGI_STE`,
`TLBI_S12_VMALL` and `SYNC`, polled; and the event queue on.

* **Every domain maps the MSI doorbell.** QEMU sends a device's MSI writes
  through its stream, where VT-d exempts them, so each `SMMUv3` domain maps
  the `GICv2m` frame's page at its own address, from `arch::msi_doorbell`.
  The entropy check's MSI-X completion now arrives through it.
* **One enum for both units.** A translated domain's map, unmap, flush,
  resolve and detach go through `Translation`, so `Domain::pin`, `unpin` and
  the domain check are the same code for VT-d and the `SMMUv3`.
* **One bug, and QEMU found it.** A stream table entry is sixteen 32-bit
  words; the first version wrote the stage-2 walk as if they were 64-bit, and
  QEMU stopped the machine the moment it read an entry whose `S2AA64` was
  zero.
* **Not on ARMv7-A.** Its device-tree SMMU is left alone: U-Boot keeps the
  32-bit machine's virtio devices from offering the platform's translation,
  so they would bypass it anyway — the exit criterion's degraded trusted mode.

The run recorded when it landed: on AArch64, 1 `SMMUv3` translating, the
entropy check's 64 bytes and its MSI-X completion through a translated domain,
2 pages pinned and unpinned through a translated domain, and 17 refusals in the
pin check, the refusal of a second pin into a translated domain among them.
x86-64 and ARMv7-A are as before.

**Done — a deliberate out-of-domain write, faulted on both 64-bit machines.**
The boot check's entropy device, once its request has completed through a
translated domain, is asked to write into a page its domain does not map, and
its unit must record the fault: VT-d's fault recording register on x86-64,
cleared before the probe because QEMU keeps one record and drops a second fault
from the same device, and the `SMMUv3`'s event queue on AArch64. The record
must name the device's own stream, the probe's page and a write. The answer is
the unit's record, not the device's completion: QEMU's virtio device completes
the request anyway, through a bounce buffer whose write-back the unit refuses
a second time (the item below, where this was learned), so a completion stops
the boot only when no fault is recorded for the probe's page by the deadline
— DMA its domain should have prevented. `xtask test-boot` requires the fault
on x86-64 and AArch64 and does not ask ARMv7-A, as the exit criterion says.

The run recorded when it landed: 1 out-of-domain write faulted on x86-64 and on
AArch64; ARMv7-A's untranslated domain was not probed.

**Done — both units' waits made with interrupts on.** A VT-d invalidation and
an `SMMUv3` command are waited for under a gate, `src/kernel/src/iommu/gate.rs`,
instead of inside `IrqSpinLock`s that masked interrupts for up to 100 ms on the
path every unpin takes. A task waiting to enter a domain's pins and unpins, or
a unit's commands, sleeps on a wait queue; the one inside polls the unit with
interrupts on and gives up its processor between looks. Only the few writes
that queue an `SMMUv3` command still mask them. Before the scheduler runs, or
in a context holding a spin lock, the gate and the wait spin as the locks did,
within the same deadlines, and an unpin that runs out of patience keeps its
frames rather than freeing what the unit may still reach. The domain check
requires a translated domain's unpin to have waited with interrupts on.

The run recorded when it landed: 10 waits on a unit with interrupts on by the
end of the domain check on x86-64, under KVM, and on AArch64; none on ARMv7-A,
whose domain is untranslated.

**Done — the block ring's protocol, as a library, host-side.** `src/lib/proto/blkring`
(`ferrix-blkring`) is the ring the kernel and a ring-3 block driver will share,
as `docs/BLOCK-RING.md` specifies it: the ring and data VMO layout, every index
and entry the other side writes checked before it is used, doorbells over stage
9 ports, and a HELLO carrying the disk's PCI location, its virtio serial and the
name `devmgr` chose, numbered as Linux numbers `vda`, `vdb`. When a driver ends,
every outstanding request fails at once, and the data VMO stays held until
`devmgr` confirms the device was reset (BLOCK-RING.md §6.3); no path in the
crate reports it releasable before that. It is pure logic, tested on the host,
under Miri, and by a fuzz target that plays one side of the ring against an
honest other. The ring's kernel side and the driver process that use it
came next, below.

**Done — the probe's answer is the unit's record, never the completion.** Once
in a few AArch64 boots the probe halted the machine with FX-1001, "the device
wrote into a page its domain does not map". It had not. QEMU's DMA map of an
address the IOMMU refuses does not fail: `address_space_map` hands the device a
bounce buffer, the device fills it and completes the request with the length it
was given, and the write-back at unmap is refused a second time and dropped —
read in QEMU 9.2's `system/physmem.c`, not remembered. So the unit records the
fault first and the device pushes a completion an instant later, every time, on
VT-d and on the `SMMUv3` alike. The check read the event queue, then the used
ring, and lost whenever the push landed between the two reads. A completion is
now held until the deadline and fails the boot only if no fault for the probe
page has been recorded by then; after the fault the probe waits a moment for
the completion, and the boot log says whether the device completed the faulted
write, whether the completion was seen before the fault, and how many further
faults were recorded for it — the dropped write-back is one — so every boot
shows the mechanism rather than the one that lost the race.

The run recorded when it landed: nine AArch64 boots in a row, each faulting
the write and then completing it, 64 bytes never delivered, with 16 further
faults for the write-back refused in 4-byte pieces; three x86-64 boots and one
under KVM, the completion after the fault and no further fault, since VT-d
keeps one record; ARMv7-A unprobed at four processors and at two.

**Done — the block ring's kernel side, up to a published disk.**
`src/kernel/src/interfaces/block_ring` is the glue `docs/BLOCK-RING.md` §8 leaves to the
kernel. A process holding a device with `MANAGE` asks for a ring with
`block_ring_create` (0x1048) and is answered the driver's end of the ring's
control channel; the kernel's end goes to a task of its own per ring, which
waits for HELLO, checks it in §6.2's order — the crate's checks, then that
`location` is the ring's own device, then the registry — and refuses or takes
the ring up: it holds the ring and data VMOs, attaches the crate's
`KernelSide` over the ring's pages, publishes the disk through stage 8's devfs
registry under HELLO's name with the virtio-blk major and `index × 16`, and
answers READY with its completion port. From then on it serves reads (and,
since stage 12, writes and flushes):
`src/lib/fs/block`'s queue in front of the ring, one submission per dispatch into a
region of the data VMO the kernel allocates, the driver rung when it asked to
be, completions taken off the ring and copied out once, readers woken. A read
on a ring whose driver has gone answers `EIO` at once; the ring ends on
STOPPED, on the channel closing or on corruption, and its registration goes
with it. The kernel never serves a disk: it issues requests and copies
payloads, and whatever answers is the process at the other end of the ring.

* **One rule the first use found unwritable.** §2 said the HELLO handles carry
  exactly `READ | WRITE | MAP`, with no `TRANSFER`; but `channel_write` takes
  only a handle that carries `TRANSFER` and delivers it with its rights, so no
  HELLO could ever have passed. The rule now says what arrives: `TRANSFER` on
  every handle a process sends, `DUPLICATE` on none, and the completion port
  the kernel places itself with `WRITE` alone.
* **The check** drives the control plane from a process the way a driver
  will, every call through the native dispatcher: a ring refused without
  `MANAGE`, on a VMO and on a device that has one; a HELLO of another version,
  one with unreduced handles and one for another location each refused with
  its reason and the kernel's end closed; a HELLO as specified answered READY
  with a `WRITE`-only port, `vda` at 254:0 in the registry with the geometry
  HELLO gave, and gone once the driver says STOPPED, after which the next
  round's ring finds the device free; all of it twice, the second round
  giving every frame back; then once more with the driver closing the channel
  instead,
  which takes the disk but leaves the device bound, since nothing reset it.
  No request is put on the ring: a sector read through it is the ring-3
  driver's check, below.

The run recorded when it landed: 13 calls and HELLOs refused as specified, 2
disks published and unpublished, 0 frames leaked, in about 50 ms, on x86-64,
AArch64 and ARMv7-A at four processors and at two.

**Done — virtio-blk's protocol and driver logic, as libraries, host-side.**
`src/lib/drivers/virtio`'s `blk` module is the device protocol: features checked against
Linux's header, the configuration, request headers and statuses, and a request
split into descriptor chains one pinned page at a time. `src/lib/drivers/block/virtio-blk` is the
driver's logic: bring-up to `DRIVER_OK`, read, write and flush, each completion
counted exactly once even from a hostile device, and a teardown that hands
memory back only after the device's reset has finished. Device addresses reach
it only through `DevicePages`: the addresses the pin query (0x1026) returned,
page by page of the pinned range, with no relation to physical addresses
assumed. It is tested on the host, under Miri, and by the `virtio_blk` fuzz
target. The driver process on `ferrix-rt` that uses it came next, below.

**Done — the kernel's half of `devmgr`: what a driver is started with, and
what ends it.** `docs/ARCHITECTURE.md` §7 has `devmgr` hand a driver its device
and its channel to the subsystem it serves; a ring-3 driver cannot walk
configuration space, so whoever starts it must say where its device's
registers are. Enumeration now keeps, on the device node, what it read and
threw away before: the PCI identity, the virtio transport's register blocks as
physical memory — the page-aligned pages holding each, the block's offset in
them, its length, the form `io_mapping_create` takes — the MSI-X table size
and the function's configuration space address. From that, `device_info`
(0x1049) writes a `DeviceInfo` for any device handle, `block_ring::start_for`
builds the START message `docs/BLOCK-RING.md` §6.4 now specifies — type 6, the
first message on a driver's bootstrap channel, carrying the blocks, the
location, the name, the device with `MANAGE` and the driver's end of the ring's
control channel — and `ferrix-blkring` encodes, decodes and checks it. Two more
things nothing did before:

* **Bus mastering goes on at a device's first `VMO_PIN`**, the moment a driver
  gives it memory, and memory decoding with it; until then a device nobody
  drives stays quiet. No kernel code had ever enabled it for a driver, so a
  ring-3 driver's DMA would have gone nowhere.
* **`device_quiesce` (0x104A)** is the reset before release §6.3 gives
  `devmgr`: with `MANAGE`, once the driver is gone, it turns bus mastering off
  and releases the device's ring claim, so the next driver may have it;
  `BAD_STATE` while a driver still serves the device through a ring. A ring
  now records its device as served before it answers READY, since the driver
  may act on READY, and `devmgr` ask after the device, the instant it is sent.

The ring check covers it: `device_info` compared field by field with the
kernel's own START for the node, every virtio block required to lie inside one
of the node's apertures; a quiesce refused without `MANAGE`, refused under a
serving driver, and, after a driver dies, freeing the device for a new ring.
The P1 row for reset on driver death closes here: §6.3 is the design and this
its kernel side. `devmgr` itself, the program, came after the exit, below.

The run recorded when it landed: 19 calls and HELLOs refused as specified, 2
disks published and unpublished, 0 frames leaked, on x86-64, AArch64 and
ARMv7-A at four processors and at two.

**Done — the exit: a sector read through a driver in ring 3, with the IOMMU
on.** `/sbin/blk` (at `/lib/drivers/blk` since `devmgr` landed), stage 11's virtio-blk driver on the native runtime
(`src/user/system/native/drivers/block/virtio-blk`, over `src/lib/drivers/block/virtio-blk` and `src/lib/drivers/block/blkserve`), is started from the
boot check by a kernel-driven parent with the START `devmgr` will send
(`docs/BLOCK-RING.md` §6.4, from `block_ring::start_for`): the device with
`MANAGE`, and the driver's end of the ring's control channel. It maps the
transport's blocks through `IoMapping`s, claims its MSI-X entry through an
`Interrupt`, pins its data VMO into the device's domain — which is where bus
mastering goes on — brings the device to `DRIVER_OK`, sends HELLO, and serves;
the kernel reads sectors through the registered disk, `vda`, and compares them
with what `xtask` wrote into the test disk. On x86-64 that DMA goes through
VT-d translating and on AArch64 through the `SMMUv3`, and the deliberate
out-of-domain write faulted on both earlier in the same boot; on ARMv7-A the
driver runs in degraded trusted mode, as the exit criterion decided. The
disk is `virtio-blk-pci` on every machine, because under ACPI QEMU describes
its virtio-mmio devices only in AML. The marker moves to `FERRIX-BOOT-OK
stages 1-10` when it landed; what the stage still owes is below, after the exit, each with a
row in `docs/BACKLOG.md`.

The run recorded when it landed: `/sbin/blk serves vda (131072 sectors)
through the block ring; 21 sectors read back through the registry as xtask
wrote them`, on x86-64 through VT-d, on AArch64 through the `SMMUv3`, and on
ARMv7-A in degraded trusted mode, at four processors and at two.

**Done — `devmgr`, the program.** `/sbin/devmgr` on the native runtime is
what `docs/DEVMGR.md` says: started by the kernel after the boot checks with
a bootstrap channel already holding DEVICES — a job, every device node
twice, and every driver image the initramfs lists in `/lib/drivers/MANIFEST`,
read by the kernel into anonymous VMOs, since a native program reads no
files — it asks `device_info` of each device, matches virtio-blk by a table
of its own, makes the ring, starts `blk` from the image in a job of its own
with START, and waits for the kernel's PUBLISHED before the next, so disks
register in PCI order and no two drivers race to be `vda`; then it REPORTs,
the kernel prints the line and `xtask` requires it, and from then on a
driver's death reaches `devmgr`'s port, which quiesces the device (retrying
a `TIMED_OUT`, never a `BAD_STATE`) and tells the kernel DIED. A channel
message carries 64 handles and ARMv7-A publishes 36 device nodes, so the
kernel sends DEVICES in as many messages as the handles need, each saying how
many devices are still to come. The boot check's own starter now runs only
when the image carries no `devmgr`; with it, the driver check reads through
the disks `devmgr`'s drivers serve. `src/lib/proto/devmgr-proto` is the protocol's
crate, host-tested. The table has since grown to five kinds — virtio-blk,
virtio-net, virtio-gpu, virtio-input and the virtio-serial port driver
`vport` — each handed its own subsystem's channel, and every driver but the
port driver, which publishes to no subsystem, is waited for with PUBLISHED.
What the stage still owes is below.

The run recorded when it landed: `devmgr   8 devices, 1 drivers, 2 started,
0 failed` on x86-64, 4 devices on AArch64, 36 on ARMv7-A at four processors
and at two, each followed by the driver check's sectors read back through
the disks `devmgr` started.

**FX-1004, the block ring check's flake, fixed on 2026-09-24.** Quiescing a
device the instant its driver's channel closed was refused, about once in a
few dozen loaded boots on x86-64 and AArch64: `object::dispose` queued a
closed channel end whole, and a close made while another processor drained
that queue returned with the end still open. A channel end now closes in
`dispose` itself and only what its unread messages carry is queued; the ring
check and the object check both close under a held drain now, and fail every
boot without the fix. The wake change c2129a68, once the suspect, was not it.

**Still to do, after the exit.** None of it was on the path to `rustc`.

* **AMD-Vi**, which the stage's scope names beside VT-d and the SMMUv3: nothing
  reads an IVRS table or drives an AMD IOMMU yet, so on such a machine DMA
  would not be translated.

* **Trusting a BAR firmware placed but did not enable**, so a device no
  firmware driver used — as virtio-rng on AArch64 was until its legacy
  interface was turned off — can still be given to a ring-3 driver. Worked
  out with the review that found the gap. **Started on 2026-09-19:** the
  device-tree half of the first bullet has landed — `src/lib/platform/fdt` reads a host
  bridge's `ranges` into `PciWindow`s that keep the bus and the CPU address
  apart, refusing a BAR only half inside a window, an empty or wrapping range,
  and a node whose cell counts are not PCI's — and the rest below is untouched:
  * *Where the windows are.* The device tree's `ranges` on the Arm machines.
    Under ACPI they are in `_CRS`, which is AML — but before
    `ExitBootServices` the loader can ask each root bridge's
    `EFI_PCI_ROOT_BRIDGE_IO_PROTOCOL.Configuration()`, which returns the same
    windows as ACPI address-space descriptors, and carry them in `BootInfo`
    beside the MCFG. The descriptor layout is to be checked against the UEFI
    specification and EDK2's `PciHostBridgeDxe`, not remembered.
  * *Bus address, not CPU address.* A BAR holds a PCI bus address; match it
    in bus space and build the aperture from the translated CPU address.
    QEMU's `virt` translates by zero, which hides a missing translation.
  * *The window's kind.* A 32-bit memory BAR only in a 32-bit memory window,
    never a memory BAR in an I/O one; a prefetchable BAR may use a
    non-prefetchable window, not the reverse.
  * *The whole BAR*, `[base, base + size)`, inside one window, and inside the
    forwarding window of every bridge upstream of it, each of them decoding.
  * *Order.* Decoding-on BARs are admitted to the overlap set first, so a
    stale decoding-off assignment cannot block a live device.
  * *When decoding goes on:* at `IoMapping` creation, not at enumeration, so
    a device nobody drives stays quiet. The boot check's entropy read turns it
    on today, and until this lands it refuses a register block that overlaps
    memory the kernel owns rather than vetting it against the windows.
  * *Unassigned BARs* — zero, or a reset value — fall outside every window,
    and are reported as unassigned rather than as outside one.
  * Later, on the device tree machines: assign addresses to decoding-off
    functions inside the windows, as Linux does unless `linux,pci-probe-only`
    is set, rather than depend on firmware's choice.

**Landed after the exit (2026-09-22): a display driver that dies is started
again** (`docs/DEVMGR.md` §4). It began as a bug: `kill -9` of `gpu` under
the desktop took card0 away for good, hyprix ended on `ENODEV`, and it was
init, so the machine powered off. The kernel had not crashed. Now
`device_quiesce` also waits for the display and render cores to let a dead
driver's device go (`src/kernel/src/claim.rs`). Cards and render nodes are
numbered lowest-free, so the card returns as `card0`. devmgr keeps its device
handle with `DUPLICATE`, starts the driver again on a duplicate (eight times
a device at most), and tells the kernel RESTARTED. hyprix treats `ENODEV`, or
its card's descriptor reading 0, as a lost screen and draws on the card again
when it is back. Gates: `cargo xtask test-restart` (a shell kills the driver
twice) and `cargo xtask test-compositor --boot restart` (a script kills it
twice under hyprix, and the screen must then be the tiled picture, every
pixel). A sound driver is started again the same way (2026-09-26), gated by
`test-audio`'s restart boot. A dead driver's pins on a translated domain stay
mapped, their frames held, until the device's core accepts the next driver's
HELLO (`src/kernel/src/object/pin.rs`, finding F-38): QEMU writes a dead
driver's buffers late, and those writes must not reach frames handed on.
Network, input and disk drivers are started again too (2026-09-27, T0 of
the live kernel update plan): a network interface is parked with its
addresses, and a disk with its requests queued, for the next driver to take
up, so a btrfs root survives every disk driver being killed. Gated by
`cargo xtask test-restart --boot all`. **Still to do:** the serial port,
USB host, GPU engine and gadget kinds; a GPU renderer that comes back, since hyprix
draws in software after a restart; and a card with two connectors, whose
screens would each try to reopen it.

**Landed after the exit (2026-09-29 to 2026-09-30): discovery behind one
trait, and a runtime the ring-3 drivers share.** Neither changes what a boot
finds or what a driver does; both change where the code is and what it has to
repeat.

* *Where things are.* The kernel's interface cores (audio, block ring,
  display, input, logctl, net ring, render) moved to
  `src/kernel/src/interfaces/` (b22841d7), since they are what a driver talks
  to, not drivers; and `acpi`, `fdt`, `pci` and `devmgr` to
  `src/kernel/src/discovery/` (aa78061e), with the board registry beside them.
* *ACPI or the device tree, decided once* (fa4e88a6): `discovery::description`
  opens ACPI's tables and falls back to the device tree, in the host-tested
  `src/lib/platform/description` (`L.discovery.1`), where each caller used to
  decide for itself.
* *The `Finder` trait* (cc4e14af, evidence re-carried by 65639967): PCI
  enumeration, the device tree's virtio-mmio nodes and the board registry are
  each a `Finder` that says what it reads and what it failed on, and
  `device::publish` runs them once, in that order, stopping the boot on the
  first failure. Two boot lines, `nodes` (the published order's digest) and
  `reserved` (the reserved ranges' count and digest), exist so the next
  discovery change can be compared with this one (`reserved` is d5896c75).
  The steps landed without a recorded review; the certification consultant
  (os-ad) reviewed them after the fact on 2026-10-01 (da45a113), and its
  conditions are a row of `docs/BACKLOG.md`.
* *`ferrix-driver`* (`src/user/system/native/driver`, d5b03142): START, register
  blocks, DMA memory, the virtio transport, and a subsystem module per class
  (`input`, `block`), which a driver implements a trait of. Matching is the
  device type, not an ID table. DMA memory has no `Drop` and frees only with a
  `Stopped` the transport makes after it has seen the device reset, so a free
  before the reset does not compile. virtio-input (642 to 117 lines) and
  virtio-blk (841 to 182 lines, 31b16d72) are on it.

**Still to do:** the other seven drivers onto `ferrix-driver` (net, snd,
vport, gpu, then stm32-ltdc, gc400, usbhid, usbdev; `docs/BACKLOG.md` P2);
a graceful STOP, since no kernel code sends one to a real driver and every
driver's stop path is unreachable (`docs/BACKLOG.md`, the desktop's table,
which needs the customer's word and os-9f's review); and five coverage
arguments the move dropped, to be re-applied at the next re-measure
(`docs/certification/TODO.md` §0.2).

---

