# Why `personality=user` was slow, and what is left

*mk-perf, 2026-11-04, x86-64 KVM on nazuna (Ryzen 9 9900X, no PCID). The method is
`bench-rows`', and the numbers are readings on a loaded host unless a row says otherwise.
None is a §7 baseline, since those need five boots on a quiet host and gate.sh.*

## The answer in one paragraph

Most of the gap was not the microkernel's design. It was a handful of plain mistakes in
the first lift: seven CPUIDs per signal, a futex lock convoy, one kernel region per filled
page, and a whole-TLB flush per large unmap, which is two `CR4` writes and two VM exits.
With those gone, each row reads about **the hybrid's figure plus 500 ns per crossing**,
and most of a crossing is the design: two root writes without PCID, and the TLB refill
behind them. `null-getppid` cannot reach §7's 1.00x (120 ns): its floor on this host is
about 450 to 480 ns.

## How to read the figures

* Both shapes come from one tree, `feature/mk-perf`. Kernel side: `--kernel-option
  ferrix.checks=skip`, because on this tree restricted-check's case 125 stops the boot.
  User side: `--personality user --set restricted_fastpath=on`. Same processor model,
  same mitigations, same release profile (LTO, one codegen unit), `linuxd` included.
* Boots alternate, with QEMU pinned to whichever of cores 6 to 11 and its sibling looks
  quietest, at `nice`. Tools are in `~/.local/share/ferrix/mk-perf/tools` (`abrun.py`).
* **Noise.** Whole boots read about 1.3x slower in every row, on both shapes (the "two
  clock modes" of `roadmap/microkernel-m0-baselines.md`). So the ratio quoted here is of
  each side's fastest boot, with the medians beside it. `pagecache-miss-4K` is one pass per
  boot through the disk and swings most, from 85 us to 618 us when the host's disk is busy;
  **its figure is the noisiest**.
* **A measuring error found**: the M0 page's `fork-exec-wait`, 494 us, is stale. The same
  tree's kernel side reads 231 us, so the 0.8x first reported was really 1.6x to 1.9x.

## The rows

Tip of `feature/mk-perf`, 14d936849; hybrid from the same tree, same processor model and
mitigations. Five boots a side, alternated, load 17 to 20, median and fastest boot in
brackets. "First" is mk-lift's reading at 475bbfce5 (`mmap-munmap-4K`: this branch's first
boot, 51c84db32). `signal-self` moves most from boot to boot, 3,770 to 5,080 ns. The ratio
is of the two fastest boots.

| Row | Hybrid | First | Now | Ratio | Target | Fails above |
|---|---:|---:|---:|---:|---:|---:|
| `null-getppid` | 120 (120) | 1,140 | 570 (570) | **4.75x** | 1.00x | 1.10x |
| `getppid-mean` | 105 (105) | | 578 (572) | **5.45x** | beside | |
| `mmap-munmap-4K` | 1,280 (1,260) | 4,370 | 1,870 (1,870) | **1.48x** | beside | |
| `mmap-touch-munmap-4K` | 1,880 (1,860) | 10,142 | 4,220 (4,150) | **2.23x** | 1.00x | 1.10x |
| `signal-self` | 2,070 (2,050) | 26,055 | 3,810 (3,770) | **1.84x** | 1.00x | 1.10x |
| `futex-thread-pingpong` | 2,210 (2,210) | 11,962 | 6,411 (6,391) | **2.89x** | 1.00x | 1.10x |
| `fork-exit-wait` | 124.3 us (124.1) | 243 us | 166.1 us (165.1) | **1.33x** | beside | |
| `fork-exec-wait` | 247.6 us (236.4) | 391 us | 334.9 us (327.4) | **1.38x** | 1.00x | 1.10x |
| `pagecache-miss-4K` | 73.8 us (71.8) | 100 us | 90.1 us (85.6) | **1.19x**, noisy | 1.00x | 1.10x |
| `warm-rustc-hello` | 48 ms | 76 to 84 ms | 60 to 64 ms | **1.3x**, few runs, sequential | 1.00x | 1.10x |
| `block-read-4K` | | | | not measured: the user shape prints no `seam` line | under 80 us | 88 us |
| Chrome, compositor, `test-unity`, self-host | | | | not measured | 1.00x | 1.10x |

**Every row still misses.** None of these is a §7 baseline (five boots a side on a quiet
host, through `gate.sh`, with a control). The `gate.sh` five-boot runs were queued and then
withdrawn at the coordinator's order, to clear the queue for a landing; they did not run. A
last pair at f816a7360 (five boots, load 20 to 40) reads the same within its noise: fastest
boots 4.75x, 1.52x, 2.27x, 1.89x, 3.08x, 1.21x, 1.46x, 1.22x in the table's order, without
`getppid-mean`'s row counted. The method's control ran locally: 200 ns injected into
`null-getppid` and 1 us into `signal-self` fail their lines (700 ns, 4,500 ns).

`max`'s row is separate. It is at most 660 ns by §7. With `partition_speculation=exit` and
K1's fast path on, built by editing `build-flags.toml` for the run since `exit` is not
buildable ad hoc yet, `null-getppid` reads **1,630 ns**; with the fast path off, 2,100 ns.

## Where a `null-getppid` goes

Rdtsc stamps at 20 points on the fast path, about 500 trips in a boot, medians. Each
stamp costs about 20 ns, so the sum reads about 1,000 ns against 560 ns unstamped; the
second column takes that out, and is therefore an estimate.

| Part | Stamped, ns | About, ns | What it is |
|---|---:|---:|---|
| Two root writes | 110 + 110 | 180 | **Inherent**: no PCID on this host. §7's floor |
| `SYSRET` to linuxd's first instruction, and to the program's | 110 + 70 | 140 | **Inherent**: the TLB is empty after each root write, and each first fetch and stack touch is a two-dimensional walk |
| `CR0.TS`, set and cleared | 50 + 20 | 60 | Removable only by a trade: NOT YET, ledger 6464, below |
| linuxd: `left_program`, `note_call`, filter, dispatch, `back`, `entering` | 250 | 130 | Trimmed once. Perhaps 30 more |
| The kernel's entries, checks and frame copies, both ways | 200 | 50 | Near what it can be |
| **Total** | 1,000 | 560 | |

So the floor on this host is about **500 ns, 4.2x the hybrid's 120**, and about 440 ns with
the `TS` trade. 1.00x is out of reach by design, not by defect: two root writes alone are
1.5x the whole hybrid call. Text ordering of linuxd's hot functions was tried and gave 30 ns
at most, so instruction-TLB misses are not a lever of size.

Under `exit`, the same stamps: the `IBPB` itself is about 340 ns, and every part after it
slows because its branches now mispredict (linuxd's dispatch 60 to 330 ns stamped): about
700 ns more in all. §7's pricing of 230 ns a barrier does not hold on this processor.

## Each row is the hybrid plus its crossings

(`bench-ipc`'s `domain-call`, for the record: 1,378 ns on the hybrid at this tree, `ipc_fastpath`
off, three boots, load 30. Under the user shape it does not finish: `ipc-bench` ends by a
segmentation fault at +0x2c5c, not diagnosed, so the native row has no figure there.)

A crossing (out to linuxd and back) now costs about 450 ns over what the same work costs in
the hybrid. Counted per operation:

| Row | Crossings | Hybrid + crossings | Now | What is left |
|---|---:|---:|---:|---|
| `mmap-munmap-4K` | 2 | 2,170 | 1,880 | Nothing: the shadow is cheaper than the hybrid's regions |
| `mmap-touch-munmap-4K` | 3 | 3,200 | 4,110 | `SPACE_PLACE` 450, `SPACE_UNMAP` 350, a fault's general path. Lazy fault path: -270, below |
| `signal-self` | 2 | 2,930 | 3,770 to 4,450 | One `VECTORS_READ` 330, one write 260 to 390, one `ENTER` 390, two context reads 300 |
| `futex-thread-pingpong` | 4 | 4,000 | 6,330 | Two wakes 1,000, two context reads 320, the switch's vector state |
| `fork-exit-wait` | 25 | 132 us | 163 us | Kernel process and thread 6 us, `SPACE_PLACE` and `SPACE_UNMAP` about 15 us, linuxd's own |
| `fork-exec-wait` | 54 | 255 us | 326 us | As above |

## What was wrong, and what fixed it

Figures are paired, or say which run. Commits are on `feature/mk-perf`.

| Finding | How it was found | Fix | Effect |
|---|---|---|---|
| **Seven `CPUID`s a signal**, each a VM exit of about 2.8 us | Timer-interrupt IP sampling in a scratch build: 63% of linuxd's samples sat right after `CPUID` | 081d198c8: kept after the first read | `signal-self` 23.8 to 4.5 us |
| **A futex convoy**: a wake under a lock switched to the woken thread, which spun on its waker's lock | The same: 65% of linuxd's futex samples in `address_wait::leave` | 701e0abb6, 1fbbb2c76: wakes wait for the last lock | `futex` 9.5 to 6.7 us |
| **A region per filled page**: every unmap walked and sorted every region | Samples, and counters per native call | mk-lift's `SPACE_PLACE` | `mmap-touch` 6.7 to 4.3 us |
| **Unmap of memory never touched** still called the kernel | Counters | b919ad332 | `mmap-munmap` 4.0 to 2.1 us |
| **A whole-TLB flush for any long range with a leaf**: two `CR4` writes, two VM exits, 45,000 cycles; an 8 MiB stack paid it twice a fork. Short ranges: an `INVLPG` a page | Per-call samples by length | ddef9042c: only what came down is named | `SPACE_UNMAP` 22,900 to 8,200 cycles a call |
| **Registers read for calls that do not read them** | Counters | `needs_context` inverted; a self-wake, a sigreturn's capture gone | Context reads 18 to 8 a fork+exec; `signal` 4.5 to 3.8 to 4.5 us by boot |
| **20,000 faults a warm `rustc`**, 70% to fresh anonymous memory | Fault kinds counted | File fault-around, write-ahead on a walk, under `fault_ahead`: `fast` on, `max` off (ledger 6537) | `warm-rustc-hello` 76-84 to 60-64 ms |
| **A page past a file's end was `SIGSEGV`**, not a figure but found on the way: the window has no regions, so the kernel's fault kind said no mapping | `fault-ahead`'s case, with the flag off too | 14d936849: the shadow's answer says why | `SIGBUS` as Linux |
| Small costs on each crossing | Stamps | ea9d3e8a2 (K1's, not landed): `FS` left in place, no kernel-time stamp, `flip` borrows | `null-getppid` 630 to 570 ns |

Not a kernel profile problem: both sides are release, LTO, one codegen unit, `linuxd`
included, with the same mitigations. No `TEMP-DEBUG` is left in the tree, by grep, and
`debug_assert` is off in that profile.

## Measuring errors found

* The M0 page's `fork-exec-wait` baseline is stale, as above.
* Boots at 630 ns and 830 ns were the two clock modes, not a bug.
* Two worktrees on one `CARGO_TARGET_DIR` run the wrong tree: xtask's root path is compiled
  in and its fingerprint does not see the move. One wrong table came from this.
* A mean kernel-call time includes being preempted inside the call: `SPACE_UNMAP`'s
  "29,000 cycles" was partly that. Per-call medians by length are the figure to trust.
* Timer samples land right after `STI` and `SYSCALL`: time with interrupts masked shows up
  there, not where it was spent.

## Tried and dropped, with why

| Lever | Result |
|---|---|
| Placing what the shadow already holds around a fault | Faults 41 of 42 and 15 of 15: the neighbours are not in the shadow. Withdrawn (ledger 6416) |
| One `SPACE_UNMAP` for a dying space | 2% on `fork-exit`, nothing on `fork-exec`, and a new order at exit. Not kept |
| Ordering linuxd's hot functions together | 30 ns on `null-getppid`, and an ordering file to keep true. Not kept |
| Fast path for page faults | `mmap-touch` -6%, `fork-exec` -4%. On `feature/mk-perf-fault-lazy`, parked: 14 cases and 11 controls owed (ledger 6458). K1's owner decides |

## Levers left

| Lever | Size | Why not now |
|---|---|---|
| No `CR0.TS` under `partition` | 100 ns a crossing | NOT YET (ledger 6464): SSE in linuxd would hand linuxd's values to the program on re-entry, a leak between programs. It trades a hardware trap for an argument in one of §3's four rules, the customer's to make on a figure |
| K1's three cuts landed | 60 ns a crossing | OK IF Z1-Z4 (6464); on mk-restricted's stack |
| Fast path for page faults | 270 ns a fault | Above |
| Program counter and stack in K1's exit registers | 320 ns a futex round trip, one context read a park | K1 ABI, unreviewed |
| PCID, on hardware that has it | Most of the 180 + 140 ns | Not on nazuna; no row runs on hardware |
| Larger pages for linuxd's text and data | Unmeasured | Global pages stay excluded (§7) |

## What this means for the decision

The hybrid stays faster on bare calls and always will on this host. How much that matters
depends on the mix: 1.2x to 1.4x for process creation and compiling, 2x to 3x for
call-heavy paths, 4.7x for a call that does nothing. Those are §7 failures, reported as
such, and no threshold has moved. Whether in-kernel code is ever deleted is the customer's
decision on these figures, after a run on a quiet host.

## Where it stands

* `feature/mk-perf`: linuxd's fixes taken by mk-lift in part. Its history holds 4433d085a
  through mk-lift, so it is read and squashed from, never joined to a batch. The window's unmap
  reviewed OK IF C1-C3 (ledger 6537): its check and controls FIRED through `gate.sh`,
  requirement ids and coverage owed on mk-lift's landing. It does not re-check that tables are released only
  after the shootdown: that is `flush_tlb_pages`' machinery, unchanged (C1).
* Fault-around and write-ahead reviewed OK IF A1-A7 (6537). A1-A6 are built. A7's cases run
  in `test-threads`, and `test-container`'s walk to just under `memory.max` moves no count, flag
  on and off: about 1,960 pages from 330,000 bytes used, of 8 MiB. Controls run **locally, not
  through `gate.sh`**: the walk's region bound and `at_limit`'s room both fire. Those of the look's
  rights, file end and private copy did not fire through `gate.sh`, since `resolve` holds them,
  so the look no longer repeats them. Owed: these re-run through `gate.sh`, and foreign
  neighbours shown not placed, which no row here can see. W+X needs no case: a read neighbour is mapped read-only, and a walk's is not executable.
* One `test-rustc` run hung in its warm loop on the fault-around commit, in about ten then:
  not diagnosed. The unlocked walk (A1) and the wake held across a yield (1fbbb2c76) were
  each a way to it, and both are gone. The three gated runs at 1fbbb2c76 and those since passed,
  which does not prove it.
* `test-net` FAILS under `--personality user` on mk-lift's own tip as on this branch:
  `linuxd: a core handed a driver an object it cannot yet: Audit` reaches `ip link`'s output.
  Not this work's.
* K1's cuts are for mk-restricted's stack. Their evidence cannot come from this tree, whose
  K1 is older than the check program expects (restricted-check case 125).
