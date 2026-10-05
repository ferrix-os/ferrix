# Verification evidence

What has been verified about the item in [ITEM.md](ITEM.md), by what, and what
the result does and does not support.

The short version: there is a great deal of verification, and almost none of it
is *traced*. That gap is finding F-14 and it is the difference between evidence
of something and evidence for something.

---

## 1. The reachability problem, and what changed

`docs/sysml/11-assurance.sysml` states the constraint that shapes everything
here. `src/lib/` is reachable by `cargo test`, Miri and the fuzzers because it is
architecture-neutral logic over bytes. `src/kernel/` is reachable **only by booting
it** — Miri cannot interpret a privileged instruction and a fuzzer cannot drive
a page-fault handler. That is the whole argument for `src/lib/` existing.

The consequence was that the item — which is entirely `src/kernel/` — had no
structural coverage measurement at all. As of 2026-09-25 it does, via QEMU's
`drcov` TCG plugin and the kernel's own DWARF line table. See §3.

---

## 2. What exercises the item

| Layer | Mechanism | Scale |
|---|---|---|
| In-kernel self-tests | `check.rs` / `*_check.rs`, run on every boot | **31,135 lines**, 29 files |
| Boot gates | `cargo xtask test-*` under QEMU | 18 commands, 3 architectures |
| Host unit tests | `cargo test` over `src/lib/` | ~1,950 plus doc tests (2026-09-23), `xtask` 242 |
| UB detection | `cargo miri test` | 13 crates |
| Fuzzing | `cargo fuzz`, corpora committed | 30 targets |
| Supply chain | `cargo deny check` | empty ignore list |
| Static analysis | `clippy` at ten configurations | denies `unwrap`, `expect`, `panic`, indexing, slicing |

**The in-kernel self-tests are the item's primary evidence**, and they are
unusual enough to be worth describing. Roughly a quarter of the kernel is test
code that runs inside ring 0 on every boot and prints what it proved rather
than that it passed:

```
w^x       3485 mappings swept, 917 executable, none writable
sealed    4416 KiB of text and read-only data, 1697 mappings of it, none writable
handles   1 device aperture mapped into a process and reached from a forked
          child, 1 interrupt held from delivery to acknowledgement, 2 VMO
          pages pinned for a device and found at their device addresses,
          18 refusals as specified
wake      16 of 16 interrupt deliveries ended their wait by waking it,
          the slowest returning after 518 us
reclaim   84 MiB from the loader and ACPI, 434 free; arena 34 live, 708 KiB
```

Counted quantities, not assertions of success. For the Security Target's
objectives this is directly usable: the `w^x` and `sealed` lines are O.WXN
demonstrated on every run, the second over the direct map's alias of the text
that the first cannot see (F-34), the `handles` line is O.CAPABILITY's refusals exercised,
and the `quota` line, since 2026-09-26, is O.QUOTA's: each job limit refused at
exactly its value while a sibling job goes on (F-35).

The largest bodies of in-kernel test code sit against the item: `syscall/
check.rs` at 9,537 lines, `object/check.rs` at 3,318, `user/check.rs` at 1,263,
`sched/check.rs` at 1,201.

**The btrfs crates are the exception** (in the item since 2026-10-02). Their
evidence is host tests, not boots: 8,278 lines of tests over images
`mkfs.btrfs` made and over a device in memory that records every write and
flush, so that a power cut at any point can be rebuilt and the volume opened
again, and the writer's own checker, which recomputes every reference,
usage and free-space count from the trees. `L.btrfs.1` to `L.btrfs.107` name
them (TRACEABILITY.md). Stage 11's and 12's boot checks
(`fs/btrfs_check.rs`, `fs/btrfs_write_check.rs`,
`fs/btrfs_powerfail.rs`) run the crates under
the kernel too, but are load-ring checks of the glue and name no item
requirement. §3's coverage is of the kernel's boots and does not measure
these crates; a floor for them is TODO.md §4.7.

---

## 3. Structural coverage

Measured 2026-09-27 on all three architectures with the same tool, over the
checks F-10's module-by-module passes wrote (FINDINGS.md F-10), and **not
comparable with the figures published on 2026-09-25**, which two defects in
the measurement made wrong in opposite directions, nor with the 2026-09-26
figures, which two more defects inflated and deflated (§3.4). Raw per-file
data in `coverage-*.json`; the ratchet in `coverage-floor.json`. Decision
coverage, from the same traces, is §3.6 and `decision-coverage-*.json`.

### 3.1 The suite, every architecture

The union of every boot gate that exercises the item and passes under the
plugin — `cargo xtask coverage` runs them. Twelve gates on x86-64:
`test-boot`, `test-shell`, `test-vfs`, `test-net`, `test-threads`, `test-pty`,
`test-btrfs`, `test-powerfail`, `test-display`, `test-input`, `test-jobs`,
`test-restart` and `test-sysfs`, and four more boots of `test-boot` on
machines or command lines the rest do not present: `boot-legacy`, a q35 with
no HPET and a processor with `RDRAND` and no `RDSEED`
(`FERRIX_X86_MACHINE=hpet=off`, `FERRIX_X86_CPU`), whose clock is the TSC
measured against the PIT; `boot-reset`, which ends in the firmware's reset
rather than a power-off; `boot-single`, one processor; and `boot-options`,
the options no other gate gives -- the boot console on the framebuffer, a pid
1 the image does not have, an `ferrix.onexit` the kernel does not know, and
`nokaslr`. Fifteen boots on AArch64 and fourteen on ARMv7-A (`--smp 2`): the
x86-64-only three are `test-jobs`, `test-restart` and `test-sysfs`, which carry
uutils. `test-vfs` joined the Arm pair's suite on 2026-09-27, once its
permissions row judged the words of whichever `cat` the image carries. AArch64 boots `test-boot` again on a `virt` with a GICv3 and its ITS
(`FERRIX_ARM_MACHINE=gic-version=3`), from its device tree with no ACPI, once
with the GICv2 and once as the Pixel 7 is (a GICv3 and ITS on a `max`
processor), with `nosmp`, into a reset, and with the options above. ARMv7-A
boots it again on a Cortex-A15, the one other core `virt` takes and one the
Spectre defences apply to, into a reset, on one processor, with 3 GiB so that
firmware loads the kernel above the split, as the DK1's memory always is, and
with the options. Debug profile, with KASLR. Every Arm boot goes through its
firmware as a board's does -- EDK2 on AArch64, U-Boot on ARMv7-A -- with its
two waits skipped since 2026-09-27: EDK2's boot menu, by a `Timeout` of 0 in
the fresh variable store xtask writes (`tools/common/xtask/src/uefi_vars.rs`), and U-Boot's
autoboot countdown, by an environment in flash that is U-Boot's own
compiled-in default with `bootdelay=0` and nothing else changed
(`tools/common/xtask/src/uboot_env.rs`). x86-64 measured again on 2026-09-27 at e5f3110f
(11532464); AArch64 and ARMv7-A measured again
on 2026-09-27 at 9e196852, with `test-vfs` in their suite and the trap
check's read past a mapped file's end:

| Ring | x86-64 | AArch64 | ARMv7-A |
|---|---:|---:|---:|
| `core` | 5,253 / 5,787 — 90.8% | 5,293 / 5,779 — 91.6% | 4,742 / 5,585 — 84.9% |
| `item` | 1,649 / 1,877 — 87.9% | 1,531 / 1,808 — 84.7% | 1,509 / 1,815 — 83.1% |
| **Certified item** | **6,902 / 7,664 — 90.1%** | **6,824 / 7,587 — 89.9%** | **6,251 / 7,400 — 84.5%** |
| `load` (not claimed) | 9,598 / 13,745 — 69.8% | 9,377 / 13,718 — 68.4% | 9,463 / 13,775 — 68.7% |

The Arm pair's figures are a little below their previous measurement (90.2%
and 84.8%) although the suite reached more: in between, init's start of
`devmgr` (L12, `docs/INIT.md` §7.3) added statements to `devmgr.rs` that only
a boot under `ferrix.devmgr=init` takes, and the suite's boots keep the
kernel's start, the reference configuration. They are 78 of AArch64's and 82
of ARMv7-A's statements that need a test.

ARMv7-A trails because more of its residual is hardware and configuration it
does not have: 442 of its 1,149 unreached statements, against 246 of 766 on
x86-64 -- no IOMMU unit programmed, no framebuffer, no SMMU -- and the
quarantine and translated paths that only a translating domain takes are
another architecture's there (§3.1.1).

**Every gate now counts.** `test-btrfs`, `test-shell`, `test-sysfs`,
`test-restart` and the rest used to pass under the plugin and write an empty
trace: the plugin writes its table when QEMU exits, and those gates ended by
killing it. xtask now asks QEMU to stop (SIGTERM, which QEMU treats as a host
shutdown and exits from normally) before it kills it, and numbers the trace of
every boot after a gate's first, since the plugin truncates its file at each
start and `test-shell` boots four times. The one boot that still leaves
nothing is `test-powerfail`'s churn, whose point is that QEMU is killed with no
chance to finish anything; its replay boots count.

**Most of it is the boot.** One `test-boot` reaches 86.4% of the item on
x86-64; the other sixteen boots add 3.1 points between them. The self-checks
that run on every boot (§2) are the item's real test suite, and the gates
mostly exercise the uncertified load ring above it — 71.1% of `load` against
one boot's 60.5%.

### 3.1.1 The residual

`coverage-residual-<arch>.json` lists every statement in the item the suite did
not reach, by file and line, on each architecture. DO-178C wants each one
either driven by a new requirements-based test or justified as unreachable
defensive code, and neither conversation can start from a percentage.
[COVERAGE-RESIDUAL.md](COVERAGE-RESIDUAL.md) sorts them:

| | x86-64 | AArch64 | ARMv7-A |
|---|---:|---:|---:|
| Unreached | 766 | 763 | 1,149 |
| Argued: another architecture or board | 184 | 156 | 248 |
| Argued: reached only when stopping | 177 | 154 | 209 |
| Argued: reached only when something has failed | 73 | 81 | 68 |
| Argued: run, and credited to another line | 11 | 10 | 19 |
| Hardware the machine does not present | 246 | 217 | 442 |
| **Needs a test** | **75** | **145** | **163** |

[COVERAGE-WORKLIST.md](COVERAGE-WORKLIST.md) groups the last row by module,
with each file's count on every architecture and the lines no architecture
reaches, so that a module can be taken as one piece of work. The largest:
`devmgr.rs` 78 and 82 on the Arm pair (init's start of `devmgr`, above);
`syscall/native.rs` 24, 7 and 9; `object/` 17 on each; `init.rs` 14 on the
Arm pair. `trap.rs`, `user/` and `arch/aarch64` left the list on 2026-09-27:
a test for what a test could reach, and an argument, statement by statement,
for the 16550 and `ramoops` consoles only the crosvm guest and the Pixel 7
have.

**Argued one statement at a time.** A file-level category cannot argue a line
of an otherwise covered file, and most of what F-10 leaves is exactly that.
`coverage-argued-<arch>.json` holds an argument per statement or small range:
the category, why no run of the measured machine reaches it -- for a
defensive path, what would have to go wrong -- and text the first line must
contain, so it cannot slide onto another statement when the file changes.
The generator fails on an argument for a line that is no longer in the
residual, as the boundary gate does on a stale debt entry. Two categories
join the file-level ones: *reached only when something has already failed*,
and *run, and credited to another line*, for a statement a test executes
whose only row in the line table sits in an inlined copy that cannot run it.

**x86-64's architecture code, `trap` and `smp` have nothing left that needs
a test.** Of `arch/x86_64`'s 637 statements 593 are reached and 44 argued:
22 on hardware the TCG machine lacks and gate 6's KVM boot has (the invariant
TSC, the speculation controls), 15 on the stopping path, 6 defensive (the
reset's fallbacks, a stopped HPET, a trampoline past a page) and one credited
to another line. Stage 3 on x86-64 now runs six programs that end by
their own exceptions -- a divide error, `ud2`, an unmapped read, `hlt` in
ring 3, an x87 zero divide, a read past a mapped file's end -- and a seventh
that asks `arch_prctl` both of its refusals; stage 4 checks shootdown page sets merged and
past their ceiling. `trap.rs`'s 22 left are the stopping path and three
invariants; `smp.rs`'s 6 are `debug_assert!` and `fatal!` messages.

**Then every module, on all three architectures** (2026-09-27). The memory
layer, the core objects, the Arm architectures' code and the kernel's
services were taken the same way, one pass each, and measured together
(FINDINGS.md F-10). What their checks leave is argued in 275, 264 and 329
arguments over 368, 320 and 485 statements; the rest of the argued rows are
the file-level categories. The per-statement arguments were written against
each pass's own tree and carried to the measured one by a diff of each file,
narrowed to the lines still unreached; the generator's check holds every one
to its line.

The failure path is mostly covered now, and by a passing test: `test-shell`'s
last boot asks for `ferrix.onexit=panic` and gets FX-1501, so the panic report
runs. What is left of it is argued, with each `fatal!` arm that follows a
self-check: a passing boot is one that never takes it.

### 3.2 One boot, and both profiles

One `test-boot` each, for the question F-11 and F-12 asked — whether each
configuration can be measured at all — and not for comparison with §3.1.

| Configuration | Certified item | Core | Statements |
|---|---:|---:|---:|
| x86-64, debug, one boot | 71.6% | 69.3% | 6,828 |
| x86-64, **release**, one boot | **75.2%** | 74.3% | 4,462 |
| AArch64, debug, one boot | 68.6% | 65.6% | 7,041 |
| ARMv7-A, debug, one boot | 69.0% | 66.3% | 6,908 |

**The profile moves the denominator more than the percentage.** Release
optimisation cuts the item's statement count by a third — 6,828 to 4,462 —
because inlining and merging leave fewer distinct `is_stmt` rows to reach. The
proportion reached moves 3.6 points. So a submission has to say which profile
it measured, and this one measures both, which is what F-11 asked for.

**The three architectures agree**, within four points, on one boot and on the
suite. The table published on 2026-09-25 had ARMv7-A at 70.8% against 46.6%
and 46.1% for the 64-bit pair, and explained the gap by x86-64's larger
arch-specific share. That explanation was wrong: the gap was a defect in the
tool that only 64-bit addresses met (§3.4).

### 3.3 Method

QEMU's `drcov` TCG plugin records every basic block the guest translates and
executes. The kernel's DWARF line table says which source statement each
address belongs to; the denominator is the rows the compiler marked `is_stmt`,
which is what gcov-shaped tools count. `tools/common/gen/coverage-report.py` intersects
the two and attributes each file to a ring using the same classifier the
boundary gate uses, so the two cannot disagree.

Gates build different kernels — `test-shell` builds its program in, `test-vfs`
and `test-net` their command lists — so each boot's trace is kept with the ELF
it ran (`<trace>.kernel` names it), each trace is read against its own ELF,
less the slide KASLR gave that boot (`<trace>.slide`), and the union is of
*statements*, a file and a line. The denominator is the
plain `test-boot` build's.

To reproduce, with QEMU's `contrib/plugins/libdrcov.so` built:

```
FERRIX_DRCOV=/path/to/qemu/build/contrib/plugins/libdrcov.so \
  cargo xtask coverage --arch x86_64 \
    --init "$HOME/.local/share/ferrix/busybox/{arch}/bin/busybox.static"
```

That runs the suite with `--accel tcg` (a TCG plugin observes nothing under
KVM, and the launcher refuses rather than reporting zero), writes the traces to
`build/coverage/<arch>`, prints the `coverage-report.py` command it runs, and
fails below the architecture's floor in `coverage-floor.json`; then it prints
and runs `decision-coverage.py` over the same traces, which has no floor yet
(§3.6), and `--json` regenerates its evidence the same way. It needs boots,
so it is not part of `cargo xtask check`. Adding `--json` and `--residual` to
the printed command regenerates the evidence, and
`tools/common/gen/gen-coverage-justification.py` the two documents from it.

**Between measurements.** The evidence names statements by file and line, so a
change that moves a line in the item leaves an anchor on the wrong statement,
and `--check` fails rather than let an argument drift. A landing that changes
the item carries the anchors with `tools/common/gen/carry-coverage.py`, which maps
each line through a diff from the tree the evidence was written on: a line the
change left alone keeps its place and its argument at its new number, and a
line it edited or removed is dropped and printed -- it is unmeasured now, and
an argument for the old text is no evidence for the new. The figures stay as
measured until the next run of the suite, which takes in the new code and
holds it to the floor.

**A carry nobody made fails the gate (F-62).** The anchors include the
checks' own reached lines (each `coverage-<arch>.json`'s `verification`
map, which `TRACEABILITY.md` reads), and those sit mostly in load-ring check
files. Two landings that touched no item file, devtty (5dddc1981) and
console-revoke (0f94a6d1a), moved `syscall/check.rs` by 200 lines and
carried nothing; a later carry from a tree after them kept the stale lines
as if they were right, and `TRACEABILITY.md` read four verified
requirements as not reached and one unverified as reached until
2026-10-05. Two things hold it now. `carry-coverage.py` carries each
evidence file from the commit that last wrote that file, by default, and
refuses a `--from` whose kernel is not the one each was written on. And
`carry-coverage.py --check`, which `gen-coverage-justification.py --check`
runs in every `check` and `check-docs`, carries each file in memory from
its own commit to the tree and fails if anything would move: the kernel
changed since the file was written, and nobody carried it. An edit of a
`coverage-<arch>.json` by hand still resets that file's base; the
`verification` maps from before 0498203ca are proved only by the next
coverage run.

**A line can stop being a statement.** The denominator is the image's
`is_stmt` rows, so a line whose function the compiler starts inlining, or
drops, can lose every row: it leaves both counts, neither reached nor
unreached. `gen-coverage-justification.py` then says a line an argument names
is "not in the residual -- reached now, or moved", and cannot yet tell that
case from being reached. On 11532464, x86-64's re-measure, `trap.rs` 545 and
`arch/x86_64/trap.rs` 631 and 662 left the image this way -- `report_trap`
inlined into `report`, inside `fatal` -- and were not reached; that commit's
message says they read reached, which is wrong. Its other trims were reached,
each found in the traces by address: `arch/x86_64/trap.rs` 615 in
`user_fault`, `iommu.rs` 724 in `Domain::pin`, and `console/output.rs` 251,
276 and 285-286 in `test-restart`, where a writer waited for room in the
transmit ring and so refuted the argument that an emulated port never fills
it.

**What this supports.** DO-178C table A-7 objective 5 at DAL C asks for
statement coverage. This is that measurement, for ring-0 code, on every
architecture and both profiles in the reference configuration, without
modifying the toolchain.

**What it does not.** 90.1%, 89.9% and 84.5% are not 100%. The residual is
enumerated and sorted, and 94 of x86-64's 725 statements, 144 of AArch64's 725
and 163 of ARMv7-A's 1,110 still need a test rather than an argument (F-10). Decision
coverage, which DAL C does not require and DAL B and A do, is measured of
object code in §3.6 and is far short; there is no MC/DC (F-13).

Two biases, both optimistic and both declared in the tool's own docstring: a
basic block credits every statement inside it even if a trap left it early, and
optimised builds map one address to several source lines.

### 3.4 Two defects, and what they did to the published figures

Found 2026-09-26 by comparing the three architectures' per-file results, which
disagreed on generic code that every boot runs — `syscall/mod.rs`'s dispatch
arms for `brk`, `munmap` and `wait4` read unreached on x86-64 and reached on
ARMv7-A — and then reading the raw blocks against `objdump`.

1. **A sentinel below the higher half.** The lookup that asks whether an
   address lies in an executed block searched with `(address, 1 << 62)`,
   meant to sort after every block starting at that address. Every block end
   in a kernel linked at `0xffffffff80000000` is above `1 << 62`, so a block
   starting *exactly* on a statement was never found. On x86-64 and AArch64
   that under-reported by about a third: one boot of the 2026-09-25 tree read
   46.6% and was 73.4%. ARMv7-A's 32-bit addresses never met it.
2. **Every trace read against one ELF.** The union read all the gates' traces
   against whichever kernel was built last, and the gates build different
   kernels whose code sits at different addresses. On the 2026-09-25 tree,
   `test-vfs`'s trace read against the `test-boot` build gives 66.4% of the
   item; against its own, 73.6%. Misattributed blocks land on statements
   nobody ran, so this over-reported, and more the more gates were in the
   union.

The published 81.9% had both, one pulling each way, and neither this tool nor
anyone could have told it from a correct figure. Both are fixed in the tool,
and TOOLS.md TOR-3 records them beside the three found before.

Two more, found the same day by walking the residual line by line, both in
how the line table is read:

3. **Rows of discarded functions.** At `opt-level = 1` a small function is
   inlined into every caller and its out-of-line copy dropped by the linker,
   whose rows stay behind at a few bytes above zero. A line whose only row
   was one of those -- the closing brace of `cpu.rs`'s `outb` -- counted as a
   statement nobody reached: about a quarter of every residual.
4. **Rows under the wrong file.** After a line-table sequence ends objdump
   prints no header for the next one, so its rows were filed under whichever
   file came last: `check.rs`, `drm.rs` and `signal.rs` statements read as
   `syscall/uaccess.rs`'s, and a line 1392 appeared in the 431 lines of
   `gdt.rs`. This one over-reported as well as under-reported.

Both are fixed, and x86-64's column above is measured with the fix; TOOLS.md
TOR-3 records them.

### 3.5 What the suite leaves out

`test-seat` and
`test-compositor` pass under TCG but not under the plugin, which slows TCG
enough that the first misses its redraw and the second trips the TLB
shootdown's bound (`processor 0 never flushed its TLB for a shootdown`). A
failing run is not coverage evidence, so neither counts. `test-foot`,
`test-video`, `test-vkgears`, `test-rustc`, `test-chrome` and `test-selfhost`
need a GL host, ports or fetched volumes.

### 3.6 Decision coverage

DO-178C table A-7 objective 6 asks, at DAL B and above, that every decision
has taken every outcome. DAL C does not ask it, and F-13 stays Informational;
this section measures how far the item is from it, from the traces §3.1
already collects, with no new instrumentation.

**Method.** `tools/common/gen/decision-coverage.py` disassembles each kernel build
with the pinned toolchain's `llvm-objdump` (the `llvm-tools` component
`rust-toolchain.toml` installs, one tool for all three architectures) and
finds every direct conditional branch: `jcc` on x86-64; `b.<cond>`,
`cbz`/`cbnz` and `tbz`/`tbnz` on AArch64; `b<cond>` on ARMv7-A, whose kernel
is A32 with no Thumb. QEMU ends a translation block at every conditional
branch, and control that leaves one starts the next block at the branch's
target if it was taken and at the instruction after it if not; drcov records
the address every executed block starts at. So a branch has taken **both
ways** when blocks began at both successors. Each branch is attributed to a
file and line by the same DWARF line table the statement figure uses (every
row, not only `is_stmt` ones) and to a ring by the boundary gate's
classifier; traces are read against their own build and slide, and the builds
are joined by naming each branch by its function, file, line and order at
that line, onto the `test-boot` build, which is the denominator. `cargo xtask
coverage` runs it after the statement report.

It reports three things, because the one number the method can give exactly
is not the one the standard means:

* **Both ways**, an upper bound. A block can begin at a successor for a
  reason other than this branch: the successor is also another branch's
  fall-through, a jump's target or a call's return.
* **Sure**, a lower bound: both outcomes seen, and for each successor every
  *other* instruction with a known edge into it lies in no executed block,
  so that this branch's edge is the only one that can have been taken. drcov
  records blocks and not the edges between them, and nothing in its traces
  narrows the gap between the two; QEMU's `cflow` plugin, built beside
  `libdrcov.so`, records edges and would.
* **By source line**: a decision is a (file, line, order at that line), and
  is covered when *any* compiled copy of it took both ways -- nearer the
  source-level decision the standard means than the object-code count, which
  asks it of every copy.

A **guard** is a branch one of whose successors runs, with no other decision
or call first, into a panic: an overflow or bounds check, an `unwrap`, a
`debug_assert!`, one of `core`'s debug-build precondition checks, a
`fatal!`. A passing run never takes its panicking way -- none did on any
architecture but the one `test-shell` asks for with `ferrix.onexit=panic` on
x86-64 -- so a passing suite can never take it both ways. The guards are
left out of the second and third rows below, as the statement residual's
"reached only when something has already failed" is argued rather than
tested (§3.1.1).

Measured 2026-09-27 on main at 88d9ce39, debug profile, the §3.1 suite on
each architecture (30, 28 and 27 traces, every gate passing; the same traces
give the statement figures 89.4%, 90.5% and 84.8% on this tree):

| Certified item | x86-64 | AArch64 | ARMv7-A |
|---|---:|---:|---:|
| Conditional branches | 12,477 | 11,006 | 13,153 |
| Both ways | 3,966 — 31.8% | 2,994 — 27.2% | 3,267 — 24.8% |
| Sure (lower bound) | 10.4% | 8.9% | 9.8% |
| Guards | 1,906 | 1,814 | 1,775 |
| **Both ways, guards left out** | **3,965 / 10,571 — 37.5%** | **2,994 / 9,192 — 32.6%** | **3,267 / 11,378 — 28.7%** |
| **By source line, guards left out** | **1,931 / 3,850 — 50.2%** | **2,000 / 4,111 — 48.6%** | **1,694 / 3,471 — 48.8%** |
| Never reached | 2,807 | 2,324 | 3,413 |
| Outcomes seen, of two per branch | 54.6% | 53.0% | 49.4% |

Per ring and per file in `decision-coverage-<arch>.json`; every short branch,
with its function and outcome, from `--branches`, which is too long to
commit. ARMv7-A has 44 more conditional calls and returns (`bl<cond>`,
`pop<cond> {..., pc}`) whose outcomes block starts cannot tell apart; they
are counted apart and never as covered.

**Where the gap is.** The files with the most non-guard branches short of both
ways are the same on the three: `timer.rs` (768, 702 and 3,362), whose
`now_nanos` is inlined into more than 400 functions and every copy carries its
`hz == 0` test, which only a read before the counter is calibrated takes --
and on ARMv7-A its 128-bit arithmetic besides; `sched/wait.rs` (738, 635,
562), whose waits are generic over their condition and compiled once per
caller, each copy with its own deadline test; `object/quota.rs` (575, 553,
435), almost all of it the `?`s of the one-line slot lookup at line 211,
inlined everywhere a quota is charged; `object/mod.rs` (387, 364, 319); and
on x86-64 `arch/x86_64/mod.rs` (692), the `if state != 0` of the interrupt
restore every lock guard inlines, and on the Arm pair `syscall/uaccess.rs`
(561, 556), the `?` of each inlined `copy_to_user`/`copy_from_user`. That
is the method's pessimism at work: most of the gap is a handful of source
decisions multiplied by inlining, which is why the source-line view reads
half as far from 100%.

**Limits, all declared in the tool.**

* *Object code, not source decisions, and not MC/DC.* A source decision the
  compiler copied must be taken both ways in every copy; one it turned into
  `cmov`, `csel` or a predicated ARM instruction is not a branch and is not
  counted; `a && b` may be one branch or two. The same source compiles to
  different branches on each architecture: in the one case read against the
  disassembly, x86-64's `copy_to_user` callers test the result so that the
  error successor is also the next branch's fall-through, and read as covered
  in the upper bound, where the Arm pair's do not. Structural coverage of object code is accepted in place
  of source coverage only with an analysis of the correspondence (DO-178C
  §6.4.4.2b), and none is written. MC/DC would need each condition's
  independent effect shown, which no block trace can give.
* *Compiler-generated branches.* The guards above are most of them. The
  lints remove indexing and `unwrap` from product code, but not arithmetic
  overflow checks in the debug profile (`overflow-checks = true`), `core`'s
  debug-build precondition checks, or `debug_assert!`; the release profile
  drops most of all three and was not measured here.
* *Inlining.* A branch is attributed to the innermost source line, so an
  inlined kernel function's decision is that function's, and `core`'s inlined
  into the item are outside it -- 65,658, 75,795 and 69,560 branches in
  `core`, `alloc` and `src/lib/` -- everything outside `src/kernel/src` -- as
  outside the item as their statements are.
* *Aliasing.* The upper bound credits a successor some other edge entered;
  the lower bound can still be fooled by an indirect jump or an interrupt or
  exception return landing on a successor.
* *Joining builds.* 579, 459 and 454 branches of the gates' other builds had
  no namesake in the reference -- the code only those builds contain, such
  as `test-shell`'s built-in program -- and add nothing.

The method's own check is the branch that ran and left no outcome, which
cannot happen when the successors are right: it reads 0 on all three. It
read 185 on AArch64 before the tool parsed `tbz w8, #0x0, <target>`'s target
rather than its bit number.

## 4. The traceability gap

Everything in §2 and §3 verifies *behaviour*. Almost none of it is linked to a
*requirement*.

`docs/sysml/` carries 33 requirements with stable ids and 32 `verify` /
`objective` links — real bidirectional traceability, and better than most
projects have. But those requirements are at system level (`<'G.1'>` kernel
threads, `<'G.2'>` address-space scale), and 49,431 lines of item product code
trace to 33 of them.

No test names a requirement id. The boot gates assert that 16 of 16 interrupt
deliveries woke their waiter; nothing records which requirement that
discharges.

This is findings F-14, F-15 and F-16 together, and it is the largest structural
gap between this body of work and a DAL C or Class C submission. The fix is not
more testing — there is a great deal of testing. It is low-level requirements
for the item's modules, and a requirement id attached to each assertion.

The [Security Target](SECURITY-TARGET.md) §7 is the first piece of that work: it
maps each of eight security objectives to the code implementing it and the test
exercising it. Eight objectives is not 49,431 lines of traceability, but it is
the shape the rest should take.

---

## 5. Independence

None. Every test in §2 was written by the same process that wrote the code it
tests. DO-178C DAL C requires independence for 5 of its 62 objectives; EN 50716
at SIL 2 permits combined roles with justification, and no justification is
written. Finding F-27.
