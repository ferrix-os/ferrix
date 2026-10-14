# M0 part B: the item reviews and the plans

**For the certification consultant, 2026-10-14.** `docs/roadmap/microkernel-m0.md`
§B and its *Reviews* paragraph ask for two item reviews before any code:
the fast path's move from the boot option `ferrix.fastpath` to the build
flag `ipc_fastpath` (`docs/BUILD-FLAGS.md` §3, **BF6**), and Zenbleed's and
GDS's fixes leaving the `mitigations` flag (§3.1, **DF6**). §1 and §2 are
those two design notes, each ending with the questions the consultant is
asked to answer. §3 to §5 are implementation plans, not reviews, for the
other three values M0 builds: `smt=off`, `ssbd=on_request` and
`program_barrier=session`. Their designs passed in ledger line 931 (DF2 to
DF4); the plans name the files, the checks and the controls, and list what
the code will have to decide.

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
| §3 `smt=off` | plan | part A | 10 to 13 |
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
  the stage-9 cases in `object/fast_path_check.rs`, and case 15 in
  `arch/x86_64/switch/check.rs:1397`, which also asks
  `trap::fast_write_read().is_some()`.
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
   on (`native.rs:959`, `switch/check.rs:1397`) read a constant,
   `fastpath::BUILT: bool = cfg!(ferrix_ipc_fastpath = "on") &&
   arch::FAST_WRITE_READ`.
2. **`FAST_PATH_ON` goes.** With the code absent in an `off` build and
   present only in an `on` build, the run-time flag has nothing left to
   decide: the entry calls the fast path for 0x1013 whenever it was built
   with it. `trap::set_fast_write_read`, `fast_path_on` and the Acquire load
   at every entry are removed. This is the core and architecture change
   BUILD-FLAGS.md §3 asks about ("whether the system-call entry's flag test
   compiles out"): it does, on both sides. One load less on every x86-64
   system call in an `on` build; no test at all in an `off` build.
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
   audit record names it; no record of its own is added.

### 1.3 The requirements restated

Requirement text for the implementer to write after this review; the ids
stay, no new ids are needed.

* **L.x86_64.150**: "fastpath::report shall print, at stage 9 or where a
  boot skips stage 9, which path channel_write_read takes, from the cfg the
  kernel was built with: on only for ipc_fastpath=on on x86-64." Criterion:
  the `off` and `on` boots of `test-ipc-equiv` print `off (built with
  ipc_fastpath=off)` and `on (built with ipc_fastpath=on)`; an Arm boot
  built `on` says x86-64 only. Units: `fastpath::report`. (Retiring the id
  instead is the other choice; Q2.)
* **L.x86_64.159**: "In a kernel built with ipc_fastpath=on,
  ferrix_syscall_entry shall call the fast path for 0x1013 and for no other
  number, before the filter and with interrupts masked, after every entry
  measure the stub makes, raising the vector mark first and lowering it on a
  decline; in a kernel built with ipc_fastpath=off the entry shall contain
  no call of it." Criterion: the `on` boot's trips; the `off` image holds no
  `ferrix_fast_write_read` symbol (1.5, C6). Units drop `trap::fast_path_on`.
* **L.x86_64.161**: the flag clause becomes "the fast path only in a kernel
  built with ipc_fastpath=on, through a hook declared and defined under that
  cfg"; the predicate's clause is unchanged. Criterion adds the cfg
  mismatch self-test case.
* **L.x86_64.160**: criterion's "with the fast path on" becomes "in the
  `on` build".
* `docs/sysml/13-item-requirements.sysml:446` ("behind ferrix.fastpath")
  and `15-sched-requirements.sysml:538` are reworded to the flag.

### 1.4 Every gate row that passes `ferrix.fastpath` today

Found in the tree, in the batch runner's history
(`~/.local/share/ferrix/fleet/batch/runs/*/gates`, `full/*.cmd`) and in the
fleet's gate files. Counts are the number of batch runs that carried the
row.

| Row | Where | Becomes |
|---|---|---|
| R1 `test-boot --arch x86_64 --accel kvm --kernel-option ferrix.fastpath=on` | 22 batch runs (IPC, scheduler and object landings) | `test-boot --arch x86_64 --accel kvm --set ipc_fastpath=on --no-claim` |
| R2 `test-boot --arch x86_64 --accel tcg --kernel-option ferrix.fastpath=on` | 22 batch runs; BACKLOG row 436 (po10-f65, stage 13's cgroupfs check under TCG at load 34, open) | the same with `--accel tcg`; row 436's reproduction recipe rewritten |
| R3 `test-boot --arch x86_64 --release --accel kvm --kernel-option ferrix.fastpath=on` | 19 batch runs; `fleet/po9-fx0902-gate.txt` | `--release ... --set ipc_fastpath=on --no-claim` |
| R4 the same with `--smp 2` | 19 batch runs; `fleet/po9-fx0902-gate.txt` | the same with `--smp 2` |
| R5 `test-ipc-equiv --arch all` and `--arch x86_64 --accel tcg` | `ipc.rs:621` writes `ferrix.fastpath={off,on}` itself; `fleet/po9-sched-{a,b}-gate.txt`, batch runs | two images, 1.5 |
| R6 `bench-ipc --kernel-option ferrix.fastpath=on`, and `bench-ipc --alternate`, which forwards the options to the other tree | measurements, not gate rows: `docs/hotpaths/results/ipc-round-trip/6bc2646211ca/` (7 records of 2026-10-07 name the option in `kernel_options`); `ipc.rs:202`, `other_tree_args` at 337 to 353 and its unit test at 706 to 724 | `--set ipc_fastpath=on --no-claim`; the record names the flag set (part A's `hotpath/record.rs` change); `--alternate` against a tree from before the move passes the option, against one after passes `--set` (the other tree's xtask decides which) |
| R7 owed: a stage-13 boot with the fast path on (`docs/CGROUPS.md:782`, BACKLOG row 591's F9) | not yet a row | written as R2's form when it is made |

Also reworded, not rows: `.claude/skills/optimize-ipc-round-trip/SKILL.md`
(the measuring protocol's step 2 records `ferrix.fastpath`, and the "tested
equivalence" paragraph), `docs/roadmap/ipc-round-trip.md:16`,
`where-it-stands.md:52`, `README.md:51`, `status.md:85`, and
`OPAQUE-KERNEL.md` §9.7 (one dated note that the switch became the flag; the
history stays as written). No row in CI (`.github/`) and none in
`xtask check` or the coverage run's gate list (`coverage.rs`) passes it.

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
  `present` kernel (`ferrix_fast_write_read`, `fast_write_read`,
  `sched::direct`'s symbols absent), in `xtask check` or `test-flags`;
  control: the `present` build with the hook's cfg removed (always
  compiled) fails it.
* C7 new: xtask refuses `--kernel-option ferrix.fastpath=on` (an
  `args.rs` unit test and one run showing the refusal).
* C8 part A's `--set` without `--no-claim`, refused, fired once with
  `ipc_fastpath=on` now that it builds code.

The loom models of the park protocol (`src/tests/loom/tests/park.rs`,
`slots.rs`) are unchanged and need no re-firing. The implementer takes the
control tags from ledger lines 404, 415 (H1 to H7) and 509 to 520 and names
each re-fired one against its original.

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
  line-keyed.
* `desktop` (`on`): no claim, no coverage owed.
* `fast` (`on`): not buildable until M5. Its coverage run will measure the
  fast path in `fast`'s own image, as BUILD-FLAGS.md says.
* `max` (`off`): absent; the 2026-10-02 rule's "one coverage run has
  measured the fast path" is a coverage run of an `on` build (Q6).

### 1.6 Size

Kernel cfgs, `fastpath.rs`, the removed flag: 4 to 5. The hook's cfg in
`check-item-boundary.py` and its self-test: 1 to 1.5. xtask
(`test-ipc-equiv` as two images, the refusal, `bench-ipc`): 2 to 3. The flag
file, profiles, registry: 0.5. Requirements, SysML, TRACEABILITY, ITEM.md
§2 and §5, the skill, the roadmap pages: 2 to 3. Coverage per profile and
the re-carry: 2. Gating and the controls C1 to C8 under KVM and TCG: 3 to
4. **15 to 19.**

### 1.7 Questions for the consultant (BF6)

1. **Compile-out depth.** Is §1.2's cut right: every item only the fast
   path reaches under `cfg(ferrix_ipc_fastpath = "on")`, `FAST_PATH_ON`
   removed, so the `off` image has neither the code nor the entry's test?
   The alternative, keeping the run-time flag and raising it from a `cfg!`
   constant, leaves the fast path as deactivated code in `present`'s and
   `max`'s images, which rule 1 forbids; it is listed only to be refused.
2. **L.x86_64.150.** Restated as §1.3 says (stage 9's line from the cfg),
   or retired, with the line's check moved into L.x86_64.159's criterion?
3. **The equivalence across two images.** Is `test-ipc-equiv` comparing
   `present` with `present --set ipc_fastpath=on --no-claim` (sha256s
   printed, boot lines required to differ in that flag alone) an acceptable
   form of the tested equivalence that §9.7 and the skill rest on, now that
   the two paths are no longer in one image?
4. **Where the `on` rows and controls run before `fast` builds.** R1 to R4
   and C1 to C8 as ad-hoc `--set ipc_fastpath=on --no-claim` boots of
   `present`, filed as the fast path's regression evidence and never as a
   claim row; `desktop` boots as rows of their own. Or should the rows be
   `desktop` boots, which will also carry `program_barrier=session` and
   `ssbd=on_request` once §4 and §5 land?
5. **`on` on Arm.** The fast path is x86-64 only, and `fast` claims AArch64
   alone (`claim_architectures`). An AArch64 kernel built `on` prints
   `ipc_fastpath=on` and has no fast path. Accept the value with stage 9's
   "x86-64 only" line and an ITEM.md note, or should the flag file mark `on`
   as selecting code on x86-64 only, so the line on Arm says so too?
6. **The 2026-10-02 rule for `max`.** `max` claims AArch64 only, where `on`
   selects nothing. Does the rule ("one coverage run has measured the fast
   path and its vulnerability-analysis entry exists") still gate `max`'s
   value, and if so, is an x86-64 coverage run of an `on` build (the
   coverage run is TCG; the KVM-only branches argued, as step 4's are) the
   measurement it asks for?
7. **The audit record.** Is the fast path's value inside `Config::Build`
   enough for FAU_GEN.1 b, with stage 9's line informational?
8. **The boot option's retirement.** Removing `ferrix.fastpath` from the
   registry, from `present`'s permits and from ITEM.md §5's count, with
   xtask refusing the old option (C7), in one landing.

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
  (twelve processors).
* `SPECULATION.md` §1: "how one build switch takes all of them out", and
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
   return, and records `Defences::NONE.with(READ_BACK_FAILED)` if the cover
   did not read back, as the hardened path does.
3. The `off` build therefore also withholds AVX on an `Uncovered` part
   (Q1).
4. `vector_leak_decisions` runs in the `off` build too (it is a pure table
   check of `decide_vector_leak`).
5. The flag file's `mitigations` `why`, BUILD-FLAGS.md §3's row, and
   SPECULATION.md §1 ("one switch takes all of them out" becomes "every
   speed-trade defence"; "What `off` removes" gains "except Zenbleed's and
   GDS's covers, §3, which stay"), §3's vector row and §9; AoU-11's Zenbleed
   and GDS sentence gains "in every build, `mitigations=off` included".
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

### 2.4 Size

Code 1; the boot-line check and its control 1 to 1.5; requirement,
SPECULATION.md, SAFETY-MANUAL.md, BUILD-FLAGS.md row, the flag file 1 to
1.5; gating 1 to 2. **4 to 6.**

### 2.5 Questions for the consultant (DF6)

1. **AVX in an `off` build on an uncovered part.** With the cover always
   on, an `off` build on, say, a Zen 2 guest that cannot see the host's fix
   loses AVX, as an `on` build does. Accept that (one behaviour in every
   build), or should `off` keep AVX there and name the leak on the boot
   line?
2. **L.x86_64.123** restated in place, as §2.2 says, rather than a new id?
3. **The flag's meaning.** `mitigations=off` then removes every speed-trade
   defence but not these two covers, and SPECULATION.md §1 and AoU-11 say
   so. Is §2.2's wording of both the restatement DF6 asks for?
4. **Evidence.** Is a boot-line check on a host where neither leak applies
   (D1), plus the decision table run in the `off` build (D2), enough, with
   the writes and the secondaries' cover argued as unexercised hardware?

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
the kernel, and **`CR4.MCE` is set nowhere** (`arch/x86_64/cpu.rs` sets PGE,
SMEP, UMIP, SMAP, FSGSBASE and OSXSAVE; the #MC vector, its IST slot and
`paranoid.rs`'s fatal handler exist). QEMU is started with `-smp N` alone
(`tools/common/xtask/src/qemu.rs:2282`), one thread per core.

**The work.**

1. **Topology** (`arch/x86_64/smp.rs`, a new `topology` function beside
   `describe_cpus`): CPUID leaf 0x1F where the maximum basic leaf reaches it
   and subleaf 0 reports a level type, else leaf 0xB; the SMT level
   (type 1)'s `EAX[4:0]` is the shift that separates a thread's bits from the
   core's in the x2APIC ID. `Described` gains each id's thread number
   (`id & ((1 << shift) - 1)`). With neither leaf: one thread per core if
   `CPUID.1:EDX.HTT` is clear, otherwise the boot stops (fail closed, a
   catalogued panic), since `smt=off` cannot be shown. Each started thread
   compares its own leaf's x2APIC ID (`EDX`) with the MADT's.
2. **Parking.** Under `smt=off`, `start_secondaries` starts every thread
   whose number is not 0 with a "park" argument: it runs the secondary path
   up to its IDT, GDT, TSS and per-CPU record (so an NMI or #MC finds a
   valid GS), sets `CR4.MCE`, reports parked, and loops `cli; hlt` for
   ever, touching only image text, its own stack and its record, none of
   which is ever unmapped. Parked threads are kept in `Topology` apart from
   the online ones, so `smp::count()`, `online()`, shootdowns, IPIs, MSI
   targets and the scheduler never see them. One kernel stack each.
   Firmware that already hides the siblings (every MADT id is thread 0) is
   accepted and named on the line.
3. **`CR4.MCE` on every thread** in every build, not only parked ones
   (`cpu.rs`, with the boot processor's bring-up and `secondary_start`).
   Without it a machine check on any processor shuts the machine down
   instead of reaching `paranoid.rs`'s handler. A small item change of its
   own, listed here because parking needs it (question P1).
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
parked thread that skips `CR4.MCE`; the check reads each parked thread's
`CR4` as it reports and fires. A `stibp` boot with `--smp-threads 2` shows
four online.

**For the code to decide.** P1: `CR4.MCE` on every thread in every
profile, landed first as its own item change, or only on parked threads
under `smt=off`. P2: park identically on AMD and Intel (one path), or leave
AMD siblings in wait-for-SIPI (BUILD-FLAGS.md names `CR4.MCE` as Intel's
need). P3: whether a hybrid Intel part's uniform shift (SDM) may be assumed
or each thread's own leaf is required, which the plan does.

**Size**: topology 2; parking and the topology split 3 to 4; `CR4.MCE` 1;
the check and line 1.5; Arm checks 1; xtask 0.5; requirements and docs 1 to
1.5; gating and controls 1.5. **10 to 13.**

---

## 4. Plan: `ssbd=on_request` (DF3)

`desktop` only; every other profile keeps `always`. `PR_GET_SPECULATION_CTRL`
answers truthfully in every profile.

**Today.** `Plan::for_processor` (`arch/x86_64/speculation.rs:223`) sets
`SPEC_CTRL_SSBD` in `PLAN_SPEC_CTRL`, or AMD's `VIRT_SPEC_CTRL`
(`PLAN_VIRT_SSBD`), once per processor at boot; nothing writes either at a
switch. AArch64 (`arch/aarch64/speculation.rs:387-407`) uses `PSTATE.SSBS`
where the core has it, else firmware's `ARCH_WORKAROUND_2`. `prctl`
(`syscall/attributes.rs`, load ring) answers the options of its `option`
module (and seccomp's, through `syscall/seccomp.rs`) and `EINVAL` to the
rest, so `PR_GET_SPECULATION_CTRL` (52) and `PR_SET_SPECULATION_CTRL`
(53) are `EINVAL` in every build today: not truthful, since a program that
asks is told nothing about a defence that is on. Values, from nazuna's
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
   code is compiled.
3. **AArch64**: with `SSBS`, the thread's saved `SPSR.SSBS` per thread;
   with `ARCH_WORKAROUND_2` only, the firmware call at a switch on change.
   ARMv7-A: no control; it answers as below.
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
   * `PR_SPEC_INDIRECT_BRANCH` and `PR_SPEC_L1D_FLUSH`: GET answers what is
     true (§5 uses the first; Q-S2).
   * **Every error value and the exact answer table are taken from Linux's
     `arch/x86/kernel/cpu/bugs.c` (`ssb_prctl_get`, `ssb_prctl_set`,
     `ib_prctl_get`) and `kernel/sys.c` before code**, not from this plan.
5. **The exposure, named**: SPECULATION.md gains `desktop`'s section, "the
   kernel runs with the current thread's setting, as Linux does", and the
   VM-exit cost of `VIRT_SPEC_CTRL` under KVM for a switch that changes it.
6. **Requirements**: new core ids for the write-on-change and inheritance;
   the prctl answer is in the load ring, which has no requirement rows of
   its own today (Q-S1).

**Checks and controls.** A stage-9 case (two threads, one asking for
`PR_SPEC_DISABLE`): the SSBD bit read back on the processor after each
switch matches the running thread; the count of writes equals the count of
changes. Control: the shadow compare removed (written every switch) fails
the count; the write skipped on the direct switch fails the read-back. A
native test program checking GET's answer in `present` (`PR_SPEC_DISABLE`
under KVM on nazuna) and in `desktop` (`PRCTL|ENABLE`, then
`PRCTL|DISABLE` after SET); control: GET answering `EINVAL` again.

**For the code to decide.** Q-S1: whether `PR_GET_SPECULATION_CTRL`'s
truthfulness, an answer about a security function, needs an item
requirement although `prctl` is in the load ring. Q-S2: what
`PR_SPEC_INDIRECT_BRANCH`'s GET says for a process inside a speculation
domain (its predictors are shared with the domain's other members), and
whether SET is accepted outside `program_barrier=session`.

**Size**: the thread word and inheritance 1.5; the switch write on x86-64 2;
AArch64 1 to 1.5; prctl GET and SET 1.5 to 2; the stage-9 case and the
native test 1.5 to 2; docs and requirements 1; gating 1. **8 to 11.**

---

## 5. Plan: `program_barrier=session` (DF2)

`desktop` only, at level `none`; every certified value keeps `switch`
unchanged. **Waits on F-64** (FINDINGS.md: `forget_root` clearing a
processor's `LAST_DOMAIN` after its `LAST_ROOT`, which can erase a domain
just recorded; branch `m0-f64`, which had no commits ahead of `main` when
this was written), because `session` makes domain leaves common. After §4,
since both change `syscall/attributes.rs`'s speculation answers.

**Today.** A domain is one marked job: `Job::new_child_domain`
(`object/job.rs:470`) gives a fresh id; `new_child` and `new_named_child`
(cgroupfs' `mkdir`, line 509) give 0, so a child job of a marked job is
**not** in its domain (`domain_check.rs` case 10 asserts it). A running
process that moves to another job leaves its domain for good
(`Process::move_to`, `object/process.rs:560-576`). Losing dumpability
leaves it (`attributes.rs:169`). Marking needs MANAGE on the parent
(`syscall/native.rs:1683`, `JOB_SPECULATION_DOMAIN`).

**The work**, each under `cfg(ferrix_program_barrier = "session")`:

1. (a) **The subtree.** `bare_child` gives a child job its parent's domain
   (`new_child` and `new_named_child`), so a session's cgroup jobs are in it;
   a marked job inside a marked one still starts its own domain (stricter).
   `move_to` keeps a process in its domain when the target job's domain is
   the one it holds, and leaves otherwise.
2. (b) Unmarked programs keep the barrier to and from everything: domain 0
   is never equal to any other, as today; a check, not new code.
3. (c) **Leaving on request**: `PR_SET_SPECULATION_CTRL(PR_SPEC_INDIRECT_BRANCH,
   PR_SPEC_DISABLE or PR_SPEC_FORCE_DISABLE)` and installing a seccomp
   filter (`syscall/seccomp.rs`, `prctl_set` and `seccomp(2)`'s filter
   mode) call `leave_speculation_domain`, so a sandboxed renderer keeps the
   barrier against its session.
4. (d) F-64 fixed first (above).
5. (e) SPECULATION.md's `desktop` section names `session`'s exposure: a
   session's programs share predictors with each other.
6. **Who marks a session.** Nothing in the Linux personality marks a job
   today: a session is a cgroup made by `mkdir`. Options: a Ferrix-native
   session launcher that makes the session's job with
   `JOB_SPECULATION_DOMAIN` before starting the session, or a
   root-writable cgroupfs file that marks a new cgroup. The plan takes the
   first (no new load-ring interface); Q-B1.

**Checks and controls.** `domain_check.rs` gains a `session` variant of
cases 7 and 10 (a child job and a cgroup `mkdir` under a marked job share
its domain: no barrier between their members) and new cases for a move
within the subtree (stays), a move out (leaves, barrier owed), a seccomp
filter and the prctl (each leaves). The `switch` build keeps today's cases
unchanged; both builds run them. Controls: the subtree inheritance taken out
(the session case fails), the seccomp leave taken out (fails), and part A's
planted `program_barrier=session` in `max`, refused. DF8's check that
`desktop`'s line names `session` is part A's.

**For the code to decide.** Q-B1: how a session is marked (above). Q-B2:
whether (c)'s leave on a seccomp filter or on the prctl should apply under
`switch` too, as a stricter certified behaviour, or stay `session`-only as
DF2 says. Q-B3: whether a process moved into the subtree from outside joins
the domain (today: never joins by moving; the plan keeps that).

**Size**: job inheritance and moves 2; the leaves on prctl and seccomp 1.5;
the marking launcher 1 to 1.5; the domain-check cases and controls 2 to 2.5;
docs 1; gating 1. **8 to 10** (F-64's fix not included).

---

## 6. Order and file ownership

```
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
