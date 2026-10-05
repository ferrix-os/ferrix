# Stage 15 — A real userland  ·  *week*

Static musl busybox as `/bin`, a working init, job control, ttys, pipes. Enough
of a system to be used rather than demonstrated.

**Exit:** an interactive shell over the serial console that a person can use.

**Most of this stage arrived early, under other stages' names.** `/bin` is
the uutils family and zinc rather than busybox (`docs/UUTILS.md`), pipes and
the pseudo-terminals are stage 8's and stage 18's, and the console's line
discipline -- `ICANON`, `ISIG`, `ECHO`, the signal characters -- has been
honoured since stage 8, over a receive interrupt since the UART drivers
gained one. What was left when this stage was looked at properly on
2026-09-19 was not the kernel's at all: every system call job control is made
of had been answered since stage 7 and nothing in user space used them.

**Done -- job control, and a gate that types (2026-09-19).** `src/user/system/linux/zinc/src/jobs.rs`
is the shell's half of what the kernel already offered. A pipeline is one
process group, so `kill %1` and the terminal's Ctrl-C reach all of it; a
foreground job is handed the terminal with `tcsetpgrp` and the shell takes it
back when the job ends or stops; a job stopped by Ctrl-Z stays in a table
that `jobs`, `fg`, `bg`, `wait`, `disown` and `kill %1` name, with zsh's `%+`,
`%-`, `%n`, `%name` and `%?text` specifications and zsh's lines
(`[1]  + suspended  sleep 30`). The shell steals the terminal before its
first prompt, stopping its own group until it has it, as the glibc manual's
job-control shell does -- a shell started in the background is otherwise
stopped by `SIGTTOU` at its first prompt with nothing on the screen to say
why. `exit` with a job suspended is refused once and obeyed the second time.
Before this the shipped shell answered `fg` with *no job control in this
shell*.

Two gates, because a process group is invisible in a transcript.
`src/user/system/linux/zinc/tests/pty_jobs.py`, in `cargo xtask check --zinc`, drives the shell on
a host pseudo-terminal and reads the process groups themselves out of
`/proc/<pid>/stat`: that the job's group is not the shell's, that a job's own
children share it, and that the terminal's foreground group is the job's
while it runs and the shell's again afterwards. Its negative control -- the
same shell with the terminal handover taken out -- fails ten of its checks.
`cargo xtask test-jobs` is the stage's exit read literally: it boots zinc as
init on x86-64 with no script, so the kernel starts `sh -i`, and types a
session at the serial port through QEMU's stdin -- `sleep 30 &`, `jobs`,
`kill %1`, a pipeline, Ctrl-Z, `bg`, `fg`, Ctrl-C, `exit` -- requiring the
answer to each keystroke. `qemu::Watching::type_in` is the new half of the
harness: until now every gate here only read what the guest said.

**Done -- the kernel's half of starting an init (L3, 2026-09-24).**
`ferrix.init=<path>` on the kernel command line starts pid 1 from that file
in the switched root, a `#!` script under its interpreter, and falls back to
the built-in program with one line saying why when the file will not start;
xtask's `--init-path` writes it into `CMDLINE.TXT`. `reboot(2)` commits `/`
and `/data` before it stops the machine, as the kernel's own power-off does,
and `ferrix.onexit=panic` gives Linux's answer to init exiting (`FX-1501`).
`cargo xtask test-shell` boots its shell a second time from
`ferrix.init=/etc/shell-test`, and under busybox writes `/data/k7`, powers
off with `poweroff -f -n` and reads the file back on a second boot of the
same volume. `docs/INIT.md` §16 has the details and the negative controls.

**Done -- L1, the unit files (2026-09-24, 5 points).** `src/lib/init/svc` is the
manager's pure core, `no_std` so that `devmgr` can share its restart
policy later. Its first landing reads units as systemd does: the INI
subset with `conf-parser.c`'s corners, the three layered directories as a
source the backend fills, drop-ins in name order with a higher directory
hiding a lower one's file of the same name, masking by `/dev/null` or an
empty file, aliases, templates and their specifiers, `.wants/` links, and
the keys of every version-1 kind with systemd's value syntaxes. A key init
does not know is a warning, so a systemd unit file loads. 52 host tests,
Miri, and the `svc_unit` fuzz target. `docs/INIT.md` §16 records what
the building changed in the design.

**Done -- L2, the manager (2026-09-24, 8 points).** The rest of
`src/lib/init/svc`: `Manager::step(event, now) -> actions` and `deadline()`, as
`docs/INIT.md` §3 has them. Requests become transactions of operations
along systemd's dependencies, with its conflict rules, a `Wants=` cycle
broken with a warning and a `Requires=` cycle refused; what is not ordered
starts in the same step. It builds the slice tree with its limits, runs
services through systemd's states for `simple`, `exec`, `oneshot` and
`forking`, stops them by `KillMode=` with `cgroup.kill` after
`TimeoutStopSec=`, counts a service stopped only when its cgroup is
empty, restarts by `Restart=` with a doubling backoff and the start limit,
boots `default.target` with `rescue.target` as the fallback, and shuts
down in reverse order before killing every cgroup left. 92 host tests
replay event scripts; Miri runs them, and the `svc_manager` fuzz target
drives the manager with events in any order. The core never holds a
handle: every action names a `UnitId`, a `GroupPath`, a `Token` or a
`ClientId`, for the init program's backends to map (`docs/INIT.md` §16).

**Done -- L4, the init program (2026-09-26, 10 points).** `src/user/system/linux/init/` is a
workspace beside zinc's, built the same way for all three architectures.
`/sbin/init` is pid 1 around `src/lib/init/svc`'s manager: it mounts `/run` and
cgroup2, moves itself into `init.scope`, runs the generators, and waits in
one `epoll_wait` on a signalfd, each child's exec report and each cgroup's
`cgroup.events`, turning what it finds into the manager's events and its
actions into system calls. A service starts in its own cgroup by
`clone3(CLONE_INTO_CGROUP)` and leads its own session; it is stopped by
`KillMode=` and counted stopped when its cgroup is empty; `SIGTERM` to pid 1
stops everything in reverse order and powers off through `reboot(2)`.
`/sbin/getty` gives a terminal a session and a login shell, and the getty
generator links one into `multi-user.target` for the console. The kernel's
`/proc/<pid>/stat` now reports a process's group, session, controlling
terminal and foreground group, which it had written as the pid, 0 and -1.
`cargo xtask test-init` is `docs/INIT.md` §15's stages one and two: it types
at the getty's shell and requires its own session and terminal from
`/proc/self/stat`, a failing service's restart budget, a daemon's grandchild
in its service's `cgroup.procs` and gone when the service stops, the
shutdown order, and a clean `btrfs check` afterwards. Both negative controls
fired. `docs/INIT.md` §16 has the details.

**Done -- L5, L6, L7 and L9 (2026-09-26, 19 points), and L8's kernel half.**
`/bin/svc` over `/run/ferrix/control` with systemctl's verbs, root-only
changes judged by `SO_PEERCRED`, and a per-unit log of what services print
(L6); `Type=notify` readiness over `NotifyFd=` and `forking` services by
their `PIDFile=` (L7); `.socket` units with `LISTEN_FDS` for `Accept=no` and
an instance per connection for `Accept=yes` (L9); and stage 13's scoped OOM
kill with init's `OOMPolicy=` on it, `Delegate=`, and scopes (L5). The kernel
calls L8 needs -- pid 1's bootstrap channel, `process_give` and
`process_bootstrap`, `process_status`, `port_fd` -- are in. `test-init`
types every one of these at the prompt on all three architectures;
`docs/INIT.md` §16 has what each stage requires and its negative control.

**Done -- L8, the directory (2026-09-26, 16 points).** Pid 1 reads the
kernel's hello on its bootstrap channel, gives every service that declares
`Uses=` or `Offers=` a channel of its own before it runs, starts `Type=native`
services with `process_create` in the job behind their cgroup, and routes an
OPEN to the unit that offers the name -- starting it first -- or refuses one
the asker did not declare. `test-init` shows a native service started by the
first OPEN and answering down the routed channel, and a refusal.

**Done -- L10, the images boot init (2026-09-26, 6 points).** `cargo xtask
run` boots `/sbin/init` with a getty and zinc on the console, and every
desktop image -- `run-compositor`, the compositor's gates and a board's card
-- boots init with hyprix as `hyprix.service` under `graphical.target`,
each program it starts in a scope of its own. `test-jobs` types its session
at the getty's shell.

**Done -- L11, `devmgr` on the restart policy (2026-09-26, 2 points).** The
policy is its own crate, `src/lib/init/restart`, which allocates nothing so
that `devmgr` can link it, and `src/lib/init/svc` re-exports it; its start limit
is systemd's fixed window. `devmgr` restarts a driver by it and reports how
each driver died through K6.

**The init is done: L1 to L11 (the customer, 2026-09-26).** The stage stays
in progress until authentication's phase 1 lands.

**Done -- L12, pid 1 starts `devmgr` (2026-09-27, 12 points).** Every image
that boots init sets `ferrix.devmgr=init`: the kernel gives pid 1 a one-shot
starter, and init runs `devmgr.service` by asking the kernel to start
`devmgr` in its cgroup's job, getting back only a handle to the process --
the device authority never passes through init. `/` switches to the root
disk once `devmgr` has reported, moving pid 1 with it, and init then boots
`default.target` from the volume. `test-init` runs it with a root disk on
all three architectures, including `svc restart devmgr.service`. Those
boots skip the kernel's disk checks of stages 10 to 12, which is why the
option is outside the certified configuration; the default boots keep them
(`docs/certification/ITEM.md` §5).

**Done -- sshd under socket activation (2026-09-27).** The sshdt port takes
its listening socket from init (`LISTEN_FDS`), and `test-init` on x86-64
has the first connection to `sshd.socket` start `sshd.service` and be
answered with its banner.

**Landed for the init (2026-10-04, from branch `l13-init`): L13a**, the sandboxing
keys (`docs/INIT.md` §4.5). All five are read; `NoNewPrivileges=`,
`PrivateTmp=` and `ProtectSystem=` are carried out in each service's own
mount namespace, gated by `test-init`'s sandboxing stage on all three
architectures, with four negative controls fired. Init is outside the
certified item.

**Built for the init (2026-10-05, branch `l13b` on `main`): L13b**,
`PrivateNetwork=`, a network namespace with only `lo`, up (`docs/INIT.md`
§4.5), gated by `test-init`'s sandboxing stage.

**Built for the init (2026-10-05, branch `l13c` on `l13b`): L13c**,
`SystemCallFilter=` with `SystemCallErrorNumber=` and
`SystemCallArchitectures=`, compiled by init to a seccomp filter per ABI
over S3 and installed as the child's last step (`docs/INIT.md` §4.5), gated
by `test-init`'s sandboxing stage on all three architectures, with six
negative controls fired. With it every key of L13 is carried out.

**Designed (2026-09-23): `docs/INIT.md`.** `/sbin/init` is pid 1 and a
service manager in one program. Its units are in systemd's syntax, with
slices, scopes, templates, generators and socket activation, and each service
runs in a cgroup of its own. Its manager is a pure state machine in
`src/lib/init/svc`, and every effect goes through a backend that a microkernel could
serve instead. Init hands each service a bootstrap channel and routes named
native services between them.

Init waited on stage 13's cgroups, which the customer put first; what its
first boot needs of them (C1 to C5 and C7) landed on 2026-09-24. Its landings
come to 67 points up to hyprix no longer being pid 1, all of them spent
(L1 to L10): six kernel items of 11 points besides stage 13, two of them
(K0, K7) built, and `cargo xtask test-init` growing a stage per landing. Its
first two landings, the unit parser and the dependency engine, were
host-only and were built while stage 13 was.

**Where it stands.** Init is pid 1 of every image a person boots, and a
service manager they drive, with the directory native services are reached
through (L1 to L11), gated by `cargo xtask test-init` on all three
architectures, `test-compositor` and `test-jobs`. The
stage's exit has been met by `test-jobs` since 2026-09-19. What keeps the
stage open is the rest of authentication's phase 2 (below), L13, and the
apps' tail (`docs/APPS.md` §10).

**Authentication (`docs/AUTH.md`, approved by the customer on 2026-09-26,
all eleven decisions as recommended).** `authd` checks a person's password
with Argon2id, throttles and audits.

*Phase 1, done (27 points).* `authd`, `passwd` and `authctl`, gated by
`cargo xtask test-auth` on all three architectures (2026-09-27), and
hyprlock over `authd` (P1.5, 2026-10-03).

*Phase 2, the desktop as a user: done but for three items.* All but the
console revoke on 2026-10-03, each with the certification consultant's OK:
`--everything`'s desktop runs as `ferrix` under `sessiond`, with its own
home disk (P2.4); hyprix unlocks only on `authd`'s grant over the seat
channel, and a new locker takes over a dead one's lock (P2.5); `login` on
the console, with a first password chosen there (P2.3, `cargo xtask run
--login`); a session ends with its compositor, at the console's login
(P2.7); `su` for `wheel` (P2.6); and `/dev/tty` is the caller's own
terminal, not the console to anyone. On 2026-10-04 getty revokes the
console before every login (0f94a6d1a, in batch 22384874f; consultant OK at
ledger line 341): every open of the console made before `vhangup` reads
nothing more, a waiting read is woken with `EIO`, only root may take the
console from a live session, and `login` stops the console's last session;
a left-over program can still write to the console (`docs/AUTH.md` §1).
Left: K-B and K-C in the kernel (P2.1, P2.2); the desktop images other than
`--everything`, which still run as root; and the customer's decision on
ending a user's processes at logout (`docs/BACKLOG.md`).

P2.1 (K-B) is NAMESPACES' NP, **landed 2026-10-04** (7e9a2806f; the text that follows is as written on branch `np-land` before it landed, so its "owes" is met or in the product owner's record):
`stage13-fdinfo` rebased onto main 22384874f and squashed into one commit
(1373cf7ca), with `ns/net` brought under the same `ptrace_may_access` rule
(netns had landed it with the old one). Its x86-64 boot passes with the
`procacc` and `netns` lines; controls c02, c03, c04, c06 and c12 FIRED on
1373cf7ca. It owes `cargo xtask check` and controls c01, c05 and c07 to c13
on its landing hash, then a batch with `test-init --arch all`, `test-shell`
and `test-vfs`; the certification consultant's OK IF is at ledger lines 226,
338 and 342, and the steps are in `docs/handover/2026-10-04-np.md` on the
branch. P2.2 (K-C, freed socket, pipe and tty buffers zeroed) is not
started.

*Phase 3* adds PAM for ferrousli's programs, TOTP, ssh passwords and
privilege prompts (about 32 points), not started.

Its P0 was a kernel hole it named:
`process_create` gave the process it made root's credentials instead of
its creator's. Fixed on 2026-09-26: the child takes a copy of its
creator's ids, and the `creator` boot line proves it for a uid-1000 service
in its delegated cgroup (2 points, spent). Beside it: P0b, a `Type=native`
service with `User=` made by a helper that has become the user, so it runs
as the user (P0a refused such a unit until then), and F-40, a delegated
cgroup lifting its own limits, closed by a `SET_LIMIT` right and the
`limits` boot line.

`test-jobs` is x86-64 only, because `sleep` is uutils' and uutils is built
for x86-64 alone (`docs/UUTILS.md` D3).

**Done -- apps, each in a folder of its own (2026-09-30).** `docs/APPS.md`,
phase 1. An optional program is a folder under `src/user/apps/` with an
`app.toml`, and xtask finds it: `cargo xtask apps` lists them, `check` gates
each (formatting, clippy on the host and the targets, host tests, and that
nothing outside the folder names it), `run` and `run-compositor` install
each `default` one from its package, and `test-apps` boots once and runs
every app's `[[smoke]]` lines. A package is a newc archive of the files and
a record, `lib/ferrix/packages/<name>.toml`, with each file's BLAKE2b-256;
`src/lib/proto/pkg` reads manifests and records and plans an install, and is
the package manager's engine from the start. The one change to the system an
app needs is `ferrix_rt::linux::call` and `numbers`: any Linux call by
number. The first app is `ferrofetch`, a native fastfetch; `test-apps`
passes its three checks on x86-64, and fails, naming the check, with one
`expect` changed to a line it never prints. The same day the stat service
became the `statd` app, the first built for the Linux ABI, and btop the
`btop` app, the first built by a script; `new-app` writes a folder that
passes every app gate as it stands. That night the system's programs moved
under `src/user/system/` beside the apps (94401d20, 318987db). Bad
Apple!!'s player, launcher entries for the apps, the other ports and the
package manager are what is left, in that order (`docs/APPS.md` §10).

---

