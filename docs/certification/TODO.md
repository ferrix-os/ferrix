# Auditor's work list

The audit-side list: what to re-measure, how to know a finding closed, and
what deliberately must not be written. For the engineering work itself see
[IMPLEMENTATION.md](IMPLEMENTATION.md), which carries the design detail this
file only gestures at — notably that the obvious reading of F-01 is the wrong
change.

Ordered by value per unit of effort, not by finding number. Each item says what to do, which finding it
closes, and how to know it is done — because "done" here means something in the
build says so, not that a document claims it.

Finding ids refer to [FINDINGS.md](FINDINGS.md). The scope of everything below
is the item defined in [ITEM.md](ITEM.md) and enforced by
`tools/common/data/certification-item.json`.

---

## 0. Before touching anything

### 0.1 Run the full gate set on this branch
**Done 2026-09-25** (IMPLEMENTATION.md, order zero). Kept for the next audit
branch, which will be in the same position.

The audit ran the eleven fast gates, `cargo fmt`, crate layering, the
SysML/arch-doc gate, and an `xtask` build. It did **not** run the ten
cross-target clippy passes or the host test suite against these changes.

```
cargo xtask check
```

Expect this to be the first thing that fails. `check.rs` gained two steps and
`qemu.rs` gained an env-var branch; clippy for three kernel targets, three
loader targets and the native programs has not seen any of it.

### 0.2 Re-measure before citing
The audit branch was cut at `bd78a286` (2026-09-24) and has since landed on
`main`. Its numbers were reconciled against the gates on 2026-09-26. Sibling
sessions land concurrently, and the load ring in particular grows with every
unrelated feature, so re-measure before citing:

```
python3 tools/common/check/check-item-boundary.py --report
```

#### Arguments to re-apply at the next re-measure

The discovery Finder landing (2026-09-30) moved lines that seven coverage
arguments stood on, and `carry-coverage.py` dropped them: an argument may only
stand on a line the last measurement found uncovered, and these lines have not
been measured since they moved. Their reasons did not change. Until the next
re-measure (F-13's) the new lines count as **unmeasured, not as needing a
test**; after it, re-create each argument on the line the re-measure reports,
with the text below, and delete this list.

| Kind | Architectures | Was | Is now | Reason, verbatim |
|---|---|---|---|---|
| failure-path | x86_64, aarch64, armv7a | `main.rs:718`, `check_pci`'s `Err(problem) => fatal!(` | `main.rs:724-725`, `check_devices`' `Err(device::Stopped::Finder { name: "pci", why })` arm | The failure arm in `check_pci`: it runs only when the bring-up or self-check it follows has reported a property broken, and it stops the machine with the catalogue's `STAGE10_PCI` and FERRIX-PANIC. A passing boot is by definition one that never takes it; making it run means breaking what that step proves. |
| failure-path | x86_64, aarch64, armv7a | the same, reached through the walk | `discovery/pci.rs:544`, `self.failure = Some(failure);` in `pci::Enumeration::find` | As above: the PCI walk's failure, which `check_devices` halts on under `STAGE10_PCI`. |
| failure-path | x86_64, aarch64, armv7a | `main.rs:790`, `check_devices`' `Err(problem) => fatal!(` | `main.rs:735-736`, `check_devices`' `Err(device::Stopped::Node(problem)) => fatal!(` | The failure arm in `check_devices`: it runs only when the bring-up or self-check it follows has reported a property broken, and it stops the machine with the catalogue's `STAGE10_DEVICES` and FERRIX-PANIC. A passing boot is by definition one that never takes it; making it run means breaking what that step proves. |
| absent-hardware | armv7a | `main.rs:763`, `if completed.before_fault {` | `discovery/pci.rs:598`, the same line in `pci::Enumeration::report` | When the device completed the out-of-domain write the unit faulted: the probe runs only through a translated domain, which ARMv7-A, leaving virt's SMMUv3 alone, never gives. |

Line numbers are the tree's at the landing; the re-measure's report is what
decides where each one goes.

The landing also added two failure arms with no argument parked:
`main.rs:727-728`, `Err(device::Stopped::Finder { name, why })` (a tree or
board finder's failure), and `main.rs:731-732`, `Err(device::Stopped::Again)`
(a second publish). Each is a failure arm under `STAGE10_DEVICES` like the
third row; argue each with that row's reason at the re-measure, or they will
show as needing a test.

#### Files never measured

Lines in these files count as **unmeasured, not needing a test**, until the
next re-measure takes them in. The core ring's figures in README.md §2
describe a tree without them.

| File | Ring | Lines | Since |
|---|---|---:|---|
| `discovery/finder.rs`, `discovery/tree.rs`, `discovery/board.rs` | core | moved and new code of the Finder | cc4e14af |
| `discovery/description.rs` | core | the ACPI-or-tree decision | fa4e88a6 |
| `sched/trip.rs` | core | 448 | 5cc5ed38 |
| the lines S2's hook added to `trap.rs` and the four system-call entries | core | the hook and `trap::ask` | 6f47bbd2f |
| the lines the speculation domain added to `arch/*/speculation.rs`, `user/space.rs`, `object/{process,job}.rs` | core | the switch rule, the marking and leaving | bf9efba95 |
| `user/cache.rs` | core | 192, and the lines the reclaim added to `user/vmo.rs`, `user/space.rs`, `object/quota.rs`, `object/job.rs`, `object/oom.rs` and `sched/` | the cgroup controllers landing (M2, F1, S2) |

COVERAGE-RESIDUAL.md's unreached counts fell at 65639967 (x86-64 720 to
683, AArch64 724 to 706, ARMv7-A 1,109 to 1,089) because moved lines left
the measurement, not because a test reached them. Do not cite that fall as
progress.

#### Where the reviews stand (consultant's wind-down, 2026-10-01)

The certification consultant (os-ad, then os-bd after a restart) reviewed
every change to the item on 2026-10-01 before it landed, and audited `main`
back to the N3 review for changes that had skipped review. Each verdict and
condition is in the ledger on the gate host,
`~/.local/share/ferrix/cert-consultant/reviews.md`, whose `HANDOVER.md`
lists the open queue. On `main` they are recorded here:
- `docs/NAMESPACES.md` §12, for N4, smallns and the native-child rule;
- `docs/SECCOMP.md` §12, for S1 and S2;
- `docs/OPAQUE-KERNEL.md` §8 and §9, for the trip trace and the speculation
  domain;
- `docs/CLAUDE-CODE.md`, for the XSAVE and Zenbleed controls re-run;
- `docs/BACKLOG.md` P2, for what N4 and the discovery Finder owe.

**Landed ahead of its evidence.** The speculation domain (bf9efba95)
landed, on the customer's word, before six of its eight negative controls,
its release build, its five boots, `test-shell`, `test-threads` and
`test-vfs` had finished. Until their gate.sh INDEX lines read
`control: FIRED (panic)` and `run: PASSED`, cite the domain's checks only
by the check itself, not as shown to fire. A failure is red on `main`.

**Reviewed, not landed** (their authors hold the conditions):

| Branch | Verdict | What it waits on |
|---|---|---|
| `stage13-s3` (seccomp filters) | cleared on evidence | the `s3f-*` gate rows and the 19 `s3c-k*` controls |
| `stage13-s4` | design OK with conditions | L.trap.8 reserved; native calls fail closed; a restart-code case |
| `stage13-cgctl` | fixes accepted | its gate and 18 controls |
| `stage13-netns` | OK with conditions | `IFLA_NET_NS_PID` through `pidns::find_in`; neighbour and reassembly charging or a VA residual; records; a gate.sh re-gate |
| `stage13-fdinfo` (NP) | not yet | the ARMv7-A boot; the dumpable test after the capability path; the newborn child failing closed |
| `os35/ipc-lazytlb`, `ipc-ring` | author gone | must carry the domain's switch hooks, and renumber ring B's L.object.106 |

**How evidence is cited now.** A negative control counts when fleet/gate.sh
says `control: FIRED (panic): <line>` or `FIRED (line, no panic)` with its
`guest started:` line. A gate row counts when it says `run: PASSED` on the
commit that lands, with the accelerator named. Requirement ids are reserved
in `tools/common/data/requirement-reservations.json` before they are
written (`docs/CONVENTIONS.md`).

---

## 1. Boundary — the cheapest real wins

The debt register in `tools/common/data/certification-item.json` is empty: no upward
references, down from 94 when the audit began (W-5, 2026-09-26). It may not
grow. A change that needs a new upward reference has to add an entry against a
finding, which is a diff somebody argues for; the gate fails otherwise.

Until 2026-09-26 the gate reported 29 and 62: it saw only the literal text
`crate::a::b`. **When re-auditing, check the gate before trusting its count.**
`python3 tools/common/check/check-item-boundary.py --self-test` runs the lexer's and the
resolver's cases; every normal run runs them first too. The things it still
cannot see are listed in its docstring -- chiefly a load-ring type reaching an
item file through a value, with no name written.

### 1.1 Split `Process` into a core object and a POSIX extension — **F-01, 10 references; F-06, 3**
**Done 2026-09-26** (IMPLEMENTATION.md W-1). F-01 and F-06 are closed: the
core process is `src/kernel/src/object/process.rs`, and nothing under `object/` or
`sched/` names the personality. Seven references are gone.

What to re-check at the next audit, because it is where a later change could
quietly undo this:

* **The core type grows no personality field.** `object::process::Process`
  has seven fields. One typed as, or leading to, POSIX state -- or a
  type-erased slot for "the extension" -- restores the dependency the split
  removed, and the gate would not see it, since it names nothing.
* **`Host` stays small.** Five methods today. Each new one is a question the
  core asks the personality; one that is really a POSIX question (the fd
  table, credentials) belongs in the personality, not on the trait.
* **The six item-ring references to `syscall::process` went to F-09 and F-07,
  and are gone with them (W-5).** They were to POSIX state, and were not
  "fixed" by adding POSIX methods to `Host`: the files that wanted the state
  are in the load ring, and what the native ABI asks of the personality is
  the item's own interface (`native::Processes`), not the core's. Keep it so.

### 1.2 Invert the `StatLayout` dependency — **F-03, 3 references**
**Done 2026-09-25.**

Each `arch/*/mod.rs` declares `STAT_LAYOUT: crate::syscall::stat::StatLayout`.
The personality should ask the arch facade which layout it wants. Smallest fix
here; do it while learning the gate.

### 1.3 Board and ring registration — **F-04 and F-05, 7 references**
**Done**: F-05 on 2026-09-25, F-04 on 2026-09-26. Board support registers
`BoardBinding`s with the registry and its boot mode with power, from
`main.rs`'s `register_load`.

*To re-audit:* the three bindings find nothing under QEMU, which has no
STM32MP15 tree, so the only evidence the DK1 still publishes its display, USB
host and GPU is a board boot. Look for the `display`, `usb` and `gpu` lines.

`device.rs` names `stm32mp1*`; `claim.rs` names `block_ring`. Both want
registration into the core rather than the core naming them.

### 1.4 Invert the trap-return upcall — **F-02, 7 references**
**Done 2026-09-25**, with F-02a. The trap path names nothing above the core.

`arch/*/signal.rs`, `arch/*/trap.rs`, `arch/x86_64/syscall.rs` and `trap.rs`
call `syscall::deliver::{needs_attention, return_to_user, sigreturn}`. The core
should define a hook the personality registers into at init.

*Hardest of the boundary items and the most valuable after F-01*: it is on the
most trusted path in the system, and while it stands the core cannot be built
or analysed without the personality present.

### 1.5 A registration table for the native dispatcher — **F-07, 12 references**
**Done 2026-09-26** (IMPLEMENTATION.md W-5). The six calls about a subsystem
above the item are a table the subsystems register handlers into; native
processes are made through a `Processes` the personality lends; a quiesce
waits out registered `Server`s. `native.rs` and `devmgr.rs` name nothing above
the item.

*To re-audit:*

* **The boot check runs.** The `match` no longer holds the six table calls to
  an answer at compile time; `main.rs` does, at boot (FX-0006), and prints
  *"6 native calls answered above the item"*. A seventh call moved to the table
  without being added to `native::SERVED` answers `ENOSYS` rather than failing
  the boot -- look for it in the `match`'s one table arm.
* **The rights stay in the item.** A table handler for a device control
  channel goes through `native::control_channel`, which checks the device
  handle's `MANAGE` and mints the driver's handle. A handler that looks up a
  handle itself has moved a capability decision into the load ring.

### 1.5a The Linux personality in the item ring — **F-09, 39 references**
**Done 2026-09-26** (W-5). The trap entries reach the dispatcher through a
`SyscallEntry` the core holds; the Linux dispatcher's routing is
`syscall/linux.rs`, the item's `Personality`, composed by `main.rs`; and `futex`,
`limits`, `memory`, `system` and `thread` moved to the load ring, each argued
in ITEM.md §2.

*To re-audit:* the Spectre clamp is in `arch::decode_syscall`, which the item's
`dispatch_with` calls before it hands the call on -- a personality reached some
other way would have to clamp for itself. And the five files moved without a
code change, so a later change that has the item call one of them again shows
up as a new upward reference, which is the gate doing its job.

### 1.6 Bring-up and power — **F-08, 6 references**
**Done 2026-09-26.** Power commits registered `Flush`es, init starts pid 1
with a registered `Launcher`, and `devmgr` reads with a registered
`ReadFile`; `main.rs` checks all three are there before anything uses them.

*To re-audit:* the two kinds of edge the gate was blind to when this closed
are measured now. `devmgr.rs`'s nested `use crate::syscall::{exec, process}`
is filed under F-07. `main.rs`'s bare-path calls into the load are the
composition root's, listed under `composition_root` in the manifest (ITEM.md
§2); read that list, since it is ratcheted but not filed against a finding,
and check each new entry is composition rather than item logic.

### 1.7 The paranoid entry's boot check — **F-33, 5 references**
**Done 2026-09-26** (W-5). The check is `arch/x86_64/paranoid/check.rs`, a
child of the entry's module that the manifest counts as verification. The
core's product code names nothing above it.

---

## 2. Coverage — measured everywhere, ratcheted, and short of 100%

One command runs the suite on an architecture and fails below its floor:

```
FERRIX_DRCOV=/home/johndoe/Documents/qemu/qemu/build/contrib/plugins/libdrcov.so \
  cargo xtask coverage --arch x86_64 \
    --init "$HOME/.local/share/ferrix/busybox/{arch}/bin/busybox.static"
```

It prints the `coverage-report.py` command it ran; add `--json` and
`--residual` to regenerate `coverage-<arch>.json` and
`coverage-residual-<arch>.json`, then run
`tools/common/gen/gen-coverage-justification.py`. **Re-measure rather than trust the
figures below**: the ones published on 2026-09-25 were wrong (2.3), and the
kernel moves under them.

### 2.1 Measure AArch64 and ARMv7-A — **F-12**
**Done 2026-09-25, the suite since 2026-09-26**: AArch64 73.7%, ARMv7-A 70.9%
(`--smp 2`); 90.2% and 84.8% on 2026-09-27 under the corrected tool, with
their own suite boots and checks (2.3). AArch64's single-boot figure first published, 46.1%, had the
sentinel defect in 2.3; ARMv7-A's 70.8% did not, and stands for its tree.

### 2.2 Measure the release profile — **F-11**
**Done 2026-09-25, re-measured 2026-09-26**: 75.2% against debug's 71.6% on one
boot. The denominator shrinks by a third and the percentage moves a few points;
VERIFICATION.md §3.2 states it.

### 2.3 Cover what needs a test — **F-10**
**Measured 2026-09-26** at 74.7% on x86-64 over thirteen gates. The 81.9%
before it was the net of two defects in `coverage-report.py`, one each way
(VERIFICATION.md §3.4), both fixed. COVERAGE-RESIDUAL.md sorts the residual
per architecture: on x86-64 146 argued, 259 depending on the machine, **1,320
that need a test**.

The work left is tests, one module at a time, from COVERAGE-WORKLIST.md.

**Re-measured the same day on x86-64** at 82.2% over sixteen boots, with two
more defects of the tool fixed (VERIFICATION.md §3.4): 757 need a test. The
architecture code, `trap` and `smp` are done on x86-64 -- covered, or argued
per statement in `coverage-argued-x86_64.json`. Next by size: `user/space.rs`
110, `iommu.rs` 83, `main.rs` 68, `syscall/native.rs` 49. The Arm pair
repeat the pass for their own architecture code.

**Re-measured on all three 2026-09-27**, after the memory layer, the objects,
the Arm architectures and the kernel's services were each taken the same way:
**89.5%** on x86-64, **90.2%** on AArch64, **84.8%** on ARMv7-A, with
**77, 146 and 130** statements that still need a test and the rest argued per
statement or put down to absent hardware. **The Arm pair again on
2026-09-27**: 89.9% and 84.5%, with 145 and 163 needing a test. `trap.rs`,
`user/` and `arch/aarch64` are off the worklist (test-vfs in the Arm suite,
the trap check's read past a file's end, the TRNG against scripted
firmware, and arguments for the 16550 and `ramoops` consoles); init's start
of `devmgr` (L12) put 78 and 82 of `devmgr.rs` on it, which a boot under
`ferrix.devmgr=init` in the suite would take. Next by size: that, then
`syscall/native.rs` (24, 7, 9) and `object/` (17 each).

### 2.4 Make coverage a ratchet
**Done 2026-09-26** as `cargo xtask coverage` against `coverage-floor.json`.
Not in `cargo xtask check`, since it needs boots, and not in CI, whose packaged
QEMU carries no drcov plugin. Raise the floor with the evidence.

### 2.5 Build a complexity and recursion gate — **F-25**
**Done 2026-09-25** by `tools/common/check/check-complexity.py`.

The one code gate the audit did not build. A SIL 2 coding standard must specify
complexity metrics; eleven gates enforce other things and none bounds
cyclomatic complexity, function length or recursion.

Follow the ratchet pattern `check-item-boundary.py` uses: measure, record the
baseline, refuse growth. Without a Rust parser, approximate complexity by
counting branch keywords per function and **say in the docstring that it is an
approximation** — the house rule is that a number whose caveats travel
separately is worse than none.

---

## 3. Traceability — the largest structural gap

F-14, F-15 and F-16 together. 49,431 lines of item product code trace to 33
system-level requirements, and no test names a requirement id. **The fix is not
more testing** — there are 31,135 lines of in-kernel self-test. It is
requirements for the existing tests to discharge.

### 3.1 Write low-level requirements for the item's modules — **F-15**
In `docs/sysml/`, with stable bracketed ids as `01-requirements.sysml` already
does. Verifiable statements with pass/fail criteria, not the narrative
rationale the current 33 are (F-16). This unblocks DAL C, 62304 §5.4 and
`ADV_TDS.3` simultaneously.

Start with the eight objectives in [SECURITY-TARGET.md](SECURITY-TARGET.md) §7,
which already map objective → code → test. That is the shape; it needs to
reach module granularity.

### 3.2 Attach requirement ids to assertions — **F-14**
The in-kernel checks print counted quantities already (*"2387 mappings swept,
899 executable, none writable"*). Each needs the requirement it discharges.

### 3.3 Gate the matrix
Generate a traceability matrix and fail the build on a requirement with no
verification or a test naming a requirement that does not exist. Same pattern
as `gen-arch-doc.py --check`.

---

## 4. EAL5-specific

### 4.1 Vulnerability analysis against the ST's threat model — **F-21a**
**Done 2026-09-25** by VULNERABILITY-ANALYSIS.md. It found V-01 (no SMAP, SMEP
or PAN), which F-32 then fixed on x86-64 and AArch64; it stands on ARMv7-A
and on the reference `cortex-a72`, which lacks PAN.

`AVA_VAN.4` wants methodical analysis against moderate attack potential. The
Security Target states seven threats — T.MEMORY, T.ESCALATE, T.FORGE, T.DMA,
T.RESIDUAL, T.EXHAUST, T.CONFUSE — and nothing has systematically tried to
realise any of them.

*The last EAL5 gap that is engineering rather than paperwork.* The 30 fuzz
targets and `syscall/check.rs`'s 9,537 lines of refusal tests are raw material;
what is missing is an analysis structured by threat with a documented verdict
per attack path.

### 4.2 Side-channel defences and layout randomisation — **F-31**
**Done 2026-09-26**, both halves, by [SPECULATION.md](SPECULATION.md), behind
the one build switch `--mitigations on|off`. F-31 is closed; what the defences
do not reach is V-06.

To re-audit: boot `x86_64 --accel kvm` and read the two `cpu      speculation`
lines — under KVM the processor's controls are real, under TCG there are none.
For KASLR, run `cargo xtask test-kaslr --arch all`, which boots each image
twice and prints both layouts, and read the `kaslr` lines of any boot: the
loader's say where and from what, the kernel's how many bits.
Re-measure the cost with both settings under KVM before quoting it; the
figures in SPECULATION.md §8 are medians of eight boots on a loaded host.
Check that `cargo xtask check` still has its `--mitigations off` clippy steps,
and that AoU-11's list matches what `arch/*/speculation.rs` actually applies.

### 4.3 The direct map's alias of the kernel's text — **F-34**
**Done 2026-09-26.** Found by the KASLR work, and closed the same day.

To re-audit: read the `sealed` line of any boot, beside the `w^x` line. It
counts every mapping of the frames holding the image's text and read-only
data. That is more than the image's own pages, because the direct map's alias
is among them, and the line says none is writable. Then reproduce the negative
control the commit quotes: a scratch write of one byte of text through
`mm::direct_map` must fault with FX-9001 on each architecture. And remove
`mm::overlaps_image`'s refusal from `vmap::map_device` (scratch): stage 2's
check must stop at *"a device window over the kernel image was mapped"*.

### 4.4 Allocation failure — **F-23**
**Done 2026-09-26** (IMPLEMENTATION.md W-12). F-23 is closed for the item's
own allocations. What it does not cover is AoU-5 and V-05.

To re-audit: run `python3 tools/common/check/check-fallible-alloc.py --report`. It must
say 0 unmarked in the item, and no more than the baseline's 14 in the load
files it reaches (`process.rs`), and list the `FATAL-ALLOC` sites, which must
all be bring-up. Check that `REACHED` still names what `process_create` and
`process_start` run (MEMORY-AND-TIMING.md §1.3), and read stage 7's
`sigpaths` line, which ends with the signal tables refused with no memory.
Read a sample of the `NOALLOC` sites against the reservation each one cites.
Re-audit the item's `.clone()` calls, which the gate cannot see
(MEMORY-AND-TIMING.md §1.3 lists the kinds). Then read the `no-mem` line of a
boot on each architecture. As a negative control, replace one `fallible::`
call on a native call's path with the standard one (scratch): the gate must
fail on it. The boot check carries its own negative control: with the reserve
refused its filling, a section must fail before it starts.

### 4.4a Page tables and the shootdown — **F-36**
**Done 2026-09-26** (IMPLEMENTATION.md W-14). Found by the memory coverage
work and closed the same day.

To re-audit: every `mm::unmap_in` caller passes the `TlbPages` it then
flushes, every `unmap_io` caller releases its list only after the unit's
flush succeeded, and every `mm::unmap_unwalked` caller is a tree nothing can
walk (grep all three). Stage 4 must pass on every architecture at `--smp 2`,
and the scratch negative control in W-14 must still stop it.

---

### 4.5 Job quotas — **F-35**
**Done 2026-09-26** (IMPLEMENTATION.md W-13). F-35 is closed, with
`FRU_RSA.1` refined to what the quotas bound; what they left out was F-37,
closed the same day (§4.6).

To re-audit: read the `quota` line of a boot on each architecture. It must
say a fork loop was refused at its job's 8 tasks, faults at the job's 48
pages less what its space's regions hold of the heap -- 47 on x86-64, some
of them page tables -- while a sibling faulted in 48, objects at 5, one task alone in its job kept about
half a processor against eight in another, and every counter and slot came
back. Read the `cgroups` line, which must name the controllers enabled and a
fork refused at `pids.max`, and `test-vfs` command 19. Then grep for
`mm::allocate_frames(0)` in `user/`, `fs/pages.rs` and the fault path: a
frame of a program's memory taken that way is charged to nobody, and each
must be `allocate_user_frame`. As negative controls (scratch), each of which
must stop the boot by its check's own message: let `quota::charge` ignore the
limit (the `cgroups` check: *"forks went past pids.max"*); drop the
uncharge from `mm::release_frame` (*"address spaces gone and their frames
still charged"*); have `Task::effective_weight` answer the base weight (*"a
job with many spinning tasks took more than its share from another job"*,
with one task at 111 per mille); and have `Process::new` charge nothing (the
`cgroups` check again).

### 4.6 The Linux personality's heap per job — **F-37**
**Done 2026-09-26** (IMPLEMENTATION.md W-15). F-37 is closed; T.EXHAUST is
re-judged resisted and V-05 low.

To re-audit: read the `kmem` line of a boot on each architecture. It must say
a job at a 32 KiB limit made some of each kind -- files, pipes, socket
pairs, descriptors in flight, epoll registrations, eventfds, regions of one
mapping, record locks -- was refused one more of each, a sibling made one,
and every byte came back. Read `test-vfs` command 21, and `memory.stat` in
any cgroup. Then look for heap the load keeps past a call that carries no
`ferrix_kmem::Charge`: grep the load ring and its libraries for `Arc::new`,
`Box::new`, `push`, `push_back`, `insert` and `extend` on a structure that
outlives the call, and check each is inside something charged, or is one of
the argued kinds FINDINGS.md F-37 lists. As negative controls (scratch), each
of which must stop the boot: drop the uncharge from `Charge`'s `Drop` (the
`quota` check: *"address spaces gone and the heap of their regions still
charged"*); let `quota::charge_kernel` pass the limit (*"kmem: a job made
more than its limit could hold"*, on files); have the kernel's account
release no slot (*"the checks' jobs are gone and their quota slots are
not"*); and have `Pipe::new` forget its charge (*"kmem: objects gone and
their heap still charged to their job"*, on pipes).

### 4.7 btrfs in the item
**Decided 2026-10-02** (the customer): the btrfs reader and write path,
`ferrix-btrfs` and `ferrix-btrfs-write`, are in the item; `ferrix-btrfs-vfs`
and `src/kernel/src/fs/btrfs*.rs` stay load. Written so far: `H.STORE.1` to
`H.STORE.8` (part 13), `L.btrfs.1` to `L.btrfs.107` (part 24), ASR-9, FM-11,
AoU-15 and AoU-16 (SAFETY-MANUAL.md), O.MEDIA, T.MEDIA, A.STORAGE,
FDP_SDI.2 and §9.8 (SECURITY-TARGET.md), V-08 and V-09
(VULNERABILITY-ANALYSIS.md), and MEMORY-AND-TIMING.md §1.8 and §2.2d. What
is left, each with how to know it is done:

1. **Done 2026-10-02: fallible allocation in the write path** (`H.STORE.7`,
   `L.btrfs.22`, `L.btrfs.108` to `L.btrfs.116`, `L.mm.63`, `L.mm.64`; branch
   `btrfs-fallible`). Both crates count 0 sites, held there by a `cargo xtask
   check` step that names `L.btrfs.22` (`item_crates_allocate_fallibly`);
   the host tests fail every allocation in turn and name `H.STORE.7` and
   `L.btrfs.108` to `L.btrfs.116`; and both ids left the baseline. The plan
   below said a host test would name `L.btrfs.22`: a gate does, since the
   property is one of the source, not of a run. It was done when
   `check-fallible-alloc.py` reads both crates and finds no unmarked site,
   a host test fails every allocation of a create, a data write, a commit
   and an open in turn and names `L.btrfs.22` and `H.STORE.7` in its
   `Verifies:` line, and both leave `traceability-baseline.json`.
2. **The item-scoped gates read the crates** (the branch
   `btrfs-cert-boundary`): the boundary, unsafe, panic, complexity and
   fallible-allocation gates. `check-traceability.py` resolves a
   `ferrix_btrfs::` unit through that branch's `item_crate_product_files`
   (branch `btrfs-cert-docs`), so part 24's units resolve only once both
   are on `main`. Done when `cargo xtask check` passes there and
   TRACEABILITY.md lists the crates' functions among the item's. Then
   `coverage-report.py` and `decision-coverage.py`, still kernel-only,
   are item 3's.
3. **A coverage floor for host crates.** `coverage-floor.json` and
   VERIFICATION.md §3 measure the kernel's boots; the two crates are
   exercised by host tests, which no coverage run measures. Done when the
   crates' statement and decision coverage under their host tests is
   measured, committed and ratcheted, and what is not covered is justified
   as the kernel's residual is.
4. **A write-path fuzzer** (V-09). Done when a fuzz target opens bent and
   resealed images with `WriteVolume::open`, replays their logs, edits and
   commits them, and requires no panic and no hang, and the reopened volume
   passes the writer's checker or is refused; and CI runs it as it runs
   `btrfs_read`.
5. **The reserves derived, and mount on a full volume** (`docs/BACKLOG.md`,
   "btrfs on a full volume, what the certification consultant left open",
   (a) to (c)). Done when `commit_reserve` and `Need::data` are derived
   rather than estimated and a full-size transaction is shown to commit on
   full trees; a reload whose own log replay aborts is tested; and log
   replay and orphan cleanup at mount are measured against a full volume.
6. **The two refusals no test opens** (`L.btrfs.9`, `L.btrfs.12`, in the
   baseline): images with each feature the reader or the writer refuses,
   opened, and the refusal named.
7. **Miri over `src/lib/fs/btrfs`** in CI (`docs/BACKLOG.md`), with the
   whole-image tests ignored under it.
8. **Arithmetic that cannot overflow** (`L.btrfs.23`, the certification
   consultant's advice). The release kernel builds without overflow checks,
   so an unchecked sum of two values read from a volume wraps silently, and
   `dev` and `iterate`, which keep them, panic on it. Done when
   clippy's `arithmetic_side_effects` is denied for both crates' product
   code with no site left -- 112 on 2026-10-02, 56 in each -- or each
   remaining one argued at the site, and `L.btrfs.23` leaves the baseline
   on that gate.
9. **The counts.** README.md, CLAIM.md §3.1, SECURITY-TARGET.md §1.2 and
   SAFETY-MANUAL.md §1 carry ITEM.md §2's re-measure of 2026-10-02 (79,079
   lines: 64,788 kernel, 14,291 crates). SECURITY-TARGET.md §2.2 and §8.3
   and VERIFICATION.md §4 still argue from older totals; re-measure and
   reconcile them at the next pass.

## 5. Tools

### 5.1 Evaluate Ferrocene — **F-17**
Establish which `rustc` versions it ships, and whether the qualified target
list covers `armv7a-none-eabi` and the three UEFI targets. Expect those four to
fall outside it; if they do, the reference configuration has to say so.

This is a procurement question as much as a technical one. Answer it before
planning anything that depends on a qualified toolchain.

### 5.2 Tool operational requirements for the two generators in scope — **F-18**
**Written 2026-09-25** as TOR-1 and TOR-2 in TOOLS.md §6. F-18 stays open:
generator and `--check` share code, so the verification is not independent.

Only `gen-panic-catalog.py` and `gen-font.py` reach the item; the other four
generate compositor code outside the boundary. Both already have `--check`
modes that re-derive output from input on every build, which is the
output-verification route DO-330 permits. Writing it up is a page, not a
project. See [TOOLS.md](TOOLS.md) §5.

---

## 6. Cannot be closed from this repository

Do not write these. A hazard analysis without an application, or a planning set
without an organisation, produces documents an assessor rejects and makes this
directory look more finished than it is.

| Finding | Needs |
|---|---|
| F-20 | An application. Hazards belong to a device or a train, not a kernel. |
| F-22 | The same, plus EN 50126/50129 system context. |
| F-27 | People who do not work on the code. |
| F-28 | An organisation, for ISO 13485 and the DO-178C planning set. |
| F-30 | Years of field history. |

### 6.1 One question to raise externally — **F-29**
No certification scheme has settled how to treat AI-authored code in a
certified item, and every commit in this repository has that provenance. Raise
it with a certification body early rather than discovering it at assessment. It
may constrain which of the four targets is worth pursuing at all.

---

## 7. Standing rules

* **Verify gates by their output, not a pipeline's exit status.**
  `gate | tail && commit` commits on failure. The audit made this exact mistake
  once and caught it only because the result looked wrong.
* **A finding closes when the build says so**, not when a document says so.
* **Re-measure before citing.** Numbers here were reconciled on 2026-09-26.
* **`docs/CONVENTIONS.md` governs commits:** one author, no `Co-authored-by:`
  trailer, no tool signature.
* **Keep the debt register honest.** `known_violations` may shrink without
  ceremony and may not grow without a diff somebody argues for.
