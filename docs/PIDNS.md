# PID namespaces

Stage 13's pid namespaces, on top of the user namespaces of
`docs/NAMESPACES.md` (N4). The roadmap's exit criterion -- "an unprivileged
user namespace runs a process whose pid is 1 inside it, under a memory limit
... with a seccomp filter" -- needs the pid-1 part from here; the memory
controller and seccomp are other streams'.

Status: **built on `stage13-pidns`, on `main` since 2026-10-01**
(a2af5061a; §9). Linux's semantics throughout; §8
lists every place this differs.

---

## 1. What a pid namespace is

A process has a pid in every pid namespace from its own up to the first. The
first process made in a namespace is pid 1 there and is that namespace's
*init*. A process sees only the processes in its own namespace and the ones
below it, each under the number its own namespace gives it; a process above
or beside it is not there: `ESRCH` for a call that names it, 0 where a pid
is reported.

Made by `clone` or `clone3` with `CLONE_NEWPID` (the *child* is pid 1 of the
new namespace) and by `unshare(CLONE_NEWPID)` (the caller does not move; its
*later children* are in the new namespace). A process has therefore two
namespaces: the one it is in (`pids.ns`, fixed for life) and the one its
children are made in (`pid_for_children`, equal to the first until an
`unshare`).

## 2. The data model

### 2.1 The kernel number stays the key

`object::process`'s table is unchanged: a number chosen by `allocate` is
still how `Process::pid()`, the registry, the job tree, `pgid`, `sid`, the
terminal's session and foreground group, the audit subject and every native
ABI object name a task. Call it the **kernel number** `K`. It is unique in
the whole machine, so a process group or a session names the same thing in
every namespace, and nothing that compares two of them needs to know about
namespaces. `K` is also the number in the first namespace: a program there
sees exactly what it saw before this work, which is why the first namespace
costs nothing (no record, no lookup, `K` itself).

A pid namespace does not have its own number space of `K`s; it has its own
space of *local numbers* that map to a `K`.

### 2.2 `PidNamespace`, `Numbers` (new file `syscall/pidns.rs`)

```
PidNamespace
  parent: Option<Arc<PidNamespace>>    None for the first
  level: u32                           0 for the first, at most 32
  id: u64                              what /proc/<pid>/ns/pid names
  local: SpinLock<Local>               map local number -> K, cursor
  init: SpinLock<Weak<Process>>        pid 1 of this namespace
  dying: AtomicBool                    init has gone; no new pids
  _charge: Option<Charge>              F-37

Numbers                                one per task id (process or thread)
  ns: Arc<PidNamespace>                the innermost namespace
  nr: [u32; 33]                        nr[0] = K; nr[i] = local number in the
                                       level-i namespace of the chain
  _charge: Option<Charge>
```

`Numbers` is Linux's `struct pid`: a number per level. A process in the first
namespace has **no** `Numbers` (`None`), and everything below treats `None`
as "`K`, visible at level 0 only". Nothing is allocated for a process that
never meets a namespace.

* `Numbers::in_ns(ns) -> u32`: `nr[ns.level]` if `ns` is on this task's
  chain (`ns.level <= self.ns.level` and walking parents from `self.ns` to
  that level reaches `ns`); else 0. The chain is at most 33 deep, so this is
  a short walk. For the first namespace the answer is `nr[0] = K`, always:
  every task is visible in the first namespace.
* `find_in(ns, nr) -> Option<K>`: the local map; for the first namespace `nr`
  is `K` and the registry decides.
* Local numbers are allocated cyclically per namespace, first number 1, then
  2, 3 ..., wrapping to 300 (`RESERVED`) past `PID_MAX` (32,768), as Linux's
  `alloc_pid` does. **`PID_MAX` bounds the whole machine's `K`s and each
  namespace's local numbers separately** (§8).

The one table of pids, `Numbers` held by a `Process` (`pids`) and by a
`Thread` (a sibling thread's own, its first thread uses its process's), and
two more `Arc<Numbers>` a process keeps, `pgrp_of` and `session_of`, which
are how a process group or session outlives its leader's reaping (Linux: a
`struct pid` lives as long as a task, a group or a session uses it). They
exist for namespaced processes only.

### 2.3 Where a number comes from

`Process::forked_into` and `registry::allocate_thread` are the two makers of
a task id. Both call `object::process::allocate` for `K` as today, then, if
the task is in a namespace below the first, `pidns::assign(ns, K)`, which
takes each level's namespace lock **alone, one after the other**, inserts the
local number, and on any failure (`ENOMEM`, `ENOSPC`) removes what it
inserted and gives `K` back. The `Numbers` drops -- a thread ending, a
process reaped and unreferenced by any group or session -- take the same
locks alone, in the same way, and remove exactly their own entries.

A fork child is in its parent's `pid_for_children`. With `CLONE_NEWPID` a
namespace is made first (`pidns::create`) and the child is its first member.
A child of a namespace whose init is gone (`dying`) is `ENOMEM`, Linux's
answer (`alloc_pid` fails once `PIDNS_ADDING` is cleared).

### 2.4 Lock order

The locks here are leaves: `TABLE` (object/process.rs) is never held with a
namespace's `local` lock; a namespace's `local` locks are never held two at
once; `init` is taken alone. `pidns::assign` allocates `K`, releases the
table, then takes the levels one at a time; a lookup reads a map under
`local`, releases it, then asks the table. A process's `membership` lock
(the job) is not held across any of it. So the order needed is none, and
there is nothing to add to `docs/NAMESPACES.md` §6 but "pid namespace
locks: leaves".

## 3. Translation

One rule: the kernel speaks `K`; a call speaks the caller's namespace.
`pidns::to_user(viewer, K) -> u32` turns a kernel number into the viewer's
(0 if not visible; `K` itself for a viewer in the first namespace, without a
lookup); `pidns::from_user(viewer, nr) -> Option<K>` the other way.
`Process::pid_in(viewer)`, `Thread::tid_in(viewer)` and
`process.pgid_in(viewer)`, `sid_in`, `parent_pid_in` are the typed forms;
`registry::find_in(viewer, nr)` replaces `registry::find(pid)` wherever a
program's number is looked up. A `viewer` is the calling `Process`; where a
reader is not the subject of the call (`/proc`, `siginfo` read by a
handler), the viewer is the reading process, `userns::acting()` -- the same
"the caller" the user-namespace work introduced for ids, including the boot
check's `acting_as`.

The zero cases, as Linux: a process whose parent is not visible has
`getppid() == 0` (pid 1 of a namespace, always); `getpgid`/`getsid` of a
group or session led outside the viewer's namespace is 0 (pid 1 of a
namespace made by `clone(CLONE_NEWPID)` inherits its parent's group and
session until it makes its own); `si_pid` and `ssi_pid` of a signal from
outside the namespace is 0.

## 4. Every site that names a pid

Found by grep of `.pid()`, `parent_pid`, `.pgid()`, `.sid()`, `tid`,
`registry::find`/`live`, `subject(`, `Origin::` and the dispatch table, not
by memory. *In* is a number the program passes; *out* one it reads.

| Site | In | Out | Change |
|---|---|---|---|
| `getpid`, `gettid`, `set_tid_address` (linux.rs) | | yes | `pid_in(caller)`, `tid_in(caller)` |
| `getppid` | | yes | `parent_pid_in(caller)`; 0 for a namespace's init |
| `clone`/`clone3`/`fork`/`vfork` return | | yes | the child's number in the parent's namespace |
| `CLONE_PARENT_SETTID` / `CLONE_CHILD_SETTID` (family.rs) | | yes | the parent's view to the parent's word, the child's own to the child's |
| `wait4`, `waitid` (`P_PID`, `P_PGID`, `P_PIDFD`), `si_pid` in `waitid`'s result | yes | yes | `wait4_selector` and `waitid` translate the named pid/group in, the reaped child's number out |
| `setpgid`, `getpgid`, `getpgrp`, `getsid`, `setsid` | yes | yes | in and out; a group led outside is 0; `setpgid` into a group finds it by `pgid` |
| `kill`, `tkill`, `tgkill`, `pidfd_send_signal`, `pidfd_open` | yes | | `find_in`; `kill(-1)` covers the caller's namespace minus its init and itself; `kill(0)` and `kill(-pgrp)` likewise |
| `Origin::{User,Thread,Child}` `pid` -> `si_pid` (`encode`, `encode_signalfd`) | | yes | the origin keeps `K`; encoding turns it into the reader's |
| signal delivery to a namespace's init | | | §5 |
| `attributes::subject` (`prlimit`, `sched_*`, `capget`/`capset`, `getpriority`, `ioprio`, `getrlimit` family) | yes | | `find_in` |
| `getpriority`/`setpriority` `PRIO_PGRP` | yes | | group in |
| `get_robust_list` | yes | | thread in |
| `flock`/`fcntl` `F_GETLK` `l_pid`, `F_OFD_*` | | yes | out, for the reader |
| System V semaphores `sempid` / `GETPID` | | yes | kept as `K`, told as the reader's |
| `SO_PEERCRED`, `SCM_CREDENTIALS` | yes (send) | yes | stored as `K`; validated in; told out as the reader's, 0 if not visible |
| `TIOCGPGRP`, `TIOCSPGRP`, `TIOCGSID`, `tcgetpgrp`, `TIOCSCTTY` (tty.rs) | yes | yes | the terminal keeps `K`s; in and out |
| `cgroup.procs` read and write (cgroupfs.rs) | yes | yes | the reader's numbers; a pid not visible is not listed, and not movable |
| `/proc` (procfs.rs, procfs/render.rs) | yes | yes | §6 |
| `/proc/<pid>/ns/pid`, `pid_for_children` | | yes | §6 |
| `audit.rs` subject pid | | | stays `K`: the log is the machine's |
| `launch.rs`, `native.rs`, `devmgr.rs`, `root_disk.rs`, `fsctl.rs` | | | kernel-internal or native-ABI: stay `K` |
| `mount -t proc` (fsctl.rs) | | | the new instance remembers the mounter's namespace |
| `unshare`, `setns` (namespace.rs) | | | `CLONE_NEWPID` accepted; `setns` still `EINVAL` |

A native process (`process_create`) made by a process in a namespace is
in it: `launch::load_native` numbers it in its creator's children's namespace
before it is findable (`Process::enter_pid_namespace`), because a native
process can make Linux calls and one left in the first namespace would name
the machine's processes by kernel number from inside a container and be out
of its init's reach. A native process the kernel starts (no creator) is in
the first. `process_give` still names its child by kernel pid (§8).

## 5. Init

*Reparenting.* A process that ends hands its children to, in Linux's order:
the nearest ancestor *in its own namespace* that set
`PR_SET_CHILD_SUBREAPER`, else **its own namespace's init**
(`reaper_for_orphans`), not pid 1 of the machine. The first namespace keeps
today's rule (`registry::find(INIT_PID)`).

*Init's death.* When the last thread of a namespace's init ends and the
process is released (`Process::release`), before its orphans go on:

1. the namespace is marked `dying`, so no process can be added;
2. every other process in it, and in namespaces below it, gets `SIGKILL`
   (Linux's `zap_pid_ns_processes`), by the same path a job kill takes, not
   through `kill`'s checks;
3. its children, having no reaper, are released as the existing "nobody to
   take them" path does; its own parent is told as for any child.

Linux keeps init alive until it has reaped every child; here init is
released at once and its children are killed and unparented. The difference
is observable only as an init that is a zombie a moment earlier (§8).

*Protection.* A namespace's init, for a namespace below the first, ignores a
signal that would take its default action, unless it comes from an ancestor
namespace -- Linux's `SIGNAL_UNKILLABLE` with `force`:

* a signal for which it has a handler is delivered, whoever sends it;
* `SIGKILL` and `SIGSTOP` are delivered only from a process in an ancestor
  namespace (the sender has no number in the init's namespace) or the
  kernel's job kill; from inside they are discarded, and `kill` still
  answers 0;
* any other signal with its default action is discarded whoever sends it,
  the kernel's own (a tty's `SIGHUP`, an alarm) included.

The check sits in `kill::send` and `send_to_thread`, where the decision to
discard is made for every origin. The first namespace's pid 1 is not
protected, as today; a job kill (`cgroup.kill`, `job_kill`) is not a signal
and reaches it as it reaches every process.

## 6. procfs

A procfs instance belongs to the pid namespace of the process that mounted
it (`Shared.pid_ns`). Its `/proc/<n>` directory names are that namespace's
numbers: `lookup`, `readdir` (sorted by local number, the cursor a local
number) and `task/` translate; a process not in the namespace is not listed
and not found (`ENOENT`). The inode numbers, `Place` and the render
functions keep `K`, so nothing about a file's identity changes.
`/proc/self` and `thread-self` are the reader's number **in the instance's
namespace**, and dangle (`ENOENT`) for a reader with none -- a procfs from
another namespace. `status` gains `NStgid`, `NSpid`, `NSpgid` and `NSsid`
(the numbers from the first namespace down to the process's own, as Linux
prints them, each truncated to the levels the *reader's* namespace can see)
and its `Pid`, `PPid`, `Tgid`, `TracerPid` are the reader's; `stat`'s pid,
ppid, pgrp, session likewise. **Deviation:** the values in a file are told
for the reader, not the instance's namespace, where Linux uses the
instance's; the two differ only for a reader looking at a procfs another
namespace mounted, and the names in the directory -- which is what the
reader navigates by -- do follow the instance.

`/proc/<pid>/ns/pid` and `ns/pid_for_children` read `pid:[N]`, N the
namespace's `id` (the first's is Linux's `0xEFFFFFFC`). `/proc/sys/kernel/
pid_max` stays `32768`.

`mount("proc", ...)` makes an instance of the mounter's pid namespace. The
call itself is as it was: `mount(2)` needs the first namespace's root, so a
process in a child *user* namespace has a `/proc` of its pid namespace only
by a bind of one, and lifting that is N5's (mount rules for child
namespaces), where Linux's `proc_init_fs_context` owner check and the "fully
visible `/proc`" rule belong (§8). `umount` and a bind of `/proc` are as
before.

## 7. F-37 and limits

Charged to the job of the task whose call makes them, refused `ENOMEM` at
the limit, given back when they go:

| Kind | Made by | Charge |
|---|---|---|
| `PidNamespace` | `clone`/`clone3`/`unshare` with `CLONE_NEWPID` | at creation |
| `Numbers` of a task in a nested namespace | every `fork`, `clone`, thread | with the task, per level |

`fs/kmem_check.rs` gets a fill of pid namespaces (made as the syscall makes
them, in a looping creator until `ENOMEM`), with the sibling-job and
returns-to-zero checks it has for user namespaces, and a negative control:
the namespace uncharged. The per-task `Numbers` are covered by the pid
check's own control (§9).

Limits: nesting depth 32 -- `CLONE_NEWPID` from a process whose
`pid_for_children` is at level 32 is `ENOSPC`; `PID_MAX` as §2.2; the
job's `pids.max` counts tasks as today, in whatever namespace.

## 8. Differences from Linux, stated

* One machine-wide `K` space of `PID_MAX` numbers: with a namespace per
  container the sum of every namespace's processes is bounded by 32,767,
  where Linux bounds each (`pid_max` is per namespace there).
* `pid_max` is not writable and not per namespace.
* Init is released when its last thread ends, and its namespace is killed
  then, not after it has reaped its children (§5).
* Only `SIGKILL`/`SIGSTOP` from an ancestor namespace are forced through to
  a protected init; Linux also forces a signal sent with `SEND_SIG_PRIV`
  from the kernel. Nothing here sends one.
* The first namespace's pid 1 is not protected from `kill` (as before).
* Files in procfs tell numbers for the reader, not the instance (§6).
* The "fully visible `/proc`" mount check is not made.
* `setns` into a pid namespace is not built (`setns` answers `EINVAL`).
* `/proc/<pid>/ns/pid` and `pid_for_children` read as `pid:[id]` but are not nsfs files: opening one is
  `EOPNOTSUPP`, since nothing could take the descriptor.
* `show_pid` tells `si_pid` and `SO_PEERCRED` by looking the sender up when
  they are read, not when they were stamped: a sender gone by then reads 0,
  and a kernel number reused since is read as the new process's (backlog).
* `process_give` takes a kernel pid, and its `NO_PROCESS` against `NOT_CHILD`
  tells a native caller in a namespace whether a machine pid exists
  (backlog).
* `PidNamespace` has no owning user namespace; harmless while `setns` into a
  pid namespace is `EINVAL`.
* `CLONE_NEWPID` with `CLONE_THREAD` or `CLONE_PARENT` is `EINVAL`, as is
  `CLONE_PARENT` from a namespace's init, and `CLONE_THREAD` from a process
  whose children are in another namespace than it is. `clone3`'s `set_tid`
  stays `ENOSYS`, as before.
* The console's and a pty's `TIOCGPGRP`, `TIOCGSID` and `TIOCSPGRP`, `F_GETLK`'s
  `l_pid` and `semctl(GETPID)` are translated by the shared helpers
  (`pgrp_to_user`, `show_pid`) and have no boot check of their own.
* A terminal's foreground group is found for a reader by scanning for a
  live member, so a group whose members are all gone reads 0 in a namespace.
* `kill(-1)` does not reach a process the caller's namespace does not show.
* A process group whose leader is gone keeps its local numbers for as long
  as a member holds them, not as Linux's `struct pid` counts (same thing,
  but `K` itself may be reused while members remain; that predates this).

## 9. Checks and landings

The `pidns` boot line (`fs/pidns_check.rs`, FX-0891), driven through the
syscall layer with check-made processes (`process::new_for_check`,
`userns::acting_as`, `Tally`), as `userns` is. One rule, one check, one
negative control:

| Rule | Check |
|---|---|
| P1 | `clone(CLONE_NEWPID)` child is pid 1 inside and has another number outside; `getpid`/`getppid` in both |
| P2 | second child is 2, parent's view differs; a sibling namespace shows the same numbers independently |
| P3 | `kill`/`wait4`/`tgkill`/`setpgid`/`getpgid`/`getsid` translate in both directions; a pid outside is `ESRCH` |
| P4 | an orphan is handed to its namespace's init, not the machine's |
| P5 | init's death kills the namespace and refuses new members |
| P6 | init ignores `SIGTERM` from inside, takes `SIGKILL` from outside, ignores `SIGKILL` from inside, takes a handled signal from inside |
| P7 | `si_pid`, `SO_PEERCRED`, `SCM_CREDENTIALS`: translated, 0 when not visible |
| P8 | `/proc` of a namespace lists only its pids; `NSpid`; `/proc/self`; `ns/pid` |
| P9 | `CLONE_NEWPID` needs `CAP_SYS_ADMIN`; `|CLONE_THREAD`, `|CLONE_PARENT` `EINVAL`; depth 32 `ENOSPC`; `CLONE_NEWUSER|CLONE_NEWPID` from an unprivileged user works |
| P10 | `unshare(CLONE_NEWPID)` moves later children, not the caller |
| P11 | cgroup.procs lists/moves by the reader's numbers |
| P12 | F-37: pid namespaces filled to `ENOMEM` |

Landings, in order: (1) this document; (2) `PidNamespace`, `Numbers`,
the maker paths, `getpid` and the calls of §4 down to `wait`/`kill`/groups;
(3) init (§5); (4) signals' and credentials' pids; (5) procfs and
`/proc/<pid>/ns/pid`; (6) tty, cgroupfs, locks, semaphores; (7) the boot
line and F-37; (8) the docs' records.

## 10. Where it stands (2026-10-01)

Built over N4 and gated: `cargo xtask check`; the `pidns` line (61 calls, 12
refusals) and the `kmem` fill on x86_64, AArch64 and ARMv7-A `--smp 2`;
`test-vfs` on all three with ferrousli's busybox; `test-shell` on x86_64; and
`test-init --arch all`. Negative controls, each stopping the boot with the
check's own message (first run on 383ff664; five of them run again on the tree
that landed, `p1`, `p5`, `p6`, `p9` and `p12` below, with a marker line where
the sabotage is not a changed constant):

| Rule | Sabotage | Message |
|---|---|---|
| P1 | first number 5 | the first process made in a pid namespace was not pid 1 there |
| P2 | numbering jumps after 1 | the second process made in a pid namespace was not pid 2 there |
| P10 | `pid_for_children` names the namespace it is in | pid_for_children after unshare(CLONE_NEWPID) names the namespace it is in |
| P3 | `from_user` takes the kernel number | kill(2) from inside a pid namespace found nothing |
| P3 | unseen parent told as the kernel's number | getppid of a pid namespace's init was not 0 |
| P4 | local reaper skipped | an orphan in a pid namespace was given no parent |
| P5 | `dying` not set | a process joined a pid namespace whose init had gone |
| P5 | members not killed | a process in a pid namespace outlived its init |
| P6 | `is_init` false | the init of a pid namespace took SIGTERM from inside it |
| P6 | ancestor test inverted | SIGKILL from inside a pid namespace ended its init |
| P7 | `show_pid` tells the kernel number | si_pid was not told in the reader's namespace's numbers |
| P8 | listing by kernel numbers | a procfs of a pid namespace did not list exactly its processes by its numbers |
| P8 | `NS*` chain off | NSpid did not list the numbers from the reader's namespace down |
| P8 | `/proc/self` by kernel number | /proc/self in a pid namespace was not the reader's number there |
| P9 | `unshare` privilege test off | uid 1000 made a pid namespace with unshare |
| P9 | `clone` privilege test off | uid 1000 cloned into a pid namespace |
| P9 | CLONE_THREAD/PARENT test off | CLONE_NEWPID was accepted with CLONE_THREAD or CLONE_PARENT |
| P9 | depth limit off by one (unshare) | a 33rd level of pid namespaces was made |
| P9 | depth limit off by one (clone) | a clone into a 33rd level of pid namespaces was accepted |
| P11 | `cgroup.procs` lists kernel numbers | cgroup.procs listed more than the reader's namespace's processes by its numbers |
| P11 | write finds by kernel number | a pid namespace moved a process it cannot see into a cgroup |
| P12 | namespace uncharged | kmem: a job made more than its limit could hold |
| P12 | `Numbers` uncharged | kmem: a fill was refused by something other than its limit |

**The `test-vfs` double fault, found.** `test-vfs` on x86_64 with the busybox
init double-faulted four times out of four. The fault looked like the ioctl
path's, because that is where a trap landed, but a walk of the frame pointers
from the double fault's report (a throwaway build that printed them) gave
`ferrix_syscall_stub`, `trap::system_call`, `dispatch_with`,
`linux::dispatch`, `clone_with`, `fork_into`, the closure, `Process::forked_into`,
`standard_streams`, `open_console`, `Namespace::open` and `open_resolving`, and a
trap on top, with the stack's bottom 0x80 bytes below. It was a fork, not an
ioctl: the stack of four pages was full when a trap came in.

The frames were the cause. `clone_with` and `fork_into` stood 3.8 KiB deep each
and `forked_into` 4.1 KiB (`objdump` of the kernel, the `sub rsp` of each), because
each held the `Process`, over a kilobyte, by value, and `fork_into` again as
`Result<Result<Process>>` and in `.map(Arc::new)`; and `forked_into` then opened
a console for a descriptor table that its copy of the parent's replaced a line
later, which is the deepest thing a fork does. That path stood within a few
hundred bytes of the bottom on N4's tree already. This work's three fields made
`Process` 48 bytes bigger, which, held in each of those copies, crossed it.
Moving this work's own code out of line (8d494755) could not cure it, because
the margin was not this work's.

The cure is the fork's, not a bigger stack: the descriptor table and the
directory context are made before the `Process` (`Process::with_context`), the
fork's from the parent's, so no console is opened for a fork; and the `Process`
is built into its `Arc` by a frame of its own (`shared_with_context`) that is
gone before anything deep runs, and handed on as an `Arc`. After it
`forked_into` stands 600 bytes and `fork_into` 80; what is left of the path is
`sys_clone`'s 3.7 KiB and the one transient `shared_with_context`.
`test-vfs` passes on all three architectures.

**Review by the certification consultant, 2026-10-01: changes required, made.**

* **B1, the native child.** `launch::load_native` went through
  `exec::load_native` and `Process::new`, which numbers nothing, so a native
  child of a container process stayed in the first namespace. It now enters its
  creator's children's namespace (§4); P13 in `fs/pidns_check.rs` makes one as
  `process_create` does and requires its `getpid` to be its creator's number for
  it, a machine pid to be `ESRCH` to it, and its init's end to end it. Control:
  the child left in the first namespace.
* **C1.** A first fork into a fresh namespace that failed left it open for a pid
  2 with no init. `Numbers::drop` of a namespace's pid 1 closes it. P14 drops a
  first child and requires the next fork refused. Control: the namespace left open.
* **C2.** `init_gone` no longer lists the registry (which can fail) but walks
  the namespace's map by key, which allocates nothing, so P5 cannot be skipped
  for a lack of memory.
* **C3.** A second `unshare(CLONE_NEWPID)` before a fork is `EINVAL`, as
  `copy_pid_ns` answers; the depth test (P9) forks once per level. `P9`'s
  `EINVAL` is checked for `unshare` and for the `clone` test.
* **C4.** `find_in` requires that what it found is what the number names (as
  the process, or one of its threads). A stale map entry cannot be made at boot,
  so it has no control; the host tests of the number maps do not reach it.
* **C5, the stack.** Frame sizes of the fork path now (`sub rsp` of the first
  instruction, x86-64 `debug` profile): `sys_clone` 3,712, `shared_with_context`
  3,728 (transient, gone before anything deeper runs), `Process::with_context`
  1,592, `forked_into` 600, `fork_into` 80. Nothing stops them growing again;
  a frame budget in `xtask check` (objdump over the kernel, as the assembly
  budget counts lines) is a backlog row, with these numbers.
* **C6.** P5's control does not reach `clone_with`'s re-check after a child is
  published (a child numbered in a namespace whose init went in between, killed
  there): the window is two instructions wide and a boot cannot open it; it is
  read, not run.

**Negative controls on the tree that landed** (`test-boot --arch x86_64`, one
sabotage each, the boot stopping with the check's own message; where the
sabotage is not a changed constant it prints `NEGATIVE CONTROL <name>` once,
and the count of those lines was read from each log). Each sabotage is one exact
text replaced in the file named, which is how it is reproduced; the logs are kept
in `~/.local/share/ferrix/logs/pidns-controls-2026-10-01/` on nazuna.

| Control | File: the text replaced, and by what | Message |
|---|---|---|
| b1-native | `syscall/launch.rs`: `let pids = creator.and_then(Process::children_namespace);` by `let pids = None;` | a native child of a process in a pid namespace was in the first namespace |
| c1-closed | `syscall/pidns.rs`, `Numbers::drop`: `if self.own() == 1 {` by `if self.own() == 1 && false {` | a fork after the first one into a pid namespace failed joined a namespace with no init |
| c3-unshare | `syscall/namespace.rs`: `if flags & CLONE_NEWPID != 0 && process.children_in_other_namespace() {` made `if false && ...` | a second unshare(CLONE_NEWPID) before a fork nested a namespace in one nobody is in |
| c3-clone | `syscall/family.rs`: the same test on `parent` made `if false && ...` | a clone into a namespace nested in one nobody is in was accepted |
| p5-members | `syscall/pidns.rs`, `init_gone`: `if inside && !core::ptr::eq(Arc::as_ptr(&process), init) {` made `if false && ...` | a process in a pid namespace outlived its init |
| p9-clone-depth | `syscall/family.rs`: `>= pidns::MAX_LEVEL` by `> pidns::MAX_LEVEL` | a clone into a 33rd level of pid namespaces was accepted |
| p9-depth | `syscall/pidns.rs`, `create`: `if level > MAX_LEVEL {` by `if level > MAX_LEVEL + 1 {` | a 33rd level of pid namespaces was made |
| p1-first | `syscall/pidns.rs`, `create`: `last: 0,` by `last: 4,` | the first process made in a pid namespace was not pid 1 there |
| p6-init | `syscall/pidns.rs`, `is_init`: `numbers.own() == 1` by `numbers.own() == 1 && false` | the init of a pid namespace took SIGTERM from inside it |
| p12-charge | `syscall/pidns.rs`, `create`: `Charge::arc::<PidNamespace>()` by `Charge::bytes(0)` | kmem: a job made more than its limit could hold |

The other controls of the table above ran on 383ff664, before the fork path
was reworked and the tree rebased; none of the code they sabotage has moved
but by lines.

Stack depth (2026-11-04, ledger 6623, F-101): a native process is numbered on the PROCESS_MAKE path through `Process::new_shared`, built in its `Arc`; `make_native_in` 352 bytes where it was 5,280 on AArch64's dev profile. The owed depth check is in BACKLOG.
