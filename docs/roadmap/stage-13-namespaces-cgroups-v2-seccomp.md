# Stage 13 — Namespaces, cgroups v2, seccomp  ·  *month*

All eight namespaces, the unified hierarchy with `cpu`/`memory`/`io`/`pids`,
cgroupfs, and classic-BPF seccomp with the interpreter in `src/lib/`.

**Exit:** an unprivileged user namespace runs a process whose pid is 1 inside
it, under a memory limit that triggers scoped reclaim and a scoped OOM kill,
with a seccomp filter that blocks a syscall.

**Cgroups first (customer, 2026-09-23).** Stage 15's init is designed as if
cgroups exist (`docs/INIT.md`), so the stage's cgroup half is built before its
namespaces and seccomp. `docs/INIT.md` §0.1 lists what init needs from it, all
in Linux's own interface:
* cgroup2 mountable at `/sys/fs/cgroup`;
* `cgroup.procs`, with a forked child staying in its parent's cgroup;
* `clone3`'s `CLONE_INTO_CGROUP`;
* `cgroup.events` waking `poll` with `POLLPRI`;
* `cgroup.kill`;
* the four controllers' files;
* delegation by directory ownership.

Init's first boot needs only C1 to C5, the first five items; the controllers
can land one at a time, `memory` and `pids` first.

One requirement there is Ferrix's rather than Linux's, and the customer
accepted it on 2026-09-23: C8, **every cgroup backed by a `Job`**. It makes
`ARCHITECTURE.md` §3's "where resource limits and kill authority live" one
object seen through two ABIs. `devmgr`'s driver jobs then appear in cgroupfs,
and a microkernel keeps the jobs if it drops cgroupfs.

**Designed (2026-09-23): `docs/CGROUPS.md`.** Every process is in exactly
one job, and a fork inherits it. A job counts its live members, so
"populated" flips at the last exit, not the reap. cgroupfs is an in-kernel
view of the job tree, with its text formats in a pure `src/lib/fs/cgroupfs`.
`POLLPRI` is new to `poll`, `select` and `epoll` for `cgroup.events`. A job
asserts a native `EMPTY` signal, and memory is charged per page to a job,
with an OOM kill scoped to it.

Five landings, G1 to G5 (27 points), give init what it needs. The
controllers are 58 more: `pids` and `memory` first (with reclaim, which the
stage carries anyway), then freezing, `cpu` and `io`. That is 85 for the
cgroup half alone, against the whole stage's pre-points guess of a month
and 60.

**Done -- G1, every process in one job (2026-09-23, 8 points).** A process
holds its job, the root job's unless moved, and a fork's child starts in
its parent's. A job finds its members through the process registry and
counts them under one tree-wide lock. It is empty from its last member's
release, not its reap, and each flip wakes its event queue. `job_kill`
still seals the job, and `kill_members` (the coming `cgroup.kill`) leaves
it usable. A fork into a job either kill is working on is ended before it
runs. `devmgr`'s job is `drivers.slice` under the root. Two boot checks
cover this under the `jobs` line.

**Done -- G2, cgroupfs (2026-09-23, 8 points).** `mount -t cgroup2`
shows the job tree: a directory per job, named by `mkdir` or `job-<id>` for
one native `job_create` made, and Linux's eleven `cgroup.*` files, the four
Linux keeps off the root kept off it. `cgroup.procs` lists and moves,
`cgroup.kill` ends a subtree and leaves it usable, `cgroup.events` reads
`populated` exactly, `cgroup.max.depth`, `cgroup.max.descendants` and
`cgroup.stat` hold, and `/proc/<pid>/cgroup` says `0::/path`. Every text and
every write's parse is `src/lib/fs/cgroupfs`, pinned against Linux's by host tests,
Miri and a fuzzer (`cgroupfs_write`). The boot check drives it through the VFS
under the `cgroups` line.

**Done -- G3, `POLLPRI` (2026-09-24, 3 points).** `cgroup.events` wakes
`poll`, `ppoll`, `select` (its exception set) and `epoll` (`EPOLLPRI`) when
its cgroup fills or empties, as Linux's does: `POLLPRI`, with `POLLERR`, from
the change until the file is read again from its start. `Readiness` has a
`priority` that every other file leaves false. The `cgroups` boot check
puts `cgroup.events` in an epoll set and requires a waiting task to sleep
through one member's release and be woken by the job's queue at the last,
then `POLLPRI` once and not after a re-read; its negative control, the file
naming no queue, fails by the check's own message. Init's C4 is met.

**Done -- G4, `CLONE_INTO_CGROUP` and delegation (2026-09-24, 5 points).**
`clone3` starts a child in the cgroup a descriptor names, counted there from
the start, with Linux's checks and errnos. A cgroup's directory and files
each have an owner and a mode, so `chown` hands a subtree to a user, who may
move processes within it and not out of it: a move needs write access to
the common ancestor's `cgroup.procs`, judged as whoever opened the file, as
cgroup v2 judges it. A `mkdir` by a user gives it the new cgroup's files. A
removed cgroup refuses moves, `ENODEV`. The no-internal-process rule is
built and host-tested, and waits for a controller to be reachable. The
`cgroups` boot check covers `CLONE_INTO_CGROUP` from a program on all three
architectures and the delegation rules, and a `test-vfs` command runs them
as the user `ferrix` in a subtree `chown`ed to it; each has a negative
control. Init's C3 and C7 are met, so C1 to C5 and C7 are: nothing of stage
13 stands before init's first boot any more.

**Done -- G5, `EMPTY` and `job_for_cgroup` (2026-09-24, 3 points).** A job
asserts the native signal `EMPTY` while it is not populated, a level that
clears when a process arrives, and a port registration for it fires at the
flip that empties the job. `job_for_cgroup` (0x102A) gives a handle to the
job behind a cgroupfs directory descriptor, with `WAIT` for whoever may
read its `cgroup.procs` and `MANAGE` as well for whoever may write it; no
call goes the other way. `src/lib/proto/native` wraps it as `job::for_cgroup`. The
`cgroups` boot check asks for the handle as root and as uid 1000, and
requires a registration for `EMPTY` to stay quiet through one member's
release and fire at the last, with `cgroup.events` already `populated 0`;
its negative control, `notify` firing no registration, fails by the check's
own message. A native job made inside a cgroup shows as `job-<id>`, as
`devmgr`'s drivers do under `drivers.slice`. Init's C8 is met, so every
`Type=native` service init's L8 starts has what it waits on.

**Done -- P1, M1's charging and S1, the job quotas (2026-09-26).** Built as
the certification's F-35, the quotas its Security Target claims: every job
has a slot of counters in `object/quota.rs`, charged hierarchically for its
tasks, its programs' memory and page tables, and the native objects they
make, and a weight that scales its tasks' so its share of a processor no
longer grows with its task count. cgroupfs's `BUILT` is `cpu memory pids`:
`pids.max`, `pids.current`, `pids.events`, `memory.max`, `memory.current`,
`memory.events` and `cpu.weight` read and write the same slot a native
`job_set_limit` and `job_get_quota` do. The `quota` boot line refuses a
fork loop at 8 tasks and faults at 48 pages with a sibling going on, and
keeps one task alone in its job at 50% of a processor against eight in
another; the `cgroups` line drives the files, and `test-vfs` forks until
`pids.max` refuses. `docs/CGROUPS.md` §7.1 says where it differs from the
plan.

**Done -- M1's scoped OOM kill (2026-09-26).** A program's page fault past
its cgroup's `memory.max` kills, as `SIGKILL`, the process with the most
resident memory in that cgroup, never one outside it, and `memory.events`
counts `oom` and `oom_kill` and polls `POLLPRI`; a charge a system call
makes for an object is still refused `ENOMEM`. The `cgroups` boot line and
`test-vfs` command 22 prove it; `docs/CGROUPS.md` §7.1 has the semantics.

**Designed -- user and mount namespaces, for Steam (2026-09-28).** The
customer decided to build the two namespaces Steam's requirements check and
its pressure-vessel container need: `docs/NAMESPACES.md`, reviewed by the
certification consultant. Landings N1 to N6 and NP, 39 points, end with
`test-steam-bootstrap` passing the check as uid 1000; pid, network, IPC,
UTS and cgroup namespaces, `setns` and seccomp stay out of it.

**Done -- N1, per-mount flags (2026-09-28, 5 points).** `ro`, `nosuid`,
`nodev` and `noexec` are a mount's own and enforced -- `EROFS` for every
change through a read-only mount, `EACCES` for a device on a `nodev` one and
a program on a `noexec` one, `EPERM` for its executable mapping, set-id bits
ignored under `nosuid`; the memory filesystems mount read-only; `MS_REMOUNT`
and `MS_REMOUNT | MS_BIND` change a mount's flags, writing its filesystem
out before it goes read-only, so init's shutdown remount of `/` and `/data`
now happens (F-53); `/proc/<pid>/mountinfo` exists, and `/proc/mounts` and
`statfs` show the flags. The `mounts` boot line (FX-0885) and `test-init`'s
shutdown lines prove it.

**Done -- N2, binds (2026-09-30, 6 points).** `MS_BIND` mounts a
directory, a subdirectory, a file or a socket on a place of the same kind
(`ENOTDIR` otherwise), with the source mount's flags; `MS_REC` copies the
mounts below it too. `umount2` with `MNT_DETACH` takes a subtree at once,
each mount of it left without a parent, so `..` from inside stops at its
root and a bind from it is `EINVAL`. `MS_PRIVATE`, `MS_SLAVE` and
`MS_UNBINDABLE` are accepted and change nothing, since no mount is ever
shared; `MS_SHARED` and `MS_MOVE` stay `EINVAL`. The binds of a filesystem
share its superblock: a plain `MS_REMOUNT` read-only reaches every bind of
it, and `MS_REMOUNT | MS_BIND` only the one mount, as the interim reviewer
asked. Mount ids, the dentry cache and the rename lock are the kernel's,
shared by every namespace to come; a mount's parent can change, under the
table lock. A mount point reached through another bind can be neither
removed nor renamed (`EBUSY`), and `MNT_DETACH` writes out every
filesystem of the subtree first. The `binds` boot line (FX-0886), seven
host tests and the
`vfs_ops` fuzzer's bind, detach and remount operations prove it. Next is
N3, mount namespaces.

**Still to do:** `memory.stat`'s other keys, and a charge past `memory.max`
reclaiming inside the job before it OOM-kills (M2), then freezing,
`cpu.max` and `io`. `docs/CGROUPS.md` §7.1 says where each
starts in the code, how landings are gated now, and what cost a gate on
2026-09-23.

---

