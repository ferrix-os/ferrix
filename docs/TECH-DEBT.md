# Technical debt

Why changes to Ferrix cost more than they used to, how to see where, and
what to pay down first. Written 2026-10-16, after the customer asked why the
fleet's velocity fell from hundreds of points a day to low double digits.
Part of the answer was process (the backlog's length, flakes failing whole
batches; both fixed that day); this document is the part in the code.

## 1. What it is

Technical debt is **the extra cost every future change pays because of how
the code is built today**. Something was built quickly, or in a shape that
was convenient at the time; the interest is paid on every later landing that
goes near it, as more files to touch, more conflicts with other sessions,
more flakes, longer gates and more evidence to carry.

Two kinds:

* **Recorded debt**, chosen and written down: the ratchet registers in
  `tools/common/data/` (the complexity baseline calls itself "a debt
  register, not an allowance"), the unverified requirements, the
  consultant's "OK IF" conditions owed, the BACKLOG rows. Ferrix keeps this
  kind unusually well.
* **Structural debt**, which grew without anyone deciding it: files every
  feature must edit, tests that wait on time, generated files that make
  every branch collide. Nobody tracks it, and it is most of the slowdown.

What is *not* debt: hard work (a GPU driver, hardware bring-up) and missing
features. They cost what they cost once. Debt is a cost paid *again* on
every change, and the question to ask of a file is not "is it ugly?" but
"how much does each change here cost, and how often do we pay it?"

Ferrix has 5 `TODO`/`FIXME` comments in 840,000 lines of Rust: debt here is
written into the backlog and the registers, not into the code, so a marker
count finds none of it. It has to be measured from what changes cost.

## 2. The signals

`tools/common/gen/debt-report.py` measures each of these from `main`'s
history (default: the last 30 days) and the tree. Each is a pointer to
files worth reading, not a verdict on them.

1. **Hotspots: changes × lines.** A large file that changes often is where
   interest is paid most. With it, the **fix share**: the part of the file's
   changes made by commits whose message names a fix, flake, race, hang,
   stall, deadlock, bug or FX- panic number. A high share means a design that
   keeps breaking, not just a busy file. The share is a word match: a
   registry that every fix adds a number to (the panic catalog) scores high
   without being fragile.
2. **Coupling.** Pairs of files, one of them code, that change together in
   most of the commits the rarer of them is in. A code file that drags
   evidence or a second code file with it is a dependency nobody declared,
   paid on every change.
3. **Landing breadth.** Files per commit, median and 90th percentile, and
   the share of commits that touch a generated document or coverage file.
   Rising breadth means the structure is getting more tangled.
4. **Flake sources.** The open items of `docs/backlog/flakes.md` by where
   they fail (kernel boot self-check, compositor test, harness), how many
   record a loaded host, and the batch runner's `flakes.log`: the gates a
   batch passed only on their rerun.
5. **Ratchets.** Each register's entry count now and at the start of the
   window. They should only go down; a jump is a scope change (btrfs joining
   the item added 40 functions to the complexity baseline on 2026-10-02) or a
   regression, and the register's `git log` says which.
6. **Waits on time.** `sleep` calls in tests and test drivers, which pass
   on a quiet host and flake on a loaded one, and the share of the kernel's
   source that is boot-time self-checks.

Not in the script, and worth reading beside it: points estimated against
points spent per landing (CONVENTIONS rule 9), which shows the interest in
the unit the fleet plans in, and the count of "OK IF" conditions owed in the
consultant's ledger.

## 3. Reading it

The product owner runs the report once a week (its skill's *Each round*):

    python3 tools/common/gen/debt-report.py [--days 30] [--top 15]

and compares it with the last one. What to do with what it shows:

* A hotspot with a fix share over a third, or one that climbed, gets a
  BACKLOG row naming the file and what keeps breaking in it, before the
  next fix goes in.
* A new coupling pair between two code files is a missing interface: the
  row says which call or table the two share.
* Landing breadth or the generated share rising for two weeks running, or
  a ratchet that grew without a scope change, goes to the customer.
* Debt is paid down only by a landing sized in points like any other, and
  only when the interest it removes is larger: a file nobody will change
  again is not worth splitting.

## 4. What it showed on 2026-10-16

`main` e53e148c1, the 1,033 first-parent commits since 2026-09-30.

**The kernel's boot self-checks.** 111 `*check*.rs` files, 87,445 of the
kernel's 243,365 lines (35%), are compiled into the kernel and run at every
boot. `src/kernel/src/syscall/check.rs` (12,489 lines, 60% fixes) is the
first hotspot, and `stages_check.rs` (60%) and `object/check.rs` (41%) are in
the top ten. 23 of the 52 open flakes are boot self-checks, and 26 record a
host load of 10 or more: many count frames, so they assume a quiet host.
The backlog carries a rule of its own because nearly every landing appends
to `check.rs`.

**Generated files in the tree.** 187 of the 1,033 commits (18%) touch a
generated document or coverage file; the three `docs/generated/` files
changed in 103 commits each, always together, the coverage residuals in about
80. A commit touches 3 files at the median and 18 at the 90th percentile. A
scheduler change drags four coverage and traceability files with it (13
times, 57% of `sched/mod.rs`'s commits), because coverage evidence is
anchored to lines and every moved line is carried.

**Central registries.** `tools/common/xtask/src/main.rs` changed 81 times
(62 test commands in one dispatch), and with `args.rs` in 69% of their
commits; `qemu.rs` 40 times at 4,439 lines; `panic/catalog.rs` 39 times,
with `docs/generated/PANICS.md` in 72% of them. Two features in flight both
edit these, so they conflict.

**Large, busy, fragile files.** `syscall/native.rs` (3,145 lines, 60%
fixes), `arch/x86_64/mod.rs` (51%), `sched/mod.rs` (36%),
`compositor/hyprix/src/state.rs` (6,603 lines).

**Waits on time.** 160 `sleep` calls in 65 test files or drivers, 43 of
them in `compositor/hyprix/tests/two_clients.rs`, whose flakes are rows of
their own. The cgroupfs fix of 2026-10-16 ("wait on the state they read, not
on luck") is the pattern for all of them.

**Recorded debt.** 70 functions in the certified item above the complexity
floor (27 before btrfs joined it), 313 requirements not yet verified, 27
files on the assembly allow-list.

## 5. Paying it down

The certification consultant reviewed the three proposals on 2026-10-17
(ledger `~/.local/share/ferrix/cert-consultant/reviews.md` lines 1950 and
1956, which hold the full conditions). In the consultant's order:

1. **xtask commands register themselves** (§4, central registries): OK, no
   item question; the full row per landing. **Done 2026-10-17** (f3261d9c5,
   5 points estimated, about 3.5 spent): each module holds its own
   `TASKS` table (`tools/common/xtask/src/task.rs`), so a new command is
   one entry in its own module's file. Left: flags are still one `Args`
   struct, so a new flag still edits `args.rs` and main.rs's options text. The panic catalog's half was **withdrawn**: a
   reservation file only moves its conflict, and `catalog.rs` is item code.
2. **`docs/generated/` made, not committed**: OK IF. Only output made
   deterministically from the tree leaves it; `check-docs` generates it
   twice and requires the same bytes, and fails if a check or generator
   starts reading `docs/generated/` (none does: `model.json` is no
   evidence). Measured coverage, the human-written coverage evidence and
   `TRACEABILITY.md` stay committed. About 8 points.
3. **Split `syscall/check.rs` and `stages_check.rs` per topic, and convert
   their waits**: a pure move under standing pattern SP-2
   (`docs/CONVENTIONS.md`), carried by span until F-75 is fixed; each wait
   conversion under SP-1. No design review.
4. **Coverage anchors by function, not line**: OK IF. Anchor = function
   path + statement index + hash of the normalized body; an edited body
   drops its evidence, a moved one carries it. A change to a T2 tool, so it
   needs tool requirements, a validation suite (a seeded move carries, an
   edit drops, a rename drops) and one full re-measure on all three
   architectures against the line-anchored set. About 13 points.
5. **Checks compiled out of the production boot**, last: OK IF (1)-(7).
   The rule is the reverse of this document's first draft: a
   `/// Verifies:` check is verification and may leave the shipped image;
   a check the safety manual, security target or timing analysis claims as
   run-time detection stays, as product code. First a classification table
   (DIAGNOSTIC / VERIFICATION / UNCLAIMED) the consultant reviews; the
   switch is an xtask `--cfg ferrix_checks`, never a Cargo feature; a gate
   that shows product object code identical in both images; the production
   image booted and its coverage published on every gate's architecture.
   Evidence from the test kernel counts for the production one only with
   that gate.
