# Stage 13: where Ferrix differs from Linux

Moved out of [`docs/BACKLOG.md`](../BACKLOG.md) on 2026-10-16: a register, not a queue of work. The rows come first; the detail of the longer ones, under their ids, after them.

## The rows

Each row is a difference from Linux that a stage 13 landing knowingly left, with
what closing it takes. Rows are added by the landing that leaves the
difference, and removed by the one that closes it. A difference that is
stricter than Linux and deliberate says so; it stays a row because a program
may one day need the relaxation.

| Item | Owner | Stage |
|---|---|---|
| **Set-id bits are ignored in any child user namespace** (U7); Linux honours an owner mapped in the caller's namespace, so `sudo`/`su` run as their caller in a container. Deliberate (customer, consultant). Relax via `bprm_fill_uid` with `kuid_has_mapping`, as its own landing [more](#s13-001) | open, relax on demand | 13 |
| **A child user namespace honours only five capabilities**; `CAP_DAC_OVERRIDE`, `CAP_FOWNER`, `CAP_CHOWN`, `CAP_KILL`, `CAP_MKNOD` do nothing there (U8), so `unshare -r` then `chown`, rootless podman and buildah fail. Deliberate. Relax one at a time via `capable_wrt_inode_uidgid`, never `CAP_MKNOD` [more](#s13-002) | open, relax on demand | 13 |
| `CLONE_NEWNS` by a holder of `CAP_SYS_ADMIN` in a child user namespace is allowed: it copies, and every mount, unmount and remount beneath still needs the mount namespace owner's capability (N5). Linux is the same; the row is here so a later change to `Namespace::copy_as` does not assume otherwise | open | 13 |
| `verify_root_map` (Linux's `CAP_SETFCAP` to map root) is not built: Ferrix has no file capabilities. If it ever has, `userns::write_map` needs the check | open, with file capabilities | 13 |
| The audit record never carries a uid: `Subject::of` answers `NO_UID`. `userns_check` holds the subject to the kernel's id or none, so the day a personality supplies one it fails on the inside id. The supplier and its record-level check are not built | open | 13, 15 |
| **No boot check runs a real set-id `execve`, or a process sharing its fs context or with several threads**; three rules (set-uid under a namespace, `unshare(CLONE_NEWUSER)` with shared fs context, multithread `EINVAL` of `unshare`/`setns`) stand on code. A `test-container`-family gate would close them [more](#s13-003) | open | 13 |
| `max_mnt_namespaces` is not a sysctl and mount namespaces are not counted; F-37's fill bounds them by the job's memory, and `mount-max` bounds one namespace's mounts. Linux has a per-user count. `max_user_namespaces` is a global 4096 in N5, not per user | open | 13 |
| N5's plain `MS_REMOUNT` rule approximates Linux's superblock owner by "every mount is in the caller's namespace" (`Namespace::sole_filesystem`), so a child can remount read-only a tmpfs it made and bound into a shared namespace. Close by recording the creating user namespace on `Superblock` [more](#s13-004) | open | 13 |
| N5's detach of another namespace's mounts on a removed name (`detach_elsewhere`) takes each other table's lock but not that namespace's change lock, so a mount made in that namespace at that instant can race it. Linux takes `namespace_sem` for all. Close by giving `Tree` a handle to its namespace's change lock | open | 13 |
| Mount locks (M3, M4) are set by `Namespace::copy_as` alone. `MS_MOVE`, `open_tree`, `fsopen` and `mount_setattr` are not built (`EINVAL`/`ENOSYS`); whichever is built first must honour the locks and set them where a mount crosses into a less privileged namespace | open, with the new mount API | 13 |
| **M5 (a bind's source must be in the caller's namespace) has no boot check**: `Namespace::owns` gives it, and a descriptor kept across an `unshare` is the test to build, with its control | open | 13 |
| A device node on a user `tmpfs` (`nodev` forced) has no check that the open is refused | open | 13 |
| `/proc`'s private links (`root`, `cwd`, `exe`, `fd`, `fdinfo`, `maps`, `ns/*`) ask `ptrace_may_access`. `mountinfo` and `mounts` do not, as on Linux (the design's M8 listed `mountinfo`; the landing follows Linux, the certification consultant's review, 2026-10-01). `environ`, `stat` and `io` are not guarded. `/proc/<pid>/mem` does not exist | open | 13 |
| `CAP_SYS_PTRACE` is not among the capabilities a child user namespace honours (`userns::HONOURED`), so root inside a namespace cannot read the `/proc` private entries of a process its namespace owns that has another uid or is not dumpable, where Linux lets it (NP, 2026-10-01; stricter than Linux) | open | 13 |
| `/proc/<pid>/stat` gives another process's `start_stack`, `start_brk` and the other address fields that Linux zeroes for a reader without `PTRACE_MODE_READ` (NP's review, 2026-10-01): an address-layout leak against a set-id target's ASLR | open | 13 |
| `/proc/<pid>/fdinfo/<n>`'s `flags:` carries the access mode alone; Linux adds `O_CLOEXEC`, `O_APPEND`, `O_NONBLOCK` and the other file status flags (NP's review, 2026-10-01) | open | 13 |
| `userns::acting` answers a context with no current task (a `sched::current_id()` of `None` matches a setting made with `None`) the process a check set from another such context, e.g. a second processor's boot thread; today only the boot checks set it, one at a time (NP's review, 2026-10-01) | open | 13 |
| A set-id `execve` clearing dumpable stands on code (`exec.rs`); the boot harness cannot run a real set-id file. Linux also clears it when the file is not readable by the caller | open | 13 |
| **Seccomp** (`docs/SECCOMP.md`) differences from Linux: `TRACE`/`USER_NOTIF` give `ENOSYS`; `NEW_LISTENER`/`WAIT_KILLABLE_RECV` `EINVAL`; `LOG` writes a rate-limited console line, not audit; `SPEC_ALLOW` is a no-op; `TRAP` is a kill until S4, `TSYNC` `EINVAL` until S5 [more](#s13-005) | open, S4 and S5 | 13 |
| Seccomp: x86-64 `uretprobe` (335) and `uprobe` (336) pass through Linux's filter unfiltered; Ferrix, with no probes, lets a filter judge them like any unknown number, which is stricter and safe. Recorded in `tests/chrome.rs` | open, deliberate | 13 |
| Seccomp SR2: a syscall number with bits above the 32nd set is judged by a filter as its low half and dispatched as `ENOSYS`, where Linux masks to the low half. Stricter, deliberate. Relax by masking in each entry [more](#s13-006) | open, deliberate | 13 |
| Seccomp, S2: a call in the native range carries the `arch` token `0xC000_0F1F`, which Linux never uses, and it has the 64-bit flag on ARMv7-A too, where no native register is 64 bits wide. A filter that allows the native token names that value on every architecture | open, deliberate | 13 |
| Seccomp S2: a thread with no filter pays the `Once` load plus one load of the boot check's probe word, more than `docs/SECCOMP.md` §3.3 prices. The probe is a test seam; remove by moving the check to filter programs and a ring-3 reader [more](#s13-007) | open, deliberate | 13 |
| Seccomp keeps the filter per thread with `TSYNC`; `KILL_THREAD` ends the process by `SIGSYS` only for the last live thread. No boot check has several started threads, so that case stands on code until S6's guest program [more](#s13-008) | open, S6 | 13 |
| Seccomp, S3: `execve` keeps a thread's filter by never touching the thread's state, and no boot check runs an `execve` (the check processes are never started); S6's guest program runs bubblewrap's path, `prctl(PR_SET_SECCOMP, 2, &prog)` then `execve` of a program that finds itself filtered | open, S6 | 13 |
| Seccomp, S3: releasing the longest chain a program can build (6,554 filters) takes 8.1 ms in the guest, where the last reference goes: in production the reaper, with preemption off. Close by handing a long chain to a deferred work item of the reaper's own, a few filters at a time | open | 13 |
| Seccomp, S3: the `LOG` rate limit is one for the machine, not one for each process, so one process in a loop under a logging filter can use up the lines another's kill would write; Linux's audit log has its own ratelimit | open | 13 |
| No boot check counts the personality's live threads and processes, so a check that keeps one alive goes unseen (five filtered threads leaked in `seccomp_filters_check` until 2026-10-07). Close with a live `Thread`/`Process` count each stage's checks require back, or argue per check [more](#s13-010) | open | 13 |
| Seccomp S3: a filtered thread's calls look up the running thread (`sched::current`, run queue lock) while any thread holds a filter; Linux reads `current`. A per-processor cache of the running thread would remove it [more](#s13-011) | open | 13 |
| **UTS, IPC and cgroup namespaces belong to the process, not the thread** (`syscall/nsproxy.rs`). `clone(CLONE_THREAD)` with one of the three flags, and `unshare` or `setns` into one from a multithreaded process, are `EINVAL`; Linux gives a thread its own. Close by moving the proxy onto `Thread` | open | 13 |
| `setns` into a mount or user namespace is `EINVAL` while the fs context is shared (`CLONE_FS`), where Linux copies the struct. A `Process` holds its context for life; closing it means letting a process swap its context | open | 13 |
| Opening a `/proc/<pid>/ns/*` link asks the same user, or root in the first namespace, and not dumpability (Linux asks `ptrace_may_access`). Fold into NP's `credentials::may_access` | open | 13 |
| **Kernel root keeps its file override in a user namespace it made**: `Access::privileged` is `uid == 0` with no namespace, so root of the first namespace reads a 0600 file of an unmapped id, where Linux refuses. Close by making `Access` carry its namespace's map [more](#s13-012) | open | 13 |
| **A pidfd of an exited process may still be joined by `setns`** where Linux answers `ESRCH`; the namespaces it names are held by the pidfd's process object. Close by refusing a pidfd whose process has ended | open | 13 |
| nsfs: `NS_GET_MNTNS_ID`, `NS_GET_ID` and the `NS_MNT_GET_*` ioctls answer `ENOTTY`; there is no per-namespace `/proc/sys/kernel/sem` and no `/proc/sysvipc/sem` at all | open | 13 |
| The hostname sysctl is judged by the file's mode only, where Linux also asks `CAP_SYS_ADMIN` over the UTS namespace's owner at the write; the syscall does | open | 13 |
| Two small-namespace boot controls show an older check's message, one control's message only roughly matches its rule, the namespace files' `kmem` fill has no control, and the pidfd `setns` all-or-none property stands on code shape [more](#s13-013) | open | 13 |
| The composite `CLONE_NEWUSER\|CLONE_NEWIPC\|CLONE_NEWCGROUP` has no end-to-end check; `test-container` (`src/tests/container`) is the place. Since 2026-10-14 it runs (`docs/CGROUPS.md` §15) and proves each kind made by uid 1000 under its own user namespace, but in two calls, `CLONE_NEWUSER` first and the other six after the maps are written, so the one-call composite is still unchecked | open | 13 |
| **A removed file's heap comes back late:** with cgroup controllers on `main`, `memory.current` still read 256 bytes just after `rm -r` and 0 within a second. Nothing leaked; `test-vfs`'s F-37 check waits up to five seconds. What holds the last reference is not found [more](#s13-014) | open | 13 |
| **`io.max` under the btrfs mount-wide lock:** `btrfs-vfs` `Shared::with` holds the volume's `SleepLock` across I/O that now sleeps in `blkio::account`, so a job at a small `io.max` on a shared writable volume stalls every job on it, the item's file faults included; release the lock before the wait, or throttle before taking it | open | 13 |
| **Cgroup controllers' costs without a bound:** `charge_cpu`/`runtime_left` walk every ancestor per tick under the run-queue lock, `oom::throttle` scans processes and file objects, `settle_frozen` is O(processes x depth). A depth cap and cheaper `memory.events` wake would bound them [more](#s13-015) | open | 13 |
| **Native processes and `cgroup.freeze`:** a freeze parks Linux processes only, `cgroup.events` can say `frozen 1` while a native task runs; either park native tasks too or refuse `job_for_cgroup` putting one in a freezable cgroup | open | 13 |
| **The page-cache list's dead entries:** a file's weak entry in `user/cache.rs` keeps its allocation, uncharged, until the list next grows and prunes; remove the entry in the object's drop (a map keyed by address) or charge it | open | 13 |
| **Dentries are not reclaimable:** a cached dentry stays charged to the job whose walk made it until the namespace's queue evicts it, and `memory.max` reclaim does not look at it (a create refused `ENOMEM` now drops its own, which showed this) | open | 13 |
| The namespaces' numbers: user from 0xF800_0000, UTS, IPC and cgroup from 0xF900_0000, mount from 0xF000_0000, where Linux draws from one counter. Nothing compares across kinds; a program that does sees them apart | open | 13 |
| **A network namespace belongs to a process, not a thread**: `CLONE_NEWNET` with `CLONE_THREAD`, and `unshare` from a multithreaded process, are `EINVAL` (the latter by one line, not boot-checked). Close with the other namespaces' move onto `Thread` | open | 13 |
| A moved network interface gets the next free index, and a clashing name is `EEXIST`; Linux keeps the index when free and renames to `dev%d`. Coming home to the first namespace renames to `devN` on a clash | open | 13 |
| Network namespaces: reassembly is capped at 32 KiB outside the first namespace; the ceilings (64 interfaces, 256 addresses, 1024 routes) also bind the first one; only `veth` exists (`dummy`, `bridge`, `macvlan`: `EOPNOTSUPP`) with a fixed MTU of 1500 and no notifications; `RTM_NEWNSID`, `RTM_GETNSID`, `IFLA_LINK_NETNSID` and `IFLA_TARGET_NETNSID` are not built; there is no `/proc/<pid>/net` | open | 13 |
| Nothing forwards or translates between two interfaces of one network namespace: the stack is a host, so a bridge or NAT between containers needs IP forwarding, which it has none of | open | 13 |
| **No limit on the number of network namespaces**: the net task ticks every one, so any user can slow the tick without bound. Add `max_net_namespaces` like N5's other sysctls, with a boot check and control (consultant's F3) [more](#s13-016) | open | 13 |
| The neighbour cache (about 1.1 MB per namespace) and reassembler grow on traffic but are charged to the job only at the namespace's next change (`fit`), against NETNS §5. Close by charging on fill or per tick (consultant's C2) [more](#s13-017) | open | 13 |
| `setns(CLONE_NEWNET)` is not wired: `net/netns_file.rs` is the seam, to become the network arm of the namespace files in `fs/nsfs.rs` | open | 13 |
| Not boot-checked for network namespaces: the `/sys/class/net` listing per reader, IPv6 over a veth pair, ARP growth past the table limit, `unshare` from several threads | open | 13 |
| Raw and packet sockets are reachable by a user who owns a network namespace (`CAP_NET_RAW` over an owned namespace is honoured): safe Rust with no ring-buffer interface, recorded as a residual in the vulnerability analysis | open, accepted | 13 |
| **Pid namespaces** (`docs/PIDNS.md` §8) differ from Linux: one machine-wide pool of 32768 numbers, `pid_max` not writable or per namespace; init's namespace killed when its last thread ends; only `SIGKILL`/`SIGSTOP` forced to a protected init; procfs numbers per reader; no `setns` into a pid namespace, no `clone3` `set_tid` [more](#s13-018) | open | 13 |
| **Pid namespaces, consultant's review (2026-10-01):** `si_pid`/`SO_PEERCRED` translated on read, not stamped on send; `process_give` takes machine pids and leaks existence via `NO_PROCESS`/`NOT_CHILD`; `PidNamespace` has no owning user namespace [more](#s13-019) | open | 13 |
| **A frame budget for the fork path:** nothing stops `sys_clone`, `Process::with_context`, `shared_with_context`, `forked_into`, `fork_into` growing on the four-page kernel stack. `xtask check` could read each `sub rsp` with objdump and fail past a budget (`docs/PIDNS.md` §10) [more](#s13-020) | open | 13 |
| Time namespaces and the cgroup controllers (M2's reclaim, `cgroup.freeze`, `cpu.max`, `io`): their own differences are added here when those landings report | open | 13 |

## Detail

### s13-001

| Item | Owner | Stage |
|---|---|---|
| **Set-id bits are ignored in any child user namespace** (U7), where Linux honours an owner mapped in the caller's namespace. `sudo`, `su`, `newgrp` and a set-group-id `ssh-agent` run inside a container as their caller. Deliberate (customer, 2026-09-28; consultant). Relax by `bprm_fill_uid` with `kuid_has_mapping`, on a mount without `nosuid` and without `PR_SET_NO_NEW_PRIVS`, as its own landing with a boot check (`docs/NAMESPACES.md` §4) | open, relax on demand | 13 |

### s13-002

| Item | Owner | Stage |
|---|---|---|
| **A child user namespace honours five capabilities** (`CAP_SETUID`, `CAP_SETGID`, `CAP_SETPCAP`, `CAP_SYS_CHROOT`, `CAP_SYS_ADMIN`), where Linux honours the rest over files whose owner is mapped: `CAP_DAC_OVERRIDE`, `CAP_FOWNER`, `CAP_CHOWN`, `CAP_KILL`, `CAP_MKNOD` do nothing there (U8). `unshare -r` then `chown`, rootless podman and buildah do not work. Deliberate. Relax one capability at a time by `capable_wrt_inode_uidgid` (both uid and gid mapped), never `CAP_MKNOD` | open, relax on demand | 13 |

### s13-003

| Item | Owner | Stage |
|---|---|---|
| **A boot check cannot run a real set-id `execve`, or a process that shares its fs context or has several threads**, so three rules stand on code and on `Credentials`/`namespaces_asked` checks: a set-uid file under a namespace (`Credentials::exec`), `unshare(CLONE_NEWUSER)` with a shared fs context, and the multithread `EINVAL` of `unshare` and `setns`. A gate that runs a program for each (`test-container`'s family) closes all three. `test-container` (2026-10-14, `docs/CGROUPS.md` §15) proves none of them: its owner execs nothing, has a fs context of its own and one thread | open | 13 |

### s13-004

| Item | Owner | Stage |
|---|---|---|
| N5's plain `MS_REMOUNT` rule approximates Linux's superblock owner by "every mount of the filesystem is in the caller's namespace" (`Namespace::sole_filesystem`). Linux asks `CAP_SYS_ADMIN` over the user namespace that made the superblock, so a child can still remount read-only a tmpfs it made and then bound into a namespace it shares. Close by recording the creating user namespace on `Superblock` | open | 13 |

### s13-005

| Item | Owner | Stage |
|---|---|---|
| **Seccomp** (`docs/SECCOMP.md`): `TRACE` and `USER_NOTIF` answer `ENOSYS` (no tracer, no supervisor); `SECCOMP_FILTER_FLAG_NEW_LISTENER` and `WAIT_KILLABLE_RECV` are `EINVAL`; `SECCOMP_FILTER_FLAG_LOG` and the `LOG` action write one rate-limited line (pid, `arch`, number, instruction) on the console and not Linux's audit log (Q4: `audit.rs` records the item's decisions, a filter's is the personality's); `SPEC_ALLOW` is accepted and changes nothing; `TSYNC` is `EINVAL` until S5. A `TRAP` handler's own frame (its write into the context, `SA_NODEFER`) is checked by S6's guest program, not at boot. User notification would need an fd type and a supervisor protocol | open, S4 and S5 | 13 |
| Seccomp, S4: a `SIGSYS` from a `TRAP` read through `signalfd` carries `ssi_errno` but not `ssi_call_addr`, `ssi_syscall` or `ssi_arch`, which Linux fills (`encode_signalfd`'s `Origin::Sys` arm); a handler's `siginfo` has them. Fill the three fields and check them with the guest test (the consultant's S4 note, 2026-10-01) | open | 13 |

### s13-006

| Item | Owner | Stage |
|---|---|---|
| Seccomp, SR2: a call whose number register has bits above the 32nd set is judged by a filter as its low half and dispatched as no call (`ENOSYS`), where Linux masks the number to its low half and runs that call. Stricter and deliberate: a number is judged as the value dispatched or not at all. Relax by masking in each entry, which makes the filter and the dispatcher agree on the low half | open, deliberate | 13 |

### s13-007

| Item | Owner | Stage |
|---|---|---|
| Seccomp, S2: a thread with no filter pays the `Once` load and one load of the boot check's probe word, not only the `Once` load `docs/SECCOMP.md` §3.3 prices. The probe is a test seam that shows what a filter was shown, which a filter program cannot say; remove it by moving the check to filter programs and a ring-3 reader | open, deliberate | 13 |

### s13-008

| Item | Owner | Stage |
|---|---|---|
| Seccomp's design keeps the filter per thread with `TSYNC` as Linux has it (`docs/SECCOMP.md` §3.4); `KILL_THREAD` ends the process, by `SIGSYS`, when the thread is the process's last live one, and only the thread otherwise, as Linux does; no boot check has a process of several started threads, so the several-thread case stands on the code until S6's guest program kills one thread among several | open, S6 | 13 |

### s13-009

Done; removed from the backlog on 2026-10-16.

| Item | Owner | Stage |
|---|---|---|
| Seccomp, S3: the boot's own checks set the machine-wide "a thread has held a filter" flag, so on a booted machine with checks the no-filter fast path was the flag's load and the thread look, not the flag alone. **Closed 2026-10-07 (po10-obj, cut 3, `docs/OPAQUE-KERNEL.md` §9.11):** the flag is a live count of filtered threads (`seccomp::FILTERED_THREADS`), which the boot's filters check requires back where it was once its threads have gone | closed |

### s13-010

| Item | Owner | Stage |
|---|---|---|
| No boot check counts the personality's live threads and processes, so a check that keeps one alive is seen by nothing: five filtered threads and their processes, held by frames of `seccomp_filters_check`'s scenarios below the call their filter ended, stayed for every boot's life until 2026-10-07 (cut 3), and were found only because the count of filtered threads did not come back. The product's kill path drops its references before it leaves. Close with a count of live `Thread`s and `Process`es that the end of each stage's checks requires back at its value from the start, or argue per check that none is kept | open | 13 |

### s13-011

| Item | Owner | Stage |
|---|---|---|
| Seccomp, S3: a filtered thread's calls look for the running thread (`sched::current`, which takes the run queue's lock) while any thread in the machine holds a filter; Linux reads `current`. The count of filtered threads (cut 3) keeps this off every machine where none does; a per-processor cache of the running thread would take it off the rest | open | 13 |

### s13-012

| Item | Owner | Stage |
|---|---|---|
| **Kernel root keeps its file override in a user namespace it made**: `Access::privileged` is `uid == 0` with no namespace in it, so root of the first namespace reads a 0600 file of an id the namespace does not map, where Linux (`capable_wrt_inode_uidgid`) refuses it. U8 holds for a process whose kernel uid is not 0; `userns_check.rs`'s `kernel_root_keeps_override` records it. Close by making `Access` carry its namespace's map | open | 13 |

### s13-013

| Item | Owner | Stage |
|---|---|---|
| Two boot controls of the small namespaces show an older check's message, not their own (`nameable` letting everyone in fires the `userns` line first), one control's message only roughly matches its rule, and the namespace files' `kmem` fill has no control (the detached mount's charge bounds it). The pidfd `setns` all-or-none property stands on the code's shape | open | 13 |

### s13-014

| Item | Owner | Stage |
|---|---|---|
| **A removed file's heap comes back late:** with the cgroup controllers on `main`, `memory.current` of the cgroup a program filled `/tmp` in to its `memory.max` still read 256 bytes (one dentry's worth) just after `rm -r` and 0 within a second; before them it read 0 at once. Nothing was leaked, and `test-vfs`'s F-37 shell check now waits up to five seconds for 0; what holds the last reference to that object for a moment (a task, a process not yet reaped, a deferred drop) is not found | open | 13 |

### s13-015

| Item | Owner | Stage |
|---|---|---|
| **Cgroup controllers' costs without a bound:** `charge_cpu` and `runtime_left` walk every ancestor per tick under the run-queue lock (no `cgroup.max.depth`), `oom::throttle` lists live processes and scans every file object when a job is over `memory.high`, `settle_frozen` is O(processes x depth) per park (`MEMORY-AND-TIMING.md` §2.2c); a depth cap and a cheaper wake of `memory.events` would bound them | open | 13 |

### s13-016

| Item | Owner | Stage |
|---|---|---|
| **No limit on the number of network namespaces**: each is charged to its job, and the net task ticks every one that exists, so a user who may make namespaces (any user, through a user namespace) makes the tick slower without bound but their job's memory. Add `max_net_namespaces` like N5's other sysctls (global or per user), with a boot check and control (consultant's F3) | open | 13 |

### s13-017

| Item | Owner | Stage |
|---|---|---|
| The neighbour cache (256 entries x 3 queued packets, about 1.1 MB per namespace) and the reassembler (32 KiB) grow on traffic and are charged to the job only at the namespace's next change (`fit`), against NETNS section 5's "charged as it fills". Close by charging on fill or per tick; until then the per-namespace bound is the limit (consultant's C2) | open | 13 |

### s13-018

| Item | Owner | Stage |
|---|---|---|
| **Pid namespaces** (`docs/PIDNS.md` §8): one machine-wide pool of 32768 kernel numbers with each namespace also numbering to 32768 locally, `pid_max` neither writable nor per namespace; an init's namespace is killed when its last thread ends, not after it reaped its children; only `SIGKILL` and `SIGSTOP` from an ancestor are forced through to a protected init, and faults bypass the protection; procfs file contents tell numbers for the reading process, not the instance's namespace; `setns` into a pid namespace and `clone3`'s `set_tid` are not built | open | 13 |

### s13-019

| Item | Owner | Stage |
|---|---|---|
| **Pid namespaces, from the consultant's review (2026-10-01):** `si_pid` and `SO_PEERCRED` are translated when read (`pidns::show_pid`), not stamped when sent, so a sender that has ended reads 0 and a reused kernel number reads as its new owner; `process_give` takes machine pids, and its `NO_PROCESS` against `NOT_CHILD` tells a native caller in a namespace whether a machine pid exists; `PidNamespace` has no owning user namespace (harmless until `setns` into a pid namespace is built) | open | 13 |

### s13-020

| Item | Owner | Stage |
|---|---|---|
| **A frame budget for the fork path:** nothing stops `sys_clone` (3,712 bytes), `Process::with_context` (1,592), `shared_with_context` (3,728, transient), `forked_into` (600) and `fork_into` (80) growing again on a four-page kernel stack; `xtask check` could read the `sub rsp` of each from the built kernel with objdump, as the assembly budget counts lines, and fail past a budget per function and per path (a double fault in `test-vfs` on x86-64 was this, 2026-10-01; `docs/PIDNS.md` §10) | open | 13 |
