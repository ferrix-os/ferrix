# Microkernel M0 — the build flags, the baselines, the checks  ·  *≈ 45–60 points*

The first stage of the switch to a certifiable microkernel
(`docs/MICROKERNEL.md`, §8). M0 moves no code out of ring 0. It builds what
every later stage stands on: one source that builds three kernels by
compile-time flags, the performance baselines the switch is gated against,
and the certification groundwork that must be true before anything larger is
sized or bought.

**Exit:** `cargo xtask check` builds the `present` and `desktop` profiles on
all three architectures and validates `max` and `fast` statically; every
kernel prints its flag set on its boot line, computed from its own `cfg`s,
and the gate compares it with its profile; §7's performance rows have a
method, a frozen baseline on a named commit and a negative control each; and
the standards' clause table and the research of 2026-10-13 are checked
against their sources and corrected, with no later stream sized before that.

**Size:** `MICROKERNEL.md` §8 gives M0 15 points for its records, rows and
checks; the configuration system adds about 30 to 45 of §12.3's 40 to 80.
Both are *guesses*.

**Reviews:** the configuration system's design passed the certification
consultant's review (ledger lines 914, 925, 931 and 941, all conditions
folded into `docs/BUILD-FLAGS.md`), so its code may start. Two pieces get an
item review of their own before code: the fast path's move from a boot
option to a flag (`BUILD-FLAGS.md` §3, BF6), and moving Zenbleed's and GDS's
fixes out of the `mitigations` flag (§3.1, DF6). Every landing that touches
the item goes through the consultant as `docs/CONVENTIONS.md` says.

## The three kernels M0 sets up

| Profile | Shape | Claim | Buildable after M0 |
|---|---|---|---|
| `present` | hybrid: today's tree | today's targets (EAL5+, DAL C, Class C, EN 50716 SIL 2), until M5's flip | **yes**: today's reference configuration |
| `desktop` | hybrid: the Linux layer and btrfs in ring 0 | none | **yes**: the customer's machines |
| `fast` | microkernel | IEC 61508-3 SC 2 / ISO 26262 ASIL B | no: validated statically, refused by `lands` until M5 |
| `max` | microkernel | IEC 61508-3 SC 4 / ISO 26262 ASIL D | no: as `fast` |

## The work

### A. The build-flag system (`BUILD-FLAGS.md` §6)

1. The flag file `tools/common/data/build-flags.toml`: thirteen flags, each
   value's allowed levels (standard by standard), `requires` and
   `conflicts`, the `lands` stage of each value, the levels as full target
   sets, and the boot-option registry with the values each profile permits.
2. The four profile files in `tools/common/data/profiles/`, every flag set
   explicitly, with the build settings that change code without a `cfg`
   (Cargo profile, toolchain, RUSTFLAGS and codegen, the link mode per
   architecture).
3. The resolver in xtask: `--kernel-profile`, `--set` (an ad-hoc build, no
   claim), `cargo xtask flags`; rustc's `--check-cfg` generated from the flag
   file and `unexpected_cfgs` denied.
4. `mitigations` moved onto the new scheme (`ferrix_mitigations="off"` in
   place of `ferrix_mitigations_off`), `build.rs`'s link-mode choice with
   it; **byte-identical images** are its check.
5. The kernel's boot line, computed from `cfg!` constants, which is also
   the start-up configuration audit record (FAU_GEN.1 b); the gate compares
   it with the profile file and ties the image's hash to the run.
6. The profile rule in `check-item-boundary.py`, with
   `certification-item.json` gaining the levels, the flags' and components'
   levels and `present` as the reference configuration.
7. The checks, each with a planted negative control: no `ferrix_` cfg in any
   crate but the kernel's; no read of an undeclared boot option; a `--set`
   build refused; a below-level value refused in a certified profile
   (`program_barrier=session` in `max`, `runtime_trades=game_mode` in
   `fast`); the `present`/`sc2` pair incomparable; `desktop`'s boot line
   naming its light values.
8. `checks=off` as an ad-hoc build, built by `xtask check` on every
   architecture so it cannot rot.

### B. The flags whose second value lands in M0

* **`ipc_fastpath`** replaces the boot option `ferrix.fastpath`, under the
  customer's rule of 2026-10-02 (`max` off until a coverage run has measured
  the fast path and its vulnerability-analysis entry exists; `fast` and
  `desktop` on). Its own item review first (BF6): L.x86_64.159 and .161
  restated, the composition-root hook, stage 9's line, the gate rows that
  pass `--kernel-option ferrix.fastpath=on`, the controls fired again,
  coverage per profile.
* **`smt=off`**: the kernel parks each core's sibling threads itself, after
  the setup a parked thread needs (CR4.MCE on Intel), found from CPUID's
  topology leaves; a boot check counts one running thread per core, with a
  control that starts a sibling.
* **`ssbd=on_request`** (`desktop`): SSBD per thread, written at a switch
  only when it changes; `PR_GET_SPECULATION_CTRL` truthful in every profile.
* **`program_barrier=session`** (`desktop`): barriers between login
  sessions' domains only, under `BUILD-FLAGS.md` §3.1's rules. **F-64 is
  fixed before its code lands.**
* **Zenbleed's and GDS's fixes** leave the `mitigations` flag and stay on in
  every profile (its own small review, DF6).

### C. The performance baselines (`MICROKERNEL.md` §7)

Each row with its method written first, five boots a side, the matched CPU
model and mitigations, a fail rule and a negative control (an injected delay
of known size). Measured on nazuna (AMD Zen 5, no PCID) under the gate's KVM,
with the gate's CPU model passing through IBPB_BRTYPE, ERAPS, SRSO_NO,
SRSO_USER_KERNEL_NO and BTC_NO:

* the bare Linux call (`getppid`), re-measured on a named commit and
  **frozen** as the baseline for the restricted-mode rows (357 ns on
  2026-10-10);
* `fork` + `exec` + `wait`, `mmap`/`munmap` churn, a page-cache miss end to
  end, signal delivery, `futex` wake;
* the 4 KiB block read, re-measured (300 to 844 us on 2026-09-27; target
  under Redox's 80 us);
* warm `rustc hello.rs`, the cold `test-rustc` run and its call count, the
  self-host build;
* Chrome's network bench, the compositor's frame time, `test-unity`.

### D. The load ratchet

A check in `xtask check` that reports the `load` ring's size per landing,
so the lift's size stays visible while feature work keeps landing in it (the
customer: allowed as now). It reports growth and does not fail on it; a
planted file is its control.

### E. The certification groundwork

* **The clause table** of `MICROKERNEL.md` §6.5 checked against the
  purchased standards' texts (IEC 61508-3, ISO 26262-6 and -8, EN 50716,
  DO-178C with DO-330 and DO-333, CC:2022 parts 2 and 3), each row's clause,
  table, grade and level confirmed or corrected. **No later stream (V1, V2,
  T1, F1, F2) is sized before it.**
* **The research of 2026-10-13** checked against its sources
  (`MICROKERNEL.md`, *Sources*): CC:2022's ADV_SPM.1 "entire TSF" reading,
  Arm's Lock-mode ratings and the A78AE manual's facts, seL4's AArch64
  proofs, Ferrocene's qualifications and targets, Verus's compiler pin.
* **A CI build of `core` and `item` with Ferrocene's compiler**, so no Rust
  feature newer than Ferrocene's creeps into the code to be certified.
* **The Pixel 7's cores**: read MIDR_EL1 on the device to confirm Cortex-A78,
  for early barrier, CSV2 and timing measurements.
* **The VEK385's four facts**, so it can be ordered by the end of M1: AMD's
  per-cluster Lock-mode configuration in its own boot software, the SoC's
  safety rating and safety-manual access, the SMMUv3 and GICv3 ITS, the
  price and lead time.

## What the customer does in M0

* **Send the first two contact messages** (drafts in
  `~/.local/share/ferrix/microkernel/contacts/`): an accredited
  certification body for the pre-assessment, and Ferrous Systems for
  Ferrocene. Proofcraft follows in M0 to M1.
* **Buy the standards' texts** that E's clause check needs; they are not in
  the tree.

## Order

```
records (done on the branch)
  └─ A. flag system ──┬─ B. ipc_fastpath (own review)
                      ├─ B. smt, ssbd, Zenbleed/GDS (own review)
                      └─ B. program_barrier=session (after F-64)
C. baselines   ── in parallel, on an idle host
D. ratchet     ── in parallel, small
E. groundwork  ── in parallel; the clause check waits on the texts
```

A, C, D and E can run as parallel streams split by file ownership
(`docs/CONVENTIONS.md`, *Splitting one piece of work across several
agents*); B waits on A.

## Where it stands

Planned, 2026-10-13, on branch `microkernel-plan`. Nothing is built. The
plan (`docs/MICROKERNEL.md`) and the flag design (`docs/BUILD-FLAGS.md`)
passed the certification consultant's reviews with every condition folded
in. M0's code starts when the customer says so.
