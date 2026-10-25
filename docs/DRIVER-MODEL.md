# Drivers without kernel changes

**Status: design, 2026-10-25. Not built.** The customer's rule
(2026-10-25): *writing a driver must never need a change to the kernel;
otherwise it is not a microkernel.* This document says what that takes,
where Ferrix breaks the rule today, and the interface that closes each gap.
It extends `MICROKERNEL.md` §3 K8 (devmgr decides which driver gets a PCI
function) to every bus and every shared platform resource, and it builds on
`CHARDEV.md` (ledger 4352) and `USB-SERIAL.md` §5 (ledger 4317). It needs
the certification consultant's verdict before any code.

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
kernel port, not a driver. A **check** makes the rule testable: §8's
`xtask test-driver-model` builds a driver for a device class and a board
peripheral the kernel has never heard of, boots it on an unchanged kernel
image (byte-identical), and requires it to serve.

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

PCI already meets the rule (devmgr matches vendor, device and class in
`PCI_DRIVERS`; the kernel mints BARs and MSI generically), apart from G7.

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
carries nor reads them.

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
   and signals each child through a **virtual interrupt**: a kernel object
   the provider triggers and the consumer waits on like a vector (port
   packet, ack). This is one generic object; Zircon has the same, as
   `zx_interrupt_create(VIRTUAL)`. It costs one extra wake, as Linux's
   threaded nested IRQs do.

### 4.4 Firmware calls

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

## 5. Buses that ring-3 drivers own (closes G4)

R2 made the USB host export each interface to a class driver. This design
makes that general: **any driver may publish child devices to devmgr**
through devmgr's protocol (not the kernel's). A child device has a bus kind
(`usb`, `i2c`, `spi`, `sdio`, `mdio`, `w1`, `serio`), identity data (USB
ids and class, a DT `compatible` from the controller's subtree, an ACPI
`_HID` from `acpid`), and a channel to its parent driver that carries the
bus's transfers. devmgr matches and starts the child's driver exactly as it
does for kernel nodes. The kernel takes no part: the child has no
apertures and no vectors, only channels. An interrupt comes as a virtual
interrupt (§4.3) or as the child's own primary vector, if the tree gives it
one.

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
at run time already works (`DEVMGR.md` §4.1).

## 8. Evidence

* **The rule's own check**, `xtask test-driver-model`. It builds the kernel
  once and records its hash. It then builds an image with a driver the
  kernel has never named: a device-tree node of a made-up `compatible`,
  added to QEMU `virt`'s tree (dumped with `-machine dumpdtb`, edited, and
  passed back with `-dtb`) over a memory region a test device backs, and
  served by a ring-3 test driver. That driver uses a clock, a reset and a pinctrl
  state from a ring-3 test provider, a virtual interrupt from a test
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
  * a virtual interrupt is triggerable only by its provider's handle;
  * an AML operation region overlapping a kernel range is refused.
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

1. **Generic device-tree nodes beside the bindings** (§3). Both paths exist
   until each `TREE_*` driver has moved to matching by `compatible`. Core
   files: `discovery/{board,tree,finder,description}.rs`, `device.rs`.
2. **Providers and wiring** (§4.1, §4.2): `ferrix-platformctl`, devmgr's
   reference graph, and a ring-3 `stm32-rcc`, `stm32-pinctrl` (with EXTI)
   and `stm32-pwr`. Then each STM32 board `prepare` is deleted as its
   driver moves: LTDC/HDMI, USB host, GPU, SDMMC. `platform/st/stm32mp1/**`
   shrinks to what the kernel itself uses (the console's clock check, if
   any).
3. **Interrupts** (§4.3): nexus routing from the tree, and the virtual
   interrupt object, a new native object (core, `L.object.*`).
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
| Cascaded interrupts | in kernel (irqchip) | virtual interrupts | notifications | virtual interrupts; nexus lines direct |
| A provider fault | kernel crash | restart, consumers rebind | depends on system | T0 restart, consumers rebind (K13) |

## 11. Questions for the consultant and the customer

1. Is §3's "kernel-owned by use, not by list" acceptable as the boundary,
   and is publishing every other enabled node acceptable under `T.DMA` on
   machines without an IOMMU (the STM32MP1)? The customer's DMA gate
   decision (G4: armv7a is outside the DMA claim) may already settle it.
2. The virtual interrupt is a new native object, a core change. Is there a
   reason a port packet signalled by the provider cannot serve? That would
   need no new object, but has no ack/mask semantics.
3. The firmware-call object: one object per node with an id list, or one
   per id?
4. Ordering against `MICROKERNEL.md`: step 6 needs K15. Should steps 1-5
   precede the M-stages, or be folded into K8?
