# Stability testing: the nightly suite and CI's light checks

**Version 1, 2026-10-27.** The customer's order: a full nightly stability
suite on GitHub Actions, modelled on what Linux uses (KernelCI's boot
farms, LTP, kselftest, stress-ng, syzbot, the fault injectors and the
lockup detectors), and light versions of it in the normal CI. This file
says what runs where, its bounds and its cost. §5 and §6 are designs for
the certification consultant's review before any kernel code; §8 is the
syzkaller design, sent to the product owner before it is built.

## 1. What runs where

| Where | When | What | Blocks |
|---|---|---|---|
| `.github/workflows/stability.yml` | nightly at 02:37 UTC on `develop`, and by hand (`gh workflow run stability.yml --ref develop`) | the full suite, §2 | nothing: no status is posted on any commit, no ruleset names it |
| `.github/workflows/ci.yml`, job `stability-light` | every CI run (promotions, pull requests to `develop`, `develop` nightly) | the light checks, §3 | nothing yet: listed in `ADVISORY` of `CI result` until it has been green on `develop` for a week, then required |
| the gate host | by hand | every job of §2 is one `cargo xtask` command, the same the workflow runs | — |

The nightly starts twenty minutes after `nightly.yml`'s CI dispatch, so the
two do not race for the same `develop` tip's caches. It runs on the public
repository's standard runners: free, a 6 h limit per job, 20 jobs at once.
Every job below has a `timeout-minutes` well under that limit.

## 2. The nightly jobs

| Job | Linux's counterpart | Arches | Bound (timeout) | Judged by |
|---|---|---|---|---|
| **kmem** | — (F-98's own) | x86_64 (KVM), aarch64, armv7a `--smp 2` | 45 min each | `cargo xtask test-kmem`, full: no stop, the limit holds, the canary lives, every request answered |
| **boots** | KernelCI's boot tests | x86_64 (KVM) ×20, aarch64 ×6, armv7a `--smp 2` ×6 | 60 min each | `cargo xtask test-boot` repeated: every boot passes; the count and the first failure's serial log are kept |
| **stall** | — (F-95's own) | x86_64 (KVM, 4 processors) | 30 min | `cargo xtask test-stall`: its own rule (asserted phases red, the rest reported) |
| **ltp** | LTP's `syscalls`, `mm`, `controllers` (memcg, cgroup) | x86_64 (KVM) | 300 min | per test pass, fail, skip (TCONF), broken, timed out; a trend, never red (§4) |
| **memcontrol** | kselftest `cgroup/test_memcontrol` | x86_64 (KVM) | 20 min | per test, as LTP's |
| **stress** | stress-ng | x86_64 (KVM, 4 processors), aarch64 | 60 min | no stop, the canary's longest gap under its bound, no leak (§4.2) |
| **failinject** | `failslab`, `fail_page_alloc` | x86_64 (KVM), aarch64 | 60 min | test-kmem and the standard boots with §5's injector on: no stop, no undocumented errno, no leak |
| **syzkaller** | syzbot | x86_64 (KVM) | 330 min | §8; not built yet |
| **report** | KernelCI's dashboard, by hand | — | 10 min | collects every job's record into the summary and the trend (§4) |

Jobs that need a part not landed yet say so in their record (`skipped:
<what is missing>`) and stay green: `kmem` until F-98's `test-kmem` is on
`develop`, `failinject` until §5 is, `syzkaller` until §8 is built.

## 3. The light checks in CI

Measured on the hosted runner before they were added (§9 has the numbers):

* `cargo xtask test-kmem --lite` on x86_64 under KVM, once F-98's agent
  lands it;
* a 60 s stress-ng smoke run on x86_64 under KVM, the same image and judge
  as the nightly's `stress` job with `--seconds 60`;
* one x86_64 boot with light fault injection (§5: one in 20 000 eligible
  allocations, frames included);
* §6's watchdogs in that same boot. Every other CI boot boots the `present`
  profile, which by §5.1 and §6.1 cannot carry them, so they are not "on in
  every CI boot": they are on in the one boot built with
  `--set stability=nightly`.

They run as one job, `stability-light`, after `build`, in parallel with the
other boots, so they add to CI's wall time only where they are its longest
path.

## 4. Records, summary and trend

Every job runs its commands through `tools/common/stability/stability.py
run`, which keeps the log, times the run and writes one JSON record:

```json
{"job": "boots", "arch": "x86_64", "sha": "…", "status": "fail",
 "seconds": 812, "counts": {"pass": 19, "fail": 1},
 "first_failure": "boot 7: FX-0503 …", "tests": {"…": "pass"}}
```

and a section of the job's own summary. The **report** job downloads every
record, the last night's `stability-history` artifact, and writes:

* the run's summary: one row per job and architecture, with its counts, its
  time, its first failure and the change since the night before;
* `stability-history.json`, the last 120 nights' records, uploaded again
  as the `stability-history` artifact (90 days' retention, carried forward
  every night, so the chain survives as long as one night runs in 90 days).
  No `gh-pages` branch, no commit: the history is an artifact only;
* for LTP and kselftest, the per-test baseline: a test that passed last
  night and fails tonight is listed as a regression, one that failed and
  passes as a gain.

**A red night files nothing in `docs/BACKLOG.md`.** The workflow has a
read-only token and never pushes (a bot commit on `develop` would bypass
the landing lock and the one-author check). It reports in the run summary
and keeps **one rolling GitHub issue**, labelled `stability`: opened on the
first red night, commented on each red night after with the summary's
failure rows, and closed on the first all-green night. The product owner
files BACKLOG rows from it, as from any failure report. That is the whole
mechanism: no pull request, no commit.

### 4.1 What is red and what is data

* Red: a stop (`FX-` after the boot marker, a panic, a hang to the
  timeout), a canary over its bound, a leak over its slack, a boot that
  fails, an asserted `test-stall` phase over its bound, a §6 watchdog line.
* Data, never red: LTP and kselftest failures. They measure Linux-behaviour
  gaps; their counts are a baseline and a trend, and only a stop during
  them is red.

### 4.2 Leaks, measured in ring 3

The leak report needs no kernel code. The runner (`src/tests/stability`)
reads, before and after a run, `/proc/meminfo`, every cgroup's
`memory.stat` (its `kernel` and `pagetables` lines are the job's kernel
memory) and, where F-98 has landed it, `/proc/ferrix-kmem`. After the run
every process it started has been reaped, so each job's kernel memory must
be back within a slack of where it began (1 MiB per cgroup, F-98's
`SLACK`, for the dentries a run leaves cached); the global free memory is
printed as data. It prints one `stability: leak <cgroup> kernel <before>
<after>` line per cgroup and the row compares them.

## 5. Kernel fault injection (design for review)

Linux has `failslab` and `fail_page_alloc`: boot parameters
`failslab=<interval>,<probability>,<space>,<times>` plus debugfs knobs for a
task filter and a call-site address range. Ferrix has a task-scoped
injector already: `ferrix_fallible::{set_injector, arm, check}`, which every
fallible construction asks, and the kernel's policy in
`src/kernel/src/fallible.rs` (`inject`, `inject_once`, `frame_refused`),
which the boot checks use to fail the n-th allocation of one task. What is
missing is a **kernel-wide** policy a whole run can be put under.

### 5.1 Absent from certified builds

* **One build flag, `stability`, values `absent` and `nightly`**, the
  pattern of `runtime_settings` and `crash_capture` (ledger 3558 (5),
  (9)-(11); 3595 (C1)-(C5)). `absent` allows every level; `nightly` allows
  `none` only. `max`, `fast`, `present` and `desktop` all set `absent`; the
  nightly and CI's light boot build with `--set stability=nightly`, which
  carries no claim and says so on its boot line.
* **One removable crate, `ferrix-kernel-stability`**, load ring, depending
  on `ferrix-kernel-load` and `ferrix-kernel-core`, never the reverse. It
  holds §5's policy and §6's detectors. An `absent` build names it nowhere,
  so rustc never loads it.
* **Core gains two typed hooks, nothing else**: a `FailPolicy` the global
  allocator's `policy` and `frame_refused` ask after their task-scoped case,
  and §6's `Watch`. Each is a trait with an `Absent` binding whose methods
  are `const`-false or empty and inline to nothing: a `crate::seam!` trait
  as `Setters` is (`src/kernel/src/setters.rs`), bound once by the binary's
  composition root, `Live` or `Absent` (static dispatch, 2026-10-24's
  rule, listed in `composition_root.policy`). With `absent`,
  the hooks' call sites compile to today's code; `check-item-boundary`
  fails an `absent` profile whose binary depends on the crate, and a symbol
  check finds no `ferrix_kernel_stability` symbol in its ELF, each with a
  planted negative control through `gate.sh control`.

### 5.2 What it fails

* **Eligible allocations**: those `ferrix_fallible::check` and
  `frame_refused` are asked about, made by a task **inside a system call
  from ring 3** (the task's `in_call` mark), after the boot marker. Never a
  kernel task's, never an interrupt handler's, never bring-up's. That is
  exactly F-98's class: every allocation a program can drive. The kernel's
  infallible allocations (outside `ferrix_fallible`) are not asked and so
  never injected; a stop there is a missed F-98 site, which is the point.
* **The decision**, Linux's order: of the eligible calls, the first `after`
  pass; then every `interval`th is a candidate; a candidate fails with
  probability `ppm` per million (a per-processor xorshift64* seeded from
  `seed` and the processor's number, so a run is reproducible on its
  processor count); at most `times` fail (0 for no bound).
* **Filters**: `syscalls` (a list of the architecture's Linux system call
  numbers; the call in flight must be one of them) is the call-site
  filter. Linux's is an address range, which `ferrix_fallible` cannot see
  without `#[track_caller]` on every constructor; the system call is the
  boundary F-98's assertions are written at. `uid` (the task's real uid
  must be it) is Linux's task filter (`make-it-fail`): the runner and init,
  root, are spared, and the programs under test, run as uid 1000, are not.

### 5.3 Boot words

Declared in `build-flags.toml`'s registry, permitted by no profile:

```
ferrix.failalloc=<ppm>,<interval>,<times>,<after>   e.g. 50,1,0,0
ferrix.failalloc.frames=on                          count and fail frames too
ferrix.failalloc.seed=<n>                           default 1
ferrix.failalloc.syscalls=<nr>[+<nr>…]              default: every call
ferrix.failalloc.uid=<n>                            default: every uid
```

An unparsable value is said and taken as off. Arming prints `failalloc:
armed ppm P interval I times T after A frames F seed S filter …`; power-off
prints `failalloc: eligible E failed F (frames G)`, and the first eight
failures each print the pid, the system call and whether it was a frame,
from a per-processor buffer drained outside the allocator.

### 5.4 Assertions (the `failinject` job)

* **No stop**: no `FX-` line, no panic, power-off reached.
* **Only documented errnos**: test-kmem's hostile program already fails on
  any errno outside each call's documented set (`UNEXPECTED`, F-98 K5); the
  stress-ng run counts its stressors' failures and prints each errno it
  met, compared with each stressor's documented set by the row.
* **No leak**: §4.2's per-cgroup kernel memory back within its slack after
  every program has been reaped, and the injector's own `failed` above zero
  (an injector that injected nothing proved nothing).
* **The standard boots under injection** are judged on the first and the
  third only: the boot's own checks run in kernel tasks, which are not
  eligible, but its ring-3 programs may meet an `ENOMEM` and fail, which is
  the right answer. `test-boot --fail-alloc SPEC` relaxes its judge
  accordingly and says so.

### 5.5 Requirements, checks, controls

* A boot check (`stability_check.rs`, outside the product, run only in a
  `nightly` build): arms the policy with `ppm=1000000, interval=1`, makes
  a system call that allocates from a user task, and requires `ENOMEM`;
  the same with `uid` set to another uid requires success. Negative control:
  the `in_call` test removed from the policy fails the boot's own bring-up
  (FIRED expected).
* No requirement of the `present` item changes: the crate is never in it.
  The two core hooks are one new requirement each (the hook is asked and an
  `Absent` binding changes nothing), ids reserved first.
* Bounds: an absent build's hot path is unchanged (checked by the IPC
  round-trip bench, `bench-ipc`, against `develop`); a `nightly` build pays
  one relaxed load per fallible allocation while unarmed.

## 6. Lockup and stall watchdogs (design for review)

Linux has the soft-lockup detector (a CPU that has not scheduled for
`watchdog_thresh * 2` s), the hard-lockup detector (an NMI watchdog: a CPU
taking no interrupt), and the hung-task detector (a task in `D` state for
120 s). Ferrix has the hard-lockup design already: POSTMORTEM.md §6, S4a,
OK IF D1-D14 (ledger 2889), off unless `ferrix.lockup=on`, on branch
`feature/lockup-s4a-wip`. This section adds the other two, aimed at F-95's
class: **a woken task kept off every processor**.

### 6.1 Where it lives

In the same flag and crate as §5 (`stability=nightly`,
`ferrix-kernel-stability`). Core gains one typed hook, `Watch`, with two
calls, each a single line at a place the scheduler already has the
clock's value:

* **W1, a reschedule kept waiting (soft lockup).** In
  `sched::preempt_on_irq_exit`, on the existing early return when the
  preemption count is raised, if a reschedule is pending:
  `Watch::deferred(cpu, now)` stamps the processor's `since` if it is
  zero. `schedule()` calls `Watch::switched(cpu, now)`, which clears it,
  and if `now - since` is over the bound, records the episode: the
  processor, its length, and the site that last raised the count
  (`preempt::preempt_site`, kept since FX-0503). Each later `deferred` on
  the same episode checks the bound too, so an episode that never ends is
  recorded while it lasts. Nothing is added to the lock path: the cost is
  on an interrupt exit that already found nothing to do.
* **W2, a runnable task not run (the hung-task idea, for runnable
  tasks).** `CpuQueue::insert_at` already holds the clock's value; it
  stores it in the task (`queued_at`). At the switch into a task, `exec_start
  = now` is already computed; `Watch::ran(task, cpu, now - queued_at)`
  records the delay if it is over the bound. One store and one compare,
  each beside one the code already makes. Under `absent` neither call nor
  the store is compiled (the store goes through the hook).

A **blocked** task (Linux's `D` state) is left to ring 3: the stress
runner's canary and the stall test already measure a sleeper's wake
lateness from outside, and a kernel scan of every task would be a walk of
the task list under a lock on every period.

### 6.2 What it says

The bound is `ferrix.watch=<ms>`, default **1000 ms** in a `nightly`
build (`off` turns both off): over a second is no slice, no emulator's
starvation that the gate host has measured (FX-0001's 3.6 s was a whole
VM, which the quiet phases of `test-stall` already void), and F-95's
episodes were 0.9-3.2 s. Records go into per-processor slots (count,
worst, its site or task), never printed from the scheduler. A deferred
work item prints each new worst, rate-limited to one line a second:

```
watch: cpu 2 kept a reschedule waiting 1840 ms; count raised at src/kernel/src/mm/vmo.rs:412 (3 locks)
watch: task 731 (stress-ng) runnable 2210 ms before it ran on cpu 1
```

and power-off prints the totals (`watch: deferred N worst X ms; run delay
N worst X ms`). Any `watch:` episode line fails the nightly job that saw
it; the totals are the trend.

### 6.3 Checks and controls

* A boot check (`stability_check.rs`): a kernel task raises the count by
  hand (`preempt_disable`) and spins 1.5 bound with a reschedule pending;
  the episode must be recorded with its site. A second makes a task
  runnable on a processor kept busy past the bound; its delay must be
  recorded. Negative controls: `deferred`'s stamp removed (no episode,
  the check fails) and `ran`'s compare removed (no delay).
* The false-positive row: every nightly boot, the 20 boots of `boots`
  among them, runs with the watchdogs on and must print no episode.
* F-95's reproduction: `test-stall` on a `nightly` build must print
  episodes for the phases F-95 still keeps late (the watchdog's evidence
  that it catches the class), reported as data while F-95 is open.

### 6.4 Leak report

§4.2, in ring 3. No kernel code.

## 7. External tools

All fetched at pinned versions with their hashes, built in a pinned Alpine
container as static musl programs, put on a btrfs volume the guest mounts
at `/data`, and **never shipped in an image** (they are test programs, run
in a guest the CI throws away; `docs/LICENSES.md`'s release inventory never
sees them). Listed in `docs/certification/TOOLS.md` as verification tools
(T1: they can fail to detect a fault, not insert one) and in `SOUP.md`'s
"not in the item" list.

| Tool | Version | Licence | Source | Fetched by |
|---|---|---|---|---|
| LTP | 20260930 | GPL-2.0-or-later | github.com/linux-test-project/ltp release tarball | `tools/common/fetch/fetch-ltp.sh` |
| kselftest `cgroup` | Linux v6.17 | GPL-2.0 | kernel.org tarball, `tools/testing/selftests/cgroup` only | `tools/common/fetch/fetch-ltp.sh` |
| stress-ng | V0.22.01 | GPL-2.0-or-later | github.com/ColinIanKing/stress-ng tag tarball | `tools/common/fetch/fetch-stress-ng.sh` |
| syzkaller | §8 | Apache-2.0 | github.com/google/syzkaller at a pinned commit | §8 |

## 8. syzkaller (design and cost, before building)

To come in this landing series' sixth slice; sent to the product owner
first.

## 9. Cost

To be filled from the measured runs: the nightly's wall time and
job-minutes per job, and CI's wall time before and after the light checks.

## 10. Where it stands

2026-10-27: version 1 of this design. §5 and §6 wait for the
certification consultant's design review; nothing of them is built.
