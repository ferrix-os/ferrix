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

**Designed -- seccomp-bpf (2026-09-30), for review:** `docs/SECCOMP.md`,
S1 to S6 (20 points); this stage's "a seccomp filter that blocks a
syscall" is met at S3.

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
`vfs_ops` fuzzer's bind, detach and remount operations prove it.

**N3, mount namespaces (2026-09-30):** `unshare` and `clone` with
`CLONE_NEWNS` copy the caller's namespace, every mount new and charged to
its job; a walk crosses a mount point in the table of the mount it is on;
`pivot_root` is Linux's, with `chroot_fs_refs`; a native child starts in its
creator's namespace; `/proc/<pid>/ns/mnt` and `/proc/sys/fs/mount-max`
exist. bubblewrap 0.12 needs `openat2` with `RESOLVE_IN_ROOT`, so N3
answers it, every resolve flag. The `mntns` boot line (FX-0887) proves it
on all three architectures, with five negative controls, and `cargo xtask
test-bwrap` runs Debian's bubblewrap as root: Steam's requirements check's
four argument lists and a pressure-vessel-shaped container. Every
namespace now stands on an empty bottom mount, as a booted Linux machine's
`/` does, so `pivot_root` works from `/` in memory too: Steam's
requirements check exits 0 as root in `test-steam-bootstrap`.

**Done -- N4, user namespaces (2026-10-01):** `CLONE_NEWUSER` through
`clone`, `clone3` and `unshare`; `/proc/<pid>/uid_map`, `gid_map`, `setgroups`
and `ns/user`, each map written once and judged against its opener and its
writer; capability sets, `capget`, `capset` and `PR_CAPBSET_*`;
`privileged()` true in the first namespace only (U1); a child namespace
honours five capabilities (U8); `execve` ignores set-id bits there (U7);
`chroot` by `CAP_SYS_CHROOT`; nesting 32 deep, then `EUSERS`. Every site that
reports an id tells it as the reader's namespace names it or 65534, and every
one that takes an id refuses an unmapped one, `si_uid` among them (it was never
filled before). The `userns` boot line (FX-0888) and `kmem`'s fill for user
namespaces prove it on all three architectures, with ten negative controls
that each stop the boot with their own message (`docs/NAMESPACES.md` §12 lists
them). The consultant's review of the landed diff is still owed; his six
conditions on the core are built.

**Done -- S1, seccomp's verifier and interpreter (2026-10-01):**
`src/lib/kernel/seccomp` is Linux's `bpf_check_classic` and
`seccomp_check_filter` rule for rule, the classic machine, the action order and
`run_all`, with `forbid(unsafe_code)`. Evidence: 23 host tests, one per rule
and opcode; a second, independent checker held to `verify` over 300,000 seeded
programs; three of Chrome 151's filters run against what a Linux 7.0 kernel did
with calls 0 to 449; a fuzz target; Miri in CI. S2 to S6 (the core hook,
filters, `TRAP`, `TSYNC`, the guest test) are designed in `docs/SECCOMP.md`
and not landed.

**Done -- S1's review conditions on the crate (2026-10-01):** the consultant's
after-the-fact review of S1 found `MAX_INSNS_PER_PATH` eight times too large, a
scratch-word rule that carried nothing from a `RET` (Linux carries its running
set on), and `run_all` of no filters answering `ALLOW`. The path limit is
Linux's 32768 with `fits_path`/`path_cost` and a host test of both ends,
`check_scratch` is `check_load_and_stores` line for line, held with the naive
checker to six programs carrying the real Linux 7.0 verifier's answer
(`oracle.c --accept`), and an empty chain answers `KILL_PROCESS`.

**Where stage 13 stands (2026-10-01).** The exit criterion -- an unprivileged
user namespace runs a pid 1 under a memory limit with a scoped OOM kill and a
seccomp filter that blocks a call -- is **not met**: pid namespaces and the
filter itself are built on branches and not on `main`. Built, gated on their
own boots, reviewed by nobody yet, and **not landed** (each on a local branch
with a side ref on nazuna; `docs/BACKLOG.md` lists their differences from
Linux):

| Branch | What | State |
|---|---|---|
| `stage13-np`, `stage13-fdinfo` | `/proc`'s private links by `ptrace_may_access`, dumpable cleared by id changes, `/proc/<pid>/fdinfo` | boot check written; last run failed on a real bug now fixed, not re-run. Overlaps `main`'s own `credentials_changed` |
| `stage13-n5` | unprivileged mounting: `may_mount` by owner, `tmpfs` alone, locked copies, detach-don't-pin, sysctls | `mountperm` line booted on x86_64, four controls fired; other gates not run |
| `stage13-bwrap-user` | `test-bwrap` as uid 1000 | its one run exited 1 near the `threads` line; cause not read |
| `stage13-smallns` | UTS, IPC, cgroup namespaces, nsfs, `setns`, pidfd `setns` | boots on three architectures, 55 controls; `check`, `test-init`, coverage not run |
| `stage13-netns` | network namespaces, veth pairs, per-namespace stacks | boots on three architectures, `test-shell`, `test-vfs`, `test-net`, 30 controls; `check` not run |
| `stage13-timens` | time namespace | agent had not reported |
| `stage13-pidns` | pid namespaces | boots pass; **`test-vfs` fails on x86_64**: a kernel stack overflow on the `ioctl` path, cause not found (`Process` grew by about 48 bytes) |
| `stage13-cgctl` | M2's reclaim and `memory.high`, `cgroup.freeze`, `cpu.max` with `cpu.stat`, the `io` controller (`io.stat`, `io.max`) | reclaim, freeze and cpu booted; the io check stopped at its last line (a quota-slot count, a fix written, not booted); no full boot, no `test-shell`/`test-vfs`, no negative control run |
| `stage13-s2` | seccomp S2 to S5 | in progress |
| `stage13-container` | `cargo xtask test-container`, the exit criterion as a program | written, never run |

These branches were written against an earlier N4 and conflict with each other
in the namespace, procfs, catalog and kmem files; they go in one at a time,
each rebased with `git rebase --onto` the landed N4. Not started: seccomp S6,
and N6 and N7 (Steam as uid 1000, pressure-vessel).

**Done -- M2, F1, S2 and B1, the rest of the controllers (2026-09-30).** A
charge past `memory.max` reclaims inside the job before it kills: the clean
page cache of files on a read-only disk mount, charged to the job and never a
sibling's, goes back the way a truncation's pages do and is read again from
its source; a job over `memory.high` is brought down to it; `memory.min` and
`memory.low` spare a child. `memory.stat` prints `file`, `kernel`, `shmem`,
`pgscan`, `pgsteal`, `pgfault` and `pgmajfault`, and `memory.events` counts
`high`. `cgroup.freeze` stops every process in a subtree where a stop would
but no signal undoes, and `cgroup.events` says `frozen`. `cpu.max` throttles a
subtree to a quota a period, `cpu.stat` and `cpu.weight.nice` exist, and the
`io` controller counts a cgroup's reads and writes of each disk in `io.stat`
and spaces them out to `io.max`. `cgroup.controllers` now lists
`cpu io memory pids`. The `cgroups` boot line has a `reclaim`, a `freeze`, a
`cpu` and an `io` line under it, each with negative controls, and
`test-vfs` has a command for `cgroup.freeze` and one for `cpu.max`.
`docs/CGROUPS.md` §10 to §13 say what each is and what it leaves out.

**Still to do:** reclaim of a writable btrfs mount's clean pages, of the
dentry and inode caches, and `memory.reclaim`; `io.weight` and `io.latency`;
`cpu.idle` and `cpu.max.burst`; `anon` and `pagetables` in `memory.stat`.
`docs/CGROUPS.md` §7.1 says how landings are gated now, and what cost a gate
on 2026-09-23.

---

