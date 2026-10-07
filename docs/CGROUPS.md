# cgroups: the job tree, seen as cgroup v2

Version 1, a draft. Written on 2026-09-23. It is the cgroup half of stage 13,
which the customer put first that day, and builds on the customer's decision
of the same day that **every cgroup is backed by a `Job`** (`docs/INIT.md` §0,
C8; `docs/BACKLOG.md`, Decisions). `docs/INIT.md` §0.1 is what the first user
needs from this; `docs/ARCHITECTURE.md` §3 and §6 are the architecture. G1 to
G5 (§7) are built. §9 lists what is still the customer's to decide.

## 1. What this is, and what it is not

It is cgroup v2 as Linux defines it: one unified hierarchy, mounted as
`cgroup2`, that `mkdir`, `rmdir`, `cgroup.procs` and the controller files
drive. A program written against Linux's cgroups (systemd, a container
runtime, `docs/INIT.md`'s init) works unchanged. The controllers are
`memory`, `pids`, `cpu` and `io`, as `ARCHITECTURE.md` §6 says, and they
arrive one at a time (§7).

It is **not** a second container object. The kernel already has one: a `Job`
is "a container of processes, and where resource limits and kill authority
live" (`ARCHITECTURE.md` §3). A cgroup *is* a job, and cgroupfs is a view of
the job tree for the Linux ABI, as `/proc` is a view of processes. A native
program sees the same tree through job handles. A microkernel that someday
drops cgroupfs keeps the jobs and everything in them.

It is not cgroup v1, threaded cgroups (`cgroup.type` is `domain` and nothing
else), or the cgroup namespace. The namespace comes with the rest of stage
13's namespaces, and nothing here stands in its way.

## 2. What the job has to become

Today (read from `src/kernel/src/object/job.rs` and the process code on
2026-09-23):

* only `process_create` puts a process in a job, so every Linux process is in
  none, and `fork` copies no membership;
* a `Process` does not know its job;
* members and children are held weakly, and an exited process leaves its
  weak entry until it is reaped: a job cannot say whether it is populated;
* a job has no name and no identity a path could be built from;
* a killed job is killed for good, and refuses new members.

Each of these changes, as follows.

### 2.1 Every process is in exactly one job

At boot the kernel makes **the root job**, which is cgroupfs's root. A
process made by `Process::new` without a job (init, and the kernel's own
programs) starts in it. `devmgr`'s job, which the kernel makes today as a
root of its own, becomes a child of the root named `drivers.slice`, the name
`docs/INIT.md` §5.1 gives it.

`Process` gains `membership: SpinLock<Arc<Job>>`, held strongly: a process
keeps its job alive, as a Linux task keeps its `css_set`. `fork` reads the
parent's job under the parent's membership lock and joins the child to it
before the child is published (`src/kernel/src/syscall/family.rs`, before the
`processes` insert in `clone_with`). So a fork and a move of its parent are
ordered: the child lands in the job the parent was in when the fork took the
lock, and never half in one and half in the other. `process_create` joins the
job it is given, as it does now.

### 2.2 Membership is counted, and "populated" is exact

Each job keeps two counts under its lock:

* `live`, its own member processes that have not exited;
* `busy_children`, its children whose subtree is populated.

A job is **populated** when either count is nonzero. `live` goes up on join
and down in `Process::end`, the moment the last thread exits, not at reap. A
zombie is not a member, exactly as on Linux. When a count crosses zero, the
change walks up the tree to the parent's `busy_children`, one job lock at a
time and child before parent, and stops at the first job whose own populated
state does not change. At every job whose state flipped, the kernel wakes
the job's event queue (§4) and fires its `EMPTY` observers (§5).

A job keeps no member list at all (changed in G1 from this document's first
draft). A process's `membership` is the one truth, and `cgroup.procs` and
both kills find members by walking the process registry for the processes
whose job the job contains. A list would have had to be filled where each
process is first shared, which is several places, while the pointer is set
where every process is built, which is one. The counts are what *decide*
populated, and the registry is where members are *found*.

A fork's child is findable before it can run. A kill that began before the
child was published finds it in the registry. A kill that began after, the
fork sees at once: `clone_with` asks the child's job whether it is dying,
and ends the child before it starts.

### 2.3 Names, and who holds whom

A job gets a `name` and an `id` from a global counter. A **named** child is
made by `mkdir` in cgroupfs. Its parent holds it strongly until `rmdir`, as a
directory in a filesystem is held by the filesystem, not by who made it. An
**anonymous** child is made by native `job_create`. It is held as now, by its
handles and its members, appears in cgroupfs as `job-<id>`, and goes away
when the last handle closes and it is empty. That is how a native job has
always behaved, and cgroupfs only shows it. `rmdir` removes a named job, and
refuses one with members or children (`EBUSY`), as Linux does. An anonymous
job answers `rmdir` with `EBUSY` too (settled in G2): its handles hold it,
and it goes when they do.

A process path in `/proc/<pid>/cgroup` (`0::/system.slice/sshd.service`) is
built by walking `parent` and joining names. It is computed when read, never
stored.

### 2.4 Two kills, kept apart

`job_kill` keeps its native meaning: it kills everything and seals the job,
so nothing can join it again. `docs/DEVMGR.md` relies on that for a dead
driver's job.

`cgroup.kill` is Linux's and does not seal. It sets a `killing` mark under the
lock, takes the current members out as the job kill does, and kills them
outside the lock (`job.rs` explains why the lock must not be held across
`process::kill`). Then it clears the mark. A fork that joins a job while the
mark is set is killed as it starts, which is how Linux keeps a forking loop
from outrunning `cgroup.kill`. Both kills walk the subtree.

### 2.5 Lock order

`Process.membership` before `Job.state`, never the reverse, and never two
job locks at once. A move takes the process's membership lock, then leaves
the old job and joins the new one, each under its own lock and one after the
other. The count walk of §2.2 takes one job lock at a time going up. The
kill takes one at a time going down, and releases each before killing. So no
path holds two job locks, and none holds a job lock where the scheduler's
locks are taken (FX-0503's rule, `crate::sync::SpinLock`).

## 3. cgroupfs

A new in-kernel filesystem, `src/kernel/src/fs/cgroupfs.rs`, registered as
`cgroup2` in `filesystem_named` (`src/kernel/src/syscall/fsctl.rs`, where the
comment already says it joins that match) and listed in `/proc/filesystems`.
Every text it reads or writes is parsed and rendered by a new
`src/lib/fs/cgroupfs`, a pure crate with host tests and a fuzzer. That covers
`+memory -cpu` lists, `max` or a byte count, `quota period`, `key value`
tables and the pid lists.

Every directory answers `caches_lookups() = false`, like procfs's, because a
native `job_create` adds a directory behind the VFS's back. That also forbids
mounts inside the tree, which Linux forbids too. `mkdir` and `rmdir` arrive
through the generic `Namespace::mkdir`/`rmdir`, and `chown` through
`set_attributes`, so the VFS needs no new paths.

The files in every cgroup:

| File | Read | Write |
|---|---|---|
| `cgroup.procs` | the pids of the members' thread-group leaders | a pid: move that process (§3.1) |
| `cgroup.threads` | the tids | refused, `EOPNOTSUPP` (no threaded mode) |
| `cgroup.type` | `domain` | `threaded` is `EOPNOTSUPP`, anything else `EINVAL`: Linux takes `threaded` alone, not even `domain` |
| `cgroup.events` | `populated 0/1`, `frozen 0/1`; pollable with `POLLPRI` (§4) | refused |
| `cgroup.kill` | refused | `1`: §2.4 |
| `cgroup.freeze` | `0/1` | `0/1`, from landing F1; until then `frozen` reads 0 and a write is `EOPNOTSUPP` |
| `cgroup.controllers` | what the parent's `subtree_control` enables here | refused |
| `cgroup.subtree_control` | what this cgroup enables for its children | `+name -name …`, split on single spaces; a later token for a controller overrides an earlier one, and a name not built is `EINVAL` |
| `cgroup.max.depth`, `cgroup.max.descendants` | `max` or a count | a limit on `mkdir` beneath |
| `cgroup.stat` | `nr_descendants`, `nr_dying_descendants 0` | refused |

The root has `cgroup.procs`, `cgroup.subtree_control` and the others, but
no controller limit files, as on Linux.

### 3.1 Moving a process, and delegation

A write of a pid to `cgroup.procs` moves the whole process, all its threads,
by §2.5's order. It is allowed when the writer holds write permission on the
target's `cgroup.procs`, which the open checks, and on the `cgroup.procs` of
the common ancestor of the source and the target, which the write checks as
whoever opened the file (Linux's `cgroup_procs_write_permission`, with the
opener's credentials, so a descriptor passed on carries only its opener's
rights). Root passes both. Those are Linux's cgroup v2 delegation rules, and
with them `chown` of a directory and of the files the delegate writes
(`cgroup.procs`, `cgroup.threads`, `cgroup.subtree_control`) hands a subtree
to a user (C7). The delegate may move its processes within the subtree and
not out of it: out of it, the common ancestor is above the subtree and not
the delegate's. This document's first draft also asked that the writer's
effective uid match the process's. That is cgroup v1's rule, which v2
dropped for the common ancestor, so it is not built (G4).

Every directory and file has its own owner, group and mode, as kernfs keeps
them per node. `chown` and `chmod` change one node; a `mkdir` by someone
other than root makes the new directory and all its files the maker's, as
Linux's `cgroup_kn_set_ugid` does, and gives the directory the mode `mkdir`
asked for. They are kept on the job, because a cgroupfs directory is a view
made afresh at every lookup. A move into a job that native `job_kill`
sealed fails `ENOENT`, and into one `rmdir` removed, `ENODEV`, as Linux
answers for a dead cgroup; a `mkdir` in a removed one is `ENODEV` too.

**No internal processes.** A cgroup that enables a domain controller
(`memory`, `io`) in `subtree_control` may not hold processes, except the
root. Such a write fails `EBUSY` while it has members, and a move into such
a cgroup fails `EBUSY`. Threaded controllers (`cpu`, `pids`) are exempt
while the cgroup could still be a threaded root: no domain controller on and
no populated child. That is Linux's `cgroup_vet_subtree_control_enable` and
`cgroup_migrate_vet_dst`, in `ferrix_cgroupfs::controllers` with host tests;
the kernel decides it under the lock a move counts a process in under, so a
move and a `subtree_control` write cannot pass each other. Until a
controller is built `subtree_control` takes no name, so no boot check can
reach the rule yet. This is the rule that makes controllers' arithmetic well
defined, and it is why `docs/INIT.md`'s init moves itself into `init.scope`
first.

### 3.2 `clone3` into a cgroup

`CLONE_INTO_CGROUP` no longer answers `ENOSYS` (`family.rs`, G4). The
`cgroup` field is a descriptor of a cgroupfs directory, `O_PATH` or not, and
the child is counted in that job from the moment it is made, never in its
parent's, so its first instruction already runs there. The checks are
Linux's `cgroup_css_set_fork`'s, with the parent as the writer: `EBADF` for a
descriptor not open and for one that is not a cgroup directory (a file
inside one included), `ENODEV` for a removed cgroup, `EACCES` without write
access to its `cgroup.procs` or the common ancestor's, `EBUSY` for the
no-internal-process rule; and `EINVAL`, before any of those, for a
descriptor past `INT_MAX` or a `struct clone_args` shorter than
`CLONE_ARGS_SIZE_VER2`. A thread asked into another cgroup is
`EOPNOTSUPP`. It is init's race-free start (`docs/INIT.md` §5.2).

## 4. `POLLPRI`: the change notification

`cgroup.events` changes are announced as Linux announces them: the file polls
`POLLPRI` (and `EPOLLPRI`) once after every change, and `select` reports it
in the exception set. Nothing polled priority before G3, so, as built:

* `Readiness` (`src/lib/fs/vfs/src/node.rs`) has `priority`, false for every file
  but `cgroup.events`;
* `poll` and `ppoll` map it to `POLLPRI` (`poll::revents`), `select` to the
  exception set (`poll::select_sets`, Linux's `POLLEX_SET`), and `epoll` to
  `EPOLLPRI` (`fs/epoll.rs`'s `bits`);
* a job's `events` queue, woken on every populated flip since G1 (and on
  frozen flips from F1), is shared (`Arc<WaitQueue>`), and an open of
  `cgroup.events` is an `EventsFile` of its own, no longer procfs's
  snapshot: it answers `poll_queues` with that queue and `poll_changes`
  with its `wakes()` count, as eventfd does.

Linux's semantics are edge-triggered in effect: `POLLPRI` is asserted from a
change until the file is read again. An `EventsFile` renders afresh on every
read from offset 0, as a `seq_file` does after `lseek` to 0, and records
the `wakes()` count it read just before rendering; it reports `priority` while
the count has moved since. With it comes `POLLERR`, because kernfs's
`kernfs_generic_poll` answers `DEFAULT_POLLMASK|EPOLLERR|EPOLLPRI`, so a
changed file is also in `select`'s read and write sets, as on Linux. An open
file not yet read reports `POLLPRI` at once, as Linux's does: kernfs's open
node starts its counter at 1 and a new open file's at 0. A program reads,
then waits, as systemd does. `memory.events` and `pids.events` will use the
same mechanism.

## 5. The native side: `EMPTY`, and a job for a cgroup

`Signals` gains `EMPTY = 1 << 4`, and `ALL` widens to `0x1F`. A job asserts
`EMPTY` while it is unpopulated and clears it when it is populated again.
Unlike `TERMINATED`, it is a level, not a latch. So `object_wait_async` on a
job for `EMPTY` fires once the job is empty, which is what a native init
watches in place of `cgroup.events` (`docs/INIT.md` §9).

`job_for_cgroup(dirfd, rights) -> handle` (0x102A) returns a handle to the
job behind a cgroupfs directory, with at most the rights the caller's access
to that directory's `cgroup.procs` allows: `MANAGE` and `WAIT` for write,
`WAIT` for read. It is the one bridge from a path to a handle, and it goes
one way only. There is no call that names a job's path from a handle,
because a handle is a capability and a path is not.

**A delegated cgroup's own limits stay its delegator's.** `job_set_limit`
asks for `SET_LIMIT` (`1 << 7`), not `MANAGE`, and `job_for_cgroup` grants
it only with `MANAGE` and only to a caller who may also write that cgroup's
`memory.max`, `pids.max` and `cpu.weight`. A `chown` delegation hands over
the directory, `cgroup.procs`, `cgroup.threads` and
`cgroup.subtree_control`, never those files, so the delegatee may fill,
empty and kill its cgroup and may not lift its own `MemoryMax=` or
`TasksMax=`, as on Linux. Until 2026-09-26 `MANAGE` was enough, and a
`Delegate=yes` service could raise its own limits to unlimited through the
native call. Like every right, `SET_LIMIT` is only ever dropped: a duplicate
or a transfer cannot regain it. A job made with `job_create` carries it, so
the delegatee limits what it makes. **A child's limit is a number, not a
guarantee**: it may be set above its parent's, and binds nothing beyond it,
because every job's use is charged to each of its ancestors
(`object::quota`) and the tightest limit on the way up refuses first. The
check is `src/kernel/src/fs/cgroupfs/limits_check.rs`, the `limits` boot line.

## 6. Where each thing is charged

The controllers need to know, at a choke point, which job pays. The survey
of 2026-09-23 found these:

| Controller | Charged where | Uncharged where |
|---|---|---|
| `pids` (tasks, as Linux counts) | `clone_with` and `process_create` for a process, `clone_thread` for a thread, before the pid is allocated | `Drop for Process` and `release_thread` |
| `memory` | `mm::allocate_frames` callers on the user path: `commit_page` (anonymous and file faults), the copy-on-write copies, the fork copies of held pages, `write_page`/`hold`, and the page-cache fill in `fs/pages.rs` | the frame's free, from the owner recorded at charge |
| `cpu` | the scheduler's entity for the job (§7, S1) | — |
| `io` | a block request `Part` carries the job that submitted it (`src/lib/fs/block/src/schedule.rs` already names this as stage 13's place) | — |

**Memory has no owner today.** A VMO carries no charge, and `AddressSpace::
resident_pages` is computed on demand. So landing M1 gives each committed
page a charge to one job. Anonymous memory is charged to the job of the
process that faulted it in. A page-cache page is charged to the first job
that brought it in, and stays charged there, which is Linux's
first-touch rule and its known imprecision. The charge is recorded in
`PageInfo`, which already carries a refcount and the owning VMO
(`ARCHITECTURE.md` §4), so an uncharge at free needs no lookup.

A charge that would exceed `memory.max` first tries reclaim inside the job
(landing M2), and then **OOM-kills inside the job**. The victim is the
member with the most charged memory in the subtree, killed as `SIGKILL`.
`memory.events` counts `max`, `oom` and `oom_kill`. The fault that could not
be charged is retried after the kill, and gets `SIGBUS` if the job is empty
and still over. That is the scoped OOM kill of stage 13's exit, and the one
`docs/INIT.md` §5.5 shows as `oom-kill`.

## 7. The landings

In story points, each landing gated by what it names. The kernel's in-boot
checks are the pattern `src/kernel/src/syscall/check.rs` sets; a user-level
check runs in `test-shell` with zinc and uutils. The G landings are what
`docs/INIT.md` needs before init boots.

| | Landing | Gives | Gate | Points |
|---|---|---|---|---|
| G1 | Every process in one job: root job, `membership`, fork inherits, `live`/`busy_children` counts, names and ids, the `cgroup.kill` kill beside `job_kill`, `drivers.slice` | C2's inheritance, C5's mechanism | boot checks: a fork's child in its parent's job; populated flips at the last exit, not the reap; a killed forking loop ends | 8 |
| G2 | `src/lib/fs/cgroupfs` and cgroupfs: mount, `mkdir`/`rmdir`, `cgroup.procs` read and move, `cgroup.kill`, `cgroup.events` (without `POLLPRI`), `subtree_control` with no controllers yet, `/proc/<pid>/cgroup` | C1, C2, C5 | host tests, Miri and the `cgroupfs_write` fuzzer; the `cgroups` boot check, which drives cgroupfs through the VFS as a program's calls would (mount, `mkdir`, a move by pid, `/proc/<pid>/cgroup`, `cgroup.events`, `cgroup.kill`, the limits, nine refusals), and its negative control, a `cgroup.kill` that does not kill. The user-level run moved to G4, whose delegation needs a user anyway | 8 |
| G3 | `POLLPRI` through `Readiness`, `poll`, `select`, `epoll`; `cgroup.events` pollable | C4 | boot check: an `epoll` on `cgroup.events` wakes at the last exit and not before | 3 |
| G4 | `CLONE_INTO_CGROUP`; delegation by ownership; the no-internal-process rule | C3, C7 | `test-shell` as a non-root user in a chowned subtree; a refused move outside it | 5 |
| G5 | `EMPTY`; `job_for_cgroup`; native jobs as `job-<id>`; `devmgr`'s jobs under `drivers.slice` | C8 | boot check: a native wait for `EMPTY` fires with the populated flip; `test-restart` still passes | 3 |
| P1 | `pids`: `pids.max`, `pids.current`, `pids.events` | C6 | a fork bomb in a `pids.max 16` cgroup fails `EAGAIN` at 16 | 3 |
| M1 | `memory` charging: `memory.current`, `memory.max`, `memory.events`, `memory.stat` (anon, file); the scoped OOM kill | C6 | a process past `memory.max` in one cgroup killed, a sibling untouched | 13 |
| M2 | Reclaim: clean page-cache pages, scoped to a job and global, and `memory.high`, which reclaims above it | stage 13's reclaim | a `memory.high` job's file pages evicted and read back identical | 13 |
| F1 | `cgroup.freeze`, over the stopped state processes already have | C4's `frozen` | a frozen job's members stop and resume | 3 |
| S1 | `cpu.weight`: a group entity per job in `src/lib/kernel/sched`'s EEVDF, hierarchical | C6 | host tests of shares; a boot check of two busy cgroups at 1:3 weights within 10% | 13 |
| S2 | `cpu.max`: bandwidth per period, throttling a group's entity | C6 | a `cpu.max 20000 100000` job held near 20% | 8 |
| B1 | `io.weight` in `src/lib/fs/block`'s scheduler | C6 | host tests of the dispatch shares | 5 |

G1 to G5 are **27 points**, and they are all `docs/INIT.md`'s first boot
waits on. Its landings L1 and L2 are host-only and can run beside them. The
controllers are **58 points** more, and `memory` and `pids` come first. The
roadmap guessed "a month, ≈ 60" for all of stage 13 before points existed.
Its cgroup half alone is 85 by this count, and the roadmap is corrected to
say so.

Order: G1, G2, G3, G4 (init's C1 to C5), then G5 and P1, then M1 and M2 (the
stage exit's memory limit), then F1, S1, S2 and B1. Namespaces and seccomp
follow, as `docs/BACKLOG.md` decided.

### 7.1 Where it stands (2026-09-24; the controllers 2026-09-26)

| | State | On `main` as |
|---|---|---|
| G1 | done | 2a56325b |
| G2 | done | ae76c099, and 491aeb1f for the two lock files it left out |
| G3 | done, 2026-09-24 | 02bfbe69 |
| G4 | done, 2026-09-24 | 410aef15 |
| G5 | done, 2026-09-24 | "Wait for a cgroup to empty through a native job handle" |
| P1 | done, 2026-09-26, with the certification's F-35 | "Charge each job for its tasks, memory, objects and processor share"; "Show the job quotas as cgroup2's cpu, memory and pids controllers" |
| M1 | charging, `memory.max`, `memory.current` and `memory.events` done, 2026-09-26; kernel memory in `memory.current` and `memory.stat`'s `kernel` line the same day, with the certification's F-37; the scoped OOM kill the same day; `memory.stat`'s other keys not | the same two; "Charge the kernel heap a job drives through the Linux personality (F-37)"; "Kill inside the cgroup whose memory.max a program's fault finds full" |
| S1 | done differently, 2026-09-26: a weight per job applied to each task's, not a group entity | the same two |
| M2, F1, S2, B1 | built, 2026-09-30: §10 to §13 | (landing pending) |

What is left of the controllers is the rest of `memory.stat`, M2's
reclaim, F1, S2 and B1. Init's L5 writes `TasksMax=` and `MemoryMax=`
to `pids.max` and `memory.max`, which exist since 2026-09-26.

**P1, M1's charging and S1, as built (2026-09-26).** The certification's
work order W-13 built them as the job quotas the Security Target claims
(`docs/certification/IMPLEMENTATION.md`, "As built", has the design and
where it moved from this document's). A job's counters are a slot in
`src/kernel/src/object/quota.rs`, charged hierarchically; cgroupfs's
`pids.max`, `pids.current`, `pids.events`, `memory.max`, `memory.current`,
`memory.events` and `cpu.weight` read and write that slot, and a native job
handle reaches the same through `job_set_limit` and `job_get_quota`.
Differences from §6 and §7: tasks are charged in the core's
`Process::new` and a thread's id allocation, and given back at reap; memory
is charged per frame at allocation to the running task's job, page tables
included, and the frame record keeps the slot, so every free uncharges; a
charge past `memory.max` is refused like running out of memory, since
there is no reclaim or scoped OOM kill yet; and `cpu.weight` scales each
task's weight by its job's weight over its job's load instead of adding a
group entity to `src/lib/kernel/sched` -- Linux's own per-processor approximation of
a group's share, one task alone in its job keeping 50.0% of a processor
against eight in another at boot. `BUILT` is `cpu memory pids`, so the
no-internal-process rule is reachable, and its host tests have a boot path.
The `cgroups` boot line drives the files, and `test-vfs` command 19 forks
until `pids.max` 10 refuses.

**Kernel memory in `memory.current` (2026-09-26).** The certification's
F-37 (work order W-15) charges the kernel heap the Linux personality holds
for a cgroup's programs to the same counter, in bytes, as cgroup v2 folds
`kmem` into it: open files, dentries, tmpfs inodes and names, pipe and
socket buffers, messages in flight, epoll registrations, regions, record
locks and network queues, each at the size class the heap serves it from.
`memory.max` limits frames and heap together, a charge past it is refused
`ENOMEM` from the call that would have made the object, and
`memory.events`' `max` counts it. `memory.stat` exists, and prints the one
key Ferrix counts apart, `kernel`: the heap part of `memory.current`. An
object made in one cgroup stays charged there until it goes, wherever it is
passed, as Linux's `obj_cgroup` keeps it. Without reclaim, the dentries a
cgroup's lookups left in the cache stay charged to it until evicted, where
Linux would reclaim them under pressure -- M2's to fix. `test-vfs` command
21 fills `/tmp` from a shell 256 KiB under its `memory.max` and reads the
three files.

**M1's scoped OOM kill, as built (2026-09-26).** `src/kernel/src/object/oom.rs`.
A charge a system call makes for an object past `memory.max` is still
refused `ENOMEM`. A page of a program's own memory -- its fault, or one a
system call faults in for it, as `read` into a fresh buffer does -- whose
charge is refused at a cgroup's `memory.max` asks for a kill in that
cgroup (the nearest one at or above the faulting task's that is full):
`memory.events`' `oom` counts there and in every cgroup above it, the
process in it or beneath it with the most resident pages is ended as
`SIGKILL` (never one outside it, never pid 1), `oom_kill` counts in the
victim's cgroup and every one above, and each cgroup counted wakes its
`memory.events`, an `EventsFile` like `cgroup.events` that polls `POLLPRI`
and `POLLERR` (`EPOLLPRI` in epoll, the exception set in `select`) until
it is read again from its start. The fault is retried; the victim, when it
is the faulting process, leaves on its way back to user mode instead, so a
parent's `wait4` sees it signalled by 9 (status 137 from a shell). A
victim's memory is freed when it is reaped, not at exit as on Linux, so a
victim that has let go of everything else has its address space emptied
by the next fault needing the room, as Linux's OOM reaper does, and a
fault finding a victim still ending retries a millisecond later. With
nothing killable left, or no limit full (the machine out of frames), the
fault fails as before -- `SIGSEGV`, not §6's `SIGBUS`, or `EFAULT` for a
system call's page. Not built: `memory.oom.group`, `memory.events.local`,
`oom_score_adj` in the choice, and a wake for `max` alone (a charge is
refused under locks; its count moves, but a sleeping poller is woken only
by an OOM). Its checks: the `cgroups` boot line runs a program writing
8 MiB in a cgroup with `memory.max` 1M beside a sibling holding 2 MiB,
and requires `SIGKILL`, the sibling alive, `oom_kill 1` there and in the
parent and nothing in the sibling, and an `EPOLLPRI` wake; `test-vfs`
command 22 has busybox `dd` read 32 MiB under a 16M limit and requires
status 137 and `oom_kill 1`. Negative controls: no kill (the program ends
by `SIGSEGV`), the victim chosen outside the cgroup (the sibling is
killed), and `memory.events` polling the wrong queue (the wait ended by
its recheck) each fail by the check's own message.

**G3, as built (3 points).** §4 says what it is. The `cgroups` boot check
(`src/kernel/src/fs/cgroupfs/events_check.rs`) gives a cgroup two members, reads
its `cgroup.events`, and puts it in an epoll set asking `EPOLLPRI`; a task
waits on the set as `epoll_wait` does. The first member's release must leave
the waiter asleep, the last must wake it with `EPOLLPRI` and its cookie, by
the job's queue and not by the wait's own recheck (`waits_ended_by_a_wake`),
and then the file must poll `POLLPRI` and `POLLERR`, and be in `select`'s
exception set, until it is read again from its start, and not after. Its
negative control, `EventsFile::poll_queues` naming no queue, fails by the
check's own message ("ended by its recheck, not by the release's wake").

**G4, as built (5 points).** §3.1 and §3.2 say what it is. Its checks are
under the `cgroups` boot line (`src/kernel/src/fs/cgroupfs/delegation_check.rs`):
a program, `arch::USER_INTO_CGROUP_PROGRAM` on all three architectures,
starts a child with `CLONE_INTO_CGROUP` that reads `0::/check-g` from
`/proc/self/cgroup` first thing, and gets `EBADF` for a descriptor not open
and for one of `/tmp`; uid 1000, given a subtree by `chown`, makes a cgroup
in it and not in root's, opens its own `cgroup.procs` and not root's, moves
a process within the subtree and back and is refused `EACCES` moving it out,
even to a `cgroup.procs` it owns; and a removed cgroup refuses a move and a
`mkdir` with `ENODEV`. The user-level run G2 moved here is a `test-vfs`
command, not `test-shell`, because `test-vfs` is where the image has `su`
and the user `ferrix`: root hands `deleg` to uid 1000, `su ferrix` makes
`work`, moves its own shell in, reads it from `/proc/self/cgroup`, and is
refused moving it to `other` and to the root. Each check has a negative
control that fails by its own message: the child forked into its parent's
job, no common-ancestor check, and `cgroup.procs` judged as root whoever
opened it. The rule of no internal processes has host tests only, since
`subtree_control` takes no name until P1. G4 also fixed G2's inode
numbers, which were one number for every file of a cgroup: `file_slot`
compared addresses in a `const` table, which need not be the same table
twice.

**G5, as built (3 points).** §5 says what it is. `Job::signals` reports
`EMPTY` from the job's own counts, `Job::observe` fires a registration at
once when the job already asserts what it waits for, and `job::notify`,
which the count walk's caller runs once every lock is let go, fires the
`EMPTY` registrations of each job that is empty by then and wakes its
native waiters beside `events`. A job kill fires only the registrations
waiting for `TERMINATED`, so one for `EMPTY` waits on for the members to
end. `job_for_cgroup` judges the caller, not whoever opened the
descriptor, since it makes a new capability rather than using the file:
`DUPLICATE`, `TRANSFER` and `WAIT` to a reader of `cgroup.procs`, and
`MANAGE` too to a writer. The check (`src/kernel/src/fs/cgroupfs/native_check.rs`,
under the `cgroups` boot line) drives it through the native dispatch; its
negative control, `notify` firing no registration, fails by the check's own
message ("the last member's release fired no EMPTY packet with its key").
`devmgr`'s jobs were under `drivers.slice` since G1, each a `job-<id>`
there.

**What the next session does first, and where each starts.**

* **P1, `pids`** (3 points), done as written below but for the charge's
  place, which is the core's `Process::new`. `BUILT` in `src/kernel/src/fs/cgroupfs.rs` gains
  `pids`, and `src/lib/fs/cgroupfs`'s `files` a table of controller files beside
  the base ones (`pids.max`, `pids.current`, `pids.events`), each listed
  only where the parent's `subtree_control` enables it. The charge is per
  task, hierarchical, and taken where §6 says: in `clone_with` for the job
  the child will be in, read once and passed to `Process::forked_into`
  (so a move of the parent cannot split the charge from the membership),
  in `native::process_create` against the target job before
  `exec::load_native` makes the process, and in `clone_thread` before
  `registry::allocate_thread`; each is let go at `Drop for Process` and
  at a thread's `release_thread`. Keep the count on `Members` beside
  `live`, changed under `TREE`, and move a process's charge with it in
  `Process::move_to` without a limit check, as Linux's `pids_can_attach`
  does. `pids.events`' `max` is an `EventsFile`-like file over a queue of
  its own. With a controller built, the no-internal-process rule becomes
  reachable: close G4's race by counting a `CLONE_INTO_CGROUP` child in
  with `count_in_checked`, failing the fork (`EBUSY`) when the target
  turned internal after `cgroup_target` looked.
* **M1, `memory` charging** (13 points), after P1: §6, and §8's warning
  about the seven allocation sites. Charging, `memory.max`,
  `memory.current` and `memory.events` done 2026-09-26, and kernel
  memory with `memory.stat`'s `kernel` line the same day (F-37), and the
  scoped OOM kill the same day; `memory.stat`'s other keys are what is
  left of it.

**How a landing was gated today, and what to keep.** The customer's rule
since 2026-09-23: `cargo xtask check` plus only the rows the change
touches, about five minutes, then land. The userland rows use ferrousli's
busybox on all three architectures (`--init
~/.local/share/ferrix/busybox/ferrousli/{arch}/bin/busybox.static`), and no
musl. Every boot check gets a negative control that prints a marker line
and fails by the check's own message. Five things cost a gate each today:
* `fs::read_file` sizes its read from `stat`, and a generated file says 0,
  so a boot check reads a cgroupfs or procfs file to the end itself.
* The kernel denies `unused_results` and `clippy::too_many_lines` (100).
* A new panic-catalog entry needs `docs/generated/PANICS.md`, regenerated
  on Linux.
* A new crate goes into `Cargo.lock` and `src/tests/fuzz/Cargo.lock`: check with
  `cargo metadata --locked --offline` in both.
* On example, `pgrep -f` with a pattern that also appears in the calling
  command line matches its own shell.

**Open, and not this stage's.** Two failures of ferrousli's busybox on the
Arm architectures reproduce on `main` without the cgroup work (a
`Permission denied` message on both, and `poll` refused on ARMv7-A). They
have a row in `docs/BACKLOG.md`. The stage 7 stop checks (FX-0701) and the
block ring's quiesce check (FX-1004) still flake under load; their rows
carry today's sightings. The init's open decisions are `docs/INIT.md` §14,
2 to 8.

## 8. Risks

* **The membership lock on the fork path.** Every fork now takes the
  parent's membership lock and one job lock. Both are short, and neither is
  held across anything that sleeps. G1's gate includes the fork-heavy
  `test-vfs` and `test-rustc` runs, to show no measurable cost.
* **Memory charging touches every user frame.** M1 changes seven allocation
  sites and the free path. The negative control that shows it fired is a
  frame charged to no job, which the charge check must catch by name.
* **`POLLPRI` is new to every poll path.** G3 adds it as a field that is
  false everywhere except `cgroup.events`, so a mistake shows only there.
* **Group scheduling (S1)** is the largest change to `src/lib/kernel/sched` since
  EEVDF. It is last on purpose: init, and the stage's exit, need none of it.

## 9. What the customer decides

1. **The order in §7.** Draft: G1 to G4, then G5 and P1, then memory, then
   the rest.
2. **`drivers.slice`** as the name of `devmgr`'s job in the tree. Draft: as
   written, matching `docs/INIT.md`.
3. **Whether S1 and S2 (cpu) are in stage 13 at all.** Draft: yes, last. The
   stage names the `cpu` controller, but neither init nor the stage's exit
   needs it.

## 10. Reclaim and `memory.high` (M2, 2026-09-30)

What is reclaimable in a job is the clean page cache of files on a disk,
charged to it, and nothing else: anonymous memory has no swap to go to, a
memory filesystem's pages have nothing to be read back from, and the heap
(`kernel` in `memory.stat`) has no shrinker. `src/kernel/src/user/cache.rs`
keeps a weak list of every file's object (`Vmo::new_filled`) and gives back
pages the way a truncation does (`Vmo::decommit_range`: out of the object,
out of every mapping, a shootdown, then the frame and its charge).

* **Whose.** The frame record names the job a page is charged to; a reclaim at
  job `J` takes pages charged to `J` or beneath it, never a sibling's.
  `memory.min` spares a child using no more than it from the reclaim of a job
  above; `memory.low` does, unless nothing else gave enough (Linux's shares
  are proportional, these are whole-or-nothing).
* **What may be dropped.** A file's source says whether its pages can be read
  again as they were (`PageSource::reclaimable`). A read-only btrfs mount
  does. A writable one does not yet: its dirty pages are in its inodes and
  not in the object, and a page taken between a write's copy and its dirty
  mark would lose the write. An object a shared mapping may write through
  does not either.
* **When.** A fault that finds `memory.max` full reclaims in the job before it
  asks for the kill of §6 (`object::oom`); a page-cache fill that finds it
  full does, and with room for part of a run fills that part; with no limit
  full but the machine out of frames, the fault reclaims anywhere. A program
  that just took a page while its job, or one above it, is over `memory.high`
  reclaims down to the mark (`oom::throttle`, after a fault and after a
  read) and, if that gave back less than the excess, pauses a millisecond.
  Linux's pause grows with the excess; this one does not.
* **Reclaim makes an absent page a thing to fill.** Two places took an absent
  page of a file for a hole: `Pages::read` read zeros, and a fault committed
  a zero page or copied one into a private mapping. Both now ask the source
  (`Filler::is_sourced`): a read fills again, and after eight tries reads the
  source directly; a fault is retried (`SpaceError::Evicted`, which never
  leaves `AddressSpace::fault`) and is `SIGBUS` after 64.
* **Files.** `memory.high` (`max`), `memory.low` and `memory.min` (`0`) take
  a byte count or `max`, rounded down to a page; there is no `memory.swap.*`.
  `memory.events` counts `high` (the mark was hit and reclaimed from, in the
  job whose mark it was and above). `memory.stat` prints `file`, `kernel`,
  `shmem`, `pgscan`, `pgsteal`, `pgfault` and `pgmajfault`; `anon` is left
  out, because page tables are charged as frames and cannot be told from
  anonymous pages, and so are the forty keys with no source.
* **Not built.** `memory.low` events, `memory.reclaim`, proportional
  protection, the dentry and inode caches as reclaimable (M1's note that
  dentries stay charged stands), `workingset_*`, and an audit record for the
  three marks.

## 11. `cgroup.freeze` (F1, 2026-09-30)

A process is frozen when its job, or one above it, has `cgroup.freeze` set. It
is a flag on the core process (`Process::is_frozen`), written under the
membership lock a move holds, set when the process is made, moved, and when
the file is written, and looked at again when the process becomes findable
(`registry::publish`), so a freeze that scanned the table before a fork child
was in it does not miss it. A frozen process's threads wait on their way back
to user mode where a stopped one's do (`Process::must_park`), counted as
parked; a thread in a blocking call is interrupted as a stop interrupts it, and
the call restarts when the cgroup thaws. `SIGKILL` and `cgroup.kill` end a
frozen process as they end a stopped one; `SIGCONT` does not thaw it, and no
parent is told.

`cgroup.events` says `frozen 1` when the cgroup is to be frozen and every live
Linux process beneath it has parked ("live" is `registry::live`, which lists
the Linux personality's processes: **a native process is not parked by a
freeze, and `frozen 1` can be said while a native task runs**), and wakes its pollers when that changes
(`cgroupfs::settle_frozen`: at a freeze, a thaw, a park, a move, a thread
leaving). A cgroup beneath a frozen one says `frozen 1` with its own
`cgroup.freeze` at `0`. A `clone3` into a frozen cgroup starts frozen, because
`Process::new` reads the job it is counted in. **`cgroup.stat` does not print
`nr_frozen_descendants`:** Linux 7.0 on the reference host prints
`nr_descendants`, `nr_subsys_*` and the dying counts and no such line.

## 12. `cpu.max` and `cpu.stat` (S2, 2026-09-30)

`cpu.max` gives a job and everything beneath it a quota of processor time each
period. `sched::CpuQueue::account_in` charges each slice to the running task's
job and every job above it, and to the machine; a slice ended by a tick from
user mode is user time, any other system time (tick accounting). A job whose
use reaches its quota in a period is throttled: its tasks wait out the period
on the way back to user mode (`sched::throttle_current`, beside
`regroup_current`). A period starts with the first charge; the next is started
by whoever looks first. A task alone on a processor gets no tick, so
`arm_timer` also arms for the moment the quota would be used up.

`cpu.max` is written and read as `cpu_max_write` has it (`src/lib/fs/cgroupfs`
`cpu.rs`, with host tests and a fuzz round-trip). `cpu.weight.nice` maps to
`cpu.weight` through Linux's table. `cpu.stat` prints `usage_usec`,
`user_usec` and `system_usec` in every cgroup and the root (the machine's),
and `nr_periods`, `nr_throttled` and `throttled_usec` where `cpu` is enabled;
`nice_usec`, `core_sched.force_idle_usec` and the burst keys have no source.
The limit is in the audit trail as a cgroup limit (`resource::CPU_MAX`).
A throttled task sleeps to its period's end, in slices of a millisecond at each
of which it asks whether a kill, a signal or a stop is for it (a victim of
`SIGKILL`, `cgroup.kill` or the OOM killer is not held to the period's end), so
a raised quota lets it go at the end of that period. `cpu.max` throttles a
native task too, a ring-3 driver included. `cpu.idle` and `cpu.max.burst` are not built.

## 13. The `io` controller (B1, 2026-09-30)

`src/kernel/src/fs/blkio.rs`. The disk the block ring registers is wrapped, so
a mount's fill, a write-back and a raw read of the node are charged, to the
job of the task that submits them and each job above, and to the machine (the
root's `io.stat`). A partition is not wrapped: its reads reach the disk it is
on. `io.stat` prints the six counters per `MAJ:MIN`, and leaves out a disk the
cgroup never used; `io.max` takes `rbps`, `wbps`, `riops` and `wiops` as
`tg_set_limit` does (`ENODEV`, `EINVAL`, `ERANGE`), and the root has none.

A request waits in its submitter for the latest instant the limits on the way
up give it (`ferrix_block::Throttle`: one virtual clock per limit, no burst
beyond a request, host-tested) and pays them all that start. A job's entries
are one table keyed by its quota slot and dropped as the job goes
(`quota::on_job_release`), each charged to the job as kernel heap (F-37). The
block core's queue (`ferrix_block::Queue`) is not where this sits: a throttled
request has not been queued, so no barrier can wait for it.

`io.weight` and `io.latency` are not built: a weight needs a scheduler with
more than one request in flight to divide, and both would accept a value and do
nothing. With `io` built, `cgroup.controllers` at the root lists
`cpu io memory pids`, and the no-internal-process rule reaches `io`.

## 14. Where the controllers stand (2026-10-01)

Reclaim, freezing, `cpu.max` and `io` are on `main`. Each check boots on
x86-64, AArch64 and ARMv7-A at `--smp 2`; the `io` line had been written and
had never been booted before this landing, and was fixed once on the way (its
quota-slot count failed when an earlier check's killed program was reaped
between the two counts; it now waits for the count to settle). Booting it on
ARMv7-A also showed the `kmem` fill of files leaving 156 bytes charged: the
attempt that hits the limit leaves its name's dentry behind, which a sibling
opening the same name used to settle, and the check now makes each name before
it removes it.

Negative controls, twenty-three, each a one-line sabotage run through
`fleet/gate.sh control` on x86-64 (`dentry-keep` on ARMv7-A at `--smp 2`),
that stops the boot with the check's own message. Where the sabotage is in a
path that may run in task context it also prints `NEGATIVE CONTROL <name>`
once; the scheduler's tick path (`cpu-charge`) and the value-only ones show
the message alone. `freeze-post` and `freeze-born` drop a post of the
pending-work word (`sched::work`), so with the self-checks on they stop the
boot with FX-0520's message rather than the check's. `reclaim-hole` needs a
second, inert edit (a flag the source read looks at), made in a commit of its
own that never lands; it breaks the refill that a read and a fault share
(`Fill::sourced`), and shows it through the read path (`Vmo::read_present`).
The fault path's own half of L.object.112, `copy_or_zero` handing an evicted
frame back, has no control of its own. Their
INDEX tags are `po6-cgctl-ctl-<name>` on nazuna (2026-10-05), on the tip
rebased onto `main` 3349682db: cf400dff2 for most, 17acf31dc for
reclaim-sibling and the four other reclaim lines, after the reclaim check
changed (below).

| Control | Sabotage | Message |
|---|---|---|
| io-charge | a read counts 0 bytes | io.stat does not count what a cgroup read and wrote of a disk |
| io-parent | a charge stops at the job, not above it | a parent's io.max did not hold for its child's reads |
| io-throttle | the wait for `io.max` is dropped | four reads under io.max rbps=16384 were not spaced out to its rate |
| io-root | the machine's entry is not charged | the root's io.stat does not count the machine's I/O |
| io-limit | `io.max` forgets a written `rbps` | io.max does not read back what was written |
| io-ended | an entry is made for a job that has gone | a disk read by a task of a job that had gone kept the job's quota slot |
| cpu-throttle | a throttled task is never put to sleep | a program under cpu.max 20000 100000 was not held to about a fifth of a processor |
| cpu-kill | the throttle's wait never looks for a kill | a program throttled by cpu.max 1000 1000000 waited out its period to die of SIGKILL |
| cpu-charge | the scheduler charges no slice to a job | the root's cpu.stat does not have its six keys and the machine's usage |
| cpu-rearm-write | a `cpu.max` write arms no processor's timer | a running program was not held to a fifth of a processor by a cpu.max written under it |
| cpu-rearm-move | a move beneath a `cpu.max` arms no timer | a running program moved beneath a cpu.max was not held to a fifth of a processor |
| freeze-park | a frozen process does not park | cgroup.events never said frozen 1 for a frozen cgroup |
| freeze-sigcont | `SIGCONT` thaws the cgroup | SIGCONT thawed a frozen cgroup's process |
| freeze-post | a process moved into a frozen cgroup is posted no `STOP` | the way back to user mode found its pending-work word clear and something to act on (FX-0520) |
| freeze-born | a task launched into a frozen cgroup is posted no `STOP` | the way back to user mode found its pending-work word clear and something to act on (FX-0520) |
| freeze-moved-out | a parked process moved out of a frozen cgroup is not released | a parked program moved out of a frozen cgroup did not run again |
| park-poll | a parked thread looks again every 5 ms, not every hour | a frozen cgroup's threads were charged processor time while parked |
| reclaim-none | a reclaim at a job takes nothing | a cgroup over its memory.high was not brought back to it |
| reclaim-hole | a reclaimed page is not read again from its source | pages reclaim took did not read back as the source has them |
| resident | a disk file's cached pages are counted as `shmem` | memory.stat's file is not the cache that memory.current holds |
| reclaim-sibling | a reclaim takes from outside its subtree | a sibling cgroup's pages were reclaimed |
| reclaim-min | `memory.min` spares nothing | reclaim took pages from a child using no more than its memory.min |
| dentry-keep | a create the job's memory refused keeps its negative dentry | kmem: objects gone and their heap still charged to their job |
| cpu-arm-order | `arm_timer`'s `cpu.max` cut taken from `current`, which a switch stores after it arms (main's 2f, 445d09420) | a program under cpu.max 20000 100000 was not held to about a fifth of a processor |

**Where it stands (2026-10-05).** The controllers were built on 2026-09-30
and waited for their gate and controls at the 2026-10-01 wind-down. Rebased
onto `main` on 2026-10-05, they met main's pending-work word (`sched::work`):
the way back to user mode asks the personality only when a bit is posted, so
`freeze_sync` now posts `STOP` to a frozen process's tasks, `tell_a_new_task`
to a task launched into one, and the `cgroup.procs` write holds a posting
across the move. The consultant's follow-up found that a parked process moved
out of a frozen cgroup was not released (the move writes the freeze before
`freeze_sync` looks); it is now, and the `freeze` line starts a program in a
frozen cgroup, moves a running one in and a parked one out, and requires each
to park or run. reclaim-sibling had never fired, because the limited cgroup's
file was made first and met every want before a reclaim past its scope reached
the sibling's: the sibling's file is now made first, and asked about right
after each read. The check's lower `cpu.max` bound is a twelfth of a
processor, as the requirements say.

**Rebased again (2026-10-07, po10-cgctl).** On `main` 4466212c3 the `cpu`
line failed: two counting threads were held to 1911 and 1934 thousandths of
the wall clock on armv7a `--smp 2` (gate INDEX lines 11654, 11668) and the
same message stopped `test-vfs` on x86-64 KVM (11681), while po6's tip on the
old `main` passed the same armv7a boot under the same load (11666). Main's
2f made `switch_chosen` arm the timer before it stores its pick in
`current`, so the cut was armed for the task being left and a thread alone
on its processor was never cut. `arm_timer` now takes the task from the fair
class, the one `account_in` charges; the same boot then held the program
with 47 periods throttled. Those failing runs are the cpu-arm-order row's
control: the code before the fix is its sabotage. The fast path's three
exits now wait out a used quota as the general way out does; the boot with
`ferrix.fastpath=on` that would prove it is owed (BACKLOG, step 4's F9).
