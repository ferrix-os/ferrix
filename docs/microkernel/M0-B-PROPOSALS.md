# M0 part B: the item reviews and the plans

**For the certification consultant, 2026-10-14.** `docs/roadmap/microkernel-m0.md`
§B and its *Reviews* paragraph ask for two item reviews before any code:
the fast path's move from the boot option `ferrix.fastpath` to the build
flag `ipc_fastpath` (`docs/BUILD-FLAGS.md` §3, **BF6**), and Zenbleed's and
GDS's fixes leaving the `mitigations` flag (§3.1, **DF6**). §1 and §2 are
those two design notes, each ending with the questions the consultant is
asked to answer. §3 to §5 are implementation plans, not reviews, for the
other three values M0 builds: `smt=off`, `ssbd=on_request` and
`program_barrier=session`. Their designs were reviewed at ledger line 931
(OK IF DF1 to DF8), with the conditions met at line 941; the plans name the
files, the checks and the controls, and list what the code will have to
decide.

**Reviewed 2026-10-14** (`~/.local/share/ferrix/cert-consultant/reviews.md`,
reviewer m0-b-cert, at `ef59e4115` on `main` `65608f7a0`):

* **BF6: OK IF (1) to (9)**, ledger line 1001. The conditions are §1.8 and
  bind the code; every answer is recorded under its question in §1.7.
* **DF6: OK IF (1) to (4)**, ledger line 1012. The conditions are §2.6;
  the answers are in §2.5.
* **§3 to §5: design comments**, ledger line 1018 (not item reviews: each
  value gets its code review before `land.sh take`). They are folded into
  the plans, marked *(1018)*.
* **F-73 reserved** (line 1018): the kernel never sets `CR4.MCE` (§3). Its
  fix is a small item change of its own, made by another agent on branch
  `m0-f73`, and lands **before** `smt=off`.

The corrections the review found (1001 f1 to f5, 1012 f1 and f2, 1018 f1
to f4) are made in the text below. These conditions are the binding plan.

Written against nazuna's local `main` at `c0bded3c7`, and against branch
`m0-flags` at `91a2b456f` (part A, not landed), whose names are used where
part A defines them: `cfg(ferrix_<flag> = "<value>")` set by the kernel's
build script alone; `build-flags.toml`'s `built = "stand-in"` for a value
whose code is not built yet; the `build` boot line from
`src/kernel/src/build_flags.rs`; `check-boot-line.py`, which refuses a
`--set` boot unless `--no-claim` names it ad hoc; `cargo xtask test-flags`.
Every part below waits on part A landing. Nothing here is built.

Sizes are in points (1 point ≈ 20 to 30 minutes of one session) and are
estimates.

| Part | Kind | Waits on | Size |
|---|---|---|---|
| §1 `ipc_fastpath` (BF6) | item review, then code | part A; this review | 15 to 19 |
| §2 Zenbleed and GDS out of `mitigations` (DF6) | item review, then code | part A; this review | 4 to 6 |
| §3 `smt=off` | plan | part A; F-73 (branch `m0-f73`) | 10 to 13 (F-73's fix not included) |
| §4 `ssbd=on_request` | plan | part A; §2 (same file) | 8 to 11 |
| §5 `program_barrier=session` | plan | part A; F-64 (branch `m0-f64`); §4 (same file) | 8 to 10 |
| | | | **45 to 59** |

That is more than the stage page assumed for B: `MICROKERNEL.md` §8 gives
M0 15 points for its records and the configuration system 30 to 45, part A
included. The difference is mostly §1's re-evidencing (the gate rows, the
controls, coverage) and §3's topology and parking, neither of which the
earlier figure itemised.

---

## 1. `ipc_fastpath` replaces `ferrix.fastpath` (BF6)

### 1.1 What there is today

The switch is a boot option read at run time, and the fast path's code is in
every x86-64 image:

* `src/kernel/src/fastpath.rs` (item ring): `fastpath::init` reads
  `ferrix.fastpath` once, from the loader's command line and then the device
  tree's `bootargs` (four readings: none, `on`, `off`, other), and for `on`
  alone, where `arch::FAST_WRITE_READ` (true only in
  `arch/x86_64/mod.rs:50`), calls `trap::set_fast_write_read`.
  `fastpath::report` prints stage 9's line, `fastpath ipc fast path for
  channel_write_read: on (as ferrix.fastpath asked); taken from x86-64's
  SYSCALL entry`. Called from `checks::init` (`checks.rs:80`) and
  `stages_check.rs:1081`. `fastpath::report_counts` prints the counts line
  (`init.rs:667`).
* `src/kernel/src/trap.rs` (core): `FAST_PATH_ON`, the core's flag (T1),
  raised by `set_fast_write_read` and read (Acquire) by `fast_path_on` at
  every entry; `fast_write_read`, `fast_write_read_now`; the `unsafe extern
  "Rust"` declaration of `ferrix_fast_write_read` beside `ferrix_filter_quiet`,
  each tied to its alias by a `const _`.
* `src/kernel/src/main.rs:146-154` (composition root): the hook's
  definition, `#[unsafe(no_mangle)] fn ferrix_fast_write_read`, one call of
  `syscall::native::fast_write_read`.
* `src/kernel/src/arch/x86_64/syscall.rs:485-505`: `ferrix_syscall_entry`
  tests `rax == CHANNEL_WRITE_READ && trap::fast_path_on()` first, before the
  filter; `frame_tail`, `write_outcome` and `leave` serve both paths.
* `src/kernel/src/syscall/native.rs`: `fast_write_read` and
  `fast_write_read_raised` (T2 to T5), `continue_general`, `park_for_reply`;
  `receive_words` parks for a fast writer only when
  `trap::fast_write_read().is_some()` (line 959).
* Elsewhere: `sched/direct.rs` (the direct switch and its counts),
  `object/channel.rs` (the park record, `send_direct`, `park`, `unpark`),
  the stage-9 cases in `object/fast_path_check.rs`, whose line 171 also
  asks `trap::fast_write_read().is_some()`, and case 15 in
  `arch/x86_64/switch/check.rs:1397`, which asks the same.
* `tools/common/data/certification-item.json`: `composition_root.hooks`
  lists `ferrix_fast_write_read` (`declared_in` `trap.rs`, `forwards_to`
  `syscall::native::fast_write_read`); `fastpath.rs` is in the item ring.
* The requirements: **L.x86_64.150** (`fastpath::init` reads the option,
  raises the flag for `on` alone, stage 9 says which; criterion: two boots of
  `test-ipc-equiv`), **L.x86_64.159** (the entry calls the fast path only
  for 0x1013 and only while the core's flag, read at every call, is raised,
  "which fastpath::init does for ferrix.fastpath=on alone"), **L.x86_64.161**
  (the core reaches the fast path and T2's predicate only through the two
  listed link-time hooks, each call only while a core flag says so;
  criterion: `check-item-boundary.py`'s self-tests and two boots), and
  **L.x86_64.160** (`frame_tail`), whose criterion says "with the fast path
  on". All four are verified by `tools/common/xtask/src/ipc.rs::test_ipc_equiv`
  (TRACEABILITY.md lines 986 to 988, 1113, 2250 and 2251;
  `docs/sysml/18-x86-64-requirements.sysml` lines 413 to 458 and 1498).
* On `m0-flags`: `ipc_fastpath` has `off` (`built = "yes"`) and `on`
  (`built = "stand-in"`, `stands_in = "off"`, allows `sc2` and `none`);
  `present` and `max` say `off`, `fast` and `desktop` say `on`; the
  boot-option registry still declares `ferrix.fastpath` with `until = "M0"`,
  and `present.toml` permits `"ferrix.fastpath" = ["off"]`.

### 1.2 The change

1. **The code is selected at compile time, and is absent where the flag is
   off** (rule 1: code a profile does not select is not in its image).
   `#[cfg(ferrix_ipc_fastpath = "on")]` on the entry's test in
   `ferrix_syscall_entry`, on the hook's declaration in `trap.rs` and its
   definition in `main.rs` (with their `const _` assertions), and on
   everything only the fast path reaches: `syscall::native::fast_write_read`,
   `fast_write_read_raised`, `continue_general`, `park_for_reply` and the
   park branch of `receive_words`; `sched::direct`; the park record and its
   takes in `object/channel.rs`; `frame_tail` and the `Fast` answer in
   `trap.rs`; the fast-path stage-9 cases in `object/fast_path_check.rs` and
   case 15's sending variant. The shared sites that ask whether the path is
   on, `trap::fast_write_read()`'s three callers (`native.rs:959`,
   `switch/check.rs:1397` and `object/fast_path_check.rs:171`), read a
   constant, `fastpath::BUILT`, from the same predicate.
   **One predicate (1001, condition 1):** the code is selected by
   `all(ferrix_ipc_fastpath = "on", target_arch = "x86_64")`, defined once
   (a cfg the kernel's build script derives, or one `cfg_attr` alias) and
   never retyped per site; `arch::FAST_WRITE_READ` goes with it. The flag
   file records that `on` selects code on x86-64 only; an AArch64 or
   ARMv7-A image built `on` holds none of it, and stage 9's line and the
   `build` line say so.
   **The cut complete (condition 2):** every general-path read of the park
   record in `object/channel.rs` (the takes in `write`, `write_small`,
   `unread` and the close) is listed in the landing and shown absent or
   unchanged in the `off` image.
2. **`FAST_PATH_ON` goes.** With the code absent in an `off` build and
   present only in an `on` build, the run-time flag has nothing left to
   decide: the entry calls the fast path for 0x1013 whenever it was built
   with it. `trap::set_fast_write_read`, `fast_path_on` and the Acquire load
   at every entry are removed. This is the core and architecture change
   BUILD-FLAGS.md §3 asks about ("whether the system-call entry's flag test
   compiles out"): it does, on both sides. One load less on every x86-64
   system call in an `on` build; no test at all in an `off` build.
   *Condition 2:* the Release/Acquire pair goes only with a written argument
   that, in an `on` build, nothing the fast path reads is set up after the
   first ring-3 0x1013. The flag was raised in `checks::init`
   (`checks.rs:80`), so a program run before that point took the general
   path before and takes the fast path now. The stage checks before stage 9
   must pass on the `on` build.
3. **`fastpath.rs` reads no option.** `init` and its four readings go;
   `report` prints `fastpath ipc fast path for channel_write_read: on (built
   with ipc_fastpath=on); taken from x86-64's SYSCALL entry`, or `off (built
   with ipc_fastpath=off)`, or on Arm `on (built with ipc_fastpath=on);
   x86-64 only, so every call here takes the general path`. `checks::init`
   no longer calls `fastpath::init`; a boot that skips stage 9 still prints
   the line there.
4. **The boot option is retired.** `build-flags.toml`: `on` becomes
   `built = "yes"`, `stands_in` goes, `[boot_options."ferrix.fastpath"]`
   goes; `present.toml` loses its permit; `check-build-flags.py`'s registry
   check then fails any read of `ferrix.fastpath` that comes back. xtask
   refuses `--kernel-option ferrix.fastpath=<anything>` with a message naming
   `--set ipc_fastpath=on --no-claim` or `--kernel-profile desktop`, so an old
   gate row fails loudly instead of running the general path and passing.
5. **The hook stays a listed hook, now conditional.**
   `composition_root.hooks`' entry for `ferrix_fast_write_read` gains
   `"cfg": "ferrix_ipc_fastpath=on"`; `check-item-boundary.py` requires the
   same `cfg` on the declaration and the definition, and its self-test gains
   a case for each half missing it. `ferrix_filter_quiet` is unchanged.
6. **The audit record.** The fast path's value is already in
   `Config::Build` (part A: two bits per flag, FAU_GEN.1 b), so the boot's
   audit record names it; no record of its own is added. SECURITY-TARGET's
   and AUDIT.md's lists of the start-up record name `ipc_fastpath` from
   `Config::Build` and no longer `ferrix.fastpath` (condition 8).

### 1.3 The requirements restated

Requirement text for the implementer to write after this review
(condition 4). No new ids are needed.

* **L.x86_64.150 is retired** (Q2's answer): its subject, reading the
  option, is gone, and part A's `build` line already reports the value. It
  is marked retired in the SysML model and TRACEABILITY.md and never
  reused. Stage 9's line becomes informational; the check that an `off`
  image says `off` moves into L.x86_64.159's criterion.
* **L.x86_64.159**: "In a kernel built with ipc_fastpath=on for x86-64,
  ferrix_syscall_entry shall call the fast path for 0x1013 and for no other
  number, before the filter and with interrupts masked, after every entry
  measure the stub makes, raising the vector mark first and lowering it on a
  decline; in a kernel built with ipc_fastpath=off the entry shall contain
  no call of it." Criterion: the `on` boot's trips; for the `off` half, C6
  (the `off` image holds neither the hook nor the marker, §1.5) and its
  stage-9 line saying `off`. Units without `trap::fast_path_on`.
* **L.x86_64.161**: the flag clause becomes "the fast path only in a kernel
  built with ipc_fastpath=on for x86-64, through a hook declared and defined
  under that predicate"; the predicate's clause is unchanged. Its criterion
  adds the cfg-mismatch self-test case, and its boots ("booted with
  ferrix.fastpath=off no fast path counter moves and with on trips are
  taken", `18-x86-64-requirements.sysml:441`) are restated as the `off` and
  `on` images of `test-ipc-equiv` (1001, f3).
* **L.x86_64.160**: the criterion's "with the fast path on" becomes "in the
  `on` build".
* **L.sched.61** (`15-sched-requirements.sysml:530-538`) and
  `13-item-requirements.sysml:446` ("behind ferrix.fastpath") are reworded
  to the flag.
* No requirement's units name `fastpath::init`, `trap::fast_path_on` or
  `trap::set_fast_write_read` any more.

### 1.4 Every gate row that passes `ferrix.fastpath` today

Found in the tree, in the batch runner's history
(`~/.local/share/ferrix/fleet/batch/runs/*/gates`, `full/*.cmd`) and in the
fleet's gate files. Counts are the number of batch runs, of the 63 there,
that carried the row (corrected by 1001 f1: each run's `gates` and
`summary` files both name a row, and were counted twice).

| Row | Where | Becomes |
|---|---|---|
| R1 `test-boot --arch x86_64 --accel kvm --kernel-option ferrix.fastpath=on` | 11 batch runs (IPC, scheduler and object landings) | `test-boot --arch x86_64 --accel kvm --set ipc_fastpath=on --no-claim` |
| R2 `test-boot --arch x86_64 --accel tcg --kernel-option ferrix.fastpath=on` | 11 batch runs; BACKLOG row 436 (po10-f65, stage 13's cgroupfs check under TCG at load 34, open) | the same with `--accel tcg`; row 436's reproduction recipe rewritten |
| R3 `test-boot --arch x86_64 --release --accel kvm --kernel-option ferrix.fastpath=on` | 9 batch runs; `fleet/po9-fx0902-gate.txt` | `--release ... --set ipc_fastpath=on --no-claim` |
| R4 the same with `--smp 2` | 9 batch runs; `fleet/po9-fx0902-gate.txt` | the same with `--smp 2` |
| R5 `test-ipc-equiv --arch all` and `--arch x86_64 --accel tcg` | `ipc.rs:621` writes `ferrix.fastpath={off,on}` itself; `fleet/po9-sched-{a,b}-gate.txt`, batch runs | two images, 1.5 |
| R6 `bench-ipc --kernel-option ferrix.fastpath=on`, and `bench-ipc --alternate`, which forwards the options to the other tree | measurements, not gate rows: `docs/hotpaths/results/ipc-round-trip/6bc2646211ca/` (7 records of 2026-10-07 name the option in `kernel_options`); `ipc.rs:202`, `other_tree_args` at 337 to 353 and its unit test at 706 to 724 | `--set ipc_fastpath=on --no-claim`; the record names the flag set (part A's `hotpath/record.rs` change); `--alternate` against a tree from before the move passes the option, against one after passes `--set` (the other tree's xtask decides which) |
| R7 owed: a stage-13 boot with the fast path on (`docs/CGROUPS.md:782`, BACKLOG row 591's F9) | not yet a row | written as R2's form when it is made |

Also reworded, not rows: `.claude/skills/optimize-ipc-round-trip/SKILL.md`
(the measuring protocol's step 2 records `ferrix.fastpath`, and the "tested
equivalence" paragraph), `docs/roadmap/ipc-round-trip.md:16`,
`where-it-stands.md:52`, `README.md:51`, `status.md:85`,
`docs/certification/ITEM.md:203` (§2's link-time hooks),
`docs/hotpaths/ipc-round-trip.json:125`, `tools/common/xtask/src/main.rs:269`
(`test-ipc-equiv`'s help), `src/user/system/native/ipc-equiv/src/main.rs:9`,
`docs/MICROKERNEL.md:1267` (a pointer-sized update: "It is a boot option
today"), the code comments at `arch/x86_64/syscall.rs:487`,
`syscall/native.rs:1023`, `trap.rs:449` and `object/fast_path_check.rs:7`
(1001, f5), and `OPAQUE-KERNEL.md` §9.7 (one dated note that the switch
became the flag; the history stays as written). No row in CI (`.github/`)
and none in `xtask check` or the coverage run's gate list (`coverage.rs`)
passes it.

**Condition 7:** after the landing, `git grep ferrix.fastpath` is empty
outside dated history (OPAQUE-KERNEL.md's history, `docs/handover`,
FINDINGS.md, `docs/hotpaths/results`, `open-branches.md`), and the product
owner is told the same day to change the fleet's gate lists outside the
repository (`po9-fx0902-gate.txt` and the batch runner's standard rows).

### 1.5 The equivalence, the controls, coverage

**The equivalence becomes two images.** `test-ipc-equiv` booted one image
twice, with two command lines. It now builds `present` and `present --set
ipc_fastpath=on --no-claim`, which differ in that flag alone; it prints both
images' sha256 and both `build` lines, requires the two lines to differ in
exactly `profile`, `level` and `ipc_fastpath`, and then compares the
transcripts and the counts as today (trips on the `on` boot; every counter
zero on the `off` boot, which now holds no counter to move, since
`sched::direct::counts` is compiled out: `report_counts` prints the line
as all zero there, or the check accepts the line's absence; the
implementer picks one and C1 covers it). On Arm the `off` boot alone, as today.
*Condition 5:* both images come from one commit, one toolchain and one
profile file; their `build` lines differ in `ipc_fastpath`, `profile` and
`level` only; both sha256s are printed, and on x86-64 they must differ.

**The controls fired again under the flag.** Each with `gate.sh control`
and a log showing it fired, on the `on` build unless said:

* C1 `test-ipc-equiv`'s own two: a fast path that always declines (T2's
  answer forced false) fails "took no trip"; an `off` image whose stage-9
  line says `on` fails the line check.
* C2 `check-item-boundary.py`'s hook self-tests: an unlisted declaration, a
  definition outside `main.rs`, a body that does more than forward; and the
  new one, a declaration under the cfg with a definition without it.
* C3 A1's control (T12 removed: FX-0530, the pick not the peer) and T11's
  control (`OPAQUE-KERNEL.md` §9.7, item 14), and case 10's control.
* C4 cases 11, 13 and 14's controls (the decline lowering `IN_CALL`,
  L.object.169; the in-domain barrier decisions; the T13 hook), and F-65's
  (a close let go with interrupts masked).
* C5 L.x86_64.160's: `frame_tail` writing a fifth register.
* C6 new: the `off` image holds no fast path. A symbol check on the linked
  `present` kernel, in `xtask check` or `test-flags`, **keyed on what cannot
  be inlined away** (condition 3): the `#[no_mangle]` hook
  `ferrix_fast_write_read`, and a `#[used]` marker static under the same
  predicate. Mangled Rust names are not used: an inlined function's
  absence proves nothing. Control: the `present` build with the hook's cfg
  removed (always compiled) fails it, FIRED.
* C7 new: xtask refuses `--kernel-option ferrix.fastpath=on` (an
  `args.rs` unit test and one run showing the refusal); and a planted read
  of `ferrix.fastpath` in the kernel is refused by `check-build-flags.py`'s
  registry check, FIRED (condition 7).
* C8 part A's `--set` without `--no-claim`, refused, fired once with
  `ipc_fastpath=on` now that it builds code.

The loom models of the park protocol (`src/tests/loom/tests/park.rs`,
`slots.rs`) are unchanged and need no re-firing. The implementer takes the
control tags from ledger lines 404, 415 (H1 to H7) and 509 to 520 and names
each re-fired one against its original. *Condition 6:* C1 to C8 FIRED by
`gate.sh control --expect` on the landing's code; R1 to R4 in Q4's form
`run: PASSED` on the landing hash, with KVM and TCG named; BACKLOG row
436's recipe rewritten.

**Coverage per profile.**

* `present` (`off`, today's reference configuration): the fast path's
  lines are not in its image. The coverage run measures `present`, so those
  lines leave its denominator: the carried and argued anchors for them
  (COVERAGE-WORKLIST.md's rows "step 4" and "step 4's groundwork:
  ferrix.fastpath"; `coverage-owed.json`'s entry at line 62 and the step-4
  entries; the `coverage-argued-*.json` and `coverage-residual-*.json` rows
  on those lines) are removed with a stated reason, "not in present's
  image: cfg(ferrix_ipc_fastpath = "on")", and a check that every removed
  anchor lies inside such an item. `fastpath.rs`'s `init` lines go with the
  code. The cfg attributes shift lines, so the evidence is re-carried
  (`carry-coverage.py`) and the traceability rows checked, which are
  line-keyed. *Condition 8:* every removed anchor is listed by file:line in
  the landing message, with the check that each lies inside a cfg'd item;
  `carry-coverage`, then `gen-coverage-justification --check` after the
  final rebase, and the line-keyed TRACEABILITY rows re-read (F-62).
* `desktop` (`on`): no claim, no coverage owed.
* `fast` (`on`): not buildable until M5. `on` selects nothing on AArch64,
  its only claimed architecture, so **no `fast` figure may rest on the
  x86-64 fast path** (Q6's answer).
* `max` (`off`): absent. The 2026-10-02 rule is moot for `max` until the
  fast path is ported to AArch64; its measurement is then a coverage run of
  the AArch64 `on` build. An x86-64 TCG coverage run of an `on` build is
  regression evidence for no claim (Q6's answer).

### 1.6 Size

Kernel cfgs, `fastpath.rs`, the removed flag: 4 to 5. The hook's cfg in
`check-item-boundary.py` and its self-test: 1 to 1.5. xtask
(`test-ipc-equiv` as two images, the refusal, `bench-ipc`): 2 to 3. The flag
file, profiles, registry: 0.5. Requirements, SysML, TRACEABILITY, ITEM.md
§2, the skill, the roadmap pages and §1.4's reword list: 2 to 3. Coverage per profile and
the re-carry: 2. Gating and the controls C1 to C8 under KVM and TCG: 3 to
4. **15 to 19.**

### 1.7 Questions for the consultant (BF6), and the answers (ledger 1001)

1. **Compile-out depth.** Is §1.2's cut right: every item only the fast
   path reaches under `cfg(ferrix_ipc_fastpath = "on")`, `FAST_PATH_ON`
   removed, so the `off` image has neither the code nor the entry's test?
   The alternative, keeping the run-time flag and raising it from a `cfg!`
   constant, leaves the fast path as deactivated code in `present`'s and
   `max`'s images, which rule 1 forbids; it is listed only to be refused.
   *Answer: yes, the cut is right; the run-time flag raised from a `cfg!` is
   refused (rule 1).*
2. **L.x86_64.150.** Restated as stage 9's line from the cfg, or retired,
   with the line's check moved into L.x86_64.159's criterion?
   *Answer: retired. Its subject, reading the option, is gone; part A's
   `build` line already reports the value; stage 9's line becomes
   informational; the check that an `off` image says `off` goes into 159's
   criterion (§1.3).*
3. **The equivalence across two images.** Is `test-ipc-equiv` comparing
   `present` with `present --set ipc_fastpath=on --no-claim` (sha256s
   printed, boot lines required to differ in that flag alone) an acceptable
   form of the tested equivalence that §9.7 and the skill rest on, now that
   the two paths are no longer in one image?
   *Answer: accepted, under condition 5.*
4. **Where the `on` rows and controls run before `fast` builds.** Ad-hoc
   boots of `present`, or `desktop` boots?
   *Answer: `present --set ipc_fastpath=on --no-claim` boots, so the
   difference is one flag, as in the equivalence. `desktop` boots are rows
   of their own and never the fast path's evidence.*
5. **`on` on Arm.** The fast path is x86-64 only, and `fast` claims AArch64
   alone. Accept the value with stage 9's "x86-64 only" line, or have the
   flag file mark `on` as selecting code on x86-64 only?
   *Answer: the cfg selects code on x86-64 only, under condition 1.*
6. **The 2026-10-02 rule for `max`.** Does it still gate `max`'s value,
   and is an x86-64 coverage run of an `on` build its measurement?
   *Answer: `max` claims AArch64 alone, where `on` selects nothing, so the
   rule is moot for `max` until the fast path is ported to AArch64; then the
   measurement is a coverage run of the AArch64 `on` build. An x86-64 TCG
   coverage run is regression evidence for no claim. The same holds for
   `fast`: no `fast` figure may rest on the x86-64 fast path.*
7. **The audit record.** Is the fast path's value inside `Config::Build`
   enough for FAU_GEN.1 b, with stage 9's line informational?
   *Answer: yes, with condition 8.*
8. **The boot option's retirement.** Removing `ferrix.fastpath` from the
   registry and from `present`'s permits, with xtask refusing the old
   option (C7), in one landing.
   *Answer: yes, one landing, with condition 7.* *Correction (1001 f4):*
   the question also spoke of "ITEM.md §5's count". That was wrong:
   ITEM.md §5 counts one boot option, `ferrix.devmgr`, and `ferrix.fastpath`
   was never in it. The fast path is described in §2, line 203. So §5 was
   understated while the option existed, and the move makes it true; only
   §2's line 203 is reworded.

### 1.8 The conditions (ledger 1001, OK IF (1) to (9))

1. **One predicate.** `all(ferrix_ipc_fastpath = "on", target_arch =
   "x86_64")`, defined once and never retyped per site; the flag file
   records that `on` selects code on x86-64 only; an AArch64 or ARMv7-A `on`
   image holds none of it, and stage 9 and the `build` line say so (§1.2).
2. **The cut complete.** `object/fast_path_check.rs:171` named; every
   general-path read of the park record in `object/channel.rs` listed and
   shown absent or unchanged in the `off` image; the Release/Acquire pair
   removed only with the written argument of §1.2 (2); the stage checks
   before stage 9 pass on the `on` build.
3. **C6 keyed on what cannot be inlined away**: the `#[no_mangle]` hook and
   a `#[used]` marker static under the same predicate; its control FIRED.
4. **Requirements**: L.x86_64.150 retired, marked so in the model and
   TRACEABILITY, never reused; 159 and 161 restated as §1.3 says; 160,
   L.sched.61 and `13:446` reworded; no units naming `fastpath::init`,
   `trap::fast_path_on` or `trap::set_fast_write_read`.
5. **The equivalence**: one commit, toolchain and profile file; the lines
   differ in `ipc_fastpath`, `profile` and `level` only; both sha256s
   printed and required to differ on x86-64; C1's two controls FIRED.
6. **Controls and rows**: C1 to C8 FIRED by `gate.sh control --expect` on
   the landing's code, each named against its original (ledger 404, 415,
   509 to 520); R1 to R4 in Q4's form `run: PASSED` on the landing hash,
   KVM and TCG named; BACKLOG row 436's recipe rewritten.
7. **Retirement in one landing**: the registry entry and `present`'s
   permit gone; C7 FIRED; a planted read of `ferrix.fastpath` refused by
   `check-build-flags.py`, FIRED; `git grep ferrix.fastpath` empty outside
   dated history, §1.4's reword list included; the PO told the same day to
   change the fleet gate lists outside the repository.
8. **Coverage and audit**: §1.5's per-profile coverage with every removed
   anchor listed by file:line and checked to lie inside a cfg'd item;
   `carry-coverage`, then `gen-coverage-justification --check` after the
   final rebase, the line-keyed TRACEABILITY rows re-read (F-62);
   SECURITY-TARGET's and AUDIT's start-up record lists name `ipc_fastpath`
   from `Config::Build`.
9. **A code review before `land.sh take`** (`trap.rs`,
   `arch/x86_64/syscall.rs`, `sched/**`, `object/**` core; `fastpath.rs`,
   `main.rs`, `syscall/native.rs` item), after part A lands; one author, no
   co-author trailer.

No FINDINGS id. Internal review, not independent verification (F-27).

---

## 2. Zenbleed's and GDS's fixes leave `mitigations` (DF6)

### 2.1 What there is today

`src/kernel/src/arch/x86_64/speculation.rs`:

* `vector_leak` (line 460), called from `arch/x86_64/mod.rs:325` before
  `XCR0` is written, returns `VectorLeak::SwitchOff` at once when
  `!HARDENED`; otherwise `decide_vector_leak(&vector_facts(&offered()))`
  and, for `ZenbleedChickenBit` or `GdsMicrocodeEnabled`, records the plan
  (`PLAN_ZENBLEED_BIT`, `PLAN_GDS_ENABLE`) and covers the boot processor
  (`set_zenbleed_bit`, `enable_gds_mitigation`), falling to `Uncovered` if
  it does not read back.
* `cpu::enable_extended_state(leak.allows_avx())` leaves AVX out of `XCR0`
  for an `Uncovered` verdict; the boot line `cpu      program register
  state: ...; vector registers: <describe>` names it
  (`SwitchOff` reads "not decided: built with --mitigations off").
* The secondaries: `apply_this_cpu` (line 664) returns at once when
  `!HARDENED`, so `apply_vector_cover` (line 652) never runs on them in an
  `off` build; `init` (line 551) likewise.
* **L.x86_64.123** opens "With the speculation switch on, vector_leak
  shall ..."; verified by `speculation/check.rs::vector_leak_decisions`
  (twelve processors), which speculation's `check()` already calls in every
  build, from `stages_check.rs:812` (only `clear_cpu_buffers` is under
  `HARDENED`; 1012 f1).
* `docs/certification/SPECULATION.md` §1: "how one build switch takes all of them out", and
  "What `off` removes"; SAFETY-MANUAL.md AoU-11 (lines 276 to 280): the
  `program register state` line must not name Zenbleed or GDS as holding
  AVX back unless accepted; a guest only on a host that mitigates both.
* On a `mitigations=off` build today, a Zen 2 host's programs read each
  other's vector registers (Zenbleed), and an affected Intel part whose
  firmware left GDS's mitigation off samples them (GDS). The ledger (926)
  says both fixes cost nothing measurable, so they are not a speed trade.

### 2.2 The change

1. `vector_leak` decides and covers in every build: the `!HARDENED` return
   goes, and `VectorLeak::SwitchOff` with it.
2. `apply_this_cpu` runs `apply_vector_cover` before its `!HARDENED`
   return, and records `Defences::NONE`, or `NONE.with(READ_BACK_FAILED)`
   when the cover did not hold (condition 1). `speculation_check`'s rule at
   line 96 then stops the boot, ahead of its "applied == NONE" rule at line
   102: a failed cover on a secondary in an `off` build is a stopped boot,
   as in an `on` build.
3. The `off` build therefore also withholds AVX on an `Uncovered` part,
   failing closed, and its line names the verdict (Q1's answer).
4. *No change* (1012 f1): `vector_leak_decisions` already runs in every
   build. D2 stays as its existing control, fired once in an `off` build.
5. Restated with it (condition 2): the flag file's `mitigations` `why`,
   BUILD-FLAGS.md §3's row, and `docs/certification/SPECULATION.md` §1
   ("one switch takes all of them out" becomes "every speed-trade defence";
   "What `off` removes" gains "except Zenbleed's and GDS's covers, §3, which
   stay"), §3's vector row and §9; AoU-11's Zenbleed and GDS sentence gains
   "in every build, `mitigations=off` included"; the `off` boot's line
   "speculation defences off: built with --mitigations off"
   (`speculation.rs` `init`) names the kept covers; ITEM.md §5's "compiles
   every defence out", and SAFETY-MANUAL AoU-8 where it says the same;
   `speculation_check`'s doc ("nothing in an unhardened one") says the
   vector cover is decided apart from `Defences`.
6. **L.x86_64.123** restated: "vector_leak shall, in every build, ..."; its
   criterion adds "and the same verdicts in a kernel built with
   mitigations=off, whose boot names one".
7. A check that an `off` boot's `vector registers:` line names a verdict
   and never "not decided": in `cargo xtask test-flags` with `--set
   mitigations=off --no-claim` on x86-64 under KVM.

AArch64 and ARMv7-A have neither leak and change nothing.

### 2.3 Controls

* D1 the `off` boot with `vector_leak`'s `!HARDENED` return put back: the
  line check fails ("not decided"). Fires on nazuna (Zen 5, `NotAffected`).
* D2 `vector_leak_decisions`' existing control (the guest rule taken out)
  fired once in the `off` build.
* The writes (`DE_CFG[9]`, clearing `GDS_MITG_DIS`) and the secondaries'
  cover stay unexercised on the gate's machines, as L.x86_64.123's criterion
  already says; the `off` path through `apply_this_cpu` is argued the same
  way.
* *Condition 3:* D1 FIRED by `gate.sh control --expect` on the `--set
  mitigations=off --no-claim` variant under KVM, and the `test-flags` `off`
  boot `run: PASSED` on the landing hash; D2 FIRED in the `off` build; the
  writes and the `off` path through `apply_this_cpu` argued as unexercised
  hardware in `coverage-argued-x86_64.json` and COVERAGE-RESIDUAL;
  `carry-coverage` and `gen-coverage-justification --check` after the final
  rebase (`speculation.rs` is core).

### 2.4 Size

Code 1; the boot-line check and its control 1 to 1.5; requirement,
SPECULATION.md, SAFETY-MANUAL.md, BUILD-FLAGS.md row, the flag file 1 to
1.5; gating 1 to 2. **4 to 6.**

### 2.5 Questions for the consultant (DF6), and the answers (ledger 1012)

1. **AVX in an `off` build on an uncovered part.** Accept one behaviour in
   every build, or should `off` keep AVX there and name the leak?
   *Answer: accepted, one behaviour in every build. An `off` build on an
   uncovered part withholds AVX, failing closed, and its line names the
   verdict; `off` is a speed build and the covers cost nothing.*
2. **L.x86_64.123** restated in place rather than a new id?
   *Answer: in place. The subject, `vector_leak`'s decisions, is unchanged;
   only the precondition widens.*
3. **The flag's meaning.** Is §2.2's wording the restatement DF6 asks for?
   *Answer: yes, with the further sites of condition 2 (§2.2 (5)).*
4. **Evidence.** Is D1 on a host where neither leak applies, plus D2, enough,
   with the writes and the secondaries' cover argued?
   *Answer: yes, with condition 3 (§2.3).*

### 2.6 The conditions (ledger 1012, OK IF (1) to (4))

1. An `off` build's `apply_this_cpu` runs `apply_vector_cover` first and
   records `NONE` or `NONE.with(READ_BACK_FAILED)`; `speculation_check`'s
   line-96 rule then stops the boot ahead of line 102's; the plan says so
   (§2.2 (2)).
2. Restated with it: §2.2 (5)'s texts, the `off` boot's defences line,
   ITEM.md §5, AoU-8, `speculation_check`'s doc, and L.x86_64.123's
   statement and criterion as §2.2 (6).
3. Evidence as §2.3 says.
4. A code review before `land.sh take`; no new `unsafe`
   (`set_zenbleed_bit`'s and `enable_gds_mitigation`'s SAFETY lines carry
   unchanged); one author.

No FINDINGS id. Internal review, not independent verification (F-27).

---

## 3. Plan: `smt=off` (DF4)

`max` only; `fast`, `desktop` and `present` keep `stibp`. Until M5 `max`
does not build, so `off` is built as an ad-hoc `--set smt=off --no-claim`
variant on x86-64 by `xtask check` (as `checks=off` is) and booted by
`test-flags`, so it cannot rot.

**Today.** `smp::start_secondaries` (`src/kernel/src/smp.rs:392`) starts
every processor the firmware lists: `arch::x86_64::smp::describe_cpus`
takes each enabled MADT local APIC and x2APIC entry, and `CpuStarter::start`
sends INIT and two SIPIs to each. No CPUID topology leaf is read anywhere in
the kernel, and **the kernel never sets `CR4.MCE`** (`arch/x86_64/cpu.rs`
sets PGE, SMEP, UMIP, SMAP, FSGSBASE and OSXSAVE; the #MC vector, its IST
slot and `paranoid.rs`'s fatal handler exist). What decides the bit is the
firmware (1018 f1): the UEFI loader keeps the firmware's CR4 and ORs in PGE
alone (`src/boot/common/uefi/src/arch/x86_64.rs:43-51`), and every
secondary adopts the boot processor's CR4 (`arch/x86_64/smp.rs:287-298`
snapshot, `404-409` write). So `CR4.MCE` is whatever the firmware left at
the exit from boot services, on every processor alike: never ensured,
checked, recorded or assumed. QEMU is started with `-smp N` alone
(`tools/common/xtask/src/qemu.rs:2282`), one thread per core.

**F-73 goes first** (ledger 1018: Moderate, the certified item, x86-64,
pre-existing since the x86-64 bring-up). The fail-stop on a detected
hardware error (H.FAIL) holds only where firmware left `CR4.MCE` set; where
it did not, an uncorrectable error puts the processor into shutdown, and an
Intel machine-check broadcast takes every processor there at once. The fix
is its own small item change, made by another agent on branch **`m0-f73`**
and landed **before** `smt=off`: `cpu.rs` sets `CR4.MCE` on the boot
processor and every secondary in every build and profile, with a stage-4
check reading it on every processor, a new L.x86_64 id from 175 reserved
first, a gate boot injecting a machine check through QEMU's monitor (`mce`)
that requires FX-9005 on serial, and a control that FIRES. `smt=off`'s
parking then finds `CR4.MCE` already set and only checks it.

**The work.**

1. **Topology** (`arch/x86_64/smp.rs`, a new `topology` function beside
   `describe_cpus`): CPUID leaf 0x1F where the maximum basic leaf reaches it
   and subleaf 0 reports a level type, else leaf 0xB; the SMT level
   (type 1)'s `EAX[4:0]` is the shift that separates a thread's bits from the
   core's in the x2APIC ID. `Described` gains each id's thread number
   (`id & ((1 << shift) - 1)`). With neither leaf: one thread per core if
   `CPUID.1:EDX.HTT` is clear, otherwise the boot stops (fail closed, a
   catalogued panic, accepted at 1018), since `smt=off` cannot be shown.
   Each started thread compares its own leaf's x2APIC ID (`EDX`) with the
   MADT's (P3: each thread's own leaf, as planned).
2. **Parking**, one path on AMD and Intel (P2's answer, as Linux's `nosmt`
   brings every sibling up once whatever the vendor). Under `smt=off`,
   `start_secondaries` starts every thread whose number is not 0 with a
   "park" argument: it runs the secondary path up to its IDT, GDT, TSS and
   per-CPU record (so an NMI or #MC finds a valid GS), with `CR4.MCE`
   already set by F-73's fix, reports parked, and loops `cli; hlt` for ever,
   touching only image text, its own stack and its record, none of which is
   ever unmapped. Parked threads are kept in `Topology` apart from the
   online ones, so `smp::count()`, `online()`, shootdowns, MSI targets and
   the scheduler never see them. One kernel stack each. Firmware that
   already hides the siblings (every MADT id is thread 0) is accepted and
   named on the line. *(1018, f2 and comment a)*: **the broadcast IPI does
   reach them**: `send_ipi_to_others` (`arch/x86_64/apic.rs:508-517`) uses
   the ALL_BUT_SELF shorthand. A parked thread keeps IF=0 for good, so the
   fixed IPI stays pending and harmless. An NMI does reach it, so **its NMI
   path returns to the park loop**; and **no stop or rendezvous may count
   parked threads** (the panic stop, `run_everywhere`, shootdowns): the plan
   names both, and the code shows each such site.
3. **`CR4.MCE`** is F-73's, on its own branch, landed first (above). Here
   the stage-4 check of 4 also requires it on each parked thread.
4. **The boot check** (stage 4, `smp.rs` beside `check_this_cpu`, a new
   catalogue entry in `panic/catalog.rs`): under `smt=off`, no two online
   processors share a core id (`apic_id >> shift`), every thread not 0 is
   parked, and each parked one reported in. Boot line: `smt      off: 4
   cores, 4 siblings parked by the kernel (CR4.MCE set)`, or `off: firmware
   runs one thread per core`. Under `stibp` the line counts threads per core
   and says `stibp`.
5. **AArch64 and ARMv7-A** (DF4 b): a check only. Topology from the device
   tree's `cpu-map` (`thread` nodes) or ACPI's PPTT, never `MPIDR_EL1.MT`
   (which reads 1 on the A78AE with no SMT). Two threads of one core under
   `smt=off` stop the boot; none is the expected case on every board.
   *(1018, comment b)*: x86-64's `smt=off` carries no claim (`max` claims
   AArch64), so it is built and booted against rot; **the claim-bearing
   part is this AArch64 check** (DF4 b and c), and its negative control,
   two threads of one core in the device tree's `cpu-map` under QEMU
   `virt`, is owed with the code, beside S1 and S2.
6. **xtask**: `--smp-threads <T>` passing `-smp N,cores=N/T,threads=T` to
   QEMU, for the check and its control.
7. **Requirements**: new L.x86_64 ids (topology, parking, the check) and
   one each on AArch64 and ARMv7-A, reserved in
   `requirement-reservations.json` before they are written (the next free
   L.x86_64 id is 175 on `c0bded3c7`); SPECULATION.md §3 and AoU-11 (the
   integrator's "disable SMT on MDS-affected parts" becomes "or build
   `smt=off`").

**Checks and controls.** A `--set smt=off --no-claim --smp 4 --smp-threads
2` boot under KVM and TCG: two cores, two parked. Control S1: the thread
test made always true for the first sibling, so one sibling starts for work;
the stage-4 check fires (FERRIX-PANIC, its catalogue name). Control S2: a
parked thread with `CR4.MCE` cleared; the check reads each parked thread's
`CR4` as it reports and fires. Control S3: the AArch64 `cpu-map` with two
threads of one core (comment b). A `stibp` boot with `--smp-threads 2`
shows four online.

**Answered at 1018.** P1: a real defect, F-73, fixed on `m0-f73` in every
build and profile and landed first. P2: one path on both vendors. P3:
each thread's own leaf.

**Size**: topology 2; parking and the topology split 3 to 4; the check and
line 1.5; Arm checks and their control 1.5; xtask 0.5; requirements and
docs 1 to 1.5; gating and controls 1.5. **10 to 13** (F-73's fix on
`m0-f73` not included).

### 3.1 The plan restated for code (2026-10-19, branch `mk/smt-off`)

Written on F-73's rebased fix (`mk/f73`, `1a2cb3485`, on `main`
`8cb1dc24a`), which `smt=off` lands after; asked for the code review the
consultant's pre-read of 2026-10-19 (ledger 2458, M7) owes before code.
What changed since line 1020, and what the code will do where the plan
above left a choice:

* **The secondary path a parked thread runs is F-73's and cr0-ne's.**
  `secondary_start` now sets `CR0`'s x87 controls, loads its GDT and TSS,
  sets `CR4.MCE` (`cpu::enable_machine_check`, counted in
  `MACHINE_CHECK_ENABLED`) and only then reaches `apic::init_this_cpu`. A
  parked thread takes that path unchanged up to the `CR4.MCE` set and
  leaves it there: it never runs `apic::init_this_cpu`, the timer, the
  scheduler or `smp::secondary_main`. Stage 4's `mce` count then equals
  online plus parked threads, and the `mce` line says both.
* **Where a parked thread's record lives.** `Topology` gains a second
  leaked slice, `parked: &'static [PerCpu]`, the same record type, so the
  thread's `GS` base (the per-CPU register) names a real record and an NMI
  or `#MC` on it finds one. `Topology::cpus()`, `count()` and `online()`
  stay over the online slice alone, so every user of them (logical
  numbers, per-CPU arrays, `run_everywhere`'s wait, `stop_others`' count,
  shootdowns, the scheduler's queues, MSI targets) never sees a parked
  thread without a change at its site. `record_at` (the start's record
  check) and `this_cpu_for_report` search both slices, and a report from a
  parked thread names it as "parked thread n of core c".
* **The sites a parked thread could be counted in**, each read for this
  plan: `run_everywhere` (waits on `cpus` filtered by `is_online`),
  `stop_others` (`online() - 1`), the shootdown's acknowledgement wait
  (over `cpus`), the stage-4 checks that `run_everywhere` (`check_this_cpu`,
  x87, `mce`), and the panic report's stopped count. None changes. The
  broadcast fixed IPI (`ICR_ALL_BUT_SELF`, `apic.rs:659`) does reach a
  parked thread; it has `IF=0` for good and no local APIC set up, so the
  interrupt stays pending, as line 1020 (a) accepted. An NMI returns
  through the paranoid entry to the `cli; hlt` loop, which re-halts.
* **The park loop** is a plain Rust function in
  `arch/x86_64/smp.rs`, `park(record) -> !`: it writes its `CR4` and its
  x2APIC id into its record, sets `parked` with Release, and loops
  `cli; hlt` (the existing `halt` helper). It allocates nothing and takes no
  lock, so nothing it could hold is ever waited on.
* **Topology** as planned (leaf 0x1F, else 0xB, else `HTT` clear means one
  thread a core, else a catalogued stop), read on the boot processor
  before `start_secondaries`; each started thread compares its own leaf's
  x2APIC id with the MADT's (P3), on a mismatch the stage-4 check fails by
  name. AMD's leaf 0xB is used where 0x1F is absent (Zen 5 has both).
* **The boot check, DF4 (c)**, in stage 4 beside `check_this_cpu`: from
  each online processor's own leaf, the number of online processors per
  core id (`x2apic_id >> shift`) must be 1 under `smt=off`; every thread
  with a thread number above 0 must be in `parked` and have reported in,
  with `CR4.MCE` set in what it wrote. Line: `smt      off: 2 cores, 2
  siblings parked by the kernel (CR4.MCE set)`, or `off: firmware runs one
  thread per core`; under `stibp`, `stibp: 2 cores, 2 threads a core`.
* **Arm, DF4 (b), check only.** AArch64's reference boot (UEFI, QEMU
  `virt`) describes processors in ACPI, so its topology comes from the
  PPTT (a processor node whose flags say "is a thread", and its parent
  core); a device-tree boot uses `/cpus/cpu-map`'s `thread` nodes. ARMv7-A
  reads `cpu-map` (its boots are device-tree only). A PPTT reader is new
  (`discovery::acpi`, bounded walk, no allocation beyond one entry per
  processor) and `ferrix-fdt` gains a `cpu-map` walk with host tests.
  Under `smt=off`, two threads of one core stop the boot at stage 4 by
  name; with no topology table the check passes as one thread a core and
  says "no topology table". `MPIDR.MT` is never read.
* **The controls.** S1 (x86-64, KVM and TCG): `smt=off`'s thread test made
  to start the first sibling for work; stage 4 must fire on "two online
  processors share core c". S2 (x86-64): the parked path's
  `enable_machine_check` call skipped; stage 4 must fire on "parked thread
  n reads CR4.MCE clear". S3 (AArch64, QEMU `virt` with `-smp
  4,cores=2,threads=2`, so the PPTT has two threads a core): stage 4 must
  fire on two threads of one core. S4 (ARMv7-A, the same through
  `cpu-map`). Positives: a `--set smt=off --no-claim --smp 4
  --smp-threads 2` boot under KVM and TCG with two online and two parked;
  the same build with `--smp-threads 1` ("firmware runs one thread per
  core"); `stibp` with `--smp-threads 2` showing four online; every
  existing row unchanged (no `--smp-threads`, one thread a core).
* **Ids**, to be reserved by the product owner before code:
  `L.x86_64.183` (topology from the CPUID leaves, and the fail-closed
  stop), `L.x86_64.184` (parking: started, set up to `CR4.MCE`, never
  counted online), `L.x86_64.185` (the stage-4 check: one thread a core
  online, every sibling parked with `CR4.MCE` set), `L.aarch64.65` (PPTT
  or `cpu-map`: no two threads of one core online under `smt=off`),
  `L.armv7a.27` (the same through `cpu-map`), reserved on `main` at
  f5c048964 with their parent `H.TRAP.19` under O.ISOLATE (ledger 2563
  (4): not `H.TRAP.3`).
* **`build-flags.toml`**: `smt.off` goes to `built = "yes"`; `max` and
  `fast` already name it, so `test-flags` builds and boots the
  `--set smt=off --no-claim` variant on x86-64 from `xtask check`, as
  `checks=off` is.

**Reviewed, ledger 2563: OK IF (1)-(8)**, owed with the code: (1) list
everything a parked thread can run (park, NMI, `#MC`/FX-9005, the panic
report) and show none indexes a per-CPU array sized for the online
processors, takes a lock, allocates or touches the local APIC; (2) a
machine check on a parked thread: say who reports it, and show FX-9005 by
`test-mce` under `--set smt=off --no-claim --smp 4 --smp-threads 2` (KVM)
injected into a parked thread, and that the panic stop's NMI leaves a
parked thread parked and counted nowhere; (3) Arm fails closed: "no
topology table" passes only where `MPIDR.MT` reads 0, a malformed PPTT or
`cpu-map` stops the boot, both walks bounded with host tests on malformed
fixtures, under F-23 and F-26; (4) parent `H.TRAP.19`; (5) the parked
thread's local APIC mode: cite the SDM on mixed xAPIC/x2APIC for the
broadcast IPI and NMIs, or put it in the boot processor's mode with every
LVT masked before parking; (6) controls S1-S4 and an AArch64 no-table
control (table withheld, `MT` forced to 1), positives, and the four
test-boot rows plus armv7a `--smp 2` on the landing hash; (7) lands after
F-73 closes on `main`; (8) SPECULATION.md §3 and AoU-11, build-flags.toml
`built = "yes"` with `test-flags`, MEMORY-AND-TIMING, coverage of the park
path and readers, carry and justification after the final rebase.

**Size, restated**: the plan's 10 to 13, plus the PPTT reader and its
tests (1.5): **11.5 to 14.5** points, about 5 to 7 hours of work and the
gate time on top.

### 3.2 The code, and ledger 2563's conditions (2026-10-21, branch `mk/smt-off-code`)

**(1) What a parked thread runs.** `arch::x86_64::smp::secondary_start`
asks `smp::parked_record(record)` first and, for a parked thread, goes to
`park`: `apic::check::put_first_in_x2apic` and `apic::enter_mode` (the
mode check every started processor makes, counted by check R8 among the
processors started), `write_cr3` of the kernel's root, the boot
processor's `CR4` without `MCE`, `trap::load_on_this_cpu`,
`smp::install_secondary_record`, `gdt::init_secondary` (its kernel stack
and interrupt stacks, allocated once at boot), `cpu::enable_machine_check`,
`smp::report_parked` and `halt`. It skips everything a processor that runs
work is counted by: the PAT, `FSGSBASE`, the x87 controls, `XCR0`, HWP, the
speculation defences' application, `apic::init_this_cpu` and
`smp::secondary_main`. After it parks it can run two things:
* **An NMI**: `ferrix_paranoid_entry`'s NMI arm, `note_gs` (which walks
  the records, `cpus` and `parked`, comparing addresses, with no lock, no
  allocation, no index) and two counter increments, then the return to the
  halt.
* **A machine check**: `trap::fatal` → `panic::begin_report` (one
  atomic), the console's lock (the one every report takes), `conclude`:
  `this_cpu_for_report` (the same walk; it names the thread "processor 3
  (hardware id 0x3)", its logical number past `count()`), `stop_others`
  (counts `online()` over `cpus` alone; sends the fixed IPI through the
  ICR, which a local APIC software-disabled since INIT may still send,
  Intel SDM Vol. 3A §11.4.7.2), the console's drain and the pstore
  write-back, and `halt`.
None of them indexes an array by the thread's logical number: every array
sized for the processors is indexed by `cpus`' logical numbers, which the
parked records' numbers (`count()` and up) never are, and `smp::record(n)`
reads `cpus` alone, so a parked number would read `None`, never another
processor's record.

**(2) A machine check on a parked thread** is reported from the parked
thread itself, as above. `test-mce` with `--smp-threads` injects into
QEMU's last processor alone, the last thread of the last core, which
`smt=off` parks; locally under TCG it reported "on processor 3 (hardware
id 0x3)" and FX-9005. The panic stop is not an NMI on x86-64: `stop_others`
sends the fixed IPI (`ICR_ALL_BUT_SELF`), which a parked thread, with
`IF=0` for good, leaves pending, and counts only `online()`.

**(3) Arm, fail closed.** `arch::arm_common::smt::record` stops the boot under
`smt=off` on a malformed PPTT or `cpu-map` (`describe_cpus`' error);
`arm_common::smt::check::check_arm_smt` passes "no topology table" only where every
online processor reads `MPIDR.MT` clear. The PPTT walk
(`ferrix_acpi::pptt`) is bounded by the table's length over the 20-byte
node size, refuses a parent outside the table, inside a node, a loop or a
thread with no parent, and allocates nothing (nine host tests, the
malformed shapes among them); `ferrix_fdt::cpu_map` walks the tree's own
bounded walk, refuses a thread outside a core or naming no processor, and
allocates nothing (seven host tests). AArch64's reference boot reads the
PPTT (QEMU `virt` publishes one); the line reads "the PPTT lists no
threads, one a core" on today's rows.

**(5) The local APIC's mode.** The parked thread runs `apic::enter_mode`,
so its APIC is in the boot processor's mode (x2APIC or xAPIC, as check R8
says for every started processor), and never runs `apic::init_this_cpu`,
so every local vector stays masked as INIT left them (Intel SDM Vol. 3A
§11.4.7.1 and §11.4.7.3: after INIT the LVT entries' mask bits are set and
the APIC is software-disabled). No mixed-mode system is made.

**(6) Controls**, each through `gate.sh control --expect`:
* S1, `to_park`'s filter made to start the first sibling for work
  (`!= 0` → `!= 0 && *id != 1`): "two online processors share a core";
* S2, `park`'s `enable_machine_check` call removed: "a parked hardware
  thread reads CR4.MCE clear";
* S3, AArch64, xtask's `smp_topology` forced to two threads a core under a
  plain `smt=off` boot: "a core lists two threads";
* S4, the same on ARMv7-A through `cpu-map`;
* S5, AArch64, `check_arm_smt` made to see no table and an `MT` set (one
  edit): "no topology table, and a processor's MPIDR.MT is set".

**(7)** F-73 landed first (d9c93c7c9). **(8)** SPECULATION.md's SMT
paragraph and SAFETY-MANUAL's AoU-11 name `smt=off`; build-flags.toml has
`smt.off` built, `xtask check` lints the variant on every architecture,
and gate-rows adds the variant's rows for changes to the parking code;
MEMORY-AND-TIMING §2.2t; `coverage-owed.json` names the new code.

---

## 4. Plan: `ssbd=on_request` (DF3)

`desktop` only; every other profile keeps `always`. `PR_GET_SPECULATION_CTRL`
answers truthfully in every profile.

**Today.** `Plan::for_processor` (`arch/x86_64/speculation.rs:198`; its
SSBD branch at line 223) sets `SPEC_CTRL_SSBD` in `PLAN_SPEC_CTRL`, or AMD's `VIRT_SPEC_CTRL`
(`PLAN_VIRT_SSBD`), once per processor at boot; nothing writes either at a
switch. AArch64 (`arch/aarch64/speculation.rs:387-407`) uses `PSTATE.SSBS`
where the core has it, else firmware's `ARCH_WORKAROUND_2`. `prctl`
(`syscall/attributes.rs`, load ring) answers the options of its `option`
module (and seccomp's, through `syscall/seccomp.rs`) and `EINVAL` to the
rest, so `PR_GET_SPECULATION_CTRL` (52) and `PR_SET_SPECULATION_CTRL`
(53) are `EINVAL` in every build today: a program that asks is told nothing
about a defence that is on. A fidelity gap, not a finding (1018): SSBD is on
regardless under `always`, so nothing is exposed. Values, from nazuna's
`/usr/include/linux/prctl.h`: `PR_SPEC_STORE_BYPASS` 0,
`PR_SPEC_INDIRECT_BRANCH` 1, `PR_SPEC_L1D_FLUSH` 2; `PR_SPEC_NOT_AFFECTED`
0, `PR_SPEC_PRCTL` 1, `PR_SPEC_ENABLE` 2, `PR_SPEC_DISABLE` 4,
`PR_SPEC_FORCE_DISABLE` 8, `PR_SPEC_DISABLE_NOEXEC` 16.

**The work.**

1. **The thread's setting** (core): a word on the task (`sched/task.rs`)
   holding Linux's three states for store bypass (disabled, forced,
   disabled until `execve`), copied by `fork` and `clone`, the until-exec
   bit cleared by `execve`, as Linux does.
2. **The write at a switch, only on change**, under
   `cfg(ferrix_ssbd = "on_request")`: a per-processor shadow of the
   SSBD bit last written; at the switch, if the incoming thread's bit
   differs, write `IA32_SPEC_CTRL` as `PLAN_SPEC_CTRL` (its IBRS and STIBP
   bits kept) with SSBD set or clear, or `VIRT_SPEC_CTRL` under AMD's KVM.
   It goes where both the general switch and the fast path's direct switch
   pass (`arch::x86_64::switch::switch_to` or `restore_user_state`, the
   one call in `sched/mod.rs:2475`; the code confirms which), so the direct
   switch cannot skip it. With `always`, `Plan` is unchanged and no switch
   code is compiled. *(1018, comment a)*: the per-processor shadow becomes
   **the only writer of `IA32_SPEC_CTRL` after boot**, so later STIBP
   trades (game mode) go through it too.
3. **AArch64**: with `SSBS`, the thread's saved `SPSR.SSBS` per thread;
   with `ARCH_WORKAROUND_2` only, the firmware call at a switch on change.
   ARMv7-A: no control; it answers as below. *(1018, comment c)*: on
   AArch64 the kernel's own SSBS comes from `SCTLR_EL1.DSSBS` on entry
   (`arch/aarch64/speculation.rs:188-196`), unlike x86-64, where the kernel
   runs with the thread's setting; SPECULATION.md names the difference per
   architecture.
4. **`prctl`** (load ring, `syscall/attributes.rs`), through a core
   function that reads the thread's word and the processor's verdict:
   * GET `PR_SPEC_STORE_BYPASS`: `NOT_AFFECTED` where the processor reports
     `SSB_NO` (or Arm lists the core unaffected); with `always` and SSBD in
     force, `PR_SPEC_DISABLE`; with `always` and none offered (AoU-11's
     uncovered case) or `mitigations=off`, `PR_SPEC_ENABLE`; with
     `on_request`, `PR_SPEC_PRCTL` and the thread's state.
   * SET: only under `on_request`; elsewhere refused as Linux refuses it
     outside its prctl mode. Arguments as Linux checks them (non-zero unused
     arguments refused; a forced disable cannot be undone).
   * `PR_SPEC_INDIRECT_BRANCH` (Q-S2's answer, 1018): GET outside every
     domain under `switch` is `PR_SPEC_DISABLE` (the barrier at every
     switch, Linux's strict mode); for a domain member,
     `PR_SPEC_PRCTL|PR_SPEC_ENABLE`; after a leave,
     `PR_SPEC_PRCTL|PR_SPEC_FORCE_DISABLE`. A leave is for good, so a later
     SET of ENABLE is `EPERM`, as Linux treats a forced disable. Where the
     processor needs no IBPB, answer as Linux's `ib_prctl_get` does. SET of
     DISABLE is taken in every `program_barrier` value (§5, Q-B2).
   * `PR_SPEC_L1D_FLUSH`: GET answers what is true, as Linux does without
     the flush.
   * **Every error value and the exact answer table are taken from Linux's
     `arch/x86/kernel/cpu/bugs.c` (`ssb_prctl_get`, `ssb_prctl_set`,
     `ib_prctl_get`) and `kernel/sys.c` before code**, not from this plan.
5. **The exposure, named**: SPECULATION.md gains `desktop`'s section, "the
   kernel runs with the current thread's setting, as Linux does", and the
   VM-exit cost of `VIRT_SPEC_CTRL` under KVM for a switch that changes it.
   *(1018, comment b)*: a switch to a kernel thread keeps the outgoing
   thread's SSBD; it is named there with §5's exposure.
6. **Requirements** (Q-S1's answer, 1018): new core ids for the
   write-on-change and inheritance, and the requirement for the truthful
   answer goes on **the core function that reads the thread's word and the
   processor's verdict** (core, ItemLowLevel). prctl's encoding of it stays
   load-ring code, with the native test as its evidence and no item id
   (`check-traceability` takes units only from core and item code).

**Checks and controls.** A stage-9 case (two threads, one asking for
`PR_SPEC_DISABLE`): the SSBD bit read back on the processor after each
switch matches the running thread; the count of writes equals the count of
changes. Control: the shadow compare removed (written every switch) fails
the count; the write skipped on the direct switch fails the read-back. A
native test program checking GET's answer in `present` (`PR_SPEC_DISABLE`
under KVM on nazuna) and in `desktop` (`PRCTL|ENABLE`, then
`PRCTL|DISABLE` after SET); control: GET answering `EINVAL` again.

**Answered at 1018.** Q-S1 (does the truthful answer need an item
requirement although `prctl` is in the load ring?): yes, on the core
function (item 6). Q-S2 (what `PR_SPEC_INDIRECT_BRANCH`'s GET says inside a
domain, and whether SET is accepted outside `session`): as item 4's
second bullet says; SET's DISABLE is taken in every value.

**Size**: the thread word and inheritance 1.5; the switch write on x86-64 2;
AArch64 1 to 1.5; prctl GET and SET 1.5 to 2; the stage-9 case and the
native test 1.5 to 2; docs and requirements 1; gating 1. **8 to 11.**

---

## 5. Plan: `program_barrier=session` (DF2)

`desktop` only, at level `none`; every certified value keeps `switch`
unchanged. **Waits on F-64** (FINDINGS.md: `forget_root` clearing a
processor's `LAST_DOMAIN` after its `LAST_ROOT`, which can erase a domain
just recorded; branch `m0-f64`, WIP `533f20daa` at the review), because
`session` makes domain leaves common. After §4, since both change
`syscall/attributes.rs`'s speculation answers.

**Today.** A domain is one marked job: `Job::new_child_domain`
(`object/job.rs:470`) gives a fresh id; `new_child` and `new_named_child`
(cgroupfs' `mkdir`, line 509) give 0, so a child job of a marked job is
**not** in its domain (`domain_check.rs` case 10 asserts it). A running
process that moves to another job leaves its domain for good
(`Process::move_to`, `object/process.rs:560-576`). Losing dumpability
leaves it (`attributes.rs:169`). Marking needs MANAGE on the parent
(`syscall/native.rs:1683`, `JOB_SPECULATION_DOMAIN`).

**The work**, each under `cfg(ferrix_program_barrier = "session")`:

1. (a) **The subtree.** `bare_child` alone, under the session cfg, gives a
   child job its parent's domain (`new_child` and `new_named_child`), so a
   session's cgroup jobs are in it; a marked job inside a marked one still
   starts its own domain (stricter). Jobs under the root (domain 0,
   `DRIVERS_JOB` among them) are unchanged (1018). `move_to` keeps a process
   in its domain only when the target job's domain is the one it holds, and
   leaves otherwise. A process moved in from outside never joins, and a
   process holding `LEFT` never rejoins by a move (Q-B3's answer: keep it,
   and add that case).
2. (b) Unmarked programs keep the barrier to and from everything: domain 0
   is never equal to any other, as today; a check, not new code.
3. (c) **Leaving on request.** Installing a seccomp filter
   (`syscall/seccomp.rs`, `prctl_set` and `seccomp(2)`'s filter mode) calls
   `leave_speculation_domain` **under `session` only**, as DF2 (c) says: under
   `switch` a domain is the MANAGE holder's deliberate mark, and an implied
   leave on every filter would change the 2026-10-01 rules for every unit
   that installs one. **The prctl leave**,
   `PR_SET_SPECULATION_CTRL(PR_SPEC_INDIRECT_BRANCH, PR_SPEC_DISABLE or
   PR_SPEC_FORCE_DISABLE)`, goes into **every** `program_barrier` value as a
   certified behaviour (Q-B2's answer, 1018): it adds no new trigger, since
   `PR_SET_DUMPABLE(0)` already leaves at the program's will
   (`attributes.rs:169`), it only adds barriers, and it makes §4's Q-S2
   answer true under `switch`. It needs its own core requirement and
   AoU-14's restatement, after F-64.
4. (d) F-64 fixed first (above).
5. (e) SPECULATION.md's `desktop` section names `session`'s exposure: a
   session's programs share predictors with each other.
6. **Who marks a session** (Q-B1's answer, 1018). Nothing in the Linux
   personality marks a job today: a session is a cgroup made by `mkdir`.
   The native launcher marks the session's job with
   `JOB_SPECULATION_DOMAIN` at its creation (`sessiond`,
   `src/user/system/linux/compositor/sessiond`, or a native launcher before
   it), with MANAGE on the parent and the DOMAIN audit event (AoU-14). A
   root-writable cgroupfs marking file is refused: it marks after creation
   and is a new load-ring route into a core decision.

**Checks and controls.** `domain_check.rs` gains a `session` variant of
cases 7 and 10 (a child job and a cgroup `mkdir` under a marked job share
its domain: no barrier between their members) and new cases for a move
within the subtree (stays), a move out (leaves, barrier owed), a `LEFT`
process moved back into its old subtree (stays out), a seccomp filter
(leaves, `session` only) and the prctl (leaves, in both builds). The
`switch` build keeps today's cases and gains the prctl case; both builds
run them. Controls: the subtree inheritance taken out (the session case
fails), the seccomp leave taken out (fails), the prctl leave taken out
(fails in both builds), and part A's planted `program_barrier=session` in
`max`, refused. DF8's check that `desktop`'s line names `session` is part
A's.

**Answered at 1018.** Q-B1: the native launcher (item 6). Q-B2: the prctl
leave in every value, the seccomp leave `session`-only (item 3). Q-B3:
never joins by a move, and `LEFT` never rejoins (item 1). §5 still waits on
F-64 and on §4.

**Size**: job inheritance and moves 2; the leaves on prctl and seccomp 1.5;
the marking launcher 1 to 1.5; the domain-check cases and controls 2 to 2.5;
docs 1; gating 1. **8 to 10** (F-64's fix not included).

---

## 6. Order and file ownership

```
F-73 (m0-f73, CR4.MCE) lands ── before §3
part A lands
  ├─ §1 ipc_fastpath   fastpath.rs, trap.rs, x86_64/syscall.rs, native.rs, sched/direct.rs,
  │                    channel.rs, main.rs's hook, xtask ipc.rs and args.rs, check-item-boundary.py
  ├─ §3 smt=off        smp.rs, x86_64/smp.rs, x86_64/cpu.rs, xtask qemu.rs (args.rs after §1)
  └─ §2 DF6            x86_64/speculation.rs (vector half), x86_64/mod.rs
       └─ §4 ssbd      x86_64/speculation.rs (switch half), switch.rs, sched/task.rs, attributes.rs
            └─ §5 session (after F-64)   job.rs, process.rs, seccomp.rs, attributes.rs, domain_check.rs
```

§1 and §3 can run beside §2; §2, §4 and §5 share files and go in that order.
Every one touches the certified item, `certification-item.json` or the
SysML model, so each goes through the consultant before `land.sh take`, and
each re-carries the line-keyed coverage evidence after the others land.
