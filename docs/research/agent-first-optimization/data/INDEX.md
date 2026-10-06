# Evidence index: the native IPC round trip (as of 2026-10-06 13:20)

## Legend

| Key | Meaning |
|---|---|
| Host | nazuna: AMD Ryzen 9 9900X (Zen 5), no PCID; QEMU 10.2.1 with KVM, `-smp 1`; guest TSC 4,400 MHz |
| Load | host 1-minute load average from the log (`load average` or `load-before`); "sib" = busy share of core 11's SMT sibling (cpu23) |
| Logs | relative to `~/.local/share/ferrix/logs/` unless absolute; `~` is `/home/sebastian` |
| G | the gate's `-cpu` model, `tools/common/xtask/src/qemu.rs` `x86_cpu("kvm")`: `qemu64` + x86-64-v3 flags + `+spec-ctrl,+stibp,+ssbd,+arch-capabilities,+auto-ibrs,+invtsc` (unchanged from a1379b25b to a1d456820) |
| G+E | G + `+eraps` (on `main` since 3b9b1e267; on `po7/step5`) |
| G+R | G + `+rdtscp,+rdctl-no`, `taskset -c 11` (the seL4 and Redox runs) |
| mit on | every mitigation Ferrix builds: AutoIBRS, STIBP, the return-stack refill at a switch of space (or ERAPS where G+E and the kernel has 3b9b1e267) |
| dom | both programs in one speculation domain (`domain-call`); `IBPB` skipped inside it |
| fp | `ferrix.fastpath`; "n/a" = before step 4 |
| hist | `bench-ipc` p50 from a histogram floored to an eighth of a power of two (before 3349682db); can read below `min` |
| exact | `lfence`-fenced counter, p50 from sorted samples, one pinned vCPU (3349682db onward) |
| B | `cargo xtask bench-ipc --release --accel kvm --smp 1 --arch x86_64 --init ~/.local/share/ferrix/busybox/x86_64/bin/busybox.static` |
| A(ref, n) | B + `--alternate <ref> --rounds n`: both trees in turn, figure = median of per-round ratios of `domain-call` p50 |
| F(x) | B + `--kernel-option ferrix.fastpath=x` |
| ratio | here ÷ reference, median of rounds (min to max ratio) |

## 1. Ferrix, the series (x86-64, KVM, `domain-call` p50 unless named)

| Id | p50 | What | Tree | Config | Instr. | Load | Log | Command |
|---|---|---|---|---|---|---|---|---|
| F1 | 37,191 ns (`trip`) | before step 1: write, wait, read on each side | origin/main of 2026-10-01 (os-ipc worktree base; hash not in log) | mit on, no dom, n/a, G | hist | not logged | `os-ipc/base-smp1.log` | B (os-ipc worktree) |
| F2 | 40,910 ns (`trip`) | the same, mitigations off | as F1 | `--mitigations off`, G | hist | not logged | `os-ipc/mitoff-smp1.log` | B + `--mitigations off` |
| F3 | 11,157 (`trip`); 6,508 (`call`) | branch `os-ipc/zircon-trip`, sync wake and 0x1013 | os-ipc worktree, 2026-10-01 17:32 | mit on, no dom, G | hist | not logged | `os-ipc/call-smp1.log` | B |
| F4 | 3,021 | branch with the speculation domain | os-ipc worktree, 18:22 | mit on, dom, G | hist | not logged | `os-ipc/domain-smp1.log` | B |
| F5 | 2,789 | branch + `XSAVEOPT` (not carried to main) | os-ipc worktree, 18:43 | mit on, dom, G | hist | not logged | `os-ipc/xopt-smp1.log` | B |
| F6 | 6.5 µs / 2.6 µs (0x1013 across domains) | §9.1's table, mitigations on / off | os-ipc/zircon-trip | as named | hist | not logged | log not located; `docs/OPAQUE-KERNEL.md` §9.1 | -- |
| F7 | 3,021 | step 1 | 5668da6e9 (landed as d750fd452) | mit on, dom, n/a, G | hist | 13.28 | `ipc-step1/bench-on.log` | B |
| F8 | 3,021 | step 1, mitigations off | 5668da6e9 | `--mitigations off`, dom, G | hist | 6.95 | `ipc-step1/bench-off.log` | B + `--mitigations off` |
| F9 | 3,021 | 2d alone | ad4c5a7f0 | mit on, dom, G | hist | 17.57 | `step2b/2d-bench.log` | B |
| F10 | 2,556 (×3) vs 3,021 (×2) | 2b vs its base | 3bb22d1bf vs 265ef1a67 | mit on, dom, G | hist | 3.41–7.80 | `step2a/2b-bench{,-2,-3}.log`, `step2a/2b-bench-base-{1,2}.log` | B, runs in turn |
| F11 | 3,021 / 3,719 vs 3,254 | 2c vs main | b17e01ee5 vs cf83b01dd | mit on, dom, G | hist | 15.9–18.1 | `step2b/2c-bench-alternating.log`, `step2b/2c-bench.log` | B, runs in turn |
| F12 | 2,556 vs 2,789 (3 rounds) | 2e vs its base | f4847ac5b vs 5cf6d7889 | mit on, dom, G | hist | 8.99–10.23 | `step2b/2e-bench-alternating.log` | B, runs in turn |
| F13 | 2,556 / 2,556 / 3,254 vs 2,556 / 2,789 / 2,556 | 2a vs its base | 25297fab1 vs a2771373b | mit on, dom, G | hist | 2.35–7.05 | `step2a/2a-bench-{1,2,3}.log`, `step2a/2a-bench-base-{1,2,3}.log` | `step2a/2a-bench.sh` (B) |
| F14 | ratio 1.117 (1.108–1.362), 3 rounds | `bench-exact` (first build) vs main a70d90039 | 81409fd7a | mit on, dom, G | exact vs hist | 8.93 | `step2b/be-bench-alternate.log` | A(main, 3) without `--smp 1` |
| F15 | 2,546 (2,536–2,566) | main with the exact bench | 3349682db | mit on, dom, n/a, G | exact | see F16 | `step2a/bench-2f-vs-main.log` (reference side), `step2a/bench-main-3349682db.log` | A(3349682db, 5) |
| F16 | 2,417 vs 2,546; ratio 0.949 (0.942–0.957), 5 rounds | 2f vs 3349682db | step2f (landed as 445d09420) | mit on, dom, n/a, G | exact | not logged | `step2a/bench-2f-vs-main.log` | A(3349682db, 5) |
| F17 | 2,427 vs 2,536; ratio 0.957 (0.953–0.961), 5 rounds | 2f vs de0eb7b32 | 445d09420 | mit on, dom, n/a, G | exact | 2–4 (§9.9) | §9.9; log path not given there | A(main, 5) |
| F18 | 2,397 vs 2,536–2,546; ratio 0.945 (0.941–0.949), 5 rounds | 3a+3b vs de0eb7b32 | 4a8b8dfee | mit on, dom, n/a, G | exact | 3.3–3.4 (per-run) | `queue/po6-ipcB-s3-bench-2.log` | gate.sh: A(main, 5) |
| F19 | 2,257–2,716 vs 2,407–2,427; ratio 0.942 (0.930–1.124), 5 rounds | 3a+3b vs 2f | bb27b5237 (= a1d456820's tree) vs 445d09420 | mit on, dom, n/a, G | exact | 4.97 | `queue/po7-ipcB-s3r-bench-1.log` | gate.sh: A(main, 5) |
| F20 | 2,467–3,046 vs 2,526–2,536; ratio 0.977 (0.977–1.201), 4 rounds | ERAPS vs de0eb7b32 | po7/step5 (bc6fa7228) | mit on (ERAPS), dom, G+E | exact | 1.75 | `po7-ipcM/eraps-alt-1.log` | `po7-ipcM/bench.sh` → A(main, 4) |
| F21 | 5 rounds, no summary line (run ended early) | ERAPS vs de0eb7b32 | po7/step5 | mit on (ERAPS), dom, G+E | exact | 3.91 | `po7-ipcM/eraps-alt-2.log` | A(main) |
| F22 | 2,357–2,746 vs 2,417–2,996; ratio 0.979 (0.787–1.136), 6 rounds | ERAPS vs 2f | po7/step5 (529f32d51) vs 445d09420 | mit on (ERAPS), dom, G+E | exact | 7.19 | `po7-ipcM/eraps-alt-3.log` | A(445d09420, 6) |
| F23 | 2,207–2,217 vs 2,257–2,267, 4 rounds so far (running at 13:13) | ERAPS retake vs ccf72dd94 | 3b9b1e267 (landed) | mit on (ERAPS), dom, G+E | exact | 2.45 | `po7-ipcM/eraps-retake-10.log` | A(ccf72dd94, 10) |
| F24 | 2,267–2,816 vs 2,377–2,876; ratio 0.958 (0.950–1.171), 6 rounds | `VZEROALL` reset vs 3a+3b | po7/step5-vec 340596560 vs 4a8b8dfee | mit on, dom, G | exact | 0.32 | `po7-ipcM/vec-alt-1.log` | A(po6/step3, 6) |
| F25 | 1,697–1,707 (fp off), 4 rounds | timer re-arm + 64-bit division, fast path off | po7/step4 (c7e5a726c, 59a1b10b3 under aa7f0d80b) | mit on, dom, fp off, G | exact | 2.10–5.26 | `po7-step4/bench-ab1-off-{1..4}.log`, `abab-1.txt` | F(off), `po7-step4/abab.sh` |
| F26 | 1,318–1,558, median 1,328 (fp on), 4 rounds | step 4 fast path | po7/step4 (aa7f0d80b) | mit on, dom, fp on, G | exact | 2.22–4.79 | `po7-step4/bench-ab1-on-{1..4}.log`, `abab-1.txt` | F(on), `abab.sh` |
| F27 | 1,318 on vs 1,707–2,147 off, 3 rounds | step 4, second ABAB | po7/step4 | as F25/F26 | exact | 2.50–2.82 | `po7-step4/bench-ab2-*.log`, `abab-2.txt` | F(on)/F(off), `abab.sh` |
| F28 | 1,318–1,328 vs 2,407–2,427; ratio 0.548 (0.543–0.549), 3 rounds | step 4 (fp on) vs 2f | po7/step4 vs 445d09420 | mit on, dom, fp on vs n/a, G | exact | not logged | `po7-step4/alt-main-1.log` | F(on) + `--alternate main --rounds 3` |
| F29 | `call` (no dom) 3,705–3,745 on vs 5,613–7,021 off | step 4, across domains | po7/step4 | mit on, `IBPB`, G | exact | as F26/F27 | `po7-step4/abab-{1,2}.txt` | F(on)/F(off) |
| F30 | 1,588 | step 4 dev build 8 | po7/step4 (dev, 10:41) | mit on, dom, fp on, G | exact | not logged | `po7-step4/bench-dev8-on.log` | F(on) |
| F31 | 2,247–2,267 vs 2,257–2,277; ratio 0.996 (0.996–1.004), 3 rounds | A/A: main against itself (instrument floor) | ccf72dd94 vs ccf72dd94 | mit on, dom, n/a, G | exact | 9.90 | `po7-hotpaths/bench-aa-1.log` | A(main, 3) |
| F32 | 2,247 | one run with result record | ccf72dd94 | mit on, dom, n/a, G | exact | 4.94 | `po7-hotpaths/bench-record-1.log` | B |

Native null-call floor (`floor` line, same logs): 464 (hist, F7), 581/523 (hist, F11), 399 (exact, F14), 389 (exact, F30), 319 (exact, F31).

## 2. Span profiles (instrumented; the p50 is inflated by the stamps)

| Id | What | Tree | Instrumented `domain-call` p50 | Load | Log |
|---|---|---|---|---|---|
| P1–P9 | one direction stamped at 40 points, stamp cost subtracted; P7, P8 are §9.10's table; P9 splits `account` | `po7/prof` d0a378f75 / 593adc233 (on de0eb7b32 + step2f + po6/step3) | 2,796–3,535 | 1.05–13.28 | `po7-ipcM/prof-{1..9}.log` |
| P10 | extra-flush ablation (`PO7_EXTRA_FLUSH`) | 593adc233 | 3,445 | 0.78 | `po7-ipcM/prof-flush.log` |
| P11 | vector reset variants (`PO7_VEC`): default `XRSTOR` 77 ns, split 134, SSE+AVX only 72, `VZEROALL`+`LDMXCSR` 15 (§9.10) | 593adc233 | 2,886–3,395 (two runs without the line) | 1.22–2.03 | `po7-ipcM/prof-vec-{default,split,sseavx,vz,vzx,x87}.log` |
| P12 | step 4's own profile (charge about 30 ns, §9.10) | po7/step4 (prof marks) | see logs | not logged | `po7-step4/bench-prof-{1..12}.log` |
| P13 | the 2026-10-01 timing builds (`os-ipc/prof`, `prof2`): per-switch spans of §9.5 | 41bdef2ca, 4b5be585c | see logs | not logged | `os-ipc/prof*-smp*.log` |

## 3. seL4 (`~/.local/share/ferrix/sel4/`, sel4bench-manifest 80add415, seL4 c6ce4d2a)

Root task: 20,000 `seL4_Call` + `seL4_ReplyRecv` trips a sample run, 5 runs a boot, `lfence; rdtsc; lfence`. CPU model G+R, core 11, `run.sh <build> <pcid|nopcid>`; summary by `summarize.py`.

| Id | p50 | Build | Mitigations | Load | Sib | Log |
|---|---|---|---|---|---|---|
| S1 | 1,936 ticks = 440 ns (rounds 2 and 3); median of rounds 465 ns | matched-nopcid | no skim window, RSB fill on switch, no `IBPB` | 14.7–19.3 | 1–87% | `sel4/logs/r{1..4}-matched-nopcid-nopcid.log{,.meta}` |
| S2 | 1,936 ticks = 440 ns (best); median of rounds 450 ns | matched-nopcid, `+pcid,+invpcid` in the model | as S1 | 14.0–25.9 | 1–53% | `sel4/logs/r{1..4}-matched-nopcid-pcid.log` |
| S3 | 2,860 ticks = 650 ns (best); median 700 ns | defaults-nopcid | skim window on, no RSB fill, no `IBPB` | 5.0–19.3 | 5–55% | `sel4/logs/r{1..4}-defaults-*.log` |
| S4 | 3,960 ticks = 900 ns | ibpb-nopcid | as S1 + `IBPB` at every switch | 7.97–25.9 | 0–70% | `sel4/logs/r{1..4}-ibpb-*.log` |
| S5 | 2,244 ticks = 510 ns | matched-slow-nopcid (`FASTPATH=OFF`) | as S1 | 8.21 | 0–3% | `sel4/logs/x{1,2}-slow.log` |
| S6 | 1,892–1,936 ticks = 430–440 ns | matched-nopcid (fast-path control pair) | as S1 | 7.72–8.21 | 0% | `sel4/logs/x{1,2}-fast.log` |
| S7 | 1,760–1,804 ticks = 400–410 ns; 2,068 with sib 96% | norsb-nopcid | no skim, no RSB fill, no `IBPB` | 6.52 | 1%, 96% | `sel4/logs/y{1,2}-norsb.log` |
| S8 | 1,936 ticks; 2,288 with sib 96% | matched-nopcid (pair to S7) | as S1 | 6.52 | 8%, 96% | `sel4/logs/y{1,2}-matched.log` |
| S9 | 1,892 ticks = 430 ns, 5 runs | matched-nopcid, retake 2026-10-06 13:11 | as S1 | 5.32 | 0% | `sel4/logs/po7linux-try.log{,.meta}` |
| S10 | 1,760 ticks = 400 ns, 5 runs | norsb-nopcid, retake 2026-10-06 13:11 | as S7 | 5.21 | 0% | `sel4/logs/po7linux-try-norsb.log{,.meta}` |
| S11 | one-way `Call` + `ReplyRecv`: 748 + 748 ticks in one space, 1,320 + 1,320 across two (about 130 ns a direction for the switch) | matched-nopcid, sel4bench's own app | as S1 | see logs | see logs | `sel4/logs/r*-*.log` (sel4bench lines); §9.6 |

## 4. Redox and Linux (`~/.local/share/ferrix/redox-bench/`, summary `logs/summary.md`)

Redox OS 0.9.0 server image (kernel 0.5.12, sha256 in `SHA256SUM`), `rbench`, 5 × 20,000 samples after 2,000 warm-up, median over boots. CPU model G+R, core 11, `run.py`; Linux by `linux.sh` natively on the host (`taskset -c 11`, not in a VM).

| Id | p50 | What | System | Boots | Load (first boot) | Log |
|---|---|---|---|---|---|---|
| R1 | 1,965 ns (8,646 ticks; range 8,492–11,176) | scheme `SYS_WRITE` 8 B to a daemon and its answer | Redox, virtio disk | 4 | 8.96 | `redox-bench/logs/r{1..4}-redox.log{,.meta}` |
| R2 | 2,025 ns | the same by `SYS_CALL` | Redox | 4 | as R1 | as R1 |
| R3 | 1,475 ns | pipe ping-pong 8 B | Redox | 4 | as R1 | as R1 |
| R4 | 40 ns p50 (29 ns batched) | invalid system call | Redox | 4 | as R1 | as R1 |
| R5 | 80,000 ns (p90 108,595) | raw 4 KiB read of the virtio-blk scheme | Redox | 4 | as R1 | as R1 |
| R6 | 19,415 ns hot, 146,755 ns cold | 4 KiB `pread` through redoxfs | Redox | 4 | as R1 | as R1 |
| R7 | 1,940 / 2,010 ns | scheme write / call, AHCI root | Redox | 2 | see meta | `redox-bench/logs/` (redox-ahci-root) |
| L1 | 55 ns p50 (37 ns batched) | invalid system call | Linux host, native | 4 | see meta | `redox-bench/logs/r{1..4}-linux.log{,.meta}` |
| L2 | 2,105 ns (range 7,524–11,220 ticks) | pipe ping-pong 8 B | Linux host, native | 4 | see meta | as L1 |
| L3 | 3,590 ns | Unix stream ping-pong 8 B | Linux host, native | 4 | see meta | as L1 |
| L4 | 59,254 ns | `O_DIRECT` 4 KiB `pread` on the NVMe | Linux host, native | 4 | see meta | as L1 |
| L5 | 135 ns | 4 KiB `pread`, cached | Linux host, native | 4 | see meta | as L1 |

## 5. Owed

| Id | Measurement | Why | Blocked on |
|---|---|---|---|
| O1 | Ferrix vs seL4 matched, alternated in one invocation (`--against-sel4`), G+E | the comparison itself; F-rows and S-rows are separate runs | `--against-sel4` not built; step 4 not landed |
| O2 | step 4 (fp on) after its review and landing, A(main, ≥10) | F25–F28 are on an unreviewed WIP branch | consultant review of po7/step4 |
| O3 | Linux null call, pipe, futex and Unix-socket ping-pong as a guest under the same QEMU, KVM, G+R, core 11 | L1–L5 are native on the host | a Linux guest image with `rbench` |
| O4 | steps 1 to 2e re-measured with the exact bench, one ABAB each | F7–F13 are hist; their p50 resolves 233 ns | nothing (old trees build) |
| O5 | the F23 ERAPS retake to its end (10 rounds) | F20–F22 spreads contain 1.0 | running |
| O6 | ablations of each step-5 item ABAB against the full tree: `VZEROALL`, ERAPS, selector skip, fewer user pages, FSGSBASE | spans (P1–P11) are instrumented | the items not built yet beyond F22–F24 |
| O7 | load for F1–F6, F16, F28, F30 | not recorded in those logs | cannot be recovered; rerun |
| O8 | p90/p99 before and after the timer fix (c7e5a726c), fp on and off | p90 10–19 µs on every exact row | step 4 landing |
| O9 | a host with PCID (bare metal and KVM), with seL4 `KernelSupportPCID` | nazuna has no PCID | a machine |
| O10 | Pixel 7 (AArch64) and STM32MP157 (ARMv7-A) round trips, with seL4 where it builds | cross-hardware | AArch64 / ARMv7-A fast path; PO approval for hardware |
| O11 | a second Zen 5 host, same model | reuse by hardware key | a machine |
| O12 | bare-metal x86 Ferrix and seL4 | the VM's share of absolute figures | a machine |
| O13 | seL4 with FPU/vector switching on | the stricter match to Ferrix's vector contract | a seL4 build |
| O14 | an application-level figure dominated by server calls, before and after | a micro-benchmark only so far | choose the workload |
| O15 | the source log of F6 (§9.1's 6.5 / 2.6 µs) | not located | search `os-ipc/` and the gate INDEX of 2026-10-01 |
| O16 | agent-process figures per step: sessions, points, gate runs, review rounds, wall time | the effort side | extraction from `fleet/log`, `queue/INDEX`, `cert-consultant/reviews.md` |
