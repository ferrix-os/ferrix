# M5: the lift

**Design, 2026-11-03 (mk-lift, branch `feature/mk-lift`).** `docs/MICROKERNEL.md`
§4, §5 and §8's M5: the load ring leaves ring 0 and runs as `linuxd`. The ids
are the run's block: **H.OBJ.30-33, L.object.310-319; native calls
0x10B0-0x10BF; panic codes FX-0940-0949** (`feature/mk-reserve`).

Everything here is behind `personality=user`. **`present` and `desktop` are
unchanged**, in behaviour, boot lines and image: §8 says how that is shown.
Rows under `personality=user` add to the gate table and replace nothing. The
flip for `fast` and `max` needs MICROKERNEL §7's rows and is not decided here.
`desktop` stays hybrid (Q20).

**One source, chosen by flags (the customer, 2026-11-03).** The goal is a tiny
kernel in `fast` and `max`, selected only by compile-time flags from one tree.
The same `ferrix-kernel-load` source builds into ring 0 for `desktop` and into
`linuxd` for `fast` and `max`, with no fork of it. **"The flip" compiles the load
out of `personality=user` builds; it deletes nothing the hybrid uses.** Kernel
code no building profile needs any more, `desktop` and `present` included, may be
deleted, through the consultant as any item change.

## 1. The shape

The lift is **one source built twice**. `ferrix-kernel-load`'s root
(`src/kernel/src/kernel_load.rs`) and every file under it stay where they are.
A new package compiles the same root as a user-space library, against a shim
in place of the core and the item:

| Package | Root | What | Depends on |
|---|---|---|---|
| `ferrix-kernel-load` | `src/kernel/src/kernel_load.rs` | as today, in the hybrid kernel | core, item |
| `ferrix-lift-core`, renamed `ferrix_kernel_core` | `src/kernel/src/lift_core.rs` | the shim: the core's interface as the load names it | `ferrix-rt`'s native layer, native-abi |
| `ferrix-lift-item`, renamed `ferrix_kernel_item` | `src/kernel/src/lift_item.rs` | the item's interface: `syscall::{Personality, native, program, ...}`, `discovery::devmgr`, `init` | lift-core |
| `ferrix-lift-load`, lib name `ferrix_kernel_load` | `src/kernel/src/kernel_load.rs`, **the same file** | the load, built with `ferrix_load_built="lift"` | lift-core, lift-item, the same libraries |
| `linuxd` | `src/user/system/native/linuxd/src/main.rs` | the program: entry, boot (§6), the restricted loop (§4.2), the global allocator | lift-load |

* **`crate::` paths keep working** in load files, as KERNEL-CRATES §1's rule
  has them: `pub use ferrix_kernel_item::*` is in the shared root.
* **The shim's files sit under `src/kernel/lift/`** except those reused
  by position (§3, class S): `lift_core.rs` writes `pub mod fallible;`, found
  beside it as `kernel_core.rs`'s is. So a pure file has one source and two
  crates. `check-item-boundary.py`'s rule "each file is reached by exactly one
  crate" (KERNEL-CRATES §3 (2)) gains one exception, its own list,
  `lift_reuse` in `certification-item.json`: a listed file may be reached by
  its ring's crate **and** `ferrix-lift-core` and by nothing else. An unlisted
  file reached twice still fails; a planted one is the control. The load's
  files are reached by `ferrix-kernel-load` and `ferrix-lift-load` by the same
  list's second key, the root alone: both crates' roots are the one file.
* **Which load modules the shared root builds**: a new derived cfg.
  `ferrix_load_built` is written by `build.rs`, never set by a profile:
  `"ring0"` for the kernel's crates under `personality=kernel`, `"none"` for
  them under `personality=user` or `none`, `"lift"` for `ferrix-lift-*`. Every
  `cfg(ferrix_linux_abi = ...)` in `main.rs` and the kernel's crate roots that
  today means "the load is in this kernel" reads it instead:
  `ferrix_linux_abi = "on"` becomes `ferrix_load_built = "ring0"` and `"off"`
  becomes `any(ferrix_load_built = "none")`. Under `present` and `desktop` the
  value is `"ring0"` where `linux_abi=on` was, so nothing changes in those
  images (§8's check). The *flag*'s meaning does not move: `linux_abi=on` with
  `personality=user` is `max`'s and `fast`'s line, and the boot line prints both.
* **The checks raised to the load stay in ring 0's crate only**: under `"lift"`
  the raised and `_check` modules are not declared. The checks the load's files
  carry about the Linux layer itself return as user-space checks (§7), the
  B5 re-homing having moved the kernel's ids first (mk-m2's row).
* `xtask`: `--personality user` (or `--set personality=user`). `build-flags.toml`'s
  `personality.user` and `partition_speculation`/`restricted_fastpath`'s
  dependants change from `built = "no"` to `built = "yes"` when milestone 2
  holds; until then the flag resolves only with `--set`, at level `none`, with
  no claim, and no profile uses it.
* `fs`: `personality=user` with `fs=kernel` is refused today. Until `btrfsd`
  exists (mk-authority's row), btrfs is **inside linuxd**: a third value,
  `fs=lift`, level `none`, requires `personality=user`, and `max` and `fast` keep
  `fs=server`, which cannot flip until `btrfsd` runs (§9).

## 2. The kernel under `personality=user`

**No load ring.** `main.rs` composes `NoLinux` (`Personality`: every Linux number
outside restricted mode answers `ENOSYS`), and does not name `ferrix_kernel_load`,
so the crate is not linked; a symbol check (`nm` finds no `ferrix_kernel_load`)
is milestone 1's gate, with a control that names one load item from `main.rs`.
What today's `linux_abi=off` lacks, in `LINUX-ABI-SEPARATION.md` §7's rows, is
added to the item under this cfg alone:

* **A native host** (row 12), new file `syscall/native_host.rs`, item ring, a
  process with no Linux state: `impl Host`, `impl UserThread`, the
  `ReturnPath`: attention ends the thread on a kill, a fault goes to K4's
  supervisor if any and otherwise ends the process with a status (never
  `trap.rs`'s fatal), `sigreturn` answers `ENOSYS`.
* **The first-image loader** (row 10, K15's "one loader only"), new file
  `first_image.rs`, item ring, held to the item's rules: reads the three
  first images from the initramfs by a read-only directory walk that needs no
  VFS (`ferrix-elf` for the headers). Read K15 on `feature/mk-m2` and use its
  file if it is ahead; the two are one loader at landing. Boot authority, what
  each first process holds, is mk-authority's (§5); until it lands this kernel
  starts `linuxd` alone with the handles §6 lists.
* **Native calls the shim needs and nobody has**, in this block (each with a
  requirement in L.object.310-319, a boot check and a control; `fast`/`max`
  only, since no other profile has a native host):

| Number | Call | Why |
|---|---|---|
| 0x10B0 | `DEBUG_WRITE(log, ptr, len)` | §2.1's write-only debug log, by a handle. It stays here: boot authority holds a budget per holder and a measured process's prefix on this one call (L.device.84-85). |
| 0x10B1 | `OBJECT_KIND(handle)` → kind, rights | what a handle in a driver's message is, for the bridge (§11) |
| 0x10B4 | `THREAD_YIELD()` | a contended shim lock; `sched_yield` |
| 0x10B5 | `CLOCK_READ(which)` → value | 0 the clock, 2 the counter's frequency when ring 3 can read the counter, 3 the processors. Ends with the time page |
| 0x10B6 | `RANDOM_FILL(ptr, len)` | §2.1's entropy call |
| 0x10B7 | `THREAD_AFFINITY(thread, ptr, words)` | `sched_setaffinity`; a region's threads only |
| 0x10B8 | `CONSOLE_READ(console, ptr, len)` → bytes | the console's input, in the kernel until M6, by a handle |

Each answers `NormalOnly` from restricted mode. Threads and processes are K11's;
no call here makes one. Only what lands is released, in the commit that writes it.

**The `load`-to-kernel seams that vanish**: `register_load`, `ReturnPath`'s Linux
half, `init::Launcher`, `devmgr::register_reader`, `power::register_flush` and
the served calls' `register_server` have no caller; `main.rs` under
`ferrix_load_built="none"` has its own short `register_load`. `DEVMGR_START` and
the retired calls of MICROKERNEL §3 answer `ENOSYS` there, from the first
build. `decode_syscall`'s Linux tables and `signal_frame.rs` are not reached
in this kernel (§2.1); deleting them from it is M6's.

## 3. The shim

Rewritten for revision 2 (ledger 6012, R7). The generated
[M5-SHIM-MAP.md](M5-SHIM-MAP.md) places every item the load names.

| Class | What it means | Where |
|---|---|---|
| **S** same source | files compiled into linuxd by position, each listed by file in `lift_reuse` | `object/*`, `user/{space,vmo,cache,address_wait,memory_type}.rs`, `syscall/uaccess.rs`, `fallible`, `hooks`, `signal_frame`, `claim`, `panic/catalog`, `arch/x86_64/signal`; in the item `init`, `syscall/program` |
| **R** reimplemented | same signature, user-space body | `sched`, `sched::wait`, `sched::work`, `sched::account`, `sync`, `mm`, `smp`, `timer`, `irq`, `vmap`, `console`, `trap`'s types, `arch`'s register shapes |
| **B** a bridge | a device core's local object against a driver's kernel handle | `bridge`, `item::cores`, `item::discovery::devmgr` |
| **N** not named | a build error if named | board glue, `smp::service`, `console::pstore` |
| **P** stub, listed so it is visible | `/proc` text the kernel holds | `sched::inspect`, `sched::trip` |

Module by module, the ones with a real decision:

* **`sync`**: a test-and-set lock that yields after 256 spins. A holder can be
  preempted by the kernel in ring 3, no preemption count protects it, and no
  interrupts exist to mask. FX-0503's rule goes with it: `may_block` is always true.
* **`sched::current`**: linuxd's normal-mode stacks are 256 KiB and aligned, and a
  `Task` pointer sits at the top, so `current` needs no thread-local register.
* **`sched::wait`**: a `WaitQueue` is a list in the shared region under a shim
  lock, and each `Task` has one parking word: sleep is K5's `ADDRESS_WAIT` with a
  deadline, wake sets it and calls `ADDRESS_WAKE`.
* **`sched::work`**: the work word as today; `post` sets the bit, wakes the
  parking word and, if the target is in restricted mode, calls K1's kick (§4.3).
* **`sched::account`**: processor time, charged at each crossing from the counter
  the clock is made of, and from outside for a program that makes no call. Wall
  time in the program: what the kernel runs meanwhile on that processor counts,
  so `cpu.max` holds a group to less than it asked under contention, not more.
  A task's group follows its process, read at every exit.
* **`mm`**: frames are pages of the arena (§10). The **heap draws on the same
  allocator**: a page or more is its own block and goes back when freed, less is
  carved from 64 KiB chunks, so there is one pool and one number, and what runs
  out meets the load's reclaim and OOM. A program's page stops 16 MiB short of the
  end, so that a section entered on `reserve` can allocate. The kernel is left an
  eighth of its free memory at linuxd's start.
* **`timer`**: the kernel's sum on the kernel's counter, without a call where the
  kernel says ring 3 can read it.
* **`user::space`, `user::vmo`, `object::*`**: the kernel's own files, by position
  (§10); `AddressSpace`'s page tables are the shadow.
* **`syscall::uaccess`**: the same file, copying through the shadow.
* **Device cores**: the hybrid's, by position; a driver holds kernel objects and
  the bridge carries what crosses (§11).

## 3a. What the kernel can see (ledger 6012, R3)

**The customer agreed this shape on 2026-11-04**: linuxd's one arena VMO with shadow page
tables. That decides the design; the table below stays as what follows from it for the ST
and for the flip.

This shape differs from the plan's. **MICROKERNEL §2.2 and §2.3's claims about
separation are not true of it**, and nothing here claims them:

| The plan says | This shape |
|---|---|
| the kernel holds each program's memory as VMOs it can tell apart | it sees one arena VMO; which frame is whose is linuxd's |
| the kernel resolves a fault on a resident page (K3) | linuxd does, and `SPACE_MAP`s it |
| a job's memory limit is the kernel's to enforce per cgroup | all programs' memory is charged to the one `linux` job; `memory.max` is linuxd's sum |
| a cgroup's processor limit is the kernel's | `cpu.max` is linuxd's sum over a kernel that schedules threads alone |
| a page freed by linuxd is the kernel's again | it stays committed in the arena |
| a cgroup is a kernel job | a flat kernel job under `linux` stands for one when `job_for_cgroup` asks; it carries no limit, and a native service in it is not in `cgroup.procs` |

So separation between Linux programs and cgroup limits **rest on linuxd**. This is
accepted dark but for `.route(at)` in `copy_to_user_through`'s present path, which `present` and `desktop` run too (ledger 6923, 6943), at level `none`, with no claim. **No flip of `fast` or `max` until
the kernel's charging and visible separation are restored for this shape, or the
ST is rewritten for it**; K3 stays on the table for that. Consultant's default is
in `morning-questions.md`.

## 4. Threads, and the paths

### 4.1 The model

Every Linux thread is **one kernel thread**, in **one kernel process per Linux
process** (K11), with linuxd's image, heap and stacks as the shared region and the
program's memory as the restricted space. Under K1 it alternates: normal mode
(linuxd's code, shared handle table) and restricted mode (the program, private
handle table). **Nothing crosses a thread boundary on a system call.**

Until K11 runs, the stub (`mk-restricted`'s branch decides how long) is K1 alone:
one kernel process, `linuxd`, one kernel thread per Linux thread, and a restricted
space per Linux process, by handle. The private handle table waits with it, so a
program's native calls answer `ENOSYS` in that stage. No stage before K11's lands
can pass a row whose claim is §2.3's isolation of handles.

### 4.2 The restricted loop

```rust
loop {
    let exit = restricted_enter(&mut task.context); // K1
    match exit {
        Exit::Syscall => dispatch_with::<Linux>(...),   // the load, unchanged
        Exit::Fault { .. } => (return_path.fault_signal)(...),
        Exit::Kick => {}
    }
    if (return_path.needs_attention)() {
        (return_path.return_to_user)(&mut task.context); // signals, stops, exit
    }
}
```

`dispatch_with` and the attention path are the load's today: the kernel's entry
stub calls them on the kernel stack, the loop calls them on the linuxd stack. A
call that blocks parks this kernel thread, as today. The load's `arch::UserContext`
is K1's register block or a straight copy of it.

### 4.3 Signals by kick

`kill` on a running thread: the load sets the bit (`sched::work::post`), the shim
wakes the parking word and calls K1's kick on the target's kernel thread. A
thread in restricted mode exits with `Exit::Kick`, the loop sees attention, and
`deliver.rs` writes the frame through `setup_signal_frame` (S, into restricted
memory by `uaccess`) and sets the context; K1's checked path takes that context,
its rules holding whatever linuxd wrote. A thread parked in `ADDRESS_WAIT` wakes,
`wait_interrupted` answers as today. `sigreturn` is `restore_signal_frame` in
linuxd, handing K1 the result, K1's check being the only one that counts.

### 4.4 Ports and channels in a shim wait

A thread that must wait on a kernel handle *and* be interruptible cannot sit in
`PORT_WAIT`. The shim keeps **one service thread per port** that blocks in
`PORT_WAIT` and turns each packet into a `WaitQueue` wake. `object::port::Observer`
is that thread's callback. It costs a hop on the slow path only (a driver's
reply), never on a cached read.

### 4.5 Fork, exec, mmap, ptrace

The load's own code in each case, on the kernel's files by position:

* **`fork`** (`family.rs`): the shadow is copied copy-on-write by `user/space.rs` as
  in the hybrid. The child's restricted space is new and empty, and fills at its
  faults. The parent's write protections are `SPACE_UNMAP`ped from the cache by
  `mm::unmap_in`, so its next write faults and copies. A new kernel process through
  K11 for the child; its first thread enters with the parent's saved context.
* **`exec`** (`exec.rs`): the load's loader into a new shadow; a new restricted
  space bound at the fault that follows. The vDSO is the load's and the same source:
  `arch/x86_64/vdso.rs` by position, the one reused file with `global_asm!`, whose code
  only ever runs in a program. `MODE_TSC` where the kernel says its clock is the TSC.
* **`mmap`** (`memory.rs`): unchanged. A page reaches the kernel at its first fault
  (`fill::resolve` → `SPACE_MAP`, one page per fault, **no map-ahead yet**); a miss
  reaches `fs/pages.rs` in the same thread, by a plain call. `MAP_PRIVATE`,
  `mremap` and `madvise` are `user/vmo.rs`.
* **`ptrace`** (`syscall/kill.rs`): a stopped tracee is parked in linuxd;
  `PTRACE_GETREGS` reads its saved context, `SETREGS` writes it, K1's checks applying
  at the next enter; `PEEK` and `POKE` go through the shadow.
* **Futex**: the Linux table stays linuxd's, sleeping on a shim `WaitQueue`. K5
  serves linuxd's own locks.
* **Time**: the load's clock calls, answered in linuxd from the counter.
* **Native calls from a program** (§2.4): straight to the kernel from restricted
  mode, on the private table, never through linuxd.

## 5. What it needs, by row

| Row | Used for | State |
|---|---|---|
| K1 | enter, exit by reason, kick, context read, vectors | unlanded branch |
| K11 | region, shared process, two tables, `Bare`, `THREAD_*`, `PROCESS_START_SHARED` | unlanded branch; K11's handle bootstrap into the private table is what `test-init` waits on |
| K2 | `SPACE_*` | unlanded branch |
| K3 | **not used**: frames are the arena's (§3a, §10) | the row stands for the flip |
| K4 | a fault in linuxd ends the partition | `Bare` |
| K5 | `ADDRESS_WAIT`, `ADDRESS_WAKE` | unlanded branch |
| K7 | charging, when `btrfsd` and `netd` exist | not used yet |
| K8, boot authority | devmgr detached, the `devmgr` and `cores` channels (§11) | unlanded branch |
| K10 | **not used**: deadline on `ADDRESS_WAIT` | |
| K15 | `first_image`, `PROCESS_MAKE`, `PROCESS_RUN` | unlanded branch |

## 6. The boot

MICROKERNEL §5: the kernel starts the partition manager, devmgr and `linuxd`. As
built: devmgr detached, then `linuxd`; the manager is not there yet.

1. The kernel boots as `linux_abi=off` does. **Those checks that need a Linux
   program are not built** (`LINUX-ABI-SEPARATION.md` §11.11, row 13), so this image
   prints `FERRIX-BOOT-UNCHECKED`, a third text naming `personality=user`, and a row
   passes on linuxd's own `  linuxd   booted` with `FERRIX-BOOT-OK` absent.
2. `linuxd_start.rs` makes a region on a `linux` child job, loads `/sbin/linuxd` with
   the first-image loader, and hands it a **boot page** (R8, accepted until
   BOOT-AUTHORITY lands, then one format, BA.5): `BootPage` in `native-abi`, 64 bytes,
   region, job, initramfs VMO, debug log, console, `devmgr` and `cores` channels, the
   memory it may use; the command line at offset 256. No audit-read and no time page.
3. `linuxd`: heap; the arena; the root in tmpfs from the initramfs VMO; the device
   cores; `init` on a thread of its own, from the three inputs.
4. It serves until pid 1 ends. Pid 1's end or `reboot(2)` ends the partition
   (`sys::end_partition`), and a `linuxd` that dies does too.
5. **The disk**: `fs::root_disk`, `data_disk` and `home_disk` are linuxd's, over a
   driver devmgr started (§11). `ferrix.root=` comes from the boot page's command line.

**Owed rows** (R8): L.init.2 is not the kernel's here; `job::root().new_child()`
becomes the `linux` job; `ferrix_vfs::initramfs` in an item file gives way to a
`ferrix-cpio` walk; `first_image::load_at` joins L.user.205's list; `linuxd_start.rs`
enters the manifest.

## 7. Milestones and gates

Each is one commit or a stack, reported by its log.

| # | Holds when | Row |
|---|---|---|
| 1 | `--set personality=user` builds a kernel with no load symbol on x86-64, and `linuxd` | new `check-lift.py` step with the symbol control; `check` builds both |
| 2 | that kernel starts linuxd, linuxd runs one static Linux program, its `write` reaches the debug log and its `exit` ends it | `test-linuxd` (new, a static hello in the initramfs); control: the program's `write` number made wrong, the row fails |
| 3 | busybox `sh` under `test-shell`'s boot | `test-shell --personality user --init-only` |
| 4 | `fork`, `exec`, `mmap`, signals, pipes | `test-shell --personality user` passes unchanged |
| 5 | the disk | `test-btrfs`, then `test-net`, under `--personality user` |
| 6 | the gate table's rows, one at a time, then AArch64 | the gate table's rows `--personality user`, each a row of its own beside today's, weakening none |

Every row is run under `fleet/gate.sh`. Every new check has a control that FIRED
(`gate.sh control --expect`). §7's performance rows run with the row they gate,
each reported, none loosened; a miss goes to `morning-questions.md`.

## 8. What holds `present` and `desktop` unchanged

* The cfg rename (§1) is the only edit to shared code: a mechanical replacement
  in `main.rs`, the crate roots and `build.rs`, with no code inside a `cfg` moved.
  **Evidence**: `present` and `desktop` release images, x86-64, AArch64 and
  ARMv7-A, before and after: same symbols with the same sizes (as
  LINUX-ABI-SEPARATION §11.11 showed), four boots' lines compared.
* New kernel files (`native_host.rs`, `first_image.rs`, the calls of §2) are
  `#[cfg(ferrix_load_built = "none")]`, so they are not in those images. New native
  numbers are declared in `native-abi` and answered `ENOSYS` from the default arm
  there, which a boot check proves (the table's check already does).
* The boundary gate's `lift_reuse`, the generated map's `--check` and the new
  build step run in `check`, each with a control.
* **Certified item**: `native_host.rs` and `first_image.rs` are item-ring files,
  held to its rules (requirements from the block, every `unsafe` traced, fallible
  allocation, no reachable panic) and reviewed before landing. **No claim attaches
  to `personality=user` until M7**: the build line says `no claim`, level `none`.
* The shim, `linuxd` and `ferrix-lift-*` are not in any ring: `linuxd` is outside
  the TSF (MICROKERNEL §6.1). They are classed `user` in the manifest's crates list,
  so the gate knows them.

## 9. Risks, and what this design does not do

* **The shim may not stay small.** 197 + 156 items are real code: *guess* 6,000 to
  10,000 lines. `object` and `user::space` are the bulk.
* **`SpinLock` in ring 3** can spin behind a preempted holder; the yield bounds it;
  §7's rows will say what it costs. A futex-style lock is the fallback, and costs
  nothing under `present`, which never compiles it.
* **`fs=lift`** is a stopgap so milestone 5 can run: `fast` and `max` set
  `fs=server`, and the flip waits for `btrfsd`.
* **AArch64** is milestone 6: K1's AArch64 half and the shim's arch module. ARMv7-A
  stays native-only (Q7).
* **What no milestone tonight gives**: K11's two tables before K11, §2.5's K17, the
  speculation rule of §7 (K1's), the deletion from `fast` and `max` of the
  Linux-shaped core (M6).

## 10. What building it showed (revision 2, 2026-11-04)

The shim came out thinner than §3 first planned, and the difference is a design
change, sent to the consultant with milestone 2's commit:

* **`user/` and `object/` are the kernel's own sources**, compiled by position into
  `ferrix-lift-core` (`user/{space,vmo,cache,address_wait,memory_type}.rs`,
  `object/*`, `syscall/uaccess.rs`, `signal_frame.rs`, `fallible.rs`, `hooks.rs`,
  `claim.rs`, `panic/catalog.rs`, `arch/x86_64/signal.rs`, and in the item `init.rs`,
  `syscall/program.rs`). None holds `asm!` or an entry. What is reimplemented is
  the substrate under them: `sched`, `sync`, `mm`, `smp`, `timer`, `irq`, the
  register shapes of `arch`, `console`, `trap`'s types.
* **Frames are pages of one VMO, the arena**, mapped once into the region, so
  `Frame` and `direct_map` mean what they always meant to that code. So `vmo.rs`'s
  page lists, copy on write, `mremap`, `madvise`, the page cache and the rmap are
  the same code in both shapes.
* **A program's page tables are a shadow in arena frames**, walked by the same
  `ferrix-paging`. `mm::map_in` writes only the shadow; **the kernel's restricted
  space is a cache of it**, filled by `SPACE_MAP` of arena pages when K1 reports a
  fault (`fill::resolve`), and emptied by `SPACE_UNMAP` in `mm::unmap_in`. W^X is
  K2's on every mapping: the arena has one writable handle and one `VMO_EXECUTABLE`
  handle with no write right.
* **`uaccess` copies through the shadow** with no system call and no fault window:
  a cached `read` is a restricted exit, a walk and a `memcpy`.
* **What this costs against §2.2**: a fault on a resident page enters linuxd, where
  the plan had the kernel resolve it by K3. §7's `mmap`/`munmap` and `fork` rows will
  say by how much; map-ahead and a batched `SPACE_MAP` are the levers before K3 is
  brought back for the page cache.
* **Linuxd's start**: a boot page rather than a bootstrap channel, handles in the
  shared table (§6). `arch_prctl` and `rt_sigreturn` are answered in the restricted
  loop before dispatch, as the kernel's entry answers them.

### Linuxd's own calls (5714 (15))

Each is `NormalOnly`, in no profile's table but `personality=user`'s, and each answers
`ENOSYS` from the default arm elsewhere. `lift/core/calls_check.rs` asks each refusal from
linuxd's start; `test-linuxd` reads its line.

| Call | Rights | Bound | Lock | Refused with |
|---|---|---|---|---|
| `DEBUG_WRITE` 0x10B0 | `WRITE` on the log | 4 KiB a call | console's | `BAD_HANDLE`, `WRONG_TYPE`, `ACCESS_DENIED`, `INVALID_ARGS` |
| `OBJECT_KIND` 0x10B9 | the caller's own handle | none | table's | `BAD_HANDLE` |
| `THREAD_YIELD` 0x10B4 | none | none | none | |
| `CLOCK_READ` 0x10B5 | none | one word | none | `INVALID_ARGS` |
| `RANDOM_FILL` 0x10B6 | none | 4 KiB a call | the generator's | `INVALID_ARGS`, `FAULT` |
| `THREAD_AFFINITY` 0x10BA | `MANAGE`, a region's thread | `WORDS` | the queue's | `BAD_HANDLE`, `WRONG_TYPE`, `ACCESS_DENIED`, `BAD_STATE`, `INVALID_ARGS` |
| `CONSOLE_READ` 0x10B8 | `READ` on the console | 4 KiB a call | console's | as `DEBUG_WRITE` |
| `BOOTSTRAP_MOVE` 0x10BB | `SUPERVISE`, the same region; `TRANSFER` on the handle | one handle | tables' and slot's | `BAD_HANDLE`, `WRONG_TYPE`, `ACCESS_DENIED`, `ALREADY_BOUND`, `BAD_STATE` |
| `SPACE_WINDOW` 0x10BC | `MAP` on the space | one space, once | the space's | `BAD_HANDLE`, `WRONG_TYPE`, `ACCESS_DENIED`, `BAD_STATE` |
| `SPACE_PLACE` 0x10BD | `MAP` on the space, `MAP` and each right placed on the VMO | `PLACE_MOST` entries a call | the space's | as `SPACE_WINDOW`, `INVALID_ARGS`, `OUT_OF_RANGE` |
| `DEVICE_SERVED` 0x10C3 | `MANAGE` on the device | one device | the pin table's | `BAD_HANDLE`, `WRONG_TYPE`, `ACCESS_DENIED`, `BAD_STATE` |

### The review's conditions (OK IF (1) to (20), ledger 5714)

Folded in or owed, by number; each is closed in the landing that says so.

| # | Condition | State at the landing |
|---|---|---|
| 1 | map `--check` fails only on an unplaced item; placement by module rule; control | built; control `mkl-l-map` through `gate.sh` on the landing hash |
| 2 | `ferrix_load_built` derived once in `build.rs`, not a flag | built: `--set` cannot reach it, no flag declares it. `check-build-flags.py` has no row for it: BACKLOG |
| 3 | all sites classified: `ring0`, or `any(ring0, lift)` for the load's Linux modules, checks `ring0` | built; `check-lift.py` counts `lift` sites, and every `linux_abi` cfg in the kernel's crates became one of these |
| 4 | `present`, `desktop`: release, three architectures, section bytes or per-symbol hash, four boots | on the landing hash; the comparison is named in the commit |
| 5 | lint allows at `"none"`: no wider, no claim | as `linux_abi=off` had them; 3257 (2)'s control not re-run: BACKLOG |
| 6 | `native_host`, the arms and linuxd's start keyed on `personality=user`; `linux_abi=off` image unchanged | built: code for a ring-0 kernel with linuxd reads `ferrix_personality = "user"` (W1); the image in the comparison of (4) |
| 7 | `lift_reuse`, a ratchet with reasons; lift packages their own key; cargo metadata both ways | built; controls `mkl-l-unlisted`, `mkl-l-asm`, `mkl-l-dep` |
| 8 | no `#[path]`: `lift_core.rs` declares inline `lift` modules | built |
| 9 | a reused file holds no `asm!` or entry; `cfg(lift)` in load files a counted list | built, in `check-lift.py`; control `mkl-l-asm` |
| 10 | `fs=lift`, level none, resolver tests both ways, BACKLOG row to remove it | built: `fs_lift_goes_only_with_the_lift_and_carries_no_claim` |
| 11 | one loader, `discovery/first_image.rs`; the no-VFS initramfs read bounded | built: `load_at`, and `loader_callers` names `linuxd_start.rs`; control `mkl-l-loader`. `file_named` is the archive's own bounded reader |
| 12 | K11's `Bare` is the host | built: no `native_host.rs` was needed |
| 13 | `THREAD_*` K11's; no time-page call | built: the table above is what remains |
| 14 | `DEBUG_WRITE` and `CONSOLE_READ` take a handle from their first commit | built; both refused from restricted mode |
| 15 | each call's row; one requirement, check, control; ENOSYS on present and desktop | the table above; L.object.310, `lift/core/calls_check.rs`, `mkl-l-holds`. The ENOSYS is the shape of `native/lift.rs`: a row owes it a boot check, BACKLOG |
| 16 | K11 slice A plus K1's general path is the floor; a control from the private table | built; control `mkl-l-r5` |
| 17 | the kernel makes the region, internally by `Region::new` with the `linux` job, not by the `REGION_CREATE` call (ledger 6012) | built: `linuxd_start.rs` |
| 18 | `personality.user` is `built="adhoc"`; a resolver test holds `fast` and `max` not buildable | built: `fast_and_max_stay_not_buildable_with_the_lift_in` |
| 19 | milestone 5 waits for K8; linuxd a client only | K8 landed first; linuxd serves through devmgr's cores channel, and holds no device's `MANAGE` |
| 20 | a third marker text; a row passes only on it, `FERRIX-BOOT-OK` absent; controls | built; control `mkl-l-marker`: the marker under `present` fails `test-boot` |

## 11. The bridge (revision 3, 2026-11-04)

A device core keeps the local objects it has in the hybrid; a driver holds kernel
objects. `lift/core/bridge.rs` is the edge, a thread per direction:

* **`OBJECT_KIND`** tells what a handle in a driver's message is. A VMO becomes
  local frames above the arena (`mm::foreign_window`); those pages count 1 and are
  never freed here (`is_foreign`). A port and a channel get carrier threads.
* **A message moves its handles.** What the core gives a driver is a duplicate with
  the core's rights **plus `TRANSFER`**, which lets linuxd's write carry it at all: a
  driver holds one right more than the hybrid's. The original stays to wait on.
* **A channel's end**: a wait on a handle keeps its channel open, so the carrier
  that reads from it closes by itself and looks at its core every 100 ms when idle:
  a driver's death reaches the core at once, a core's letting go within that.
* **No device handle reaches linuxd**: `cores.rs` asks devmgr by location
  (`CORE_OPEN`, `CORE_OPENED` on the `cores` channel), and prints devmgr's reports.
* **Cost**, in `test-btrfs`'s fill: 8 s against the hybrid's 3.5 s, the difference
  this hop and a fault per page. A cached read crosses nothing.
* **A core's buffer** (a card's dumb buffer) becomes a kernel VMO whose pages are the
  local VMO's foreign frames, and a program's mapping is filled from that VMO. The job
  charged is `linux`. **The window stays mapped for linuxd's life**, bounded at 16 GiB
  of address space, past which a driver's object cannot be made (ENOMEM, never a
  panic). Unmapping at the last reference, and a send that strips `TRANSFER`, are
  **flip preconditions** (ledger 6268, C).
* **What a driver says**: devmgr puts a message ahead of START whose one handle is a
  write-only duplicate of the debug log; the runtime's start takes it by its sizes.
* **Native calls from a program**: `PROCESS_BOOTSTRAP` is answered by the kernel from
  the slot; `PROCESS_GIVE` exits, linuxd judges the Linux half, and `BOOTSTRAP_MOVE`
  (0x10BB, `SUPERVISE` on the shared process handle, the same region) moves the handle
  from the caller's private table, so it never enters the shared table (L7).
  `PORT_FD`: a wait-only kept duplicate and a thread per port, charged to the
  partition. `job_for_cgroup` exits and answers a flat kernel job per cgroup under
  `linux` (§3a), with no way yet to kill it from `cgroup.kill` (P8, owed).
* **A copy that misses the cache** (ledger 6326): the kernel's restricted space is only
  a cache of the shadow, so an answered call's copy can miss memory the program holds.
  uaccess notes it and the error goes up; at the dispatcher's end a call whose
  `nr::copy_miss` row says it may run again exits with `FaultKind::CallCopy`, pc back
  at its `SYSCALL`. Linuxd fills and continues, or answers the `FAULT` status, never a
  signal. Folded into K1 by 2f2f68b27's copy-miss (P1 to P6): linuxd passes `ENTER_COPY_MISS_EXITS` on its own entry, and a row and real init's negative control are this landing's (6529 Q5).
* **A row's kill of a driver** (L9): `kill:<slot>` to a driver's `unbind`, compiled only
  with `FERRIX_ROW_DRIVER_KILL`. It shows a restart after a kill, not after a crash.

## 12. Rows and figures (2026-11-04, x86-64 KVM, tip of `feature/mk-lift`)

Run by hand with `cargo xtask <row> --personality user`, **not yet through `gate.sh`, so
none is evidence**. Rows with a control boot run it too.

| Holds | Rows |
|---|---|
| exit 0 | test-linuxd, test-shell (zinc, busybox), test-vfs, test-threads, test-btrfs, test-net, test-jobs, test-container, test-seccomp, test-procfs, test-input, test-sysfs, test-seat, test-auth, test-pkg, test-pty, test-sem, test-pipewait, test-shm, test-bwrap, test-stall, test-first-processes, test-rustc, test-restart (gpu, blk), test-nvme, test-usb, test-audio, test-video, test-apps, test-mce, test-disk-offline, test-kernel-shape, test-chrome, test-yserver, test-settings, test-frames, test-wine-sync, test-wine-socket, test-xwindow, test-dxvk, test-steamcmd, test-claude-code; test-init with three parts NOT RUN (a native service in `cgroup.procs`, devmgr by init, the audit record) |
| through `gate.sh`, PASSED | at f40b84098: test-shell, test-btrfs, test-net, test-vfs, test-threads; at af079ec0f: test-init, test-seat |
| controls FIRED through `gate.sh` | the arena's rights (R5), a handle of the wrong kind, the copy-miss rewind removed (real init's `vmo_write: Fault`), a refused unmap (R2), W+X in a place, `SPACE_MAP` on a window, a 32-bit program's null selectors. `SPACE_PLACE`'s count: DID NOT FIRE, to be looked at |
| fails, why | test-sud: native numbers need K1's "all calls exit" (Q2 (a), FAILING until then). test-wine's 32-bit half: Wine allocates top down from 0x7fffffff0000, where K11 has the region (morning question). test-compositor: its stall needs a stop nothing can do (NOT RUN); under load its second boot did not finish. test-clipboard, test-usb-serial: a driver that publishes through a Linux socket or node. test-quarantine, test-log-budget, test-log-pstore, test-log-remote, test-removable: lode, which needs the sealed `drivers.slice` and a processor service token from the kernel. test-mce-driver, test-postmortem, test-selfhost (queued), test-gnome-apps: not finished |
| the same on the hybrid | test-steam-bootstrap and with it test-steam-window: `steam.sh` exits 0 with no client; develop's hybrid FAILS it the same way (mkl-steamboot-hybrid) |
| refused by design | test-flags, test-kaslr, test-netboot: they hold a build line to a profile, and an ad-hoc build carries none |
| **NOT RUN** | test-powerfail, and every row resting on a kernel boot check: no check until B5's re-homing. A flip precondition (L10) |
| **`present`** | `test-boot` FAILS stage 9, FX-0925, on the K1 this branch merged; `feature/mk-restricted`'s newer K1 is said to fix it. The landing takes K1 from there and runs it before joining a batch |

`bench-rows --release --rounds 1`, one boot on a busy host (load near 30), **a reading, not
a baseline**; §7's targets are 1.00x and these **miss**:

| Row | Lift (475bbfce5) | `--personality kernel`, baselines page | Ratio |
|---|---:|---:|---:|
| `null-getppid` | 1,140 ns | 130 ns | 8.8x |
| `mmap-touch-munmap-4K` | 10,142 ns | 1,920 ns | 5.3x |
| `signal-self` | 26,055 ns | 2,050 ns | 12.7x |
| `futex-thread-pingpong` | 11,962 ns | 2,030 ns | 5.9x |
| `fork-exit-wait` | 243 us | 94 us | 2.6x |
| `fork-exec-wait` | 391 us | 494 us | 0.8x |
| `pagecache-miss-4K` | 100 us | 75 us | 1.3x |

`restricted_fastpath` is off here. These are superseded by
[PERFORMANCE.md](PERFORMANCE.md), which also corrects the `fork-exec-wait` baseline: 231 us on
the same tree, so this row's 0.8x was 1.6x to 1.9x.

`test-rustc --release`, one run, load near 12, 51c84db32, against the baselines page's
readings (a dev kernel at load 30): `cold-test-rustc` 3,190 ms against 3,140,
`cold-rustc-hello` 2,100 ms against 2,050, `warm-rustc-hello` 982 ms against 172 ms, the page's unit to be checked before these two
are compared. So real work is far nearer
parity than the bare rows; none of this is a matched run.

**Where a signal's time went** (mk-perf, 081d198c8): CPUID, seven times a signal, each a VM
exit of about 2.8 us: 19 of 24 us. Now asked once: about 4.5 us. An earlier guess here
about the TLB was wrong and is withdrawn. Other readings of mk-perf's, one boot each on a
loaded host, not paired: a wake under a lock deferred to the unlock takes futex from 9.5
to 6.7 us; `fork-exec-wait`'s baseline of 494 us is stale, and `--personality kernel` on the
same tree reads 231 us (fork-exit 121 us), so it is 1.6x to 1.9x, not the 0.8x above.

## 13. The window (ledger 6411, W1-W8)

`SPACE_WINDOW` (0x10BC) and `SPACE_PLACE` (0x10BD), under `ferrix_personality="user"`: the first
hardware mappings in the kernel with no region, rmap or reference behind them. It is
shape-specific kernel code: **it goes if K3 comes back** (M6, R3).

* A restricted space, not measured, with nothing in it, may become a window. Nothing makes a
  region there: `SPACE_MAP` answers `BAD_STATE`; `SPACE_ALLOCATE`, `SPACE_PROTECT`,
  `SPACE_CLONE`, `SPACE_READ` and `SPACE_WRITE` answer `NOT_SUPPORTED`. ptrace reads the shadow.
* A VMO becomes placed for good at its first place. Refused then: `replace`, every decommit
  (`decommit_range`, `decommit_from`, `decommit_clean_range`, `decommit_clean`). Pager-backed
  VMOs are refused, which keeps `supply_from` and `detach` out. **A driver's foreign VMO
  becomes unchangeable**, and the space holds a reference to each object until its tables
  are gone, 4,096 objects at most.
* A batch of at most 64 entries is copied in before any lock, each checked, and stops at the
  first refusal, answering how many went in. What it replaced is shot down before it returns.
* **An unmap** (ledger 6421, U1-U9): at its return, no translation of the range is on any
  processor. Holes are stepped whole; each leaf that came down is named for the flush, the
  whole TLB past `TlbPages`' ceiling; a shootdown of another call's still out asks for
  everything, read under the space's lock; a range with nothing in it and none out begins
  no shootdown. Held at boot by `user::space::window_check` (`window   an unmap's 8 decisions
  held`), and a stale read across processors by `test-threads`' `unmap-stale` at `--smp 2`.
  Controls FIRED at 303f9c8c6: mkp4-c2, c3, c4, c5, c5t, c6b. Requirement id owed from the PO.
* **Owed**: W1's absence from `present` and `desktop`; W5's hold-time line in MEMORY-AND-TIMING;
  W6's sweeps; **W7, a machine check or scrub on a placed frame, which finds no mappers and
  must fail closed, is not built**: until it is, `test-mce` under user shows nothing about
  it; W8's controls; requirements from the block.

## Where it stands

* **On `feature/mk-lift-land`, for landing:** one commit on K1 and K15, reviewed against
  ledger 6012 (R1-R9), 6411 (W1-W8) and 6542 (B1-B7). What follows W2-W8 is in BACKLOG, a
  precondition of any flip. The rows of §12 were measured on the dev tree, hash by hash;
  on this commit `test-linuxd`, `test-shell` and `test-threads` hold, and rows that need a
  driver wait for the boot authority's commit on top of it (ledger 6343).

* Design, 2026-11-03: reviewed **OK IF (1) to (20)**, ledger 5714; §10 above is the
  table. Revision 2's change of §3 goes back with milestone 2.
* **Milestone 1 holds**: `--personality user` builds on x86-64, no `ferrix_kernel_load`
  symbol in the kernel, and `linuxd`.
* **Milestone 2 holds** on `feature/mk-lift` bd9bd269f, over K1's and K11's and K15's
  unlanded branches: `cargo xtask test-linuxd --personality user`, KVM.
* **Milestones 3, 4 and 5 hold** by hand: `test-shell`, `test-btrfs`, `test-net`; **milestone
  6 in progress**, §12's table. x86-64 only; AArch64 waits for K1's half. Nothing landed:
  K1, K11, K15 and boot authority are unlanded, and mk-authority's parts this branch merged
  read NOT YET (ledger 6316). The landing is a fresh squash without 4433d085a.
