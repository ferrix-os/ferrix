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

**Done -- seccomp S1 and S2 (2026-10-01).** S1 is the verifier and
interpreter, `src/lib/kernel/seccomp`. S2 is the hook: the core's four
system call entries ask a registered filter about every call first, before
their own early answers (`arch_prctl`, `set_tls`, the signal returns) and
before the native range is split, with the entry's own `arch` token and the
instruction after the call. The filter can fail a call with an errno or let it
go on and nothing else: its `Verdict` has no variant that carries an
`Outcome`, and the core cuts an errno to 4095 (the consultant's review of the
first form). It answers `Continue` for every program until S3; the `seccomp`
boot line (FX-1302) drives each entry with frames of its own to prove the
filter is asked once, first, and as the entry's own, and a second line reads
the cost: 15 to 38 ns a call for the hook, and 26 to 60 ns for one interpreted
filter instruction, so at most 0.8 to 2.0 ms for the longest chain
(`docs/certification/MEMORY-AND-TIMING.md` §2.2b). Gated on the final tree:
`cargo xtask check`, the three boots, `test-threads`, `test-init` and
`test-shell` on all three architectures, and nine negative controls that each
stopped the boot with the check's own message (`docs/SECCOMP.md` §12).
ARMv7-A has its first requirements (`L.armv7a.1`, `L.armv7a.2`).

**Done -- seccomp S3, filters (2026-10-01, 6 points).** `seccomp(2)` and
`prctl(PR_SET_SECCOMP)` install classic-BPF filters per thread: `ALLOW`,
`ERRNO`, `KILL_THREAD`, `KILL_PROCESS`, `LOG`, strict mode, `TRACE` and
`USER_NOTIF` as `ENOSYS`, the strictest answer of a chain winning, inherited by
fork, clone and a native child and kept by `execve`, charged to the job
(F-37), bounded at 32,768 instructions, released by a walk, shown in
`/proc/<pid>/status`. **This meets the stage's exit clause "a seccomp filter
that blocks a syscall".** The `seccomp` boot line (FX-1303) installs filters
as a program does in a real thread of a check process and makes the calls
through the core's own entry; thirteen negative controls each stopped the
boot with the check's own message (`docs/SECCOMP.md` §12). `TRAP` (S4),
`TSYNC` (S5) and the guest test (S6) follow.

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

**Where stage 13 stands (wind-down, 2026-10-01 evening).** The takeover guide, with every branch's tip, worktree and what it owes, is [stage-13-handover.md](stage-13-handover.md). The exit criterion
-- an unprivileged user namespace runs a pid 1 under a memory limit with a
scoped OOM kill and a seccomp filter that blocks a call -- is **not met**.
On `main`: N4 user namespaces, S1 the filter checker, S2 the core hook, the
small namespaces and `setns`, pid namespaces, and the reservation of
`L.trap.8` for S4. Everything else is pushed to `origin` as a branch, gated as
the table says, and stopped at the customer's wind-down. Evidence is cited by
fleet/gate.sh INDEX tags on nazuna; the certification consultant's verdicts and
conditions are in `~/.local/share/ferrix/cert-consultant/reviews.md` there.

| Branch (origin) | What | State at the wind-down |
|---|---|---|
| `stage13-cgctl` 670b4c49a | M2's reclaim and `memory.high`, `cgroup.freeze`, `cpu.max`, the `io` controller | every gate row PASSED on 89c2f911a (`cs-*`, kvm and release included); 8 of 18 controls FIRED, 11 not run; consultant: fixes accepted, but it has not seen the `cpu.max` bound widened to two thirds of a processor or the stale CGROUPS §14 control table |
| `stage13-s3` d19606300, `stage13-s3-onmain` 8ce395f53 | seccomp filters (`SECCOMP_SET_MODE_FILTER`, strict mode, the actions) | check and three boots PASSED on 6ec5b4081; controls k1-k5, k11-k15 FIRED; owed: k6-k10, k16-k19, `test-shell`, `test-vfs`, and the rows on the rebased hash. Cleared on that evidence |
| `stage13-s4` 9ad2c718e | `SECCOMP_RET_TRAP` | consultant: OK if five conditions; three written; controls t2-t7 and the rows owed; lands after S3 |
| `stage13-s5` 298399f1e | `TSYNC` | consultant: OK if; a new thread fails closed (written); owed: a measured bound for the TSYNC ancestor walk, the rows, four controls |
| `stage13-s6` 64e891f7e | `cargo xtask test-seccomp` (S6a) | passed on all four ABIs in a direct run; pool INDEX lines owed; Linux's `seccomp_bpf` selftest not done |
| `stage13-fdinfo` 225eb2410 | `/proc` by `ptrace_may_access`, dumpable, `/proc/<pid>/fdinfo` (NP) | check, three boots (armv7a at two and four processors), `test-init`, `test-shell` PASSED (`fdi23-*`); controls k1-k3, k6, k7 FIRED; consultant: OK if `test-vfs`, k4/k5 on their own message, k8-k17 on the landing hash, §12 quoting them |
| `stage13-n5` 7a0c04fdd | unprivileged mounting | on fdinfo; consultant asked for changes, built (remount by the superblock's owner, the bottom mount locked, a sleeping write-out), not gated; earlier tip's boots, `test-vfs`, `test-shell`, `test-bwrap` PASSED |
| `stage13-bwrap-user` dad30e7f2 | `test-bwrap` as uid 1000 | passes (a new tmpfs is now 1777 and the mounter's); needs a rebase onto N5, a control for the tmpfs case, review |
| `stage13-netns` 4cc2ab39d | network namespaces, veth, per-namespace stacks | every row PASSED on 5e19c599c (`ae23-*`), controls ae-nn-01..31 FIRED; cleared by the consultant if `main` changed nothing under `src/` since 75c03a259; owed: the rebase, coverage carry, landing |
| `stage13-timens` 88b073c3a | time namespaces | on netns; boots on x86_64; native child gets its creator's time namespace (c25, c26); gate and 26 controls stopped mid-run (c01, c03-c07 FIRED); not reviewed |
| `stage13-container` cda83faef | `cargo xtask test-container`, the exit criterion as a program | written, never built or run; needs cgctl and S3 on `main` |

Every namespace and seccomp review found the same hole: `launch::load_native`
must give a native child its creator's whole namespace set, pid namespace,
seccomp chain and `no_new_privs`. Each branch above carries that fix and its
check. Not started: N6 and N7 (Steam as uid 1000, pressure-vessel). Rough
remaining size: about 40 to 45 points, about 5 to 7 hours with four landing
chains and a consultant at once.

**Landed -- the small namespaces and `setns` (built 2026-09-30, landed
2026-10-01 after the consultant's review):** UTS, IPC and cgroup
namespaces through `clone`, `clone3` and `unshare`; `sethostname`,
`setdomainname`, `uname` and the two sysctls per UTS namespace; the
System V semaphore table per IPC namespace; `/proc/<pid>/cgroup` told from
the reader's cgroup namespace root and a `cgroup2` mount rooted there;
`/proc/<pid>/ns/{uts,ipc,cgroup}`, all five `ns` links opening as nsfs files
with `NS_GET_USERNS`, `NS_GET_PARENT`, `NS_GET_NSTYPE` and `NS_GET_OWNER_UID`;
`setns` by namespace file or pidfd for mount, user, UTS, IPC and cgroup
namespaces. Network namespaces stay `EINVAL`; pid ones landed after (below). The `smallns` boot
line (FX-0892) and its negative controls prove it; `docs/NAMESPACES.md` §12
has the list and the differences from Linux (a namespace set per process,
not per thread, among them). The review found that a native `process_create`
child left its creator's UTS, IPC and cgroup namespaces and that a cgroup move
was judged in the writer's namespace, not the opener's (and not at all for
`CLONE_INTO_CGROUP` and `job_for_cgroup`); all are fixed with checks and
controls (NAMESPACES §12). Later namespace landings extend `launch::load_native`'s
proxy copy and its check.

**Done -- pid namespaces (2026-10-01; `docs/PIDNS.md`):** `CLONE_NEWPID` through `clone`, `clone3` and
`unshare`, so an unprivileged user namespace can run a process that is pid
1 inside it -- the pid-1 part of the exit criterion. A task in a namespace
below the first has a number in each level; the kernel number stays the key
of the registry, the job tree, groups and sessions. Every call that names or
reports a pid speaks the caller's namespace, orphans go to their own
namespace's init, an init's end ends its namespace, and an init ignores
what it has no handler for except `SIGKILL` and `SIGSTOP` from an ancestor.
procfs, `cgroup.procs`, `si_pid`, `SO_PEERCRED` and the terminal's groups
follow. The `pidns` boot line (FX-0891) and a pid-namespace fill in `kmem`
prove it, each rule with a negative control; `PIDNS.md` §8 lists where it
differs from Linux.

**Still to do:** `memory.stat`'s other keys, and a charge past `memory.max`
reclaiming inside the job before it OOM-kills (M2), then freezing,
`cpu.max` and `io`. `docs/CGROUPS.md` §7.1 says where each
starts in the code, how landings are gated now, and what cost a gate on
2026-09-23.

---

