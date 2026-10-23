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
