# Ferrix as a certifiable microkernel

**Draft, 2026-10-13.** The customer asked for a plan to switch Ferrix to a
microkernel whose ring 0 can be certified whole. This document is that plan.
Nothing in it is decided until the customer answers §10 and the answers are
written under *Decisions* in `docs/BACKLOG.md`. It reopens two earlier
decisions: the 2026-09-16 one (a monolithic core, capability seams, device
drivers in ring 3) and the 2026-09-27 shelving of `docs/OPAQUE-KERNEL.md`'s
option B.

Figures marked *guess* are estimates, not measurements. Points are in the
backlog's units, for the product owner to convert.

## 1. Why

Today ring 0 holds the certified item and the uncertified load together
(`docs/certification/ITEM.md` §2). Measured with
`check-item-boundary.py --report` on `main` at 5450ee1fe:

| Ring | Product lines | In-kernel test lines |
|---|---:|---:|
| `core` | 70,158 | 32,432 |
| `item` | 9,382 | 4,274 |
| `load` | 72,228 | 47,319 |

The item is 52.4% of the kernel's product code. The other 47.6% runs with the
item's privileges, so every claim about the item is argued *around* the load:
28 composition-root edges from `main.rs` into the load, 57 `mod` declarations
of a higher ring, the known-violations list, and AoU-3 forbidding a safety
function anywhere near the load. An assessor reads that as "the certified part
shares an address space and a privilege level with 72,000 lines nobody
assessed". Every SFR that depends on isolation (FDP_IFC, FPT_*) rests on an
argument that a load bug cannot corrupt the item, and in one address space no
such argument holds by construction; it holds only by Rust's safety and the
audit of the load's `unsafe`.

The shape a certificate wants is the separation kernel's: **everything in
ring 0 is the certified item, and everything else is a process the item
isolates.** Then:

* The TOE and the safety element are one artifact with one boundary, the
  privilege boundary, which the hardware enforces.
* Freedom from interference (IEC 61508-3 7.4.2.8 and Annex F, ISO 26262-6
  Annex D) is argued once, for the kernel's spatial, temporal and resource
  isolation, instead of per load subsystem.
* Ferrix's claim (`CLAIM.md` §1, mixed criticality) becomes literal: unmodified
  Linux software in QM partitions beside native safety functions, with no
  QM code in ring 0.
* User-space components can carry certification of their own, at their own
  level, as separate items (§6.3). btrfs is the first candidate.
* Restart replaces hot patch for everything but the kernel, which is what the
  live-update plan and T0 were reaching for one subsystem at a time.

## 2. What the switch is

### 2.1 The target

**Ring 0 contains one ring, `kernel`, and it is the certified item.** The
manifest's `load` ring is empty and then deleted, and so is the
`core`/`item` split: what stays in ring 0 is reviewed, traced and covered
to the item's rules, and what cannot meet them leaves ring 0.

The kernel keeps what only a kernel can do:

* trap, system call and exception entry on x86-64, AArch64 and ARMv7-A;
* address spaces, VMOs (anonymous, physical, contiguous, pager-backed),
  mapping into a space named by a handle;
* threads, processes, jobs, quotas, the scheduler with time budgets;
* handles and rights, channels, ports, events, timers;
* interrupts as objects, the IOMMU, I/O mappings, pins;
* SMP, speculation domains, the speculation defences (`SPECULATION.md`);
* **restricted mode** (§3.1), the mechanism that lets a personality server
  run a Linux program's thread;
* a write-only debug log, the panic path (serial only), the kernel's own
  entropy seed;
* the firmware tables needed to find memory, processors, interrupt
  controllers, timers and IOMMUs. Everything else in ACPI or the device tree
  is handed to user space as a read-only VMO.

Everything else leaves ring 0: the Linux personality, the VFS and page-cache
policy, every filesystem, the network stack, the device cores (display,
render, audio, input, chardev, block ring, net ring, logctl), PCI enumeration
and devmgr's in-kernel half, the console's screen and input, the panic
screen's drawing, the board glue (`platform/st/stm32mp1.rs`,
`platform/google/gs201/usb.rs`), and the in-kernel checks that exercise them.

### 2.2 The shape: option C, with restricted mode

`docs/OPAQUE-KERNEL.md` §1 drew three shapes. B (servers behind an in-kernel
page cache) leaves the VFS, the page cache, the fd table, pipes, epoll and
procfs in ring 0. That is roughly half of today's load, and it keeps ring 0
uncertifiable as a whole. **This plan is option C**, with the one change that
answers §1's objection to C ("the compiler's hot path becomes IPC round
trips"):

* A Linux program's threads run in **restricted mode** inside a personality
  process, `linuxd`. A Linux system call or fault in the program leaves
  restricted mode and resumes the same kernel thread in `linuxd`'s code, in
  the same process, with no switch of thread, scheduler decision or channel
  message. This is the mechanism Zircon built for starnix.
* `linuxd` holds the VFS, the page cache's policy, the fd table, pipes,
  epoll, signals, credentials, procfs, sysfs, cgroupfs, devfs and the pty.
  A cached `read` is a restricted-mode exit, a function call in `linuxd` and
  a copy into the program's memory, which `linuxd` maps. It never leaves the
  process.
* The page cache's **pages** live in kernel VMOs with `linuxd` as their
  pager. A file `mmap` maps the VMO into the program's space, and a fault on a
  resident page is resolved by the kernel without `linuxd`. Only a miss goes
  to the pager. That keeps rustc's `mmap`-heavy path where it is today.
* Servers that are not the personality talk to `linuxd` over channels: the
  filesystem servers on a miss or a metadata write, the network stack on a
  socket operation, the device servers on a `/dev` file operation.

The hot path the 2026-09-16 decision protected (warm `open`, `stat`, `read`,
`mmap` on cached files) costs one restricted-mode round trip more than a
system call does today, and no IPC. §7 sets that as a gated budget.

### 2.3 One `linuxd` per Linux partition

A `linuxd` and the programs it runs form one partition: one job, one quota,
one time budget. A compromised `linuxd` reaches its own programs and nothing
else. Containers, which today are namespaces in the kernel
(`NAMESPACES.md`, `PIDNS.md`, `NETNS.md`), become separate `linuxd` instances
where that is cheaper, or stay namespaces inside one `linuxd`; that choice is
`linuxd`'s and does not touch the kernel.

## 3. Kernel mechanisms the switch needs

Each is a landing with its own design review before code (CONVENTIONS,
*Changes to the certified item go through review*), its own requirements
(reserved first), checks and negative controls. Each is useful before any
code leaves ring 0, so none of them is wasted if the switch stops half way.

| # | Mechanism | Today | What is new |
|---|---|---|---|
| K1 | **Restricted mode**: enter a saved user context, return on syscall, fault or kick with the context saved; a kick from another thread for signals | nothing | entry/exit on three ISAs, vector-state rules (§9.8 3a's contract carries over), the speculation barrier between the program and `linuxd` (same process, different trust: an open question for the consultant, §10 Q5) |
| K2 | **Address space by handle**: map, unmap, protect in another process's space, or in the restricted region | `VMO_MAP` into the caller only | a space handle with rights; the Linux `mmap` family becomes `linuxd` calling it |
| K3 | **Pager-backed VMOs**: a VMO whose missing pages are requested from a user pager over a port; dirty tracking and write-back requests | none (`fs/pages.rs` fills from the block ring in ring 0) | the pager protocol, the kernel's fault path waiting on the pager with a timeout, eviction under the job's quota |
| K4 | **Exception channels**: faults of native processes delivered to a supervisor | panic or kill | needed for supervised servers and for `linuxd`'s own faults |
| K5 | **Kernel futex on user addresses** | the Linux `futex(2)` in the load | a generic wait-on-address primitive; `linuxd` maps Linux's operations onto it |
| K6 | **Kernel-attested sender identity** on a channel message | none | `OPAQUE-KERNEL.md` §2 point 4: the job and process written by the kernel, never taken from the body |
| K7 | **Charging that follows the client** | F-35/F-37 charge the caller's job | `OPAQUE-KERNEL.md` §2 point 5: a token tied to one outstanding request, valid until the reply, so a server can never name a job to charge |
| K8 | **Resource capabilities** for devmgr: MMIO ranges, interrupt lines, ECAM, I/O ports, the IOMMU's units | the kernel enumerates PCI and the device registry claims on devmgr's behalf | a root resource handle that devmgr divides; `device.rs`'s exclusive claim is kept, granted by handle instead of by the kernel's own enumeration |
| K9 | **Time budgets per job** (temporal isolation): a guaranteed share and a cap within a period, enforced by the scheduler | `cpu.max` throttling (a cap only) | a reservation a QM job cannot take away, seL4 MCS's scheduling contexts as the reference; this is the temporal half of freedom from interference |
| K10 | **Timer objects** | the Linux personality's timers in the load | a deadline object signalled to a port, with slack |

Retired once their users have moved: the class-specific creates and calls
`BLOCK_RING_CREATE` (0x1048) through `CHARDEV_SYNC_RESOLVE` (0x1064), and
`DEVMGR_START`. A device server is built from channels, VMOs, interrupts,
I/O mappings and pins, which the kernel already has.

## 4. The move: lift the load out whole, then split it

`OPAQUE-KERNEL.md` §4 planned to take subsystems out one at a time, each
behind an in-kernel forwarding layer (a served inode, a forwarding socket).
Under option C that forwarding is written, gated, certified and then deleted
when the VFS itself leaves. This plan avoids building it:

1. **Lift.** Build the whole `load` ring as one user program, `linuxd`,
   against a shim that replaces each core interface the load calls with a
   native call. Measured on 5450ee1fe, the load's product files name 89
   distinct `crate::` paths in the core and item rings, most of them in
   `object`, `sched`, `console`, `sync`, `timer`, `arch`, `fallible`, `user`
   and `trap`. That list is the shim's first draft, and K1 to K10 are what
   it needs from the kernel. The `Personality` trait (W-5) is already the
   seam between the item's decode and the load.
2. **Run both.** The in-kernel load and `linuxd` build from the same source
   behind one build switch, `--personality kernel|user`. `kernel` stays the
   reference configuration until `user` passes every gate the load passes
   today (§8, M5).
3. **Flip and delete.** `user` becomes the default and the reference, and the
   in-kernel build of the load is deleted in the same stage. Two personality
   builds are never kept past one stage.
4. **Split.** Inside user space, `linuxd` is cut along the lines B drew:
   `btrfsd`, `netd`, the device servers. These are user-space refactors.
   They change no kernel code and need no kernel review, except where a
   split needs a new kernel mechanism.

The lift is large but mostly mechanical: the load is Rust, already
`no_std`-shaped, and already reaches the core through named paths. What is
not mechanical is listed in §5.

## 5. What is hard

* **Copies across spaces.** In ring 0 the load reads user memory with the
  core's user-copy routines. In `linuxd` the program's memory is the
  restricted region of its own process, so a copy is a plain access with
  fault handling (K1 returns the fault to `linuxd`, not a panic).
* **Interrupt-time work in the load.** The device cores and the block and net
  rings take wake-ups in kernel context today. In user space each becomes a
  port wait in a server thread. The wake paths (`seam-trip`'s hops) change
  shape and must be measured again.
* **The page cache.** Splitting it into kernel-held pages and a user pager
  (K3) is the one change that touches both rings at once. Writeback, `msync`,
  `fsync`, eviction under quota and the power-fail rules (`test-powerfail`,
  AoU-15, AoU-16) all cross the new line.
* **Signals and `ptrace`.** Delivering a signal to a thread running in
  restricted mode needs the kick (K1). `ptrace`, seccomp and core dumps move
  into `linuxd`, which sees every call.
* **The 32-bit ABIs.** i386 under x86-64 (`I386.md`) and ARMv7-A programs run
  through `linuxd`'s compat tables. On ARMv7-A, restricted mode shares a 3 GiB
  user half between the program and `linuxd`, which shrinks the program's
  space; §10 Q7 asks whether that is acceptable.
* **Boot.** The kernel starts only init (L12 already does), init starts
  devmgr and `linuxd`, and the root mount becomes `linuxd`'s, not the
  kernel's. The kernel's re-root of pid 1 (INIT.md §7.3) goes.
* **The in-kernel checks of the load.** Stage 8 to 12's boot checks that
  exercise fs, net and block in ring 0 become user-space test programs run by
  the gates. The kernel's own checks stay and grow (§6.2).
* **The NVIDIA and card work.** `nvrm` reaches the chardev core
  (`CHARDEV_*`), which moves to a server. The card VM protocol and the N
  stages must be re-planned against the server, not broken by the move.

## 6. Certification on the new boundary

### 6.1 The kernel as the item

* `ITEM.md` is rewritten around one ring. The composition-root edges, the
  higher-ring `mod` declarations and the known violations go to zero, and
  `check-item-boundary.py` asserts that ring 0 has no `load` ring at all.
* The Security Target's TOE is the kernel. The Linux personality is already
  a threat agent outside the TSF (ST §3.2). Servers that enforce file
  permissions (FDP_ACC) are named as environment in an A./OE. pair, on the
  OE.AUTH precedent.
* The safety manual's AoU-3 changes from "the load in ring 0 is excluded" to
  "no user-space component is part of the safety element unless it is
  certified as one (§6.3)". The integrator places safety functions in native
  partitions with K9 budgets.
* `MEMORY-AND-TIMING.md` gains the temporal half: every system call bounded,
  the kernel's longest non-preemptible section stated and measured per
  architecture, K9's budget guarantee argued. A microkernel's claim to
  isolation is only as good as this.
* Kernel memory stays charged to jobs (F-35, F-37) and a partition's kernel
  memory is reserved, not only capped, so a QM partition cannot exhaust the
  memory a certified one needs (the frame watermark of the ledger's
  endfield-oom review is the start). A seL4-style "no kernel heap" design is
  the route to SIL 3 and is out of this plan's scope (§10 Q8).

### 6.2 What the kernel's code must meet

The item's existing rules apply to every line in ring 0, and they are stated
here because today they apply only to half of it:

* Requirements: every kernel function traced to a low-level requirement, one
  check each, reserved before writing.
* Coverage: 100% of statements covered or individually justified on every
  architecture (F-10's rule), decision coverage as recorded today.
* Allocation: every allocation fallible (F-23's rule, held by an `xtask
  check` step).
* `unsafe`: every site traced (F-26's rule).
* No panic reachable from a system call except the documented fatal ones.
* Static stack depth bounded and checked in the build, per architecture.
* A qualified toolchain for every target the kernel builds for (F-17).

### 6.3 Certified user-space components

A user-space component can be certified on its own, as a separate item with
its own manifest entry, requirements and coverage, and the kernel's
isolation as an assumption it rests on. Proposed first: `btrfsd`, built from
`ferrix-btrfs` and `ferrix-btrfs-write`, which are certified today and leave
the item if they move into an uncertified `linuxd` (the customer's
2026-10-02 rule). `btrfsd` is therefore split out of `linuxd` in the same
stage as the lift, not later (§8, M5), so btrfs never spends a release
uncertified. Others are the customer's choice: devmgr, init, a block driver
for a safety partition's storage.

### 6.4 What this does not change

The steps outside the repository still block every target: the accredited
pre-assessment (`CLAIM.md` M2), a quality management system (F-28),
independent reviewers (F-27), a position on AI-authored code (F-29) and a
qualified toolchain (F-17). A smaller ring 0 makes each of them cheaper; it
replaces none of them. The consultant's reviews remain internal and are not
independent verification.

## 7. Performance budgets

The switch is gated by numbers, as S0 gated B. Each row is a perf row with
its measurement method fixed before the work, measured on nazuna under the
gate's KVM, alternated against `main`.

| Figure | Today | Budget before the lift lands as reference (M5) |
|---|---|---|
| Native channel round trip, `domain-call` p50, mitigations on | about 820 ns (2026-10-07) | at or below seL4's 440 ns (the customer's existing target) |
| Bare Linux system call (`getppid`), `qemu64` with controls | 357 ns, Linux 53 ns (2026-10-10) | *guess* within 2x Linux on the same CPU model, before restricted mode is added |
| The same call through restricted mode and `linuxd` | not built | *guess* within 3x Linux |
| 4 KiB block read, ring and ring-3 driver | 300 to 844 us (2026-09-27) | under Redox's 80 us p50 (the customer's existing target) |
| Warm `rustc hello.rs` | 0.14 s | within 5% of `--personality kernel` |
| Cold `test-rustc` and the self-host build | today's | within 10% |
| Chrome network bench, the compositor's frame time, `test-unity` | today's | within 5% |

The thresholds are this draft's proposal; §10 Q3 asks the customer for the
real ones. If the restricted-mode budget cannot be met, the plan stops at M4
with every mechanism landed and nothing lost, and the customer decides again
on the numbers, as on 2026-09-27.

## 8. Stages

Points are *guesses*. Each stage's gate must pass before the next stage's code
lands; designs for the next stage may be written and reviewed in parallel.

| Stage | What | Points | Gate |
|---|---|---|---|
| **M0** | Decisions (§10) written in BACKLOG; the perf rows of §7 with methods and baselines; the manifest gets a `--personality` axis and the `load` ring a "must shrink" check | 8 | the customer's answers; baselines recorded |
| **M1** | K5, K6, K7, K10 (small, independent) | 20 | each mechanism's checks and controls; consultant OK each |
| **M2** | K2, K3, K4 (address space by handle, pager VMOs, exception channels); the in-kernel page cache moved onto K3 **while still in ring 0** | 35 | `test-btrfs`, `test-powerfail`, `test-rustc` unchanged; F-37's charging tests |
| **M3** | K1 restricted mode on x86-64, then AArch64, then ARMv7-A; K9 time budgets | 40 | the restricted-mode perf row; a K9 interference test (a QM job at full load cannot push a budgeted job past its deadline) |
| **M4** | The perf prerequisites of §7 that are not K-items: the round trip, the bare system call, the block read | already on the backlog | §7's rows |
| **M5** | **The lift**: `linuxd` built from the load behind `--personality user`; `btrfsd` split out; devmgr and PCI enumeration out on K8; console screen and input out | 120 to 180 | every gate in `docs/BACKLOG.md`'s gate table under `--personality user`, §7's rows, Steam and Chrome rows; then the flip, and the in-kernel load deleted |
| **M6** | **Shrink**: the panic screen's drawing out, discovery cut to §2.1's list, board glue to drivers, retired native calls removed; `core` and `item` merged into `kernel` | 25 | `check-item-boundary.py` with one ring; coverage carried; boot lines identical on four boots where the code did not change |
| **M7** | **Evidence**: ITEM.md, ST, SAFETY-MANUAL, MEMORY-AND-TIMING, TRACEABILITY, VULNERABILITY-ANALYSIS, CLAIM rewritten for the new boundary; the stack-depth check; coverage at 100%-or-justified on the whole kernel | 40 | the consultant's OK on each document; register recounted |
| **M8** | **Split** `linuxd` in user space: `netd`, then the device servers | 40 | `test-net`, compositor, audio, card rows; kill-and-restart rows per server |

Total *guess*: about 330 to 390 points, three to four times option B's 117.
M1 to M4 can run in parallel streams split by file ownership (CONVENTIONS,
*Splitting one piece of work across several agents*). M5 is one stream with
one owner; its shim is a single interface and splitting it early makes two.

## 9. Risks

* **The lift stalls half way** and leaves two personality builds for months.
  Mitigation: the build switch lives one stage only, and M5 does not start
  until M1 to M4 have landed.
* **Performance.** If restricted mode costs more than §7 allows, Linux
  software gets slower everywhere at once. Mitigation: M3 measures it before
  M5 starts, and §7's stop rule.
* **Desktop and Steam regressions** during M5, the customer's main priorities
  this month. Mitigation: `--personality kernel` stays the default until M5's
  gate, so the desktop runs on today's path throughout.
* **Certification debt moves rather than shrinks.** The kernel's evidence must
  be re-based on a moving tree for months. Mitigation: requirements of code
  that does not change keep their ids and checks; only the boundary documents
  are rewritten, in M7, once.
* **The plan competes** with NVIDIA, Steam, the installer and the certification
  backlog for the same sessions.

## 10. Decisions for the customer

1. **Reopen the 2026-09-16 decision** (monolithic core) and the 2026-09-27
   shelving, for option C with restricted mode as drawn here?
2. **Order**: lift-then-split (§4, recommended), or B's subsystem-at-a-time
   order with in-kernel forwarding?
3. **The budgets** of §7: which regressions are acceptable for Linux software,
   and is the stop rule right?
4. **btrfs**: a certified `btrfsd` split out at the lift (recommended), or
   btrfs inside `linuxd` and out of the item?
5. **Restricted mode's speculation rule**: the program and `linuxd` share a
   process but not trust. A barrier on every exit costs on every Linux system
   call. The consultant's view is asked first; the customer decides the
   matched-figure rule for it as for the round trip on 2026-10-02.
6. **Which user-space components become certified items** beyond `btrfsd`.
7. **ARMv7-A**: restricted mode there shares the 3 GiB user half. Keep ARMv7-A
   for Linux programs, or make it native-only (the DK1's safety role)?
8. **Ambition**: SIL 2 / DAL C / Class C / EAL5+ for the kernel (the present
   targets), with SIL 3 and a no-kernel-heap design left for later?

## Where it stands

Draft on branch `microkernel-plan`, 2026-10-13. Sent to the certification
consultant for a design review of the plan.
