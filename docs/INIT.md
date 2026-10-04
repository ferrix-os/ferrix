# Init: a service manager for Ferrix

Version 2, a draft. Written on 2026-09-23 at the customer's asking: *a real
init, fitting to our kernel, usable if Ferrix later becomes a microkernel,
and somewhat extensible like systemd*. Version 2 follows the customer's
second order of the same day: *build stage 13's cgroups first, and plan the
init as if they exist*. §14 lists the decisions that
are the customer's and not this document's; until they are answered, the
draft answers are what the rest of the text assumes. The first, C8, was
answered the same day. §16 says what has been built since.

It finishes stage 15. `docs/ROADMAP.md` says what is left there: "nothing in
user space mounts `/proc` and `/dev`, reaps what a session orphans, gives a
shell a session and a controlling terminal of its own, respawns one that dies,
or brings the machine down", and a getty per terminal.

## 0. Prerequisite: stage 13's cgroups, built first

**Init is not started until the cgroup half of stage 13 has landed.** Every
section below assumes cgroup v2 exists as `docs/ARCHITECTURE.md` §6 designs
it: one unified hierarchy, exposed as cgroupfs, with the `cpu`, `memory`,
`io` and `pids` controllers. Stage 13's other two thirds, namespaces and
seccomp, are **not** prerequisites. Once they land, init gains sandboxing keys
for them (§4.4, landing L13).

Why first, rather than working around it. A service manager has to know what
belongs to a service, end all of it, know when it has ended, and bound what
it may use. A cgroup answers all four questions, and it is the answer every
Linux program that manages services already expects: `systemd-run`, a
container runtime, a nested manager with `Delegate=yes`. Version 1 of this
document answered the first three with native jobs, and it needed four kernel
additions to do it. Those additions made jobs into cgroups without the
filesystem, and without the resource limits that are the fourth answer.
Building the real thing first costs no more, and init then has nothing to
unlearn.

### 0.1 What init needs from stage 13

These are init's requirements on stage 13, not stage 13's design, which is
`docs/CGROUPS.md`; its landings G1 to G4 meet C1 to C5 and C7, and G5 meets
C8. Each is Linux's own interface, so a program written for Linux cgroups
works unchanged:

| | Interface | What init uses it for |
|---|---|---|
| C1 | `mount -t cgroup2` at `/sys/fs/cgroup`, with `mkdir` and `rmdir` making and removing cgroups. The mount point is sysfs's `fs/cgroup` (`docs/SYSFS.md`), which the kernel mounts on `/sys` at boot | the tree of slices and services (§5.1) |
| C2 | `cgroup.procs`: read to list members, write to move one; `fork` and `clone` put the child in the parent's cgroup | membership, and a service's forked children staying its own |
| C3 | `clone3` with `CLONE_INTO_CGROUP`, which answers `ENOSYS` today (`src/kernel/src/syscall/family.rs`) | starting a service already inside its cgroup, with no window outside it (§5.2) |
| C4 | `cgroup.events`, with `populated` and `frozen`, waking `poll`/`epoll` with `POLLPRI` when it changes (there is no `inotify`) | knowing a service has ended, all of it |
| C5 | `cgroup.kill` | ending a service whose processes do not stop when asked |
| C6 | `cgroup.subtree_control` and the controllers' files: `memory.max`, `memory.high`, `memory.events` (with `oom_kill`), `pids.max`, `cpu.weight`, `cpu.max`, `io.weight` | the resource keys of §5.5 |
| C7 | Ownership of a cgroup directory by `chown`, with writes to `cgroup.procs` checked against it, as Linux's delegation rules check them | handing a subtree to a nested manager (`Delegate=`) |
| C8 | **Every cgroup is backed by a `Job`.** A job created natively appears as a cgroup, and `job_for_cgroup(dirfd)` returns a handle to the job behind a cgroup. The job asserts a new `EMPTY` signal exactly when `populated` becomes 0 | native services in a cgroup, and the microkernel's view of the same tree (§7) |

Init strictly needs C1 to C5 to start at all. C6 comes in controller by
controller, and a resource key whose controller does not exist yet is a
warning, not an error. So `memory` can land before `io` without init waiting
for both. C7 is needed for `Delegate=`, and C8 for `Type=native` services.

**C8 is the one requirement that is not Linux's, and the customer accepted it
for stage 13 on 2026-09-23.** `docs/ARCHITECTURE.md` §3 already calls the job
"where resource limits and kill authority live", and §4 scopes the OOM kill
"by Job and cgroup". If a cgroup is a job with a filesystem view, there is
one container concept in the kernel rather than two, and three things
follow:

* `devmgr`'s per-driver jobs show up in cgroupfs, where a driver's memory can
  be read and limited;
* `process_create` in a job and `CLONE_INTO_CGROUP` put a process in the
  same kind of place;
* a microkernel that drops cgroupfs keeps the jobs underneath it, and with
  them everything init relies on (§7).

The alternative was to build cgroups as their own kernel object, as Linux
does. Then `Type=native` services would have run in a job beside their
cgroup instead of in it, and §7's microkernel column would have needed one
translation layer more.

## 1. What this is, and what it is not

`/sbin/init` is pid 1 and the service manager, one program as on systemd, for
the same reason systemd gives: the process that reaps everything is the one
that knows which exit belonged to which service. It:

* starts the system from *units*, declarative files with dependencies, in
  parallel where the dependencies allow;
* runs each service in a **cgroup** of its own, restarts it by policy, bounds
  what it may use, and stops it as a unit, whatever it forked;
* is the one writer of the cgroup tree, except for subtrees it delegates;
* reaps orphans, since it is pid 1;
* gives each terminal a getty, with its own session and controlling terminal;
* hands services the capabilities their unit names, and nothing else (§6);
* brings the machine down in order: services in reverse, then sync, then
  unmount, then `reboot(2)`;
* answers a control socket, through which `svc` starts, stops and reports.

It is **not** `devmgr`. Drivers stay `devmgr`'s, and in this version `devmgr`
is still started by the kernel before init (§7.3 is how that changes). It is
not a logging daemon, a login manager, a network configurator or an IPC bus:
systemd grew those in the same tree, and here each would be a service that
init starts.

## 2. The three requirements, read as constraints

**Fit the kernel.** Linux is the native ABI (`docs/ARCHITECTURE.md` §2), and
with stage 13's cgroups, everything init does to a *Linux* service is Linux's
own interface: `clone3`, `execve`, `wait4`, `setsid`, `TIOCSCTTY`, `mount`,
cgroupfs, `reboot`. So init is a Linux program, std Rust on musl like zinc.
It makes native calls only for what Linux cannot express: handing a service
its bootstrap channel, and starting a native program. §11 lists the six small
kernel additions it needs besides stage 13, each one also useful outside
init.

**Survive a microkernel.** One rule makes this hold: **the manager's logic
never names a system call.** It is a pure state machine in `src/lib/init/svc` that
takes events and returns actions (§3). Every effect goes through one of nine
*backends*, and §7 says for each who serves it today and who would serve it
in a microkernel. There is a second half to the rule. A microkernel's root
task is the process that hands out capabilities, so init is designed to be
that from the first version: services get handles from init, never by name
from the kernel (§6). With C8, the cgroup tree init builds is also a job
tree, which is what a native init would build. Moving to a microkernel then
changes backends and leaves unit files, the directory protocol and the
manager unchanged.

**Extensible like systemd.** What makes systemd extensible is not its size but
five mechanisms. This design takes all five, and adds one of its own:

| Mechanism | What it lets a newcomer do without touching init |
|---|---|
| Unit files in layered directories, with drop-ins | Override one key of a shipped unit |
| Templates (`getty@.service`) | One file for every terminal |
| Targets, slices and `[Install]` | Hook into boot at a named point, and into the resource tree at a named branch |
| Generators | Write units from the machine's state at boot (a getty per console) |
| Socket activation | Start a service on its first connection |
| **The directory** (Ferrix's own, §6) | Offer or use a named native service, started on first use |

Unit *kinds* are a trait in `src/lib/init/svc` (§4.2). A new kind, such as `timer` or
`path`, is a new implementation of the trait; the graph and the operations
do not change.

## 3. Shape

```
src/lib/init/svc         no_std + alloc; host-tested, Miri, fuzzed
                 unit files -> model, the dependency graph, operations,
                 the slice tree, restart policy, and
                 Manager::step(event, now) -> actions
src/lib/init/svc-proto   the control and notify wire formats, shared with `svc`
src/user/system/linux/init/            its own workspace, std on *-linux-musl, like src/user/system/linux/zinc/
  init           /sbin/init: the event loop and the Linux backends
  svc            /bin/svc: the control client
  getty          /sbin/getty
```

The core is one function:

```rust
impl Manager {
    /// Everything that happened, in; everything to do about it, out.
    /// `now` comes from the Clock backend, so a test replays time too.
    pub fn step(&mut self, event: Event, now: Instant) -> Actions;
    /// When `step` next wants a `Timer` event, if ever.
    pub fn deadline(&self) -> Option<Instant>;
}

pub enum Event {
    Spawned   { unit: UnitId, main: Pid },
    Exited    { pid: Pid, how: Exit },          // wait4, or a native status
    Emptied   { unit: UnitId },                 // cgroup.events: populated 0
    OomKilled { unit: UnitId },                 // memory.events: oom_kill grew
    Ready     { unit: UnitId, status: Option<String> },
    Timer,
    Request   { client: ClientId, request: Request },   // from `svc`
    Open      { from: UnitId, name: Name, end: Token }, // the directory, §6
    Mounted   { unit: UnitId, result: Result<(), Errno> },
}

pub enum Action {
    MakeGroup   { unit: UnitId, path: GroupPath, limits: Limits },
    SetLimits   { unit: UnitId, limits: Limits },
    RemoveGroup { unit: UnitId },
    Spawn   { unit: UnitId, spec: SpawnSpec },  // argv, env, fds, user, group path, grants
    Signal  { unit: UnitId, signal: Signal, whom: Whom },  // main, or every member
    KillGroup { unit: UnitId },                 // cgroup.kill
    Mount   { unit: UnitId, spec: MountSpec },
    Unmount { unit: UnitId },
    Route   { to: UnitId, name: Name, end: Token },
    Reply   { client: ClientId, reply: Reply },
    Log     { unit: Option<UnitId>, line: String },
    Power   (PowerAction),
}
```

`Token` is opaque, and so is `GroupPath` as far as the core is concerned:
the core never holds a handle, a descriptor or a directory, only names the
backend maps to them. That is what lets the same crate run inside a
Linux-ABI init today and a native-only root task later. It is `no_std` so
that `devmgr` (a `no_std` native program) can use its restart policy (§5.4),
and so that a native init is a new event loop, not a new manager.

Because `step` is pure, the manager's tests are host tests. A test is a unit
set plus a script of events, and it asserts the actions. That covers boot
order, cycle breaking, the slice tree, restart backoff and shutdown order
under `cargo test` and Miri. The unit-file parser gets a fuzzer, as the
hyprlang parser has.

## 4. Units

### 4.1 Files

The syntax is systemd's INI subset: `[Section]`, `Key=value`, `#` and `;`
comments, a trailing backslash to continue a line, and repeated keys adding
to a list, where an empty assignment clears it. It is chosen for familiarity
over elegance: anyone who has written a systemd unit can write a Ferrix one,
and a distribution's unit file mostly loads. A key init does not know is a
warning in the log, not an error, so such a file still loads.

Units are searched in three directories, each overriding the one below it:

| Directory | Who writes it | Lives on |
|---|---|---|
| `/run/ferrix/units` | generators, at every boot | tmpfs |
| `/etc/ferrix/units` | the administrator; `svc enable` | the root volume |
| `/lib/ferrix/units` | the image (`xtask`) | the initramfs, installed onto `/` |

A drop-in `name.service.d/*.conf`, in any of the three, overrides single keys
of `name.service`, with files applied in name order. A unit linked to
`/dev/null` in a higher directory is *masked*. A template `getty@.service` is
instantiated as `getty@console.service`, with `%i` naming the instance.

### 4.2 Kinds

A unit's suffix names its kind, and each kind implements one trait:

```rust
pub trait Kind {
    /// The keys of its own section, parsed; unknown keys are warnings.
    fn parse(&self, section: &Section, log: &mut Warnings) -> Result<Config, UnitError>;
    /// Dependencies it implies (a mount wants its mount point's parent
    /// mounted; a service wants its slice).
    fn implied(&self, config: &Config, graph: &mut Edges);
    /// Drive one unit toward `goal`, given what just happened to it.
    fn advance(&self, unit: &mut UnitState, goal: Goal, event: Option<&Event>, now: Instant) -> Actions;
}
```

| Kind | Version | What it is |
|---|---|---|
| `.service` | 1 | Processes init starts, in a cgroup of their own (§5) |
| `.slice` | 1 | A branch of the cgroup tree, with limits over everything beneath it |
| `.scope` | 1 | Processes init did *not* start, grouped on request: a login session, a compositor's client |
| `.target` | 1 | A named point in boot; no processes |
| `.mount` | 1 | A mount point, which init mounts and unmounts |
| `.socket` | 2 | A listening socket init holds; the service starts on the first connection |
| `.builtin` | 1 | Something the kernel provides, always active (§7.2) |
| `.timer` | later | Starts a unit on a schedule |
| `.path` | later | Starts a unit when a path changes (needs `inotify`, in the kernel since 2026-09-27) |

### 4.3 Dependencies and operations

The dependency keys are systemd's, with systemd's meanings: `Requires=`,
`Wants=`, `BindsTo=`, `PartOf=`, `Conflicts=`, `After=`, `Before=`, and
`ConditionPathExists=` and its siblings, which skip a unit rather than fail it.

What systemd calls a *job*, a pending start or stop, is called an
**operation** here. The word *job* is the kernel's (§0.1, C8), and one word
must not mean two things in one design. A request is expanded into a
*transaction*: every operation it pulls in, ordered by `After=`/`Before=`.
A cycle is broken by dropping a `Wants=` edge, with a warning; a cycle made
only of `Requires=` refuses the whole transaction. A transaction that
conflicts with a running one replaces it, or is refused, by systemd's rules
for `replace` and `fail`. Operations that are not ordered against each other
run at once. That parallelism is the reason to have a graph at all.

Every service and socket gets `After=sysinit.target` and
`Before=shutdown.target Conflicts=shutdown.target` unless it sets
`DefaultDependencies=no`. That is what makes shutdown stop everything
without every unit saying so. A service is also `After=basic.target`, as
under systemd, so shutdown stops it before `basic.target`; a socket is
`Before=sockets.target`, which `basic.target` wants, and before the service
it activates.

The targets shipped with the image:

```
sysinit.target     /run and /sys/fs/cgroup mounted, generators run, hostname set
basic.target       sysinit + the builtins (§7.2)
network.target     after net.builtin; udhcpc's unit is WantedBy it
multi-user.target  gettys, sshd
graphical.target   multi-user + hyprix
rescue.target      one shell on the console, nothing else
shutdown.target / poweroff.target / reboot.target
```

`default.target` is a link to one of them, and `ferrix.target=` on the kernel
command line overrides it, as `systemd.unit=` does.

### 4.4 Services

```ini
# /lib/ferrix/units/getty@.service
[Unit]
Description=Login prompt on %i
After=basic.target

[Service]
Type=exec
ExecStart=/sbin/getty %i
Restart=always
RestartSec=0
TTYPath=/dev/%i
TasksMax=512

[Install]
WantedBy=multi-user.target
```

| Key | Meaning here |
|---|---|
| `Type=` | `simple`, `exec` (ready once `execve` succeeded), `oneshot`, `forking`, `notify` (§5.3), `native` (a native program started with `process_create` in the cgroup's job, ready on its READY message) |
| `ExecStart=`, `ExecStartPre=`, `ExecStartPost=`, `ExecStop=`, `ExecReload=` | As systemd; `-` before a path ignores its failure |
| `Restart=` | `no`, `on-failure`, `on-abnormal`, `always` |
| `RestartSec=`, `StartLimitBurst=`, `StartLimitIntervalSec=` | Backoff and the budget (§5.4) |
| `KillMode=` | `control-group` (the default: every member of the cgroup), `mixed` (the signal to the main process, then `cgroup.kill` for the rest) or `process` |
| `KillSignal=`, `TimeoutStopSec=` | The polite signal, and how long before `cgroup.kill` |
| `Slice=` | Which slice the cgroup goes under; `system.slice` by default |
| `MemoryMax=`, `MemoryHigh=`, `TasksMax=`, `CPUWeight=`, `CPUQuota=`, `IOWeight=` | The resource limits of §5.5 |
| `OOMPolicy=` | `stop` (the default), `continue` or `kill`, when the kernel's OOM kill reaches the service |
| `Delegate=` | `yes` gives the service its cgroup subtree to manage (C7) |
| `User=`, `Group=`, `WorkingDirectory=`, `Environment=`, `EnvironmentFile=` | As systemd |
| `StandardInput=`, `StandardOutput=`, `StandardError=` | `null`, `tty`, `console`, `log` (§10) |
| `TTYPath=` | The terminal for `tty`; init makes it the controlling terminal of a new session |
| `Offers=`, `Uses=` | Names in the directory (§6) |
| `NoNewPrivileges=`, `PrivateTmp=`, `ProtectSystem=`, `PrivateNetwork=`, `SystemCallFilter=` | The sandboxing keys of L13 (§4.5) |

The other sandboxing keys systemd has warn by name, and init runs the
service without them. It does not refuse the service, because a unit that
loads on systemd should load here. The two of L13 whose kernel half has not
landed, `PrivateNetwork=` and `SystemCallFilter=`, are different: they are
read, and a unit that asks for one is refused at its start, with the reason,
rather than run without what it asked for (§4.5).

`[Install]` takes `WantedBy=`, `RequiredBy=` and `Alias=`. `svc enable` makes
the links in `/etc/ferrix/units/<target>.wants/` that systemd makes.

### 4.5 Sandboxing (L13)

Five of systemd's sandboxing keys, each read as systemd 259 reads it
(`systemd.exec(5)`, checked against the host's man page and
`systemd-analyze syscall-filter`). They are parsed into a `Sandbox` in
`src/lib/init/svc/src/kind/sandbox.rs`, handed to the backend in each
`SpawnSpec`, and carried out by init's child between `clone3` and `execve`
(`src/user/system/linux/init/init/src/sandbox.rs`). A command with the `+`
prefix runs without them, as under systemd. A value that does not parse is
a warning in systemd's words and the assignment is ignored. The other
sandboxing keys systemd has (`ProtectHome=`, `PrivateDevices=`,
`ReadOnlyPaths=`, `SystemCallErrorNumber=`, ...) still warn by name, and the
service runs without them (§4.4).

| Key | systemd | Ferrix |
|---|---|---|
| `NoNewPrivileges=` | A boolean. No `execve` of the service or anything it starts gains a privilege, through set-uid or set-gid bits or file capabilities (`PR_SET_NO_NEW_PRIVS`). | The same `prctl`, the last step before `execve`. The kernel drops a set-id program's bits under it (`syscall/exec.rs`). Ferrix has no file capabilities, so set-id bits are all it covers. systemd's extra of mounting everything `nosuid` in a new mount namespace is not done; the flag makes it unneeded. |
| `PrivateTmp=` | A boolean or `disconnected`. `yes`: a new mount namespace whose `/tmp` and `/var/tmp` are directories of the host's own `/tmp` and `/var/tmp`, made per unit and removed when it stops, and shared with units that name it in `JoinsNamespaceOf=`. `disconnected` (since 256): a new tmpfs on each. | `yes` and `disconnected` alike are a new tmpfs on `/tmp` and `/var/tmp`, mode 1777, `nosuid,nodev`, made in each process's own namespace. Nothing is left on the host to remove; what the service wrote goes with its namespace. **Differs:** each command of the unit gets its own, so what `ExecStartPre=` writes to `/tmp` is not seen by `ExecStart=`, and there is no `JoinsNamespaceOf=`. `/var` and `/var/tmp` are made if missing. |
| `ProtectSystem=` | A boolean, `full` or `strict`. `yes`: `/usr`, `/boot` and `/efi` read-only. `full`: and `/etc`. `strict`: the whole hierarchy but `/dev`, `/proc` and `/sys`; with `PrivateTmp=` the private `/tmp` and `/var/tmp` stay writable. `ReadWritePaths=` opens places again. | Each place is remounted `MS_REMOUNT\|MS_BIND\|MS_RDONLY` in the service's own mount namespace, keeping the mount's `nosuid`, `nodev`, `noexec` and atime flags as `mountinfo` gives them, since a bind remount sets exactly the flags it is given. A place that is no mount's root is bound onto itself first, with `MS_REC`. Every mount beneath it, from `/proc/self/mountinfo` read in the parent, is remounted the same way, so `/data` and `/run` under `/` are read-only under `strict`. **Differs:** `yes` adds `/bin`, `/sbin`, `/lib` and `/lib64` when they are directories, since Ferrix's images keep them at the top where a merged `/usr` would have them under `/usr`; a link among them is left, as what it names is covered where it is. `ReadWritePaths=` is not built. |
| `PrivateNetwork=` | A boolean. A new network namespace with only `lo` in it, up; implies a private mount namespace, and `/sys` is remounted for the new namespace. | **Refuses to start the unit** until network namespaces land (branch `stage13-netns`), with `PrivateNetwork= needs network namespaces, which this kernel does not have yet; refusing to start the unit without it`. A sandboxing key that the unit asked for is never dropped in silence. |
| `SystemCallFilter=` | A list of system call names and `@group`s. Without `~` it is an allow-list: only those (and `@default`, added first) run. With `~` a deny-list. Later assignments add to the set when they agree with the first and take from it otherwise; an empty one resets. A denied call kills the process with `SIGSYS`, or returns `SystemCallErrorNumber=`'s errno, or a deny-list word's own `:errno`. A `User=` service gets `NoNewPrivileges=` implied, since it has no `CAP_SYS_ADMIN` to install the filter without it. | Parsed with the merge kept as an ordered list of words, each marked add or take, since expanding a group needs its per-ABI members. Unknown groups and malformed names or actions warn. **Refuses to start the unit** until seccomp filters land (S3, branch `stage13-s3-rebase`), with `SystemCallFilter= needs seccomp filters, ...`. |

**The child's order.** Everything that decides or allocates is done in the
parent first (`sandbox::plan`): a key that cannot be had refuses the unit
there, as `SpawnFailed` with a line naming the reason, before any other
lookup. The child then, after its signals, session, terminal and streams:

1. **Network** (when built): `unshare(CLONE_NEWNET)` and `lo` up.
2. **Mounts**, while still root and before anything drops a privilege:
   `unshare(CLONE_NEWNS)`; `/` and everything under it made slaves
   (`MS_REC|MS_SLAVE`, accepted and a no-op today, since no mount is ever
   shared); `/var` and `/var/tmp` made if missing; a tmpfs on each of `/tmp`
   and `/var/tmp`, then `chmod 1777`, because Ferrix's tmpfs reads no
   options; then the read-only remounts. A failure is systemd's
   `EXIT_NAMESPACE`, 226.
3. **Directory and user**, as before (§5.2): `chdir` resolves in the new
   namespace, then `setgroups`, `setgid`, `setuid`.
4. **init's go-ahead** for a bootstrap channel (§6).
5. **`PR_SET_NO_NEW_PRIVS`**, `EXIT_NO_NEW_PRIVILEGES`, 227.
6. **The seccomp filter** (when built), last of all, so that it sees
   nothing of init's but the `execve`. `EXIT_SECCOMP`, 228.
7. `execve`.

The mounts come before the user change because only a privileged process
may make a mount namespace or mount (Ferrix's `unshare` and `mount` ask for
privilege as Linux asks for `CAP_SYS_ADMIN`). `no_new_privs` comes after the
user change and before the filter because a filter needs it, or privilege,
to be installed, and a filter that ran earlier would have to allow
everything init's own steps call. A `Type=native` service with any of the
keys is refused, since none applies to a native process yet: it is not
started by `execve` and has no Linux mount namespace of its own to change.

**`PrivateNetwork=`, once network namespaces land.** Step 1 is
`unshare(CLONE_NEWNET)` in the child, as root, then a `SIOCSIFFLAGS` with
`IFF_UP` on `lo` through a datagram socket made and closed there (a stack
`ifreq`, so nothing is allocated), which gives the namespace 127.0.0.1 and
`::1` (`docs/NETNS.md` §2.2). It implies a mount namespace, as systemd's
does; `/sys` is not remounted, since Ferrix's sysfs shows no network
devices per namespace. A socket unit's `PrivateNetwork=` and
`JoinsNamespaceOf=` are not in this design. What changes: `plan` stops
refusing, the plan gains a flag, and `enter` gains step 1. The gate's
refusal check becomes a check, from inside the unit, that `lo` is up and
`10.0.2.2` cannot be reached.

**`SystemCallFilter=`, once S3 lands.** In the parent, per architecture the
kernel serves to the service (x86-64's native and its i386 entry, AArch64,
ARMv7-A; `docs/SECCOMP.md` §3.2):

1. The words are applied in order to a set of names, each group expanded
   from a table of systemd's groups kept beside the parser (from
   `systemd-analyze syscall-filter` of the version it names), each name
   looked up in that ABI's table (`ferrix-linux-abi`'s `nr`). A name the ABI
   lacks is left out of that ABI's filter, as libseccomp leaves it; a name
   no ABI has warns, as systemd's does.
2. The calls systemd always allows are added: `execve`, `exit`,
   `exit_group`, `getrlimit`, `rt_sigreturn`, `sigreturn` and the time and
   sleep calls. **Ferrix adds** the native calls a unit's `Uses=` or
   `Offers=` imply (`process_bootstrap` and the channel calls, §6), since
   the native range goes through the filter like any other number
   (`docs/SECCOMP.md` SR2) and the program takes its bootstrap channel after
   `execve`; and a group `@ferrix-native` names the whole range for a unit
   that wants it. On ARMv7-A the private calls a C library needs (`set_tls`,
   `cacheflush`) are checked against systemd's own ARM table when built.
3. A classic BPF program: load `arch`; for each ABI a block, entered by its
   `AUDIT_ARCH_*` token, which loads `nr`, refuses an x86-64 number with the
   x32 bit (`0x40000000`) with the default action, and compares the number
   against each listed call (`JEQ`, linear, at most `docs/SECCOMP.md` §3.5's
   4096 instructions; a binary search when a measured filter needs it); an
   architecture with no block gets `KILL_PROCESS`, libseccomp's bad-arch
   action. An allow-list's listed calls return `ALLOW` and everything else
   the default; a deny-list's listed calls return their own `:errno`, or the
   default, and everything else `ALLOW`. The default is
   `SECCOMP_RET_KILL_PROCESS`, or `SECCOMP_RET_ERRNO` with
   `SystemCallErrorNumber=` once that key is read too.
4. `NoNewPrivileges=` is implied when the unit has `User=`, as systemd's
   rule has it: the child is unprivileged by then, and the kernel refuses
   its filter without `no_new_privs` (`docs/SECCOMP.md` §3.5, step 5).

The child installs the program with `seccomp(SECCOMP_SET_MODE_FILTER, 0,
&prog)` as its step 6. The compiler is host code and gets host tests: the
merge rules, the expansion, and a filter run through `src/lib/kernel/seccomp`'s
interpreter on each ABI's numbers, with the arch check's negative case.
`SystemCallArchitectures=` and `SystemCallErrorNumber=` are read in the
same landing. The gate gains a unit whose `SystemCallFilter=~@mount` makes
its `mount` fail and kill it, and one whose `:EPERM` returns the errno.

**The gate (`test-init`'s sandboxing stage).** It starts two oneshots from
the prompt, `boxed.service` (uid 1000, `NoNewPrivileges=yes`,
`PrivateTmp=yes`, `ProtectSystem=strict`) and `open.service` (the same script
and user, no sandbox), and each looks from inside, running a set-uid root
copy of zinc. What each check proves:

* **`NoNewPrivileges=`.** `open.service`'s shell must report uid 1000 and
  effective uid 0: the set-uid bit works, so it is a real privilege to
  withhold. `boxed.service`'s must report 1000 and 1000: its `execve` of the
  same file gained nothing, which only `no_new_privs` (or a kernel ignoring
  every set-uid bit, which `open.service` rules out) explains. Where the
  kernel prints `NoNewPrivs:` in `/proc/self/status` (S3 adds the line), it
  must read 1 and 0.
* **`PrivateTmp=`.** A file the prompt wrote to `/tmp` just before must be
  missing from `boxed.service`'s `/tmp` and present in `open.service`'s;
  `boxed.service` must still write its own `/tmp` (the mode is 1777), and
  that file must not be in the machine's `/tmp` afterwards. That shows the
  tmpfs is new, writable by the user and private both ways.
* **`ProtectSystem=strict`.** The prompt makes `/run/l13-open` with mode
  0777, so the only thing that can refuse a uid-1000 write into it is a
  read-only mount. `boxed.service`'s write must be refused and
  `open.service`'s must succeed, and the refused file must not be there
  afterwards. That shows a mount beneath `/` was remounted read-only in the
  service's namespace and not in the machine's. `/` itself is remounted the
  same way first; the check reaches it through `/run`, a mount of its own,
  because a uid-1000 write to `/` is refused by its mode anyway.
* **The two keys not yet built.** `netns.service` and `filtered.service`
  must be refused by init with their reasons and must never print.

The host tests in `src/lib/init/svc/src/tests/sandbox.rs` hold the parsing
and the `+` prefix, those in `init/src/sandbox.rs` the mount plan (which
places, which flags, what is bound first, what is exempt) and the two
refusals, and `tools/common/xtask/src/init.rs`'s judge tests hold the stage
to one failure line per key that did nothing.

**Certification.** Init is ring 3 (`src/user/system/linux/init`) and outside
`tools/common/data/certification-item.json`'s rings; whether its keys belong
to the item is for the certification consultant to rule. If they do, the
claims above are the ones to review: each key's check and its negative
control, listed in §16.

## 5. Supervision: a service is a cgroup

### 5.1 The tree

Init mounts cgroup2 at `/sys/fs/cgroup` (C1) and builds systemd's layout,
because tools that read it already know it:

```
/sys/fs/cgroup/
  init.scope/                       pid 1 itself
  system.slice/
    getty@console.service/
    sshd.service/
    hyprix.service/
      app-foot-12.scope/            a client hyprix asked init to group (§5.6)
  user.slice/
    user-1000.slice/
      session-1.scope/              the shell a getty's login started
  drivers.slice/                    devmgr's jobs, as C8 makes them visible (§7.3)
```

Init moves itself into `init.scope` first, because cgroup v2 lets processes
live only in leaves once a cgroup's controllers are enabled for its children.
Then it enables the controllers that exist in the root's `subtree_control`,
then in each slice's as it creates the slice. A slice's limits bound the
whole branch beneath it. So `MemoryMax=` on `system.slice` keeps the
services, together, from starving the login sessions.

Init is the one writer of this tree. Everything else reads it, except a
service with `Delegate=yes`: its subtree is chowned to its `User=` (C7), and
init never writes beneath it.

### 5.2 Starting and stopping

A Linux service starts as follows:

1. Init `mkdir`s the service's cgroup under its slice, writes its limits, and
   opens the directory.
2. `clone3` with `CLONE_INTO_CGROUP` and that directory (C3). The child is in
   the service's cgroup from its first instruction, so nothing it does before
   `execve`, or at any time after, can land anywhere else. `fork` inherits
   the cgroup (C2), so a daemon that forks twice stays the service's.
3. The parent hands the child its bootstrap channel (K3), if the unit has a
   `Uses=` or `Offers=`, and then writes one byte on a pipe the child is
   blocked on.
4. The child sets up its credentials, session, terminal, descriptors and
   environment, then calls `execve`.
5. The pipe closes on exec (`O_CLOEXEC`), which is how the parent learns that
   `execve` succeeded (`Type=exec`), as `posix_spawn` implementations do.

A native service is `job_for_cgroup` on the directory (C8), then
`process_create` in that job and `process_start` with its bootstrap channel.
That is exactly what `devmgr` does for a driver, so a native service is in
the service's cgroup just as a Linux one is.
A native process runs as the process that made it, as a fork child does
(`docs/AUTH.md` §7, P0), so a native service init makes itself is root's.
One with `User=`, `Group=` or `SupplementaryGroups=` is made by a helper
instead (P0b): init forks it, gives it a channel with `process_give` (K3)
carrying the service's bootstrap end, and lends it the cgroup's
`cgroup.procs` alone, so that `job_for_cgroup` answers it `MANAGE`. The
helper becomes the unit's ids, reads the image as them, makes and starts
the process, and writes a handle to it back; init watches that handle as it
watches one it made, takes `cgroup.procs` back (unless the unit is
delegated) and reaps the helper. The process runs as the unit's ids in
every role.

To stop a service, init runs `ExecStop=` if the unit has one. Then it sends
`KillSignal=` (`SIGTERM` by default) as `KillMode=` says: to the main process
only, or to every pid in `cgroup.procs`. It waits up to `TimeoutStopSec=` for
`cgroup.events` to say `populated 0` (C4), then writes `cgroup.kill` (C5),
and finally `rmdir`s the cgroup. So the service is stopped when its cgroup is
empty, not when its main pid exits. A service whose main process exits while
other processes remain is `deactivating` until they go.

### 5.3 Readiness

A service is *active* when it says so, not when it has started. Which of
these a service uses is set by its `Type=`:

* `exec`: `execve` succeeded (§5.2 step 5).
* `notify`: the service writes a line to descriptor `NotifyFd=`, a pipe init
  passed it. `READY=1` means ready and `STATUS=…` is shown by `svc status`.
  This is s6's readiness descriptor with sd_notify's words. It is not
  sd_notify itself, because that protocol sends a datagram to a named socket
  and Ferrix answers that with `EOPNOTSUPP` today. When the kernel sends
  datagrams to names, `NOTIFY_SOCKET` is a second transport for the same
  parser, and ported daemons that speak sd_notify work unchanged.
* `native`: the service's first message on its bootstrap channel is READY.
  A driver's HELLO is the same idea.
* `forking`: the parent exited 0. The main pid is the one the service writes
  to `PIDFile=`; failing that, it is the one process left in the cgroup.

### 5.4 Restarting

Restarting is a policy function in `src/lib/init/restart`, which `src/lib/init/svc`
re-exports and which allocates nothing, so that `devmgr` can link it: given
the history of a unit's exits and the clock, restart now, restart at `t`, or
give up. The defaults are systemd's: `RestartSec=100ms`, and at most five
starts in ten seconds, after which the unit is `failed` and stays so until
`svc reset-failed` or a new start. The ten seconds are systemd's fixed
window (`ratelimit_below` in `src/basic/ratelimit.c`), not a sliding one: it
begins at a start, holds while no more than the interval has passed since,
and the first start after that begins the next, counting from one. Each restart doubles the delay, up to
`RestartSec=` times 32. A restart always begins with an empty cgroup: a
service's leftovers from its last run are killed before it starts again.

`devmgr` has the same problem with no clock. It restarts a display driver at
most eight times, counted (`docs/DEVMGR.md` §4), through the same `Policy`
since L11. The policy function takes `Option<Instant>`: with none it falls
back to a pure count, and with a clock it uses the window. So `devmgr` and
init share one tested implementation, and
when `devmgr` becomes a unit under init (§7.3) its drivers' budget can
become a rate.

### 5.5 Resources

Each resource key writes one controller file in the service's cgroup (C6):

| Key | File | Controller |
|---|---|---|
| `MemoryMax=` | `memory.max` | memory |
| `MemoryHigh=` | `memory.high` | memory |
| `TasksMax=` | `pids.max` | pids |
| `CPUWeight=` | `cpu.weight` | cpu |
| `CPUQuota=` | `cpu.max` | cpu |
| `IOWeight=` | `io.weight` | io |

`svc set-property unit Key=value` writes the file at run time, and writes
the key to a drop-in only if `--persistent` is given. A key whose controller
the kernel does not have is a warning and is skipped, so a unit written for
the full set loads on a kernel that has built only `memory` and `pids`.

When the kernel's OOM kill takes a process in a service, `memory.events`'
`oom_kill` count grows, and `poll` reports it like `cgroup.events`. Init
records the result `oom-kill`, and `OOMPolicy=` decides the rest: `stop`
takes the whole service down, since a service missing one process is often
worse than a stopped one, and `Restart=` then applies as for any failure.
This is where stage 13's exit, an OOM kill scoped to one cgroup, becomes
something a person sees: `svc status` says which service it was.

### 5.6 Scopes: processes init did not start

A login shell is started by getty's `login`, not by init. A terminal window's
shell is started by hyprix's `exec`. Both should be accountable, limitable
and killable as a unit. A **scope** is a cgroup for processes that already
exist. A program asks init for one over the control socket, with the pids to
move in and the slice to put the scope under:

```
svc scope --slice user-1000.slice --unit session-1.scope --pid 4242
```

Init makes the cgroup, moves the pids into it (C2) and supervises it from
then on like a service it did not start. A scope has no `ExecStart=`, is
stopped by signal and `cgroup.kill`, and is removed once empty. `login`
creates `session-N.scope` under `user-<uid>.slice` for the session it
starts, while it is still root (`docs/AUTH.md` §6.2; `getty --login` execs
it). hyprix creates
`app-<name>-<n>.scope` for each program it starts, so `svc status` answers
which window a runaway process came from, and `svc stop` closes all of it.

### 5.7 pid 1's other duties

Init reaps every orphan the kernel reparents to it. An exit that matches no
unit's main process is reaped and forgotten, because the orphan is still in
its service's cgroup, and `populated` is what init listens for. Init sets
`PR_SET_CHILD_SUBREAPER` anyway, so a future nested manager (a
`Delegate=yes` session manager) behaves the same. It ignores every signal but
the ones it acts on (§8.2). If it panics, it does not unwind: it is built with
`panic = "abort"`, and §8.3 says what the kernel does next.

## 6. Capabilities and the directory

This is the part of the design that a microkernel needs and systemd does not
have. The cgroup tree says what a service may *use*; the directory says what
it may *reach*.

**The kernel gives init its capabilities (K2).** The kernel starts init with
a bootstrap channel, just as it starts `devmgr`. Before init runs, the kernel
writes one message on its end carrying the handles init may pass on. The
first version carries none that init needs to start: its jobs come from
cgroupfs through C8. The channel exists so that later messages can carry a
power handle, and in a microkernel the device root and physical memory, which
`devmgr` receives today (§7.3). How a Linux program finds its bootstrap
handle is **K3**: the `process_bootstrap` call returns it once, then nothing.

**Init gives each service a bootstrap channel.** Init keeps the other end.
On that channel the service can:

```
READY    service -> init   Type=native readiness (§5.3)
OFFER    service -> init   name; one handle: a channel end init sends OPENs down
OPEN     service -> init   name; one handle: the client's end of a new channel
CONNECT  init -> provider  name, the client unit; one handle: that end, forwarded
REFUSED  init -> service   name; why
```

A unit declares what it offers and uses:

```ini
# /lib/ferrix/units/clipboard.service
[Service]
Type=native
ExecStart=/sbin/vdagent
Offers=ferrix.clipboard

# /lib/ferrix/units/hyprix.service
[Unit]
Wants=clipboard.service
[Service]
ExecStart=/bin/hyprix --config /etc/hyprland.conf
Uses=ferrix.clipboard
Delegate=yes
```

The client creates a channel pair, keeps one end and sends the other in OPEN.
Init checks that the client's unit lists the name in `Uses=`. It starts the
provider if the provider is not running, since an OPEN is an activation just
as a connection to a `.socket` is, and forwards the end in CONNECT. From then
on the two talk directly; init is out of the path. A name nobody offers, or
that the unit does not declare, gets REFUSED.

The first pair a shipped image uses (2026-10-03, `docs/AUTH.md` §3.7):
`auth.service` says `Offers=ferrix.auth.seat`, and a session's
`hyprix.service` says `Uses=ferrix.auth.seat`. There the unit's own process
is `sessiond`, which takes the bootstrap and relays to the compositor it
starts as the user: a bootstrap belongs to the process init spawned and is
sealed at `execve`, so a child cannot take it.

Four things follow:

* **The unit files are the policy.** What a service can reach is what its unit
  says, and nothing reaches a service except through a channel init routed. No
  global namespace exists to search. A compromised service can open only the
  names it declared.
* **Handles never pass through the core.** An OPEN arrives as `Event::Open`
  carrying a `Token`, and the backend moves the real handle when the core
  returns `Action::Route`.
* **Linux services use it the same way.** A musl program may make native calls
  (`docs/ARCHITECTURE.md` §2), and `process_bootstrap` gives it its channel. A
  Linux service that makes no native call ignores the channel, and closing it
  costs nothing.
* **Sockets and the directory do the same job for two worlds.** A `.socket`
  unit gives a Linux daemon `LISTEN_FDS` and is activated on connect. An
  `Offers=` name gives a native service CONNECT and is activated on open.
  Both come from one manager with one dependency graph.

## 7. The backends, and who serves each

### 7.1 The table

| Backend | Today (Linux-ABI init, monolithic kernel) | In a microkernel |
|---|---|---|
| **Spawn** | `clone3(CLONE_INTO_CGROUP)`, `execve`; `process_create` in the cgroup's job for `Type=native` | the process server's spawn, or `process_create` everywhere |
| **Groups** | cgroupfs: `mkdir`, `rmdir`, `cgroup.procs`, `cgroup.kill` | the jobs behind them (C8): `job_create`, `job_kill` |
| **Resources** | cgroupfs controller files | limits on the job, which `docs/ARCHITECTURE.md` §3 already puts there |
| **Supervise** | `wait4` on `SIGCHLD`; `cgroup.events` and `memory.events` by `POLLPRI`; process `TERMINATED` on a port | the job's `EMPTY` (C8) and the process's `TERMINATED`, on a port; statuses from K6 |
| **Clock** | `clock_gettime`; the event loop's timeout (§9) | a timer object on the port |
| **Filesystem** | `mount(2)`, `umount2(2)`, `sync(2)` | a channel to the VFS server, from the directory |
| **Terminal** | `open` of `/dev/*`, `setsid`, `TIOCSCTTY` | a channel to the console server |
| **Power** | `reboot(2)` | the power handle from K2 |
| **Directory** | init's own, over bootstrap channels | unchanged: it is already what a root task is |

Each backend is a Rust trait in `src/user/system/linux/init/`, and the Linux implementations are
the only ones built in version 1. The table's right-hand column is not
promised work. It is the check that nothing in `src/lib/init/svc` would have to change
if that column were built. It holds for the Groups, Resources and Supervise
rows only because of C8. Without C8, the microkernel column of those three
rows would need a cgroup server rebuilt from nothing.

### 7.2 What the kernel provides is a unit too

The filesystem, the network stack, the block core and `devmgr` are in the
kernel today (`devmgr` is started by it). In a microkernel they would be
services. So init has units for them now, of kind `.builtin`: always active,
started by nobody, stopped by nobody.

```
vfs.builtin   net.builtin   block.builtin   devmgr.builtin
```

A unit written today says `After=net.builtin` or `Uses=ferrix.vfs`, and it is
correct on both kernels. The day the network stack becomes a server,
`net.builtin` is replaced by `net.service` with `Alias=net.builtin`, and no
unit that depended on it changes. The names are the contract. Whether a kernel
subsystem or a process serves one is an implementation detail, which is the
whole point.

### 7.3 `devmgr`, and the one step a microkernel changes

Today the kernel starts `devmgr` before init, because the root volume needs a
ring-3 block driver before any file on it can be read (`docs/ARCHITECTURE.md`
§7, "Bootstrap"). Init starts from the initramfs copy, as `devmgr` does, so
nothing in this design has to change that order. With C8, `devmgr`'s root
job and each driver's job are cgroups. Init adopts them read-only as
`drivers.slice`, so a driver's memory shows in `svc status devmgr.builtin`,
though init neither starts nor stops them.

The step toward a microkernel is to reverse it. The kernel starts only init,
and hands it on K2's channel what it now hands `devmgr`: the device nodes and
the driver images. Init then starts `devmgr.service` (`Type=native`,
`Slice=drivers.slice`) with them. The DEVICES protocol (`docs/DEVMGR.md` §2)
stays the same; init writes it instead of the kernel. `devmgr.builtin`
becomes `devmgr.service`, as in §7.2. `DEVMGR.md` §5's rule, that no driver
may fault on the disk it serves, still holds, because init starts `devmgr`
from the initramfs before any mount it depends on. This is landing L12,
and it is the rehearsal that proves the backend table true.

**As built (L12, 2026-09-27), after the certification review.** Two things
differ from the sketch above, both for the item's sake. The DEVICES channel
carries every device's authority, and through it DMA, so it never passes
through init: init is given a *starter* instead, and the kernel does the
rest. And the images that boot init opt in, so the certified configuration
keeps the kernel's path.

* `ferrix.devmgr=init` on the command line, which the images that boot
  `/sbin/init` carry (`run`, the desktop, `test-init`, `test-compositor`,
  `test-jobs`). Without it, or with `kernel`, the kernel starts `devmgr` at
  bring-up as before; that is the reference configuration
  (`docs/certification/ITEM.md` §5), and the boots that carry the evidence
  keep it.
* The kernel writes a second message on pid 1's K2 channel after the hello:
  `FXDS`, carrying `Object::Starter` with `MANAGE` alone -- no `TRANSFER`,
  no `DUPLICATE`, so it can never leave pid 1's table.
* `devmgr.service` (`Type=simple`, `ExecStart=/sbin/devmgr`,
  `Slice=drivers.slice`, `Restart=always`, `DefaultDependencies=no`) is the
  only unit init starts before it knows where `/` is. Its spawn is
  `devmgr_start(starter, job)` (0x1052) with the job behind the unit's
  cgroup: the kernel loads `/sbin/devmgr`, writes DEVICES with that job for
  the drivers, makes and starts the process in it, and answers a handle to
  it with `PROCESS` rights, which reach nothing inside it. The channel's end
  goes into `devmgr`'s table and nowhere else, and every start checks that
  as the caller, with `process_start` on the handle `BAD_STATE` and a second
  start while it lives `ALREADY_BOUND` (FX-1009). A start after one died is
  `BAD_STATE` until every process in its job has ended; then the kernel
  quiesces every device before handing them on. So `svc restart
  devmgr.service` works, as does a restart by its policy.
* The stage 10 to 12 boot checks that read through `devmgr`'s drivers are
  not run on those boots (`SAFETY-MANUAL.md` AoU-13).
* `/` is switched once `devmgr` has reported, by a kernel task
  (`fs::root_disk::switch_after_devmgr`), which then mounts the data disk and
  writes `FXRT` on K2: switched or not. The switch **re-roots pid 1**: its
  root and working directory become the volume's in the step that publishes
  the new root, under pid 1's filesystem lock (FX-1202 checks that, and that
  a fork of it sees the volume). Init then mounts `/run` and cgroup2 again,
  binds its control socket again, reads its units from the volume, and boots
  `default.target`. What init opened before keeps the root it was opened
  under, as after a `chroot`.
* With it, init's `KillMode=control-group` signals every process in the
  unit's cgroup and beneath it, as systemd does, not only its own
  `cgroup.procs`: `devmgr`'s drivers are in jobs beneath its unit's.
* **The switch moves pid 1 only.** `devmgr` and every driver it starts keep
  the initramfs root, before the switch and after a restart alike, since
  `devmgr_start` loads and starts them from it. So a driver-side socket that
  other programs find must not be a path: bound under the initramfs's
  `/tmp` or `/run`, it is out of sight of every program started from the
  volume, and init mounts a fresh `/run` there rather than moving the old
  one across as Linux's `mount --move /run` would. The rule is an abstract
  `AF_UNIX` name, gated on the peer's `SO_PEERCRED` in place of a file
  mode: `vport`'s `\0ferrix.vport` is the first (ferrix-e4, 2026-09-27).
  Abstract names are per network namespace, so a unit with L13's
  `PrivateNetwork=` will not see them.
* When a unit's cgroup goes, init removes the cgroups beneath it first and
  tries a cgroup still busy again every 50 ms for up to 5 s: a dead
  driver's job outlives its last process for a moment, while the kernel
  lets go of that process, and a job `job_create` made has no name to
  remove it by.

## 8. Boot and shutdown

### 8.1 Boot

The kernel runs its boot checks, starts `devmgr`, switches the root, then
starts **the program `ferrix.init=` names** (K0, built in L3). The file may
be a `#!` script, run under its interpreter as `execve` runs one. A file that
is missing or will not start is said on one line (`init     ferrix.init=…
could not be started: …; falling back to the built-in program`), and the
built-in program runs as before. With nothing named, the order is the
built-in program, then `/sbin/init` if the image has one, then nothing. The
built-in program goes first so that no gate that embeds a shell or hyprix
can change: none of today's images carries a `/sbin/init`, and one that
starts to must not take a gate's boot from it. `test-init` (§15) embeds
nothing and names `/sbin/init`, so it reaches init either way.

Init then:

1. Mounts `/run` (tmpfs), then cgroup2 at `/sys/fs/cgroup`. It moves itself
   into `init.scope` and enables the controllers the kernel has (§5.1).
   `/proc`, `/dev`, `/sys` and `/tmp` are already mounted by the kernel
   today; they have `.mount` units, whose mount
   is skipped when the kernel already mounted them. That way a kernel that
   stops mounting them changes nothing above it.
2. Receives K2's first message.
3. Runs the generators in `/lib/ferrix/generators/`, each with
   `/run/ferrix/units` as its argument, with a deadline. The first one shipped
   is `getty-generator`, which writes a `getty@<name>.service` link into
   `multi-user.target.wants` for every terminal the kernel command line names
   in `console=`, and for `/dev/console` otherwise.
4. Loads the units and starts `default.target`, or `ferrix.target=`.
5. Prints one boot-log line per unit that becomes active or fails, in the
   `  init     …` format `xtask` reads.

If `default.target`'s transaction fails, init starts `rescue.target`, a shell
on the console, so a broken unit file is fixable from the machine itself.

### 8.2 Shutdown

`svc poweroff`, `svc reboot` (also `poweroff`, `reboot` and `shutdown [-r]
now`, links to `svc` that take their name as the verb, as systemctl's do),
or `SIGTERM`/`SIGINT` to pid 1 (Ctrl-Alt-Del's
signal, and what a QEMU `system_powerdown` will become) start
`poweroff.target` or `reboot.target`. Starting either stops everything that
`Conflicts=shutdown.target`, which is everything by default (§4.3), in
reverse dependency order, each unit by its own `KillMode=`. Then init:

1. writes `cgroup.kill` in every cgroup left beside `init.scope`, deepest
   first, and waits for each to report `populated 0`;
2. calls `sync`, remounts `/`, `/data` and `/home` read-only, and unmounts
   the rest in reverse order;
3. calls `reboot(2)`.

Each remount of step 2 is checked by a write after it, which must be
refused with `EROFS`; init then says `/ is read-only` (and the same for
`/data` and `/home`, each that is mounted), and `test-init` requires the
lines for the disks its boot has. Until 2026-09-28 the kernel
refused `MS_REMOUNT` and init only said so (finding F-53, closed by
`docs/NAMESPACES.md`'s N1).

A process in no service does not survive step 1: it is in some cgroup, and
every cgroup but init's is killed. The cgroup tree is what makes "stop
everything" mean everything.

`reboot(2)` commits `/`, `/data` and `/home` before it acts (**K7**, built in
L3; `/home` since 2026-10-03), as
`power::finish` does, so a program calling it directly cannot lose a btrfs
transaction either. Init's own `sync` in step 2 then leaves it nothing to
commit.

### 8.3 When init dies

Linux panics. The kernel here does what it does today when pid 1 exits: it
prints `init     … exited with N` and runs `power::finish`, which syncs and
powers off. That is what every gate relies on, so it stays. The kernel option
`ferrix.onexit=panic`, beside `reset`, gives Linux's behaviour to anyone who
wants it (L3): the disks are committed, and then the kernel panics with
`FX-1501`.

## 9. The event loop

Init waits in one `epoll_wait`. Its descriptors are:

* a self-pipe that the handlers for `SIGCHLD`, `SIGTERM` and `SIGINT` write to
  (there is no `signalfd`);
* each cgroup's `cgroup.events`, and each `memory.events` where the memory
  controller is on, with `EPOLLPRI` (C4);
* the control socket, and each connected `svc` client;
* each `Type=notify` service's readiness pipe;
* each `.socket` unit's listening socket;
* **the port**, through K4's `port_fd`: a descriptor that is readable while
  the port has packets. On the port, init watches every native service's
  process for `TERMINATED`, and every bootstrap channel for READABLE and
  PEER_CLOSED.

The timeout is `Manager::deadline()`, so no `timerfd` is needed.

K4 is needed because native waits and file descriptors cannot see each other
today. The bridge goes this way round, a port becoming a descriptor, because
a descriptor that polls is one `File` implementation, while a port that
watches descriptors would reach into every file type's wakeups. hyprix and
the terminal have the same problem as soon as they use a native service, so
K4 is not init's alone.

A native init, the microkernel column of §7.1, would wait on the port alone.
It would watch each job's `EMPTY` in place of `cgroup.events`, and bind the
signals it needs to the port. It would be a different loop over the same
`Manager`.

## 10. Control and logs

`/run/ferrix/control` is a stream socket. Requests and replies are
length-prefixed records in `src/lib/init/svc-proto`, not text, so `svc`'s output can
change without breaking another client. `SO_PEERCRED` says who is asking:
anyone may ask for status. Only root may change state, except that a user
may make a scope for their own processes under their own `user-<uid>.slice`.
`svc` takes the verbs people already know:

```
svc status [unit]      svc start|stop|restart|reload unit
svc list [--failed]    svc enable|disable|mask|unmask unit
svc log unit           svc daemon-reload   svc isolate target
svc poweroff|reboot    svc reset-failed [unit]
svc scope …            svc set-property unit Key=value [--persistent]
svc top                (cgroup usage per unit, from the controller files)
```

There is no journal. A service's standard output and error default to `log`:
a pipe init reads, prefixes each line with the unit's name, writes to the
console, and keeps in a ring of the last 256 lines per unit for
`svc log unit`. A journal, if one is ever wanted, is a service that takes
`Uses=ferrix.log`, and nothing here needs to be written differently for it.

## 11. What the kernel needs, besides stage 13

Stage 13's cgroups are §0's prerequisite, sized by stage 13 and not here.
Beyond them, init needs these six small items. Each is argued by something
outside init as well:

| | Change | Also wanted by | Points |
|---|---|---|---|
| K0 | `ferrix.init=<path>`: start pid 1 from a file, the embedded program as the fallback. **Done in L3** | any image that is not a gate | 2 |
| K2 | Init started with a bootstrap channel, as `devmgr` is. **Done 2026-09-26**, kernel half of L8 | §7.3 | 2 |
| K3 | `process_give(pid, handle)`: a parent installs one handle in its own child that has not yet called `execve`; `process_bootstrap()` returns that handle once, to the child. **Done 2026-09-26**, kernel half of L8 | any Linux program that starts a native-aware one | 2 |
| K4 | `port_fd(port)`: a descriptor readable while the port has packets. **Done 2026-09-26**, kernel half of L8 | hyprix and the terminal, once they use a native service | 3 |
| K6 | Read a process's exit status and signal from its handle (in the reserved `0x1032..0x1037`). **Done 2026-09-26**, kernel half of L8 | `devmgr` reports each driver's own status since L11 | 1 |
| K7 | `reboot(2)` syncs `/`, `/data` and `/home` first, as `power::finish` does. **Done in L3**, `/home` 2026-10-03 | any program calling it | 1 |

Together that is 11 points. None of the six changes the ABI of an existing
call. Version 1's K1 (jobs inherited by `fork`) and K5 (a signal to every
member of a job) are gone, because C2 and `cgroup.procs` are the same things
in Linux's own words. The numbers are kept so that version 1's references
still resolve.

A **second terminal** is a separate item, outside these points. devfs has
`console`, `tty` and `ptmx`, and no second serial device. A getty per terminal
is built here and gated on the console. A second terminal needs a second UART
node or `hvc0` over the virtio-console library that `vport` already uses, and
that belongs to the rest of stage 15's word *ttys*.

## 12. The order

```
stage 13, cgroups: C1-C5 ──┬──> L3 ──> L4 ──┬──> L5 (with C6, C7) ──> L10
                  (then C6,│                ├──> L6, L7, L9
                   C7, C8) │                └──> L8 (with C8) ──> L12 (later)
L1 ──> L2 ─────────────────┘
stage 13, namespaces + seccomp ─────────────────> L13
```

L1 and L2 are host-only and need nothing from the kernel, so they can be
built while stage 13 is. Init's first boot, L4, waits for C1 to C5, which
landed with C7 on 2026-09-24 (`docs/CGROUPS.md` §7.1).

## 13. Landings and points

In story points, each landing gated by the row it names. Stage 13's own
points are not here; the roadmap sizes that stage at about 60, and its
cgroup half is what §0 asks for first.

| | Landing | Needs | Gate | Points |
|---|---|---|---|---|
| L1 | `src/lib/init/svc`: the unit-file parser, drop-ins, templates, the model; a fuzzer | | host tests, Miri, fuzz | 5 |
| L2 | `src/lib/init/svc`: the graph, transactions, operations, the slice tree, the restart policy, `step` | L1 | host tests replaying event scripts | 8 |
| L3 | K0, K7. **Done 2026-09-24** (§16) | | `test-boot`, `test-shell` | 3 |
| L4 | `init` minimal: pid 1, reaping, `/run` and cgroupfs, `init.scope`, a cgroup per service, `simple`/`exec`/`oneshot`, `KillMode=`, restart, shutdown by `cgroup.kill`; `getty` and the generator. **Done 2026-09-26** (§16) | L2, L3, C1-C5 | `test-init` stages one and two (§15) | 10 |
| L5 | Slices and scopes; the resource keys; `OOMPolicy=`; `Delegate=`. **Done 2026-09-26** (§16) | L4, C6, C7 | `test-init` stage three | 6 |
| L6 | `svc` and the control socket; `log` output; `set-property`, `top`. **Done 2026-09-26** (§16) | L4 | `test-init` stage four | 5 |
| L7 | `notify` readiness; `forking`. **Done 2026-09-26** (§16) | L4 | host tests + `test-init` | 3 |
| L8 | K2, K3, K4, K6; `Type=native` in the cgroup's job; the directory (§6). **Done 2026-09-26** (§16) | L5, C8 | `test-init` stage five | 16 |
| L9 | `.socket` units. **Done 2026-09-26** (§16), sshd aside | L4 | `test-init`: sshd activated on connect | 5 |
| L10 | Move the images over: `cargo xtask run` and `run-compositor` boot init with `multi-user.target` / `graphical.target`; hyprix stops being pid 1 and makes a scope per client. **Done 2026-09-26** (§16) | L5, L6 | `test-compositor` under init | 6 |
| L11 | `devmgr` shares the restart policy. **Done 2026-09-26** by ferrix-55b: the policy is its own no-alloc crate, `src/lib/init/restart`, with systemd's fixed-window start limit, and devmgr reads each death's status through K6 | L2 | `test-restart` | 2 |
| L12 | The kernel starts init alone and init starts `devmgr` (§7.3). **Done 2026-09-27**, re-sized from 8 to 12 for the starter, the re-root and the certification record | L8 | `test-init --arch all` with a root disk, the whole image row | 12 |
| L13 | The sandboxing keys (§4.5), re-sized from 8 to 10 and split by the kernel half each needs. **L13a built 2026-10-04** (§16): all five parsed, `NoNewPrivileges=`, `PrivateTmp=`, `ProtectSystem=` carried out, the other two refusing the unit. L13b: `PrivateNetwork=`. L13c: `SystemCallFilter=`, with `SystemCallArchitectures=` and `SystemCallErrorNumber=` | L4; L13a mount and user namespaces (N1 to N3) and `prctl`; L13b network namespaces; L13c S3 | `test-init`'s sandboxing stage | 5 + 1 + 4 |

The kernel items of §11 are counted inside the landings that carry them.
L1 to L10 add up to 67 points, and L11 to L13 to 24 more (L12 re-sized from 8 to 12, L13 from 8 to 10). L1 to L4 are what
the roadmap calls "a working init". L5 is what makes it a resource manager.
L8 is what makes it one a microkernel could keep.

## 14. What the customer decides

1. ~~**C8, cgroups built over jobs.**~~ **Decided 2026-09-23: yes.** It is
   the one choice here that is stage 13's design and not init's, and the
   microkernel requirement rests on it (§7.1).
2. **Stage 13's order inside itself.** Draft: cgroups (C1 to C5, then the
   controllers, `memory` and `pids` first), then namespaces, then seccomp.
   Init waits for only the first of these.
3. **The unit syntax.** Draft: systemd's INI subset (§4.1), for familiarity.
   The alternative is TOML, which is cleaner but has no serde here; its parser
   would be hand-written like hyprlang's.
4. **Whether hyprix stops being pid 1** (L10). Draft: yes. Today a compositor
   crash powers the machine off, and under init it is a restart.
5. **Whether `devmgr` moves under init** (L12), and when. Draft: after the
   directory has been used for real, not before.
6. **What happens when init dies** (§8.3). Draft: keep `power::finish`, and
   add `ferrix.onexit=panic`.
7. **The names**: `/sbin/init`, `svc`, `/lib/ferrix/units`, `ferrix.*`
   directory names, systemd's slice names. Draft: as written.
8. **Where the design goes on the roadmap.** Draft: stage 15, whose remaining
   item it is, with stage 13's cgroups as its prerequisite. L12 belongs with
   whatever stage first takes the microkernel question up again
   (`docs/BACKLOG.md`, 2026-09-16).

## 15. The test

`cargo xtask test-init` boots `/sbin/init` from the image, with
`ferrix.init=` and no embedded program, on all three architectures. Each
landing adds a stage, and each stage requires its lines:

1. **Boot and a terminal** (L4). `multi-user.target` becomes active. The
   getty on the console gives a zinc prompt whose session and controlling
   terminal are its own, read from `/proc/self/stat`, not taken from the
   transcript. A test service that exits 1 is restarted by its budget and then
   reported `failed`. `svc poweroff` from the prompt ends with every unit
   stopped in reverse order, `sync`, and a `btrfs check` of the volume that
   finds it clean.
2. **Groups** (L4). A service forks twice and its main process exits. The
   grandchild's pid, read from a file the service wrote, is in the service's
   `cgroup.procs`, and stopping the service ends it.
3. **Resources** (L5). A service with `MemoryMax=64M` that allocates past it
   is OOM-killed and reported `oom-kill`, while a sibling service with no
   limit keeps running. A `TasksMax=` service's `fork` fails with `EAGAIN` at
   the limit.
4. **Control** (L6). The session types `svc status`, `svc restart`,
   `svc log`, and a non-root `svc stop` that must be refused.
5. **The directory** (L8). A native test service is offered and started on
   first OPEN, and is found in its unit's cgroup. A unit that does not declare
   the name is REFUSED.
6. **Sandboxing** (L13). A uid-1000 service with `NoNewPrivileges=yes`,
   `PrivateTmp=yes` and `ProtectSystem=strict` runs a set-uid root shell
   and stays uid 1000, cannot see the machine's `/tmp` and leaves nothing in
   it, and cannot write into a 0777 directory under `/run`, while the same
   service without the keys can do all three (§4.5).

Every stage has a negative control, per this repository's rule. It must show
it fired: a marker line, a sabotage that matches exactly one line, then that
stage's own failure. Stage two's control, for example, is `KillMode=process`
on the forking service, which must leave the grandchild alive and fail that
stage's check. `test-jobs` moves onto a getty under init, and then it tests
the system people will actually use.

## 16. Where it stands (2026-09-26)

| | State | On `main` as |
|---|---|---|
| L3 | done, 2026-09-24 | "Start pid 1 from the file ferrix.init= names, and commit the disks in reboot(2)" |
| L1 | done, 2026-09-24 | "Read unit files in systemd's syntax" |
| L2 | done, 2026-09-24 | "Run units as one state machine of events and actions" |
| L4 | done, 2026-09-26 | "Add /sbin/init, getty and the getty generator around src/lib/init/svc's manager" |
| L5, L6, L7, L9 | done, 2026-09-26 | "Give the manager reload, a readiness status, and socket units"; "Add src/lib/init/svc-proto: svc's control records and readiness lines"; "Give init svc, the log, readiness, sockets and resources" |
| L8 | done, 2026-09-26: the kernel half (K2, K3, K4, K6), then init's | "Let a parent hand its child a bootstrap handle across execve" and the five after it; "Route the directory's OPENs, and start Type=native services" |
| L10 | done, 2026-09-26 | "Boot the images through init, and the compositor as its service" |
| L11 | done, 2026-09-26, by ferrix-55b with T0 | "Give the restart policy a crate of its own that allocates nothing"; "Restart drivers by the service manager's policy, and report how they died" |
| L12 | done, 2026-09-27, as built in §7.3 | "Let pid 1 start devmgr, through a starter the kernel gives it" |
| L13a | built 2026-10-04 on branch `l13-init`, gating (below) | |
| L13b, L13c | wait for network namespaces and seccomp's S3 | |

All of L1 to L12's 81 points are spent. L11 put `devmgr` on the restart
policy, which moved into `src/lib/init/restart` because `devmgr` has no
allocator; its start limit became systemd's fixed window. The customer
counts the init done at L11 (2026-09-26). L12 (2026-09-27) has pid 1 start
`devmgr` through a starter under `ferrix.devmgr=init`, which every image
that boots init now sets. `sshd` runs under socket activation in L9's gate
since 2026-09-27. L13a, the sandboxing keys that need only mount namespaces and
`prctl`, is built (2026-10-04); L13b and L13c wait for network namespaces and
seccomp.

**L1, as built (5 points).** `src/lib/init/svc` is on `main`: `no_std` with
`alloc`, `forbid(unsafe_code)`, 52 host tests, a Miri step in CI and in
`cargo xtask check --miri`, and the `svc_unit` fuzz target with a seed
corpus. What it does, module by module:

* `ini`: §4.1's syntax, read as systemd's `conf-parser.c` reads it,
  including the corners: a comment line in the middle of a continued line is
  dropped, an escaped backslash does not continue, a continuation at the end
  of the file is kept, CRLF and a byte-order mark are read, and a section
  header without its bracket refuses the file. Every other fault is a
  warning in systemd's words.
* `source`: the three directories as a `Source` the backend fills with
  `add(layer, path, Entry)`, where an entry is a file's bytes, `Masked` (a
  link to `/dev/null`) or `Alias(name)`. `Source::load(name)` does the rest:
  the highest file under the name, then under its template; aliases,
  instantiated through templates; drop-ins of the unit, its aliases and its
  template, a higher layer's file hiding a lower one of the same name,
  applied in file-name order; `.wants/` and `.requires/` links; specifiers.
* `name`, `specifier`, `value`, `exec`: names with templates and slice
  parents, path escaping for mounts, `%i %I %n %N %p %P %j %J %f %t %S %C %L
  %E %%`, booleans, time spans, base-1024 sizes, percentages, signals,
  quoted and C-escaped words, and `Exec…=` lines with their `-@:+!` prefixes
  and `;` separators.
* `unit` and `kind`: `[Unit]` (the nine dependency keys, seventeen
  conditions and assertions with `!` and `|`, start limits) and `[Install]`,
  then the `Kind` trait with the six kinds of version 1 and `.socket`, and
  every key of §4.4.

**What building L1 changed.**

* `Kind::parse` takes the unit's name as well as its section, because a
  mount's `Where=` must match its name. The trait gained `section()` and
  `needs_file()`; `implied` and `advance` come with L2.
* Slices, scopes and builtins load with no file. A builtin's name is its
  contract (§7.2), and the manager makes every slice a `Slice=` path names.
* An empty unit file masks, as it does in systemd.
* Specifiers are expanded in every value when the unit loads, except in the
  six resource keys, whose `%` is a percentage. systemd expands none there
  either. `%H`, `%m`, `%u` and the other specifiers that need the machine are
  refused by name, and the assignment carrying one is dropped with a
  warning.
* A link to a unit file under the link's own name is the backend's to
  follow: it hands in the file's bytes. Only a link to another name, an
  alias, or to `/dev/null` reaches the crate as a link.
* `StandardOutput=journal`, `kmsg` and their `+console` forms read as
  `log`, since the log reaches the console (§10). `console` is Ferrix's own
  value.
* `NotifyFd=` is a service key: the descriptor §5.3's readiness line is
  written to.
* The sandboxing keys of L13 warn by name. Type-wide drop-in directories
  (`service.d/`) are not read.
* Conditions are parsed into `Condition` values, and `conditions_hold`
  combines results. The tests themselves look at the machine, so they are
  the backend's to run.

**L2, as built (8 points).** `Manager` in `src/lib/init/svc`: `step(event, now)`
returns the actions, and `deadline()` says when the next `Timer` is due. It
loads units as they are named (at `Boot`, every unit the directories have),
resolves their dependencies with each kind's implied and default ones, and
turns a request into a transaction of *operations* (§4.3): pulled in along
`Requires=`, `BindsTo=`, `Wants=`, `Requisite=` and `Conflicts=`, stopped
along what requires or is part of a stopping unit, checked for units both
started and stopped, cut free of ordering cycles by dropping an operation
only `Wants=` pulled in (with a log line), refused on a cycle of essential
ones, and merged into the queue in `replace`, `fail`, `isolate` or
irreversible mode. An operation starts when no operation it is ordered after
is queued, so what is unordered starts in the same step. The slice tree is
built from `Slice=` down from `-.slice`, a `MakeGroup` for each level with
its `Limits`. Services have systemd's state machine: cleaning, start-pre,
start, start-post, running, exited, stop, stop-sigterm, stop-sigkill,
stop-post, auto-restart; `simple`, `exec`, `oneshot` and `forking` are
driven through it, and `notify` and `native` reach running on `Ready`. A stop
sends `KillSignal=` by `KillMode=`, writes `cgroup.kill` after
`TimeoutStopSec=`, and is done when the cgroup is empty (`Emptied`), not when
the main process exits. `Restart=` follows systemd's table, with
`RestartSec=` doubled per restart in a row up to 32 times, and the start
limit counts every start in systemd's fixed window; the policy is
`restart::Policy`, since L11 in `src/lib/init/restart`, which takes
`Option<Instant>` and is a pure count without a clock, for `devmgr` (L11).
Boot starts `default.target` or `ferrix.target=`'s, and isolates
`rescue.target` if that cannot start or fails. Shutdown stops everything
that conflicts with `shutdown.target` in reverse order, writes `cgroup.kill`
in every cgroup left, deepest first, waits up to 90 seconds for them to
empty, and then asks for `Power`. Scopes (`Request::Scope`, `Move`) and the
directory's OPEN (`Route`, `Refuse`, starting the provider first) are in the
core already, for L5 and L8 to wire up. 92 host tests replay event scripts:
boot order and parallelism, both kinds of cycle, the slice tree, backoff to
the cap, the start limit and `reset-failed`, stop escalation in all three
kill modes, shutdown order and the final kill, a forking service
deactivating until its cgroup empties, rescue, conditions, conflicts,
`BindsTo=`, `OOMPolicy=`, isolate, restart, `fail` mode, the directory and a
scope. Miri runs them, and a second fuzz target, `svc_manager`, drives the
manager with event scripts in any order and requires that a cgroup is made
once, removed only when made, and spawned into only when made.

**What building L2 changed.**

* §3's lists grew. `Event` gained `Boot` (the backend has mounted cgroup2
  and moved itself into `init.scope`), `Execed` (the exec pipe closed, for
  `Type=exec`), `SpawnFailed`, `MainPid` (a `forking` service's daemon, for
  L7) and `Unmounted`. `Action` gained `Move` (a scope's processes into its
  cgroup) and `Refuse` (§6's REFUSED). `Request` carries systemd's job mode.
* Conditions are run by a `Probe` the backend gives `Manager::new`, when an
  operation begins, since they look at the machine. A failed condition skips
  the unit, and what is ordered after it still starts.
* `Kind::implied` is on the trait, with each kind's default dependencies.
  §4.2's `advance` is not: each kind's state differs, so the state machines
  are modules of the manager, chosen by the unit's type, and a new kind adds
  one there as well as its trait implementation.
* The default dependencies are §4.3's for services, and systemd's for the
  rest: slices, scopes and targets conflict with and are before
  `shutdown.target`, and a target is after what it wants. Mounts have none,
  because shutdown's last step unmounts (§8.2). `poweroff.target` and
  `reboot.target` pull in `shutdown.target` themselves.
* **`drivers.slice` is never killed.** §8.2's step 1 kills every cgroup
  beside `init.scope`, but the block driver that step 2's `sync` needs lives
  in `drivers.slice`. The manager adopts it, as §7.3 says, and never makes,
  stops, removes or kills it; `-.slice` and `init.scope` are the same.
* A stop that has to escalate ends `failed`, with the result `timeout`, as in
  systemd. `KillMode=mixed` writes `cgroup.kill` at the timeout, not when the
  main process exits. With `KillMode=process` the next start first kills
  what the last run left (the `cleaning` state), as §5.4 asks.
* The backoff starts again at a requested start and at `reset-failed`.

**For the init program's author (L4).** The loop, in outline:

```rust
let mut source = Source::new();          // walk the three directories
source.add(Layer::Image, "getty@.service", Entry::File(bytes))?;
let mut manager = Manager::new(source, Box::new(probe), Options { target });
let mut actions = manager.step(Event::Boot, now());
loop {
    for action in actions { backend.perform(action) }   // map names to fds
    let event = backend.wait(manager.deadline());       // epoll, timeout
    actions = manager.step(event, now());
}
```

Every `Action` names a `UnitId`; `Manager::group(unit)` gives its
`GroupPath`, relative to the cgroup2 mount, and `Manager::name(unit)` its
name. `MakeGroup` is `mkdir` and the limit files (and, for a slice,
`subtree_control`); `RemoveGroup` is `rmdir`; `Spawn` is `clone3` into
`spec.group`, answered by `Spawned` (or `SpawnFailed`), then `Execed` when
the exec pipe closes; `Move` writes `cgroup.procs`; `Signal` goes to one pid
or every pid in `cgroup.procs`; `KillGroup` writes `cgroup.kill`; `Log` is a
line to print as `  init     <line>`; `Power` is §8.2's steps 2 and 3. Every
reaped pid goes in as `Exited`, the manager's or not; every cgroup it made
reports `Emptied` when `populated` drops to 0. A `SIGTERM` or `SIGINT` to
pid 1 is `Request::Poweroff` from a client of the backend's own. What needs
the machine stays the backend's: expanding `$VAR` in a command whose
`expand_environment` is set, reading `EnvironmentFile=`, looking up `User=`
and `Group=`, opening `TTYPath=`, and a `Probe` for the conditions.

**L3, as built (3 points).** K0: `ferrix.init=<path>` on the kernel command
line (`CMDLINE.TXT`, or U-Boot's `bootargs` on a board) starts pid 1 from
that file in the switched root, with the path as its only argument and
`PATH=/bin HOME=/ TERM=dumb` as its environment; a `#!` script runs under
its interpreter. The kernel prints `init     starting <argv>`, then
`init     <path> exited with <status>`. A missing or unstartable file prints
`init     ferrix.init=<path> could not be started: <why>; falling back to
the built-in program`, and a path that is not absolute is refused when the
option is read. `cargo xtask build`, `run` and `test-boot` take
`--init-path <PATH>`, which writes `ferrix.init=<PATH>` into `CMDLINE.TXT`;
`qemu::init_option` builds the same option for a test (§8.1 for the order
of the defaults). K7: `reboot(2)` calls `power::sync_disks` before power-off,
halt and restart. `ferrix.onexit=panic` makes init's exit panic with
`FX-1501` after the disks are committed (§8.3).

`cargo xtask test-shell` is the gate (`tools/common/xtask/src/init_file.rs`). After its
built-in boot it boots the same shell and script from files,
`ferrix.init=/etc/shell-test`, and requires the kernel's `starting` and
`exited with 7` lines and no fallback. Under busybox (`--init`) it then
writes `/data/k7` on a fresh volume and runs `poweroff -f -n`, which skips
busybox's own `sync`. A second boot of that volume, under
`ferrix.onexit=panic`, must read the file back and then panic with
`FX-1501`. A test boot has no root disk and so no committer, so only the
call can have committed the file. The three negative controls fired by the
checks' own messages: a wrong path (the fallback line, then "the kernel
fell back to the built-in shell"), no `sync_disks` in `reboot(2)` ("/data/k7
did not survive poweroff -f -n"), and no `ferrix.onexit=panic` ("did not
panic with FX-1501"). Under zinc only the first boot runs, because zinc
cannot make the call.

**L4, as built (10 points).** `src/user/system/linux/init/` is a workspace of its own beside
zinc's and built the same way, a static musl program linked by rust-lld on
any host, for all three architectures: `/sbin/init`, `/sbin/getty` and
`/lib/ferrix/generators/getty-generator`, with the units of `src/user/system/linux/init/units` in
`/lib/ferrix/units`. `cargo xtask check` runs its formatting, clippy and 8
host tests by default, as it runs the compositor's.

* **The loop** is §9's, over L2's manager. Init ignores every signal but
  `SIGCHLD`, `SIGTERM` and `SIGINT`, which it blocks and reads through a
  signalfd, and becomes a subreaper. It mounts a tmpfs on `/run` and cgroup2
  on `/sys/fs/cgroup`, moves itself into `init.scope`, enables the
  controllers the root offers, runs each generator with a deadline of 5 s,
  reads the three directories into a `Source` and steps `Boot`. Then it waits
  in one `epoll_wait` whose timeout is `deadline()`, and turns each wake into
  events in the order the manager wants them: exec reports first, then every
  child reaped, then every cgroup that emptied, then the timer. If cgroup2
  cannot be mounted, init says why and becomes `/bin/sh -i` on the console.
* **Spawn** is §5.2. Everything the child needs is worked out before
  `clone3(CLONE_INTO_CGROUP)`: the program on the search path, `$VAR`
  expanded as systemd expands it, `User=` and `Group=` from `/etc/passwd`
  and `/etc/group`, `EnvironmentFile=` read. A failure there is
  `SpawnFailed`, and the child allocates nothing. The child unblocks its
  signals, resets every disposition, leads a session of its own as under
  systemd, takes its terminal for a `tty` stream, sets up its three streams,
  changes directory and user, and calls `execve`. A pipe closed on exec says
  how that went: end of file is `Execed`; otherwise the child writes the
  step and its errno, init says so in a line, and the child ends with
  systemd's exit code for the step (203 for `execve`). Output to `log` goes
  to the console until L6 gives the log a pipe.
* **`Emptied`.** Init believes a cgroup populated from the spawn or move
  into it, as the manager does, and sends `Emptied` when a cgroup believed
  populated reads `populated 0`. It reads every cgroup's `cgroup.events`
  after every wake, not only those that raised `EPOLLPRI`. The read at offset
  0 is what clears the event, and a process spawned and gone between two
  waits leaves no event behind.
* **`Power`** is §8.2's steps 2 and 3: `sync`, `/`, `/data` and `/home`
  remounted read-only (refused with `EINVAL` today, and said), the rest
  unmounted in reverse order, and `reboot(2)`, which commits each disk anyway
  (K7).
  `SIGTERM` or `SIGINT` to pid 1 is `Request::Poweroff` from a client that
  gets no answer. `Route`, `Refuse` and `Reply` wait for L6 and L8.
* **`getty TTY`** calls `setsid` (an `EPERM` is ignored), opens the
  terminal, takes it with `TIOCSCTTY` 1, puts it on descriptors 0, 1 and 2,
  prints `Ferrix <host> on <tty>`, and becomes `$SHELL`, `/bin/sh` by
  default, as a login shell. There is no `login` yet. `getty-generator`
  links `getty@<name>.service` into `multi-user.target.wants` for each
  `console=` on the command line, and for `console` when there is none,
  which is always today: there is no `/proc/cmdline`.
* **The units** are the targets, `getty@.service` (`Type=exec`,
  `Restart=always`, `SendSIGHUP=yes`, `TimeoutStopSec=5s`), `rescue.target`
  with `rescue.service`, and `default.target` as a link to
  `multi-user.target`.

The kernel's part is `/proc/<pid>/stat`. It now gives `pgrp` and `session`
from the process, and, when the console is the controlling terminal of the
process's session, `tty_nr` 1281 (5:1) and the foreground group as `tpgid`.
A pseudo-terminal's session reads as having none, since nothing maps a
session to its pty.

**The gate.** `cargo xtask test-init` (`tools/common/xtask/src/init.rs`) is §15's
stages one and two, and passes on all three architectures. It boots a
kernel with no program built in, `ferrix.init=/sbin/init`, zinc at
`/bin/sh`, its own units in `/etc/ferrix/units` and a fresh blank volume at
`/data`, and types at the prompt the getty gives. Every check is on a line
the kernel or init printed, or on a line the shell printed in answer. A
marker is built from a variable (`echo "$m-self $s"`), because the console
echoes the typed line.

* Stage one: `multi-user.target` becomes active and the getty prints its
  banner. The shell's own `/proc/self/stat` must show init as its parent,
  its own session and process group, the console as its controlling
  terminal (1281), and its own group in the foreground. `$TERM` must be
  `dumb`, which only the test's drop-in for `getty@.service` sets.
  `flaky.service` exits 1, is restarted by `Restart=on-failure`, and ends
  `failed (start-limit-hit)` on its third start. No unit file may draw a
  warning. `kill -TERM 1` must stop `multi-user.target`, then
  `getty@console.service`, then `basic.target`, then `sysinit.target`, and
  the kernel must say `reboot: Power down`. `btrfs check` of the volume,
  written from the prompt just before, must find nothing wrong.
* Stage two: `forker.service` is a oneshot that remains after its main
  process exits. It leaves behind a grandchild that writes its own pid and
  blocks on `read x < /dev/ptmx`. That pid must be in the service's
  `cgroup.procs`. Killing `anchor.service`'s main process from the prompt,
  by the pid it wrote, must stop `forker.service` through `BindsTo=`, and
  `/proc/<pid>` of the grandchild must then go.

Both negative controls fired, each on its own check alone, on x86-64 under
TCG. The kernel without the `stat` change failed stage one with "its
controlling terminal is Some(0), not the console (1281); the console's
foreground group is Some(-1), not the shell's". `KillMode=process` on
`forker.service` failed stage two with "stopping forker.service left its
grandchild 356 alive", after 20 looks 1.5 s apart.

**What building L4 changed.**

* **The manager logs a unit's warnings as it loads it**, as
  `file:line: message`. Before, they were kept in the loaded unit and never
  said. The test's own drop-in, `Environment=TERM=dumb "PS1=init-test%# "`,
  lost the whole assignment to the specifier `%#`, and nothing said so. It is
  `%%#` now, and `test-init` fails on any warning about a unit file. One
  new host test, and one for a remaining oneshot stopped through `BindsTo=`
  under each kill mode.
* **`sysinit.target` names `Conflicts=shutdown.target` itself.** With
  `DefaultDependencies=no` it has no default conflict, and shutdown stops only
  what conflicts with `shutdown.target`, so the first boot powered off with it
  still active. systemd's own `sysinit.target` has the same line.
* **Init's lines reach the console between the shell's.** A line of init's
  may follow a prompt on the same line, so the gate looks for init's lines
  anywhere in a line, not only at its start. Its first negative control
  failed on the wrong check before that was fixed.
* As found before building: `MS_REMOUNT` is `EINVAL`, and init says so and
  goes on. Init's files go only into images that name `/sbin/init`, which is
  `test-init`'s alone. The gate uses zinc's builtins only, so it runs on
  AArch64 and ARMv7-A without uutils.

**L5, L6, L7 and L9, as built (19 points).** Four landings in one
branch, since each is a part of the same event loop, gated together by
`test-init`'s stages three to six below on all three architectures.

* **L6, control and the log.** `/run/ferrix/control` is a stream socket,
  mode 0666; `SO_PEERCRED` says who connected. Anyone may call `status`,
  `list` and `log`; everything else is root's, but for a scope a user makes
  of their own processes under their own `user-<uid>.slice`. A connection
  carries one call and init's answers, in `src/lib/init/svc-proto`'s records (a
  length, a tag, fields; 1 MiB at most; fuzzed). Nothing blocks: a
  connection is read as bytes arrive, written as the socket takes them, and
  dropped once 4 MiB of answers wait unsent. `/bin/svc` has systemctl's
  verbs: `status`, `list [--failed]`, `start`, `stop`, `restart`, `reload`,
  `isolate`, `reset-failed`, `poweroff`, `reboot`, `log [-n N]`,
  `daemon-reload`, `enable`, `disable`, `mask`, `unmask`, `set-property
  [--persistent]`, `scope` and `top`, with systemctl's exit statuses (3 for
  a unit not active, 4 for one not loaded). `enable` links what `[Install]`
  names in `/etc/ferrix/units`; `set-property` writes a drop-in
  (`/run/ferrix/units/<unit>.d/50-set-property.conf`, or `/etc` with
  `--persistent`), reloads, and writes the running cgroup's files; `top`
  reads `memory.current`, `pids.current` and `cpu.stat` itself.
  `daemon-reload` runs the generators and reads the directories again. A
  stream whose output is `log` -- the default off a terminal -- is a pipe
  init reads: each line goes to the console as `unit[pid]: line` and into a
  ring of the unit's last 256 lines, which `svc log` reads. The manager
  gained `Request::Reload`, which runs `ExecReload=` with the service active
  throughout.
* **L7, readiness.** A `Type=notify` service gets a pipe on `NotifyFd=` (3
  by default, 3 to 63) and `NOTIFY_FD` saying which; `READY=1` is readiness,
  `STATUS=` what `svc status` shows -- a new `Event::Status`, since
  `Event::Ready` is readiness itself -- and `MAINPID=` the main process. A
  `forking` service's main process is the live pid its `PIDFile=` names,
  or the one process left in its cgroup, told to the manager as soon as the
  first process's exit 0 is reaped. Init's own pipes sit above descriptor 100
  in the parent, so the child's `dup2` onto 3 can never land on one.
* **L9, sockets.** A `.socket` unit's sockets are made by init
  (`ListenStream=` a port, `address:port` or a path; `ListenDatagram=`,
  `ListenSequentialPacket=`, `ListenFIFO=`), stay blocking as systemd hands
  them over, and are watched until readable. With `Accept=no` that starts the
  service, whose main process gets every up socket of its own as 3 and up
  with `LISTEN_FDS`, `LISTEN_FDNAMES` and `LISTEN_PID` -- written by the
  child, which alone knows its pid, into a buffer on its stack -- and the
  socket is watched again when the service is down. With `Accept=yes` init
  accepts, and each connection starts `name@N.service` with the connection
  as its `socket` streams. A socket is before the service it activates and
  before `sockets.target`, which `basic.target` now wants.
* **L5, resources.** The limits were written since L4; what L5 added is the
  kernel's side (stage 13's scoped OOM kill, landed beside it) and init's
  watch of it. Each cgroup's `memory.events` is read after every wake and
  before any child is reaped, so a service the OOM kill ended is `oom-kill`
  and not `signal`, and `OOMPolicy=` then applies. `Delegate=yes` with
  `User=` chowns the service's cgroup and its `cgroup.procs`,
  `cgroup.subtree_control` and `cgroup.threads` to that user, and a cgroup
  is removed with everything beneath it, deepest first, so what a delegate
  made goes with it. Slices and scopes were the manager's since L2; `svc
  scope` is how a scope is asked for.

**The gate.** `test-init` now also types, after stage two:

* **Four (L6):** `svc status` gives `echoer.service`'s main pid and `svc
  log` the line it wrote; `svc restart` changes the pid; `su ferrix -c 'svc
  stop …'` is refused with its reason, exits 1, and changes nothing;
  `set-property TasksMax=7` reaches `pids.max`; `enable` and `disable` make
  and remove the `.wants/` link; `top` lists the unit.
* **Readiness (L7):** `notifier.service` is active on `READY=1` and shows
  its last `STATUS=`; `lazy.service`, which never says `READY=1`, stays
  `activating (start)` with its status; `daemon.service`'s main pid is its
  `PIDFile=`'s.
* **Sockets (L9):** `hello.service` is inactive until `nc` connects to
  `hello.socket`, and then says `LISTEN_FDS=1`, its socket's name and
  `LISTEN_PID` equal to its own pid; `echo ping | nc` to `echo.socket` is
  answered `echoed ping` by an `echo@1.service` instance.
* **Three (L5):** `hog.service`, in `test.slice` with `MemoryMax=16M`,
  grows until the OOM kill takes it and is reported `failed (oom-kill)`,
  while `echoer.service` runs on; `tasks.service`'s forks past `TasksMax=3`
  fail and `pids.events` counts them; a process started at the prompt is in
  `probe.scope` after `svc scope`; `deleg.service`, uid 1000 with
  `Delegate=yes`, makes a cgroup in its own.
* Shutdown is `svc poweroff`, and every unit stops before the targets it is
  after.

Negative controls, scratch edits booted once on x86-64: no `memory.events`
read failed with "hog.service, past its MemoryMax=, was not reported failed
(oom-kill)"; no delegation failed with "deleg.service, uid 1000 with
Delegate=yes, could not make a cgroup in its own"; every uid allowed to
change state failed stage four's refusal checks; `STATUS=` taken as
readiness failed with "lazy.service, which never says READY=1, was not left
activating"; no main pid for a forked service failed with "daemon.service's
main pid is None"; no sockets handed on failed with "hello.service was
started without the socket as sd_listen_fds says it: `hello-fds
pid-ok-0`".

**What building them changed.**

* Services are `After=basic.target` by default, as under systemd (§4.3).
  Without it shutdown stopped `basic.target` before most services.
* A typed `m=x; echo "$m $?"` reads the assignment's status, 0, not the
  command's: the gate saves `$?` first. It looked like `su` losing the
  status until three probes showed it was the line.
* `test-init`'s image carries busybox for `su`, `nc` and `mkdir`, and
  `/etc/passwd` with uid 1000.
* **The getty generator masked every getty on its second run.** `svc
  daemon-reload` runs the generators again; the link from the first run was
  there, `symlink` failed, and the fallback's `File::create` followed the
  link and emptied `/lib/ferrix/units/getty@.service` -- and an empty unit
  masks. Shutdown then never ended. The generator now leaves an entry that is
  there and never opens one to write (`create_new`), with a host test.
* **A running unit a reload fails keeps its settings until it stops.**
  With the getty masked while it ran, the manager had no settings to stop it
  by, and `poweroff.target` waited for ever. systemd can stop a unit it has
  since masked, and so can the manager now; it says so in a line.
* **A socket neither re-arms at shutdown nor spins on a refused start.** A
  connection still in `hello.socket`'s backlog made it readable again the
  moment `hello.service` stopped, and a start refused as "the machine is
  going down" put it back to listening, for ever. Now nothing listens again
  once shutdown has begun, and a socket whose service cannot start -- refused,
  or its start limit spent -- fails and closes, as systemd's does.
* **sshd, done 2026-09-27.** §13 names "sshd activated on connect" as L9's
  gate. The sshdt port carries `listen-fds.patch`: its binary takes fd 3 when
  `LISTEN_PID` is its own and `LISTEN_FDS` counts one, as `sd_listen_fds(3)`
  has it, and hands it to the library's new `serve_on`, which serves on it
  in place of binding (the library forbids `unsafe`; the one `from_raw_fd`
  is in the binary). On x86-64, where sshdt is built, `test-init` carries it
  with a host key and an authorized key, and `sshd.socket` on port 2200:
  `sshd.service` is inactive until a connection comes, the first one starts
  it with the socket passed and is answered on that same connection with
  `SSH-2.0-...`, and it runs on. sshdt of its own accord binds
  127.0.0.1:2222, so only one that took the passed socket can answer on
  2200. Negative control: sshdt built without the patch binds its own and
  the connection queued on init's socket is never answered.

**K2, K3, K4, K6, as built (2026-09-26, the kernel half of L8).** Four
native calls, numbered in `src/lib/proto/native-abi/src/nr.rs`, and one message
layout in `src/lib/proto/native-abi/src/bootstrap.rs`, so `src/user/system/linux/init/` can take both by
path. A Linux program makes each with `syscall(number, ...)`; a failure is
`-1` and `errno`, as `src/lib/proto/native-abi`'s `status` names it.

* **K3.** `process_give(pid, handle)`, `0x1032`, moves one handle out of the
  caller's table into the bootstrap slot of `pid`, which must be the caller's
  own child (its parent pointer, not its pid, is compared) and must not have
  completed an `execve`. Needs `TRANSFER`; one give per child, ever. Refused,
  with the handle left under its number: `ESRCH` (`NO_PROCESS`) for a pid
  naming no live process or naming a thread, `ECHILD` (`NOT_CHILD`) for
  another's process, `EBADF`, `EACCES` without `TRANSFER`, `EBUSY`
  (`ALREADY_BOUND`) for a second give whether or not the first was taken, and
  `EIDRM` (`BAD_STATE`) for a child that has exec'd with nothing given or has
  ended. `process_bootstrap()`, `0x1033`, moves the slot's handle into the
  caller's table and answers its value, once; every later call, and a call in
  a process given nothing, answers **0**, which is never a handle (not an
  error). `EMFILE` with a full table leaves it for a later call. The slot is
  on the core process beside the handle table, so the handle has no number
  until the program asks, and it survives every `execve` until taken; an
  `execve` seals an empty slot under the same lock a give is judged under, so
  a give racing one lands before it or is refused after it. A slot never
  taken is closed when the process ends.
* **K2.** Every program `init::run` starts -- `ferrix.init=`'s file, the
  built-in program, each command of a list, `/sbin/init` -- gets a fresh
  channel from `init::bootstrap_channel` in its slot, put there by
  `exec::run_init` between load and start. Before it runs, the kernel's end
  carries one message: eight bytes, `FXIN` and a little-endian `u32` version,
  **1**, and no handles (`init_hello()`, recognised by
  `init_hello_version(bytes)`, which accepts any version from 1, since a later
  one only adds after the header). The kernel keeps its end while the program
  runs, so the program's end does not read `PEER_CLOSED`. Pid 1 takes its end
  with `process_bootstrap()`, rights `Rights::CHANNEL`.
* **K6.** `process_status(process, out)`, `0x1034`, writes a `ProcessStatus`,
  two `u32`s: `state` and `value`. `PROCESS_RUNNING` (0) with 0 until the
  process starts to end; `PROCESS_EXITED` (1) with the exit code, 0 to 255;
  `PROCESS_KILLED` (2) with the signal -- 9 for a job's kill, the fault's
  signal for a fault. It stops saying running before the handle asserts
  `TERMINATED`, so a waiter woken by `TERMINATED` always reads an end. Needs
  `WAIT`. `devmgr` does not use it yet: that is its own change.
* **K4.** `port_fd(port, flags)`, `0x101B`, beside the other port calls,
  answers a descriptor that `poll`, `select` and `epoll` report readable
  (`POLLIN`, `EPOLLIN`) while the port has any packet queued, and not
  otherwise; every queued packet wakes a wait on it. Packets are still taken
  with `port_wait`; `read` and `write` are `EINVAL`. `flags` is
  `PORT_FD_CLOEXEC` (1) or 0, anything else `EINVAL`. Needs `WAIT`. It holds
  the port, so it outlives the handle.

The check is one boot line, `initcall`, and one catalogue entry, FX-1502
(`src/kernel/src/syscall/init_calls_check.rs`), on all three architectures. It
reads init's hello through `process_bootstrap` and `channel_read` by number,
and has a ring-3 program take the channel and close it; drives every give
and refusal between a process and forks of it, then has a fork given a
channel end `execve` a program from `/tmp` that takes and closes it and exits
0, while its parent, given nothing, exits with `EBADF` (247) from closing
handle 0; reads a running process, a `SIGTERM`'d one (killed, 15), a job-killed
one (killed, 9) and a program that exited 42; and wakes a five-second
`epoll_wait` on a port's descriptor with a packet queued 50 ms in (50 ms on
x86_64). Each item's negative control, booted once on x86_64, stopped the
boot with FX-1502 on the check's own sentence: execve not sealing the slot
("process_give to a child that had completed an execve was not refused with
BAD_STATE"), the hello not written ("init's bootstrap channel held no message
for it to read"), the recorded signal ignored ("process_status of a process a
signal ended did not say killed, by it"), and the descriptor offering no wake
queue ("a packet queued on a port did not wake an epoll_wait on its
descriptor", after 51 ms with 0 waits woken).

**L8's init side, as built (8 points).** `src/user/system/linux/init/init/src/directory.rs`.

* **Pid 1's channel.** Init takes it with `process_bootstrap` at boot,
  reads the kernel's hello and says `the kernel greeted init, version 1`,
  and holds it; version 1 sends nothing after.
* **The port.** One port, whose `port_fd` descriptor sits in init's epoll
  beside the rest (§9); every service channel and native process is watched
  through it with `object_wait_async`, re-armed after each packet.
* **A channel per service that needs one** -- `Uses=` or `Offers=`, and
  every `Type=native` service. A Linux service's child waits, as the last
  step before `execve`, on a pipe init writes once `process_give` has put
  the service's end in its slot (§5.2 step 3); a give that fails is said,
  and the child runs without. A `Type=native` service is made with
  `process_create` in the job behind its cgroup (`job_for_cgroup`, C8) from
  the ELF file `ExecStart=` names, started with its end by `process_start`,
  and watched for `TERMINATED`; how it ended is read with `process_status`
  (K6) and given to the manager as an exit, under the pid its cgroup lists.
  Its readiness is READY on its channel.
* **The directory.** READY is readiness; OFFER keeps a provider's channel
  for its name; OPEN becomes the manager's `Event::Open`, and its handle
  waits under a token. `Route` forwards the end as CONNECT with the client
  unit's name, down the offered channel or else the provider's bootstrap
  channel; `Refuse` writes REFUSED with the reason and then closes the end,
  so a client that sees its end close finds the reason waiting. A unit's
  channel and offers go when its cgroup does. The messages are fixed
  little-endian layouts in `src/lib/proto/native-abi/src/directory.rs`, which
  allocates nothing, since native programs have no allocator.

**The gate, stage five.** `src/user/system/native/pong` is a native service (READY, then
`pong <name> to <client>` down each CONNECTed end); `pong.service` is
`Type=native` with `Offers=ferrix.test` and wanted by nothing, so only an
OPEN starts it. `src/user/system/linux/init/dirclient NAME` is a Linux client that OPENs a name
and prints `dir-answer: …` or `dir-refused: …`. `asker.service`
(`Uses=ferrix.test`) must print `dir-answer: pong ferrix.test to
asker.service`; `rogue.service` (`Uses=ferrix.other`) must be REFUSED;
`svc status pong.service`'s main pid must be the one process in its
`cgroup.procs`; and the kernel's hello must have been read. Passed on all
three architectures. Negative controls, on x86-64: no `process_give` failed
with "asker.service's OPEN of ferrix.test was not answered by
pong.service: … dir-none"; refusals dropped failed with "rogue.service's OPEN
… was not REFUSED: … dir-silent". Building it found that a refusal must be
sent before the end closes, or the client sees only the close.

**Not done.** `devmgr` still reports 137 for every death; K6 is there for
it to use. Linux services' own OFFERs are kept but no gate offers one yet.

**L10, as built (6 points).** The images a person boots now boot init.

* **`cargo xtask run`**, with no program named, builds a kernel with
  nothing in it and an initramfs with `/sbin/init`, its units, zinc, the
  utilities and the ports, and `ferrix.init=/sbin/init` on the command line:
  the console gets a getty and a zinc session, and `svc` drives the machine.
  `--init <program>` still makes that program pid 1, as it asks.
* **The desktop** -- every `test-compositor` boot, `run-compositor`, and a
  board's card -- is built the same way by `compositor::build_parts`: init
  is pid 1, and `/etc/ferrix/units` in the image adds `hyprix.service`
  (`Restart=on-failure`, its output on the console), links it into a shipped
  `graphical.target`, and points `default.target` at it. An image with no
  shell masks the getty, which would only fail. A card gets
  `ferrix.init=/sbin/init` in `FERRIX/DEFAULTS.TXT`, beside
  `ferrix.checks=skip`, since its `CMDLINE.TXT` is its owner's.
* **hyprix** asks init for a scope for each program it starts, on a thread
  of its own with a two-second limit: `app.slice/app-<name>-<pid>.scope`
  (`src/user/system/linux/compositor/hyprix/src/scope.rs`). With no init, there is no socket, and
  the program stays where it started.
* **The compositor gates** count `hyprix.service` failing or being
  restarted as the compositor ending, where they counted pid 1's exit: a
  restart would otherwise hand a test a second compositor's pictures. The
  `desktop` boot also requires that the kernel started `/sbin/init`, that
  `hyprix.service` became active, and that both clients came up in scopes of
  their own.
* **`test-jobs`** types its session at the getty's shell, and ends it with
  `exit`, after which init must give the console a new session.

**L13a, as built (5 points, 2026-10-04).** §4.5 is the design. What is on
branch `l13-init`:

* `src/lib/init/svc/src/kind/sandbox.rs`: the five keys parsed into a
  `Sandbox`, in `SpawnSpec` for every command but a `+` one. 11 host tests
  (`tests/sandbox.rs`): systemd's values, bad values warning and keeping the
  last good one, the filter's merge in both directions, its reset, the
  warnings for an unknown group, a malformed name, a bad action and an
  allow-listed action, the keys not built, and the `+` prefix.
* `src/user/system/linux/init/init/src/sandbox.rs`: the plan in the parent
  and the child's two steps, `enter` and `lock`, with 6 host tests of the
  mount plan and the two refusals.
* `test-init`'s sandboxing stage (§4.5, the gate), with 2 host tests of its
  judge.

**The kernel, checked first.** `MS_REMOUNT|MS_BIND|MS_RDONLY` works on
`main`: N1 made per-mount flags real and N2 the bind remount of one mount
(`syscall/fsctl.rs`, `remount_at`; the `binds` boot line, FX-0886), and the
first boot of the stage showed it from inside a service:
`boxed.service[471]: suid-sh:1: read-only file system (os error 30):
/run/l13-open/boxed`. `unshare(CLONE_NEWNS)` from a forked root child, the
`MS_REC|MS_SLAVE` no-op, tmpfs mounts in the copy and `PR_SET_NO_NEW_PRIVS`
all worked as documented. No kernel gap was found. One difference the code
works around: Ferrix's tmpfs reads no mount options, so `mode=1777` is
followed by a `chmod`.

**What building it changed.**

* `PrivateNetwork=` and `SystemCallFilter=` refuse the unit, where §4.4
  said every sandboxing key would warn and run. A unit asking to be cut off
  the network, or filtered, and run without it would be a sandbox that
  reports itself on and is off.
* `NoNewPrivs:` is not in `/proc/<pid>/status` until S3, so the stage shows
  `no_new_privs` by its effect, a set-uid root shell that stays uid 1000
  beside one that becomes root, and reads the line too once it exists.
* `ProtectSystem=yes` covers Ferrix's top-level `/bin`, `/sbin`, `/lib`
  and `/lib64` (§4.5).

**The gate, and the negative controls.** First boot, x86-64 under TCG, on
the host directly: the stage passed with the rest of `test-init`. The rows
(`check`, `test-init --arch all`) and the controls are listed below as the
pool gives them.

**What the next session does first.** Nothing of L1 to L12 is left. L13b
waits for network namespaces (branch `stage13-netns`) and L13c for
seccomp's S3 (branch `stage13-s3-rebase`); §4.5 says what each changes. `docs/AUTH.md`'s P0 to P0c are done: a native
process runs as its maker, a native service as its `User=`, and a
delegated cgroup's limits stay its delegator's.
