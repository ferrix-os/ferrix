/*
 * Ferrix — SysML v2 model, part 10: the roadmap.
 *
 * docs/ROADMAP.md as requirements: one per stage, each with its exit
 * criterion, its status, the boot test that verifies it, and the part of
 * the system that satisfies or will satisfy it. Each stage's status
 * keyword says whether it is done; docs/ROADMAP.md's "Where it stands" is
 * the prose this follows.
 */
package FerrixRoadmap {
    private import ScalarValues::*;
    private import VerificationCases::*;
    private import FerrixLifecycle::*;
    private import FerrixStructure::*;
    private import FerrixRequirements::*;

    requirement def Stage {
        doc /* Two rules govern the ordering: every stage ends in something
             * that runs, and nothing is stubbed that a later stage has to
             * unpick. Sizes are order-of-magnitude and not a schedule. */
        attribute number : Natural;
        attribute status : StageStatus;
        attribute size : String;
        subject os : Ferrix;
        requirement exitCriterion {
            doc /* How the stage knows it is finished: a QEMU boot that
                 * demonstrates the capability and stays in CI. */
        }
    }

    verification def BootTest {
        doc /* cargo xtask test-boot --arch <a>: boot firmware, loader and
             * kernel under QEMU and require FERRIX-BOOT-OK within the
             * timeout. Every stage's exit criterion is a self-check in kmain
             * and a line in this log. Under tcg by default, except an x86-64
             * guest on an x86-64 Linux host outside CI, which runs under kvm;
             * kvm and --accel auto use the host's MMU and TLB, which is the
             * only way a missing invalidation is reachable. */
        subject os : Ferrix;
        attribute arch : IsaFamily;
        attribute marker : String = "FERRIX-BOOT-OK stages 1-12";
    }

    // -----------------------------------------------------------------------
    // Done
    // -----------------------------------------------------------------------

    #implemented requirement <'S0'> stage0Foundation : Stage {
        attribute :>> number = 0;
        attribute :>> status = StageStatus::Done;
        attribute :>> size = "weekend";
        doc /* Workspace, the quality gates ported from Starling, CI, the first
             * host-testable libraries. Exit: cargo xtask check passes on an
             * empty tree. */
    }

    #implemented requirement <'S1'> stage1Boot : Stage {
        attribute :>> number = 1;
        attribute :>> status = StageStatus::Done;
        attribute :>> size = "week";
        doc /* UEFI loader in Rust to a kernel Rust entry point, zero
             * bootstrap assembly. Exit, met on all three: the hand-off's
             * magic, version and layout agree; the memory map is sorted,
             * non-overlapping, has usable RAM and describes the loader's own
             * allocations; the direct map aliases physical memory; the kernel
             * can walk and extend the loader's tables. */
    }

    #implemented requirement <'S2'> stage2Memory : Stage {
        attribute :>> number = 2;
        attribute :>> status = StageStatus::Done;
        attribute :>> size = "week";
        doc /* Buddy allocator, heap, vmap arena with guard pages and stacks,
             * identity map dropped, W^X sweep, boot memory reclaimed, empty
             * slab pages returned. Exit, met: 4096 blocks allocated and freed
             * with the free count returning exactly; Box, Vec, BTreeMap;
             * arena and stack checks; address zero translates to nothing; the
             * sweep finds no writable-and-executable leaf. */
        requirement perCpuCaches {
            @deferred { reason = "Needs a workload that can measure them; the per-CPU area exists since stage 4."; }
        }
    }

    #implemented requirement <'S3'> stage3TrapsInterruptsTime : Stage {
        attribute :>> number = 3;
        attribute :>> status = StageStatus::Done;
        attribute :>> size = "week";
        doc /* Vectors on every architecture, one dispatch above them; LAPIC,
             * I/O APIC, HPET (TSC/PIT fallback); GICv2 and the architected
             * virtual timer; the facade's irq::register, timer::after and
             * trap::Frame. Exit, met: two breakpoints with a register canary,
             * four page faults with an exact frame bound, a one-shot that
             * fires once, a thousand ticks measured against the counter at
             * 998 to 999 Hz for a requested 1000. */
        requirement gicv3 {
            @deferred { reason = "Refused rather than guessed; needs a second boot-test configuration."; }
        }
        requirement tscDeadline {
            @deferred { reason = "Calibration exists; waits for a tickless scheduler."; }
        }
    }

    #implemented requirement <'S4'> stage4Smp : Stage {
        attribute :>> number = 4;
        attribute :>> status = StageStatus::Done;
        attribute :>> size = "week";
        doc /* Every processor online: INIT-SIPI-SIPI and a real-mode
             * trampoline, PSCI CPU_ON through an identity map of the entry;
             * a record per processor; every "one CPU" global now a lock; IPIs;
             * TLB shootdown where hardware does not broadcast; grace periods.
             * Exit, met: four processors increment one counter under one
             * ticket lock 25,000 times each to exactly 100,000 with their
             * shares overlapping in time, while an unlocked count beside it
             * loses updates. */
        requirement x2apic { @deferred { reason = "APIC IDs above 255 refused; QEMU's are 0 to 3."; } }
        requirement psciParking { @deferred { reason = "Refused, not guessed at."; } }
        requirement cpuOffline { @deferred { reason = "Nothing takes a processor offline."; } }
    }

    #implemented requirement <'SA'> armv7aPort : Stage {
        attribute :>> number = 4;
        attribute :>> status = StageStatus::Done;
        attribute :>> size = "month";
        doc /* The third architecture, joined after stage 3 and brought through
             * stage 4: the same loader converted ELF to PE32, a 32-bit layout
             * argued rather than shrunk, LPAE as a paging geometry, device
             * tree only, traps without mode stacks. Exit, met: the same
             * self-checks and the same marker under U-Boot on QEMU virt. */
        #implemented requirement ramAbove2GiB {
            doc /* RAM above the 2 GiB split, where the board's DDR starts
                 * at 3 GiB: the loader maps itself where the kernel can
                 * reach it, since 9ae0180f. QEMU's virt machine cannot place
                 * RAM there, so the board run is what exercises it. */
        }
        #implemented requirement boardUart {
            doc /* The STM32 USART driver and the flash/watch-serial/deploy
                 * path, run on hardware on 2026-09-13 at fd4442e: an
                 * STM32MP157D-DK1 at two cores reached FERRIX-BOOT-OK stages
                 * 1-9 and ran stage 7's script, its exit line whole once the
                 * console drained before power-off. */
        }
        #implemented requirement lazyVfp {
            doc /* ARMv7-A's user state for the DK1 IPC matrix
                 * (docs/OPAQUE-KERNEL.md §9.14, §9.15): TPIDRURO kept in the
                 * task and TPIDRURW switched (F-66), the blocking native calls'
                 * VFP reset, and since §9.15 FPEXC.EN clear for a program that
                 * has never used VFP, so that two native programs trade a core
                 * with no VFP work at all; its first VFP instruction is taken
                 * and re-run, and the register file is zero whenever EN is
                 * clear (L.armv7a.21-26, F-68). Stage 9's lazy, vectors and
                 * tls lines; the board's ABAB figure is os-07's. */
        }
        requirement ed1Ev1Boards { @deferred { reason = "1 GiB boards put RAM's identity range on the direct map; plan_identity_map refuses, so they need a trampoline page."; } }
        requirement hardwareBootTest { @deferred { reason = "Automated hardware boot testing beyond watch-serial."; } }
    }

    // -----------------------------------------------------------------------
    // Ahead
    // -----------------------------------------------------------------------

    #implemented requirement <'S5'> stage5Scheduler : Stage {
        attribute :>> number = 5;
        attribute :>> status = StageStatus::Done;
        attribute :>> size = "week";
        doc /* Task, kernel stacks, context switch, per-CPU runqueues, the
             * class stack, EEVDF in src/lib/kernel/sched. Scheduling domains from the
             * start with Throughput alone implemented. Exit, met on all three:
             * a thousand kernel threads spawned on one processor run bounded
             * work to completion and give every stack back (about 28,000
             * switches and 1,000 steals when it landed); then twelve spinners
             * at two weights, each required to stay within EEVDF's own bound
             * of its weighted share, the bound printed beside the lag. Three
             * bugs it found: an idle processor never told of work, vmap::free
             * releasing the address before unmapping, and banked private
             * interrupt enables leaving every other core's timer off. */
        #implemented requirement loadBalancing {
            doc /* Stage 5 finished with nothing beyond work stealing on an
                 * idle processor. Added after it, each with a boot check that
                 * fails without it: load tracking, placement, affinity,
                 * periodic balancing that pushes as well as pulls, and slice
                 * scaling. */
        }
    }

    #implemented requirement <'S6'> stage6UserMode : Stage {
        attribute :>> number = 6;
        attribute :>> status = StageStatus::Done;
        attribute :>> size = "week";
        doc /* AddressSpace, VMOs, the VMA tree as a process map, demand
             * paging, copy-on-write, the ELF loader, the ring-3/EL0/USR
             * transition. Exit, met on all three: a program at user privilege
             * writes to fd 1 and exits with 42, with a page fault serviced
             * along the way -- counted either side of the program and required
             * to be non-zero. Down to user mode by sysretq on x86-64, eret to
             * EL0 on AArch64 and rfeia to USR on ARMv7-A, each onto a dedicated
             * entry stack. Also landed: fork with copy-on-write, TLB
             * invalidation where a live mapping changes, tasks carrying an
             * address space with the root swapped in choose_next, and a read of
             * an inaccessible region refused. Left: scoping the user TLB
             * shootdown, the missing invalidation in protect, and reading the
             * console on Arm. */
    }

    #implemented requirement <'S7'> stage7LinuxAbi : Stage {
        attribute :>> number = 7;
        attribute :>> status = StageStatus::Done;
        attribute :>> size = "month";
        doc /* Syscall entry on every architecture, the dispatch table, the
             * core surface: memory, files, process, threads and futex,
             * signals with sigaltstack and rt_sigreturn, time, identity.
             * Exit, met on all three with the script given to sh -c: Alpine's
             * static musl busybox runs a builtins-only script and exits with
             * its status, required line by line by cargo xtask test-shell,
             * which is outside the boot test because it needs a binary the
             * repository does not carry.
             *
             * Built: the three number tables in src/lib/proto/linux-abi, the startup
             * stack in src/lib/kernel/ustack, dispatch in src/kernel/src/syscall, the copy
             * layer, the ELF loader, and the calls a static binary makes --
             * mmap, mmap2, munmap, mprotect, brk, set_tid_address, read and
             * write/writev on the console, the clocks, getrandom, uname, the
             * identity calls, and rt_sigaction, rt_sigprocmask and
             * sigaltstack recorded without delivery; exit_group, arch_prctl
             * and set_tls in each architecture's trap path. Running foreign
             * binaries found a Thumb entry point entered in ARM state and the
             * FPU closed to user mode on both Arm kernels. Since the exit:
             * programs are scheduled tasks, preempted in user mode, with their
             * thread pointer and FPU state switched per task, load/start/kill
             * on Process, and two programs taking turns in the boot test;
             * then fork, vfork and clone without threads, execve, wait4 and
             * waitid, and the process-group and session calls, with a forking
             * and an exec'ing program in the boot test, and the forking one's
             * frames required back once its tasks are reaped; futex waits, wakes and
             * requeues, and clone3 through the same path as clone;
             * descriptors closed when a process ends; and the edge calls
             * busybox makes -- prctl, limits, priorities, credentials, sleeps,
             * clock setting, host names, sysinfo, syslog, reboot, and
             * sockets refused; mremap, execveat, unshare and setns; and
             * /proc/self/exe as the resolved file actually loaded. The
             * program init starts is pid 1, and a process's orphans go to the
             * nearest reaping ancestor or else to init, zombies included, with
             * the parent-death signal each asked for.
             *
             * Signals delivered on every return to user mode, on Linux's
             * frames, with rt_sigreturn, stop and continue, SIGPIPE, SIGALRM
             * and faults as signals; an interrupted call restarted under
             * SA_RESTART or with no handler and EINTR otherwise, poll never
             * restarting and nanosleep resuming through restart_syscall, as
             * arch_do_signal_or_restart decides. A sigpaths boot check drives
             * the delivery decisions -- SIGCHLD to a handler with wait4 still
             * reaping, stop and continue, the alarm, the alternate stack, the
             * forced fault, and the SA_RESTART truth table -- each with a
             * negative control.
             *
             * The console a terminal: termios honoured by a line discipline,
             * the terminal and job-control requests, select and pselect6, and
             * Ctrl-C raised on the foreground group.
             *
             * Left: threads, and the stand-ins for a tty, a clock chip and an
             * entropy source. The fault-to-signal catch and an SA_RESTART
             * interrupted read are proven at the kernel's decision, not yet
             * end-to-end by a user program. */
    }

    #implemented requirement <'S8'> stage8Vfs : Stage {
        attribute :>> number = 8;
        attribute :>> status = StageStatus::Done;
        attribute :>> size = "month";
        doc /* Inode and dentry caches, the mount table, fd sharing rules,
             * tmpfs, devfs, procfs, cpio initramfs. Exit: busybox ls -R /proc,
             * cat /proc/self/maps, a script manipulating files under tmpfs. */
    }

    #implemented requirement <'S9'> stage9NativeAbi : Stage {
        attribute :>> number = 9;
        attribute :>> status = StageStatus::Done;
        attribute :>> size = "week";
        doc /* Handle tables, Channel with handle passing, Port, Interrupt,
             * IoMapping, Job; the 0x1000 syscalls. Exit: two processes
             * exchange messages and a handle over a channel, and a Job kill
             * takes down a process tree.
             *
             * Built: the byte-level half, in src/lib/proto/native-abi and src/lib/kernel/objects,
             * host-tested, fuzzed and under Miri; and in src/kernel/src/object and
             * syscall/native, handle tables on every Process, channels carrying
             * handles, VMOs, signals and object_wait_one, Job, ports with
             * object_wait_async, and IoMapping and Interrupt minted from stage
             * 10's device nodes, with interrupts delivered to ports. The exit
             * criterion runs in the boot test: two user-mode programs exchange
             * a message and a VMO handle over a channel, and a job kill ends
             * every program in the job and beneath it. Since: Vmo::hold, which
             * keeps a page a DMA pin holds on its frame, an interrupt that
             * wakes its waiter from the handler, and vmo_map, shared and never
             * executable. Left for later: an EXECUTE right on a VMO handle,
             * with the native loader, sub-page apertures, and process creation
             * in the native ABI. */
    }

    #implemented requirement <'S10'> stage10UserspaceDrivers : Stage {
        attribute :>> number = 10;
        attribute :>> status = StageStatus::Done;
        attribute :>> size = "month";
        doc /* Enumeration, IOMMU domains, devmgr, the shared-ring block
             * protocol, virtio-blk as a user process. Exit: a sector read
             * through a ring-3 driver with the IOMMU on, and a deliberate
             * out-of-domain DMA attempt faulting.
             *
             * Built: PCI configuration space in src/lib/platform/pci, host-tested, fuzzed
             * and under Miri; kernel enumeration from the MCFG and the device
             * tree, in the boot test on all three architectures against a
             * virtio-rng-pci device; device nodes whose apertures and vectors
             * are tokens only device.rs mints; a virtio-rng device driven by DMA
             * from the boot check, 64 bytes on every architecture; MSI-X tables and pending bits withheld
             * from apertures; the DMAR and IORT parsed; the ten
             * defects a review found fixed, apertures screened against
             * everything the kernel owns and each other; the
             * virtio-rng completion delivered by MSI-X on every architecture,
             * through arch::msi_allocate (local APIC vectors, GICv2m SPIs);
             * PCI vectors minted per MSI-X entry and masked at the entry,
             * which stage 9's Interrupt uses; every IOMMU found and each PCI
             * function placed behind one (DMAR, IORT, device tree), virtio's
             * DMA sent through it on x86-64 and AArch64, and VT-d and stage-2
             * table encodings in src/lib/kernel/paging; an iommu::Domain every device
             * node has, which the entropy check's DMA goes through; VT-d
             * programmed at boot on x86-64 and the SMMUv3 under ACPI on
             * AArch64, with translated domains. VMO_PIN pinning VMO pages into
             * a device's domain; the out-of-domain write faulted on x86-64
             * and AArch64. The block ring (ferrix-blkring, BLOCK-RING.md)
             * with its kernel side, block_ring_create and a disk in the devfs
             * registry from an accepted HELLO; the kernel half of devmgr
             * (device_info, device_quiesce, START, bus mastering at the first
             * pin; DEVMGR.md). Exit met: /sbin/blk, the virtio-blk driver in
             * ring 3, started from the boot check with START, reads sectors
             * through the ring with VT-d and the SMMUv3 translating, ARMv7-A
             * in degraded trusted mode. devmgr the program started by the
             * kernel with every device and driver image, starting blk per
             * disk. Still owed after the exit: trusting decoding-off BARs. */
    }

    #implemented requirement <'S11'> stage11BtrfsRead : Stage {
        attribute :>> number = 11;
        attribute :>> status = StageStatus::Done;
        attribute :>> size = "month";
        doc /* Block core, then btrfs stage A. Exit: an image made by real
             * mkfs.btrfs is mounted and a file tree read out byte-for-byte
             * matching what the host wrote.
             *
             * Built, host-side: the btrfs read path in src/lib/fs/btrfs (mount
             * bootstrap, lookup, readdir, read, zlib/LZO/zstd), reading four
             * real mkfs.btrfs images back exactly; the mount over stage 8's
             * traits in src/lib/fs/btrfs-vfs; the block queue in src/lib/fs/block. In the
             * kernel: mount -t btrfs on a registered block device, read-only,
             * file data in the inode's VMO pages. Exit met: the mkfs.btrfs
             * fixture on a second virtio-blk disk, served by the ring-3
             * driver, mounted at /mnt and read back against its manifest
             * on all three architectures. */
    }

    #implemented requirement <'SN'> stageNetworking : Stage {
        attribute :>> number = 11;
        attribute :>> status = StageStatus::Done;
        attribute :>> size = "month";
        doc /* Placed after stage 11 without a number of its own, as the
             * ARMv7-A port sits after stage 4. The net core: AF_UNIX, AF_INET
             * and AF_INET6 sockets, the AF_NETLINK route family, interfaces,
             * routes and loopback; virtio-net as a userspace driver on stage
             * 10's device objects; /proc/net; parsers and the TCP state
             * machine in src/lib/, fuzzed. The host side exists already, in
             * tools/common/xtask/src/gateway/: a NAT gateway on QEMU's dgram backend,
             * written rather than reusing -netdev user because that is slirp
             * and slirp is an optional QEMU build dependency, while tap and
             * unprivileged user namespaces both need privilege a build tool
             * must not ask for. Exit: under that gateway, busybox configures
             * eth0 with ip, route and netstat report through /proc/net, wget
             * fetches a file byte-for-byte, and nc carries a stream over
             * loopback and an AF_UNIX socket. Met: cargo xtask test-net does
             * all of it on all three architectures, and the AF_UNIX stream
             * is proven by the stage 7 boot check, because this busybox's nc
             * has no -U and cannot open a local socket at all. */
    }

    #implemented requirement <'SD'> stageDynamicLinking : Stage {
        attribute :>> number = 11;
        attribute :>> status = StageStatus::Done;
        attribute :>> size = "39 points, spent; ferrousli's port about 34, spent";
        doc /* Placed after Networking without a number of its own: nothing
             * on rustc's path needs it, since std targets static musl. The
             * kernel half loads ET_DYN at a base with relative relocations
             * and honours PT_INTERP with AT_BASE in the auxiliary vector (5
             * points); ferrousli's loader, ld.so with libferrousli.so,
             * binds every relocation type of the three architectures at
             * load, with dynamic TLS and dlfcn.h (21); then glibc's symbol
             * versions and SONAMEs, so a glibc-linked binary loads
             * ferrousli in glibc's place (13). No vDSO, no 32-bit ABI. Exit:
             * a distribution's dynamic glibc busybox runs stage 7's
             * test-shell script with its own ld-linux, then with ferrousli's
             * loader in glibc's place, on all three architectures.
             *
             * The first half is met on all three architectures and the
             * second on x86-64 (2026-09-21): the kernel half, the loader
             * with symbol versions, COPY relocations, every TLS form and
             * dlfcn.h, and a libc.so.6 carrying glibc's versions.
             * ferrousli's own port to AArch64 and ARMv7-A, which the
             * customer put inside this stage, and then the loader and the
             * version tables there, completed the exit on all three
             * architectures (2026-09-23). */
    }

    #implemented requirement <'SY'> stageSysfs : Stage {
        attribute :>> number = 12;
        attribute :>> status = StageStatus::Done;
        attribute :>> size = "26 points, spent";
        doc /* Placed after stage 12 without a number of its own, built on
             * 2026-09-24 in the shape the customer chose that day: an
             * in-kernel view like procfs and cgroupfs, fed by the services
             * that own each fact, with bind and unbind sent to devmgr to
             * decide (docs/SYSFS.md). Exit met: the boot check walks a whole
             * sysfs on all three architectures after devmgr has started its
             * drivers and holds it against enumeration, the cores and
             * devmgr, and cargo xtask test-sysfs reads what libdrm reads
             * from a shell and unbinds and binds the card's driver through
             * sysfs. */
    }

    #inProgress requirement <'SC'> stageChrome : Stage {
        attribute :>> number = 12;
        attribute :>> status = StageStatus::InProgress;
        attribute :>> size = "headless and a window spent; the DK1 about 45 to 55 points";
        doc /* Placed after sysfs without a number of its own, asked for by
             * the customer on 2026-09-23 (docs/CHROME.md). Google's prebuilt
             * Chrome for Testing on Debian's glibc from a btrfs volume, by
             * the customer's choice of 2026-09-24. Exit met on x86-64:
             * cargo xtask test-chrome runs it headless, and
             * cargo xtask test-chrome-window in a window on the compositor.
             * On ferrousli in glibc's place, headless and in a window,
             * since 2026-09-26: cargo xtask test-chrome and
             * test-chrome-window with --interpreter ferrousli
             * --library ferrousli.
             * On the persistent btrfs root since 2026-09-26.
             * With its zygote since 2026-09-26, once SCM_CREDENTIALS carried
             * its children's pids; idle at 13% of a processor where it took
             * 443%, after the futex, clock, munmap, shootdown and doorbell
             * fixes and a vDSO (cargo xtask bench-chrome); and playing
             * sound through /dev/snd (cargo xtask test-chrome-audio).
             * Owed: the GPU (inotify landed on 2026-09-27); and the
             * STM32MP157D-DK1, where nothing has run yet. Debian's armhf
             * Chromium runs headless on ARMv7-A under QEMU in 512 MiB
             * (2026-10-08, after ARM's send and recv, cacheflush and mmap of
             * a read-only btrfs were fixed); the SD card has a ring-3 SDMMC1
             * driver (2026-10-08); the cgroup controllers (2026-10-07) let
             * the page cache give back a read-only btrfs's clean pages. */
    }

    #implemented requirement <'S12'> stage12BtrfsWrite : Stage {
        attribute :>> number = 12;
        attribute :>> status = StageStatus::Done;
        attribute :>> size = "about 60 points, spent";
        doc /* btrfs stage B. Exit, strict: Ferrix writes a tree and host
             * btrfs check finds nothing; then the power-fail test — kill QEMU
             * at a random point inside a transaction, remount, replay, check
             * again — over hundreds of seeds. Met: cargo xtask test-btrfs and
             * cargo xtask test-powerfail. Owed beside it: writeback of
             * MAP_SHARED pages. */
    }

    #inProgress requirement <'S13'> stage13Isolation : Stage {
        attribute :>> number = 13;
        attribute :>> status = StageStatus::InProgress;
        attribute :>> size = "month";
        doc /* All eight namespaces, the unified cgroup hierarchy with cpu,
             * memory, io and pids, cgroupfs, classic-BPF seccomp with the
             * interpreter in src/lib/. Exit: an unprivileged user namespace runs
             * pid 1 under a memory limit that triggers scoped reclaim and a
             * scoped OOM kill, with a seccomp filter blocking a syscall.
             *
             * Cgroups first (customer, 2026-09-23): stage 15's init is
             * planned as if they exist, so the cgroup half is built before
             * namespaces and seccomp. docs/INIT.md section 0.1 lists what
             * init needs, C1-C5 before its first boot. C8, accepted the
             * same day: every cgroup is backed by a Job. docs/CGROUPS.md is
             * the design: landings G1-G5 (27 points) for init, then the
             * controllers (58), 85 for the cgroup half. G1 done on
             * 2026-09-23: every process in exactly one job, fork inheriting
             * it, populated counted exactly, the two kills apart. G2
             * done the same day: cgroupfs mounts as cgroup2, over
             * src/lib/fs/cgroupfs. G3 and G4 done on 2026-09-24: cgroup.events
             * wakes poll, select and epoll with POLLPRI; clone3 starts a
             * child in a cgroup; chown delegates a subtree under cgroup v2's
             * common-ancestor rule. C1-C5 and C7 are met. G5 done on
             * 2026-09-24. P1, M1's charging and S1 done on 2026-09-26, as
             * the certification's job quotas (F-35): a quota slot per job,
             * charged for tasks, memory and native objects, a weight per
             * job scaling its tasks', and cgroupfs's cpu, memory and pids
             * files over the same slot. M1's scoped OOM kill done on
             * 2026-09-26: a fault past a cgroup's memory.max kills the
             * process with the most resident pages in that cgroup, counted
             * in memory.events with POLLPRI. M2's reclaim of a read-only mount's
             * clean pages, memory.high, cgroup.freeze, cpu.max with cpu.stat
             * and the io controller with io.max landed on 2026-10-07.
             *
             * Namespaces, for Steam's container, designed in
             * docs/NAMESPACES.md (39 points, N1 to N6) and reviewed by the
             * certification consultant on 2026-09-28. N1 done on
             * 2026-09-28: per-mount flags enforced, MS_REMOUNT and
             * mountinfo. N2 done on 2026-09-30: binds, MS_REC, MNT_DETACH
             * of a subtree, and a superblock per filesystem so a plain
             * remount reaches every bind of it. N3, mount
             * namespaces with pivot_root and openat2, on 2026-09-30. The user,
             * UTS, IPC, cgroup, pid and network namespaces are in too, and so
             * are seccomp's classic-BPF checker and interpreter, its hook at
             * the four system-call entries and its filters (S1 to S3, the
             * last on 2026-10-04). Left: time namespaces, SECCOMP_RET_TRAP and
             * TSYNC (S4 to S6), and test-container, the exit criterion as a
             * program. */
    }

    #planned requirement <'S14'> stage14RealTime : Stage {
        attribute :>> number = 14;
        attribute :>> status = StageStatus::Planned;
        attribute :>> size = "month";
        doc /* SoftRt and HardRt: FIFO/RR, threaded interrupts, PI mutexes,
             * EDF with CBS admission, the runtime mode switch with its
             * quiescence protocol. Exit: a cyclictest-shaped boot test on a
             * HardRt domain while a Throughput domain is saturated, maximum
             * wake-up latency inside the stated bound; plus an admission test
             * that refuses an unschedulable set. */
    }

    #inProgress requirement <'S15'> stage15Userland : Stage {
        attribute :>> number = 15;
        attribute :>> status = StageStatus::InProgress;
        attribute :>> size = "week, about 20 points, of which job control is spent";
        doc /* Static musl busybox as /bin, a working init, job control, ttys,
             * pipes. Exit: an interactive shell over serial a person can use.
             *
             * Most of it arrived under other stages' names: /bin is the
             * uutils family and zinc rather than busybox, pipes are stage
             * 8's, pseudo-terminals stage 18's, and the console's line
             * discipline stage 8's over stage 7's receive interrupt.
             *
             * Job control landed on 2026-09-19, in the shell rather than the
             * kernel: every call it is made of -- setpgid, TIOCSPGRP,
             * TIOCSCTTY, the line discipline's SIGTSTP, wait4's WUNTRACED --
             * had been answered since stage 7 with nothing using them.
             * src/user/system/linux/zinc/src/jobs.rs puts a pipeline in one process group, hands
             * the terminal to the foreground job and takes it back, and
             * keeps the table jobs, fg, bg, wait, disown and kill %1 name.
             * Verified by jobsSession and by zinc's pty gate.
             *
             * A working init landed on 2026-09-26 (L1 to L4 of
             * docs/INIT.md): /sbin/init over src/lib/init/svc's manager, a cgroup
             * per service, a getty on the console, shutdown by SIGTERM; the
             * same day svc and its control socket, readiness, socket
             * activation and resource limits (L5 to L7, L9) and the
             * directory with native services (L8), and the images booting
             * it with the compositor as its service (L10). Verified by
             * initSession. devmgr on the restart policy (L11) the same day;
             * the customer counts the init done there. L12, pid 1 starting
             * devmgr through a kernel starter, on 2026-09-27; L13, the
             * sandboxing keys, over stage 13's namespaces: L13a
             * (2026-10-04) and L13b, PrivateNetwork= (2026-10-05), landed;
             * L13c, SystemCallFilter= over seccomp's S3, landed on
             * 2026-10-08.
             *
             * Next: authentication (docs/AUTH.md, approved by the customer
             * on 2026-09-26): authd, passwords checked by Argon2id, passwd,
             * authctl and a real hyprlock, phase 1, 27 points, not started;
             * its P0, process_create giving a child root's credentials
             * rather than its creator's, 2 points, fixed.
             *
             * Designed on 2026-09-23 in docs/INIT.md: pid 1 and a service
             * manager, systemd-shaped units, a cgroup per service, a pure
             * manager in src/lib/init/svc behind backends a microkernel could
             * serve. 67 points to L10, all spent; the stage 13
             * cgroups its first boot needs landed on 2026-09-24. */
    }

    #implemented requirement <'S16'> stage16Rustc : Stage {
        attribute :>> number = 16;
        attribute :>> status = StageStatus::Done;
        attribute :>> size = "the goal; about 40 guessed, 8 spent";
        doc /* The remaining syscall surface, the memory scale, the spawn path
             * for rust-lld, a sysroot on btrfs. Exit: rustc hello.rs &&
             * ./hello on Ferrix, in CI.
             *
             * Met on 2026-09-22 on x86-64, verified by rustcTest: the
             * rust-lang.org rustc 1.97.1, run by Debian's ld-linux and
             * linking through cc and rust-lld, from a btrfs volume mounted
             * at /data. What was new: the sysroot, the gate, and faults on
             * a disk file's mapping that fill from the disk. */
    }

    #implemented requirement <'S17'> stage17DisplayAndInput : Stage {
        attribute :>> number = 17;
        attribute :>> status = StageStatus::Done;
        attribute :>> size = "74 points, spent";
        doc /* A display core and a virtio-gpu driver in ring 3 behind
             * /dev/dri/card0, an input core and a virtio-input driver behind
             * /dev/input/eventN, and the calls a Rust event loop makes:
             * epoll, eventfd, FIONBIO, memfd sealing, AF_UNIX with
             * SCM_RIGHTS. Nothing is drawn by the kernel.
             *
             * The display iteration is done (xtask test-display) and so is
             * the input iteration (xtask test-input, which sends a key and a
             * touch through QMP and requires them back out of the nodes on
             * x86-64 and AArch64, with a negative control that must fail).
             * The seat came with the compositor: xtask test-seat types into
             * a window on Ferrix from QEMU's far end, and the stage is met.
             * docs/DISPLAY.md and docs/INPUT.md are the designs. */
    }

    #implemented requirement <'S18'> stage18Compositor : Stage {
        attribute :>> number = 18;
        attribute :>> status = StageStatus::Done;
        attribute :>> size = "96 points, spent";
        doc /* The compositor itself: the Wayland wire protocol and its
             * server, xdg-shell, wl_shm, the dwindle and master layouts, a
             * CPU renderer, hyprland.conf and the hyprctl socket. Exit: two
             * real Wayland clients tiled on Ferrix's screen, pixel for pixel
             * as the renderer draws them, which xtask test-compositor
             * requires on x86-64 and AArch64. Met. */
    }

    #inProgress requirement <'S19'> stage19HyprlandFidelity : Stage {
        attribute :>> number = 19;
        attribute :>> status = StageStatus::InProgress;
        attribute :>> size = "178 points, about 8 left";
        doc /* What makes a Hyprland rather than a tiling compositor:
             * animations with bezier curves, rounded corners, blur, shadows,
             * opacity rules, special workspaces, groups, multiple monitors,
             * plugins -- and the GPU behind them.
             *
             * Well under way: every one of Hyprland's globals, dispatchers
             * and hyprctl commands is answered, and a person's own
             * hyprland.conf -- 377 lines, a bar, a dock and a wallpaper
             * daemon -- runs with no diagnostic.
             *
             * The GPU path, decided on 2026-09-18 and built on 2026-09-19
             * (docs/GPU.md 3.7 and 3.8), is Path A: the host's driver
             * through virtio-gpu 3D. All four of its pieces are in -- the
             * ring-3 driver's 3D commands, a render node with the virtgpu
             * ioctls and 3D scanout, the host half in xtask, and a Rust
             * virgl encoder behind a renderer trait with the software
             * renderer still under it. The desktop composites on the GPU:
             * a 1920x1080 frame of a video wallpaper behind a blurred
             * translucent terminal went from 39 ms to 12, where 60 fps is
             * 16.7. A card of Ferrix's own is stage 21. Since 2026-09-23 a
             * served desktop takes the 3D card by default (docs/GPU.md 3.9)
             * and the pointer is on virtio-gpu's cursor plane, so moving it
             * draws no frame (docs/GPU.md 3.10). The driver keeps eight
             * commands in flight on the control queue and has the device
             * read a command stream where it lies, and an upload and a
             * stream return once they are on their way, so a frame waits
             * once, for its flush (docs/GPU.md 3.11).
             *
             * Of the 178, about 8 are left: the desktop's speed as it is
             * watched (client pages as texture backing: 8 of 34, the cursor
             * plane and the device queue spent). The pointer-driven options
             * and second-pass effects (xray, no_screen_share, blur_popups,
             * precise_mouse_move) landed on 2026-10-07. The X server the stage
             * counted 40 for is yserver, a Rust X11 server with a rootless
             * Wayland backend of Ferrix's own, 36 points, done on
             * 2026-09-29 (docs/YSERVER.md). The rest
             * of the GPU road is for clients that render for themselves, not
             * for the compositor: zwp_linux_dmabuf_v1 in hyprix and a
             * GBM-shaped allocator, so a client's GPU buffer is shown where
             * it lies, built on 2026-10-07 and landed on 2026-10-08 (docs/GPU.md 3.13), and a Mesa on
             * ferrousli, which is left.
             *
             * Gears, the customer's order of 2026-09-24 (docs/GPU.md 6):
             * vkgears through Venus -- Mesa's Vulkan driver in the guest,
             * the host's GPU under virglrenderer's render server -- on the
             * Linux host, 39 points, done the same day: vkgears draws on
             * the host's RADV (cargo xtask test-vkgears); and gears drawn by
             * the DK1's own Vivante GC400T, an OpenGL ES 2.0 core with no
             * Vulkan in any driver, through a ring-3 driver of Ferrix's own,
             * 32 points, of which G1 and G2 (11) ran on the board the same
             * day: the core runs a command buffer, its events by interrupt.
             *
             * The desktop's own clients, begun on 2026-09-26 at the
             * customer's request: waybar, fuzzel, hyprlock and hypridle in
             * Rust, reading their own configuration files unchanged, over a
             * shared foundation of 21 points. waybar and fuzzel are on
             * main (2026-09-27) and run the customer's own configuration
             * on run-compositor --everything, and hypridle is on main
             * (2026-09-26); hyprlock is on main too (2026-10-03), its lock over authd. */
    }

    #inProgress requirement <'S21'> stage21BareMetalGpu : Stage {
        attribute :>> number = 21;
        attribute :>> status = StageStatus::InProgress;
        attribute :>> size = "unsized, over 100 points";
        doc /* Ferrix on bare metal with an NVIDIA card driven by Ferrix
             * itself: Path B of the GPU decision of 2026-09-18, opened when
             * the customer wants real hardware. NVIDIA's open kernel modules
             * as a ring-3 driver process behind an OS interface layer written
             * for Ferrix, their GSP firmware, and a userspace that is either
             * glibc-built closed libraries or Mesa's NVK over a Rust driver
             * such as Linux's Nova, weighed when the stage opens. The
             * customer chose NVIDIA's own driver and userspace on 2026-10-02
             * (docs/NVIDIA.md): N0, the kernel prerequisites, and N1, nvrm
             * booting the RTX 3060's GSP with nvidia-smi, are on main
             * (2026-10-03), and the chardev core's queue bound (F-63) with
             * stage 10's chardev self-check since 2026-10-05. N2 to N4 and
             * the screen on the customer's TV are built and not landed.
             * Exit: the stage 19 exit on real hardware, drawn by the card. */
    }

    #inProgress requirement <'S22'> stage22Steam : Stage {
        attribute :>> number = 22;
        attribute :>> status = StageStatus::InProgress;
        attribute :>> size = "unsized, over 300 points";
        doc /* Steam on Ferrix, put on the roadmap by the customer on
             * 2026-09-18 as the step after the GPU decision; a guest's
             * stage first, on Path A's GPU, that does not wait for bare
             * metal. What it stands on that nothing else staged: the 32-bit
             * x86 ABI for the i386 client and 32-bit Wine, glibc's place
             * taken by ferrousli under the Steam runtime, bubblewrap's needs
             * over stage 13, a root on btrfs, XWayland, sound (virtio-snd,
             * an audio core, a PulseAudio or PipeWire server), and Vulkan
             * through Venus on a KVM host. Exit in three boots: the client
             * logs in with its browser helper drawing; a native game
             * installs, plays and sounds; a Windows game runs through
             * Proton.
             *
             * Under way since 2026-09-26. Sound's playback is done
             * (docs/AUDIO.md, 24 points): a ring-3 virtio-snd driver, the
             * kernel's audio core and /dev/snd, verified by
             * cargo xtask test-audio on x86-64 and AArch64; alsa-lib (U1,
             * 3) and a sound server (U2, unsized) are left. The 32-bit x86
             * ABI is designed in docs/I386.md, I1 to I4 at 42 points, and
             * I1 is met: a 32-bit program entered in compatibility mode,
             * making its calls through int $0x80, on a GDT in Linux's
             * order. I2 to I4 followed on 2026-09-27 (threads, signals,
             * fork, and Alpine's and Debian's i386 busybox), with I5a:
             * Valve's steamcmd logs in to Steam. Sound was finished the
             * same day with alsa-lib and pulsed, a PulseAudio-protocol
             * server. On 2026-09-29 the Steam client, unchanged, drew its
             * sign-in window on hyprix through yserver (docs/STEAM.md,
             * cargo xtask test-steam-window), with launch-side
             * workarounds each owned by a fix: namespaces for
             * pressure-vessel, the /proc gaps, and a SIGBUS that needs a
             * 16 GiB guest. On 2026-09-30 the exit's first step was met:
             * on the --everything desktop, its 64-bit side on ferrousli,
             * the client signs in and shows its store, gated by cargo
             * xtask test-steam-store with a test account. Its sandbox
             * waits on stage 13, and its GPU process on user copies
             * through a device window and a render group
             * (docs/STEAM.md section 6). On 2026-10-01 the second step
             * got its gate, cargo xtask test-steam-game, not passing
             * yet: Teeworlds installs on request and downloads, and
             * Steam then stalls staging it, for a reason not yet
             * known (docs/STEAM.md section 7). */
    }

    #inProgress requirement <'S20'> stage20SelfHosting : Stage {
        attribute :>> number = 20;
        attribute :>> status = StageStatus::InProgress;
        attribute :>> size = "longer";
        doc /* Build Ferrix on Ferrix; the image the hosted compiler produces
             * boots and passes every test above. First step met 2026-09-23:
             * `cargo xtask test-selfhost` runs `cargo xtask build --arch
             * x86_64` on Ferrix, from the toolchain, the tree and its
             * vendored crates on a btrfs volume, and the image it made
             * passes the boot test on the host. It fixed the writable
             * btrfs's write offset and dirty-inode lifetime, made MAP_FIXED
             * one step under a per-space layout lock, and added
             * /proc/sys/vm/overcommit_memory. On Arm hardware since
             * 2026-10-03: on a Pixel 7, the phone's own desktop image in
             * crosvm runs `cargo xtask build --arch aarch64` from an AArch64
             * toolchain volume, and `test-selfhost --arch aarch64 --volume`
             * boots the image it made. The matrix's plan mode is complete since 2026-10-05
             * (S-2): every test builds its variants before its first boot,
             * 162 distinct builds against 139. Owed: ARMv7-A, the programs
             * the other tests boot (musl std, a C compiler for ferrousli,
             * the compositor's crates), and the whole matrix on the guest's
             * images. */
    }

    // -----------------------------------------------------------------------
    // Ordering and the debts the roadmap states
    // -----------------------------------------------------------------------

    dependency from stage1Boot to stage0Foundation;
    dependency from stage2Memory to stage1Boot;
    dependency from stage3TrapsInterruptsTime to stage2Memory;
    dependency from stage4Smp to stage3TrapsInterruptsTime;
    dependency from stage5Scheduler to stage4Smp;
    dependency from stage6UserMode to stage5Scheduler, stage3TrapsInterruptsTime;
    dependency from stage7LinuxAbi to stage6UserMode;
    dependency from stage8Vfs to stage7LinuxAbi;
    dependency from stage9NativeAbi to stage8Vfs;
    dependency from stage10UserspaceDrivers to stage9NativeAbi;
    dependency from stage11BtrfsRead to stage10UserspaceDrivers;
    dependency from stageNetworking to stage11BtrfsRead, stage10UserspaceDrivers;
    dependency from stageDynamicLinking to stage8Vfs, stage7LinuxAbi;
    dependency from stageSysfs to stage8Vfs, stage10UserspaceDrivers;
    dependency from stageChrome to stageDynamicLinking, stage12BtrfsWrite, stage18Compositor;
    dependency from stage12BtrfsWrite to stage11BtrfsRead;
    dependency from stage13Isolation to stage12BtrfsWrite;
    dependency from stage14RealTime to stage13Isolation, stage4Smp;
    dependency from stage15Userland to stage14RealTime;
    dependency from stage16Rustc to stage15Userland;
    dependency from stage17DisplayAndInput to stage10UserspaceDrivers, stage7LinuxAbi;
    dependency from stage18Compositor to stage17DisplayAndInput;
    dependency from stage19HyprlandFidelity to stage18Compositor;
    dependency from stage20SelfHosting to stage16Rustc;
    dependency from stage21BareMetalGpu to stage19HyprlandFidelity, stageDynamicLinking;
    dependency from stage22Steam to stage19HyprlandFidelity, stageDynamicLinking,
        stage12BtrfsWrite, stage13Isolation, stageNetworking;

    #planned requirement <'D.fuzz'> fuzzTargetsOwed : Principles::DesignRule {
        doc /* src/tests/fuzz/ has two targets, elf_parse and frame_alloc. Every
             * written-ahead crate parses bytes from outside the system and is
             * owed a target before its consuming stage starts. */
    }
    #planned requirement <'D.miri'> miriOwed : Principles::DesignRule {
        doc /* CI runs Miri over src/lib/platform/elf and src/lib/proto/bootinfo only; frame, heap
             * and paging run in the kernel today and are owed a step ahead of
             * every crate in the written-ahead table. */
    }

    // -----------------------------------------------------------------------
    // What satisfies what
    // -----------------------------------------------------------------------

    satisfy stage1Boot by ferrix.loader;
    satisfy stage2Memory by ferrix.kernel.mm;
    satisfy stage2Memory by ferrix.kernel.vmap;
    satisfy stage3TrapsInterruptsTime by ferrix.kernel.trap;
    satisfy stage3TrapsInterruptsTime by ferrix.kernel.irq;
    satisfy stage3TrapsInterruptsTime by ferrix.kernel.timer;
    satisfy stage4Smp by ferrix.kernel.smp;
    satisfy armv7aPort by ferrixArmv7a;
    satisfy stage5Scheduler by ferrix.kernel.sched;
    satisfy stage5Scheduler by ferrix.kernel.tasks;

    allocate stage6UserMode to ferrix.kernel.vm;
    allocate stage7LinuxAbi to ferrix.kernel.syscalls;
    allocate stage7LinuxAbi to ferrix.kernel.signals;
    allocate stage7LinuxAbi to ferrix.kernel.futex;
    allocate stage8Vfs to ferrix.kernel.vfs;
    allocate stage8Vfs to ferrix.kernel.filesystems;
    allocate stageSysfs to ferrix.kernel.filesystems;
    allocate stageChrome to ferrix.userland;
    allocate stage9NativeAbi to ferrix.kernel.native;
    allocate stage10UserspaceDrivers to ferrix.kernel.devices;
    allocate stage10UserspaceDrivers to ferrix.kernel.iommu;
    allocate stage11BtrfsRead to ferrix.kernel.blockCore;
    allocate stageNetworking to ferrix.kernel.netCore;
    allocate stageDynamicLinking to ferrix.kernel.syscalls;
    allocate stageDynamicLinking to ferrix.userland;
    allocate stage13Isolation to ferrix.kernel.namespaces;
    allocate stage13Isolation to ferrix.kernel.cgroups;
    allocate stage13Isolation to ferrix.kernel.seccomp;
    allocate stage14RealTime to ferrix.kernel.sched;
    allocate stage15Userland to ferrix.userland;
    allocate stage16Rustc to ferrix.userland.rustc;

    // Which goal requirements each stage discharges.
    dependency from stage5Scheduler to hostsRustc::kernelThreads;
    dependency from stage6UserMode to hostsRustc::addressSpaceScale;
    dependency from stage7LinuxAbi to hostsRustc::signalDelivery, hostsRustc::processSpawn;
    dependency from stage8Vfs to hostsRustc::syscallSurface, hostsRustc::procfs;
    dependency from stage12BtrfsWrite to hostsRustc::durableFilesystem;
    dependency from stage13Isolation to hostsRustc::memoryPressure;

    // -----------------------------------------------------------------------
    // The boot tests that verify the done stages, one per architecture
    // -----------------------------------------------------------------------

    verification bootX86 : BootTest {
        attribute :>> arch = IsaFamily::X86_64;
        objective {
            verify stage1Boot;
            verify stage2Memory;
            verify stage3TrapsInterruptsTime;
            verify stage4Smp;
            verify stage5Scheduler;
        }
    }
    verification bootAArch64 : BootTest {
        attribute :>> arch = IsaFamily::AArch64;
        objective {
            verify stage1Boot;
            verify stage2Memory;
            verify stage3TrapsInterruptsTime;
            verify stage4Smp;
            verify stage5Scheduler;
        }
    }
    verification bootArmv7a : BootTest {
        attribute :>> arch = IsaFamily::Armv7a;
        objective {
            verify stage1Boot;
            verify stage2Memory;
            verify stage3TrapsInterruptsTime;
            verify stage4Smp;
            verify stage5Scheduler;
            verify armv7aPort;
        }
    }

    verification def BoardBoot {
        doc /* cargo xtask deploy --arch armv7a: flash two files onto the
             * card's bootfs, watch the ST-LINK serial port for the marker.
             * Manual today; the automated form is deferred above. */
        subject os : Ferrix;
        objective { verify armv7aPort; }
    }

    verification def JobsSession {
        doc /* cargo xtask test-jobs: boot /sbin/init on x86-64 and type a
             * session at the zinc its getty gives on the serial port,
             * through QEMU's stdin -- sleep 30 &, jobs, kill %1, a
             * pipeline, Ctrl-Z, bg, fg, Ctrl-C, exit -- requiring the
             * answer to each keystroke, and after exit a new session from
             * the getty init respawned. The only gate here that types
             * rather than reading what the guest said. x86-64 only: sleep is
             * uutils', which is built for x86-64 alone. */
        subject os : Ferrix;
        objective { verify stage15Userland; }
    }

    verification jobsSession : JobsSession;

    verification def InitSession {
        doc /* cargo xtask test-init: boot /sbin/init as pid 1 from
             * ferrix.init= on all three architectures and type at the shell
             * its getty gives. Its own session and the console as its
             * controlling terminal, from /proc/self/stat; a failing
             * service's restart budget; a daemon's grandchild in its
             * service's cgroup.procs and gone when the service stops; the
             * shutdown order after kill -TERM 1; and btrfs check of the
             * volume afterwards. docs/INIT.md section 15, stages one and
             * two. */
        subject os : Ferrix;
        objective { verify stage15Userland; }
    }

    verification initSession : InitSession;

    #implemented verification def RustcTest {
        doc /* The acceptance test itself: cargo xtask test-rustc, and the CI
             * job rustc. It attaches the sysroot tools/common/fetch/fetch-rustc-sysroot.sh
             * makes -- a btrfs volume the kernel mounts at /data -- under
             * snapshot=on, and zinc runs rustc -vV, rustc hello.rs and
             * ./hello, each with an exit status of its own. x86-64 only. */
        subject os : Ferrix;
        objective {
            verify hostsRustc;
            verify stage16Rustc;
        }
    }

    verification rustcTest : RustcTest;
}
