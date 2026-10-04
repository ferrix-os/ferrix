/*
 * Ferrix — SysML v2 model, part 2: structure.
 *
 * The shape of the system (docs/ARCHITECTURE.md §1), the workspace it is
 * built from (§9), the architecture facade, and the three architectures
 * behind it. Subsystem internals live in the packages that follow; this one
 * says what exists and how it is connected.
 */
package FerrixStructure {
    private import ScalarValues::*;
    private import FerrixLifecycle::*;
    private import FerrixBoot::*;
    private import FerrixMemory::*;
    private import FerrixScheduling::*;
    private import FerrixObjects::*;
    private import FerrixIsolation::*;
    private import FerrixDrivers::*;
    private import FerrixStorage::*;

    // -----------------------------------------------------------------------
    // Context: the machine firmware hands us
    // -----------------------------------------------------------------------

    enum def IsaFamily {
        X86_64;
        AArch64;
        Armv7a;
    }

    enum def FirmwareKind {
        doc /* Every architecture boots via UEFI, so firmware calls a Rust
             * efi_main with a stack set up and the MMU on. Which firmware
             * differs: EDK2 on the 64-bit pair, U-Boot's bootefi on ARMv7-A
             * (upstream EDK2 dropped its 32-bit Arm build, and U-Boot is what
             * the STM32MP157 runs anyway). */
        Edk2;
        UBoot;
    }

    enum def MachineDescription {
        doc /* How the kernel learns what the machine contains: ACPI tables on
             * x86-64 and on AArch64 under EDK2, a flattened device tree on
             * ARMv7-A and on AArch64 firmware that offers one. */
        Acpi;
        DeviceTree;
    }

    part def Cpu {
        attribute isa : IsaFamily;
        attribute hardwareId : Natural {
            doc /* What the machine calls it: an APIC ID, or the affinity
                 * fields of MPIDR. Not dense, so the kernel also assigns a
                 * logical index; the boot CPU is always logical zero. */
        }
    }

    part def Iommu {
        doc /* VT-d or AMD-Vi on x86-64, SMMUv3 on AArch64. Absent on QEMU's
             * default machines, which is why the degraded trusted mode exists. */
    }

    part def Device;

    part def Machine {
        doc /* The system context. Ferrix targets QEMU's q35 and virt
             * machines in CI and the STM32MP157 on the ARMv7-A side. */
        attribute firmware : FirmwareKind;
        attribute description : MachineDescription;
        part cpus : Cpu[1..*];
        attribute ramBytes : Natural;
        part devices : Device[0..*];
        part iommu : Iommu[0..1];
        port efi : EfiHandoffPort;
    }

    // -----------------------------------------------------------------------
    // Interfaces between the big pieces
    // -----------------------------------------------------------------------

    item def EfiSystemTable {
        doc /* Firmware's table: boot services, con_out, the configuration
             * tables that carry the RSDP or the device tree. */
    }

    port def EfiHandoffPort {
        doc /* extern "efiapi" fn efi_main(image, system_table). On ARMv7-A the
             * calling convention lowers to AAPCS on the musleabi target, so the
             * signature is the same on all three. */
        out item systemTable : EfiSystemTable;
    }

    port def BootHandoffPort {
        doc /* The loader and the kernel are two programs linked for different
             * targets that meet at exactly one struct, declared once in
             * src/lib/proto/bootinfo. A mismatch is a type error rather than a triple
             * fault. */
        in item info : BootInfo;
    }

    item def SyscallRequest {
        attribute number : Natural;
        attribute args : Integer[0..6] ordered;
    }

    item def SyscallResult {
        doc /* A value, or a negative errno, exactly as Linux returns it. */
        attribute value : Integer;
    }

    port def LinuxSyscallPort {
        doc /* Syscall numbers 0.., each architecture's table as Linux defines
             * it (the EABI table on ARMv7-A). A compatibility obligation: no
             * opinions live here. src/lib/proto/linux-abi holds the numbers, errnos and
             * repr(C) layouts. */
        in item request : SyscallRequest;
        out item result : SyscallResult;
    }

    port def NativeSyscallPort {
        doc /* Syscall numbers from 0x1000, capability-handle based. Where the
             * design opinions live; what devmgr and drivers speak. A process
             * may use both ports. */
        in item request : SyscallRequest;
        out item result : SyscallResult;
    }

    port def SerialConsolePort {
        doc /* Early boot and panic output. The boot test reads this stream
             * and waits for the marker FERRIX-BOOT-OK. */
        out item line : String;
    }

    // -----------------------------------------------------------------------
    // The system of interest
    // -----------------------------------------------------------------------

    part def Ferrix {
        doc /* An operating system written in Rust for x86-64, AArch64 and
             * ARMv7-A, whose acceptance test is that it compiles Rust. */

        part loader : Loader;
        part kernel : Kernel;
        #implemented part initramfs : Initramfs;
        #inProgress part userland : Userland;

        connect loader.handoff to kernel.handoff;
        connect userland.posix to kernel.linuxAbi;
        connect userland.native to kernel.nativeAbi;
    }

    part def Deployment {
        doc /* Ferrix on a machine. CI instantiates three of these under QEMU;
             * the STM32MP157-DK board is the fourth, reached by
             * `cargo xtask deploy --arch armv7a`, which copies two files onto
             * a vendor-provisioned card rather than writing a whole image,
             * because the board's boot chain lives in partitions imaging would
             * destroy, then watches the ST-LINK serial port for the marker.
             * It has run on hardware: an STM32MP157D-DK1 at two cores reached
             * FERRIX-BOOT-OK stages 1-9 and ran stage 7's script at fd4442e. */
        part machine : Machine;
        part ferrix : Ferrix;
        connect machine.efi to ferrix.loader.efi;
    }

    part ferrix : Ferrix {
        doc /* The reference instance the roadmap and assurance packages point
             * at. */
    }

    // -----------------------------------------------------------------------
    // The loader
    // -----------------------------------------------------------------------

    #implemented part def Loader {
        @stage { number = 1; }
        doc /* src/boot/common/uefi/: the UEFI loader. Reads the kernel from the volume it was
             * booted from, copies it to its link address, builds the address
             * space, takes the memory map, leaves boot services, installs the
             * new tables and jumps. Only the last step is assembly, because
             * the return address of a Rust call would be in the address space
             * just replaced. One program for three architectures: src/boot/common/uefi/src/arch
             * has one file per machine and no second loader. */

        port efi : ~EfiHandoffPort;
        port handoff : ~BootHandoffPort;

        attribute kernelPath : String = "/FERRIX/KERNEL.ELF";
        attribute bootInfoBytes : Natural = 65536;
        attribute bootStackBytes : Natural = 65536;

        part uefi : UefiBindings {
            doc /* src/boot/common/uefi/src/uefi: the handful of protocols and tables used —
                 * simple file system, loaded image, graphics output, and the
                 * ACPI and device-tree configuration tables. Hand-written,
                 * no crates.io dependency. */
        }
        part services : Services {
            doc /* src/boot/common/uefi/src/services.rs: allocation, memory map, and the one
                 * call after which firmware is gone: ExitBootServices. */
        }
        part load : ImageLoading {
            doc /* src/boot/common/uefi/src/load.rs: ELF parse via src/lib/platform/elf, placement, page
                 * tables via src/lib/kernel/paging, the identity plan for the switch. */
        }
        part arch : LoaderArch;

        perform action boot : LoaderSequence;
    }

    part def UefiBindings;
    part def Services;
    part def ImageLoading;

    abstract part def LoaderArch {
        doc /* prepare_cpu, clean_dcache, enter_kernel: what each machine needs
             * between ExitBootServices and the kernel's first instruction. */
        action prepareCpu;
        action cleanDcache;
        action enterKernel {
            doc /* The one assembly site in the loader. x86-64: EFER.NXE,
                 * CR4.PGE, write CR3, jump. AArch64: MMU off, MAIR/TCR/TTBR0/1,
                 * SCTLR, MMU on, jump — TCR cannot be reprogrammed under a
                 * live regime. ARMv7-A: the same through cp15, refusing to
                 * start in HYP mode rather than leaving it. */
        }
    }

    // -----------------------------------------------------------------------
    // The kernel
    // -----------------------------------------------------------------------

    part def Kernel {
        doc /* src/kernel/: monolithic core, capability seams, userspace device
             * drivers. Entered from the loader with the MMU on, three
             * mappings in place and nothing else: no vectors, no allocator,
             * no other CPU running. */

        port handoff : BootHandoffPort;
        port console : SerialConsolePort;
        #implemented port linuxAbi : LinuxSyscallPort;
        #implemented port nativeAbi : NativeSyscallPort;

        // -- Runs today, stages 1 to 4 --------------------------------------
        part arch : ArchLayer;
        #implemented part printer : Console {
            doc /* src/kernel/src/console.rs: println over the arch console, behind
                 * a lock that a panicking CPU waits a bounded time for, so a
                 * fault while printing still produces its FERRIX-PANIC line.
                 * It keeps its last 4 KiB in a lock-free ring, which a panic
                 * draws on the firmware framebuffer when there is one
                 * (src/kernel/src/panic/screen.rs): the second in-kernel output
                 * device, write-only and used for nothing else. */
        }
        #implemented part early : EarlyMemory;
        #implemented part trap : TrapDispatch;
        #implemented part mm : PhysicalMemory;
        #implemented part vmap : VmapArena;
        #implemented part mmio : MmioWindows;
        #implemented part irq : IrqTable;
        #implemented part timer : Timer;
        #implemented part smp : Smp;
        #implemented part acpi : AcpiAccess;
        #implemented part fdt : FdtAccess;
        #implemented part tasks : Tasks;
        #implemented part sched : Scheduler;

        // -- Stages 6 to 12 and networking run today; stage 13's do not -----
        #implemented part vm : VirtualMemory;
        #implemented part syscalls : LinuxSyscallLayer;
        #implemented part native : NativeAbi;
        #implemented part futex : Futex;
        #implemented part signals : Signals;
        #implemented part ipc : PosixIpc;
        #implemented part vfs : Vfs;
        #implemented part pageCache : PageCache;
        #implemented part filesystems : Filesystems;
        #implemented part blockCore : BlockCore;
        #implemented part netCore : NetCore;
        #inProgress part namespaces : Namespaces;
        #inProgress part cgroups : Cgroups;
        #planned part seccomp : Seccomp;
        #implemented part devices : DeviceEnumeration;
        #implemented part iommu : IommuDomains;

        perform action bringUp : KernelBringUp;

        // The lock order stages 2 and 4 wrote down, as a constraint on the
        // parts that hold them: tables before frames before heap.
        assert constraint lockOrder {
            doc /* mm.rs: TABLES serialises every walk and change of the
                 * kernel page tables and is taken before FRAMES (a mapping
                 * may need a frame for a table) and before HEAP (an unmap
                 * records what it released). Never the other way round.
                 * Stage 5 adds a run queue's lock outside the heap's and the
                 * frame allocator's, with nothing ever taken outside it. */
        }
    }

    part def Console;

    // -----------------------------------------------------------------------
    // Userland, from stage 6
    // -----------------------------------------------------------------------

    #implemented part def Initramfs {
        @stage { number = 8; }
        doc /* A cpio "newc" archive xtask writes and the loader reads from
             * FERRIX/INITRD.IMG into memory nothing reclaims; the kernel
             * unpacks it into the tmpfs root through the same VFS calls a
             * program makes. It holds /sbin/devmgr, the drivers devmgr starts
             * under /lib/drivers with a MANIFEST naming them, and /bin, whose
             * shell since 2026-09-18 is zinc. What breaks the cycle "the root
             * filesystem needs a block driver that lives on the root
             * filesystem"; a btrfs root, on a boot that has one, is installed
             * from the same archive (src/kernel/src/fs/root_disk.rs). */
        part devmgr : UserBinary {
            attribute :>> staticMusl = false;
            doc /* /sbin/devmgr, a native program on src/user/system/native/rt. */
        }
        part virtioBlk : UserBinary {
            attribute :>> staticMusl = false;
            doc /* /lib/drivers/blk, a native program on src/user/system/native/rt. */
        }
        part virtioGpu : UserBinary {
            attribute :>> staticMusl = false;
            doc /* /lib/drivers/gpu, a native program on src/user/system/native/rt. */
        }
        part virtioNet : UserBinary {
            attribute :>> staticMusl = false;
            doc /* /lib/drivers/net, a native program on src/user/system/native/rt. */
        }
        part virtioInput : UserBinary {
            attribute :>> staticMusl = false;
            doc /* /lib/drivers/input, a native program on src/user/system/native/rt. */
        }
        part vport : UserBinary {
            attribute :>> staticMusl = false;
            doc /* /lib/drivers/vport, the virtio-serial port driver, a native
                 * program on src/user/system/native/rt. */
        }
        part virtioSndDriver : UserBinary {
            attribute :>> staticMusl = false;
            doc /* /lib/drivers/snd, the virtio-snd driver (docs/AUDIO.md), a
                 * native program on src/user/system/native/rt over src/lib/drivers/sound/virtio-snd. */
        }
        part init : UserBinary {
            doc /* The program the kernel starts first (src/kernel/src/init.rs):
                 * /bin/zinc unless a build names another with --init, or a
                 * list of commands run in turn. */
        }
    }

    part def UserBinary {
        attribute staticMusl : Boolean = true;
    }

    #implemented part def UserProcess {
        @stage { number = 6; }
        doc /* A running Linux binary -- static musl, a static PIE, or
             * dynamically linked through the interpreter its PT_INTERP names
             * -- as a Process (see FerrixObjects) speaking the Linux ABI, and
             * optionally the native one. devmgr and the drivers are native
             * programs on src/user/system/native/rt, which make the few Linux calls they need
             * as well. */
        attribute usesNativeAbi : Boolean = false;
    }

    #inProgress part def Userland {
        doc /* Linux binaries speaking the Linux ABI -- zinc, the uutils
             * family and C programs ported over ferrousli, somebody else's
             * busybox, rustc -- and devmgr and its drivers, native programs
             * that speak the native ABI as well, and /sbin/init, pid 1 of
             * every image since 2026-09-26. */
        port posix : ~LinuxSyscallPort;
        port native : ~NativeSyscallPort;

        #implemented part init : UserProcess {
            @stage { number = 15; }
            doc /* /sbin/init (src/user/system/linux/init/, docs/INIT.md), since 2026-09-26: pid 1
                 * around src/lib/init/svc's manager. It mounts /run and cgroup2, runs
                 * each service in a cgroup of its own from clone3, reaps,
                 * restarts, gives the console a getty and a shell with a
                 * session of its own, and powers off. /bin/svc drives it over
                 * /run/ferrix/control; it waits for notify readiness, starts
                 * services on a socket's first connection, and applies
                 * MemoryMax=, TasksMax= and OOMPolicy= over the scoped OOM
                 * kill. It gives services bootstrap channels, starts native
                 * ones in their cgroup's job, and routes the directory's
                 * OPENs. It is pid 1 of cargo xtask run and of every desktop
                 * image, where the compositor is hyprix.service and each
                 * program it starts is in a scope of its own (L10). devmgr
                 * restarts drivers by its restart policy (L11), and on an
                 * image booted with ferrix.devmgr=init pid 1 starts devmgr
                 * itself, through the kernel's starter (L12, 2026-09-27).
                 * L13, the sandboxing keys, waits for the rest of stage 13. */
        }
        #implemented part shell : UserProcess {
            doc /* busybox sh was the first foreign binary to run on Ferrix:
                 * Alpine's static musl build runs a script at stage 7, given
                 * with sh -c, and cargo xtask test-shell still runs it when
                 * --init names it. The image's own shell since 2026-09-18 is
                 * zinc, whose job control cargo xtask test-jobs types at. */
        }
        #implemented part devmgr : UserProcess;
        #implemented part drivers : DriverProcess[0..*];
        #implemented part rustc : UserProcess {
            doc /* The goal, stage 16, met on x86-64 on 2026-09-22: the
                 * rust-lang.org rustc, run from a btrfs volume mounted at
                 * /data, compiles hello.rs and runs what it made (cargo xtask
                 * test-rustc). */
        }
    }

    // -----------------------------------------------------------------------
    // The architecture facade
    // -----------------------------------------------------------------------

    abstract part def ArchFacade {
        doc /* src/kernel/src/arch/mod.rs: the one module generic code reaches the
             * CPU through. Each architecture exports the same set of names;
             * the facade re-exports whichever one the build is for. Adding an
             * architecture is a third cfg branch, not a second facade. */

        attribute NAME : String;
        attribute TLB_FLUSH_IS_BROADCAST : Boolean {
            doc /* True where the hardware invalidates every core's TLB in one
                 * instruction (tlbi ...is on both Arm architectures); false
                 * where the kernel has to send a shootdown IPI (x86-64). */
        }

        item def TrapFrame {
            doc /* The interrupted context as the entry stub saved it. Shape
                 * differs per machine; classify turns it into a Trap. */
        }
        part encoding : PageTableEncoding {
            doc /* PageEncoding: which src/lib/kernel/paging Encoding this machine's
                 * descriptors use. */
        }

        // Console and traps
        action initConsole;
        action initTraps;
        action breakpoint;
        action advancePastBreakpoint;
        action classify;
        action reportTrap;

        // Interrupts and time
        action initInterrupts;
        action enableInterrupts;
        action disableInterrupts;
        action serviceInterrupts;
        action timerArm;
        action timerDisarm;
        attribute timerIrq : Natural;
        attribute ipiIrq : Natural;
        action counterNow;
        action counterHz;
        action waitForInterrupt;
        action waitForWork;

        // Memory
        action flushTlb;
        action dropIdentityMap;
        action identityRoot;
        action prepareUserRoot {
            @stage { number = 6; }
            doc /* Make a freshly allocated user root usable. x86-64 keeps both
                 * halves in one root, so it shares the kernel's top-level
                 * slots into it — shared, not copied, so a later kernel
                 * mapping appears in every space without walking any. The
                 * two Arm architectures do nothing: the kernel is in TTBR1. */
        }
        action installUserRoot {
            @stage { number = 6; }
            doc /* Translate this processor's user half through a given root.
                 * x86-64 is one CR3 write, whose own side effect is to drop
                 * every non-global entry while the kernel's global ones — set
                 * because the loader enables CR4.PGE — survive. The two Arm
                 * architectures write TTBR0, clear EPD0 (the lower half is
                 * disabled rather than dismantled, so an install re-enables a
                 * regime), and then invalidate by ASID, in that order: the
                 * other order leaves the regime live with the register still
                 * naming the previous space's tables. */
        }
        action uninstallUserRoot {
            @stage { number = 6; }
            doc /* Stop translating user addresses, which is the state a kernel
                 * thread runs in. x86-64 goes back to the kernel's own root;
                 * the Arm pair set EPD0 and invalidate, because EPD0 governs
                 * walks and not the TLB. Not lazy TLB: keeping the outgoing
                 * root installed is faster and has to keep an address space
                 * alive under a thread that holds no reference to it. */
        }

        // Processors
        action describeCpus;
        action hardwareId;
        action cpuLocal;
        action setCpuLocal;
        action sendIpiToOthers;
        part cpuStarter : CpuStarter;

        // Power
        action halt;
        action shutdown;

        // Tasks (stage 5)
        action prepareStack {
            doc /* Lay out a fresh task's stack so that the first switch into
                 * it "returns" into its entry function. */
        }
        action switchTo {
            doc /* The context switch, arch/<machine>/switch.rs: saves the
                 * callee-saved set and the stack pointer, and returns onto a
                 * stack that belongs to another task, which Rust cannot say.
                 * The last entry the allow-list's every-architecture half was
                 * meant to gain. */
        }

        // Owed by later stages, and the only things a stage may add.
        #implemented action enterUser {
            @stage { number = 6; }
            doc /* enter_user: the ring-3 / EL0 / USR transition, from the
                 * program's own task, never returning. Every general register
                 * zeroed, then sysretq, eret or rfeia with interrupts open.
                 * Exceptions from user mode land on the task's own kernel
                 * stack: gs:8 and RSP0 set on each switch on x86-64, and the
                 * stack pointer at the return on the Arm pair, where SP_EL1
                 * and the SVC stack already are that. exit_group ends the
                 * task through the scheduler. */
        }
        #implemented action systemCall {
            @stage { number = 6; }
            doc /* The Arm half of system call entry: svc arrives through the
                 * trap vector as Trap::SystemCall, so the architecture reads the
                 * number and arguments from the saved registers, handles exit
                 * and exit_group with leave_user before dispatch, and writes the
                 * result back. x86-64's body refuses, because SYSCALL has an
                 * entry of its own. */
        }
        #implemented action syscallEntry {
            @stage { number = 6; }
            doc /* x86-64: SYSCALL leaves the return address in rcx and does
                 * not switch the stack, so entry swaps to the kernel stack
                 * through swapgs before anything can be pushed
                 * (src/kernel/src/arch/x86_64/syscall.rs). */
        }
        #implemented part iommu : IommuDriver {
            @stage { number = 10; }
            doc /* The unit drivers live in src/kernel/src/iommu rather than under
                 * arch/: VT-d and the SMMUv3, each found from what the DMAR,
                 * the IORT or the device tree describes. What the facade
                 * gained for device interrupts is MSI (msi_allocate,
                 * msi_doorbell) and masking one line (mask_interrupt,
                 * unmask_interrupt). */
        }
    }

    part def PageTableEncoding;
    part def CpuStarter {
        doc /* How a secondary processor is brought from "off" to running the
             * kernel's secondary_main with the MMU on: INIT-SIPI-SIPI and a
             * real-mode trampoline, or PSCI CPU_ON through an identity map of
             * the entry sequence. */
    }
    part def IommuDriver;

    #implemented part def X86_64Arch :> ArchFacade {
        attribute :>> NAME = "x86_64";
        attribute :>> TLB_FLUSH_IS_BROADCAST = false;
        doc /* src/kernel/src/arch/x86_64. Target x86_64-unknown-none, kernel code
             * model, no red zone, soft float. Machine description: ACPI. */

        part gdt : Gdt {
            doc /* Per processor, with its TSS: a TSS cannot be shared because
                 * loading one marks its descriptor busy. */
        }
        part idt : Idt {
            doc /* 256 gates, every one naming the kernel code selector; the
                 * double-fault gate on an IST stack of its own. Entry stubs are
                 * an assembler .rept, ten of which push an error code. */
        }
        part lapic : LocalApic {
            doc /* Mapped from the MADT, enabled, task priority dropped. Its
                 * timer is calibrated against the HPET. One per processor. */
        }
        part ioapics : IoApic[1..*] {
            doc /* Every I/O APIC firmware described is mapped and every input
                 * masked at boot; the inputs the kernel uses are routed one
                 * at a time, which is the 16550's receive interrupt, found
                 * from the MADT. Devices interrupt by MSI-X instead. */
        }
        part clock : X86Clock {
            doc /* HPET main counter, whose period firmware states in
                 * femtoseconds. Falls back to the TSC calibrated against the
                 * PIT on a machine with no HPET table, and to the TSC
                 * calibrated against the HPET when its counter is 32 bits
                 * wide and would wrap every five minutes. */
        }
        part serial : Serial16550 {
            doc /* COM1 through port I/O. The one in-kernel device. */
        }
        part trampoline : ApTrampoline {
            doc /* Real mode to long mode in one step, on a root table below
                 * 1 MiB sharing the kernel's upper half. The only 16-bit code
                 * in the project. */
        }
        part shootdown : TlbShootdown;

        part tscDeadline : TscDeadlineTimer {
            @deferred { reason = "Replaces the LAPIC countdown with a comparator against the TSC; the calibration exists. Waits for a tickless scheduler to want it."; }
        }
        part x2apic : X2ApicMode {
            @deferred { reason = "APIC IDs above 255 are refused with a message; QEMU's are 0 to 3."; }
        }
        #implemented part vtd : IommuDriver {
            @stage { number = 10; }
            doc /* src/kernel/src/iommu/vtd.rs: legacy mode, checked against QEMU's
                 * intel_iommu.c -- a root table per unit, a context table per
                 * bus, a second-level table per domain -- programmed before
                 * PCI enumeration, so a function behind it reaches only what
                 * its domain maps. The boot test's out-of-domain write faults
                 * in the unit's fault recording register. */
        }
        part amdVi : IommuDriver {
            @deferred { reason = "Nothing reads an IVRS table or drives an AMD IOMMU, so on such a machine DMA would not be translated. Left after stage 10's exit: none of it was on the path to rustc."; }
        }
    }

    part def Gdt; part def Idt; part def LocalApic; part def IoApic;
    part def X86Clock; part def Serial16550; part def ApTrampoline;
    part def TlbShootdown; part def TscDeadlineTimer; part def X2ApicMode;

    #implemented part def AArch64Arch :> ArchFacade {
        attribute :>> NAME = "aarch64";
        attribute :>> TLB_FLUSH_IS_BROADCAST = true;
        doc /* src/kernel/src/arch/aarch64. Target aarch64-unknown-none-softfloat.
             * Machine description: ACPI (MADT, FADT for the PSCI conduit,
             * GTDT for the timer interrupt). */

        part vectors : VbarEl1Table {
            doc /* Sixteen entries at fixed 128-byte offsets; the layout is the
                 * interface. Each entry stashes two registers, records which
                 * entry it is and branches to the common save path. */
        }
        part gic : Gicv2Front {
            doc /* aarch64/gic.rs: the MADT walk alone — distributor and CPU
                 * interface addresses, the check that every core's interface
                 * is the same banked address, the version check — then a call
                 * into the shared gicv2 driver. */
        }
        part timer : GenericTimer {
            doc /* The architected virtual timer, CNTV_CVAL_EL0, because a
                 * kernel at EL1 is below any hypervisor present. Compares
                 * against an absolute instant; there is no periodic mode. */
        }
        part serial : Pl011Console {
            doc /* Still aarch64's own copy with a hard-coded address; moving
                 * it onto the shared pl011 driver is the open half of
                 * docs/arm32.md decision 6. */
        }
        part psci : PsciCpuOn {
            doc /* Secondaries start with the MMU off at a physical address and
                 * enter through an identity map of the entry sequence alone,
                 * in a tree of their own, loading every parameter before the
                 * MMU goes on. */
        }
        part el2Drop : El2ToEl1 {
            doc /* Firmware may hand off at EL2; lowering is an eret into a
                 * constructed context. */
        }

        part gicv3 : Gicv3 {
            @deferred { reason = "gic::init refuses anything that is not a GICv2; QEMU virt gives GICv2 unless asked, so this needs a second boot-test configuration as much as code."; }
        }
        part parking : PsciParkingProtocol {
            @deferred { reason = "For firmware without PSCI. Refused, not guessed at."; }
        }
        #implemented part smmu : IommuDriver {
            @stage { number = 10; }
            doc /* src/kernel/src/iommu/smmuv3.rs: every SMMUv3 the IORT describes,
                 * checked against QEMU's smmuv3.c -- a linear stream table
                 * aborting until a domain is attached, stage 2 per stream,
                 * the command queue polled and the event queue on -- with the
                 * GICv2m doorbell mapped into every domain, because QEMU sends
                 * MSI writes through the stream. The boot test's out-of-domain
                 * write faults in the event queue. */
        }
    }

    part def VbarEl1Table; part def Gicv2Front; part def GenericTimer;
    part def Pl011Console; part def PsciCpuOn; part def El2ToEl1;
    part def Gicv3; part def PsciParkingProtocol;

    #implemented part def Armv7aArch :> ArchFacade {
        attribute :>> NAME = "armv7a";
        attribute :>> TLB_FLUSH_IS_BROADCAST = true;
        doc /* src/kernel/src/arch/armv7a: the Cortex-A7 of the STM32MP157, run on
             * QEMU virt under U-Boot. Target armv7a-none-eabi with LPAE.
             * Machine description: device tree only. Joined after stage 3
             * without a second loader, a second facade or a line of bootstrap
             * assembly; docs/arm32.md is the plan it followed. */

        part vectors : Armv7aVectorTable {
            doc /* Eight one-instruction entries at VBAR. Each corrects the
                 * banked lr, stores it and the status on the SVC stack with
                 * srsdb and switches to SVC with cps; return is rfeia. No
                 * processor mode but SVC is ever given a stack. */
        }
        part gic : Gicv2FromFdt {
            doc /* Distributor and CPU interface from the device tree, then the
                 * shared gicv2 driver. */
        }
        part timer : GenericTimerCp15 {
            doc /* The same counter as AArch64's, reached through cp15; the
                 * interrupt number comes from the device tree. */
        }
        part serial : ConsoleChoice {
            doc /* armv7a/console.rs chooses between the ports the device tree
                 * describes, in priority order: `console=pl011|stm32` from
                 * /chosen/bootargs (U-Boot sets it without reflashing, for a
                 * board whose tree is exactly what is in question), then
                 * stdout-path, then the first enabled port. A name matching
                 * no drivable node falls through rather than failing. The
                 * PL011 on QEMU virt, the STM32 USART on the board. */
        }
        part coherency : ActlrReport {
            doc /* Every core reads ACTLR.SMP once it is in Rust and the boot
                 * log says how many had it set. An observation, never a
                 * failure: QEMU does not implement the register, so
                 * "clear on all" is ambiguous there. `noactlr` on the command
                 * line suppresses the read, for silicon that turns it into an
                 * undefined instruction. */
        }
        part psci : PsciCpuOn {
            doc /* Conduit from the device tree: hvc on QEMU, not the smc the
                 * plan first guessed. Secondaries found in /cpus. Entry above
                 * the 2 GiB TTBR0 covers is refused. */
        }

        part boardDeferred : Ed1Ev1Boards {
            @deferred { reason = "The ED1 and EV1 have 1 GiB, whose identity range lands on the direct map; Layout::plan_identity_map refuses rather than guesses, so they need a trampoline page not yet written."; }
        }
        part highRam : RamAbove2GiB {
            @deferred { reason = "RAM beyond the 1.25 GiB direct map: a machine with it boots and reports the excess unused. RAM above the 2 GiB split, which the board's DDR at 3 GiB needs, works since 9ae0180f."; }
        }
        part thumb2 : Thumb2 {
            @deferred { reason = "ARM code generation only, as docs/arm32.md argues."; }
        }
        part vfp : Vfp {
            @deferred { reason = "Soft float; a UEFI application may not assume firmware enabled the VFP."; }
        }
        part smmu : IommuDriver {
            @deferred { reason = "The device tree's SMMUv3 is found and left alone: U-Boot's virtio-pci driver resets when a device offers VIRTIO_F_ACCESS_PLATFORM, so the machine's virtio devices would bypass it anyway, and stage 10's exit runs ARMv7-A in degraded trusted mode, as decided."; }
        }
    }

    part def Armv7aVectorTable; part def Gicv2FromFdt; part def GenericTimerCp15;
    part def ConsoleChoice; part def ActlrReport; part def Ed1Ev1Boards;
    part def RamAbove2GiB; part def Thumb2; part def Vfp;

    variation part def ArchLayer :> ArchFacade {
        doc /* Exactly one is compiled in, chosen by the build target. */
        variant part x86 : X86_64Arch;
        variant part arm64 : AArch64Arch;
        variant part arm32 : Armv7aArch;
    }

    #implemented part def SharedArmDrivers {
        doc /* Register-level drivers under arch/ shared by the two Arm
             * architectures, gated by cfg(any(aarch64, arm)) inside the arch
             * directory where the layering check permits it. */
        part gicv2 : Gicv2 {
            doc /* src/kernel/src/arch/arm_common/gicv2.rs: distributor (machine-wide) and CPU
                 * interface (per core). 0..16 software-generated, 16..32
                 * private peripheral, 32.. shared. IPIs through GICD_SGIR.
                 * Private interrupts' enable bits are banked per core, so the
                 * driver records what the boot core enabled and init_this_cpu
                 * replays the whole set on every other core — the timer
                 * included, which is what stage 5 found missing. */
        }
        part pl011 : Pl011 {
            doc /* src/kernel/src/arch/arm_common/pl011.rs. Used by ARMv7-A today. */
        }
        part stm32Usart : Stm32Usart {
            doc /* src/kernel/src/arch/arm_common/stm32_usart.rs: the board's own UART. */
        }
    }
    part def Gicv2; part def Pl011; part def Stm32Usart;

    // Configurations, one per CI boot.
    part ferrixX86 : Ferrix {
        part :>> kernel { part :>> arch = ArchLayer::x86; }
    }
    part ferrixAArch64 : Ferrix {
        part :>> kernel { part :>> arch = ArchLayer::arm64; }
    }
    part ferrixArmv7a : Ferrix {
        part :>> kernel { part :>> arch = ArchLayer::arm32; }
    }

    // -----------------------------------------------------------------------
    // The workspace: crates and the layering between them
    // -----------------------------------------------------------------------

    package Workspace {
        doc /* docs/ARCHITECTURE.md §9. src/lib/ is host-testable by design and
             * is the only code cargo test, Miri and the fuzzers can reach;
             * src/boot/common/uefi/ and src/kernel/ are reached by the QEMU boot test; tools/common/xtask/ by
             * cargo test. src/user/system/native/ holds the native programs the initramfs
             * carries -- devmgr, the drivers and the runtime they link --
             * built for the kernel's targets and kept out of the host gates.
             * tools/common/check/check-crate-layering.sh keeps the arrows below pointing
             * the right way. */

        enum def CrateKind {
            Library;
            Loader;
            Kernel;
            HostTool;
            FuzzTargets;
        }

        part def Crate {
            attribute path : String;
            attribute kind : CrateKind;
            attribute hostTestable : Boolean;
            attribute forbidsUnsafe : Boolean;
            attribute hostTests : Natural[0..1];
        }

        part def LibraryCrate :> Crate {
            attribute :>> kind = CrateKind::Library;
            attribute :>> hostTestable = true;
        }

        // The five the boot path was built on.
        #implemented part bootinfo : LibraryCrate {
            attribute :>> path = "src/lib/proto/bootinfo";
            attribute :>> forbidsUnsafe = false;
            doc /* The hand-off ABI, both address layouts, checked at compile
                 * time on every build. */
        }
        #implemented part elf : LibraryCrate {
            attribute :>> path = "src/lib/platform/elf";
            attribute :>> forbidsUnsafe = true;
            doc /* ELF64 and, since the ARMv7-A port, ELF32. Used by the
                 * loader, by xtask's PE converter, and fuzzed. */
        }
        #implemented part frameCrate : LibraryCrate {
            attribute :>> path = "src/lib/kernel/frame";
            attribute :>> forbidsUnsafe = true;
        }
        #implemented part heap : LibraryCrate {
            attribute :>> path = "src/lib/kernel/heap";
            attribute :>> forbidsUnsafe = false;
            doc /* The body has no unsafe; the crate cannot forbid it because
                 * declaring Backing as an unsafe trait is the point. */
        }
        #implemented part paging : LibraryCrate {
            attribute :>> path = "src/lib/kernel/paging";
            attribute :>> forbidsUnsafe = false;
        }

        // Written ahead, and reached early.
        #implemented part acpi : LibraryCrate {
            attribute :>> path = "src/lib/platform/acpi";
            attribute :>> forbidsUnsafe = true;
            attribute :>> hostTests = 83;
            doc /* Reached at stage 3: the MADT walk the interrupt controller
                 * needed. Stage 10 uses the rest: the MCFG, GICv2m frames,
                 * the DMAR and the IORT. The acpi_tables fuzz target reads
                 * its input as physical memory and walks it from the RSDP
                 * down, as the kernel does. */
        }
        #implemented part fdt : LibraryCrate {
            attribute :>> path = "src/lib/platform/fdt";
            attribute :>> forbidsUnsafe = true;
            attribute :>> hostTests = 93;
            doc /* Reached at stage 1 on ARMv7-A: console, GIC, timer
                 * interrupt and PSCI conduit come from it there. The
                 * fdt_parse fuzz target holds it to a second walk of the
                 * token stream written from the specification. */
        }
        #implemented part sync : LibraryCrate {
            attribute :>> path = "src/lib/kernel/sync";
            attribute :>> forbidsUnsafe = false;
            attribute :>> hostTests = 30;
            doc /* Reached at stage 4: its ticket locks guard every shared
                 * kernel structure, the kernel's SpinLock being
                 * PreemptSpinLock, which holds off preemption, and
                 * IrqSpinLock masking interrupts as well. SleepLock, whose
                 * waiters sleep on a wait queue the kernel lends, since stage
                 * 11. RwSpinLock still waits. */
        }
        #implemented part crng : LibraryCrate {
            attribute :>> path = "src/lib/kernel/crng";
            attribute :>> forbidsUnsafe = true;
            attribute :>> hostTests = 9;
            doc /* Reached with curl's HTTPS: ChaCha20 with fast key erasure
                 * behind getrandom, /dev/urandom and AT_RANDOM, seeded from
                 * firmware, the CPU and timer jitter. The block function is
                 * checked against RFC 8439 and OpenSSL's keystream. */
        }
        #implemented part sched : LibraryCrate {
            attribute :>> path = "src/lib/kernel/sched";
            attribute :>> forbidsUnsafe = true;
            attribute :>> hostTests = 77;
            @stage { number = 5; }
            doc /* The half of the scheduler that is arithmetic: the EEVDF
                 * tree, weights, lag, the domain partition. cargo test drives
                 * a run queue through hundreds of thousands of decisions. */
        }
        #implemented part vma : LibraryCrate {
            attribute :>> path = "src/lib/kernel/vma";
            attribute :>> forbidsUnsafe = true;
            attribute :>> hostTests = 72;
            doc /* Written for stage 6; reached at stage 2 as the vmap arena's
                 * range map, and since stage 6 every process's address
                 * space. */
        }

        // Written ahead, and reached by their stages since.
        #implemented part linuxAbi : LibraryCrate {
            attribute :>> path = "src/lib/proto/linux-abi";
            attribute :>> forbidsUnsafe = true;
            attribute :>> hostTests = 146;
            @stage { number = 7; }
            doc /* Three system call number tables, not two: x86-64's own,
                 * AArch64's generic one, and ARMv7-A's EABI one. The 32-bit
                 * architecture needs its own Syscall variants as well as its
                 * own numbers -- mmap2 counts its offset in pages, _llseek
                 * returns through a pointer, fstat64 writes a different
                 * structure -- so they are separate calls rather than the
                 * same calls renumbered. Reached early for stage 17: the
                 * DRM/KMS and evdev ioctls and layouts card0 and eventN
                 * answer, each pinned line by line to a committed UAPI
                 * probe run at both widths, and for stage 19 the virtgpu
                 * ioctls the render node answers. */
        }
        #implemented part ustack : LibraryCrate {
            attribute :>> path = "src/lib/kernel/ustack";
            attribute :>> forbidsUnsafe = true;
            attribute :>> hostTests = 22;
            @stage { number = 7; }
            doc /* The initial process stack image: argc, argv, envp and the
                 * auxiliary vector, laid out as a program's _start reads them,
                 * at both pointer widths. Carries a reader as well as a
                 * writer, because the writer is checked by walking the bytes
                 * back out with nothing but a stack pointer -- which is also
                 * what the fuzz target does. */
        }
        #implemented part cpio : LibraryCrate {
            attribute :>> path = "src/lib/fs/cpio";
            attribute :>> forbidsUnsafe = true;
            attribute :>> hostTests = 45;
            @stage { number = 8; }
            doc /* The newc reader the initramfs is unpacked with, reached
                 * through src/lib/fs/vfs. The cpio_parse fuzz target writes every
                 * archive it walks to a trailer back out and requires it to
                 * read back identical. */
        }
        #implemented part vfs : LibraryCrate {
            attribute :>> path = "src/lib/fs/vfs";
            attribute :>> forbidsUnsafe = true;
            attribute :>> hostTests = 128;
            @stage { number = 8; }
            doc /* Dentries with negative entries, mounts, the path walk, open
                 * file descriptions, descriptor tables, tmpfs over a page
                 * store the kernel supplies, pipes and socket buffers, the
                 * permission rules as pure functions, initramfs unpacking and
                 * the getdents64 packer. The kernel's filesystem is built on
                 * it (src/kernel/src/fs). */
        }
        #implemented part procfs : LibraryCrate {
            attribute :>> path = "src/lib/fs/procfs";
            attribute :>> forbidsUnsafe = true;
            attribute :>> hostTests = 36;
            @stage { number = 8; }
            doc /* The text of /proc as pure functions: maps lines padded to
                 * their name column at both pointer widths, meminfo, status,
                 * stat and mounts, pinned byte for byte against lines a real
                 * Linux printed, and the maps parser the kernel's boot check
                 * reads its own output back with. */
        }
        #implemented part sysfs : LibraryCrate {
            attribute :>> path = "src/lib/fs/sysfs";
            attribute :>> forbidsUnsafe = true;
            attribute :>> hostTests = 15;
            doc /* The text of /sys as pure functions (docs/SYSFS.md): PCI
                 * identifiers, modalias and uevent, processor lists, input
                 * capability bitmaps in words of the kernel's long,
                 * connector names, kernfs's relative link targets, and the
                 * name a write to bind or unbind gives; pinned against
                 * Linux's and fuzzed (sysfs_names). */
        }
        #implemented part virtio : LibraryCrate {
            attribute :>> path = "src/lib/drivers/virtio";
            attribute :>> forbidsUnsafe = false;
            attribute :>> hostTests = 180;
            @stage { number = 10; }
            doc /* The split virtqueue as logic over an abstract shared memory,
                 * the PCI transport's status protocol, and each device
                 * class's own protocol: blk, net, gpu, input and console.
                 * Reached at stage 10 by the boot check's virtio-rng driver,
                 * and by every ring-3 driver since. */
        }
        #implemented part netwire : LibraryCrate {
            attribute :>> path = "src/lib/network/netwire";
            attribute :>> forbidsUnsafe = true;
            attribute :>> hostTests = 54;
            doc /* The byte-level half of the net core, written ahead of the
                 * networking stage: Ethernet with one 802.1Q tag, ARP, IPv4
                 * with its options, IPv6 with the extension-header walk,
                 * ICMPv4, ICMPv6 and Neighbor Discovery, UDP, and TCP headers
                 * with their negotiated options, each parsed without
                 * allocation and emitted into a caller's buffer, checksums
                 * verified where the format carries them. The netwire_parse
                 * fuzz target requires every header that parses to emit and
                 * parse back unchanged. */
        }
        #implemented part nettcp : LibraryCrate {
            attribute :>> path = "src/lib/network/nettcp";
            attribute :>> forbidsUnsafe = true;
            attribute :>> hostTests = 31;
            doc /* The TCP state machine, written ahead of the networking
                 * stage and above netwire: the eleven states of RFC 9293 in
                 * the standard's order, reassembly of what arrives out of
                 * order, window scaling, selective acknowledgment blocks,
                 * Nagle, delayed acknowledgments, silly-window avoidance and
                 * the zero-window probe, retransmission timing by RFC 6298
                 * with Karn's algorithm, and NewReno congestion control. It
                 * holds no clock and no address: segments and a time go in,
                 * segments come out, which is what lets its tests drive two
                 * connections against each other over a wire they control.
                 * The nettcp_state fuzz target drives one connection from a
                 * stranger's segments and requires every header it answers
                 * with to write and parse back. */
        }
        #implemented part netCore : LibraryCrate {
            attribute :>> path = "src/lib/network/net";
            attribute :>> forbidsUnsafe = true;
            attribute :>> hostTests = 72;
            doc /* The net core, written ahead of the networking stage:
                 * interfaces and the addresses on them, one routing table for
                 * both families, a neighbour cache that answers ARP's
                 * question and Neighbor Discovery's the same way, IPv4
                 * fragmentation and reassembly under a ceiling a stranger
                 * cannot raise, ICMP echo in both directions with the
                 * unprivileged socket ping uses, UDP with Linux's
                 * socket-matching order, and TCP connections and listeners
                 * over nettcp. It holds no lock, no clock and no device:
                 * frames and a time go in and frames come out, and a packet
                 * routed to the loopback goes back into the input path rather
                 * than out of a driver. The net_input fuzz target drives a
                 * whole host from a stranger's frames. */
        }
        #implemented part netServe : LibraryCrate {
            attribute :>> path = "src/lib/drivers/net/netserve";
            attribute :>> forbidsUnsafe = true;
            attribute :>> hostTests = 12;
            doc /* A ring-3 network driver's serve loop: the net ring on one
                 * side, a virtio-net device on the other, and the rules where
                 * they meet. The two directions are not symmetrical -- sending
                 * is a copy and a submission, while a frame arrives into a
                 * buffer the device chose and takes the oldest receive slot
                 * the kernel posted, or is dropped if none is waiting. A
                 * submission is never taken that cannot be answered, and a
                 * device buffer goes back the moment its bytes are copied,
                 * because a buffer the driver holds is a frame the device
                 * cannot deliver. Written against a trait, so the loop runs on
                 * the host with a fake device on one side and the ring's own
                 * kernel end on the other. */
        }
        #implemented part netRing : LibraryCrate {
            attribute :>> path = "src/lib/proto/netring";
            attribute :>> forbidsUnsafe = true;
            attribute :>> hostTests = 32;
            doc /* The net ring: the memory the kernel shares with a ring-3
                 * network driver, specified in docs/NET-RING.md. The block
                 * ring's discipline with its allocator removed: a frame is
                 * bounded by the MTU, so the data VMO is entries slots of a
                 * fixed size and a submission names its slot, which takes
                 * away the class of bug where a region is reused before its
                 * completion. Private indices, checked reads of the peer's,
                 * the want-bell handshake, and every entry checked when it is
                 * read; corruption is terminal for the side that sees it. */
        }
        #implemented part netlink : LibraryCrate {
            attribute :>> path = "src/lib/network/netlink";
            attribute :>> hostTests = 48;
            doc /* The byte-level half of netlink, over linux-abi's headers:
                 * walking a buffer of messages and the attributes after each
                 * fixed header, and building replies into a caller's buffer
                 * with every length and pad computed rather than taken. Both
                 * walks refuse a length below the header it introduces, one
                 * past the end, and the zero that would walk the same message
                 * for ever, and every step forward is at least a header wide,
                 * so a walk over any bytes at all ends -- which is what lets
                 * the kernel read a buffer a program wrote. It allocates
                 * nothing, so a whole dump is encoded inside the lock over the
                 * net core. The netlink_walk fuzz target requires the walk to
                 * end, what it borrows to lie inside the input, and anything
                 * the builder writes to walk back to what was built. */
        }
        #implemented part virtioNet : LibraryCrate {
            attribute :>> path = "src/lib/drivers/net/virtio-net";
            attribute :>> hostTests = 22;
            @stage { number = 10; }
            doc /* The virtio-net driver logic, written ahead of the network
                 * device it drives from src/user/system/native/drivers/net/virtio-net: bring-up in the order the
                 * status protocol fixes, a receive queue the driver fills and
                 * refills because an empty one drops every frame in silence, a
                 * transmit queue whose buffers stay the caller's until the
                 * device says it has read them, and a drain that acknowledges
                 * the interrupt before it starts. It negotiates no checksum,
                 * segmentation or merge-buffer feature, so a frame is one
                 * buffer and every header is the twelve bytes VERSION_1 makes
                 * it. It holds no handle: a transport, pinned pages and a
                 * header area go in, and frames go each way. The virtio_net
                 * fuzz target drives it from a device whose every register,
                 * used entry and header byte a stranger chose, and requires
                 * every accepted frame to be answered exactly once. */
        }
        #implemented part virtioGpu : LibraryCrate {
            attribute :>> path = "src/lib/drivers/display/virtio-gpu";
            attribute :>> hostTests = 31;
            @stage { number = 17; }
            doc /* The virtio-gpu 2D driver logic, written ahead of the driver
                 * process for the compositor's first iteration and driven
                 * from src/user/system/native/drivers/display/virtio-gpu since: bring-up with
                 * the control queue alone, one command at a time, every
                 * response checked, and the pipeline from the display core's
                 * requests to device commands. A failed step is undone as far
                 * as is safe, and pages the device may still hold are never
                 * unpinned. Its tests drive a frame to a fake device's screen;
                 * the virtio_gpu_pipeline fuzz target requires one reply per
                 * request and no unpin without a pin. Since stage 19 it takes
                 * the features 3D needs when asked. */
        }
        #implemented part virtioSnd : LibraryCrate {
            attribute :>> path = "src/lib/drivers/sound/virtio-snd";
            attribute :>> hostTests = 14;
            doc /* The virtio-snd driver logic, written ahead of the driver
                 * process for the audio iteration (docs/AUDIO.md L4): bring-up
                 * with DRIVER_OK before HELLO, since a sound device describes
                 * its streams only through its control queue; READY into
                 * SET_PARAMS and PREPARE; one transmit chain per SUBMIT, the
                 * header and status from the driver's scratch memory and the
                 * samples from the core's buffer pinned read-only; completions
                 * back to the core oldest first; a halt as STOP, RELEASE and a
                 * fresh prepare. Tested against a device doing what QEMU
                 * 9.2.4's does, with the samples the device read compared to
                 * those submitted. */
        }
        #implemented part virtioInput : LibraryCrate {
            attribute :>> path = "src/lib/drivers/input/virtio-input";
            attribute :>> hostTests = 28;
            @stage { number = 17; }
            doc /* The virtio-input driver logic, written ahead of the driver
                 * process for the input iteration (docs/INPUT.md) and driven
                 * from src/user/system/native/drivers/input/virtio-input since: bring-up
                 * that stops at FEATURES_OK so no event is discarded before
                 * the core has judged the device, the device's description
                 * read as a HELLO, the event queue kept full because QEMU
                 * drops a whole report for want of one buffer, and the events
                 * the device wrote batched into the EVENTS messages the core
                 * reads. It forwards only what the core would publish and
                 * drops the rest; it ends a report the core would refuse for
                 * length and counts the cut; and memory the device may still
                 * write into is never handed back. The virtio_input_driver
                 * fuzz target plays a device and the glue against a real
                 * session, and requires the core never to refuse a message
                 * the batch made and no event to be lost or reordered. */
        }
        #implemented part displayctl : LibraryCrate {
            attribute :>> path = "src/lib/proto/displayctl";
            attribute :>> forbidsUnsafe = true;
            attribute :>> hostTests = 33;
            @stage { number = 17; }
            doc /* The control protocol between the kernel's display core and
                 * a ring-3 display driver, written ahead of both for the
                 * compositor's first iteration (docs/DISPLAY.md). Frames do
                 * not move -- the card VMO's ranges are the device's backing,
                 * or since stage 19 a GPU object ATTACH_OBJECT names -- so
                 * there is no ring, only thirteen fixed messages decoded
                 * strictly, and the core's side of the conversation as a
                 * state machine of fixed capacity that accepts only the reply
                 * it waits for and stays broken once a driver lies. The
                 * displayctl fuzz target requires decoding to be exact and
                 * the session never to accept a flip out of order. */
        }
        #implemented part inputctl : LibraryCrate {
            attribute :>> path = "src/lib/proto/inputctl";
            attribute :>> forbidsUnsafe = true;
            attribute :>> hostTests = 26;
            @stage { number = 17; }
            doc /* The control protocol between the kernel's input core and a
                 * ring-3 input driver, and evdev's per-open queues, written
                 * ahead of both for the input iteration (docs/INPUT.md). Six
                 * fixed messages decoded strictly; the core's side of the
                 * conversation, which publishes only the event types it
                 * supports, refuses a message holding any event the driver
                 * did not declare before any of it takes effect, keeps the
                 * device state as Linux's input_get_disposition does and
                 * delivers whole reports to the grabbing open or to all; and
                 * the queue, Linux evdev's ring with its SYN_DROPPED rule,
                 * packet boundary, flush on a state read and clock change,
                 * read as input_event at both widths. The inputctl fuzz
                 * target requires decoding to be exact, every delivered
                 * report whole and published, and every drained read to end
                 * at a SYN_REPORT with its clean reports the ones delivered,
                 * in order. */
        }
        #implemented part sndctl : LibraryCrate {
            attribute :>> path = "src/lib/proto/sndctl";
            attribute :>> forbidsUnsafe = true;
            attribute :>> hostTests = 30;
            doc /* The control protocol between the kernel's audio core and a
                 * ring-3 sound driver, and a PCM stream's state as ALSA keeps
                 * it, written ahead of both for the audio iteration
                 * (docs/AUDIO.md L3). Nine fixed messages decoded strictly;
                 * the core's side of the conversation, which publishes the
                 * first stream that plays version 1's one configuration and
                 * refuses any report out of turn; the stream, Linux's
                 * pcm_native.c and pcm_lib.c function by function: states,
                 * pointers and boundary at both widths, thresholds, start,
                 * underrun, drain, SYNC_PTR, STATUS and poll, with what is
                 * submitted to the driver and when; and HW_REFINE and
                 * HW_PARAMS with Linux's interval and mask rules. The sndctl
                 * fuzz target drives requests, completions and lies at a
                 * stream and requires it never to overrun, never to submit
                 * across a period or during a halt, and to refuse every lie. */
        }
        #implemented part logctl : LibraryCrate {
            attribute :>> path = "src/lib/proto/logctl";
            attribute :>> forbidsUnsafe = true;
            attribute :>> hostTests = 12;
            doc /* The log control protocol: how a ring-3 driver reads the
                 * kernel log over a control channel, for the Pixel 7's USB
                 * serial port (docs/vendor/google/pixel7/USB-HANDOVER.md phase 4). Three
                 * messages decoded strictly -- READ, DATA with the bytes the
                 * ring lost since the last, REFUSED -- and the kernel's half
                 * of the conversation: one READ outstanding, answered once
                 * the log has a byte, lost counts carried to the next DATA. */
        }
        #implemented part audit : LibraryCrate {
            attribute :>> path = "src/lib/proto/audit";
            attribute :>> forbidsUnsafe = true;
            attribute :>> hostTests = 7;
            doc /* The audit record's 64-byte layout, shared by the kernel's
                 * store (src/kernel/src/audit.rs) and the reader it hands the
                 * records to (docs/certification/AUDIT.md §2): the classes,
                 * the events, the configuration keys, and the start-up
                 * record that carries the boot's 128-bit audit id and both
                 * ring lengths, round-tripped through the bytes by its host
                 * tests. */
        }
        #implemented part pci : LibraryCrate {
            attribute :>> path = "src/lib/platform/pci";
            attribute :>> forbidsUnsafe = true;
            attribute :>> hostTests = 51;
            @stage { number = 10; }
            doc /* PCI configuration space over a ConfigSpace the caller
                 * implements: ECAM geometry, headers, BAR decoding and sizing,
                 * both capability lists with a visited set, MSI-X, the bus
                 * walk without recursion or allocation, and virtio's PCI
                 * transport. The kernel's enumeration walks with it
                 * (src/kernel/src/discovery/pci.rs). The pci_walk fuzz target requires
                 * every walk to end and sizing to restore every register. */
        }
        #implemented part nativeAbi : LibraryCrate {
            attribute :>> path = "src/lib/proto/native-abi";
            attribute :>> forbidsUnsafe = true;
            attribute :>> hostTests = 23;
            @stage { number = 9; }
            doc /* The native ABI's numbers, handle values, rights, signals,
                 * errno names and repr(C) layouts. One table on every
                 * architecture, in 0x1000..=0x1FFF, tested clear of all three
                 * Linux tables. No argument wider than a register. */
        }
        #implemented part objects : LibraryCrate {
            attribute :>> path = "src/lib/kernel/objects";
            attribute :>> forbidsUnsafe = true;
            attribute :>> hostTests = 29;
            @stage { number = 9; }
            doc /* The handle table and the channel message queue, generic
                 * over what a handle names. A handle is a slot and a
                 * generation, and a slot retires rather than wrap, so a closed
                 * handle never resolves again -- which the handle_table fuzz
                 * target checks against a model. */
        }
        #implemented part btrfs : LibraryCrate {
            attribute :>> path = "src/lib/fs/btrfs";
            attribute :>> forbidsUnsafe = true;
            attribute :>> hostTests = 167;
            @stage { number = 11; }
            doc /* The btrfs read path, allocating nothing: parsing, mount
                 * bootstrap, lookup, readdir and read with every data sector
                 * checked against the checksum tree, and zlib, LZO and zstd
                 * decoders. Tested against real mkfs.btrfs images, fuzzed
                 * from them by btrfs_read, and mounted by the kernel since
                 * stage 11. */
        }
        #implemented part btrfsWrite : LibraryCrate {
            attribute :>> path = "src/lib/fs/btrfs-write";
            attribute :>> forbidsUnsafe = true;
            attribute :>> hostTests = 32;
            @stage { number = 12; }
            doc /* The btrfs write path: copy-on-write trees, delayed refs,
                 * extent and free-space-tree bookkeeping, chunk allocation,
                 * the commit with its flush before the superblock, file
                 * operations, and the log tree fsync writes and the mount
                 * replays. Host btrfs check finds its output clean, both
                 * from host tests and from what the kernel writes in the boot
                 * check, and after power failures cut at thousands of points
                 * on the host and hundreds of QEMU kills. */
        }
        #implemented part btrfsVfs : LibraryCrate {
            attribute :>> path = "src/lib/fs/btrfs-vfs";
            attribute :>> forbidsUnsafe = true;
            attribute :>> hostTests = 35;
            @stage { number = 11; }
            doc /* btrfs mounted into the VFS: FileSystem and Inode over the
                 * read path, read-only and holding no lock across I/O, and a
                 * writable mount over src/lib/fs/btrfs-write behind one sleeping
                 * lock, whose writes become extents at the commit. */
        }
        #implemented part blockQueue : LibraryCrate {
            attribute :>> path = "src/lib/fs/block";
            attribute :>> forbidsUnsafe = true;
            attribute :>> hostTests = 37;
            @stage { number = 11; }
            doc /* The block core's request queue: merging, flush and FUA
                 * barriers no request crosses, deadline scheduling. In front
                 * of every block ring in the kernel (src/kernel/src/interfaces/block_ring).
                 * Checked against a model by the block_queue fuzz target. */
        }
        #implemented part blkRing : LibraryCrate {
            attribute :>> path = "src/lib/proto/blkring";
            attribute :>> forbidsUnsafe = true;
            attribute :>> hostTests = 66;
            @stage { number = 10; }
            doc /* The block ring, docs/BLOCK-RING.md in code: the memory the
                 * kernel shares with a ring-3 block driver, every index and
                 * entry the other side writes checked before it is used,
                 * doorbells over ports, and the HELLO, READY and START
                 * messages, linked by both the kernel's ring glue and the
                 * driver. The blkring fuzz target plays one side against an
                 * honest other. */
        }
        #implemented part blkServe : LibraryCrate {
            attribute :>> path = "src/lib/drivers/block/blkserve";
            attribute :>> forbidsUnsafe = true;
            attribute :>> hostTests = 8;
            @stage { number = 10; }
            doc /* A ring-3 block driver's serve loop between the block ring and
                 * a virtio-blk device, written against a trait so it runs on
                 * the host with a fake device; src/user/system/native/drivers/block/virtio-blk adds only the
                 * handles. */
        }
        #implemented part virtioBlk : LibraryCrate {
            attribute :>> path = "src/lib/drivers/block/virtio-blk";
            attribute :>> hostTests = 39;
            @stage { number = 10; }
            doc /* The virtio-blk driver logic over a transport and DMA memory
                 * it is handed: bring-up to DRIVER_OK, read, write and flush,
                 * each completion counted once even from a hostile device, and
                 * memory handed back only after the device's reset. Driven
                 * from src/user/system/native/drivers/block/virtio-blk; fuzzed by virtio_blk. */
        }
        #implemented part devmgrProto : LibraryCrate {
            attribute :>> path = "src/lib/proto/devmgr-proto";
            attribute :>> forbidsUnsafe = true;
            attribute :>> hostTests = 7;
            @stage { number = 10; }
            doc /* The messages between the kernel and devmgr on its bootstrap
                 * channel as docs/DEVMGR.md fixes them -- DEVICES, REPORT,
                 * PUBLISHED, DIED and RESTARTED -- so the kernel and the
                 * program share one definition. */
        }
        #implemented part nativeCrate : LibraryCrate {
            attribute :>> path = "src/lib/proto/native";
            attribute :>> forbidsUnsafe = true;
            attribute :>> hostTests = 30;
            doc /* Typed, safe wrappers over every native system call, with
                 * owned handles, written against a raw call trait: src/user/system/native/rt
                 * implements it with the trap instruction, and the tests with
                 * a recorder that plays the kernel's part. */
        }
        #implemented part renderctl : LibraryCrate {
            attribute :>> path = "src/lib/proto/renderctl";
            attribute :>> forbidsUnsafe = true;
            attribute :>> hostTests = 22;
            @stage { number = 19; }
            doc /* The render control protocol between the kernel's render core
                 * and a ring-3 GPU driver (docs/GPU.md §3.3), in displayctl's
                 * shape: fixed messages decoded strictly, command buffers
                 * passed through as bytes the driver understands and the core
                 * does not, and the core's side as a state machine. Fuzzed by
                 * renderctl. */
        }
        #implemented part gc400 : LibraryCrate {
            attribute :>> path = "src/lib/drivers/gpu/gc400";
            attribute :>> forbidsUnsafe = true;
            attribute :>> hostTests = 28;
            @stage { number = 19; }
            doc /* The STM32MP157's Vivante GC400T as logic over registers and
                 * command memory (docs/GPU.md §6.3): the registers and the
                 * command stream from the etnaviv project's MIT-licensed
                 * register database, the identity, reset and initialisation
                 * in the order Linux's etnaviv driver uses them, and a
                 * WAIT/LINK ring. Linked by src/user/system/native/drivers/gpu/gc400, which devmgr starts
                 * on the board, where it ran on 2026-09-24; no QEMU machine
                 * has the core, so no gate drives it. */
        }
        #implemented part fbtext : LibraryCrate {
            attribute :>> path = "src/lib/kernel/fbtext";
            attribute :>> forbidsUnsafe = true;
            attribute :>> hostTests = 30;
            doc /* Text and filled rectangles on a linear framebuffer, with an
                 * embedded Spleen 8x16 font, clipped rather than refused and
                 * allocating nothing: what the panic screen
                 * (src/kernel/src/panic/screen.rs) draws with, and
                 * the term app (ferrix-os/apps) as well. */
        }
        #implemented part qr : LibraryCrate {
            attribute :>> path = "src/lib/kernel/qr";
            attribute :>> forbidsUnsafe = true;
            attribute :>> hostTests = 20;
            doc /* A QR code encoder with no heap, ported from Linux's
                 * drm_panic_qr.rs, for the panic screen's report. */
        }
        #implemented part vdagent : LibraryCrate {
            attribute :>> path = "src/lib/drivers/console/vdagent";
            attribute :>> forbidsUnsafe = true;
            attribute :>> hostTests = 17;
            doc /* SPICE's vdagent protocol as bytes, the chunk framing and the
                 * clipboard messages (docs/CLIPBOARD.md §4). Written ahead of
                 * the clipboard agent; since 2026-09-27 that agent,
                 * src/user/system/linux/compositor/vdagent, speaks it, and cargo
                 * xtask test-clipboard drives it both ways. */
        }
        #implemented part virtioConsole : LibraryCrate {
            attribute :>> path = "src/lib/drivers/console/virtio-console";
            attribute :>> forbidsUnsafe = false;
            attribute :>> hostTests = 15;
            doc /* The virtio-console driver: bring-up, the control
                 * conversation until a named port is open, and that port's
                 * bytes (docs/CLIPBOARD.md §3). Linked by src/user/system/native/drivers/console/vport, which
                 * devmgr starts when --clipboard puts the device on the bus;
                 * cargo xtask test-clipboard drives it (2026-09-27). */
        }

        // Owed by later stages.
        #planned part seccompBpf : LibraryCrate {
            attribute :>> path = "src/lib/kernel/seccomp";
            attribute :>> forbidsUnsafe = true;
            @stage { number = 13; }
            doc /* The classic-BPF interpreter as a pure function over bytes. */
        }

        #implemented part bootCrate : Crate {
            attribute :>> path = "src/boot/common/uefi";
            attribute :>> kind = CrateKind::Loader;
            attribute :>> hostTestable = false;
            attribute :>> forbidsUnsafe = false;
        }
        #implemented part kernelCrate : Crate {
            attribute :>> path = "src/kernel";
            attribute :>> kind = CrateKind::Kernel;
            attribute :>> hostTestable = false;
            attribute :>> forbidsUnsafe = false;
        }
        #implemented part xtask : Crate {
            attribute :>> path = "tools/common/xtask";
            attribute :>> kind = CrateKind::HostTool;
            attribute :>> hostTestable = true;
            attribute :>> forbidsUnsafe = false;
            attribute :>> hostTests = 395;
            doc /* Cross-compiles both halves and the native programs, writes
                 * the FAT32 image and the initramfs with its own writers,
                 * converts the ARMv7-A loader ELF to PE32, drives QEMU, runs
                 * the NAT gateway the network tests sit behind, and runs every
                 * gate. Nothing from crates.io. It no longer forbids unsafe:
                 * a few calls into Windows, for the console and an ICMP echo,
                 * each with its SAFETY comment. */
            attribute commands : String[68] = ("build", "run", "run-compositor", "run-badapple", "remote-desktop", "wallpapers", "everything-volume", "test-btrfs", "test-powerfail", "test-boot", "test-kaslr", "test-shell", "test-vfs", "test-net", "test-clipboard", "test-adb", "test-display", "test-compositor", "test-video", "test-input", "test-audio", "test-badapple", "test-seat", "test-pty", "test-foot", "test-vkgears", "test-jobs", "test-init", "bench-seam", "test-auth", "test-restart", "test-install", "test-sysfs", "test-threads", "test-sem", "test-procfs", "test-rustc", "test-chrome", "test-steamcmd", "test-steam-bootstrap", "test-steam-window", "run-steam", "test-yserver", "test-xwindow", "test-chrome-window", "test-chrome-audio", "bench-chrome", "bench-chrome-video", "test-selfhost", "builds-execute", "coverage", "check", "host-clippy", "host-test", "host-doctest", "host-doc", "check-ferrousli", "miri", "native-clippy", "model-doc", "busybox", "uutils", "ports", "flash", "watch-serial", "deploy", "omz", "zsh-functions");
        }
        #implemented part fuzz : Crate {
            attribute :>> path = "src/tests/fuzz";
            attribute :>> kind = CrateKind::FuzzTargets;
            attribute :>> hostTestable = true;
            attribute :>> forbidsUnsafe = false;
        }

        // Dependency edges, as Cargo.toml has them ([dependencies] only, and
        // only to crates this package names).
        dependency from kernelCrate to acpi, blkRing, blockQueue, bootinfo, btrfs, btrfsVfs,
            btrfsWrite, crng, devmgrProto, displayctl, elf, fbtext, fdt, frameCrate, heap,
            inputctl, linuxAbi, nativeAbi, netCore, netlink, netRing, nettcp, netwire, objects,
            paging, pci, procfs, qr, renderctl, sched, sync, sysfs, ustack, vfs, virtio, vma;
        dependency from bootCrate to bootinfo, elf, paging;
        dependency from xtask to elf, netwire;
        dependency from fuzz to acpi, blkRing, blockQueue, btrfs, cpio, displayctl, elf, fdt,
            frameCrate, inputctl, linuxAbi, nativeAbi, netCore, netlink, nettcp, netwire,
            objects, pci, renderctl, sndctl, sync, ustack, vfs, virtio, virtioBlk, virtioGpu,
            virtioInput, virtioNet;
        dependency from blkRing to linuxAbi, nativeAbi;
        dependency from blkServe to blkRing, virtio, virtioBlk;
        dependency from btrfsVfs to btrfs, btrfsWrite, sync, vfs;
        dependency from btrfsWrite to btrfs;
        dependency from displayctl to linuxAbi, nativeAbi;
        dependency from inputctl to linuxAbi, nativeAbi;
        dependency from sndctl to linuxAbi, nativeAbi;
        dependency from nativeAbi to linuxAbi;
        dependency from nativeCrate to linuxAbi, nativeAbi;
        dependency from netCore to netwire, nettcp;
        dependency from netlink to linuxAbi;
        dependency from netRing to linuxAbi, nativeAbi;
        dependency from netServe to netRing, virtioNet;
        dependency from nettcp to netwire;
        dependency from objects to nativeAbi;
        dependency from renderctl to nativeAbi;
        dependency from ustack to linuxAbi;
        dependency from vfs to cpio, linuxAbi, sync;
        dependency from virtio to linuxAbi;
        dependency from virtioBlk to virtio;
        dependency from virtioConsole to virtio;
        dependency from virtioGpu to displayctl, virtio;
        dependency from virtioInput to inputctl, linuxAbi, virtio;
        dependency from virtioSnd to linuxAbi, sndctl, virtio;
        dependency from virtioNet to virtio;

        // Build targets.
        attribute kernelTargets : String[3] = ("x86_64-unknown-none", "aarch64-unknown-none-softfloat", "armv7a-none-eabi");
        attribute loaderTargets : String[3] = ("x86_64-unknown-uefi", "aarch64-unknown-uefi", "armv7-unknown-linux-musleabi");
        attribute toolchain : String = "1.97.1";
    }
}
