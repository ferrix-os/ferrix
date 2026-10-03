# Tool qualification register

Every tool that builds, generates or verifies the item in
[ITEM.md](ITEM.md), classified by what a defect in it could do.

Three standards ask nearly the same question in different words. EN 50716 §6.7
classifies tools **T1** (cannot affect the product), **T2** (can fail to detect
a defect), **T3** (can introduce a defect into the product undetected).
DO-178C defers to DO-330, whose TQL-1..5 turn on the same distinction plus the
software level. IEC 62304 has no tool clause but §5.1.4 expects development
tools to be identified and controlled.

This register classifies. It does not qualify anything: no tool here has a
qualification package, which is findings F-17, F-18 and F-19.

---

## 1. The compiler and linker

| Tool | Version | Class | Note |
|---|---|---|---|
| `rustc` | 1.97.1, pinned in `rust-toolchain.toml` | **T3** | Generates the product. |
| `rust-lld` | bundled with the toolchain | **T3** | Links the product. |
| `cargo` | bundled | T2 | Selects what is compiled; a wrong selection is visible in the artifact. |

**T3 and unqualified — finding F-17.** Good practice is in place and is not
qualification evidence: the channel is pinned exactly rather than floating,
`src/kernel/` and `src/boot/common/uefi/` use no `#![feature]`, and the assembly budget keeps 303
lines across 19 sites outside the compiler's remit where hand analysis is
cheap.

The concrete route is **Ferrocene**, a qualified Rust toolchain with evidence
packages for IEC 62304 Class C, IEC 61508 SIL 4 and ISO 26262 ASIL D. Adopting
it means pinning a Ferrocene-released `rustc` in place of 1.97.1 and checking
the qualified target list. `armv7a-none-eabi` and the three UEFI targets are
the ones expected to fall outside it, and the reference configuration would
have to say so.

---

## 2. Generators that emit product code

Each writes Rust that is committed and compiled into the artifact, so each is
**T3**. Each also has a `--check` mode that fails the build when its output and
its input disagree — which is not qualification, but is the difference between
a generator whose output is verified on every run and one whose output is
trusted.

| Generator | Emits | In the item? | Class |
|---|---|---|---|
| `gen-panic-catalog.py` | the panic explanation catalogue | **yes** | T3 |
| `gen-font.py` | the panic screen's font | **yes** | T3 |
| `gen-term-font.py` | the terminal's font | no — compositor | T3 |
| `gen-wayland-protocol.py` | compositor interface tables | no — compositor | T3 |
| `gen-xkb-tables.py` | keymap tables | no — compositor | T3 |
| `gen-btrfs-fixtures.py` | test fixtures | no — verification only | T2 |

**Only two touch the item** (finding F-18). That is the useful result of doing
this classification against a boundary rather than against the repository: four
of the six are out of scope at the present item definition, and would come into
scope only if the boundary widened to the compositor, which it will not.

---

## 3. Verification tools

A defect here cannot put a fault into the product; it can fail to reveal one.
**T2** throughout, and DO-330 would ask for qualification only of those whose
output is used to *satisfy* an objective rather than to find defects.

| Tool | Role | Class |
|---|---|---|
| `xtask` | build driver and boot-test harness; 242 own tests | T2 |
| `check-item-boundary.py` | the item boundary — this scheme's own scope | T2 |
| `check-complexity.py` | complexity, length and recursion in the item | T2 |
| `check-traceability.py` | the item's requirements against their parents, units and checks; writes TRACEABILITY.md; self-tested on every run | **T2, and load-bearing** |
| `rustlex.py` | tells code from comments and literals for the two above; self-tested on every run | T2 |
| `check-unsafe-audit.py` | every `unsafe` block documented, one operation; every unsafe site in the item names an obligation id the register defines, the untagged remainder ratcheted (F-26) | T2 |
| `check-panic-audit.py` | every panic-lint exemption justified | T2 |
| `check-asm-budget.py` | the assembly allow-list and budget | T2 |
| `check-device-access.py` | the kernel-enumerates-drivers-drive seam | T2 |
| `check-crate-layering.sh` | layering; `src/lib/` depends on nothing above | T2 |
| `gen-soup.py` | the item links no external crate | T2 |
| `coverage-report.py` | statement coverage | **T2, and load-bearing** |
| `decision-coverage.py` | decision (branch) coverage of object code, from the same traces; not yet offered against an objective (F-13) | T2 |
| `llvm-objdump`, `llvm-nm` (the pinned toolchain's `llvm-tools`) | disassembly and symbols for `decision-coverage.py` | T2 |
| `clippy` | ten configurations; denies `unwrap`, `panic`, indexing | T2 |
| `miri` | UB detection over 13 crates | T2 |
| `cargo fuzz` | 30 targets with committed corpora | T2 |
| `loom` 0.7.2 | permutation testing of the orderings the kernel's lock-free looks, and step 4's park protocol, rest on: 15 models in `src/tests/loom` (`cargo xtask loom`, a step of `check`), its own workspace, with the crate set pinned in `src/tests/loom/Cargo.lock`. It checks the protocol restated in the models, not the kernel's code: each model's match to the kernel sites it names (`tests/models.rs`, *Kernel sites*) is by review, and the park models (`tests/park.rs`) name the design's steps, to become kernel sites when the fast path lands | T2 |
| `cargo deny` | RUSTSEC advisories, licences, bans | T2 |
| QEMU 9.2.4 + `libdrcov.so` | executes the item; records coverage | **T2, and load-bearing** |

**`coverage-report.py` and QEMU deserve the emphasis** (finding F-19). Coverage
is not a defect-finding activity whose failures are self-revealing — it is
evidence offered directly against DO-178C table A-7. A tool that over-reports
coverage produces a number nobody can distinguish from a correct one. Its two
known biases are documented in its own docstring and in F-11: it measures the
profile actually booted, and a basic block credits every statement inside it,
which errs optimistically.

The two ratchets share the failure mode. A boundary or complexity gate that
under-reports passes, and a pass is indistinguishable from a correct one. Both
did until 2026-09-26: `check-item-boundary.py` saw 29 of 56 upward references
and `check-complexity.py` measured 1,559 of 1,887 functions, because they
found strings with a pattern that mis-paired quotes and the boundary gate read
only the literal text `crate::a::b` (FINDINGS.md §A, F-25). Both now read code
through `rustlex.py` and run their own self-tests before every measurement,
so a lexer or resolver regression fails the build instead of shrinking a count.

`check-traceability.py` (W-8, since 2026-09-27) is load-bearing for the same
reason: the matrix it writes is offered against DO-178C's requirements-based
testing objectives and 62304 §5.4, not used to find defects. Its failure mode
is to *credit* a requirement: a `/// Verifies:` tag is a claim by whoever wrote
it that a check discharges a requirement, and the gate checks that the claim
is well formed, names a requirement that exists and sits on a check -- not that
the check actually tests what the requirement says. That judgement is review,
recorded by the negative control in the commit that adds the tag. Its
run-time column reads the `verification` map `coverage-report.py` writes (the
checks' own reached statements, kept apart from the item's coverage), so it
inherits TOR-3's biases, and until a coverage run has written that map it says
*not measured* rather than guessing. Every run starts with its self-test over
crafted model, Rust and coverage fragments, as the two ratchets above do.

QEMU is also the *execution platform* for all boot evidence, not merely an
observer of it. Every claim in [VERIFICATION.md](VERIFICATION.md) except the
host unit tests is a claim about the item's behaviour under emulation, and the
STM32MP157D-DK1 is the only hardware any of it has run on.

**x86-64's QEMU is a patched build** since NVIDIA's N0g: QEMU 10.2.1 with
`tools/common/data/qemu/0002-intel_iommu-honour-CFI-and-block-compatibility-format.patch`,
built by `tools/common/fetch/fetch-qemu-linux.sh` and reporting `(ferrix-cfi)`
in its version, which xtask requires for an x86-64 boot on Linux. Stock QEMU's
VT-d ignores `GCMD.CFI` and passes a compatibility-format interrupt through
with remapping on, so on it no configuration isolates interrupts and finding
F-57's closure, and checks R1 and R2, would show nothing. The patch makes
`GSTS.CFIS` follow `CFI` and refuses such a message with fault 0x25; its qtest
fails with either half removed. It is offered upstream, and the pin moves to
the first release that carries it. It is test infrastructure of the reference
configuration, T2 as QEMU is. The reference machine's x86-64 CPU model
carries `+x2apic` under TCG as well as KVM (`qemu::x86_cpu`), so that check
R8 can put a processor in x2APIC mode and see it switched back; the kernel
runs xAPIC either way.

---

## 4. Host crates

The 21 external crates in `Cargo.lock` are tools by this register's definition,
since none is in the item — see [SOUP.md](SOUP.md) §2 for the list with
versions and licences. They build, test and package; `syn`, `quote` and
`proc-macro2` are T3 by the strict reading, since a procedural macro emits
code, though none is used in `src/kernel/` or `src/boot/common/uefi/`.

All are watched by `cargo deny check` with an empty `advisories.ignore` list
and `yanked = "deny"`.

---

## 5. What qualification would actually require

For the two T3 tools that reach the item — `rustc` and the two generators —
DO-330 offers two routes, and the cheaper one is available here.

* **Qualify the tool.** For `rustc` this means Ferrocene, and buying it rather
  than building it.
* **Verify the output instead.** The generators already do this: `--check`
  re-derives the output from the input on every build, so a generator defect
  that changed its output would fail the gate. Extending that argument into a
  DO-330 tool operational requirements document is a page of writing, not a
  project.

For `rustc` the output-verification route is the DAL A source-to-object
analysis, and it is not proportionate at DAL C. Ferrocene is the answer.

---

## 6. Tool operational requirements

DO-330 asks, for each tool whose output is relied on, what the tool must do,
what it must not do, and how that is verified. For the three T3 tools inside
the item boundary the output-verification route applies, and this is that
argument written down. It closes the *documentation* half of F-18 and F-19; the
qualification of `rustc` (F-17) is untouched and is the reason neither finding
is struck out.

### TOR-1 — `gen-panic-catalog.py`

| | |
|---|---|
| Output in the item | the panic explanation catalogue compiled into `src/kernel/src/panic/catalog.rs` |
| **Shall** | derive every entry from the catalogue source, deterministically |
| **Shall not** | emit an entry that its input does not contain, or omit one it does |
| Failure mode | a panic prints the wrong explanation; the kernel's behaviour is unchanged |
| Verification | `--check` re-derives the output and fails the build on any difference, on every run of `cargo xtask check` |
| Residual | a defect present in *both* the generator and its `--check` path would not be caught. The two share code, so this is not independent verification. |

### TOR-2 — `gen-font.py`

| | |
|---|---|
| Output in the item | the panic screen's glyph table |
| **Shall** | rasterise from the committed BDF, byte-identically on any host |
| **Shall not** | depend on a font library installed on the build machine |
| Failure mode | the panic screen is unreadable. It cannot affect any other behaviour: the table is read only by the panic path, after the serial report has already been written. |
| Verification | `--check`, as TOR-1 |
| Residual | as TOR-1, plus: nothing verifies the glyphs are *legible*, only that they match the input |

### TOR-3 — `coverage-report.py`

The one whose failure is least visible, and the only one whose output is
offered directly as evidence against an objective rather than used to find
defects.

| | |
|---|---|
| Output | the statement-coverage figure in [VERIFICATION.md](VERIFICATION.md) |
| **Shall** | count as reached only statements whose address lies in a basic block the run executed |
| **Shall not** | over-report; where it cannot be exact it must err low, or declare the direction |
| Failure mode | **a coverage figure nobody can distinguish from a correct one** |
| Verification | none independent. This is the gap. |
| Known biases | two, both optimistic and both declared in the tool's docstring and in F-11: a basic block credits every statement inside it even when a trap left it early, and optimised builds map one address to several source lines |
| Evidence it is not wildly wrong | three measurement defects were found and fixed by cross-checking its output against raw `objdump` and against the source — the bare-name recursion match, the `Drop::drop` case, and `extern "C"` declarations taking the following item's body. Two more on 2026-09-26, found by comparing architectures: a search sentinel below the higher half that dropped every block starting on a statement (under-reported x86-64 and AArch64 by about a third), and a union that read every gate's trace against one kernel although the gates build different ones (over-reported). The published 81.9% had both; VERIFICATION.md §3.4. A third the same day, found walking AArch64's residual line by line: line-table rows the linker had left behind for functions it discarded (every caller inlined them) were counted as statements, at addresses a few bytes above zero that no run can execute. They were about 30% of every architecture's residual and under-reported; rows outside the image's loadable span are now not statements. And a fourth: objdump prints a file header only when the file changes, and after a sequence ends the line program returns to the unit's first file without printing it again, so those rows were filed under whichever header came last -- `core`'s `map.rs` as lines of `arch/arm_common/gicv2.rs`, other files' 210 as `syscall/uaccess.rs`'s. Both directions, about 700 rows on AArch64; the parser now reads objdump's wide output, which names each unit's first file |

**TOR-3 has no independent verification and should not pretend to.** The
honest mitigation is that its biases are documented, its residual output is
enumerable (`coverage-residual-<arch>.json`), and a reviewer can spot-check any
file against the source. Measuring three architectures is itself a cross-check:
generic code every boot runs must read alike on all three, and the two defects
of 2026-09-26 were found because it did not. A qualification effort would need a second
implementation to compare against.

### TOR-4 — `decision-coverage.py`

Classified with `coverage-report.py` and read the same way, one step further
from an objective: DAL C asks no decision coverage, so its figure is offered
as a measurement of the distance to DAL B (F-13), not as evidence meeting one.
It becomes load-bearing the day it is.

| | |
|---|---|
| Output | the decision-coverage figures in [VERIFICATION.md](VERIFICATION.md) §3.6 and `decision-coverage-<arch>.json` |
| **Shall** | count a direct conditional branch as taken both ways only when executed blocks began at both its target and its fall-through; count every such branch in the item's code, guards included, in the denominator |
| **Shall not** | count a conditional call or conditional indirect branch as covered; count an outcome as *sure* while another instruction with a known edge into that successor ran |
| Failure mode | as TOR-3: a figure nobody can tell from a correct one |
| Verification | none independent. Its own check is the branch that ran and left no outcome, which correct successors make impossible; it reads 0 on every architecture |
| Known biases | the upper bound credits a successor another edge entered; the lower bound can be fooled by an indirect jump or an interrupt or exception return; the object-code unit counts each inlined or monomorphised copy of a decision; branchless code (`cmov`, `csel`, ARM predication) is not counted. All in its docstring and VERIFICATION.md §3.6 |
| Evidence it is not wildly wrong | two defects found before the first figure was published, both by its own checks. `tbz w8, #0x0, <target>` was read as a branch to the bit it tests, leaving 185 AArch64 branches that ran with no outcome; the target is now the operand objdump annotates. And the guard walk ran on past an ARM `pop {..., pc}` return and past a call that does not return (`idle_loop`), into the next block's panic, making 192 ARMv7-A branches and 3 AArch64 ones look like guards that had panicked; it now stops at a return, a function's start and the first call. Every executed block of an x86-64 boot starts on an instruction boundary of the disassembly, which checks the slide and the reconstruction it shares with TOR-3 |
