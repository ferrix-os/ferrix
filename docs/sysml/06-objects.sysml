/*
 * Ferrix — SysML v2 model, part 6: kernel objects and the two ABIs.
 *
 * docs/ARCHITECTURE.md §2 and §3. The constants for the Linux half are in
 * src/lib/proto/linux-abi and for the native half in src/lib/proto/native-abi. The objects a
 * handle can name exist in src/kernel/src/object, the core half of a process
 * among them; the POSIX half and threads in src/kernel/src/syscall. No handle
 * names an address space or a thread yet.
 */
package FerrixObjects {
    private import ScalarValues::*;
    private import FerrixLifecycle::*;
    private import FerrixMemory::*;
    private import FerrixScheduling::Task;

    // -----------------------------------------------------------------------
    // The nine kernel objects, stage 9 (Task and Process earlier)
    // -----------------------------------------------------------------------

    abstract #implemented part def KernelObject {
        @stage { number = 9; }
        doc /* Typed, reference-counted, reached through per-process handle
             * tables. The native ABI is built on these. src/kernel/src/object's
             * Object is a channel end, a VMO, a job, a device node, an
             * interrupt, an I/O mapping, a pin, a process's ending, or a
             * port; each reports its signals as a level and names the queue
             * woken when they may have changed. */
        attribute refcount : Natural;
    }

    // Vmo and ProcessAddressSpace are defined in FerrixMemory; they are two of
    // the nine.
    #implemented part def VmoObject :> KernelObject, Vmo;
    #planned part def AddressSpaceObject :> KernelObject, ProcessAddressSpace;

    #implemented part def Channel :> KernelObject {
        doc /* Bidirectional datagram pipe carrying bytes and handles. The
             * basis of driver IPC. */
        action write;
        action read;
    }

    #implemented part def Port :> KernelObject {
        doc /* An event queue a thread waits on; how one driver thread
             * services many sources. */
        action wait;
        action queue;
    }

    #implemented part def Interrupt :> KernelObject {
        doc /* A bindable hardware interrupt. A userspace driver waits on it
             * through a Port. */
        attribute irq : Natural;
        ref part boundPort : Port[0..1];
        action bindToPort;
        action ack;
    }

    #implemented part def IoMapping :> KernelObject {
        doc /* An MMIO aperture, mappable into a driver's address space, with
             * its IOMMU domain. Nothing outside the aperture:
             * src/kernel/src/object/io_mapping.rs makes one only from an
             * Aperture a device node minted, and whole pages or none. */
        attribute phys : Natural;
        attribute len : Natural;
        ref part domain : FerrixDrivers::IommuDomain[0..1];
    }

    #planned part def TaskObject :> KernelObject, Task {
        @stage { number = 5; }
        doc /* Threads exist (Thread below), each running on a task of its
             * own, but no native handle names one. */
    }

    #implemented part def Process :> KernelObject {
        @stage { number = 6; }
        doc /* The process as the core enforces and reports it, and nothing a
             * personality adds: src/kernel/src/object/process.rs. Its address
             * space, its pid, when it was made, its handle table, its job,
             * and how it ended -- which is all a native handle to one names,
             * so a handle kept past its end keeps nothing else it owned
             * alive. The pid table is here too, weak, so a job kill finds
             * its members without naming the personality. Split from the
             * POSIX process by certification work order W-1: the core has no
             * field leading back to PosixProcess, and where it must hold a
             * process whole it holds a Host, the personality's object seen
             * through the few questions the core asks of it -- kill, a
             * thread starting or gone, whether a wait must end. */
        ref part space : AddressSpaceObject;
        part handles : HandleTable;
        ref part job : Job;
        attribute pid : Natural {
            doc /* Meaningless without saying in which pid namespace. */
        }
        attribute exitStatus : Integer[0..1];
        action kill;
    }

    #implemented part def PosixProcess :> Process {
        @stage { number = 6; }
        doc /* A group of threads sharing an address space, fd table, fs
             * context and signal dispositions. Precisely a particular sharing
             * arrangement of independently shareable objects, composed by
             * clone flags as Linux composes them, within what Clone below
             * refuses. src/kernel/src/syscall/process.rs, in the uncertified
             * load ring: it contains the core Process and derefs to it, and
             * decides how a process ends -- descriptors closed, orphans
             * handed on, the parent told. No namespace set until stage 13:
             * every process shares the one of everything. */
        part threads : Thread[1..*];
        ref part fdTable : FdTable;
        ref part fsContext : FsContext;
        ref part sigHandlers : SignalDispositions;
        ref part namespaces : FerrixIsolation::NsSet;
        attribute credentials : FerrixIsolation::Credentials;
    }

    #implemented part def Thread :> FerrixScheduling::UserThread {
        @stage { number = 7; }
        doc /* One line of execution through a PosixProcess:
             * src/kernel/src/syscall/thread.rs. Its thread id from the pid space,
             * its own signal mask and queue, the address to clear when it
             * ends. The task that runs it holds it only as a UserThread, and
             * the personality has it back by downcast. */
        ref part process : PosixProcess;
    }

    #implemented part def Job :> KernelObject {
        doc /* A container of processes, where resource limits and kill
             * authority live. A wedged userspace driver has to be killable as
             * a unit together with anything it spawned. */
        part processes : Process[0..*];
        part children : Job[0..*];
        part quota : JobQuota[0..1] {
            doc /* None for the tree's root, which nothing is charged to. */
        }
        action kill;
        action setLimit;
        action getQuota;
    }

    #implemented part def JobQuota {
        @stage { number = 13; }
        doc /* What a job and every job beneath it may hold at once, and hold
             * now: the Security Target's FRU_RSA.1 (certification finding
             * F-35, work order W-13). src/kernel/src/object/quota.rs: a slot in a
             * table of atomics, named by a u32 so a frame freed under any
             * lock finds its charge from the frame record alone. A charge
             * walks the slots from the job up with a compare-and-swap at
             * each, so a limit anywhere above refuses and no use passes a
             * limit even briefly. Charged: the job's tasks (fork, clone,
             * process_create; uncharged at reap), its programs' frames and
             * page tables (at allocation, to the running task's job; at every
             * free), the native objects they make (a token in each), and the
             * kernel heap the Linux personality and its libraries hold for
             * them (a src/lib/kernel/kmem token in each object, charged as memory in
             * bytes against the same limit, as cgroup v2 folds kmem into
             * memory.max; certification finding F-37, work order W-15). The
             * processor is a weight, not a limit: each task runs at its own
             * weight times its job's weight over its job's load. cgroupfs's
             * pids, memory and cpu files are a view of it, memory.stat's
             * kernel line the heap alone. */
        attribute tasksUsed : Natural;
        attribute tasksLimit : Natural[0..1];
        attribute memoryBytesUsed : Natural;
        attribute memoryBytesLimit : Natural[0..1];
        attribute kernelBytesUsed : Natural;
        attribute objectsUsed : Natural;
        attribute objectsLimit : Natural[0..1];
        attribute cpuWeight : Natural;
    }

    #implemented part def HandleTable {
        @stage { number = 9; }
        attribute handles : Handle[0..*];
    }

    attribute def Handle {
        attribute index : Natural;
        attribute rights : String;
    }

    part def FdTable {
        doc /* File descriptors and their sharing rules, stage 8: src/lib/fs/vfs's
             * descriptor table behind an Arc, shared where CLONE_FILES asks,
             * with close-on-exec belonging to the number. */
    }
    part def FsContext { doc /* cwd, root, umask. */ }
    part def SignalDispositions;

    // -----------------------------------------------------------------------
    // Clone flags: what a process shares
    // -----------------------------------------------------------------------

    enum def Shareable {
        AddressSpace;
        FdTable;
        FsContext;
        SignalHandlers;
        NamespaceSet;
        ThreadGroup;
    }

    #implemented action def Clone {
        @stage { number = 7; }
        doc /* clone: each Shareable is shared or copied independently.
             * CLONE_THREAD|CLONE_VM|CLONE_SETTLS is a thread; none of them is
             * fork, which marks both address spaces copy-on-write.
             * src/kernel/src/syscall/family.rs, with clone3 through the same
             * path. What Linux allows between a thread and a process is
             * refused with ENOSYS: memory shared between processes without
             * CLONE_VFORK, handlers shared between processes, and a thread
             * with a descriptor table or directories of its own. vfork
             * copies rather than lends, and the parent still waits. */
        in share : Shareable[0..*];
        out child : PosixProcess;
    }

    // -----------------------------------------------------------------------
    // The Linux ABI, stage 7
    // -----------------------------------------------------------------------

    enum def SyscallGroup {
        Memory;      // mmap, mprotect, munmap, brk, madvise
        Files;       // openat, read, write, pread64, getdents64, statx, renameat2 ...
        Process;     // clone, execve, wait4, exit_group, getpid ...
        Threads;     // set_tid_address, futex, robust lists, sched_*
        Signals;     // rt_sigaction, rt_sigprocmask, sigaltstack, rt_sigreturn
        Time;        // clock_gettime, nanosleep
        Identity;    // getuid, uname ...
        Native;      // 0x1000 and up
    }

    #implemented part def LinuxSyscallLayer {
        @stage { number = 7; }
        doc /* The entry path on every architecture, the dispatch table, and
             * the ~150-call surface rustc needs. src/lib/proto/linux-abi holds the
             * numbers for x86-64, AArch64 and the ARM EABI table, the errnos,
             * and the repr(C) layouts (statx, dirent64, sigaction, ...).
             *
             * src/kernel/src/syscall, reached through arch::decode_syscall, which
             * is the only place in the kernel that knows which of the three
             * number tables this build uses. Memory, files and paths,
             * processes and threads, futex, signals, time, identity and
             * credentials, terminals, sockets, epoll, eventfd, timerfd and
             * signalfd; enough for somebody else's busybox (cargo xtask
             * test-shell), a Rust program's threads (test-threads) and rustc
             * compiling hello.rs (test-rustc). The boot test puts every number in 0..=600
             * through dispatch. Still ENOSYS, each named at its arm: swap,
             * modules, System V message queues, acct, vhangup and rseq.
             * System V semaphores (syscall/sem.rs) and shared memory
             * (syscall/shm.rs) are answered, per IPC namespace. */
        attribute groups : SyscallGroup[1..*];
        #implemented action syscallEntry {
            doc /* The assembly trampoline; then Rust. Stage 6's, and it fills
                 * a SyscallArgs from the trap frame and applies the Outcome
                 * that comes back, so no register convention reaches the
                 * dispatch layer and no ABI knowledge reaches the vector.
                 * Both types are the core's (src/kernel/src/trap.rs), and the
                 * trap path calls trap::system_call, which answers through
                 * the SyscallEntry main.rs registered: the core names no
                 * dispatcher above it (F-09). */
        }
        #implemented action dispatch {
            doc /* One function, the certified item's (syscall/mod.rs): the
                 * native range to the native ABI, and a Linux number decoded
                 * by arch::decode_syscall, clamp first, then handed to the
                 * Personality the Linux personality implements
                 * (syscall/linux.rs, in the load ring), which routes it
                 * through its modules; main.rs composes the two at compile
                 * time, so a call pays one indirect call, the core's. Returns Outcome::Return for a value,
                 * or Outcome::Enter for the calls that resume on a frame
                 * which was constructed rather than returned into -- execve,
                 * and a clone child. Self-checked in the boot test on all
                 * three architectures. */
        }
        #planned action seccompCheck {
            @stage { number = 13; }
            doc /* The filter runs on entry, before dispatch. */
        }
    }

    #implemented part def Signals {
        @stage { number = 7; }
        doc /* src/kernel/src/syscall/signal.rs keeps each disposition, the
             * blocked mask and the alternate stack, split between thread and
             * process as Linux splits them; deliver.rs delivers on every
             * return to user mode, onto each architecture's own Linux frame,
             * and rt_sigreturn unwinds it. Stop and continue, SIGPIPE,
             * SIGALRM, and a user-mode fault as SIGSEGV, SIGILL, SIGBUS,
             * SIGFPE or SIGTRAP, which is what rustc's stack-overflow guard
             * needs. An interrupted call restarts or is EINTR as Linux's
             * arch_do_signal_or_restart decides. No vDSO, so a handler needs
             * SA_RESTORER, and only ITIMER_REAL arms. */
        action deliver;
        action sigreturn;
    }

    #implemented part def PosixIpc {
        @stage { number = 15; }
        doc /* Pipes, ttys and job control: what an interactive shell needs.
             * Pipes and FIFOs are stage 8's (src/kernel/src/fs/pipe.rs), the
             * console a terminal with a line discipline since stage 7
             * (fs/terminal.rs) and pseudo-terminals stage 18's (fs/pty.rs),
             * and every call job control is made of has been answered since
             * stage 7; zinc uses them. System V semaphores and shared
             * memory are answered (syscall/sem.rs, syscall/shm.rs, for
             * Steam); its message queues are ENOSYS. */
    }

    // -----------------------------------------------------------------------
    // The native ABI, stage 9
    // -----------------------------------------------------------------------

    #implemented part def NativeAbi {
        @stage { number = 9; }
        doc /* Syscall numbers from 0x1000. Handle-table operations, channel
             * send/receive with handle passing, port wait, interrupt bind,
             * VMO create/map, job create/kill. What devmgr and drivers speak;
             * a process may use both ABIs.
             *
             * src/kernel/src/syscall/native.rs, branched to before any Linux
             * table is asked, with the numbers in src/lib/proto/native-abi/src/nr.rs
             * and typed wrappers in src/lib/proto/native. Beyond that first list:
             * waits on one object or asynchronously through a port, VMO
             * pins for a device's DMA, a job's quota set and read, process
             * create and start, device
             * info and quiesce, and the calls that make a block or net ring
             * and a display, input or render control channel. Left: an EXECUTE
             * right on a VMO handle, sub-page apertures, and the process
             * calls beyond create and start, 0x1032..=0x1037 held for them.
             *
             * The item answers every call on the core's objects in a match
             * the compiler holds exhaustive. The six about a subsystem above
             * it -- the five control-channel creates and job_for_cgroup --
             * are a table the subsystems register handlers into from
             * main.rs's register_load, and the boot stops (FX-0006) if one
             * has none. A native process is made and started through the
             * Processes the Linux personality lends (syscall/launch.rs),
             * which devmgr starts through too, and a quiesce waits out the
             * Servers the rings and cards register. native.rs names nothing
             * above the item (F-07). */
        attribute firstNumber : String = "0x1000";
        action handleClose;
        action handleDuplicate;
        action handleReplace;
        action objectWaitOne;
        action objectWaitAsync;
        action channelCreate;
        action channelWrite;
        action channelRead;
        action portCreate;
        action portQueue;
        action portWait;
        action vmoCreate;
        action vmoRead;
        action vmoWrite;
        action vmoGetSize;
        action vmoMap;
        action vmoPin;
        action vmoPinAddresses;
        action jobCreate;
        action jobKill;
        action jobSetLimit;
        action jobGetQuota;
        action processCreate;
        action processStart;
        action interruptCreate;
        action interruptBind;
        action interruptAck;
        action ioMappingCreate;
        action ioMappingMap;
        action blockRingCreate;
        action netRingCreate;
        action displayControlCreate;
        action inputControlCreate;
        action renderControlCreate;
        action deviceInfo;
        action deviceQuiesce;
    }
}
