# Drivers without kernel changes

**Status: design, 2026-10-25. Not built.** The customer's rule
(2026-10-25): *writing a driver must never need a change to the kernel;
otherwise it is not a microkernel.* This document says what that takes,
where Ferrix breaks the rule today, and the interface that closes each gap.
It extends `MICROKERNEL.md` §3 K8 (devmgr decides which driver gets a PCI
function) to every bus and every shared platform resource, and it builds on
`CHARDEV.md` (ledger 4352) and `USB-SERIAL.md` §5 (ledger 4317).

**Reviewed** (ledger 4475). OK IF conditions D1-D23 for §1-§5, §7 and §8;
OK for §6 and §10; §9's order as amended in §9 below. **NOT YET:**
* **B1**, the virtual interrupt object (§4.3 shape 2): use §4.1's
  interrupt channel bound to a port instead, unless a measured row shows
  the channel's cost matters for a named consumer.
* **B2**, the SMC firmware-call object (§4.4).
* **B3**, acpid (§4.4).

The condition labels are the ledger entry's; each section below carries
its own. Two decisions are the customer's (§11).

## 1. The rule, made testable

A **driver** is a program that makes one device usable by the system. The
rule holds when every one of these can be added as a ring-3 program and an
entry in devmgr's image-time policy, with **no line changed under
`src/kernel/` and no kernel rebuild**:

1. a driver for a device on any bus Ferrix already enumerates (PCI, the
   device tree, ACPI, USB, and buses that ring-3 drivers own: I2C, SPI,
   SDIO, MDIO, 1-Wire);
2. a driver for a new **class** of device (a sensor, CAN, a JTAG cable, a
   GPIO chip, a UART that is a tty);
3. a **provider** of a shared platform resource: a clock controller, a reset
   controller, a pin controller, a regulator, a power domain, a secondary
   interrupt controller, a DMA engine, a mailbox, a firmware interface.

What stays in the kernel **by definition**, as in seL4 or Zircon: the CPU
and its MMU, the primary interrupt controller (GIC, APIC/IOAPIC), the
architectural timers, the IOMMU (`MICROKERNEL.md` K8: its registers are
never mapped to ring 3), the memory map, the early console, and parsing the
firmware's descriptions (device tree, ACPI tables, PCI configuration
space). A port to a new CPU architecture or interrupt controller is a
kernel port, not a driver. So is a new **typed core** (§6). A **check**
makes the rule testable: §8's `xtask test-driver-model` builds a driver
for a device class and a board peripheral the kernel has never heard of,
boots it on an unchanged kernel image, and requires it to serve. **(D1)**
"Unchanged" means each architecture's kernel ELF, and the loader if it
carries per-board data, is byte-identical by sha256 from one build. Only
the initramfs, devmgr's table and the DTB may differ.

## 2. Where Ferrix breaks the rule today

Measured on develop 455acdee6:

| Gap | Where | Effect |
|---|---|---|
| G1. Device-tree peripherals are published only for bindings the kernel knows: 7 fixed `TREE_*` numbers | `ferrix_native_abi::types` (`TREE_STM32_HDMI` ... `TREE_X86_POWER`), `discovery/board.rs` (core), `platform/st/stm32mp1/*.rs` (load) | A DK1 I2C, SPI, GPIO, ADC, CAN or timer driver needs a new binding and new kernel board code |
| G2. Each peripheral is clocked, reset and pin-muxed by kernel board code (`BoardBinding::prepare`), with one clock knob (`BoardBinding::clock`) | `platform/st/stm32mp1.rs` (`RCC_COMPATIBLE`, `PINCTRL_COMPATIBLE`), `stm32mp1/{sdmmc,usb,gpu}.rs` | Device knowledge per SoC and per peripheral in ring 0, exactly what "keep the kernel slim" forbids |
| G3. A new class needs a kernel core | `interfaces/{input,block_ring,net_ring,display,render,audio,chardev}` | Closed by `CHARDEV.md` (steps 1-3) and K1-K6/K4' for ttys, once built |
| G4. A device a ring-3 driver finds (USB, I2C child) cannot be handed to another driver | devmgr starts drivers only for kernel nodes | Closed for USB by R2 (landed 23158676a); not yet general |
| G5. Interrupts behind a secondary controller (STM32 EXTI, a GPIO expander, a PMIC) | no mechanism | A GPIO interrupt (the FPGA IRQ probe on PF3) needs kernel code |
| G6. Firmware calls (Arm SMC/SCMI, PSCI beyond CPU on/off, OP-TEE; ACPI AML methods) | only the kernel can `smc`; no AML in ring 3 | Clocks the secure world owns, and x86 device power, need kernel code |
| G7. At most 32 driver images, all in the initramfs's first DEVICES message | `MAX_DRIVERS`, `discovery/devmgr.rs` (item) | A product with more drivers needs a kernel limit raised |

PCI **partly** meets the rule: devmgr matches vendor, device and class in
`PCI_DRIVERS`, and the kernel mints BARs and MSI generically. But two
further gaps remain (D2):

| Gap | Where | Effect |
|---|---|---|
| G8. Device knowledge in core predicates: `input_functions` (by the xHCI class code), `intel_gpu` (vendor and class), `reads_log` (`TREE_GS201_DWC3`), `may_have_client_nodes` | `device.rs` | A new device of those kinds needs a core change. Each predicate either leaves, as a devmgr grant or an admission rule, or stays with its stated security reason |
| G9. No I/O-port grant exists at all | kernel, native ABI | A ring-3 driver for a legacy-port device, and acpid (§4.4), have nothing to use |

## 3. Generic device-tree nodes (closes G1)

The kernel publishes **every** device-tree node that is enabled, has a
`compatible`, and is not kernel-owned, as a generic device node, the way it
publishes every PCI function:

* **Its resources, minted by the existing rules**: each `reg` range as an
  aperture, page-aligned and refused if it overlaps a kernel-owned range;
  each `interrupts` entry that names the primary controller as a vector;
  `dma-ranges`/`iommus` as its `DmaShape`.
* **Its identity, as data**: the `compatible` list, the node's path, and its
  `status`. `device_info` returns them. devmgr matches `compatible` against
  its image-time table, so the kernel knows no compatible string.
* **Its references, as data**: `clocks`, `resets`, `pinctrl-*`,
  `power-domains`, `*-supply`, `interrupts-extended`, `gpios`, `dmas`,
  `mboxes` and `iommus` become (provider node, specifier cells) pairs.
  The kernel resolves phandles to nodes and does not interpret the cells.
* **Its subtree, as data**: child nodes of a bus controller (an I2C
  controller's sensors) go to the controller's driver as a read-only blob,
  not as kernel nodes (§5).
* **Kernel-owned nodes** are named by what the kernel uses, never by a
  list of compatibles: the nodes behind `/chosen/stdout-path`, the primary
  interrupt controller, the architectural timer, `/memory`,
  `/reserved-memory` regions the kernel took, the IOMMU, the PSCI node, and
  any node whose `reg` overlaps one of those. Everything else is
  publishable.

**Fails closed.** A node whose `reg` cannot become apertures, or whose
interrupt is not on the primary controller and has no provider (§4.3), is
published without that resource, and its `device_info` says what is
missing. It is not left out.

The 7 `TREE_*` bindings become ordinary generic nodes, and their drivers
match by `compatible` in devmgr's table. The `BoardBinding` registry, the
board `prepare` functions and the `clock` hook leave the kernel (§9).

The conditions (ledger 4475):

* **(D3) Kernel-owned by use.** It is every node a kernel subsystem
  claimed through one registration before a seal point. A claim after the
  seal is refused, and that is checked. The set also covers pstore, the
  spin-table release addresses, a framebuffer the kernel draws on, a
  watchdog it pets, an RNG `random.rs` reads, and the power-off/restart
  node.
* **(D4)** A node referenced, even transitively, by a kernel-owned node
  (clocks, resets, power-domains, pinctrl, supplies, interconnects) is
  published with a **KERNEL_DEPENDS** flag. devmgr's admission refuses a
  driver for it in claimed configurations and admits it on armv7a.
  Otherwise a ring-3 RCC could gate the console's clock, or STGEN's, which
  drives the kernel's timer.
* **(D5) DMA fails closed.** Every tree node reaches memory. NO_DMA comes
  only from nodes the kernel describes itself, never from a tree property.
  `iommus` counts as translated only once the core binds platform stream
  ids, which needs a design of its own.
* **(D6)** On claimed architectures, an untranslated generic peripheral
  starts only on an integrator's per-compatible "not a bus master" entry in
  devmgr's image-time table. The entry carries a written reason, an AoU in
  SAFETY-MANUAL and an assumption in the SECURITY-TARGET. This is an
  exemption to the DMA gate and is **the customer's decision** (§11).
* **(D7)** Overlap is tested on page-rounded ranges, against kernel-owned
  ranges and between published nodes. A page shared by two published nodes
  is minted to neither, unless both go to one driver.
* **(D8)** Only SPIs are minted to ring 3, never SGIs or PPIs, and each
  line has one owner.
* **(D9)** Tree parsing has stated bounds, a host test and fuzzing.
* **(D10)** No node is published by both the `TREE_*` path and the generic
  path, and a check proves it.

## 4. Platform resources served from ring 3 (closes G2, G5, G6)

### 4.1 Providers

A **provider** is a ring-3 driver for a node other nodes reference: the
STM32 RCC (clocks and resets), the pin controller with EXTI, the PWR
regulators, an SCMI agent, a DMA controller, an I2C GPIO expander. It
serves one **resource protocol** per kind, on channels:

| Kind | Operations (consumer → provider) |
|---|---|
| clock | enable, disable, set_rate, get_rate, set_parent |
| reset | assert, deassert, pulse |
| pinctrl | apply named state (`default`, `sleep`) |
| regulator | enable, disable, set_voltage, get_voltage |
| power-domain | on, off |
| gpio | direction, get, set, request interrupt (§4.3) |
| interrupt | request line with trigger, mask, unmask, ack (§4.3) |
| dma | request channel, submit (with pins the consumer owns), cancel |
| mailbox | send, receive |

These are ring-3 message protocols in a crate of their own
(`ferrix-platformctl`), versioned like `chardevctl`. The kernel neither
carries nor reads them. **(D11)** The dma kind passes VMOs, and the
provider pins them for its own device. No cross-domain pin is added.

### 4.2 Wiring by devmgr

devmgr reads each node's references (§3) and, before starting a consumer,
starts the consumer's providers. It then gives the consumer one channel per
reference, already bound to the specifier cells (`clocks = <&rcc SPI5_K>`
becomes a clock channel for SPI5_K). A consumer can name no other clock,
because the channel is the authority: the capability model, not a check
in the provider.

* **Order**: a provider before its consumers. A cycle in the graph is a
  refusal at devmgr's start, so neither side gets started.
* **Death**: a provider that dies is restarted by T0. Its consumers' channels
  answer `PEER_CLOSED`, and devmgr re-binds them to the new instance, the
  rebind contract of `MICROKERNEL.md` K13. A consumer whose device cannot
  survive a lost clock is quiesced first (`DEVMGR.md` §4).
* **Boot devices**: the console and anything the kernel itself uses are
  kernel-owned (§3) and run on the clocks firmware left on. The RCC provider
  is told which clocks are firmware-critical (`/chosen`, `clock-critical`,
  `protected-clocks`) and never gates them.
* **(D12)** A provider restarted by T0 first reads the hardware state back.
  Until its consumers have rebound, or devmgr's bounded deadline passes, it
  gates, resets and powers off nothing.
* **(D13)** devmgr refuses a reference cycle or a missing provider at
  start, naming the nodes.

### 4.3 Interrupts behind a secondary controller

Two shapes, both generic:

1. **A nexus that routes to a primary line**, such as the STM32 EXTI, whose
   lines land on GIC SPIs, or an `interrupt-map`. The kernel resolves the
   route from the tree to the primary line and mints that **vector for the
   consumer**, so delivery is as fast as any other interrupt. The provider
   configures the nexus itself (trigger, mask) through its own apertures.
   The kernel knows the routing only from the tree's `interrupt-map` or
   `interrupt-parent`/`interrupts-extended` chain, never from a driver's
   claim.
2. **A truly cascaded controller**, such as a GPIO expander on I2C or a
   PMIC, whose one parent line fans out. The provider gets the parent vector
   and signals each child on §4.1's **interrupt channel** (request, mask,
   unmask, ack), bound to the consumer's port. It costs one extra wake, as
   Linux's threaded nested IRQs do. *A virtual interrupt object (Zircon's
   `zx_interrupt_create(VIRTUAL)`) was proposed and is NOT YET (B1). It
   comes back only with a measured row showing the channel's cost matters
   for a named consumer.*

The conditions:

* **(D14)** Routes come only from the DT-spec forms: `interrupt-map` with
  its mask, matched cell for cell, and the parent chain.
* **(D15)** For each nexus, the doc says who clears its latch. STM32 EXTI
  latches pending in its own registers, which the provider clears, so EXTI
  is **shape 2**, not shape 1. A line shared across GPIO banks is shape 2
  too.
* **(D16)** A residual, stated: through mask and trigger, a provider has
  reach over its consumer's line.

### 4.4 Firmware calls

**Both halves of this section are NOT YET (B2, B3).** What follows is the
proposal, and below it what each needs before it comes back.

* **Arm SMC/HVC** (SCMI, OP-TEE, vendor SiP calls): a **firmware-call
  object** minted for a node whose binding names its function ids in the
  tree (`arm,smc-id`, the SCMI transport's shared memory, OP-TEE's
  `method`). The object allows exactly those ids, with registers passed
  through. PSCI's CPU and system calls stay the kernel's. The kernel knows
  the conduit (`smc` or `hvc`) and the id list, never what the call does.
* **ACPI AML**: devices' `_PS0`, `_STA`, `_CRS` and `_DSM` are evaluated by a
  ring-3 `acpid` that is given the DSDT/SSDTs as read-only VMOs. Each
  operation region it declares (SystemMemory, SystemIO, PCI_Config,
  EmbeddedControl) is granted through the existing aperture, I/O-port and
  config-write rules, refused where it overlaps a kernel-owned range. The
  kernel keeps the tables it needs for itself (MADT, DMAR/IVRS, FADT for
  sleep). This is how Fuchsia runs ACPI, and it removes the reason the x86
  power node (`TREE_X86_POWER`) exists in the kernel.

**B2, the SMC object.** An id list does not bound what the calls reach.
OP-TEE's standard calls and many SiP calls take non-secure physical
addresses, so the object can read and write any non-secure memory by
proxy. It comes back with a design that:
* admits it under the DMA gate as an untranslated device;
* never allows SMCCC owner 0 (Arm Architecture, including ARCH_WORKAROUND)
  or owner 4 (PSCI, SDEI, TRNG), whatever the tree lists;
* checks the conduit against the kernel's PSCI conduit;
* states the unbounded secure-world time in MEMORY-AND-TIMING.

One object per node, with that node's id list.

**B3, acpid.** It needs a new I/O-port mechanism (G9, the x86-64 TSS
bitmap), designed on its own. That mechanism must refuse:
* the ports the kernel uses;
* 0xCF8-0xCFF, because configuration space stays kernel-mediated (K8);
* the ISA DMA ports;
* SystemMemory regions in usable RAM.

Through the SMI port, acpid holds integrity authority via SMM. So in a
claimed configuration it is either a certified user-space component or
outside the claim: **the customer's decision** (§11). `TREE_X86_POWER`
stays until then.

## 5. Buses that ring-3 drivers own (closes G4)

R2 made the USB host export each interface to a class driver. This design
makes that general: **any driver may publish child devices to devmgr**
through devmgr's protocol (not the kernel's). A child device has a bus kind
(`usb`, `i2c`, `spi`, `sdio`, `mdio`, `w1`, `serio`), identity data (USB
ids and class, a DT `compatible` from the controller's subtree, an ACPI
`_HID` from `acpid`), and a channel to its parent driver that carries the
bus's transfers. devmgr matches and starts the child's driver exactly as it
does for kernel nodes. The kernel takes no part: the child has no
apertures and no vectors, only channels. An interrupt comes on an
interrupt channel (§4.3), or as the child's own primary vector if the tree
gives it one.

* **(D17)** If the kernel's DEVICES, PUBLISHED or REPORT messages change,
  `discovery/devmgr.rs` (item) comes back for review. devmgr verifies the
  identities it can: device-tree children against the kernel's subtree
  blob, and ACPI `_HID` once acpid exists. USB and I2C identities are the
  parent's word, and stated as such.
* **(D18)** A lying parent can obtain names, never more authority than it
  has.

## 6. Classes (closes G3)

A driver's device becomes visible to programs through one of:

* an existing **typed core** (block, net, input, display, render, audio),
  where the kernel or its filesystems must understand the data;
* a **driver-backed terminal** (K1-K6, K4'), for anything that is a tty;
* a **generic character device** (`CHARDEV.md`), for every other class.

A class that needs a new typed core is by construction one the kernel's own
code consumes (a new filesystem's block layer, say). That is kernel work,
not driver work, and this design accepts it.

## 7. Driver images (closes G7)

DEVICES's first message carries the images today, at most 32 of them. The
images move out of the bootstrap message: devmgr gets **one read-only VMO
of the driver store** (the initramfs's `/lib/drivers/` as an archive, or
later a sealed, measured package volume, `MICROKERNEL.md` K14) and loads
drivers from it with the user-space loader (K15). The kernel's cap becomes
a cap on devmgr's memory, which is charged to its job. Adding a driver to
a product is then an image change and never a kernel change. Updating one
at run time already works (`DEVMGR.md` §4.1). **(D19)** The store comes
after K15 (M2), and after K14 (M3) for certified drivers. The DEVICES change
gets its own `L.discovery` id at that step.

## 8. Evidence

* **The rule's own check**, `xtask test-driver-model`. It builds the kernel
  once and records its hash. It then builds an image with a driver the
  kernel has never named: a device-tree node of a made-up `compatible`,
  added to QEMU `virt`'s tree (dumped with `-machine dumpdtb`, edited, and
  passed back with `-dtb`) over a memory region a test device backs, and
  served by a ring-3 test driver. That driver uses a clock, a reset and a pinctrl
  state from a ring-3 test provider, an interrupt channel from a test
  cascade, and a generic chardev for programs. The check requires the
  kernel hash unchanged and the device working. **Negative control:** the
  same driver with the provider absent must not start, and devmgr reports
  the missing reference.
* **Self-checks with controls that FIRE:**
  * a node overlapping a kernel-owned range is published without that
    range;
  * the kernel-owned set is derived, not listed (a control that adds a fake
    kernel-owned node);
  * a vector routed through a nexus is minted only along the tree's route,
    and a forged route is refused;
  * a firmware-call object refuses an id the tree did not list;
  * an interrupt channel is served only by the provider devmgr bound it to;
  * an AML operation region overlapping a kernel range is refused.
* **(D20)** The made-up node is backed by a device QEMU actually models
  (`virt`'s pl061 or pl031 under a made-up compatible). On aarch64 it is
  admitted by a test-only (D6) entry or the unclaimed boot word. A control
  changes one kernel byte, to show the hash comparison fails.
* **(D21)** More self-checks, each with a gate.sh control:
  * a shared page minted to neither;
  * a PPI refused;
  * KERNEL_DEPENDS set across a transitive reference;
  * a kernel claim after the seal refused;
  * a tree node never NO_DMA;
  * no double publication.
* **(D22)** V-19, and a CLAIM.md sentence for armv7a: the DMA-capable
  peripherals' drivers can reach all memory (DMA1/DMA2/MDMA, SDMMC IDMA,
  USB, LTDC, ETH). Wherever firmware leaves RCC, SYSCFG, DDRCTRL, BSEC and
  DBGMCU non-secure, their drivers hold the SoC's integrity.
* **(D23)** Rows `run: PASSED` by hash: check, test-driver-model, test-boot
  on all four boots, and the board rows each step touches. Board rows run
  on hardware, and the product owner schedules them.
* **IDs for step 1**, reserved in a docs-only landing before its code and
  shrunk at landing: H.DEV.7-8, L.discovery.8-12, L.device.60-65, V-19.
  `L.discovery.7` is already the chardev root grant's.
* **On the DK1** (when the customer schedules it): I2C5, SPI5 and the PF3
  interrupt served by ring-3 drivers against the FPGA test target, with the
  RCC, pinctrl/EXTI and PWR providers in ring 3, on a kernel built before
  those drivers existed.
* **Certification:** every touched core or item file gets L ids reserved
  first, and the VULNERABILITY-ANALYSIS rows: a provider sees its
  consumers' requests (accepted, as with any server); a firmware call's
  reach is bounded by the tree's id list; ring 3 now holds the clock and
  reset authority it already effectively had through board drivers.

## 9. What leaves the kernel, and in what order

**The order as reviewed** (ledger 4475, against `MICROKERNEL.md`):
* Steps 1 and 3 (shape 1) are K8's device-tree half, recorded as **K8b**,
  and land ahead of M5. They need neither the lift nor K13.
* Step 2's kernel deletions are M6's "board glue out", moved earlier. Each
  is its own reviewed landing, made after its ring-3 provider passes the
  board's rows.
* Step 4 is ring 3, any time after R2.
* Step 5 waits for B2 and B3.
* Step 6 follows K15, plus K14 for certified drivers.
* Step 7 goes with step 1.

The original list:

1. **Generic device-tree nodes beside the bindings** (§3). Both paths exist
   until each `TREE_*` driver has moved to matching by `compatible`. Core
   files: `discovery/{board,tree,finder,description}.rs`, `device.rs`.
2. **Providers and wiring** (§4.1, §4.2): `ferrix-platformctl`, devmgr's
   reference graph, and a ring-3 `stm32-rcc`, `stm32-pinctrl` (with EXTI)
   and `stm32-pwr`. Then each STM32 board `prepare` is deleted as its
   driver moves: LTDC/HDMI, USB host, GPU, SDMMC. `platform/st/stm32mp1/**`
   shrinks to what the kernel itself uses (the console's clock check, if
   any).
3. **Interrupts** (§4.3): nexus routing from the tree (core), and the
   interrupt channel for cascades and EXTI (ring 3, no new object, B1).
4. **Child devices** (§5) in devmgr's protocol, with USB's R2 moved onto it.
5. **Firmware calls** (§4.4): the SMC object first (the DK1's OP-TEE and SCMI
   clocks), then `acpid`, and `TREE_X86_POWER` leaves.
6. **The driver store** (§7), after K15.
7. **`test-driver-model`** goes in with step 1 and grows with each step.
   When it passes with a board peripheral (step 2) and a cascaded interrupt
   (step 3), the rule holds for the DK1.

The Pixel 7 (`platform/google/gs201`) follows the same steps for its DWC3
and watchdog. The watchdog is kernel-owned if the kernel pets it, and a
provider's otherwise.

## 10. Compared with Linux and other microkernels

| | Linux | Zircon (Fuchsia) | seL4 + CAmkES | Ferrix (this design) |
|---|---|---|---|---|
| New driver needs a kernel change | yes (in-tree module, or out-of-tree against an unstable API) | no | no (but static system build) | no |
| Clocks, resets, pinctrl | in kernel (CCF, pinctrl) | ring-3 drivers (platform bus) | per-system components | ring-3 providers, wired by devmgr |
| Device-tree matching | in kernel | ring-3 driver manager | build-time | ring-3 devmgr; the kernel publishes nodes only |
| Cascaded interrupts | in kernel (irqchip) | virtual interrupts | notifications | interrupt channels; routed lines direct |
| A provider fault | kernel crash | restart, consumers rebind | depends on system | T0 restart, consumers rebind (K13) |

## 11. Questions, answered (ledger 4475), and the customer's two

1. **Is "kernel-owned by use" acceptable?** Yes, under D3-D10. On the
   STM32MP1 it is acceptable under `T.DMA` because armv7a is outside the
   claim (DMA gate G4 (a)), and every armv7a image carries
   `ferrix.unisolated`. It widens what that unclaimed configuration
   exposes, which D22 states. The end condition, an SMMU driven, is
   unchanged.
2. **Does the virtual interrupt need a new object?** No: the interrupt
   channel serves, so there is no new object (B1).
3. **One firmware-call object per node, or per id?** Per node, after B2's
   owner filter, and only once B2's design exists.
4. **What order against `MICROKERNEL.md`?** See §9, as reviewed.

**For the customer, through the product owner:**

* **D6:** may an integrator's per-compatible "not a bus master" entry
  exempt an untranslated peripheral from the DMA gate on claimed
  architectures?
* **B3:** does acpid become a certified user-space component, or does
  ring-3 ACPI stay outside the claim?
