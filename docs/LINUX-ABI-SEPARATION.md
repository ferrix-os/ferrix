# How separate is the Linux layer?

**A review, 2026-10-23, read-only, on `main` b8b8871d6.** The customer
asked: is the Linux compatibility layer truly well separated from the rest
of the kernel, and could it easily be stripped to build a kernel that
offers only the native Ferrix ABI? This document answers with evidence:
a dependency map built with the item-boundary gate's own resolver, a
compile experiment that cuts the layer out, and line counts. Nothing here
changes code on `main`. The experiment is the WIP commit 94a41c2af on the
branch `linux-abi-strip-experiment` (its scripts in c33730632), which is
not for landing.

## Verdict: partly separated

* **In the source tree, it is well separated, and the separation holds up
  when tested.** No core, item or native-ABI product file names a Linux
  module. With every Linux-layer module removed from the build (the
  personality's 52 `syscall/` modules, `fs/`, `net/`, `interfaces/`:
  75,643 product lines), `cargo check` fails in **one product file**,
  `main.rs`, the composition root, at 27 sites. Five more product lines
  (`arch/*/mod.rs`'s re-exports and wrappers of check code) needed removing
  before it built, and then it built on x86-64, AArch64 and ARMv7-A
  (§4). The `Personality` trait (W-5) and the hook lists make this a
  one-way, compile-time-checkable seam, and `check-item-boundary.py` holds
  it on every landing.
* **When the kernel runs, the two are entangled.** The native ABI runs
  on top of the Linux personality. Every native process *is* a Linux
  process (`syscall/process.rs:2193`, the only `impl Host`). It is
  loaded by the Linux ELF loader, ended with Linux's `exit_group`, reads the
  clock with `clock_gettime`, logs with `write(2)`, and is killed through
  the Linux signal path. A kernel without the personality builds, but stops
  at its first boot check. It could not start `devmgr`, and it panics on
  the first user-mode fault (`trap.rs:640`: "a kernel built that way has no
  user programs to fault").
* **The in-kernel evidence is entangled too.** 25 check modules, 25,536
  lines of the core's and the item's own verification code, need a Linux
  program to run: they build a test process with `syscall::exec`/`image`/
  `process`. They do not compile without the layer. Neither does
  `syscall/check` (13,123 lines, the personality's own checks).
* **The core still holds Linux-shaped mechanisms.** Signal frames
  (`signal_frame.rs`, `arch/*/signal*.rs`: 1,889 lines), the Linux number
  tables behind `decode_syscall`, i386's `int 0x80` entry,
  `arch_prctl`/`rt_sigreturn` answered in the x86-64 entry, the vDSO and
  the ARMv7-A sigpage, POSIX signal numbers on the fault path, the core
  `Job` as a cgroup (`ferrix_cgroupfs`), and native status codes that are
  Linux errno values. None of these is large, but each is a Linux concept
  in code the item certifies.
* **Ferrix's own userland is almost all Linux programs.** init, authd,
  sessiond, hyprix, zinc, dhcp, pulsed and nvrm are musl/ferrousli programs.
  Only devmgr, the drivers, drvupdated and the IPC test programs are
  native, and every one of those also makes Linux calls.

So stripping the layer to get a kernel that *builds* is cheap (about 5
points). Stripping it to get a kernel that *boots, runs Ferrix's native
programs and carries its own evidence* is **about 85 points** (a guess,
range 65 to 120), and about a quarter of that is already planned as
`docs/MICROKERNEL.md`'s K5, K10 and K15. A native-only kernel that runs
a *useful system* is M5's work plus a native init and native services.
That is out of reach as a side option.

## 1. Where the Linux layer lives

The kernel crate's product code is about 167,000 lines (`check-item-boundary.py
--report`: core 79,425, item 10,707, load 77,405; the classifier below
counts 166,858). Classified by role with the gate's own classifier and
resolver (§10):

| Class | Where | Product lines | Check lines | Ring |
|---|---|---:|---:|---|
| Linux system calls | `syscall/*` except `mod.rs`, `native.rs`, `program.rs`, `uaccess.rs` (52 modules) | 32,063 | 20,848 | load |
| Linux file model | `fs/**`: VFS glue, tmpfs, pipes, epoll/eventfd/signalfd/timerfd, pty, procfs, sysfs, cgroupfs, devfs, nsfs, pidfd, inotify, btrfs glue, disks | 22,400 | 26,567 | load |
| Linux sockets | `net/**`: sockets, netlink, veth, netns | 5,120 | 1,232 | load |
| Device cores | `interfaces/**`: block ring, net ring, display/DRM, render, input/evdev, audio/PCM, chardev, logctl | 16,060 | 6,628 | load |
| Board glue (not Linux) | `platform/st/stm32mp1.rs`, `platform/google/gs201/usb.rs` | 1,762 | 0 | load |
| Linux in the core | `signal_frame.rs`, `arch/*/signal.rs`, `arch/x86_64/signal/compat.rs` | 1,889 | 0 | core |
| Native entry | `syscall/mod.rs`, `native.rs`, `program.rs` | 3,588 | (counted with the syscalls' checks) | item |
| Native objects | `object/**`, `claim.rs`, `fastpath.rs`, `discovery/devmgr.rs` | 9,261 | 10,878 | core, item |
| Shared core | everything else: arch, mm, sched, user, trap, smp, iommu, device, console, panic, discovery, init, power, uaccess | 72,873 | 29,440 | core, item |
| Composition root | `main.rs` | 1,842 | - | item |

The device cores belong to both sides. Their driver end is native (a
control channel made by a native call); their client end is Linux (a
`/dev/dri` file, an evdev node, a PCM device). In a native-only kernel
the client end has no user.

**Library crates linked into ring 0 that only the load reaches** (each
one went unused in the experiment's build): btrfs, btrfs-write,
btrfs-vfs, block, partition, net, nettcp, netwire, netlink, procfs,
sysfs, seccomp, ustack, displayctl, renderctl, inputctl, sndctl,
chardevctl, logctl, netring. Together that is **48,739 lines**. Of
these, `ferrix-btrfs` and `ferrix-btrfs-write` (14,830 lines) are in
the *item* ring (2026-10-02), even though only the load ring reaches them.
`ferrix-linux-abi` (12,883 lines) is still linked: the core uses it for
errno, signal numbers and the decode tables (§3).

## 2. Couplings from the Linux layer into the rest

Measured with the item gate's resolver (references by site, product files
only). Every edge below points *from* the Linux layer *into* the
core, item or native code. That direction is the allowed one, and it is
what `docs/MICROKERNEL.md` §4 calls the shim: 1,859 references to 401
items in 56 modules, measured there on 5450ee1fe.

| From → to | Sites | Files | What |
|---|---:|---:|---|
| Linux syscalls → shared core | 747 | 42 | `sched`, `user::space`, `uaccess`, `object::process`, `trap`, `arch` |
| Linux fs → shared core | 316 | 31 | VMOs, page sources, waits |
| Linux fs ↔ Linux syscalls | 215 / 270 | 20 / 23 | fd table, paths, credentials (the two are one subsystem) |
| Device cores → native objects / entry | 178 / 55 | 10 / 8 | channels, ports, `syscall::native::serve` |
| Linux fs / syscalls → native objects | 120 / 73 | 9 / 16 | jobs (cgroups), processes, quotas |
| Linux net → shared core | 53 | 9 | |

That subsystem by subsystem, as the customer listed them:

| Area | Where the Linux side is | How it reaches the rest | Depth |
|---|---|---|---|
| Syscall entry and dispatch | `syscall/linux.rs` (621) | The item decodes the number and hands the call through `Personality::answer` (`syscall/mod.rs:241`). `main.rs:134` composes `dispatch_with::<Linux>`. The native range is split off first (`syscall/mod.rs:136`) | **Shallow**: one trait, one line |
| Early answers in the arch entry | `arch/x86_64/syscall.rs:768-811` | `arch_prctl(ARCH_SET_FS)` and `rt_sigreturn` are answered in the core's entry, before dispatch. i386's `int 0x80` entry is its own `Abi::Compat` (`trap.rs`, `arch::decode_compat_syscall`) | Medium: Linux numbers inside the core |
| Seccomp | `syscall/seccomp*` | The core holds a filter pointer (`trap.rs:330`) and a link-time hook (`trap.rs:401`, `main.rs:142` `ferrix_filter_quiet`) | Shallow |
| Signals and the return to user mode | `syscall/deliver.rs`, `signal.rs`, `kill.rs` | `trap::ReturnPath` (`trap.rs:558`): `needs_attention`, `return_to_user`, `sigreturn`, `fault_signal`. Registered at `main.rs:179`. With none registered, a user fault is a kernel panic (`trap.rs:640`). Frames are written by the core's `arch/*/signal.rs` | **Deep**: the core has no other way to end a faulting or killed thread |
| Processes and threads | `syscall/process.rs` (`impl Host`, :2193), `thread.rs`, `family.rs` (clone/fork) | The core's `object::process::Host` (`object/process.rs:777`) and `sched::UserThread` (`sched/task.rs:45`) are traits. Only the Linux `Process` implements them, so every native process is one | **Deep** |
| Futex | `syscall/futex.rs` | No core primitive. K5 (wait on a user address) is designed and WIP on `mk/k5` | Native has none |
| exec / ELF loading | `syscall/exec.rs`, `image.rs`, `load.rs`, `launch.rs` | Native `process_create`/`process_start`, `devmgr`'s start and pid 1 all go through `syscall::native::Processes` (`syscall/native.rs:244`), which `syscall::launch::install` registers (`main.rs:1388`) | **Deep**: the only loader is the personality's |
| VFS and fd tables | `fs/**`, `syscall/fd.rs`, `file.rs`, `path.rs` | `init::Launcher` (`init.rs:160`, opens `/sbin/init` through the VFS), `devmgr::register_reader` (`discovery/devmgr.rs:104`, reads `/sbin/devmgr` and drivers), `power` flush hooks, `syscall/program.rs:27` (`ferrix_vfs::OpenFile`), `user/vmo.rs:174` (a page source fails with `ferrix_vfs::Errno`) | Deep at boot; shallow in the code |
| mmap / brk | `syscall/memory.rs` | Argument decoding onto `user::space::AddressSpace`, which is core. Native has `VMO_MAP` but no unmap or protect | Shallow (K2 extends it) |
| Sockets | `net/**`, `fs/socket.rs` | Only the `net_ring` device core and `PORT_FD` touch native | Shallow |
| ptrace / prctl | none / `syscall/attributes.rs`, `thread_area.rs` | `sched/work.rs` reserves a tracer bit; the arch keeps TLS registers | Shallow |
| /proc, /sys, cgroupfs | `fs/procfs*`, `sysfs*`, `cgroupfs*` | `object/job.rs:64` uses `ferrix_cgroupfs` controllers: **the core's job *is* a cgroup** (freeze, ownership and mode of its cgroupfs nodes, `cgroup.max.depth`) | Medium, and it points the wrong way (§3) |
| Namespaces | `syscall/namespace.rs`, `nsproxy.rs`, `pidns.rs`, `userns.rs`, `net/namespace.rs` | Entirely in the load | Shallow |
| vDSO | `syscall/vdso.rs`, `sigpage.rs` | The time page is the core's (`timer.rs` uses `ferrix_vdso`). `arch::vdso_spec`/`sigpage_code` (`arch/aarch64/mod.rs:1610`, `arch/armv7a/mod.rs:1617,1629`, `arch/x86_64/vdso.rs`) are core | Medium |
| Wall clock | `syscall/time.rs` | `main.rs:1277` sets `CLOCK_REALTIME` from firmware through the personality | Shallow, but a native-only kernel loses it |
| Boot path | `fs::init` (initramfs into tmpfs), `fs::root_disk`, `syscall::launch` | pid 1 is a Linux ELF started by `init::Launcher`. devmgr's image is read through the VFS. `register_load` (`main.rs:1387`) stops the boot if no launcher, devmgr reader or native-process maker is registered (`main.rs:1418`), or if a served native call has no handler (`main.rs:1432`) | **Deep** |

## 3. The reverse: native or core code shaped by Linux

The static answer is clean. **No core, item or native product file refers
to a Linux-layer module** (my resolver run: 0 sites from shared core,
native entry or native objects into `syscall`'s Linux modules, `fs`, `net`
or `interfaces`; the compile experiment confirms it). The coupling that
remains is Linux *semantics* in code that is not the layer:

1. **Native processes are Linux processes.** `syscall/native.rs:236-242`
   says so outright: "A native process is loaded from an ELF image into a
   process of the Linux personality's -- it has descriptors and a thread
   like any other". The native runtime agrees
   (`src/user/system/native/rt/src/linux.rs:4-7`): "the kernel gives
   *every* process a descriptor table and a namespace".
2. **The native ABI has no exit, thread, clock, log, unmap or random
   call.** `rt::exit` is `exit_group` (`rt/src/lib.rs:83-87`). The 0x1000
   range (`native-abi/src/nr.rs`) is handles, channels, ports, VMOs, jobs,
   process create/start/give/bootstrap/status, interrupts, I/O mappings and
   the device calls. A deadline is "in `CLOCK_MONOTONIC` nanoseconds", but
   the clock is read through Linux.
3. **Native status codes are Linux errno values.**
   `native-abi/src/status.rs:9` imports `ferrix_linux_abi::errno`.
   `BAD_HANDLE` is `EBADF`, `PEER_CLOSED` is `EPIPE`, and so on, by design
   ("so that musl's `strerror` prints something"). `ferrix-native-abi`
   depends on `ferrix-linux-abi`. In the kernel, 13 core/item product files
   carry `Errno` (`syscall/native.rs`, `program.rs`, `uaccess.rs`,
   `discovery/devmgr.rs`, `init.rs`, `power.rs`, `trap.rs`, `audit.rs:713`,
   `arch/x86_64/syscall.rs` ...). This is harmless as numbers, but the
   native ABI cannot be described without the Linux crate.
4. **The core's `Job` is a cgroup v2.** `object/job.rs` (50 mentions)
   holds `cgroup.freeze`, `cgroup.events`, `cgroup.max.depth`, and the
   owner and mode `chown`/`chmod` gave its cgroupfs nodes, through
   `ferrix_cgroupfs::controllers`. The native call `JOB_FOR_CGROUP` exists
   for init.
5. **The fault path speaks POSIX signals.** The three `arch/*/trap.rs`
   files map faults to `SIGSEGV`/`SIGBUS`/`SIGILL`/`SIGFPE`/`SIGTRAP` with
   `si_code`s (`trap.rs:768` `SIGBUS, BUS_ADRERR`). There are 145 such
   uses in 19 core files. A native fault has no other outcome (no K4
   exception channel).
6. **Linux-only machinery in the arch layer.** Signal frames
   (`setup_signal_frame`/`restore_signal_frame`, 1,889 lines including
   i386's), `decode_syscall` and `decode_compat_syscall`
   (`arch/x86_64/mod.rs:572,626`, `aarch64/mod.rs:266`,
   `armv7a/mod.rs:244`), restart-syscall numbers written into frames
   (`arch/x86_64/signal.rs:197`), `arch_prctl`, the i386 GDT TLS slots
   (`arch/x86_64/gdt.rs:59`), the AArch64 and ARMv7-A hwcaps
   (`ferrix_linux_abi::hwcap`), the vDSO spec and the sigpage.
7. **Product files carry Linux test programs.** `arch/*/mod.rs` hold the
   hand-assembled programs the boot checks run (`exit_group(98)`, a
   `clone3` into a cgroup), which is why "cgroup" and "exit_group" show
   up 13 and 24 times in each architecture's facade.
8. **Smaller items.** `sched/work.rs` reserves a `ptrace` stop bit and the
   seccomp `FILTERED` bit. `user/space.rs` keys shared futexes. The pid
   allocator follows Linux's `alloc_pid` (`object/process.rs:45`), which is
   neutral enough.

## 4. A build switch: none today; the experiment

**What exists.** No Cargo features (`kernel_cargo_features` is 0 by rule;
MICROKERNEL §4 and §12 say switches are `--cfg`s that xtask sets). There
is a flag, `personality`, in `tools/common/data/build-flags.toml`, with
values `kernel` ("today", built) and `user` (`linuxd`, lands M5, **not
built**). The kernel reports it (`build_flags.rs:53`), but no code is
selected by it. There is no `none` value. `fs=kernel` requires
`personality=kernel`.

**The experiment** (worktree `.claude/worktrees/linux-abi-sep`,
`CARGO_TARGET_DIR` of its own, the `present` profile's flag words in
the environment, `cargo check -p ferrix-kernel`, about 7 s a run):

| Step | Result |
|---|---|
| A. `main.rs:134` composes a `NoLinux` personality (every Linux number `ENOSYS`) | **0 errors.** 57 items become dead, mostly in `syscall/family.rs`. Almost all of the personality stays reachable through the boot checks and the load's registrations |
| B. Undeclare the 52 personality modules of `syscall/mod.rs`, and `fs`, `net`, `interfaces` in `main.rs` | **288 errors**: 27 in `main.rs`, all the rest in check files. **No other product file fails** |
| C. Stub `main.rs`'s load calls (filesystem bring-up and checks, net, btrfs, disks, the `register_load` installs, `deliver::install`, the seccomp filter, `set_boot_time`, the boot framebuffer note) | `main.rs` clean; errors left only in checks |
| D. Undeclare each failing check module, cascading, and remove its product wrappers | 25 modules cut, 38,659 lines (`syscall/check` 13,123; then the core's and item's own: `object/check` 4,018, `arch/x86_64/switch/check` 2,958, `arch/armv7a/switch/check` 2,526, `stages_check` 2,272, `object/fast_path_check` 1,765, `arch/x86_64/trap/check` 1,448, `object/domain_check`, `device/config_check`, `iommu/domain_check`, `object/write_read_check`, `object/pin/check`, `object/alloc_check`, `user/rmap_check`, `user/madvise_check`, `service_check`, `object/quota_check`, `user/alloc_check`, `arch/x86_64/paranoid/check`, `arch/armv7a/trap/check`, `object/edge_check`, `object/format_check`, `arch/aarch64/trap/check`, `object/device_copy_check`, `arch/stage_check`). Product lines removed: `drive_native_words`/`drive_system_call` re-exports in `arch/{mod,aarch64/mod,armv7a/mod}.rs`; `check_x87_controls` and `check_machine_check_enabled` in `arch/x86_64/mod.rs` |
| E. Build on three architectures | **`cargo check` passes on x86_64-unknown-none, aarch64-unknown-none-softfloat and armv7a-none-eabi** |

After the cut, 20 kernel dependencies are unused (48,739 library lines,
§1). There are 567 dead-code warnings in product files: 277 functions,
103 constants and 34 structs that only the Linux layer or the cut checks
used. The largest groups are `arch/x86_64/signal.rs` (50),
`arch/x86_64/mod.rs` (47, the test programs), `user/space.rs` (35),
`object/process.rs` (27), `sched/` (~60), `init.rs` (15) and
`discovery/devmgr.rs` (13).

**What it means.** Making the cut a real `--cfg` (`personality=none`) is
mechanical. It needs `#[cfg]` on the 3 module declarations in `main.rs`,
the 52 in `syscall/mod.rs`, about 15 sites in `main.rs`, and the 25 check
modules. The image would not boot usefully: `register_load`'s runtime
checks (`main.rs:1406-1437`) stop it because nothing can start a native
process. That is §7's work.

## 5. What the userland needs

| Program | Lines | ABI | Native calls it makes | Linux calls it makes |
|---|---:|---|---|---|
| devmgr (`native/devmgr`) | 2,273 | native (`rt`) | all of its work | `exit_group`, `clock_gettime` (4), `write(2)` |
| drivers (`native/drivers`, 17 + `native/driver`) | 19,965 | native | rings, IRQs, I/O mappings, pins, device config | every one: `exit_group`. Most: `clock_gettime`, `write`. efifb and intel: `munmap`. intel and usbdev: `reboot`. vport and usbdev: **Unix sockets** |
| drvupdated | 193 | native | | Unix socket server (`socket/bind/listen/accept4`, `SO_PEERCRED`) |
| channel-echo, pong, ipc-bench, ipc-equiv | ~1,300 | native | channels | the benches use `openat/lseek/read` |
| init, svc, getty, dirclient, drvupdate (`linux/init`) | 7,552 | **Linux** (musl, std) | `DEVMGR_START`, `AUDIT_READ`, directory, `JOB_FOR_CGROUP` (4 files) | everything else |
| authd, login, su, passwd (`linux/auth`) | 6,347 | **Linux** | one file (`authd/src/seat.rs`) | everything else |
| hyprix, sessiond and the compositor tree | 173,691 | **Linux** | `sessiond/src/authd.rs` | everything else, including DRM, evdev and Wayland over Unix sockets |
| dhcp, adbd, pulsed/media, installer, pkg | ~11,100 | **Linux** | none | all |
| nvrm (`linux/drivers/nvrm`) | 7,286 | **Linux** | `CHARDEV_*` (5 files) | all else |
| zinc, ferrousli, apps (components) | | **Linux** | | |

**A Ferrix-only kernel can run today's devmgr and drivers only after**
(a) `rt` moves `exit`, the monotonic clock, the log line and `munmap`
onto native calls that do not exist yet, and (b) drvupdated, vport and
usbdev move from Unix sockets to channels. It cannot run init, authd,
sessiond, hyprix or the shell at all. Those are Linux programs by design
(`ARCHITECTURE.md` §2), and MICROKERNEL §2.4 keeps them so, under
`linuxd`.

## 6. Line counts and the certified item

| | Product lines | Share of kernel product |
|---|---:|---:|
| Linux layer (syscalls 32,063 + fs 22,400 + net 5,120) | 59,583 | 35.7% |
| Device cores (mixed native/Linux) | 16,060 | 9.6% |
| Board glue in the load | 1,762 | 1.1% |
| Native ABI (entry 3,588 + objects 9,261) | 12,849 | 7.7% |
| Shared core (72,873, the composition root 1,842, and 1,889 Linux-in-core) | 76,604 | 45.9% |
| Kernel crate total (this classifier) | 166,858 | |
| Plus ring-0 libraries only the load reaches | 48,739 | |
| In-kernel checks: the personality's / the core's and item's that need it | 13,123 / 25,536 | |

**For the certified item, stripping the layer:**

* **Removes the dated exception.** Today the `present` profile links the
  uncertified load into ring 0 under `personality=kernel` and `fs=kernel`,
  "until M5's flip" (`profiles/present.toml`). A `personality=none` kernel
  has **no load ring in ring 0**: 90,132 item lines plus the board glue.
  That is the shape `ITEM.md`'s argument already assumes, and the first
  configuration in which the claim would not need CLAIM §3.1's
  freedom-from-interference argument against an in-kernel QM layer.
* **Leaves ITEM.md's rings unchanged** (the layer is all `load`), but it
  strands the btrfs crates (14,830 item-ring lines that nothing would
  reach) and 25,536 lines of core and item checks. Those checks carry
  `Verifies:` tags for core requirements (traps, switch, the paranoid
  checks, objects, quotas, pins, the IOMMU domain checks, device config).
  Each requirement whose only check is cut becomes unverified in that
  profile until the check is re-homed onto a native test program.
  MICROKERNEL §6.2's B5 re-homing (91 ids) covers the load's checks. These
  are the *item's* checks that borrow the load to run a program. They are
  a separate debt, which `KERNEL-CRATES.md` §2 already counts as 26 raised
  check files.
* **Needs its own reference configuration.** Per MICROKERNEL §12.3, that
  means a profile in `certification-item.json`, traceability and coverage
  per profile, the boot line from `cfg!`, and `xtask check` building it on
  three architectures.

## 7. The couplings, ranked by effort to cut

Points are *guesses*, in the backlog's units. The "K" column names the
MICROKERNEL row that already plans the same work.

| # | Coupling | What cutting it takes | Points | K |
|---|---|---|---:|---|
| 1 | Linux dispatch (`main.rs:134`) | compose `NoLinux` under `cfg` | 0.5 | |
| 2 | Module declarations (`main.rs` 3, `syscall/mod.rs` 52) and the load's registrations/bring-up in `main.rs` (~15 sites) | `cfg` them; split `register_load`'s required-hooks check per profile | 2 | |
| 3 | Seccomp hooks (`trap.rs:330,401`, `main.rs:142`) | no filter, `quiet` = false under `cfg` | 0.5 | |
| 4 | The flag value and the gates: `personality=none` in `build-flags.toml` and a profile; `xtask check` builds it on three architectures; item-boundary's profile rule | | 3 | M0 |
| 5 | Served native calls (21, `syscall/native.rs:162`) and quiesce servers: unserved in this profile is ENOSYS, not a boot stop; drivers handle the refusal | | 3 | retired calls (§3) |
| 6 | Wall clock and power: `set_boot_time` into the core; the power-flush and boot-mode registrations without a filesystem | | 2 | |
| 7 | Linux-shaped core under `cfg`: signal frames, `decode_syscall`/compat entry, `arch_prctl`/`rt_sigreturn` early answers, vDSO/sigpage, Linux test programs in `arch/*/mod.rs` | `cfg` plus moving the test programs into check files | 6 | §2.1's table |
| 8 | Userland on native calls: `rt` (exit, clock, log, unmap), then drvupdated, vport, usbdev, intel and power/x86 off sockets and `reboot` | | 11 | |
| 9 | Boot without the VFS: pid 1, devmgr and driver images read straight from the initramfs; a native pid 1 (or devmgr as the first process) | | 6 | §5 (boot) |
| 10 | Native ELF loader for the first images in the item, out of `exec/image/load/launch` | | 8 | K15 (keeps exactly this one loader) |
| 11 | Native ABI gaps: process exit, thread create/start, clock read (or a native time page), debug log, unmap/protect, random, wait on a user address, timers | ids, checks, negative controls, three architectures | 15 | K5, K10, K2, §2.1 |
| 12 | A native `Host`/`UserThread`: a process without descriptors, exit and kill without POSIX signals, a fault that ends the process instead of panicking (`trap.rs:640`) | | 10 | K4, K11 |
| 13 | **The core's and item's checks that borrow a Linux program** (25 modules, 25,536 lines): a native test-program harness in place of `syscall::exec/image/process`, then the checks moved onto it; `stages_check` split | | 15 | B5 (for the load's) |
| 14 | Evidence for the profile: traceability, coverage per profile and architecture, boot-line expectations, the consultant's reviews | | 8 | M7 / §12.3 |
| 15 | Native status as its own type, not errno; `Job` without cgroupfs semantics | optional: these are ABI decisions, not blockers | (5) | |
| | **Total for a bootable, evidenced native-only kernel** | rows 1 to 14 | **~85** (65 to 120) | about 25 overlap K5, K10, K15 |

Rows 1 to 4 (about **5 points**) give a kernel that *builds* in a
`personality=none` profile. That is the experiment, made permanent and
gated.

## 8. Point estimate

* **Build-only option** (`personality=none` compiles and is checked on
  three architectures, but does not boot usefully): **5 points**.
* **Ferrix-only-ABI kernel** that boots on four boots, starts devmgr and
  the drivers natively, runs native test programs, and carries its own
  checks and evidence: **about 85 points** (65 to 120, a guess). About 25
  of those are rows the microkernel plan already has (K5, K10, K15 and
  parts of K2 and K4). Some are in flight on `mk/k5` and `mk/k10`.
* **A useful system on it** (a native init, sessions, a display and input
  path without Linux device files): that is MICROKERNEL's M5 to M8, 130 to
  200 points for M5 alone, and it is not a side option.

## 9. Recommendations

1. **Tell the customer "partly".** The source tree is honestly separated
   and the gate keeps it so. At runtime the native ABI stands on the Linux
   personality, and so does the item's own boot evidence. "Strip it easily"
   is true for the build (5 points) and false for a kernel that runs (about
   85).
2. **Add `personality=none` as a flag value now, as the experiment
   committed under `cfg`** (rows 1 to 4). It costs little, `xtask check`
   then stops a new static coupling the moment it lands, and it is the
   earliest build with no load in ring 0. It is exactly the kernel
   MICROKERNEL's `max` and `fast` profiles boot before `linuxd` exists, so
   it is a stepping stone to M5, not a detour.
3. **Re-home the item's checks onto native test programs first (row 13).**
   This is the coupling that matters most for certification. Today the
   item's own claims (traps, switch, objects, quotas, pins, the IOMMU) are
   verified by checks that need the uncertified load to run, so those
   claims rest on it. The harness is needed under every route: the
   microkernel plan, a native-only build, or neither.
4. **Close the native ABI's gaps before more native programs are written
   (row 11).** Every native program today calls `exit_group`,
   `clock_gettime` and `write` through the Linux table. A native exit,
   clock and log are small, and they stop that dependency from growing.
   K5 and K10 are already designed.
5. **Give native processes a host of their own (row 12), with K4's fault
   outcome.** A kernel that panics on a user fault when no personality is
   registered is the clearest sign that the core is not yet independent of
   the layer above it.
6. **Leave errno-valued native statuses and the cgroup-shaped `Job`
   alone for now.** They are ABI choices that cost nothing at runtime.
   Revisit them only if the native ABI is to be specified without the
   Linux crate.
7. **Record the measures.** The class map (§1) and the "no product file
   but `main.rs` names the layer" result could be a mode of
   `tools/common/gen/microkernel-surface.py`, so the next review is a
   command and not a study.

## 10. Reproducing

* Class map and edges: `tools/common/gen/linux-abi/classes.py` on the
  experiment branch (c33730632), `python3 classes.py <checkout>
  [from>to ...]`. It loads `tools/common/check/check-item-boundary.py`
  with importlib, classifies every kernel file by the table in §1, and
  counts resolved references between classes, product files only.
  `tools/common/gen/microkernel-surface.py surface` gives the
  load-to-item surface.
* Compile experiment: check out `linux-abi-strip-experiment` (94a41c2af
  for the cut; c33730632 adds the scripts, with `autocut.py` for the
  cascade and `present-env.sh` for the flags),
  set `FERRIX_BUILD_FLAGS` to the `present` profile's words
  (`profile=present level=present personality=kernel fs=kernel
  mitigations=on partition_speculation=not_applicable ipc_fastpath=off
  restricted_fastpath=not_applicable kernel_memory=heap schedule=fair
  checks=on program_barrier=switch ssbd=always smt=stibp
  runtime_trades=none`) and `FERRIX_FLAG_VALUES` to every declared flag's
  values (with `profile=none,present,fast,max,desktop` and
  `level=none,present,sc2,sc4`). Then run
  `cargo check -p ferrix-kernel --target <t>` for the three targets.
* Line counts: `check-item-boundary.py --report` on b8b8871d6, and `wc -l`
  over the crates named in `src/kernel/Cargo.toml`, `src/` without tests.

## 11. Two ABIs behind one build flag (the customer's follow-up, 2026-10-23)

The customer asked whether the kernel could offer two ABIs, with a build
flag choosing which one, and with both able to coexist. As the
coordinator framed it: one flag with three values. `native` would offer
the Ferrix ABI only, `linux` the Linux ABI only, and `both` would be
today's kernel and the default.

**Short answer.** `native` and `both` are coherent. `linux`, read
literally, is not, because Ferrix's drivers *are* native programs. Read as
"native only for the system's own driver processes, Linux for everything
else", it is coherent and cheap, but it saves attack surface, not code.

| Value | What ring 0 holds | Coherent? | Points (*guess*) |
|---|---|---|---:|
| `native` | core + item (native entry, objects, devices), no load | yes | ~85 (65 to 120); 5 to build only |
| `linux` (literal: no native ABI at all) | core + item minus the native table + the Linux layer + a new Linux-shaped device-access surface | no, see 11.2 | 150 to 250, not recommended |
| `linux` (as recommended: native for driver processes only) | today's code, with the native range closed to every process not given the right | yes | 12 to 18 |
| `both` (made clean) | today's code, with one process model and a personality per process | yes | ~10 on top of `native`'s rows 11 and 12; ~35 alone |
| Wiring, shared by all values | flag, xtask, gates, item boundary per value | | ~8 |

### 11.1 `native`

This is §7's rows 1 to 14, written in terms of the flag:

* `abi=native` puts `#[cfg(not(ferrix_abi = "native"))]` on the 3 module
  declarations in `main.rs`, the 52 personality modules in
  `syscall/mod.rs`, the load's ~15 sites in `main.rs`, and the 25 check
  modules that borrow a Linux program until row 13 re-homes them (rows 1
  to 4, 5 points: the experiment 94a41c2af, made permanent).
* `main.rs:134` composes a personality that answers `ENOSYS` (row 1).
  `register_load`'s required hooks become per-value: no launcher, no
  filesystem flush, and served calls whose answer is `ENOSYS` (rows 2 and
  5).
* It boots only once rows 8 to 12 exist. Those are a native host for
  processes, a native loader for the first images, a boot path through the
  initramfs without the VFS, the native exit, thread, clock, log and unmap
  calls, and `rt` and the socket-speaking drivers moved onto them. Each
  value's gates are in 11.4.

### 11.2 `linux`: cutting the native ABI

**The experiment** is commit 2e420d625 on the local branch
`linux-abi-native-cut-experiment`, from b8b8871d6. It used the same
`cargo check` method as §4.

* **Refusing the native range at the item's entry** gives **0 errors**.
  `syscall/mod.rs:136`'s `native_call` answers `ENOSYS` instead. Only
  `native_call` and one `native.rs` item go dead. The rest of
  `syscall/native.rs` stays reachable through devmgr's start
  (`discovery/devmgr.rs:842` calls `native::dispatch`), the IPC fast path
  (`main.rs:155`) and the boot checks.
* **Removing the native table** (undeclaring `syscall::native`) breaks
  **13 product files**: the eight device cores (`interfaces/*/mod.rs`),
  `fs/cgroupfs.rs`, `fs/portfd.rs`, `discovery/devmgr.rs`,
  `syscall/launch.rs` and `main.rs`. Every break is the same seam: the
  native table's hook lists (`native::serve` for the 21 served calls,
  `register_server`, `control_channel`, `Processes`, `insert_new`). The
  *objects* stay (`object/**`: channels, ports, VMOs, jobs, interrupts,
  I/O mappings, pins). The kernel uses them internally too, for example
  init's bootstrap channel and the device cores' control channels.

**Why a literal "Linux-only" is not coherent.** A driver reaches its
device only through native objects. It gets an `IoMapping` and an
`Interrupt` token by handle, pins DMA memory through `VMO_PIN` into an
IOMMU domain, and has its configuration space mediated by
`DEVICE_CONFIG_*`. All of it is held to the item's device rules (AoU-12,
`verify_config`, the pin quarantine). The Linux ABI has none of this.
Linux's own answer is VFIO (container, group and device files; `mmap` of
BARs; DMA mapping by `ioctl`; interrupts through `eventfd`/`irqfd`). For
`linux` to mean "no native ABI", Ferrix would have to:

* build a VFIO-shaped device surface in the Linux personality, which is
  device authority, today the item's, now reached through the uncertified
  layer;
* port devmgr, the 17 drivers and the driver library to it;
* rebuild every device core's driver end (block ring, net ring, display,
  render, input, audio, chardev, logctl) from native channels onto files;
* find another way for init to start devmgr (`DEVMGR_START`), read the
  audit log (`AUDIT_READ`) and map cgroups to jobs (`JOB_FOR_CGROUP`), for
  authd and sessiond to keep their seat channels, and for nvrm to keep
  `CHARDEV_*`.

That is 150 to 250 points (a *guess*). It moves device authority out of
the item into the load, and it goes against MICROKERNEL's route C, where
the native ABI is the kernel's whole interface. **I recommend not
building it.**

**What `linux` should mean instead: "native for the system's own
drivers, Linux for everything else".** The native range is refused
(`ENOSYS`, audited) to any process without a **native-ABI right**. The
kernel grants that right to the processes it starts as drivers: devmgr,
which it starts under `ferrix.devmgr=kernel`, and whatever devmgr starts
in the drivers job. Today's mixed programs need it as well: init for
`DEVMGR_START` (retired once the kernel starts devmgr) and for
`AUDIT_READ` and `JOB_FOR_CGROUP`, authd and sessiond for their channels,
and nvrm for `CHARDEV_*`. Either the right is granted explicitly, through
a handle init is given, or those calls move behind files.

* **What it saves.** No code: the native entry (3,588 lines) and the
  objects stay, because drivers need them. The gain is attack surface.
  An ordinary Linux program, which is every app, Steam and Wine, cannot
  reach any of the 0x1000 range's ~60 calls. That matters because the
  native ABI is the item's exported interface, and today every Linux
  process can call it.
* **What it costs** is 12 to 18 points:
  * the right on the core process and its grant rule;
  * one check in `dispatch_with` before `native_call`, on the hot path, so
    a single compare;
  * an audit record for each refusal;
  * a boot check with a negative control (a Linux program's
    `channel_create` refused, then allowed with the right);
  * init's four native uses settled;
  * the requirement ids and the consultant's review, since the change is
    in the item.
* **It is worth having in `both` too, as a policy rather than a build
  value.** The same right lets a `both` kernel close the native range to
  apps. In that case `linux` hardly needs to be its own build value: it is
  `both` with the right granted narrowly. See 11.3.

### 11.3 `both`: what makes coexistence clean

Today the two ABIs coexist by accident of history. There is one process
type, the Linux one, and a native process is a Linux process that happens
to make native calls. Coexistence would be clean with four rules:

1. **One process model, with a personality per process.** The core
   process (`object::process::Process`: space, handles, job, exit) is the
   process. A Linux personality is *attached* to it: descriptors,
   credentials, signals, namespaces. A native process has none attached.
   `Host` (`object/process.rs:777`) gets a native implementation beside
   the Linux one (row 12). K11's shared processes and `linuxd` take the
   same shape, so this does not diverge from MICROKERNEL.
2. **Dispatch by personality, not only by number.** `dispatch_with`
   (`syscall/mod.rs:136`) splits off the native range by number, as it
   does today. A Linux number then goes to the caller's attached
   personality, and a process without one gets `ENOSYS`. The native range
   checks the right from 11.2. The composition at `main.rs:134` becomes
   per value: `both` composes `Linux`, `native` composes the `ENOSYS`
   personality. No indirect call is added.
3. **The fault outcome follows the personality.** Today the core's
   `ReturnPath` (`trap.rs:558`) is global, and with none registered a user
   fault panics (`trap.rs:640`). Per process: a Linux process gets
   `fault_signal` (a POSIX signal); a native process gets K4's exception
   channel to its supervisor, or, until K4, ends with a status that names
   the fault. Kill, exit and "must leave a wait" follow the same split
   (`Host::kill`, `syscall/native.rs` `must_leave`).
4. **A process may use both, as a capability.** Drivers use both today
   (native work, Linux `exit`, clock and log), and so do init, authd,
   sessiond and nvrm (Linux work, native channels). MICROKERNEL §2.4
   keeps "a native call goes straight to the kernel, against the calling
   process's own handle table". I recommend keeping mixing allowed, under
   two rules:
   * **Linux processes reach the native range only with the native-ABI
     right (11.2).** Handles are capabilities already, but the creating
     calls (`channel_create`, `vmo_create`, `job_create`) and the
     waits/ports are reachable by any process today.
   * **Native processes stop needing Linux calls.** With native exit,
     clock, log and unmap (row 11), a native process has no Linux
     personality attached, and a Linux number from it is `ENOSYS`. Drivers
     then never touch the Linux layer, which is what `native` needs anyway.

Points: about 10 on top of `native`'s rows 11 and 12, which `both`
needs as well (dispatch by personality, the per-process fault outcome,
the mixing rule with its checks and controls). About 35 if `both` is
done alone.

### 11.4 How the flag is wired

* **Name and values.** (Superseded by §11.6: the flag is `linux-abi =
  on | off`.) A new flag, `abi = native | linux | both`, in
  `tools/common/data/build-flags.toml`, turned by xtask into
  `--cfg ferrix_abi="..."`. That follows the existing rule: a `--cfg`,
  never a Cargo feature (`kernel_cargo_features` stays 0); every value
  declared for `--check-cfg`; `unexpected_cfgs` denied. I advise against
  reusing `personality`. It already means *where* the Linux layer runs
  (`kernel` in ring 0, `user` as `linuxd`), which is orthogonal to
  *whether* it is offered. Folding both into one flag would need values
  like `both-kernel` and `linux-user`. Instead:
  * `personality` gains `none`, which requires `abi=native`;
  * `abi=linux|both` requires `personality=kernel|user`;
  * `fs=kernel` keeps requiring `personality=kernel`, and `abi=native`
    requires `fs=server` or a new `fs=none`.

  The kernel reports the value in its `build` boot line from `cfg!`
  (`build_flags.rs`, as `personality` is today), so the boot rows can
  check it.
* **Levels.** Each value's `allows` follows what it puts in ring 0:
  * `native`: `sc4, present, sc2, none`, because no load is linked.
  * `both` and `linux` with `personality=kernel`: `none`, plus `present`
    under today's dated exception until M5; with `personality=user`, as
    `user` allows.
* **Profiles.** `present`, `desktop`, `max` and `fast` keep `abi=both`
  (the default). A new profile, `native` (or a `--set abi=native` on any
  profile), selects the native-only kernel. `xtask` already takes
  `--set name=value` (`args.rs:869`) and resolves it against the profile's
  rules (`flags.rs`).
* **Gate rows per value.**
  * `both`: today's whole gate table, unchanged.
  * `linux`: today's table, plus one refusal row. A Linux program's native
    call is refused without the right and answered with it, with a
    negative control. Plus a check that init, authd and sessiond still
    work with the rights they are given.
  * `native`: needs an image of its own, with devmgr as the first process,
    the drivers and native test programs in the initramfs, and no Linux
    pid 1. Rows: the four boots (x86-64 KVM and TCG, AArch64, ARMv7-A)
    with the item's checks re-homed onto native test programs (row 13),
    one driver row per device kind that has a native consumer, and the
    IPC rows.

  There is no Linux test, Steam or compositor row for `native`.
* **The check that keeps every value building.** `xtask check` builds
  (clippy) the kernel for every value on all three architectures. Today
  that is one kernel per architecture; with this it is three, about 7 s a
  `cargo check` each on nazuna (measured here), more under clippy. There
  is also one boot row per value in the landing table for the item's
  checks (`native` on its image, `both` and `linux` on today's). A change
  that breaks a value it does not touch is caught at check time, the way
  the experiment showed the seams.
* **The certified item per value.** `check-item-boundary.py` today
  classifies by path and ignores `cfg`. Each value needs the set of files
  actually compiled under it. The gate would read the `cfg` attributes on
  `mod` declarations, or take the set from rustc's dep-info for that
  value's build, and report rings, sizes and edges per value:
  * `native`: the item is core plus item, no load ring, and the load's
    `in_ring0_when` exception does not apply. This is the first value
    whose ring 0 holds no QM code.
  * `both` and `linux`: today's item and load, unchanged, plus `linux`'s
    native-right check in the item.

  The consultant sees one reference configuration per certified value
  (MICROKERNEL §12.3): traceability and coverage per value and
  architecture, a requirement whose truth depends on the value saying so,
  and the per-value file sets in `certification-item.json`. The flag
  itself is an item change (BUILD-FLAGS rule 2), and so are `linux`'s
  native right and `both`'s per-process fault outcome. All three need the
  consultant's review before landing; `native`'s `cfg`s on `main.rs` do
  too.
* **Points for the wiring:** ~8, shared by all values: the flag, the
  profile rules, `xtask check`'s matrix, the item gate's per-value file
  sets, and the boot-line expectations.

### 11.5 Recommendation

Build `abi` with `both` (the default) and `native`. Make "Linux programs
get no native calls unless granted" a per-process right that `both`
enforces, instead of a third kernel. That gives the customer the switch
and the coexistence at about 85 + 10 + 8 ≈ **100 points**, and every row
of it is on MICROKERNEL's path anyway. Add `linux` as a build value only
if a customer needs a kernel that *cannot* be configured to give apps the
native ABI. Then it is `both` with the grant hard-wired to the drivers
job, about 5 more points.

### 11.6 The flag's name: `linux-abi = on | off` (the customer, 2026-10-23)

The customer, on §11.5: "when we didn't have support for Linux only we
could just name the flag something like Linux-Support or linux-abi". The
native ABI is in every kernel, so the flag says only whether the Linux ABI
is there as well:

| `linux-abi` | Was in §11.1–11.5 | Ring 0 | Points |
|---|---|---|---|
| `on` | `both` | the item plus the Linux load, under today's exception until M5 | ~10 on top of `off`'s rows (one process model, per-personality fault outcome, the native right for apps) |
| `off` | `native` | the item only, no QM code | ~85 (65–120); ~5 to only build |

* There is no Linux-only value. §11.2's "drivers only" right is not a
  build value but part of `on`: Linux programs get no native calls unless
  granted (devmgr, the drivers job, init, authd, sessiond, nvrm).
* `--cfg ferrix_linux_abi="on"` or `="off"`, always passed, both values
  declared for `--check-cfg`, the boot line computed from `cfg!`; the row
  in `build-flags.toml` is `linux_abi`. A bare `cfg` left out for `off`
  would let a forgotten flag select a configuration silently (BUILD-FLAGS
  rules 1 and 6; ledger 3218 (9)). `personality` gains `none`, which requires
  `linux-abi=off`; `linux-abi=on` requires `personality=kernel|user`.
* Everything else in §11.4 holds with `both` read as `on` and `native` as
  `off`: the profiles keep `on`; `xtask check` builds both values on all
  three architectures; the item gate reports a file set per value; the
  flag, the native right and the per-process fault outcome need the
  consultant's review before landing.
* The estimate stays about **100 points** (§11.5) less the 5 a `linux`
  value would have cost.

### 11.7 Room for more ABIs: one flag per personality (the customer, 2026-10-23)

The customer, on §11.6: "I would design it in a way that if e.g. in the
future we want windows ABI as well we could add a windows flag there
easily". So the Linux ABI is the first of a kind, not a special case:

* **One boolean flag per foreign ABI**, all of one shape:
  `linux-abi = on | off` now, `windows-abi = on | off` (or any other)
  later, each turned into its own key-value `--cfg ferrix_<name>_abi="on"|"off"`,
  always passed (§11.6, ledger 3218 (9)). The native
  ABI is in every kernel and has no flag. Adding an ABI is: a flag row in
  `build-flags.toml` with its levels and requires rules, a module behind
  its `cfg`, a registry entry, its gate rows. Nothing in the core changes.
* **A personality interface in the core**, which the Linux layer is the
  first to implement, so that the core never names an ABI:
  * syscall dispatch: each process carries the personality it was loaded
    with, and entry dispatches by it (no global hook);
  * the fault outcome per personality (a Linux signal, a Windows SEH
    exception, a native exception channel);
  * the program loader per personality (ELF for Linux, PE for Windows,
    the native loader), chosen at exec by the image's format;
  * per-personality process state behind one native process type (§11.3),
    created and freed by the personality;
  * optional user-visible pieces (vDSO/sigpage for Linux; a Windows
    personality would bring its own).
  This is §11.3's "clean coexistence" made general: the same four seams,
  defined once as a trait, with a registry the kernel builds from the
  enabled flags.
* **Several ABIs at once** (`linux-abi=on` and `windows-abi=on`): each
  process has exactly one personality; the native-call right of §11.6
  stays per process and independent of the personality.
* **A Windows ABI here means NT syscalls in the kernel or in a ring-3
  server**, which is a large undertaking (Wine's approach, NT syscalls
  translated in user space on top of the Linux ABI, already runs Windows
  programs on Ferrix). The design only keeps the door open; nothing is
  planned.
* **Where each ABI runs** (ledger 3219 (11)): one placement flag per
  foreign ABI, `<name>_abi_ring = kernel | user` (Linux's is today's
  `personality`, renamed with its next touch), required when that ABI is
  `on` and refused when it is `off`. The levels follow the placement: a
  foreign ABI in ring 0 allows only `none`; Linux alone also allows
  `present` under BF1's dated exception, which ends at M5; a Windows ABI
  in ring 0 gets no exception. In ring 3 it allows what its user-space
  value allows.
* **The trait is an item interface** (3219 (12)) and gets its own design
  review before code, with 3208 (4) and (5): it lives in the core and
  names no ABI; the registry is a compile-time constant built from the
  cfgs, not a run-time registration; every process has an explicit
  personality from creation (none means native); exec picks the loader by
  image format and refuses an unknown or disabled one; exec never changes
  the native-call right. "Nothing in the core changes" holds only while
  the trait suffices; a change to the trait is an item change.
* **Evidence** (3219 (13)): only named profiles carry a claim; `xtask
  check` builds each foreign-ABI flag both ways on all three
  architectures, not every combination; the item boundary is reported per
  named profile.
* **Cost:** the trait and registry are what §11.3 already costs (~10
  points on top of `off`); generality adds ~2 to 3 points (naming, a
  registry instead of one hook, the flag rows' shared rule). A Windows
  personality itself is not estimated.

Corrections owed to ledger 3208 and 3218 (the consultant's OK IF, (1)-(10)):
§11.6's table says "the item only, no QM code" for `off` and that the
profiles "keep `on`"; both are corrected by (1) and (2): `off` *allows* the
levels, carries no claim until it is a named profile with its own evidence,
still has the board glue and F-76's unclassified crates in ring 0, and
every certified profile sets `linux_abi` explicitly (no default applies to
a certified profile). Which level an `off` profile claims, and when, is the
customer's to decide (3208 (7)).

### 11.8 Each foreign ABI is one crate; the switch is its inclusion (the customer, 2026-10-23)

The customer, for DO-178C: "make the surface for the switches small, so
ideally just move everything in a Linux ABI crate and only make the
include the switch surface, so that if we want to get certified we just
remove the lib from the source and we are good".

**The rule.** Everything that exists only for the Linux ABI lives in one
crate, `ferrix-linux-abi` (under `src/kernel/linux-abi/`, its own package
in the kernel workspace). Nothing outside it names it except the
composition root. The switch surface is exactly two places:

1. the kernel binary's `Cargo.toml`: the dependency on
   `ferrix-linux-abi`, optional, enabled by the `linux_abi` flag;
2. `main.rs`: one registry line that adds the crate's personality to the
   core's compile-time registry (§11.7), behind `cfg(ferrix_linux_abi =
   "on")`.

No other `cfg` on the ABI anywhere in the core, item or native code. A
later `ferrix-windows-abi` is the same shape: one crate, one dependency
line, one registry line.

**The certified source tree has no Linux code in it.** For a certified
configuration the crate directory is removed from the tree, not compiled
out: an export step (`xtask cert-tree`, a TOOLS.md entry) copies the tree,
deletes `src/kernel/linux-abi/` and its two switch lines, and proves the
result: it builds on every certified architecture with `linux_abi=off`,
the item-boundary gate finds no file and no symbol of the crate, and a
grep for the crate's name finds nothing. A negative control (one stray
reference left in the core) must make the export fail. For DO-178C this
means the certified configuration contains neither dead code nor
deactivated code from the Linux ABI (DO-178C 6.4.4.3 and its glossary):
the code is simply not part of the configuration, and the switch points
themselves are absent from it. The configuration index (SCI) names the
exported tree's hash, not the development tree's.

**What has to move into the crate** (from §§3-5 of this report):

| Today | Where it goes |
|---|---|
| the 52 Linux syscall modules (32,063 lines) | the crate |
| `fs/` (22,400): the Linux VFS, `/proc`, `/sys`, cgroupfs, file descriptors | the crate; the native loader and devmgr's start stop going through it first (K15, §11.1 rows) |
| `net/` (5,120): sockets | the crate |
| `interfaces/**` (16,060): device cores | **split**: the native driver side stays in the item, the Linux device-file side (`/dev` nodes, ioctls such as the i915 uAPI front end's kernel half) moves to the crate |
| Linux-shaped code in the core: signal frames (1,889), `decode_syscall`'s number tables, the `int 0x80`/compat entries, vDSO and sigpage, POSIX signals on the fault path | the crate, behind the personality trait (§11.7); what the trait cannot carry is a trait change with its own review |
| the 20 library crates only the load reaches (48,739), btrfs among them | dependencies of the crate only |
| the 25,536 lines of core/item checks that run a Linux test program | re-homed onto native test programs first (3208 (6)(b)); a check that only works with the crate lives in the crate |

**Relation to `docs/KERNEL-CRATES.md`.** That plan already splits the kernel
into core, item, load and the binary along the rings. The Linux ABI crate
is the load crate's Linux part: the load ring splits into
`ferrix-linux-abi` and whatever load code serves the native side (the
device cores' native halves move to the item or stay load, as the
consultant rules). Doing KERNEL-CRATES' steps first makes this a smaller
step, not a parallel one.

**Cost (a guess, to be measured per step):** the crate extraction itself,
with the device-core split and the core's Linux-shaped code behind the
trait, about 20 to 30 points beyond §11's ~100 (some of it overlaps
§11.3's trait rows and KERNEL-CRATES' S3/S4); the export step with its
control and TOOLS.md entry about 3.

### 11.9 The customer's decisions (2026-10-23)

* **Level** (ledger 3208 (7)), the customer's words: "sc4 if it can be
  reached with additional 30SP". The `linux-abi=off` kernel aims at `sc4`
  (IEC 61508-3 SC 4, ISO 26262 ASIL D, EN 50716 SIL 4, DO-178C DAL A
  objectives, IEC 62304 Class C, CC EAL6+) if the work beyond this
  document's estimate (~100 points, §11.5, plus §11.8's 20 to 30) needs at
  most about 30 more points; otherwise `present` without the dated
  exception first. Which of the two holds is measured against MICROKERNEL's
  M-milestones and the work already done on the build-profile branches
  before any code.
* **Before starting**: check what earlier work on the build profiles
  (`m0-flags`, `bs-a-s*` (KERNEL-CRATES), `os7s/k1-flags-after-program`,
  `microkernel-plan`) already did, and continue from there instead of
  duplicating it.
* **Condition 3220 (14)**: option (a) unless the customer says otherwise —
  the crate is an ordinary dependency in the development tree, only the
  export removes it, and the `off` image's symbols show it is not linked
  (the bit-identity of (16) proves it again).

### 11.10 Is `sc4` within about 30 more points? No (2026-10-23)

**What earlier build-profile work already did** (surveyed on `main`
a622d2266 and the branches §11.9 names):

| Work | Where | State |
|---|---|---|
| M0 part A: `build-flags.toml`, the four profiles (`tools/common/data/profiles/`), the resolver, `--check-cfg`, the `build` boot line from `cfg!` and its gate (`check-boot-line.py`), the profile rule, `check-build-flags.py`, the load ratchet | `m0-flags` → landed as d98ab2314 and b91a85311 (`m0-flags-land` has nothing beyond `main`) | **on `main`**; the `linux_abi` row is added to it, nothing is rebuilt |
| `MICROKERNEL.md` (M0-M8, K1-K17), `BUILD-FLAGS.md` | `microkernel-plan` | on `main` (nothing beyond it) |
| KERNEL-CRATES S1, S2, S3a; the dev and coverage profiles | `bs-a-s1`, `bs-a-s2`, `bs-a-s3`, `bs-a-dev-profile`, `bs-a-coverage-profile` | on `main` |
| KERNEL-CRATES S3, the kernel as four crates (2,609 `pub`, 26 raised checks, F-76) | `bs-a-s3b` (also on origin) | code review OK IF (S3.1)-(S3.8), ledger 1777 and 1795, S3.2a/b built; **stopped mid-rebase** on 2026-10-15 (WIP 166cdb0e5 does not build), 440 commits behind `main`, 83 of its kernel files changed on `main` since. Not usable as a rebase; usable as a recipe: its scripts (`~/ferrix-logs/bs-a/s3b/split.py`, `raise.py`, `bodies.py`, `span_check.py`, `floor_proof.py`) redo it on a new base, and then (S3.1)'s full row set is owed again |
| `os7s/k1-flags-after-program` | origin | not build-profile work: a seccomp negative control of stage 7 (2026-10-01), never for landing |
| M1's K5, K6, K7, K10, K12; K2, K3, K4; smt=off; F-73, F-64 | `mk/*`, `mk-m0rest/*` (and origin) | designs on `main` with OK IF; code WIP and unlanded, 98 to 130 commits behind |

**What `sc4` asks of an `off` profile beyond this document's work.** The
profile rule passes a profile at a level only when every flag value and
every ring-0 component allows that level (BUILD-FLAGS §4). An `off`
profile with today's other values fails `sc4` on three flags, and the level's
claim needs §6.5's streams of `MICROKERNEL.md`. None of these is in §7's
rows, §11's ~100 points or §11.8's 20 to 30:

| Owed for `sc4` | Why | Points (*guess*) | Done already |
|---|---|---:|---|
| `kernel_memory=retyped`: K12 (a job's reserved kernel memory) then K16 (no kernel heap after boot) | `heap` allows `present`, `sc2`, `none` only | K12 ~9 left; K16 25 to 40 (no design yet; part of M6's 70) | K12's design (OK IF 2550) on `main`, K12a/b code WIP on `mk/k12`, `mk/k12b` |
| `schedule=windows`: K9's static windows with EDF inside, a server's time charged to its client, interrupt time bounded, on stage 14's HardRt | `fair` allows `present`, `none` only | stage 14 ≈ 40 (not started) + K9 15 to 25 | nothing |
| `smt=off`: the kernel parks sibling threads | `stibp` does not allow `sc4` | 4 to 6 (with F-73, which it lands after) | design on `main` (OK IF 2563); code on `mk-m0rest/smt-off` and `smt-wip`, unlanded |
| Ring-0 components at `sc4`: F-76's ~25 unclassified libraries the core links, the board glue, the device cores' native halves with item evidence at `sc4` (3220 (18)) | the profile rule reads each component's level | 15 to 30 | F-76's register (on `bs-a-s3b`) |
| **Subtotal: the profile rule passes** | | **~110 to 150** | |
| V1, MC/DC on the whole kernel, with a tool found or built and qualified | ISO 26262 ASIL D, IEC 61508 SC 4, DO-178C Level A | 60 to 120 | object-code decision coverage only (`decision-coverage.py`; "no MC/DC", F-13) |
| V2, object-code verification | DO-178C 6.4.4.2b at Level A | 30 to 80 | none |
| T1, timing: the method on the Pixel 7's A78 cores, the evidence on a Lock-mode Cortex-A78AE platform not owned | IEC 61508 SC 4, ISO 26262 ASIL D, AoU-4/F-24 | 60 to 100 | none |
| F1, the formal model of the whole TSF | CC EAL6's ADV_SPM.1 (required), formal methods HR at SC 4 | 150 to 300 | none |
| F2, refinement proofs in Verus (`max`'s plan) | HR at SC 4 / ASIL D; EAL7's step | 300 to 700 | none |
| Outside the repository: a SIL 4 compiler route (F-17; Ferrocene is SIL 3, `aarch64` only), an independent assessor (F-27), a QMS (F-28), F-29, the pre-assessment (CLAIM M2) | | not points | none |

**The figure.** Beyond §11's ~120 to 130 points, `sc4` needs about
**110 to 150 points for the build to pass the profile rule at `sc4`**, and
about **300 to 600 more for the evidence the level's claim rests on
(V1, V2, T1, F1)**, 600 to 1,300 with F2: in all **about 410 to 750
points without F2**. Even the first part alone is four to five times the
customer's ~30 points. `sc4` is also an AArch64-only claim (`max`'s
`claim_architectures`), and T1's evidence needs hardware that is not owned.

**So, by the customer's rule (§11.9): `present` without the dated
exception first.** An `off` profile at `present` needs no flag work beyond
this document: `heap`, `fair` and `stibp` all allow `present`, and with no
Linux layer in ring 0 the profile needs no exception. That claim waits on
§11's rows, (16)'s equivalence and its own evidence (3208 (1), (6)(g)). The
`sc4` rows above stay `max`'s path (MICROKERNEL M1, M3, M6, M7 and the
streams), and an `off` kernel reaches `sc4` with them, not before.

### 11.11 The build-only `linux_abi=off`, as built (2026-10-23)

The first step of the consultant's order (3208 (6)(a)): §7's rows 1 to 4,
the experiment 94a41c2af made permanent under the flag, with no claim.
The later steps (KERNEL-CRATES S3, the personality trait, the crate
extraction, `xtask cert-tree`) replace these `cfg`s; until then they are the
switch.

**The flag.** `tools/common/data/build-flags.toml` declares `linux_abi =
on | off`, key-value, both values declared to `--check-cfg` and named on
the kernel's `build` line from `cfg!` (3218 (9)); it is last in the flag
file's order, so the audit record's existing two-bit places do not move.
Its levels are its placement's: `personality` gains `none`, with
`linux_abi=off` requiring `personality=none` and `personality=none`
requiring `linux_abi=off`; `fs` gains `none`, requiring `personality=none`;
`partition_speculation` and `restricted_fastpath`'s `not_applicable` now
conflict with `personality=user` instead of requiring `kernel`, which is the
same set plus `none`. Every profile sets `linux_abi = "on"` explicitly
(3208 (2)); no profile carries `off`, so it has no claim and its `build`
line says `no claim` (3208 (1)). `personality` keeps its name here; the
rename to a per-ABI placement flag (§11.7) is left to the trait's landing,
which changes the flag's meaning anyway.

**What `off` compiles out**, each by `#[cfg(ferrix_linux_abi = "on")]` on
a declaration or a statement:

* `main.rs`: the modules `fs`, `net`, `interfaces`, `service_check` and
  `stages_check`; `syscall::deliver::install`; the seccomp filter; the
  boot's filesystem, block-ring, devmgr, driver, btrfs and net bring-up and
  checks; the configuration-window and skipped-arm checks; the measured
  stage 3 timer check; the wall clock (`set_boot_time`); the boot
  framebuffer's note to the display core.
* `syscall/mod.rs`: the 55 personality and check modules (all but
  `native`, `program` and `uaccess`).
* The 22 check modules outside `syscall` that build a Linux test program
  (with `stages_check` and `service_check`, §4's list:
  `arch/*/{trap,switch,paranoid}/check`, `arch/stage_check`, `object/*_check`,
  `object/pin/check`, `user/{alloc,madvise,rmap}_check`,
  `device/config_check`, `iommu/domain_check`), the per-architecture
  `drive_*` re-exports, and x86-64's x87 and machine-check wrappers with
  their two calls in `smp/check.rs`.

`main.rs` composes `NoLinux` (every Linux number `ENOSYS`, the native range
answered as always) and a `register_load` of its own that registers board
support alone and requires only its bindings and the boot mode (§7 rows 1,
2, 5). An `off` kernel never prints the success marker: its boot ends with
`FERRIX-BOOT-UNCHECKED ... the self-checks that run a Linux program not
built: linux_abi=off`. Under `off` alone the crate allows `dead_code`,
`unused_imports`, `unused_variables`, `unfulfilled_lint_expectations` and
`unused_crate_dependencies`, with a reason: about 750 items only the cut code
used, and the 20 library crates only the Linux ABI reaches, which stay
ordinary dependencies (3220 (14) option (a)).

**Evidence** (release builds, nazuna, 2026-10-23):

| | x86-64 | AArch64 | ARMv7-A |
|---|---:|---:|---:|
| `on` image, text bytes | 7,434,265 | 6,750,641 | 7,853,044 |
| `off` image, text bytes | 1,366,645 | 1,175,201 | 1,421,024 |
| `off`: symbols of the Linux-only library crates / of `fs`, `net`, `interfaces`, `syscall::{linux,exec,signal,fd,file}` | 0 / 0 | 0 / 0 | 0 / 0 |
| `on`: the same | 443 / 2,691 | 436 / 2,713 | 435 / 2,781 |

* `cargo clippy -D warnings` passes for `on` and `off` on all three
  architectures; `cargo xtask check` now lints `linux_abi=off` beside
  `checks=off` and `mitigations=off`, so a new static coupling fails the
  check the day it lands (§9 recommendation 2).
* The `present` x86-64 image against `main`'s: the same 7,618 symbols with
  the same sizes, `.text` the same length; only `__data_start` moves, by the
  flag table's growth in read-only data.
* `xtask`'s resolver test: `off` without `personality=none`, and
  `personality=none` with `on`, are refused; with `personality=none` and
  `fs=none` it resolves as an ad-hoc build at level `none`.

**What it is not.** The `off` image links but does not boot usefully: no
process can be started (§7 rows 8 to 12), a user fault still ends in
`trap.rs`'s fatal (3208 (5)), stage 1's and stage 3's checks in
`stages_check` are not built until row 13 splits it, and no requirement
counts as verified in it (3208 (8)). Kernel lines: +277 / -14 in
`src/kernel` (almost all `cfg` attributes and the two `off` functions);
nothing is added to the `on` image's code.
