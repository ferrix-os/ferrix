# Microkernel M0's performance baselines

*Stream C of [Microkernel M0](microkernel-m0.md), written 2026-10-13 to
2026-10-14 on branch `m0-baselines`; its tooling landed on its own on
2026-10-21 (40996829f, from the branch `mk/baselines`), the baselines to be measured on `main`
once the gate host is quiet.* The switch to a microkernel is gated by the rows of
`docs/MICROKERNEL.md` §7. This page gives each row its method, the command
that measures it, its baseline on a named commit with the range and the host
load beside it, its fail rule, and its negative control with the control's
log. Rows whose baseline could not be taken say why and give the command.

## The rules every row follows

From `MICROKERNEL.md` §7, as applied here:

* **Machine.** nazuna: AMD Ryzen 9 9900X (Zen 5), microcode 0xb404035, host
  kernel 7.0.0-29-generic, 24 processors, no PCID (CPUID.1:ECX[17] clear).
  QEMU 10.2.1 (`~/.local/bin/qemu-system-x86_64`, the gate's), KVM, one
  virtual processor unless a row says otherwise, QEMU pinned to host core 11
  (`taskset -c 11`, `bench-ipc`'s pin; its SMT sibling is core 23). No row
  runs on hardware.
* **Processor model** (the matched rule, 2026-10-02): the gate's own,
  `qemu::x86_cpu("kvm")`, printed at the top of every `bench-rows` run:
  `qemu64,+pdpe1gb,+smep,+smap,+umip,+rdrand,+rdseed,+ssse3,+sse4.1,+sse4.2,+popcnt,+cx16,+movbe,+xsave,+xsaveopt,+avx,+avx2,+f16c,+fma,+bmi1,+bmi2,+abm,+pclmulqdq,+aes,+x2apic,+spec-ctrl,+stibp,+ssbd,+arch-capabilities,+auto-ibrs,+invtsc,+eraps,+xgetbv1`.
* **Mitigations**: every one on, as the kernel boots by default. Its boot
  line in each log: "speculation defences: clamped indices, SWAPGS fence,
  entry registers cleared, AutoIBRS, STIBP, SSBD, predictor barrier on
  switch, RSB fill on switch, ERAPS empties the RSB on switch".
* **Build**: `--release` for the kernel, as `bench-ipc` measures, except the
  self-host row (below). Today the Linux personality is only in the kernel,
  so every figure here is the `--personality kernel` side that §7's Linux
  rows are held to (1.00x, failing above 1.10x).
* **Repeats**: five boots a side; each boot's figure is the median of five
  repetitions' p50s (`bench-rows`), and a row's figure is the median of the
  five boots, quoted with the lowest and highest boot.
* **Quiet host**: a measuring run starts only when the 1-minute load is 3 or
  under (`m0b-run.sh`, below), holds the fleet's `bench.lock`, and runs in
  gate slot 2. `bench-rows` prints the load before and after every boot and
  how busy core 23 was; those figures are in the tables.
* **Fail rule**: 1.10 times the baseline's median, unless the row says
  otherwise. **Control**: a delay of known size injected into the row's
  measured path must make the row fail by that rule, run through
  `gate.sh control` so the log keeps the one-line edit, the marker and the
  failure.
* **The clock policy**: cores 11 and 23 were at governor `powersave`, EPP
  `balance_performance` on 2026-10-14 (the `optimize-ipc-round-trip` skill
  records `performance` from 2026-10-07; it has been set back since). Under
  that policy the core can run in two clock modes; each boot's figure is
  given so a mode change shows.

## The commands

```
# the Linux rows, one run: five boots, every row
gate.sh run <ref> <tag> bench-rows --release --accel kvm --arch x86_64 --rounds 5 \
    [--fail-above <row>=<ns> ...]
# the rustc rows, one boot a run, five runs
gate.sh run <ref> <tag> test-rustc --release --accel kvm [--fail-above <row>=<ms> ...]
# the self-host row, one boot a run
gate.sh run <ref> <tag> test-selfhost --accel kvm [--fail-above selfhost-build=<ms>]
```

`gate.sh` is `~/.local/share/ferrix/fleet/gate.sh` on nazuna. Every
measuring run here went through
`~/.local/share/ferrix/logs/m0-baselines/m0b-run.sh <max-load> <mode> <ref> <tag> <args...>`,
which puts `~/.local/bin` first on `PATH` (a bare ssh shell finds QEMU 9.2.4
in `/usr/local/bin` first, which does not name `eraps`), waits until the
1-minute load is at or under `<max-load>`, takes `bench.lock`, and runs
`gate.sh` in slot 2; its `<tag>.wrap` file beside it records each wait, the
load at the start and the exit.

`bench-rows` (`tools/common/xtask/src/rows_bench.rs`) boots
`src/tests/linux-rows`, a static musl program, as init: KVM, `--smp 1`, a
4 GiB guest, QEMU pinned to core 11, the rustc volume attached at `/data`
read-only for the page-cache row. Each sample is `lfence; rdtsc; lfence` on
both sides of one operation, the guest TSC at 4,400 MHz (10 ns steps), 2,000
untimed operations then 20,000 timed a repetition (20 and 1,000 for the
process rows), five repetitions. `--fail-above <row>=<ns>` fails the run when
a row's median over the boots is above it.

## The rows

| Row (`MICROKERNEL.md` §7) | `bench-rows` row, or gate | What one sample is |
|---|---|---|
| Linux `getppid`, the frozen baseline | `null-getppid`; `getppid-mean` beside | `syscall(SYS_getppid)`, timed per call; and `clockbench`'s method, 20,000 calls between two `clock_gettime` readings, their mean |
| `fork` + `exec` + `wait` of a static program | `fork-exec-wait`; `fork-exit-wait` beside | `fork`, the child `execve`s `/bin/rows-true` (static, returns at once), the parent `waitpid`s it; beside it the same without the `execve` |
| `mmap`/`munmap` churn, 4 KiB anonymous | `mmap-touch-munmap-4K`; `mmap-munmap-4K` beside | a private anonymous 4 KiB mapping, one byte written (the fault), unmapped |
| A page-cache miss end to end | `pagecache-miss-4K` | a 4 KiB `pread` of the rustc volume's two largest libraries at every MiB, each the page's first read in the boot: btrfs, the page cache, the block ring and the ring-3 driver. One pass a boot (a page misses once); the seam's counters must show one disk page per read (`LB pagecache reads 342 from-disk-pages 342`) |
| Signal delivery to a running thread | `signal-self` | `tgkill` of the calling thread with `SIGUSR1`, delivered on the way out of the call, the handler run, `rt_sigreturn` (lmbench's `lat_sig catch`); the count caught is checked |
| `futex` wake to a waiting thread | `futex-thread-pingpong` | two threads of one process: `FUTEX_WAKE_PRIVATE` of the waiting one and the wait for its wake back, a round trip |
| 4 KiB block read, ring and ring-3 driver | `block-read-4K` | the kernel's own `seam` boot line in the same boot: 1,024 reads of 4 KiB of the pattern disk through the block ring and the ring-3 virtio-blk driver, one at a time, its depth-1 p50 (`block_ring::hop_check`) |
| Warm `rustc hello.rs` | `test-rustc`'s `warm-rustc-hello` | five `rustc hello.rs` after a first warm one, timed together from `/proc/uptime` read by the shell (10 ms steps, 2 ms a compile) |
| Cold `test-rustc`, and its call count | `test-rustc`'s `cold-test-rustc`, `cold-rustc-hello`; the `cold seam syscalls` line | the script from its start to its cold counters (`rustc -vV`, `cargo -V`, `rustc hello.rs`, `cc hello.c`, both programs run); the first `rustc hello.rs` alone; the system calls the seam counted since boot |
| The self-host build | `test-selfhost`'s `selfhost-build` | `cargo xtask build --arch x86_64` on Ferrix, from the stamp before it to the one after |
| Chrome's network bench | none yet | owed, below |
| The compositor's frame time | `bench-chrome`'s ms/frame | owed, below |
| `test-unity` | `test-unity` | owed, below |

## The baselines

**Baseline owed: host never idle.** From 2026-10-14 18:47 UTC the measuring
run waited for a 1-minute load of 3 or under, re-checked every 30 s and
logged every 10 minutes (`m0b3-base-rows.wrap`); the load stayed between 9
and 49 (24 processors, five gate slots in use by other sessions). Nothing
below is a baseline. The commit to measure is the branch's tip on `main`,
and the commands, run as they are queued:

```
cd ~/.local/share/ferrix/logs/m0-baselines
./m0b-run.sh 3 run <tip> m0b3-base-rows bench-rows --release --accel kvm --arch x86_64 --rounds 5
for n in 1 2 3 4 5; do ./m0b-run.sh 3 run <tip> m0b3-base-rustc-$n test-rustc --release --accel kvm; done
./m0b-wall.sh <tip> <cold-rustc-hello> <cold-test-rustc> <warm-rustc-hello> 5   # C4, C5, then selfhost x5
./m0b-wall.sh <tip> - - - 0 <selfhost-build>                                     # C6
./m0b-controls.sh <tip> <getppid> <getppid-mean> <block> <fork-exec> <mmap> <signal> <futex> <pagecache>
```

each fail line being 1.10 times the row's median from the runs before it.

**Readings so far** (not baselines: a busy host, or one boot), release
kernel unless said, one processor, the gate's model:

| Row | 2026-10-13, `m0b-base-rows`: five boots on 95257274c (`main` a21835fbb), load 3.7 to 16.8 | 2026-10-14, `m0b2-smoke-rows2`: one boot on 9360d73d7 (`main` c0bded3c7), load 33 | Fail line (1.10x) once measured |
|---|---|---|---|
| `null-getppid` | 130 ns (130 to 170) | 130 ns | 1.10x |
| `getppid-mean` | 117 ns (116 to 162) | 118 ns | 1.10x |
| `fork-exec-wait` | 494 us (352 to 563) | 416 us | 1.10x |
| `fork-exit-wait` | 94 us (92 to 132) | 113 us | beside |
| `mmap-touch-munmap-4K` | 1,920 ns (1,910 to 2,660) | 1,940 ns | 1.10x |
| `mmap-munmap-4K` | 1,310 ns (1,300 to 1,860) | 1,340 ns | beside |
| `pagecache-miss-4K` | 75 us (70 to 102) | 186 us | 1.10x |
| `signal-self` | 2,050 ns (2,030 to 2,650) | 2,070 ns | 1.10x |
| `futex-thread-pingpong` | 2,030 ns (1,600 to 2,640) | 2,100 ns | 1.10x |
| `block-read-4K` (depth 1) | 63.3 us (61.8 to 65.5) | 63.4 us | under 80 us, failing above 88 us (§7's target) and above 1.10x |
| `warm-rustc-hello` | — | 172 ms (`m0b2-smoke-rustc`, dev kernel, load 30) | 1.10x |
| `cold-rustc-hello` | — | 2,050 ms (same) | 1.10x |
| `cold-test-rustc` | — | 3,140 ms (same); 518,834 system calls | 1.10x |
| `selfhost-build` | — | not run (245 to 323 s for the whole gate, 2026-10-03 and -04) | 1.10x |

A dev kernel at load 2.6 (`m0b-base-rows-dev`, 2026-10-13, five boots)
read `null-getppid` 180 ns, `getppid-mean` 171, `fork-exec-wait` 1,040 us,
`mmap-touch-munmap-4K` 8,761 ns, `signal-self` 7,161, `futex-thread-pingpong`
5,391, `pagecache-miss-4K` 101 us, `block-read-4K` 91.7 us: the profile
matters several times over for the process and memory rows, so each row's
profile is part of its definition.

## The controls

Each control is one exact-text edit run by `gate.sh control`, which logs the
edit, undoes it afterwards, and says FIRED only when xtask failed and a line
of its output holds `is over its fail line` beside a guest's serial line.
The scripts that run them are `m0b-controls.sh` and `m0b-wall.sh` beside
`m0b-run.sh`. **None has run: each needs its row's measured fail line.**

| Control | The edit | Known delay | Rows it must fail |
|---|---|---|---|
| C1, in the kernel | `src/kernel/src/syscall/linux.rs`: 200 ns of busy wait on the kernel's clock before `getppid` answers | 200 ns a call | `null-getppid`, `getppid-mean` |
| C2, in the kernel | `src/kernel/src/interfaces/block_ring/hop_check.rs`: 30 us of busy wait after `let started` in each timed read | 30 us a read | `block-read-4K` |
| C3, in the program | `linux-rows`' `INJECT` line: 150 us for `fork-exec-wait`, 1 us each for `mmap-touch-munmap-4K`, `signal-self` and `futex-thread-pingpong`, 40 us for `pagecache-miss-4K`, inside each timed sample; the `LB inject` lines are the marker | as given | those five, each named in the one failure line |
| C4 | `test-rustc`'s script: `sleep 2;` before the first `rustc hello.rs` | 2 s | `cold-rustc-hello`, `cold-test-rustc` |
| C5 | `test-rustc`'s script: `sleep 1;` before the `warmed` stamp | 200 ms a compile | `warm-rustc-hello` |
| C6 | `test-selfhost`'s script: `sleep 60;` before the `built` stamp | 60 s | `selfhost-build` |

C1 and C2 put the delay in the kernel's path, so they also show that the
row's timed window holds the kernel's work; C3 to C6 put it inside the timed
window from the program's or script's side.

## The gate's processor model and HR6

`MICROKERNEL.md` §7 (*The certified build's speculation rule*) wants the
gate's model to pass IBPB_BRTYPE, ERAPS, SRSO_NO, SRSO_USER_KERNEL_NO and
BTC_NO through, or the kernel to treat a missing bit as affected. Read with
CPUID on the host (`m0b-cpuid`, `host-cpuid.txt`) and in the guest
(`linux-rows`' `LB cpuid` line, every boot):

| Bit | Leaf | Host | Gate model today | Gate model plus the names | QEMU 10.2.1's name |
|---|---|---|---|---|---|
| AutoIBRS | 8000_0021 EAX[8] | 1 | 1 | 1 | `auto-ibrs` |
| ERAPS | 8000_0021 EAX[24] | 1 | **1** | 1 | `eraps` |
| SBPB | 8000_0021 EAX[27] | 1 | 0 | 1 | `sbpb` |
| IBPB_BRTYPE | 8000_0021 EAX[28] | 1 | **0** | 1 | `ibpb-brtype` |
| SRSO_NO | 8000_0021 EAX[29] | 0 | 0 (true: the host is affected) | 0 | `srso-no` |
| SRSO_USER_KERNEL_NO | 8000_0021 EAX[30] | 1 | **0** | 1 | `srso-user-kernel-no` |
| IBPB (AMD) | 8000_0008 EBX[12] | 1 | 0 (IBPB is offered as Intel's 7.EDX[26]) | 1 | `ibpb` |
| BTC_NO | 8000_0008 EBX[29] | 1 | **0** | 0 | **none** |

So the gate's model today passes ERAPS and none of IBPB_BRTYPE,
SRSO_USER_KERNEL_NO and BTC_NO; SRSO_NO is clear on the host itself. With
`+ibpb,+ibpb-brtype,+sbpb,+srso-user-kernel-no` added (through
`FERRIX_X86_CPU`, one boot, `m0b2-hr6-model`) KVM passes all four, and the
kernel booted and ran every row with the same speculation boot line. BTC_NO
has no feature name in QEMU 10.2.1, so no `-cpu` list can pass it; it needs
`-cpu host` or a QEMU patch. **Owed, not done here** (the brief: xtask's
model is not changed in M0's stream C): adding the four names to
`qemu::x86_cpu` under KVM, and a way to pass BTC_NO; until then the kernel
must treat IBPB_BRTYPE, SRSO_USER_KERNEL_NO and BTC_NO as absent, which is
the fail-closed reading HR6 asks for. Whether today's kernel reads any of
these bits is not part of this stream.

## What is owed

* **Chrome's network bench.** There is no such bench in the tree:
  `OPAQUE-KERNEL.md` S3 names it as the row for the network stack's split,
  and nothing measures it yet. The method, for the stage that splits the
  stack: `test-chrome`'s volume, Chrome headless loading one page of a fixed
  16 MiB body from a server on the host through xtask's gateway (guest
  downloads ran at 4 to 5 MB/s after the gateway's fix of 2026-09-30), the
  time from navigation to `load`, five boots
  a side, failing above 1.10x; its control a delay in the gateway's
  forwarding. Baseline owed: the bench does not exist.
* **The compositor's frame time** is `bench-chrome`'s `ms/frame` column per
  phase (idle, scroll, hover: hyprix's own frame reports). `bench-chrome` is
  "a number to hold a change to, not a gate": it has no fail rule, so a
  control cannot fire through it. Owed: a `--fail-above` on its ms/frame
  (as `wall_rows` gives `test-rustc`), a control that sleeps in hyprix's
  frame loop, then five runs a side
  (`gate.sh run <ref> <tag> bench-chrome --accel kvm`).
* **`test-unity`** runs headless (about 7 minutes a boot, 2026-10-13's
  `endfield-yserver-unity`) but prints no timing a row can hold: it judges
  lit and changed pixels. Owed: a figure (frames presented in its window
  over its 30 s, which its judgement already reads), a fail rule on it, a
  control, and ten boots of about 7 minutes on a quiet host.
* **The gate's processor model** (HR6, above): the four names, and BTC_NO.
* **A Linux guest's figures** beside these, for context only (§7's targets
  are in multiples of today's kernel, not of Linux): `linux-rows` runs as a
  Linux guest's `/init` unchanged; not run here.

## Findings

* **The frozen baseline is not 357 ns.** The 357 ns of 2026-10-10 was
  `clockbench`, a static glibc program run over ssh inside a full
  `run-compositor --everything` desktop of 8 processors, a dev kernel, at a
  host load of 18 to 60 (BACKLOG, *A bare system call costs several times
  Linux's*). The same method (`getppid-mean`) under `bench-rows`' conditions
  reads about 118 ns, and the per-call p50 about 130 ns, on every run of
  2026-10-13 and 2026-10-14 so far, release kernel, one processor or eight
  (`m0b-base-rows-smp8`: 130 ns). A dev kernel reads 171 to 180 ns
  (`m0b-base-rows-dev`, load 2.6). **This moves the (ii) row's fail line**:
  1.10 times the re-measured figure is about 143 ns, not 393 ns, which is
  below the floor `MICROKERNEL.md` §7 gives for a call through restricted
  mode (two root switches, about 260 ns on nazuna) and below seL4's 440 ns
  round trip. The (ii) target "1.00x of the frozen baseline" is therefore
  out of reach by construction once the baseline is measured as here: the
  customer decides again (the stop rule's numbers), or the row is
  restated (for instance as the 393 ns of the customer's round-trip goal).
  This page does not change §7.
* **`fork` + `exec` + `wait` loses about 344 KiB of free memory a child**
  with one processor, reaped or not, and `fork` + `exit` + `wait` about
  86 KiB (`LB meminfo`, every one-processor boot since 2026-10-13); with
  eight processors neither loses any (`m0b-base-rows-smp8`). Something
  freed at a child's end waits for work a single processor never gets to
  while the parent keeps forking. It is why `bench-rows` gives the guest
  4 GiB (a 512 MiB guest ran out at the 1,317th child, `m0b-smoke2`). For
  the BACKLOG: the memory a dead child keeps on one processor.
* **The 4 KiB block read is already under Redox's 80 us** at depth 1 with
  one processor: 61.8 to 65.5 us (`m0b-base-rows`, 2026-10-13, load 4 to
  17), against 300 to 844 us on 2026-09-27. With eight processors it was
  41.7 to 156.5 us, median 129.4 (`m0b-base-rows-smp8`, load 18 to 23).
* **The page-cache miss is the row most sensitive to the host**: 70 to
  102 us at loads 4 to 17, 186 us at load 33 (`m0b2-smoke-rows2`); the
  disk reads reach the host's page cache through QEMU, which other
  sessions' I/O shares.
* **The cold `test-rustc` makes about 519,000 system calls** to its cold
  counters (the seam's count from boot, `m0b2-smoke-rustc`, dev kernel,
  load 30), and the warm compile alone 4,431 (4,345 in S0).

## The tooling

The tooling adds no kernel code and touches no file of the certified item:

* `src/tests/linux-rows/` (new): the static musl program, and `rows-true`;
  `INJECT`, the one line a control replaces, empty in every measured run.
* `tools/common/xtask/src/rows_bench.rs` (new): `bench-rows`, its fail rule,
  the seam line's parse, and the wall-clock rows' stamps and fail rule
  (`Span`, `wall_rows`) that `test-rustc` and `test-selfhost` use.
* `tools/common/xtask/src/rustc.rs`, `selfhost.rs`: `rows-time` stamps in the
  scripts (the shell's builtin `read` of `/proc/uptime`, no program
  started), five more warm compiles in `test-rustc` (about 0.9 s), and the
  rows printed after the gate's own judgement; `--fail-above` on both.
* `args.rs`, `main.rs`, `pipe_bench.rs`: the flag, the command (its own
  task table in `rows_bench.rs`), `summary` made `pub(crate)`.

The logs, scripts and wrap files are in
`~/.local/share/ferrix/logs/m0-baselines/` on nazuna; the gate logs are
`~/.local/share/ferrix/logs/queue/<tag>.log`.
