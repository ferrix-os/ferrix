# Why `max`'s restricted row read 1,630 ns, and what it reads now

*mk-ibpb, 2026-11-04, x86-64 under KVM on nazuna (Ryzen 9 9900X, microcode 0xb404035, no PCID,
guest TSC 4,400 MHz). Every figure is a scratch run by hand, at `nice -n 19`, on a shared host.
**None is gated evidence**: no `gate.sh`, no negative control, and not `max` itself, which does not
build yet. Consultant's rulings: ledger 6747-6755.*

## The answer

`null-getppid` reads **650 ns** on boots whose processor and SMT sibling were both under 5% busy
(650, 650, 670, 650, 650, 650; six boots; host load 4 to 21), against 1,560 to 1,590 ns for the
same rule before. Over all 13 boots of that tree, load up to 58, it reads 670 (650 to 800), so a
loaded host still misses a limit with no tolerance. **`getppid` alone has the straight path: a
null call through linuxd's general path (`getuid`) reads 1,180 ns**, and must be reported beside it.

| Build, same run, alternated | `null-getppid` | `null-general` |
|---|---:|---:|
| Before: `exit`, K1's fast path (feature/mk-perf 64e7d900c) | 1,560 to 2,030 | not in that tree |
| Now: `exit` | 650 to 800 | 1,180 to 1,560 |
| Now: `partition`, no exit barrier | 470 to 520 | 550 to 590 |
| Hybrid, `ferrix.checks=skip` | 120 to 140 | 140 to 170 |

Built as `--personality user --set restricted_fastpath=on --set partition_speculation=exit`, with
`values.exit` in `build-flags.toml` changed to `built = "adhoc"` for the run and left uncommitted.
Flags that differ from `max`: `fs=lift` (max: server), `kernel_memory=heap` (retyped),
`schedule=fair` (windows), `fault_ahead=on` (off), `smt=stibp` (off), `runtime_settings=live` (fixed).

## What the time was

Counts per crossing from KVM's guest PMU, in the same boot as the row's own figure:

| | Cycles | Instructions | Taken branches | Mispredicts | dTLB walks | iTLB walks |
|---|---:|---:|---:|---:|---:|---:|
| Before | 8,540 | 1,890 | 198 | 76 | 17 | 22 |
| Now | 3,505 | 722 | 20 | 12.5 | 9 | 7 |
| Bare assembly, IBPB, frame, `TS` | 2,895 | 145 | 6 | 7 | 1 | 2 |

The bare harness (`~/.local/share/ferrix/mk-ibpb/kvmb`) has no kernel in it: ring 0 and ring 3, two
roots, the row's stamps. Its figures, stamp included:

| What | ns |
|---|---:|
| Crossing with two root writes | 270 |
| The same with the IBPB | 430 |
| The same with frame save, register clears and `TS` | 500 |
| The same with the 32-call refill after the IBPB | 610 |
| IBPB alone: tight loop / inside a crossing, with or without 512 branches first | 150 / 160 |
| SBPB alone, tight | 250 |
| A root write alone | 100 |

After an IBPB the branch target buffer is empty, so **every** taken branch runs cold on every
crossing, wherever it sits: code before the barrier was flushed by the last one. Measured a branch:
direct `jmp` 3.3 ns, taken `jcc` 3.9, not taken 0, indirect `jmp` 4.2, `call`+`jmp`+`ret` 13.
64 cold direct jumps add **no** retired mispredicts: most of it is decode-time redirects, not
mispredicts.

## What each fix bought

Alternated boots, medians at load 4 to 30, so read the steps as sizes.

| Fix | Before | After |
|---|---:|---:|
| No refill where ERAPS empties the predictor | 1,560 | 1,480 |
| K1's helpers inline, no look after a fast exit | 1,480 | 1,390 |
| `getppid` answered ahead of linuxd's general path | 1,390 | 910 |
| No `memcpy`, closures in line, cold marks | 910 | 810 |
| One branch on all conditions, kept parent word | 810 | 770 to 830 |
| Crossings in a small function of their own | 770 to 830 | 660 to 730 |
| Only the 8 kept registers cross | 660 to 730 | 650 |

Tried and dropped: a function a direction, statics grouped (taken 20.5 to 18.5, cycles no better);
signals read without locks (no gain, and a data race, ledger 6751). Order of barrier and root write: no change.

## Which earlier figures were wrong

* **340 ns for the IBPB (mk-perf): wrong.** The instruction is 150 ns. That span held the refill,
  110 ns cold, and the stamps.
* **230 ns for the IBPB (`OPAQUE-KERNEL.md` §9.6): not the instruction.** It is (3,960 - 1,936) / 2,
  seL4's ablation at every switch, cold branches of seL4's path included.
* **About 700 ns of cold predictors (mk-perf): right in size, wrong in meaning.** The base's 1,000 ns
  over the `partition` build is the IBPB 150, the refill 110 and about 740 cold. It was read as the
  cost of the barrier. It is the cost of 198 taken branches, and it is software's.
* **A floor of about 500 ns without a barrier (PERFORMANCE.md): wrong.** Two root writes are 180 ns,
  as it says. The whole bare crossing is 260, not 320 for those two items alone. seL4's 400 to 410
  on this machine already said so. The real path now reads 470 to 520.
* **630 to 640 ns (§7, HR5): the right size for the wrong reasons.** It left out `TS` (70 ns) and
  every cold branch of linuxd and the program.

## 440 ns

Not with two root writes under HR1: bare assembly with the barrier and `TS` reads 500 ns, the row's
stamp included (570 with a row-shaped program), before any real code. seL4's 440 has no barrier.

**The window, a prototype on this branch (0f237e9be), reads 420 ns**: five boots alternated with the
two-root path, load 8 to 32, 410, 410, 420, 420, 480, against 650, 660, 670, 700, 730. By hand, one
processor, not gated. An armed number flips one root slot instead of writing the root twice; four fixed
instructions at ring 3, with interrupts masked, load a word and call `RESTRICTED_RESUME`; the slot
is then cleared and its two pages invalidated. The exit IBPB and `TS` are as today: without the
barrier the same tree reads 220 (220, 220, 220, 240, 250). Counts a crossing: 2,165 cycles, 415
instructions, 17.5 taken, 11 mispredicts, 1 dTLB walk, 1 iTLB walk. A general null call: 1,250.

**It is not like for like with seL4**, and the figure may not be called better than seL4 (ledger
6787 (e)): seL4's 440 pays two roots and runs a server, with no barrier. The like-for-like figure
without a barrier is the two-root `partition` path, 470 to 520.

**The prototype is not safe to use**: its slot is in a root other processors' threads share, so it
holds on one processor only; a reparent leaves the kept answer stale until the next general call; the
stub's bytes and the arming call are scratch. Design and conditions: ledger 6787, W1 to W12, with
W1 for the customer. Nothing lands before W1. `TS` under the window is a separate trade (6787 (f)).

## The customer's decisions, 2026-11-04

Put with these figures: (1) this path skips the root write, reversing K1 §7 on it; (2) the kernel
holds a number and returns a word linuxd wrote, by fixed ring-3 instructions: that stays within "no
Linux calls answered in ring 0"; (3) the IBPB stays on it, though it protects no linuxd code there;
(4) `TS` is traded away on it, for the exact kernel-written stub only, about 60 to 70 ns, calculated,
the row to print it measured both ways. The customer's words: "yes to all (1-4) and improve the
ordinary path now". Recorded by the consultant at ledger 6826 as relayed; W2 to W12 still stand, and
the two-root row is gated first. Not on this tree: no `TS`-off build has been measured.

## The ordinary path, where it was left

A general null call: 1,190 ns, then about 1,110 on one boot of a busy host after named calls for
pointers, tables for decoding and for who reads registers, and the handlers' scan behind a range
check. Counts a call: 5,880 cycles, 1,380 instructions, 83 taken, 44 mispredicts, 17 dTLB and 21
iTLB walks. Tried: linuxd's 262 hot functions together by an ordering file: no gain (6,140 cycles,
24 iTLB walks). Next: the taken branches left in `answer_as` and its chain; 2 MiB pages for linuxd's
text; the kernel's native-call path, about 95 taken a call, which `mmap` and `fork` cross.

## Where the code is

On develop, at the customer's order of 2026-11-05 ("I told you to land what you have to develop"): the
K1 fast path and these cuts, behind `restricted_fastpath`, which only `fast` sets, and `fast` is refused
until M5, so no image that builds has it (`nm` of an off image shows no fast function). What does reach
every image: VERW out of line; K1's record change in the general path; the `#NM` test and FX-0928;
`vectors_elsewhere`; `parent_pid_kept`. linuxd's straight path and cuts run in any `personality=user`
build, flag or not. Ledger 6966. By `gate.sh` at the landing's code: `check`, four test-boots, test-linuxd,
test-threads at two processors, test-shell and test-rustc with the fast path on, all passed. K1's `FS` cut and the stamp followed (ledger 6464, 6981), with Z2 to Z4 owed before the flag. Not landed: the window's prototype, whose
slot is safe on one processor only; `feature/mk-ibpb` and
`~/.local/share/ferrix/mk-ibpb/mk-ibpb-code.patch` on nazuna hold it.

**The row through `gate.sh`, 2026-11-05, nazuna idle, load under 2, sibling 0%, five boots**: `bench-rows --release --accel kvm --personality user --set restricted_fastpath=on --set partition_speculation=exit --no-claim`: 720 ns (710 to 890) before the cut (`mkm-row-2`, 70bad907e), 690 ns (690 to 700) after it (`mkm-row-3`): 30, not ea9d3e8a2's 60. **660 is not reached**, and of the 40 against the 650 above, 30 is develop's tree against the by-hand one (`OPEN-QUESTIONS.md` §1, by hand), not yet named. No delay control and no general-path row, flag on and `personality=user`, not `max`.

## Owed, now that it has landed

For the two-root path, ledger 6747-6755, briefly: fatal on a mark up with no root write; refill-count boot without ERAPS
(row read 770 to 780 by hand); two-processor kill, stop and kick against a `getppid`-only loop, with
a control; 6093 (2)'s fast-only count check on this tree; a table-bound check for the removed guards;
SAFETY categories and coverage; no `Weak` expiring without a clear of the kept word; the row's text
(ledger 6755) and a general-path null call beside it; five quiet boots through `gate.sh` and the
delay control. Base: K1 here is feature/mk-perf's; it must be carried onto mk-restricted's stack.
