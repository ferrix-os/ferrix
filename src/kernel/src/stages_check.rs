//! The boot's stage checks: the functions `main.rs` calls to prove each
//! stage as it comes up, moved here from the crate root so that the item
//! counts them as verification and a `Verifies:` tag may go on them
//! (docs/certification/IMPLEMENTATION.md W-8). A child of the crate root,
//! so it reads `main.rs`'s items as `object/check.rs` reads `object`'s. What
//! brings something up as well as checking it -- the filesystems, the PCI
//! scan, the devices, `devmgr`, the block ring's switch of `/` -- stays in
//! `main.rs`.

#![allow(
    clippy::wildcard_imports,
    reason = "the crate root's items, as the functions had them there"
)]

use super::*;

/// Stage 2's allocators and stage 3's synchronous traps: all check.
pub(super) fn check_allocators_and_traps(stats: &mm::Stats) {
    if let Err(problem) = mm::check::memory_check(stats) {
        fatal!(
            catalog::STAGE2_ALLOCATORS,
            "stage 2 self-check failed: {problem}"
        );
    }
    println!("  stage 2  frame allocator, heap and vmap arena verified");

    if let Err(problem) = trap_check() {
        fatal!(
            catalog::STAGE3_TRAPS,
            "stage 3 self-check failed: {problem}"
        );
    }
}

/// What a wait and a sleep do while there is no task to switch away from:
/// stage 5's, checked here because this is the one stretch of boot where they
/// can be seen doing it, with the clock stage 3 just proved.
pub(super) fn check_waits_before_the_scheduler() {
    if let Err(problem) = sched::check_before_start() {
        fatal!(
            catalog::STAGE5_SCHEDULER,
            "stage 5 self-check failed before the scheduler started: {problem}"
        );
    }
}

/// The checks that drive programs through the dispatch path, from stage 7's
/// table to stage 9's objects: every one of them only checks, each tears down
/// the processes it made and requires their frames back, and none brings up
/// anything a later step uses, so they are skipped together.
pub(super) fn check_programs() {
    // Stage 7's dispatch path. After stage 5 because two of the calls it
    // answers ask the scheduler which task is running, and deliberately here
    // rather than waiting for a user program: the one thing this check
    // establishes -- that the kernel was built against its *own*
    // architecture's system call table -- is a fact about the build, and a
    // build that got it wrong would answer a program's `write` with `unlink`.
    // Finding that under the first user process, in the same commit as the
    // ring-3 transition, is a debugging session nobody wants.
    check_syscalls();

    // A VMO taking a page away from two processes running on two processors,
    // seen from user mode. Stage 6's memory, but it needs programs that run,
    // so after stage 7's check has shown they do.
    check_reverse_map();

    // The ways into the kernel a program can bend from ring 3, and the ones
    // nobody can mask. After stage 7's check, which has shown a program runs.
    check_entry_paths();

    // Stage 8's path calls, through the same dispatch table. After stage 7's
    // check because they share its table and its copy layer, and after the
    // root was built because they work under /tmp.
    check_path_calls();

    // System V semaphores, through the functions their calls reach; after
    // the root, whose context the check's process is made in.
    check_semaphores();

    // System V shared memory, the same way, beside them.
    check_shared_memory();

    // The filter the core's entries ask first about every call, driven through
    // each entry with frames of the check's own.
    check_seccomp();
    check_seccomp_filters();

    // A mount's own flags, enforced and shown; under /tmp, after the root.
    check_mount_flags();

    // Binds, detach and the two remounts, on a tmpfs under /tmp.
    check_binds();

    // Stage 9's objects, driven through the native handlers between two
    // processes this check builds. After stage 7 because it shares the
    // dispatch path and the user copy layer, and here rather than under a
    // program for the reason stage 7's check gives: the rules a capability
    // system rests on are cheaper to find broken at boot than inside a
    // driver.
    check_native_objects();

    // Finding F-23's negative control: allocations made to fail under the
    // native calls stage 9 just proved, and the kernel required to carry on.
    check_allocation_failure();

    // Finding F-35: the job quotas FRU_RSA.1 claims, each driven to its
    // limit through the path a program's use takes.
    check_quotas();
    check_kernel_memory();
}

/// The registration lists, a device's claim and number, the boot-mode word,
/// and the sentences failures are reported with.
pub(super) fn check_services() {
    let report = match service_check::run() {
        Ok(report) => report,
        Err(problem) => fatal!(catalog::SERVICES, "service self-check failed: {problem}"),
    };
    println!(
        "  services {} registrations kept in order, {} refusals as documented, {} failure \
         sentences read back",
        report.registered, report.refusals, report.sentences,
    );
}

/// The native calls' refusals stage 9's check does not make: the wrong kind
/// of handle, a missing right, a handle named twice, a full table, a send too
/// deep to check, a faulting packet buffer, a copy past the caches, a clock.
pub(super) fn check_native_refusals() {
    let report = match syscall::native_check::run() {
        Ok(report) => report,
        Err(problem) => fatal!(
            catalog::STAGE9_REFUSALS,
            "native refusal self-check failed: {problem}"
        ),
    };
    println!(
        "  refusals {} native calls refused as the ABI says, a table refused past {} handles and \
         kept what it could not deliver, a send refused past {} queued endpoints",
        report.refusals, report.filled, report.walked,
    );
}

/// Stage 6: the memory objects, the frames they must give back, and the
/// processor walking an address space it has been given.
///
/// Halts rather than returning, as every other stage's check does: the useful
/// report is which property failed, not that stage 6 did.
pub(super) fn check_user_memory() {
    let report = match user::check::run() {
        Ok(report) => report,
        Err(problem) => fatal!(
            catalog::STAGE6_USER_MEMORY,
            "stage 6 self-check failed: {problem}"
        ),
    };

    println!(
        "  objects  {} pages reserved, {} committed, {} faulted in, {} walked by the MMU, \
         {} copied on write, {} frames leaked",
        report.reserved,
        report.committed,
        report.faulted,
        report.walked,
        report.copied,
        report.leaked,
    );
    println!(
        "  spaces   {} reads of one address in two address spaces, each its own",
        report.swapped,
    );
}

/// Stage 8's memfd check: `memfd_create` and its seals, and the may-write
/// accounting a write seal is refused by.
pub(super) fn check_memfd() {
    let sealed = match fs::memfd_check::run() {
        Ok(sealed) => sealed,
        Err(problem) => fatal!(
            catalog::STAGE8_MEMFD,
            "stage 8 memfd self-check failed: {problem}"
        ),
    };
    println!(
        "  memfd    {} seals added and enforced, {} calls refused as Linux refuses them, a \
         write seal refused while a shared mapping could write, a fork's copy included; {} \
         frames leaked",
        sealed.seals, sealed.refusals, sealed.leaked,
    );
}

/// Stage 8's epoll check: level, edge and one-shot registrations, sets inside
/// sets, and the refusals.
pub(super) fn check_epoll() {
    let checked = match fs::epoll_check::run() {
        Ok(checked) => checked,
        Err(problem) => fatal!(
            catalog::STAGE8_EPOLL,
            "stage 8 epoll self-check failed: {problem}"
        ),
    };
    println!(
        "  epoll    {} events delivered by level, edge and one-shot registrations and a set inside \
         a set, {} calls refused as Linux refuses them; {} frames leaked",
        checked.events, checked.refusals, checked.leaked,
    );
}

/// Stage 8's eventfd check: counting, the ceiling, a woken reader and an edge
/// in an epoll set.
pub(super) fn check_eventfd() {
    let checked = match fs::eventfd_check::run() {
        Ok(checked) => checked,
        Err(problem) => fatal!(
            catalog::STAGE8_EVENTFD,
            "stage 8 eventfd self-check failed: {problem}"
        ),
    };
    println!(
        "  eventfd  {} values read back, a waiting reader, poll and epoll_wait each woken by a \
         write, a quiet poll asleep on its queues, {} calls refused as Linux refuses them; {} \
         frames leaked",
        checked.reads, checked.refusals, checked.leaked,
    );
}

/// System V semaphores: keys, values, operations, the layouts, `SEM_UNDO`
/// at exit, every way a blocked `semop` ends, and the per-job bound.
pub(super) fn check_semaphores() {
    let checked = match syscall::sem_check::run() {
        Ok(checked) => checked,
        Err(problem) => fatal!(
            catalog::STAGE7_SEMAPHORES,
            "System V semaphore self-check failed: {problem}"
        ),
    };
    println!(
        "  sem      {} semaphore calls answered as Linux answers them, {} of them refusals; {} \
         blocked semops ended by an increment, SETVAL, EINTR, their deadline and EIDRM, each \
         job's heap back; SEM_UNDO paid at exit, and the set found by its key and taken \
         from another job once its maker had ended; a job refused ENOSPC at {} sets while a \
         sibling made one",
        checked.calls, checked.refusals, checked.waits, checked.per_job,
    );
}

/// System V shared memory: keys, the layouts, who may attach, `IPC_RMID`
/// deferred to the last detach, and the per-job bound.
pub(super) fn check_shared_memory() {
    let checked = match syscall::shm_check::run() {
        Ok(checked) => checked,
        Err(problem) => fatal!(
            catalog::STAGE7_SHARED_MEMORY,
            "System V shared memory self-check failed: {problem}"
        ),
    };
    println!(
        "  shm      {} shared memory calls answered as Linux answers them, {} of them refusals; \
         a removed segment kept, SHM_DEST, until its last detach and then gone; root attached \
         a stranger's mode-0600 segment; a key and an id kept to their IPC namespace; a job \
         refused ENOSPC at {} segments, and at {} reserved pages, while a sibling made one",
        checked.calls, checked.refusals, checked.per_job, checked.per_job_pages,
    );
}

/// seccomp (`docs/SECCOMP.md`): the registered filter asked first at every
/// entry, under the entry's own architecture token.
pub(super) fn check_seccomp() {
    let checked = match syscall::seccomp_check::run() {
        Ok(checked) => checked,
        Err(problem) => fatal!(
            catalog::STAGE13_SECCOMP,
            "seccomp self-check failed: {problem}"
        ),
    };
    println!(
        "  seccomp  {} calls driven through the core's own entries, the filter asked first \
         each time: {} calls an entry answers itself reached it before that answer, {} entries \
         judged under the architecture token of their own, and {} native-range call judged \
         under a token of its own, refused by a filter that refuses every foreign arch and \
         let by one that allows it by name; and the core cut every errno a filter could \
         answer to 0 to 4095 ({} range)",
        checked.calls, checked.early, checked.tokens, checked.native, checked.clamped,
    );
    println!(
        "  seccomp  a thread with no filter pays {}.{} ns a call for the hook; a call no table \
         has costs {}.{} ns in the dispatcher and {}.{} ns through the whole entry; one \
         interpreted filter instruction costs {}.{} ns, so the longest chain (32,768 steps) \
         costs at most {} us a call",
        checked.hook / 10,
        checked.hook % 10,
        checked.dispatch / 10,
        checked.dispatch % 10,
        checked.entry / 10,
        checked.entry % 10,
        checked.step / 10,
        checked.step % 10,
        checked.step * 32_768 / 10_000,
    );
}

/// seccomp's filters (`docs/SECCOMP.md` §8.1, S3): installed as a program does,
/// judged through the core's own entry, ordered, inherited and released.
pub(super) fn check_seccomp_filters() {
    let checked = match syscall::seccomp_filters_check::run() {
        Ok(checked) => checked,
        Err(problem) => fatal!(
            catalog::STAGE13_SECCOMP_FILTERS,
            "seccomp filters self-check failed: {problem}"
        ),
    };
    println!(
        "  seccomp  {} probes answered as Linux answers them, {} calls a filtered thread made \
         through the entry and judged, {} children and threads that held their creator's chain, \
         {} processes a filter ended, and a chain of {} filters made and released",
        checked.probes, checked.calls, checked.inherited, checked.killed, checked.chain,
    );
    println!(
        "  seccomp  a call costs {} us for the 6,554-filter chain, {} us for seven filters of \
         4,096 instructions, the most steps there can be, and releasing the long chain costs {} us",
        checked.walk_many / 1000,
        checked.walk_long / 1000,
        checked.release / 1000,
    );
}

/// A mount's own flags: `ro`, `nodev`, `noexec` and `nosuid` enforced,
/// `MS_REMOUNT` changing them, and `mountinfo`, `mounts` and `statfs`
/// showing them (`docs/NAMESPACES.md`, N1).
pub(super) fn check_mount_flags() {
    let checked = match fs::mount_check::run() {
        Ok(checked) => checked,
        Err(problem) => fatal!(
            catalog::STAGE8_MOUNT_FLAGS,
            "mount flag self-check failed: {problem}"
        ),
    };
    println!(
        "  mounts   {} calls answered as Linux answers them, {} of them refusals: a read-only \
         mount EROFS for every change, nodev EACCES, noexec EACCES and its mappings EPERM, \
         nosuid ignoring a set-user-id bit; remounts only at a mount's root and only by root; \
         mountinfo naming each as its descriptor link does",
        checked.calls, checked.refusals,
    );
}

/// Binds of a directory, a subtree, a file and a socket, `MS_REC`,
/// `MNT_DETACH` of a subtree, the propagation no-ops, and a plain remount
/// reaching every bind where `MS_REMOUNT | MS_BIND` reaches one
/// (`docs/NAMESPACES.md`, N2).
pub(super) fn check_binds() {
    let checked = match fs::bind_check::run() {
        Ok(checked) => checked,
        Err(problem) => fatal!(catalog::STAGE8_BINDS, "bind self-check failed: {problem}"),
    };
    println!(
        "  binds    {} calls answered as Linux answers them, {} of them refusals: a directory, \
         a subtree, a file and a socket bound, MS_REC copying a submount, MNT_DETACH taking \
         one; a bind remount reaching one mount and a plain remount every bind; mountinfo \
         naming each as its descriptor link does",
        checked.calls, checked.refusals,
    );
}

/// Mount namespaces: a copy private both ways, bubblewrap's sequence as root
/// to its last `pivot_root(".", ".")`, a native child kept in its creator's
/// namespace, and a btrfs write inside a detached subtree on the disk after
/// the detach (`docs/NAMESPACES.md`, N3). After stage 12's check, whose
/// volume at `/mnt-rw` the last is shown on; `disk` says it is there.
pub(super) fn check_namespaces(disk: bool) {
    let checked = match fs::namespace_check::run(disk) {
        Ok(checked) => checked,
        Err(problem) => fatal!(
            catalog::STAGE13_MOUNT_NAMESPACES,
            "mount namespace self-check failed: {problem}"
        ),
    };
    println!(
        "  mntns    {} calls answered as Linux answers them, {} of them refusals: openat2's \
         resolve flags; a copy private \
         both ways and named apart; bubblewrap's calls as root to pivot_root(\".\", \".\"), with \
         nothing of the old tree reachable; a native child in its creator's namespace; {}",
        checked.counts.calls,
        checked.counts.refusals,
        if checked.committed {
            "a btrfs write inside a detached subtree on the disk after the detach"
        } else {
            "no stage 12 disk for the detach's write-out"
        },
    );
    check_user_namespaces();
    check_small_namespaces();
    check_pid_namespaces();
    check_network_namespaces();
    check_proc_access();
    check_mount_permissions();
}

/// Who may change mounts: ownership, `tmpfs` alone from a user namespace,
/// locked copies (`docs/NAMESPACES.md` M1 to M4, N5).
fn check_mount_permissions() {
    let checked = match fs::mountperm_check::run() {
        Ok(checked) => checked,
        Err(problem) => fatal!(
            catalog::STAGE13_MOUNT_PERMISSIONS,
            "mount permission self-check failed: {problem}"
        ),
    };
    println!(
        "  mountperm {} calls answered as Linux answers them, {} of them refusals: no mounting in a mount namespace a user namespace does not own, tmpfs alone and nosuid,nodev from one that does, a copy's flags locked and its mounts locked to their parents, the host's filesystem not its to remount, no directory pinned by a mount of its own, and no more user namespaces than max_user_namespaces",
        checked.calls, checked.refusals,
    );
}

/// User namespaces: the rules of `docs/NAMESPACES.md` §4 attempted and
/// refused (N4).
fn check_user_namespaces() {
    let checked = match fs::userns_check::run() {
        Ok(checked) => checked,
        Err(problem) => fatal!(
            catalog::STAGE13_USER_NAMESPACES,
            "user namespace self-check failed: {problem}"
        ),
    };
    println!(
        "  userns   {} calls answered as Linux answers them, {} of them refusals: a namespace named apart, ids 65534 until mapped, a gid_map refused before setgroups is denied, kernel root and a second id unmappable, a map written once, fake root refused what only root may do, a chrooted process refused, a set-id bit ignored, a read-only /proc/sys refusing a write",
        checked.calls, checked.refusals,
    );
}

/// What a process keeps private in `/proc`, refused to other users
/// (`docs/NAMESPACES.md` M8, landing NP).
fn check_proc_access() {
    let checked = match fs::procaccess_check::run() {
        Ok(checked) => checked,
        Err(problem) => fatal!(
            catalog::STAGE13_PROC_ACCESS,
            "/proc access self-check failed: {problem}"
        ),
    };
    println!(
        "  procacc  {} calls answered as Linux answers them, {} of them refusals: root, cwd, exe, fd, fdinfo, maps and ns/* of another uid's or a non-dumpable process refused EACCES, read by the same user, by root, and by root inside a user namespace as ptrace_may_access allows; get_robust_list of another uid's thread refused",
        checked.calls, checked.refusals,
    );
}

/// The small namespaces -- UTS, IPC and cgroup -- and `setns` (`docs/NAMESPACES.md`
/// §12).
fn check_small_namespaces() {
    let checked = match fs::smallns_check::run() {
        Ok(checked) => checked,
        Err(problem) => fatal!(
            catalog::STAGE13_SMALL_NAMESPACES,
            "small namespace self-check failed: {problem}"
        ),
    };
    println!(
        "  smallns  {} calls answered as Linux answers them, {} of them refusals: UTS names copied and then private, a maker of a user namespace naming its own and never the first's, IPC keys and counts private, a cgroup namespace's paths, mount and move rule, namespace files opened, named and asked, setns refused and joined by kind",
        checked.calls, checked.refusals,
    );
}

/// Pid namespaces: the rules of `docs/PIDNS.md` attempted and refused.
fn check_pid_namespaces() {
    let checked = match fs::pidns_check::run() {
        Ok(checked) => checked,
        Err(problem) => fatal!(
            catalog::STAGE13_PID_NAMESPACES,
            "pid namespace self-check failed: {problem}"
        ),
    };
    println!(
        "  pidns    {} calls answered as Linux answers them, {} of them refusals: pid 1 and 2 in a namespace and other numbers outside, kill, wait4, getppid, groups and sessions in the caller's numbers, an orphan given to its namespace's init, the init's end ending the namespace, an init ignoring what it does not catch from inside, si_pid and SO_PEERCRED told to the reader, a procfs and cgroup.procs of a namespace by its numbers, CLONE_NEWPID's privilege, flags and depth, a native child numbered in its creator's namespace, and a namespace closed by a failed first fork",
        checked.calls, checked.refusals,
    );
}

/// Network namespaces: the rules of `docs/NETNS.md` section 8 attempted and
/// refused or accepted (NN1 to NN17).
fn check_network_namespaces() {
    let checked = match fs::netns_check::run() {
        Ok(checked) => checked,
        Err(problem) => fatal!(
            catalog::STAGE13_NETWORK_NAMESPACES,
            "network namespace self-check failed: {problem}"
        ),
    };
    println!(
        "  netns    {} calls answered as Linux answers them, {} of them refusals: a new namespace with a loopback that is down, brought up by netlink and by ioctl, ports and sockets private to each, a socket staying where it was made, changes and raw sockets judged over the owning user namespace, a veth pair carrying a datagram and a stream between two namespaces and nothing to a third, moved and deleted, a device served where it went and home when the namespace ended, abstract names per namespace, /proc/net the reader's, tables at their ceilings",
        checked.calls, checked.refusals,
    );
}

/// Stage 8's timerfd check: flags and clocks, expirations counted, the
/// settings in both layouts, a clock set, and waiters woken at the deadline.
pub(super) fn check_timerfd() {
    let checked = match fs::timerfd_check::run() {
        Ok(checked) => checked,
        Err(problem) => fatal!(
            catalog::STAGE8_TIMERFD,
            "stage 8 timerfd self-check failed: {problem}"
        ),
    };
    println!(
        "  timerfd  {} expirations read back, a read, poll and epoll_wait each woken at the \
         deadline, the latest {} us after it; {} calls refused as Linux refuses them; {} frames \
         leaked",
        checked.expirations, checked.late_micros, checked.refusals, checked.leaked,
    );
}

/// Stage 8's check of programs mapped from their files: one past 64 MiB
/// loaded at the cost of its headers and paged in on demand, its writes kept
/// from the file, and a fork of it running `/proc/self/exe`.
pub(super) fn check_program_files() {
    let checked = match fs::exec_check::run() {
        Ok(Some(checked)) => checked,
        Ok(None) => {
            println!("  exec     no user-mode program on {} yet", arch::NAME);
            return;
        }
        Err(problem) => fatal!(
            catalog::STAGE8_PROGRAM_FILES,
            "stage 8 program file self-check failed: {problem}"
        ),
    };
    println!(
        "  exec     a {} MiB program was loaded reading {} of its {} pages, {} by the time it had \
         been touched and had run; a write kept from the file; it exited with {}, and a fork of \
         it ran /proc/self/exe with the file's name gone and exited with {}",
        checked.bytes >> 20,
        checked.loaded,
        checked.pages,
        checked.read,
        checked.status,
        checked.self_status,
    );
}

/// `madvise`'s check: frames given back by `MADV_DONTNEED`, `MADV_FREE` and
/// `MADV_REMOVE`, what a dropped page reads, the hints and the refusals.
pub(super) fn check_madvise() {
    let checked = match user::madvise_check::run() {
        Ok(checked) => checked,
        Err(problem) => fatal!(
            catalog::STAGE8_MADVISE,
            "stage 8 madvise self-check failed: {problem}"
        ),
    };
    println!(
        "  madvise  {} frames given back by MADV_DONTNEED, MADV_FREE and MADV_REMOVE; dropped \
         pages read as zeros, a private file mapping as its file, a shared one as it was; {} \
         calls refused as Linux refuses them; {} frames leaked",
        checked.given_back, checked.refusals, checked.leaked,
    );
}

/// Stage 8's signalfd check: flags and masks, signals read and dequeued, and
/// waiters woken by a signal's arrival.
pub(super) fn check_signalfd() {
    let checked = match fs::signalfd_check::run() {
        Ok(checked) => checked,
        Err(problem) => fatal!(
            catalog::STAGE8_SIGNALFD,
            "stage 8 signalfd self-check failed: {problem}"
        ),
    };
    println!(
        "  signalfd {} signals read back, a read, poll and epoll_wait each woken by the \
         signal's arrival, the latest {} us after it; {} calls refused as Linux refuses them; {} \
         frames leaked",
        checked.signals, checked.late_micros, checked.refusals, checked.leaked,
    );
}

/// Stage 13's cgroupfs, landings G2 to G5: the job tree mounted as cgroup2,
/// driven through the VFS as a program would drive it, `cgroup.events`
/// waited on with epoll, a subtree delegated by `chown`, a child started in
/// a cgroup by `clone3`, and a cgroup's job waited on for `EMPTY` through a
/// handle `job_for_cgroup` gave.
/// The kernel calls init needs beyond stage 13 (`docs/INIT.md` §11).
pub(super) fn check_init_calls() {
    let report = match syscall::init_calls_check::run() {
        Ok(report) => report,
        Err(problem) => fatal!(
            catalog::INIT_CALLS,
            "init's kernel calls self-check failed: {problem}"
        ),
    };
    println!(
        "  initcall init's bootstrap channel greeted it with version {}, {} bootstraps given and \
         taken, {} ends read through process handles, an epoll_wait on a port's descriptor woken \
         by its packet after {} ms, {} refusals as the ABI says, {}",
        report.hello,
        report.taken,
        report.statuses,
        report.woken_after_ms,
        report.refusals,
        if report.from_a_program {
            "a program took its bootstrap after its execve and one given none was answered zero"
        } else {
            "no program run on this architecture"
        },
    );
}

/// Pid 1's inputs from the initramfs: each refused as `init::set_inputs`
/// says, and taken once.
pub(super) fn check_init_inputs() {
    let report = match init_check::check() {
        Ok(report) => report,
        Err(problem) => fatal!(
            catalog::INIT_INPUTS,
            "init's inputs self-check failed: {problem}"
        ),
    };
    println!(
        "  inputs   {} sets of entries under .ferrix/init judged, {} entries refused as each \
         should be and taken as absent, a second set_inputs refused",
        report.cases, report.refusals
    );
}

pub(super) fn check_cgroupfs() {
    let checked = match fs::cgroupfs::check() {
        Ok(checked) => checked,
        Err(problem) => fatal!(
            catalog::STAGE13_CGROUPFS,
            "stage 13 cgroupfs self-check failed: {problem}"
        ),
    };
    println!(
        "  cgroups  {} cgroups made and removed through cgroup2, a process moved in by its pid \
         and ended by cgroup.kill, {} writes and names refused as Linux refuses them, {} epoll \
         wait on cgroup.events woken with EPOLLPRI by the last release and not before, {} moves \
         judged by delegation to uid 1000, {} native waits for EMPTY fired with the populated \
         flip, cpu memory pids enabled and a fork refused at pids.max with {} controller writes \
         refused as Linux refuses them, {} program past a 1 MiB memory.max OOM-killed by SIGKILL \
         and counted in memory.events with EPOLLPRI, its sibling cgroup untouched, {}",
        checked.made,
        checked.refusals,
        checked.woken,
        checked.moves,
        checked.emptied,
        checked.controlled,
        checked.oom_killed,
        if checked.cloned {
            "a child started by CLONE_INTO_CGROUP in its cgroup from its first instruction"
        } else {
            "CLONE_INTO_CGROUP not run on this architecture"
        },
    );
    println!(
        "  creator  a native process made by uid 1000 in its delegated cgroup has its creator's \
         ids and exited with getuid's answer, {} (uid 1000's low byte; root's would be 0)",
        checked.created_as,
    );
    println!(
        "  limits   a cgroup delegated to uid 1000 left its limits to root: {} attempts on them \
         refused, natively and through its files, and a job it made itself limited",
        checked.limits_refused,
    );
}

/// sysfs (`docs/SYSFS.md`): the device tree mounted and walked through the
/// VFS, and what it says held against what enumeration and the cores know.
pub(super) fn check_sysfs() {
    let checked = match fs::sysfs::check::run() {
        Ok(checked) => checked,
        Err(problem) => fatal!(catalog::SYSFS, "sysfs self-check failed: {problem}"),
    };
    println!(
        "  sysfs    {} names walked in {} directories, {} links followed; {} devices, {} bound \
         as devmgr says, {} disks, {} interfaces and {} processors as their owners say; {} \
         refusals as kernfs refuses",
        checked.names,
        checked.directories,
        checked.links,
        checked.devices,
        checked.bound,
        checked.disks,
        checked.interfaces,
        checked.cpus,
        checked.refusals,
    );
}

/// Stage 8: the system calls that take a path, against the real namespace.
///
/// Halts rather than returning, as every other stage's check does.
pub(super) fn check_path_calls() {
    let report = match syscall::check::run_paths() {
        Ok(report) => report,
        Err(problem) => fatal!(
            catalog::STAGE8_PATH_CALLS,
            "stage 8 path call self-check failed: {problem}"
        ),
    };
    if let Err(problem) = syscall::check::run_handles() {
        fatal!(
            catalog::STAGE8_PATH_CALLS,
            "stage 8 path call self-check failed: {problem}"
        );
    }
    println!(
        "  paths    {} path calls under /tmp, {} names listed in {} getdents64 calls, \
         {} device nodes opened by number, {} frames leaked, dentry cache {:+}",
        report.calls,
        report.listed,
        report.listing_calls,
        report.devices,
        report.leaked,
        report.cache_growth,
    );
}

/// A program that makes a system call with its trap flag set has the call
/// served, instead of stopping the kernel.
///
/// Only x86-64 has a flag a program can set that changes how the kernel is
/// entered, and the architecture says so by giving a program here and an empty
/// one elsewhere. Without `TF` in `SYSCALL`'s flag mask, the call single-steps
/// the kernel's first instruction and this halts with a debug exception. The
/// call is `exit_group`, whose status is the proof it was served; one that
/// returned would trap in ring 3 instead, for the reason the program's
/// documentation gives.
///
/// Halts rather than returning, as every other stage's check does.
pub(super) fn check_trap_flag_entry() {
    if arch::USER_STEP_PROGRAM.is_empty() {
        println!(
            "  step     nothing a program sets changes how it enters the kernel on {}",
            arch::NAME
        );
        return;
    }

    let class = if usize::BITS == 64 {
        ferrix_elf::Class::Elf64
    } else {
        ferrix_elf::Class::Elf32
    };
    let file = syscall::image::build_with(
        class,
        arch::ARCH.elf_machine(),
        syscall::image::Shape::Good,
        arch::USER_STEP_PROGRAM,
    );
    let Ok(status) =
        syscall::exec::run(&file, &[b"/step"], &[], [0x5a; ferrix_ustack::RANDOM_BYTES])
    else {
        fatal!(
            catalog::STAGE7_SYSCALLS,
            "stage 7 self-check failed: the trap flag program could not be started"
        );
    };
    if status != arch::USER_STEP_STATUS {
        fatal!(
            catalog::STAGE7_SYSCALLS,
            "stage 7 self-check failed: a program that called exit_group with its trap flag \
             set did not exit with the status it asked for"
        );
    }
    println!(
        "  step     a program made a system call with its trap flag set and exited with {status}"
    );
}

/// What arrives in the kernel from ring 3 without being asked for: first a
/// trap flag a program set, which `SYSCALL` must mask; then the exceptions
/// nothing masks, which must find the kernel's `GS` wherever they land. In
/// that order, because the second is checked where the first was shown safe.
pub(super) fn check_entry_paths() {
    check_trap_flag_entry();
    check_exception_entry();
    check_speculation();
}

/// The side-channel defences every processor applied, read back, and the
/// barriers issued at the switches stage 7's programs made.
///
/// Halts rather than returning, as every other stage's check does.
pub(super) fn check_speculation() {
    let report = match arch::check_speculation() {
        Ok(report) => report,
        Err(problem) => fatal!(
            catalog::SPECULATION_DEFENCES,
            "side-channel defence self-check failed: {problem}"
        ),
    };
    if !report.hardened {
        println!(
            "  cpu      speculation defences off on {} processors, as built",
            report.processors
        );
        return;
    }
    println!(
        "  cpu      speculation defences read back on {} processors, {} switch barriers",
        report.processors, report.barriers,
    );
    // A big.LITTLE machine's little cores may need less than its big ones.
    if report.differing > 0 {
        println!(
            "  cpu      {} processors applied other than the boot processor's {}",
            report.differing,
            report.boot.names()
        );
    }
}

/// The exceptions nothing masks come back from wherever they land.
///
/// On x86-64 an NMI, a debug exception or a machine check can arrive on the
/// `SYSCALL` trampoline's instructions that run in ring 0 on the program's
/// stack and `GS`; the architecture raises an NMI and hardware breakpoints
/// there and requires each to return with the kernel's `GS`. The Arm pair
/// take every exception on a stack a program cannot set, and say so.
///
/// Halts rather than returning, as every other stage's check does.
pub(super) fn check_exception_entry() {
    if let Err(problem) = arch::check_exception_entry() {
        fatal!(
            catalog::STAGE3_TRAPS,
            "stage 3 self-check failed: {problem}"
        );
    }
}

/// Stage 7: the system call dispatch path, before there is anything to call
/// it.
///
/// Halts rather than returning, as every other stage's check does.
pub(super) fn check_syscalls() {
    let report = match syscall::check::run() {
        Ok(report) => report,
        Err(problem) => fatal!(
            catalog::STAGE7_SYSCALLS,
            "stage 7 self-check failed: {problem}"
        ),
    };

    println!(
        "  syscall  {} numbers dispatched, {} answered, getpid is {} on {}",
        report.dispatched,
        report.answered,
        report.getpid_number,
        arch::NAME,
    );
    println!(
        "  pids     {} processes numbered, found by pid, listed in order and let go",
        report.pids,
    );
    println!(
        "  uaccess  {} pages mapped, written and read back through a user space, \
         {} frames leaked",
        report.pages, report.leaked,
    );
    match report.user_status {
        Some(status) => println!("  usermode a program ran in user mode and exited with {status}"),
        None => println!("  usermode not on {} yet", arch::NAME),
    }
    if let Some((first, second)) = report.concurrent {
        println!(
            "  procs    two programs took turns on one processor, switched to {first} and \
             {second} times"
        );
    }
    if let Some(status) = report.killed {
        println!("  kill     a spinning program was ended from outside and reported {status}");
    }
    if let Some(status) = report.forked {
        println!("  fork     a program forked, waited for its child, and exited with {status}");
    }
    print_threads(&report);
    if let Some((runs, window)) = report.reclaimed {
        println!(
            "  exits    {runs} programs that forked and exited gave every frame back once \
             reaped, in window {window}"
        );
    }
    if let Some(status) = report.signalled {
        println!(
            "  signals  a program's handler ran on its own frame, changed a saved register, \
             returned through sigreturn, and the program exited with {status}"
        );
    }
    if let Some(status) = report.copied {
        println!(
            "  cow      a forked child and its parent each wrote a page the other shared, \
             neither saw the other's write, and the program exited with {status}"
        );
    }
    if let Some(status) = report.shared {
        println!(
            "  shared   a child's writes to MAP_SHARED pages reached its parent and its \
             MAP_PRIVATE write did not; the program exited with {status}"
        );
    }
    if let Some(status) = report.narrowed {
        println!(
            "  mprotect a program wrote a page, made it read-only, wrote it again and was \
             ended with {status}"
        );
    }
    println!(
        "  cost     ms per check: numbers={} handlers={} user={} procs={} kill={} fork={} signals={} execve={} futex={}",
        report.spent_ms[0],
        report.spent_ms[1],
        report.spent_ms[2],
        report.spent_ms[3],
        report.spent_ms[4],
        report.spent_ms[5],
        report.spent_ms[6],
        report.spent_ms[7],
        report.spent_ms[8],
    );
    if let Some((found, missing)) = report.execed {
        println!(
            "  execve   a program became another and exited with {found}; with the file gone \
             it got errno {missing}"
        );
    }
    if let Some(status) = report.started_with {
        println!(
            "  argument a program started with an argument found it on entry and exited with {status}"
        );
    }
    println!(
        "  futex    a changed word got EAGAIN and a timed wait ETIMEDOUT; a wake, a requeue and a \
         wake from a fork child on a MAP_SHARED word roused {} waiters, and a wake that roused \
         nobody and one keyed by the waker's own space were caught",
        report.futex_woken,
    );
    println!("  heap     a brk waited for a fork holding the heap lock, and a fork for a brk");
}

/// Stage 7's thread checks, and the vDSO's, as `check_syscalls` reports them:
/// each line only when its check ran on this architecture and processor
/// count.
pub(super) fn print_threads(report: &syscall::check::Report) {
    if report.vdso_trampoline_only {
        println!(
            "  vdso     the signal return trampoline exported where its code is, and found by a \
             space it is mapped into; the pages refused writes"
        );
    } else if let Some(answered) = report.vdso {
        println!(
            "  vdso     a program called clock_gettime for seven clocks, gettimeofday and time through \
             AT_SYSINFO_EHDR, each answered by {answered} between two system calls; the pages \
             refused writes"
        );
    }
    if report.unmap_waited {
        println!(
            "  unmap    an unmap on one processor waited for a copy on another to let go of its page"
        );
    }
    if let Some(status) = report.threaded {
        println!(
            "  threads  a program's threads ran in its memory and ended alone, one cleared its id as \
             it went and one without CLONE_CHILD_CLEARTID did not, and it exited with {status}"
        );
    }
    if let Some(runs) = report.exits_together {
        println!(
            "  threads  {runs} programs whose last two threads called exit together each ended \
             with its first thread's status"
        );
    }
    if let Some(status) = report.dethreaded {
        println!(
            "  threads  a thread replaced its program while the first thread waited, which was \
             ended; the process kept its pid and exited with {status}"
        );
    }
    if let Some(status) = report.stopped_threads {
        println!(
            "  threads  a stop parked all three threads of a program and a continue ran them \
             again, a futex wait restarted rather than failed; SIGKILL ended it with {status}"
        );
    }
    if let Some(status) = report.handed_on {
        println!(
            "  threads  a signal reached the one thread not blocking it, and one a handler's mask \
             blocked was handed on to it; SIGKILL ended the program with {status}"
        );
    }
}

/// Stage 6's reverse map: a shared object's page decommitted, replaced and
/// held under two processes on two processors, which must never reach a frame
/// the object gave back.
///
/// Halts rather than returning, as every other stage's check does.
pub(super) fn check_reverse_map() {
    let report = match user::rmap_check::run() {
        Ok(Some(report)) => report,
        Ok(None) => {
            println!(
                "  rmap     needs two processors and a program for {}",
                arch::NAME
            );
            return;
        }
        Err(problem) => fatal!(
            catalog::STAGE6_REVERSE_MAP,
            "stage 6 reverse map self-check failed: {problem}"
        ),
    };
    let (taken_on, other) = report.processors;
    println!(
        "  rmap     {} frames taken from two processes on processors {taken_on} and {other} and \
         poisoned, never reached from user mode; a held page kept its mappings; scoped \
         shootdowns {} sent to {} processors and {} needing no interrupt, against {} global; \
         {} frames leaked",
        report.taken,
        report.scoped.sent,
        report.scoped.processors,
        report.scoped.unsent,
        report.global,
        report.leaked,
    );
}

/// Stage 9: the native ABI's objects, driven through their handlers by two
/// processes the check builds, before any program can make a native call.
///
/// Halts rather than returning, as every other stage's check does.
pub(super) fn check_native_objects() {
    let report = match object::check::run() {
        Ok(report) => report,
        Err(problem) => fatal!(
            catalog::STAGE9_OBJECTS,
            "stage 9 self-check failed: {problem}"
        ),
    };

    println!(
        "  native   {} messages and {} handles carried between two processes, \
         {} refusals as specified, {} frames leaked",
        report.messages, report.moved, report.refusals, report.leaked,
    );
    println!(
        "  jobs     {} processes in a tree of three jobs ended by two kills, \
         {} wait woken by a message rather than its deadline",
        report.killed, report.woken,
    );
    println!(
        "  ports    {} packets taken from ports, from programs and from registrations \
         that fired",
        report.packets,
    );
    println!(
        "  exit     two programs in user mode exchanged {} messages and a handle over a channel",
        report.exchanged,
    );
    println!(
        "  spawn    {} programs made from a VMO and started through a handle, their ends heard; \
         {} never started, ended with their last handle",
        report.spawned, report.abandoned,
    );
}

/// Stage 9: allocation failure, injected under the native calls, survived
/// (finding F-23).
///
/// Halts rather than returning, as every other stage's check does.
pub(super) fn check_allocation_failure() {
    let report = match object::alloc_check::run() {
        Ok(report) => report,
        Err(problem) => fatal!(
            catalog::STAGE9_ALLOCATION,
            "allocation failure self-check failed: {problem}"
        ),
    };
    println!(
        "  no-mem   {} native calls with {} allocations failed under them: {} answered \
         NO_MEMORY, the rest succeeded, nothing leaked; {} allocations served from a reserve; \
         {} pages decommitted with none",
        report.calls, report.injected, report.refused, report.drawn, report.torn_down,
    );
}

/// `defer` giving an object up past the in-place depth, counted, with the
/// kernel running on (L.object.8). Halts rather than returning.
pub(super) fn check_a_give_up() {
    let given_up = match object::alloc_check::give_up() {
        Ok(given_up) => given_up,
        Err(problem) => fatal!(
            catalog::STAGE9_ALLOCATION,
            "allocation failure self-check failed: {problem}"
        ),
    };
    println!(
        "  give-up  a close {} drops deep in place with no memory gave up {given_up} object, \
         counted, and the kernel ran on",
        object::IN_PLACE_DEPTH
    );
}

/// Finding F-35: the job quotas, refused at exactly their limits with a
/// sibling untouched, and empty again after.
///
/// Halts rather than returning, as every other stage's check does.
pub(super) fn check_quotas() {
    let report = match object::quota_check::run() {
        Ok(report) => report,
        Err(problem) => fatal!(catalog::STAGE9_QUOTAS, "quota self-check failed: {problem}"),
    };
    println!(
        "  quota    a fork loop refused at its job's {} tasks; faults refused at {} pages ({} \
         of them page tables, beside {} bytes of its regions' heap) while a sibling job \
         faulted in {}; objects refused at {}; one \
         task alone in its job kept {}.{}% of a processor against eight in another; every \
         counter back to zero and every quota slot given back",
        report.tasks,
        report.pages,
        report.tables,
        report.heap,
        report.sibling_pages,
        report.objects,
        report.alone_share / 10,
        report.alone_share % 10,
    );
}

/// Finding F-37: the kernel heap a program makes through the Linux calls,
/// charged to its job and refused at its memory limit, kind by kind, with a
/// sibling untouched and everything given back.
///
/// Halts rather than returning, as every other stage's check does.
pub(super) fn check_kernel_memory() {
    let report = match fs::kmem_check::run() {
        Ok(report) => report,
        Err(problem) => fatal!(
            catalog::STAGE9_KMEM,
            "kernel memory self-check failed: {problem}"
        ),
    };
    println!(
        "  kmem     at a {} KiB memory limit a job made {} files, {} pipes, {} socket pairs, \
         {} descriptors in flight, {} epoll registrations, {} eventfds, {} regions of one \
         mapping, {} record locks, {} semaphore sets, {} shared memory segments, {} mount namespaces of {} mounts, {} user, {} UTS, {} IPC, {} cgroup and {} pid namespaces, {} pid numbers and {} namespace files, \
         {} network namespaces, {} veth pairs and {} routes in a network namespace, {} seccomp filters, and \
         was refused one more of each -- \
         ENOMEM, ENOLCK for a lock -- while a sibling made one; every byte of heap charged \
         came back",
        fs::kmem_check::LIMIT / 1024,
        report.files,
        report.pipes,
        report.sockets,
        report.in_flight,
        report.registrations,
        report.eventfds,
        2 * report.regions + 1,
        report.locks,
        report.sets,
        report.segments,
        report.namespaces,
        fs::kmem_check::TREE,
        report.user_namespaces,
        report.uts_namespaces,
        report.ipc_namespaces,
        report.cgroup_namespaces,
        report.pid_namespaces,
        report.pid_numbers,
        report.namespace_files,
        report.network_namespaces,
        report.veth_pairs,
        report.routes,
        report.filters,
    );
}

/// Stage 9: interrupts and I/O mappings, minted from stage 10's device nodes.
///
/// Halts rather than returning, as every other stage's check does.
pub(super) fn check_device_objects() {
    let report = match object::check::run_devices() {
        Ok(report) => report,
        Err(problem) => fatal!(
            catalog::STAGE9_OBJECTS,
            "stage 9 device object self-check failed: {problem}"
        ),
    };
    println!(
        "  handles  {} device aperture mapped into a process and reached from a forked \
         child, {} mapped write-combining, {} interrupt held from delivery to acknowledgement, \
         {} VMO pages pinned for a device and found at their device addresses, {} refusals as \
         specified",
        report.mapped, report.combined, report.interrupts, report.pinned, report.refusals,
    );
    // F-55: a copy through any of them is refused, and the boot goes on.
    let copies = match object::device_copy_check::run() {
        Ok(copies) => copies,
        Err(problem) => fatal!(
            catalog::STAGE9_OBJECTS,
            "stage 9 device object self-check failed: {problem}"
        ),
    };
    println!(
        "  copies   {} copies to and from {} device windows EFAULT before any page was faulted \
         in: {}a cached window past all RAM{}",
        copies.refused,
        copies.windows,
        if copies.aperture {
            "a device's aperture, "
        } else {
            "no aperture on this machine, "
        },
        if copies.inside {
            ", and one between two runs of RAM"
        } else {
            "; no hole between runs of RAM for one inside the direct map"
        },
    );
    if report.delivered > 0 {
        println!(
            "  wake     {} of {} interrupt deliveries ended their wait by waking it, the slowest \
             returning after {} us",
            report.wakes,
            report.delivered,
            report.slowest_wake / 1_000,
        );
    }
    if report.coalesced > 0 {
        println!(
            "  irq      {} edge-triggered MSI-X vector left unmasked by its deliveries, a delivery \
             between acknowledgement and drain queued its own packet, and the delivery past {} \
             unacknowledged masked it until the acknowledgement ({} us for those, charged to the \
             task that ran them)",
            report.coalesced,
            object::interrupt::STORM_BOUND,
            report.storm_nanos / 1_000,
        );
    }
}

/// Stage 10's block ring control plane, from a process given a device: all
/// check, and on a machine with no PCI function nothing at all.
pub(super) fn check_ring_control() {
    let report = match block_ring::check::run() {
        Ok(report) => report,
        Err(problem) => fatal!(
            catalog::STAGE10_RING,
            "stage 10 block ring self-check failed: {problem}"
        ),
    };
    println!(
        "  reread   {} fields of a posted completion rewritten by its driver after the kernel \
         read them, the kernel acting on its first read of each",
        report.rewrites,
    );
    if let Some(why) = report.skipped {
        println!("  ring     not checked: {why}");
    } else {
        println!(
            "  ring     {} calls and HELLOs refused as specified, {} disk published from an \
             accepted HELLO, unpublished when its driver stopped and parked for the next when it \
             died, {} frames leaked",
            report.refusals, report.published, report.leaked,
        );
    }
}

/// Stage 10's chardev core, from a fake driver given a PCI function and
/// programs played by kernel tasks (N12; F-63): all check, and on a machine
/// with no PCI function nothing at all.
pub(super) fn check_chardev() {
    let report = match chardev::check::run() {
        Ok(report) => report,
        Err(problem) => fatal!(
            catalog::STAGE10_CHARDEV,
            "stage 10 chardev self-check failed: {problem}"
        ),
    };
    if let Some(why) = report.skipped {
        println!("  chardev  not checked: {why}");
        return;
    }
    println!(
        "  chardev  {} HELLOs, copies and requests refused as specified, {} requests answered, \
         {} abandoned with the queue to a driver that reads nothing at most {} requests{}",
        report.refusals,
        report.answered,
        report.abandoned,
        report.most_queued,
        if report.one_device {
            "; one PCI function, so no second driver"
        } else {
            ""
        },
    );
}

/// The net ring, played from both ends with no network device: the whole
/// kernel side of an interface, from the control handshake to a frame in and a
/// frame out.
///
/// Halts rather than returning, as every other stage's check does.
pub(super) fn check_net_ring() {
    let report = match net_ring::check::run() {
        Ok(report) => report,
        Err(problem) => fatal!(catalog::NET_RING, "net ring self-check failed: {problem}"),
    };
    if let Some(why) = report.skipped {
        println!("  netring  not checked: {why}");
        return;
    }
    println!(
        "  netring  {} HELLOs refused as specified, {} slots posted for a driver to fill, \
         {} frames taken up the stack and {} answered back down it, \
         {} through a packet socket; the interface parked when its driver went \
         and taken up again, address and all, by the next",
        report.refusals, report.posted, report.received, report.sent, report.packet_frames,
    );
}

/// `AF_NETLINK`, over the same loopback: the requests `ip` makes are answered
/// from the net core, and what they changed is in the next dump.
///
/// Here, straight after the net core's own check, because it is the same
/// subsystem reached through a different family: a socket, a buffer of
/// requests, and the tables `src/lib/network/net` holds. Nothing about it touches a
/// device either.
///
/// Halts rather than returning, as every other stage's check does.
pub(super) fn check_netlink() {
    let report = match net::netlink::check::run() {
        Ok(report) => report,
        Err(problem) => fatal!(catalog::NETLINK, "netlink self-check failed: {problem}"),
    };
    if let Some(why) = report.skipped {
        println!("  netlink  not checked: {why}");
        return;
    }
    println!(
        "  netlink  {} links, {} addresses and {} routes dumped, an address and a route added \
         and taken away again, {} requests refused as specified",
        report.links, report.addresses, report.routes, report.refusals,
    );
}

/// The log core, played from a driver's end: READ answered with the log's
/// oldest bytes and then the next, a second reader refused, a driver that
/// breaks the protocol refused, and a claim ended by its channel.
pub(super) fn check_log_control() {
    match logctl::check::run() {
        Ok(report) => println!(
            "  logctl   {} bytes from the log's oldest in two DATA ({} lost before them), a \
             second reader refused, a driver that lied lost its claim, a closed claim ended in {} ms",
            report.carried, report.lost, report.released_ms,
        ),
        Err(problem) => fatal!(
            catalog::LOG_CONTROL,
            "log control self-check failed: {problem}"
        ),
    }
}

/// Stage 11's exit: the btrfs fixture on the second disk, served by its own
/// driver, mounted at `/mnt` through the kernel's mount path and read back
/// against the fixture's manifest, file by file. The mount is left for the
/// programs that run next.
///
/// Halts rather than returning, as every other stage's check does. A machine
/// without a second disk passes and says so.
pub(super) fn check_btrfs_disk() {
    let report = match fs::btrfs_check::run() {
        Ok(report) => report,
        Err(problem) => fatal!(
            catalog::STAGE11_MOUNT,
            "stage 11 self-check failed: {problem}"
        ),
    };
    if let Some(why) = report.skipped {
        println!("  btrfs    not checked: {why}");
        return;
    }
    println!(
        "  btrfs    vdb mounted read-only at /mnt: {} files ({} bytes), {} directories and {} \
         links read back as the host wrote them",
        report.files, report.bytes, report.directories, report.links,
    );
}

/// Stage 5: start the scheduler, and require it to be fair.
///
/// Halts rather than returning, for the reason `bring_up_processors` does:
/// "stage 5 failed" would say nothing about which of four checks, on which of
/// a thousand threads, did.
/// The audit record at the end of boot: every decision the boot's own
/// checks made at a recording site is in it (`audit::check::booted`).
pub(super) fn check_audit_booted(translating: bool) {
    match audit::check::booted(&audit::check::Booted { translating }) {
        Ok(found) => println!(
            "  audit    {found} kinds of decision the boot's checks made are recorded, each with \
             its outcome and the subject that decided it; the boot recorded among its own records"
        ),
        Err(problem) => fatal!(catalog::AUDIT_STORE, "audit self-check failed: {problem}"),
    }
}

/// Stage 3's exit criterion: the kernel can take a trap and carry on.
///
/// Two things, and the second is the one that matters. A breakpoint proves the
/// whole entry path works — vector, register save, dispatch, restore, return —
/// because execution continues on the next instruction with every register
/// intact. A page fault proves the kernel can *resolve* a fault and let the
/// faulting instruction retry, which is exactly what demand paging is, and is
/// how every anonymous mapping will work from stage 6.
pub(super) fn trap_check() -> Result<(), &'static str> {
    check_breakpoint()?;
    check_demand_paging()
}

/// A breakpoint must return to the instruction after it, twice.
///
/// Verifies: L.aarch64.6
pub(super) fn check_breakpoint() -> Result<(), &'static str> {
    let before = trap::breakpoint_count();

    // A canary in a register the trap frame saves and restores. If the entry
    // path drops a register, this is what notices.
    let canary: u64 = 0x0123_4567_89AB_CDEF;
    let mut witness = canary;

    arch::breakpoint();
    witness = witness.rotate_left(1);
    arch::breakpoint();

    if trap::breakpoint_count() != before + 2 {
        return Err("a breakpoint did not reach the handler");
    }
    if witness != canary.rotate_left(1) {
        return Err("a register did not survive the trap");
    }
    Ok(())
}

/// A fault in the on-demand window must be resolved by mapping a page.
pub(super) fn check_demand_paging() -> Result<(), &'static str> {
    let before = trap::handled_fault_count();
    let free_before = mm::free_frames();

    // Three pages, touched out of order, so a handler that mapped a fixed
    // address rather than the faulting one would fail here.
    let probes = [
        mm::DEMAND_WINDOW + 0x2000,
        mm::DEMAND_WINDOW,
        mm::DEMAND_WINDOW + 0x1000,
    ];

    for (index, probe) in probes.iter().enumerate() {
        if mm::translate(*probe).is_some() {
            return Err("the on-demand window was already mapped");
        }

        let value = 0xFEED_0000_u64 + index as u64;
        // SAFETY: (PROBE) nothing is mapped here, which is the point: the write takes a
        // page fault, the handler maps a zeroed page, and the CPU retries the
        // instruction. `volatile` so the compiler cannot decide the write is
        // dead and remove the fault along with it.
        unsafe { core::ptr::write_volatile(*probe as *mut u64, value) };
        // SAFETY: (PROBE) the page is mapped now, by the fault the write above took.
        let read_back = unsafe { core::ptr::read_volatile(*probe as *const u64) };

        if read_back != value {
            return Err("memory faulted in did not hold what was written to it");
        }
        if mm::translate(*probe).is_none() {
            return Err("the fault handler did not leave a mapping behind");
        }
    }

    let handled = trap::handled_fault_count() - before;
    if handled != probes.len() as u64 {
        return Err("the number of faults handled does not match the pages touched");
    }

    // Each fault consumes a frame for the page itself, and the *first* one into
    // a fresh region also consumes frames for the page tables above it -- three
    // of them here, since nothing was mapped in this window at all. So the
    // total is bounded rather than exact.
    let consumed = free_before.saturating_sub(mm::free_frames());
    if consumed < probes.len() as u64 {
        return Err("faulting in pages consumed fewer frames than pages");
    }
    if consumed > probes.len() as u64 + 3 {
        return Err("faulting in pages consumed more frames than pages plus a table per level");
    }

    // What *is* exact: a fault into a region whose tables already exist costs
    // one frame and no more. Checking it separately is what makes the bound
    // above a measurement rather than a shrug.
    let settled = mm::free_frames();
    let neighbour = mm::DEMAND_WINDOW + 0x3000;
    // SAFETY: (PROBE) unmapped, so this faults; the handler maps a zeroed page and the
    // instruction retries.
    unsafe { core::ptr::write_volatile(neighbour as *mut u64, 1) };
    if mm::free_frames() != settled - 1 {
        return Err("a fault into an already-tabled region cost more than one frame");
    }

    // The rest of the page must read as zero: a page handed out still holding
    // the last owner's data is an information leak, and from stage 6 the last
    // owner is another process.
    // SAFETY: (PROBE) mapped by the faults above.
    let tail = unsafe { core::ptr::read_volatile((mm::DEMAND_WINDOW + 0x800) as *const u64) };
    if tail != 0 {
        return Err("a faulted-in page was not zeroed");
    }
    Ok(())
}

/// A one-shot must fire exactly once.
///
/// **This is the check that catches the bug worth catching here.** AArch64's
/// timer interrupt is level triggered: the line stays asserted while the
/// comparator is in the past, so a handler that acknowledges the controller
/// without disarming the timer is re-entered immediately, forever. That does
/// not show up as a wrong number — it shows up as a machine that stops, with
/// the last thing in the log being whatever it printed before arming.
///
/// So: arm once, wait for the tick, then wait several further intervals and
/// require the count not to have moved.
///
/// **On x86-64 the second half cannot fail, and that is known.** The local
/// APIC's one-shot does not refire whether or not the handler disarms it, so
/// there is no handler bug for the count to catch. The one mistake that would
/// make it refire -- programming the timer's LVT in periodic mode -- was tried:
/// at this one-millisecond interval under `tcg` the boot stops after the
/// interrupt bring-up line and never reaches this check's report, the same
/// shape as AArch64's storm. There the boot test's timeout is what catches it,
/// and what this check contributes is its first half: that the timer fires.
pub(super) fn check_one_shot(interval_nanos: u64) -> Result<(), &'static str> {
    let before = timer::ticks();
    timer::after(interval_nanos);

    let mut spins: u64 = 0;
    while timer::ticks() == before {
        arch::wait_for_interrupt();
        spins = spins.saturating_add(1);
        if spins > 10_000_000 {
            timer::stop();
            return Err("a one-shot timer never fired");
        }
    }

    let settled = timer::ticks();
    spin_nanos(interval_nanos.saturating_mul(10));
    if timer::ticks() != settled {
        timer::stop();
        return Err("a one-shot timer fired more than once");
    }
    Ok(())
}

/// How long the skipped-arm check gives a timer interrupt to arrive after its
/// deadline: an emulated processor on a loaded host, not the skip, which adds
/// nothing (`timer::ARMED`). Far below the long one-shot, so a short one
/// that waited for it is plainly late.
const ARRIVAL_NANOS: u64 = 200_000_000;
/// The long one-shot of the skipped-arm check.
const LONG_ARM_NANOS: u64 = 1_000_000_000;
/// The short one.
const SHORT_ARM_NANOS: u64 = 2_000_000;

/// What the skipped-arm check measured: how long after it was asked for the
/// short one-shot fired, in microseconds, asked after the long one and then
/// before it.
pub(super) struct SkippedArm {
    /// Short asked for after long: written over it.
    pub(super) after_long: u64,
    /// Long asked for after short: skipped, the short left to fire.
    pub(super) before_long: u64,
}

/// A one-shot asked for while a later one is armed fires by its own
/// deadline, and one asked for while an earlier one is armed leaves the
/// earlier one to fire: `timer::after`'s skip, which keeps an upper bound on
/// the armed interrupt and writes only an earlier deadline.
///
/// On the boot processor once it has its record, which the skip needs, and
/// before any other processor or the scheduler arms a timer, so every tick
/// counted is this processor's. Each one-shot must fire within
/// [`ARRIVAL_NANOS`] of its 2 ms deadline; one the skip wrongly left waits
/// for the 1 s one.
///
/// Verifies: L.sched.5
pub(super) fn check_a_skipped_arm() -> Result<SkippedArm, &'static str> {
    if smp::this_cpu().is_none() {
        return Err("the skipped-arm check ran before the boot processor had its record");
    }
    let after_long = short_fires_in_time(&[LONG_ARM_NANOS, SHORT_ARM_NANOS]).ok_or(
        "a 2 ms one-shot asked for while a 1 s one was armed did not fire by its deadline",
    )?;
    let before_long = short_fires_in_time(&[SHORT_ARM_NANOS, LONG_ARM_NANOS]).ok_or(
        "a 1 s one-shot asked for while a 2 ms one was armed kept the 2 ms one from firing",
    )?;
    Ok(SkippedArm {
        after_long,
        before_long,
    })
}

/// Ask for each of `arms` in order, then wait for the first tick: the
/// microseconds it took, if it came within [`ARRIVAL_NANOS`] of the short
/// deadline.
fn short_fires_in_time(arms: &[u64]) -> Option<u64> {
    let before = timer::ticks();
    let start = timer::now_nanos();
    for nanos in arms {
        timer::after(*nanos);
    }
    let give_up = start.saturating_add(LONG_ARM_NANOS + ARRIVAL_NANOS);
    while timer::ticks() == before && timer::now_nanos() < give_up {
        arch::wait_for_interrupt();
    }
    let took = timer::now_nanos().saturating_sub(start);
    timer::stop();
    (timer::ticks() != before && took <= SHORT_ARM_NANOS + ARRIVAL_NANOS).then_some(took / 1000)
}

/// Spin on the counter for `nanos`.
///
/// Deliberately not `wait_for_interrupt`: the point is to let real time pass
/// while *not* waiting for a timer, so that a timer which fires anyway is
/// noticed.
pub(super) fn spin_nanos(nanos: u64) {
    let until = timer::now_nanos().saturating_add(nanos);
    while timer::now_nanos() < until {}
}

/// A task's write to the console, checked to leave the writer at once and go
/// out by the port's transmit interrupt, and said how it went; then the log
/// every byte the console sends is recorded in.
pub(super) fn check_console_output() {
    match console::output::check() {
        // What the interrupt sent while the line went out, which includes
        // whatever was queued ahead of it: on the DK1 the boot's own lines
        // are, and the writer then leaves all but a few bytes to the port.
        Ok(Some(checked)) => println!(
            "  output   a task's line of {} bytes went out by interrupt: {} bytes sent by the \
             transmit interrupt into {} meanwhile, what was queued ahead of it included, {} by \
             the writer",
            checked.line,
            checked.by_interrupt,
            arch::console::transmit_buffer(),
            checked.by_writer,
        ),
        Ok(None) => {
            println!("  output   the port is polled: it has no interrupt this kernel installed");
        }
        Err(problem) => fatal!(
            catalog::CONSOLE_OUTPUT,
            "console output self-check failed: {problem}"
        ),
    }
    check_console_log();
}

/// The kernel log, which every line after this is recorded in: what a reader
/// of it is promised, and what the console records and keeps out. Here, as
/// the output check, because it needs tasks: two racing writers and a task's
/// write.
pub(super) fn check_console_log() {
    if !checks::run() {
        return;
    }
    match console::log_check::run() {
        Ok(report) => println!(
            "  log      {} bytes kept of a wrapped ring and {} reported lost, resumed after \
             a partial read; {} bytes from two writers on {} processor(s) counted, {} read \
             while they wrote; a task's write and a failure report recorded, an unlogged line not",
            report.kept, report.lost, report.raced, report.processors, report.read_racing,
        ),
        Err(problem) => fatal!(
            catalog::CONSOLE_LOG,
            "console log self-check failed: {problem}"
        ),
    }
}

/// The rest of stage 3's exit criterion: arm a timer, count the ticks,
/// and require the rate to be the one that was asked for.
///
/// The measurement is what makes this a test rather than a demonstration. A
/// timer that fires is easy; a timer that fires at the frequency it was
/// programmed to is the thing every later stage depends on, because a
/// scheduler quantum, a `TCP` retransmit and a `futex` timeout are all this
/// number multiplied by something.
///
/// Ticks are counted with the *interrupt* and elapsed time is measured with
/// the *counter* — two independent pieces of hardware on x86-64. Counting
/// ticks and then converting them to seconds by the rate they were programmed
/// at would be arithmetic, not a measurement: it could not fail.
///
/// Verifies: L.aarch64.24
pub(super) fn timer_check() -> Result<u64, &'static str> {
    /// Ticks to count.
    ///
    /// A quarter of a second's worth. It was a thousand, a full second, and
    /// the tolerance below never needed it: three architectures report within
    /// two parts in a thousand, and the check is about whether the clock and
    /// the timer agree on a second, which they agree on just as well over a
    /// quarter of one. A second per boot per architecture, run dozens of
    /// times a day, was the boot test's single largest fixed cost.
    const TICKS: u64 = 250;
    /// The interval to ask for: a millisecond, so the rate under test is the
    /// kilohertz every architecture's timer is expected to keep.
    const INTERVAL_NANOS: u64 = 1_000_000;
    /// How far the measured rate may sit from the requested one.
    ///
    /// Generous, and still generous now that it need not be: the periods are
    /// a schedule measured from the deadlines themselves, so a tick that
    /// arrives late no longer pushes its successor late, and all three
    /// architectures report within two parts in a thousand. What is left for
    /// the tolerance to absorb is a host too loaded to deliver a thousand
    /// interrupts in a second at all, which is a fact about the host.
    ///
    /// What this is testing is that the clock and the timer agree about how
    /// long a second is -- not the interrupt latency, which is stage 14's
    /// subject and needs a different test. Until the re-arm was fixed it was
    /// quietly testing both, and the latency term was the larger one.
    const TOLERANCE_PERCENT: u64 = 25;

    if timer::counter_hz() == 0 {
        return Err("the counter reports no frequency");
    }

    check_one_shot(INTERVAL_NANOS)?;

    let before = timer::ticks();
    let started = timer::now_nanos();
    timer::every(INTERVAL_NANOS);

    let mut spins: u64 = 0;
    while timer::ticks().wrapping_sub(before) < TICKS {
        arch::wait_for_interrupt();
        spins = spins.saturating_add(1);
        // `wait_for_interrupt` can return without one having arrived, so this
        // counts iterations rather than trusting it. Far more than the ticks
        // could need, and far less than the boot test's timeout.
        if spins > 10_000_000 {
            timer::stop();
            return Err("the timer stopped arriving before the ticks were counted");
        }
    }

    let elapsed = timer::now_nanos().saturating_sub(started);
    timer::stop();

    if elapsed == 0 {
        return Err("the ticks took no measurable time");
    }
    if irq::unclaimed() != 0 {
        return Err("an interrupt arrived that nothing had registered for");
    }
    if irq::delivered() < TICKS {
        return Err("fewer interrupts reached a handler than ticks were counted");
    }

    let measured = TICKS * 1_000_000_000 / elapsed;
    let requested = 1_000_000_000 / INTERVAL_NANOS;
    let lowest = requested * (100 - TOLERANCE_PERCENT) / 100;
    let highest = requested * (100 + TOLERANCE_PERCENT) / 100;
    if measured < lowest || measured > highest {
        // The numbers as well as the verdict: which way the rate is off, and
        // by how much, is most of the diagnosis. Slow means interrupts arrive
        // late; fast means the timer was programmed from a wrong frequency.
        println!(
            "  timer    {TICKS} ticks took {} ms: {measured} Hz against {requested} requested",
            elapsed / 1_000_000
        );
        return Err("the timer and the counter disagree about how long a second is");
    }
    Ok(measured)
}

/// Prove the kernel runs where the loader says it put it, and that it moved
/// if it was built to (KASLR, `docs/certification/SPECULATION.md` §6).
///
/// The running image's first byte is `__kernel_start` as the code itself
/// computes it, which after the loader's fixups is where the image really is.
/// A loader that says it moved the image, and did not, would be reporting a
/// randomisation that never happened; a kernel built `--mitigations on` that
/// arrived as a fixed-address image lost its fixups on the way, in the build
/// or in a copy stripped of them. Every other reason not to move is honest
/// and reported, not failed: `nokaslr`, no source of randomness, a loader
/// that does not randomise.
pub(super) fn check_layout(view: &BootView<'_>) -> Result<(), &'static str> {
    let info = view.raw();
    let kaslr = info.kaslr;
    let running = backtrace::image_start();
    if running != info.kernel_virt {
        return Err("the kernel is not running where the loader says it put it");
    }
    let moved = running != kaslr.link;
    match kaslr.state {
        KASLR_MOVED if !moved => {
            Err("the loader says it moved the kernel, and it runs at its link address")
        }
        KASLR_MOVED => Ok(()),
        _ if moved => Err("the kernel moved, and the loader says it did not"),
        KASLR_FIXED_IMAGE if arch::HARDENED => Err(
            "a kernel built to move (--mitigations on) arrived without its fixups; \
             a stripped copy loses them, `--strip-debug` keeps them",
        ),
        _ => Ok(()),
    }
}

/// Require each thing the loader handed over to lie inside one region of the
/// memory map, of the kind that keeps it.
///
/// The kind decides what becomes of the frames: the allocator takes `Usable`
/// at once, and `Loader` data when boot memory is reclaimed. So it is not
/// enough that a region of each kind exists somewhere. A boot stack reported
/// as loader data would be handed out while the kernel still ran on it, and
/// a check that only looked for a `BootStack` region anywhere would pass.
pub(super) fn check_loader_allocations(view: &BootView<'_>) -> Result<(), &'static str> {
    let info = view.raw();
    // The boot info, its array and the stack are handed over as direct-map
    // addresses; everything else as physical ones.
    let physical = |virt: u64| {
        virt.checked_sub(info.physmap_base)
            .and_then(|offset| offset.checked_add(info.physmap_phys))
    };
    let info_at = physical(core::ptr::from_ref(info).addr() as u64);
    let array_at = physical(view.regions().as_ptr().addr() as u64);
    let array_len = size_of_val(view.regions()) as u64;
    let stack_base =
        physical(info.boot_stack_top).and_then(|top| top.checked_sub(info.boot_stack_size));

    let required = [
        (
            Some(info.kernel_phys),
            info.kernel_len,
            MemKind::Kernel,
            "the memory map does not describe the kernel image as the kernel",
        ),
        (
            Some(info.root_table_phys),
            PAGE_SIZE,
            MemKind::PageTables,
            "the memory map does not describe the root table as page tables",
        ),
        (
            info_at,
            size_of::<BootInfo>() as u64,
            MemKind::BootInfo,
            "the memory map does not describe the boot info as boot info",
        ),
        (
            array_at,
            array_len,
            MemKind::BootInfo,
            "the memory map does not describe its own array as boot info",
        ),
        (
            stack_base,
            info.boot_stack_size,
            MemKind::BootStack,
            "the memory map does not describe the boot stack as the boot stack",
        ),
    ];
    for (at, len, kind, problem) in required {
        if !at.is_some_and(|at| described_as(view, at, len, kind)) {
            return Err(problem);
        }
    }

    let optional = [
        (
            (info.ttbr0_phys != 0).then_some((info.ttbr0_phys, PAGE_SIZE)),
            MemKind::PageTables,
            "the memory map does not describe the identity root table as page tables",
        ),
        (
            view.device_tree(),
            MemKind::DeviceTree,
            "the memory map does not describe the device tree copy as the device tree",
        ),
        (
            view.initrd(),
            MemKind::Initrd,
            "the memory map does not describe the initramfs as the initramfs",
        ),
    ];
    for (range, kind, problem) in optional {
        if range.is_some_and(|(at, len)| !described_as(view, at, len, kind)) {
            return Err(problem);
        }
    }
    Ok(())
}

/// True if `len` bytes from `at` lie inside a single region of kind `kind`.
pub(super) fn described_as(view: &BootView<'_>, at: u64, len: u64, kind: MemKind) -> bool {
    view.region_of(at).is_some_and(|region| {
        region.kind == kind && at.checked_add(len).is_some_and(|end| end <= region.end())
    })
}

/// Prove the direct map really does alias physical memory.
///
/// Everything from stage 2 onwards reads physical memory through it — page
/// tables, page-cache pages, `DMA` buffers — so if the loader mapped it at the
/// wrong offset, the first symptom would be a page table full of plausible
/// nonsense.
///
/// The test is to read the kernel's own first bytes twice: once through the
/// image mapping, once through the direct map at the physical address the
/// loader reported. They are the same bytes, so they must agree.
pub(super) fn check_direct_map(
    view: &BootView<'_>,
    memory: &EarlyMemory,
) -> Result<(), &'static str> {
    let info = view.raw();

    for offset in [0u64, 1, 2, 3, 64, 4095] {
        // SAFETY: (BOOT-DATA) `kernel_virt` is where the loader mapped the kernel image and
        // `offset` is inside its first page, which is `.text` and always
        // present.
        let through_image =
            unsafe { core::ptr::read_volatile((info.kernel_virt + offset) as *const u8) };
        let through_physmap = memory.read_physical_byte(info.kernel_phys + offset);

        if through_image != through_physmap {
            return Err("the direct map does not alias the kernel image");
        }
    }
    Ok(())
}

/// No early device window whose end wraps the address space: the top page,
/// whose end is one past the last address, and a length that rounds past it.
/// Refused before the image test, which clamps a wrapped end, and before the
/// rounding, which would stop the kernel on the overflow; and nothing mapped.
pub(super) fn check_no_early_window_wraps(memory: &mut EarlyMemory) -> Result<(), &'static str> {
    let top_page = !(PAGE_SIZE - 1);
    for (phys, bytes) in [(top_page, PAGE_SIZE), (PAGE_SIZE, u64::MAX)] {
        match memory.map_device(vmap::DEMAND_WINDOW, phys, bytes) {
            Err(early::EarlyError::MapFailed(MapError::RangeOverflow)) => {}
            Err(_) => return Err("an early device window that wraps failed for the wrong reason"),
            Ok(()) => return Err("an early device window that wraps was mapped"),
        }
    }
    if memory.translate(vmap::DEMAND_WINDOW).is_some() {
        return Err("a refused early device window that wraps left a mapping behind");
    }
    Ok(())
}
