# PID namespaces

Stage 13's pid namespaces, on top of the user namespaces of
`docs/NAMESPACES.md` (N4). The roadmap's exit criterion -- "an unprivileged
user namespace runs a process whose pid is 1 inside it, under a memory limit
... with a seccomp filter" -- needs the pid-1 part from here; the memory
controller and seccomp are other streams'.

Status: **built on `stage13-pidns`, not landed** (§9). Linux's semantics throughout; §8
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

Native processes (`process_create`) are in the first namespace.

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
