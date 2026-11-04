/*
 * Ferrix — SysML v2 model, part 1: requirements.
 *
 * docs/ARCHITECTURE.md §0 read as a specification, plus the design rules the
 * rest of the model has to satisfy and the promises it deliberately does not
 * make. Ids in angle brackets are stable; the roadmap and assurance packages
 * cite them.
 */
package FerrixRequirements {
    private import ScalarValues::*;
    private import FerrixLifecycle::*;
    private import FerrixStructure::Ferrix;

    // -----------------------------------------------------------------------
    // The goal
    // -----------------------------------------------------------------------

    requirement def GoalRequirement {
        doc /* A requirement the acceptance test forces. Every one of these is
             * load-bearing: it is why the system is shaped the way it is, and
             * why toy-OS shortcuts are ruled out at the start rather than
             * discovered to be dead ends at the end. */
        subject os : Ferrix;
    }

    requirement <'G'> hostsRustc : GoalRequirement {
        doc /* The acceptance test: a statically linked musl `rustc` compiles
             * `hello.rs` on Ferrix, the binary it produced runs, and CI proves
             * both. Not "has a shell", not "draws a window". Hosting a compiler
             * is the hardest thing a general-purpose OS is routinely asked to
             * do and the only goal that forces every subsystem to be real. */

        requirement <'G.1'> kernelThreads : GoalRequirement {
            doc /* rustc needs clone(CLONE_THREAD|CLONE_VM|CLONE_SETTLS), the
                 * futex family, set_tid_address and robust lists.
                 * Forces: 1:1 kernel threads, a real futex, per-thread TLS
                 * registers. */
        }
        requirement <'G.2'> addressSpaceScale : GoalRequirement {
            doc /* mmap/mprotect/munmap with MAP_FIXED and MAP_NORESERVE over
                 * 2 to 8 GiB of address space.
                 * Forces: demand paging, a VMA tree, lazy anonymous memory,
                 * overcommit. */
        }
        requirement <'G.3'> signalDelivery : GoalRequirement {
            doc /* A SIGSEGV handler on an alternate stack.
                 * Forces: real signal delivery, sigaltstack, rt_sigreturn. */
        }
        requirement <'G.4'> processSpawn : GoalRequirement {
            doc /* fork/execve/wait4 to run the linker.
                 * Forces: copy-on-write fork, an ELF loader, process groups,
                 * exit status plumbing. */
        }
        requirement <'G.5'> syscallSurface : GoalRequirement {
            doc /* openat, getdents64, statx, renameat2, pread64, about 150
                 * syscalls in total.
                 * Forces: a VFS with inode and dentry caches. */
        }
        requirement <'G.6'> procfs : GoalRequirement {
            doc /* /proc/self/maps, /proc/self/exe, /proc/self/fd.
                 * Forces: a procfs backed by the real VM and fd table. */
        }
        requirement <'G.7'> durableFilesystem : GoalRequirement {
            doc /* A writable filesystem with room for a ~2 GiB sysroot.
                 * Forces: a block stack and an on-disk filesystem that
                 * survives a crash. */
        }
        requirement <'G.8'> memoryPressure : GoalRequirement {
            doc /* rustc will exhaust memory on a small machine, so reclaim is
                 * a correctness requirement: an OOM kill scoped by Job and
                 * cgroup, never a livelock. */
        }
    }

    requirement <'G+'> selfHosting : GoalRequirement {
        doc /* Stage 20: build Ferrix on Ferrix. The image the Ferrix-hosted
             * compiler produces boots and passes every boot test. */
    }
    dependency from selfHosting to hostsRustc;

    // -----------------------------------------------------------------------
    // Design rules
    // -----------------------------------------------------------------------

    package Principles {
        doc /* Decisions that shape every subsystem. They are requirements
             * because the assurance package verifies most of them with a
             * script or a build failure rather than a review. */

        requirement def DesignRule {
            subject os : Ferrix;
        }

        requirement <'P.1'> linuxIsTheNativeAbi : DesignRule {
            doc /* Syscall 0 is `read`. The Linux syscall ABI is the native
                 * interface, not a compatibility layer, so the static-musl
                 * world is the userland from the first day there is one, and
                 * POSIX conformance is inherited rather than reimplemented.
                 * The alternative — a native target with its own std — needs
                 * LLVM built for it, which drags a C++ runtime into the tree. */
        }
        requirement <'P.2'> monolithicCoreCapabilitySeams : DesignRule {
            doc /* A syscall is a function call, not four IPC hops. Device
                 * drivers run as user processes holding capabilities, so a
                 * driver fault is a process fault. The line is drawn at
                 * devices: filesystems and the page cache stay in the kernel
                 * because rustc touches them on every path. */
        }
        requirement <'P.3'> oneInKernelDevice : DesignRule {
            doc /* The one in-kernel device is a serial port writer, for early
                 * boot and panic output when no userspace exists. Named as an
                 * exception so that it stays one. */
        }
        requirement <'P.4'> nothingStubbed : DesignRule {
            doc /* Nothing is stubbed that a later stage has to unpick: no
                 * fixed-size process table, no in-memory-only filesystem, no
                 * cooperative scheduler. */
        }
        requirement <'P.5'> pureFunctionsInLibs : DesignRule {
            doc /* Anything expressible as a pure function of bytes goes to
                 * `src/lib/`, and gets a fuzz target and a Miri run, before the
                 * kernel calls it. That is the only code cargo test, Miri and
                 * the fuzzers can reach. */
        }
        requirement <'P.6'> oneArchFacade : DesignRule {
            doc /* Generic code never names an architecture and
                 * #[cfg(target_arch)] appears nowhere outside arch/.
                 * Enforced by tools/common/check/check-crate-layering.sh, because a
                 * facade maintained by convention is a facade for about six
                 * weeks. */
        }
        requirement <'P.7'> assemblyOnlyWhereTheMachineDefinesIt : DesignRule {
            doc /* No assembly at boot on any architecture. What exists is
                 * confined to constructs the machine defines before a Rust
                 * function could run, listed in tools/common/data/asm-allowlist.json
                 * with an argument each, under an absolute line cap. */
        }
        requirement <'P.8'> oneLayoutPerAddressWidth : DesignRule {
            doc /* The 64-bit pair share identical layout constants; ARMv7-A
                 * has a 32-bit layout that is argued rather than merely
                 * different. Both are checked at compile time on every build. */
        }
        requirement <'P.9'> namespacesDesignedIn : DesignRule {
            doc /* All eight namespaces from the start: every global table is
                 * reached through the task's NsSet from the first line, so
                 * nobody has to find those tables years later. */
        }
        requirement <'P.10'> iommuIsNotOptional : DesignRule {
            doc /* A userspace driver without an IOMMU can write any physical
                 * address, which is worse than an in-kernel driver. Where no
                 * IOMMU exists, drivers run in a degraded trusted mode and the
                 * kernel says so loudly at boot. */
        }
        requirement <'P.11'> everyStageEndsInSomethingThatRuns : DesignRule {
            doc /* A stage's exit criterion is a QEMU boot that demonstrates
                 * the new capability and stays in CI forever after. */
        }
        requirement <'P.12'> unsafeIsExpensive : DesignRule {
            doc /* unsafe is not forbidden, it is made expensive: a SAFETY
                 * comment on every block, one unsafe operation per block, a
                 * Safety section on every unsafe fn, and a script in CI so a
                 * softened clippy lint cannot retire the rule. */
        }
        requirement <'P.13'> noReachablePanic : DesignRule {
            doc /* panic!, unreachable!, unwrap, expect and unchecked indexing
                 * are denied in production code; every exemption is an
                 * #[expect] whose reason begins AUDIT:. A reachable panic is
                 * an unrecoverable machine. A fatal condition in the kernel
                 * is fatal!, which names the catalog entry that explains it
                 * and panics from inside the macro; a bare panic! is denied
                 * there as everywhere. */
        }
        requirement <'P.14'> overflowChecksInRelease : DesignRule {
            doc /* Overflow checks stay on in release. A wrapped frame number
                 * is a write to the wrong physical page whose symptom appears
                 * elsewhere; a panic that names the line is strictly better. */
        }
        requirement <'P.15'> provedOnEveryBoot : DesignRule {
            doc /* The kernel proves its invariants on every boot rather than
                 * asserting them: the memory map is checked, the direct map is
                 * checked to alias physical memory, the allocators are
                 * required to give every frame back. */
        }
        requirement <'P.16'> oneAuthorPerCommit : DesignRule {
            doc /* docs/CONVENTIONS.md: a commit names one author. No
                 * Co-authored-by trailer, no Generated-with line, no tool
                 * signature, and no tool (a name or address matching claude
                 * or anthropic) as author or committer, whoever or whatever
                 * made the change; the author is the accountable owner, not a
                 * record of what made the change. A message is
                 * a subject, a blank line, and a body that argues the why.
                 * This overrides any agent's default attribution instruction. */
        }
        requirement <'P.17'> aGateNeedsNoArming : DesignRule {
            doc /* A control that only runs once a clone has been configured is
                 * not a control. Two hooks behind one un-committable
                 * core.hooksPath switch were one control with two names, and
                 * neither fired while eight trailer-carrying commits were
                 * written. Every rule has a check in CI that needs no local
                 * setup and cannot be skipped with --no-verify; hooks are the
                 * convenience, CI is the guarantee. */
        }
    }

    // -----------------------------------------------------------------------
    // Promises deliberately not made
    // -----------------------------------------------------------------------

    package NonPromises {
        requirement <'N.1'> noCertifiedWcet : Principles::DesignRule {
            doc /* HardRt promises EDF with admission control, bounded kernel
                 * critical sections on the RT path, preallocated pools there,
                 * and interrupts that cannot steal unaccounted time. It does
                 * not promise a certified worst-case execution time for the
                 * whole kernel; no OS that also hosts LLVM can. */
        }
        requirement <'N.2'> noRaid56 : Principles::DesignRule {
            doc /* btrfs is single-device to begin with and RAID 5/6 is out of
                 * scope; it is where btrfs itself is weakest. */
        }
        requirement <'N.3'> noAml : Principles::DesignRule {
            doc /* src/lib/platform/acpi reads the fixed tables only. There is no AML
                 * interpreter and there will be none. */
        }
    }
}
