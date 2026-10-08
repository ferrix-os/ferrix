/*
 * Ferrix — SysML v2 model, part 8: devices and userspace drivers.
 *
 * docs/ARCHITECTURE.md §7. The kernel enumerates buses because that needs
 * ACPI or a device tree and privileged access; it does not drive devices.
 * Everything from the device node outward is stage 10, which is done: the
 * drivers are processes devmgr starts. What it still owes is named on the
 * part that owes it.
 */
package FerrixDrivers {
    private import ScalarValues::*;
    private import FerrixLifecycle::*;
    private import FerrixObjects::*;
    private import FerrixMemory::Vmo;

    // -----------------------------------------------------------------------
    // Reaching the machine's description, today
    // -----------------------------------------------------------------------

    #implemented part def AcpiAccess {
        @stage { number = 3; }
        doc /* src/kernel/src/discovery/acpi.rs: the one place a physical address firmware
             * wrote becomes a reference the parser reads, through the direct
             * map, refusing addresses or lengths outside it. src/lib/platform/acpi never
             * dereferences a pointer: RSDP, XSDT/RSDT, MADT, FADT fixed
             * fields, GTDT, HPET, and for stage 10 the MCFG, GICv2m frames,
             * the DMAR and the IORT. No AML. */
        action open;
        action madt;
        action fadt;
        action gtdt;
        action hpet;
    }

    #implemented part def FdtAccess {
        @stage { number = 1; }
        doc /* src/kernel/src/discovery/fdt.rs: the loader's copy of firmware's tree, read
             * through the direct map from DeviceTree memory that nothing
             * reclaims, so the borrow is honestly 'static. src/lib/platform/fdt: nodes,
             * properties, reg, interrupts, compatible, stdout-path, the
             * interrupt controller, the timer, /cpus, the PSCI conduit. */
        action open;
        action console;
        action interruptController;
        action timer;
        action cpus;
        action psci;
    }

    #implemented part def MmioWindows {
        @stage { number = 3; }
        doc /* src/kernel/src/mmio.rs: the one place that says read_volatile and
             * write_volatile. A window with no base reads zero and discards
             * writes, so "never mapped" is a value rather than a null
             * dereference. */
        action read32;
        action write32;
    }

    // -----------------------------------------------------------------------
    // Stage 10
    // -----------------------------------------------------------------------

    #implemented part def DeviceNode {
        @stage { number = 10; }
        doc /* What the kernel creates per device found, and hands devmgr a
             * handle to. src/kernel/src/device.rs: one per PCI function and per
             * virtio,mmio tree node, published at boot. Apertures and vectors
             * are Aperture and Vector tokens only this module mints, and
             * IoMapping and Interrupt take those rather than numbers, so a
             * driver cannot name memory or an interrupt its device does not
             * have. The pages of a PCI function's MSI-X table and pending-bit
             * array are withheld from its apertures. A PCI function's vectors
             * are its MSI-X entries, minted on first ask and masked at the
             * entry's own bit. device_info writes what a driver needs to
             * find its registers, and each node has the IOMMU domain its
             * DMA goes through. */
        attribute compatible : String;
        attribute mmioWindows : Natural[0..*];
        attribute interrupts : Natural[0..*];
    }

    #implemented part def DeviceEnumeration {
        @stage { number = 10; }
        doc /* ACPI on x86-64 and AArch64 under EDK2, device tree on ARMv7-A
             * and where AArch64 firmware offers one; PCIe bus walk from
             * either. src/kernel/src/discovery/pci.rs: MCFG (src/lib/platform/acpi) or
             * pci-host-ecam-generic (src/lib/platform/fdt), ECAM mapped a bus at a time,
             * the src/lib/platform/pci walk with every BAR sized and every capability
             * list walked, in the boot test on all three architectures.
             * Device nodes are built from what it finds: see DeviceNode.
             * Owed after the exit: trusting a BAR firmware placed but left
             * decoding off, of which only the device tree's host-bridge
             * windows are read so far. */
        part nodes : DeviceNode[0..*];
        action enumerate;
    }

    #implemented part def IommuDomain {
        @stage { number = 10; }
        doc /* Scoped to one device: the device addresses a DMA VMO gets come
             * from here, so a driver cannot DMA over the kernel or over
             * another driver. src/kernel/src/iommu.rs's Domain, one per device
             * node, made the first time it is asked for: a pin gives each
             * page a device address and an unpin takes them back. A
             * translated domain maps each pinned page at its own physical
             * address and nothing else; an untranslated one, where no unit
             * translates, hands out physical addresses. */
        ref part device : DeviceNode;
        action mapForDevice;
        action unmapForDevice;
    }

    enum def IommuMode {
        Enforced;   // VT-d, AMD-Vi, SMMUv3
        DegradedTrusted;  // no IOMMU: announced loudly at boot
    }

    #implemented part def IommuDomains {
        @stage { number = 10; }
        doc /* Where the hardware is: src/lib/platform/acpi's dmar module (VT-d units
             * and their device scopes) and iort module (root complex to
             * SMMUv3 stream IDs), tested against QEMU's own table builders,
             * and the device tree's arm,smmu-v3 nodes and iommu-map.
             * src/kernel/src/iommu.rs places every PCI function behind a unit
             * before any is programmed, counting what it cannot follow as
             * unresolved rather than bypassing. VT-d on x86-64 and the
             * SMMUv3 under ACPI on AArch64 translate from before PCI
             * enumeration, and a deliberate out-of-domain write faults on
             * both in every boot test. ARMv7-A stays in degraded trusted
             * mode, announced at boot. AMD-Vi is not driven. */
        attribute mode : IommuMode;
        part domains : IommuDomain[0..*];
    }

    #implemented part def DriverProcess {
        @stage { number = 10; }
        doc /* An ordinary user process in its own Job, holding exactly the
             * capabilities devmgr gave it. A driver fault is a process fault;
             * a wedged driver is a Job kill. Eleven exist, native programs on
             * src/user/system/native/rt under /lib/drivers: virtio-blk,
             * stm32-sdmmc, virtio-net, virtio-gpu, stm32-ltdc, gc400,
             * virtio-input, usbhid, usbdev, virtio-snd and vport, each a thin
             * layer of handles over its src/lib/ crates. */
        ref part job : Job;
        part ioMappings : IoMapping[0..*] {
            doc /* One per BAR or MMIO window, and nothing outside it. */
        }
        part interrupts : Interrupt[0..*] {
            doc /* One per vector, bound to a Port. */
        }
        part eventPort : Port;
        ref part dmaBuffers : Vmo[0..*] {
            doc /* Device addresses from the device's IOMMU domain. */
        }
        part channel : Channel {
            doc /* To the kernel subsystem it serves: block, display, net, input. */
        }
        part ring : SharedRing;
    }

    #implemented part def SharedRing {
        @stage { number = 10; }
        doc /* The data path is not per-request IPC: driver and kernel share
             * a descriptor ring in a VMO and ring a doorbell; requests batch.
             * The same shape virtio and NVMe already use. src/lib/drivers/virtio's split
             * virtqueue (driver and device halves, over abstract shared
             * memory) is the first of these. Between kernel and driver:
             * src/lib/proto/blkring (docs/BLOCK-RING.md) and src/lib/proto/netring
             * (docs/NET-RING.md), with src/kernel/src/interfaces/block_ring and
             * src/kernel/src/interfaces/net_ring as the kernel's ends and doorbells as port
             * packets. The display and input drivers need none: frames do
             * not move, and their control channels carry the rest. */
        ref part memory : Vmo;
        action addChain;
        action takeUsed;
        action doorbell;
    }

    #implemented part def DevMgr {
        @stage { number = 10; }
        doc /* A native program on src/user/system/native/rt, not a musl binary: src/user/system/native/devmgr,
             * /sbin/devmgr. Receives a handle per device node, matches a
             * driver, spawns it in its own Job with the four resources
             * above. The kernel starts it from stage 10's boot check with
             * DEVICES messages holding a job, every device node twice and
             * every image /lib/drivers/MANIFEST lists -- the drivers, and
             * drvupdated in the images that carry it (DrvUpdated below) --
             * (src/kernel/src/discovery/devmgr.rs, src/lib/proto/devmgr-proto); it matches
             * virtio-blk, virtio-net, virtio-gpu, virtio-input and the
             * virtio-serial port by a table of its own, starts each driver
             * with START, waits for every one but the port driver to be
             * PUBLISHED before the next, and REPORTs. docs/DEVMGR.md. */
        action match;
        action spawnDriver;
        #implemented action quiesceOnDeath {
            doc /* device_quiesce after a driver's TERMINATED: the kernel waits
                 * for the block, display and render cores to let the device go
                 * (src/kernel/src/claim.rs), then DIED. */
        }
        #inProgress action updateDriver {
            doc /* An image root sent through drvupdated put on a device that
                 * a restartDriver kind drives: devmgr copies it, proves it
                 * loads with process_create in a scratch job, stops and
                 * quiesces the running driver, starts the copy and gives it
                 * 15 s to publish, or starts the image the device had again.
                 * The display kind is gated by test-restart --update; the
                 * other kinds and the image's signature (D3) are owed.
                 * docs/DEVMGR.md section 4.1. */
        }
        #implemented action restartDriver {
            doc /* A display driver that died is started again on a duplicate
                 * of devmgr's kept device handle, at most eight times a device;
                 * its card returns as the number it had. RESTARTED tells the
                 * kernel. docs/DEVMGR.md section 4. */
        }
    }

    #inProgress part def DrvUpdated {
        @stage { number = 10; }
        doc /* src/user/system/native/drvupdated: the helper through which a
             * new driver image reaches DevMgr, which reads no files and waits
             * on one port. Started by devmgr from /lib/drivers when the image
             * carries it -- only test-restart --update's do (SAFETY-MANUAL
             * AoU-26) -- it takes root's connections on the abstract socket
             * ferrix.devmgr.update, refuses any other peer and any header past
             * 8 MiB before it makes a VMO, and hands devmgr the request and
             * the image's VMO on a channel. /bin/drvupdate is its client.
             * docs/DEVMGR.md section 4.1. */
    }

    #implemented part def VirtioBlkDriver :> DriverProcess {
        @stage { number = 10; }
        doc /* The first driver: virtio-blk as a user process. Exit test: a
             * sector read through ring 3 with the IOMMU on, and a deliberate
             * out-of-domain DMA attempt faulting. src/user/system/native/drivers/block/virtio-blk over
             * src/lib/drivers/block/virtio-blk and src/lib/drivers/block/blkserve; the exit is met in the boot
             * test, with VT-d translating on x86-64 and the SMMUv3 on
             * AArch64, and ARMv7-A in degraded trusted mode. */
    }

    #implemented part def VirtioNetDriver :> DriverProcess {
        doc /* src/user/system/native/drivers/net/virtio-net over src/lib/drivers/net/virtio-net and src/lib/drivers/net/netserve, speaking the
             * net ring; cargo xtask test-net is its gate. */
    }

    #implemented part def VirtioGpuDriver :> DriverProcess {
        @stage { number = 17; }
        doc /* src/user/system/native/drivers/display/virtio-gpu over src/lib/drivers/display/virtio-gpu: the display core's card0 over
             * displayctl, and since stage 19 the render core's renderD128
             * over renderctl, turning its messages into virtio-gpu's 3D
             * commands. cargo xtask test-display is its gate. The one driver
             * devmgr starts again after it dies (restartDriver). */
    }

    #implemented part def VirtioInputDriver :> DriverProcess {
        @stage { number = 17; }
        doc /* src/user/system/native/drivers/input/virtio-input over src/lib/drivers/input/virtio-input, feeding the input core's
             * /dev/input/eventN over inputctl; cargo xtask test-input is its
             * gate. */
    }

    #implemented part def UsbHidDriver :> DriverProcess {
        @stage { number = 17; }
        doc /* src/user/system/native/drivers/usb/usbhid over src/lib/drivers/usb/usb-host: the STM32MP157's EHCI
             * controller, its hubs, and HID keyboards, mice and media keys
             * read by their report descriptors, each served to the input
             * core over a control channel of its own, keyboards' LEDs lit
             * by the core's STATUS.
             * devmgr starts it as a bus host and does not wait for it; its
             * memory is pinned PIN_COHERENT. No QEMU machine has the
             * device, so no boot test exercises it: its gates are
             * src/lib/drivers/usb/usb-host's tests against a model of EHCI and the board's
             * bus, and it ran on the DK1 on 2026-09-23 (docs/INPUT.md §7). */
    }

    #inProgress part def Gc400Driver :> DriverProcess {
        @stage { number = 19; }
        doc /* src/user/system/native/drivers/gpu/gc400 over src/lib/drivers/gpu/gc400: the STM32MP157's Vivante GC400T,
             * the G2 step of the board's gears (docs/GPU.md §6.3). It
             * prints the core's identity, soft-resets and initialises it as
             * etnaviv does, starts the front end on a WAIT/LINK loop in one
             * page pinned PIN_COHERENT, splices in two blocks that each
             * raise an event, and takes each as the interrupt. devmgr starts
             * it as an engine, which serves no kernel subsystem yet, and does
             * not wait for it. No QEMU machine has the core; its gates are
             * src/lib/drivers/gpu/gc400's tests against models of the host interface and
             * the front end, and on the board, on 2026-09-24, both events
             * arrived by interrupt (G2 done). Drawing is G3 to G5. */
    }

    #inProgress part def Stm32SdmmcDriver :> DriverProcess {
        @stage { number = 10; }
        doc /* src/user/system/native/drivers/block/stm32-sdmmc over
             * src/lib/drivers/block/stm32-sdmmc: the STM32MP157's SDMMC1 as
             * a disk behind the block ring, so the kernel's GPT scan publishes
             * the card's partitions and btrfs mounts from one (docs/CHROME.md
             * 10.1). The kernel publishes the controller as a device tree node
             * and writes none of its registers: U-Boot left it running. The
             * driver moves every word through the FIFO with hardware flow
             * control and never turns the internal DMA on, and serves a write
             * only inside a partition named ferrix-*. Its gates are the
             * library's host tests against a model of the controller and a
             * card, and the boot check of the one tree disk; no emulator has
             * the controller, and it has not run on the board (2026-10-08). */
    }

    #implemented action def DriverBootstrap {
        @stage { number = 10; }
        doc /* The loader places an initramfs in RAM holding devmgr, the
             * drivers and init. The kernel unpacks it into a tmpfs root and
             * starts devmgr from stage 10's boot check, and devmgr starts the
             * drivers; on a boot with a btrfs root disk the kernel then
             * mounts it and moves every process after onto it, as Linux's
             * switch_root does (src/kernel/src/fs/root_disk.rs), and starts init
             * last, after the boot marker. Linux's answer, for the same
             * reason. The test boots keep the tmpfs root, so only cargo
             * xtask run mounts a root disk and moves onto it. pivot_root
             * itself, so the tmpfs can be unmounted from under the new root,
             * is an init's work, which stage 15 owes. */
        first start;
        then action mountInitramfsAsRoot;
        then action startDevMgr;
        then action spawnBlockDriver;
        then action mountBtrfs;
        then action pivotRoot;
        then action startInit;
        then done;
    }
}
