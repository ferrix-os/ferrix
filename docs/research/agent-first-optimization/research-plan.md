# Research plan: agent-first, hardware-specific optimization of OS hot paths

Status: draft, 2026-10-06 (session po7-paper). Docs only. Every figure below
names the log or section it comes from; a figure with no source is marked
*owed*. Paths under `~/` are on nazuna, the development host (AMD Ryzen 9
9900X, Zen 5, 12 cores, 24 threads).

Section signs (§9.1 to §9.10) refer to `docs/OPAQUE-KERNEL.md` on `main`
(§9.10 since ccf72dd94).

Companion files: [paper.md](paper.md) (the skeleton and the claim list),
[related-work.md](related-work.md) and [references.bib](references.bib).

## 1. The thesis, stated so it can fail

Microkernels lost to monolithic kernels on the cost of crossing between
protection domains, not on design. Liedtke showed in 1993 that a careful,
processor-specific IPC path closes most of that gap, and seL4 keeps such a
path today. What made that hard to repeat was the *labour*: the fast path is
specific to one processor, one ABI and one set of speculative-execution
defences, it must be re-derived when any of them changes, and in a
certifiable system every change needs evidence.

The thesis: **AI coding agents, given written hot-path instructions and a
review loop, can do that labour per hardware target, cheaply enough that a
microkernel can carry a path tuned to each machine it runs on, and the
results can be shared keyed by a fingerprint of the hardware.** Ferrix, a
Rust operating system whose services run as user-space servers over native
channels (`docs/OPAQUE-KERNEL.md`), is the vehicle; its channel round trip
(`channel_write_read`, 0x1013) is the first hot path.

What would refute it:
- the round trip stalls well above seL4's with matched protections, so the
  agents reproduce generic engineering but not the last factor;
- the effort per hardware target does not fall from the first target to the
  second, so there is no "agent-first" advantage over a person doing it once;
- results keyed by a hardware hash do not transfer even between machines with
  the same key, or the key cannot be made precise enough to be useful;
- the review loop does not catch the agents' errors, so the speed is bought
  with defects.

## 2. Research questions and hypotheses

**RQ1 (reach).** How close can agent-driven, per-hardware specialization bring
a microkernel's IPC round trip to (a) seL4's on the same machine with
matched protections, and (b) the monolithic floor: a null system call and
the cheapest Linux same-host round trip (pipe or futex ping-pong)?

- *H1a.* On nazuna under KVM, the matched native round trip reaches seL4's
  measured 440 ns within 1.10× (the perf row the plan fixes, §9.6), and the
  customer's 400 ns target if every step-5 item lands (§9.10's budget).
- *H1b.* Every remaining nanosecond above seL4 is attributable to a named
  item in the per-direction budget, i.e. the budget, not luck, explains the
  result (the spans add to within 10% of the uninstrumented half trip).
- *H1c.* The matched round trip is below a Linux pipe ping-pong measured in
  the same VM configuration, i.e. a server call costs less than the
  monolithic kernel's cheapest comparable cross-process exchange.

**RQ2 (where the gains come from).** Which share of the improvement is
generic microkernel engineering (fewer locks, a direct switch, a register
fast path: what Liedtke and seL4 already teach) and which share is specific to
the hardware (no PCID, ERAPS, the cost of `XRSTOR` against `VZEROALL` on this
core, `IBPB` under KVM, selector loads)?

- *H2.* At least a quarter of the time removed after step 2 comes from
  decisions that depend on a measured property of the processor or its
  virtual model, and those decisions differ on at least one other target.

**RQ3 (transfer).** Do hot-path instructions written for agents transfer to
another architecture with less effort than the first target took?

- *H3.* Porting the fast path to AArch64 (the Pixel 7) and ARMv7-A (the
  STM32MP157) takes measurably fewer agent points, review rounds and gate
  runs per unit of speedup than x86-64 did, because the instructions carry
  the method (profile, budget, ablation, matched baseline, review) and only
  the hardware facts change.

**RQ4 (reuse by hardware key).** When does a result keyed by a hash of the
hardware stay valid on another machine with the same key, and when not?

- *H4a.* Two machines with the same key reproduce each other's step-5
  choices (which items win, by sign) and their ratios to seL4 within the
  noise band.
- *H4b.* The key must include the virtual CPU model and the hypervisor, not
  only the host CPUID: the same host gives different answers under KVM with
  and without `+pcid`, `+eraps` or `FSGSBASE` exposed.
- *H4c.* Results are not reusable across microcode or kernel-mitigation
  changes that alter barrier costs (`IBPB`, the return-stack refill); the key
  must name them or the record must be re-measured.

**RQ5 (process and assurance).** What does the agent process cost, and does
the certification review act as a guard against the agents' own errors?

- *H5a.* Agent effort (points, sessions, gate runs) per nanosecond saved is
  roughly constant over steps 2 to 4 and rises in step 5, as the remaining
  items get smaller and each needs its own review.
- *H5b.* The review loop catches defects that the gates do not, at a rate
  that justifies its cost (counted from the ledger: findings per review round,
  and how many would have reached `main`).

## 3. What the system and the measurement are

- **System under test.** Ferrix `main` and the unlanded branches named in §6,
  x86-64, release build, under QEMU with KVM on nazuna, one virtual processor
  pinned to one host core (11), the gate's CPU model plus `+rdtscp` and
  `+rdctl-no` (§9.6). nazuna has no PCID (CPUID.1:ECX[17] clear), has
  INVPCID, ERAPS, TCE and PKU.
- **The figure.** `cargo xtask bench-ipc --release --accel kvm --smp 1`, line
  `domain-call`: a native client's `channel_write_read` answered by an echo
  server's own, eight bytes, both programs in one speculation domain, every
  mitigation on (AutoIBRS and STIBP at boot, the return-stack refill at
  every address-space switch, or ERAPS where reviewed). 20,000 timed round
  trips a run; `lfence; rdtsc; lfence` around each sample since
  `bench-exact` (3349682db); p50 from sorted samples. Before 3349682db the
  p50 came from a histogram floored to an eighth of a power of two, which
  resolved 233 ns at 2.5 µs and could read below the minimum (e.g.
  `~/.local/share/ferrix/logs/ipc-step1/bench-on.log`: min 3,066, p50 3,021).
- **Matched protections.** The promise is the matched figure (customer,
  2026-10-02, §9.5): both kernels with the same defences, Ferrix's programs in
  one speculation domain. `--mitigations off` and `IBPB` at every switch are
  reported beside it, never as the result.
- **The comparison kernels,** measured on the same host, QEMU, KVM, CPU model
  and pinned core:
  - seL4 (sel4bench-manifest 80add415, seL4 c6ce4d2a), a root task timing
    20,000 `Call`/`ReplyRecv` trips the way `ipc-bench` does (§9.6;
    `~/.local/share/ferrix/sel4/`);
  - Redox OS 0.9.0 (kernel 0.5.12), a scheme request and its answer
    (§9.6a; `~/.local/share/ferrix/redox-bench/`).
  - Linux: so far only on the host, natively (§9.6a: a null call 37 ns, a pipe
    ping-pong 2,105 ns). *Owed:* Linux as a guest in the same VM setup.

## 4. Experimental protocol

1. **Alternation.** Every comparison of two trees is ABAB in one invocation:
   `bench-ipc --alternate <ref> --rounds N`, which builds both and runs them in
   turn, so host load drifts over both equally. The reported figure is the
   median of the per-round ratios of p50s, with the spread (min to max
   ratio). Absolute figures across hours are never compared.
2. **Runs.** At least 5 rounds per comparison (most existing comparisons have
   5 or 6; step 4's has 4 + 3). For the paper: 10 rounds per pair, three
   independent invocations on different days, which gives a bootstrap 95%
   confidence interval of the median ratio. *Owed:* the bootstrap script
   (a host-side Python reading the round lines; no new repository
   dependency).
3. **Percentiles.** p50 is the headline; p90 and p99 are reported, and their
   cause named. Today's p90 of 10 to 19 µs is the local APIC timer reprogram,
   an exit under KVM, on 10 to 20% of directions (§9.10); c7e5a726c on
   `po7/step4` removes most of it. A paper figure without the p90 hides that.
4. **Load.** Every round logs the 1-minute load and the busy share of core
   11's SMT sibling. A busy sibling adds 15 to 20% (seL4, §9.6) to 30% (Redox,
   §9.6a). Rounds with the sibling over 20% busy are rerun, not dropped
   silently; the count of reruns is reported.
5. **Attribution.** The timing build (`po7/prof`, never lands) stamps 40
   points of one direction and subtracts the stamp's own 7 to 9 ns; a span
   is quoted as its mean up to p90 (the guest TSC moves in 10 ns steps).
   Acceptance: the spans add to within 15% of half the uninstrumented
   round trip on the same tree (today 1,209 to 1,300 against 1,130 ns,
   §9.10). Ablations (one item skipped at a time, ABAB against the full tree)
   check each span's cost without the stamps' distortion.
6. **Equivalence of the fast path.** `ferrix.fastpath=on|off` on the same
   image; `test-ipc-equiv` must show every observable result equal both ways
   (§9.7 part 6). A speedup only counts if equivalence and the negative
   controls passed on the measured tree.
7. **Floors.** On each target: a null native call (`bench-ipc`'s `floor`),
   a Linux null system call and a Linux pipe and futex ping-pong in the same
   VM, and seL4's matched round trip and its round trip in one address space
   (the switch's cost).

## 5. Threats to validity

- **Measured in a virtual machine.** Every x86 figure is under KVM. A `CR3`
  write and the TLB refill after it go through nested paging (two-level
  walks), timer reprograms are exits, and `IBPB` costs 230 ns here, not §9.1's
  2 µs. Mitigation: both kernels under the same VM, ratios not absolutes,
  plus bare-metal runs owed on x86 (a second PC booted natively) and on Arm
  (the phone and the board are bare metal or crosvm, which must be stated).
- **One host.** All x86 data come from nazuna. H4 cannot be tested without a
  second machine with the same and with a different key.
- **The CPU model.** The gate's model hides PCID (the host has none), names
  ERAPS only if `+eraps` is added, and clears `RDCL_NO` unless told. The
  model is part of the hardware key (H4b).
- **Noise and a shared host.** nazuna runs other sessions' builds. Ratios,
  alternation and sibling-load logging (§4) are the defence; several existing
  runs show single rounds 10 to 25% off (e.g.
  `~/.local/share/ferrix/logs/po7-ipcM/eraps-alt-3.log`, spread 0.787 to 1.136),
  so medians of at least 5 rounds are required.
- **The instrument changed mid-series.** `bench-exact` (3349682db) replaced
  the histogram p50. Figures before it (3,021 and 2,556 ns) and after it
  (2,536 ns for the same code) differ by method; the paper reports the
  series in two segments, or re-measures the early steps with the exact
  bench (*owed*, cheap: the old commits still build).
- **The baselines are not the same workload.** seL4's root task passes one
  register word; `ipc-bench` passes eight bytes through `channel_write_read`
  with the vector-state contract. Redox's request goes through a scheme
  daemon. Linux's pipe moves bytes through a buffer. The paper states each.
- **seL4's configuration.** seL4 is measured as built for this processor
  without PCID (it refuses to boot with `KernelSupportPCID` on nazuna) and
  with its FPU off, as sel4bench runs. A seL4 with lazy or eager FPU state
  switching would be the stricter match (*owed*).
- **The agents' own errors.** Agents misread logs, mis-attribute noise to
  their change, and re-run until green. Defences that exist: negative
  controls that must fire, gates on the exact landed tree, a certification
  consultant session that reads diffs and logs (the ledger), and alternation
  inside one invocation. The paper must report the cases where the
  defences fired (F-60, the spec domain's F1 blocker, 3b's write-skip, the
  65-signal cap, two lost wakes) and at least one case where an agent's
  reported figure did not reproduce (e.g. ERAPS's −50 to −100 ns against a
  median ratio of 0.979 with spread 0.787 to 1.136).
- **Who did the work.** Agents wrote the code, the reviews and the
  measurements; the customer made the decisions (targets, ABI contract,
  whether a fast path is acceptable). The paper must separate the two, and
  must not claim the agents chose the goals.
- **Construct validity of "practical".** A micro-benchmark round trip is
  necessary, not sufficient. *Owed:* one application-level measurement where
  server calls dominate (the block ring's 4 KiB read, a compositor frame, or
  a VFS-server path), before and after.

## 6. The measurements we have

x86-64, KVM, nazuna, `domain-call` p50 unless named. "Exact" = after
3349682db.

| # | What | Figure | Source |
|---|---|---|---|
| M1 | Before step 1: write, wait and read on each side, no domain, mitigations on | 37 µs | §9.1; `~/.local/share/ferrix/logs/os-ipc/base-smp1.log` (`trip` p50 37,191) |
| M2 | The branch's 0x1013 across domains, mitigations on / off | 6.5 / 2.6 µs | §9.1; `logs/os-ipc/` |
| M3 | Step 1 (d750fd452) | 3,021 ns (histogram) | `logs/ipc-step1/bench-on.log` |
| M4 | 2b, against its base | 2,556 vs 3,021 ns (histogram) | `logs/step2a/2b-bench*.log` |
| M5 | 2a, 2c, 2d, 2e | 2,556 ns; savings below the histogram's resolution | `logs/step2a/2a-bench*.log`, §9.9 |
| M6 | `main` with the exact bench (3349682db) | 2,536–2,566 ns | `logs/step2a/bench-2f-vs-main.log` |
| M7 | 2f against 3349682db, 5 rounds ABAB | 2,417 vs 2,546; ratio 0.949 (0.942–0.957) | `logs/step2a/bench-2f-vs-main.log`; §9.9 gives 0.957 for the run against de0eb7b32 |
| M8 | 3a+3b (a1d456820) against 2f, 5 rounds | 2,257–2,357 vs 2,427; ratio 0.942 (0.930–1.124) | `logs/queue/po7-ipcB-s3r-bench-1.log` |
| M9 | ERAPS (`po7/step5`, unlanded) against 2f, 6 rounds | ratio 0.979 (0.787–1.136) | `logs/po7-ipcM/eraps-alt-{1,2,3}.log` |
| M10 | `VZEROALL` reset (`po7/step5-vec`, unlanded) against 3a+3b, 6 rounds | ratio 0.958 (0.950–1.171) | `logs/po7-ipcM/vec-alt-1.log` |
| M11 | Timer re-arm and 64-bit division (`po7/step4`, unlanded) | 2,267 → about 1,700 ns, fast path off | c7e5a726c and 59a1b10b3's messages; `logs/po7-step4/` |
| M12 | Step 4's fast path (`po7/step4`, unlanded, unreviewed), 7 rounds | 1,318–1,558 (median 1,328) on, 1,697–2,147 off; cross-domain `call` 3,715 vs 5,643 | `logs/po7-step4/abab-{1,2}.txt` |
| M13 | One direction, 40 stamped spans | table in §9.10 | `logs/po7-ipcM/prof-7.log`, `prof-8.log`, `prof-9.log`, `prof-vec-*.log` |
| M14 | seL4 matched / defaults / `IBPB` / fast path off / no refill | 440 / 650 / 900 / 510 / 400–410 ns | §9.6; `~/.local/share/ferrix/sel4/logs/` |
| M15 | seL4's address-space switch, a direction | about 130 ns (refill 20) | §9.6 |
| M16 | Redox scheme round trip; pipe; null call | 1,965 ns; 1,475 ns; 29 ns | §9.6a; `~/.local/share/ferrix/redox-bench/logs/` |
| M17 | Linux on the host, natively: null call; pipe ping-pong | 37 ns; 2,105 ns | §9.6a |
| M18 | `IBPB` under KVM, a switch | about 230 ns | §9.6 |
| M19 | Correctness evidence per landing | gates PASSED, negative controls FIRED | `logs/queue/INDEX` (tags `po6-ipc*`, `po7-ipc*`, `osc7-*`), `logs/ipc-step1/`, `logs/step2a/` |
| M20 | Reviews | about 30 IPC entries, one NOT YET with a blocker | `~/.local/share/ferrix/cert-consultant/reviews.md`, lines from 192 |

## 7. The measurements owed, by value to the paper

1. **The final x86 figure, matched, against seL4 in one alternated run**
   (`--against-sel4`, step 5's perf row). Without it H1a is a projection.
   Needs step 4 reviewed and landed, then step 5's items.
2. **Linux in the same VM**: null system call, pipe and futex ping-pong, and a
   Unix-socket request/response, under the same QEMU, KVM, CPU model and
   pinning. The monolithic floor of RQ1(b) and H1c rests on it; today's Linux
   figures are native on the host, which flatters Linux.
3. **One target other than nazuna**, with its own seL4 figure: an x86 PC with
   PCID (H2, H4b: the PCID step only exists there), or the Pixel 7 under
   crosvm. Without it RQ3 and RQ4 are untested and the paper is about one
   machine.
4. **A re-measurement of steps 1 to 2e with the exact bench**, so the
   waterfall is one instrument (the old commits build; one evening of ABAB
   runs).
5. **Ablations of every step-5 item**, each ABAB against the full tree,
   so the waterfall's last bars are measured, not budgeted (H1b, H2).
6. **The agent-effort table** (§8), extracted from the logs, with points
   estimated against spent for each step.
7. **An application-level figure** dominated by server calls, before and
   after (construct validity).
8. **The tail:** p90/p99 before and after the timer fix, with the cause.
9. **A second x86 machine with the same key**, for H4a.
10. **Bare metal on x86**, to bound the VM's distortion of the absolute
    figures.
11. **seL4 with FPU switching enabled**, for the stricter match.

## 8. Metrics for the agent process

Counted per step (1, 2a to 2f, 3a/3b, 4, each step-5 item), from these
sources:
- *Sessions and roles:* the session names in commit messages, the fleet log
  (`~/.local/share/ferrix/fleet/log`: `TAKEN`, `landed`, `RELEASED` lines with
  session names) and the ledger's reviewer headers (e.g. "reviewer:
  po7-ipcM's consultant"). So far: os-c7, os-86, os-35, os-ad (consultant),
  ferrix-fe/7b (coordinator), sessions A and B, po6-ipcA, po6-ipcB, po7-ipcA,
  po7-ipcB, po7-ipc4, po7-ipcM, po7-bridge (PO).
- *Points:* estimated in §9.5 (80 to 129 in all) and §9.8, against spent,
  from the commit and log timestamps (one point = 20 to 30 minutes of one
  session; CONVENTIONS.md rule 9 asks every report for both).
- *Gate runs:* `~/.local/share/ferrix/logs/queue/INDEX` lines by tag prefix
  (137 lines with an IPC tag on 2026-10-06: `osc7-*` 41, `po6-ipc*` and
  `po7-ipc*` 96; 21 of them read FAILED, which includes negative controls
  that must fail, so each needs reading), plus the stand-alone gates in `logs/ipc-step1/` and
  `logs/step2a/` (their own `gate.sh`), with PASSED/FAILED and reruns.
- *Review rounds:* ledger entries per piece and their verdicts (NOT YET, OK
  IF, MET, OK TO LAND), and the conditions per verdict. Defects found by
  review and not by a gate are listed separately (H5b).
- *Wall time:* from the first commit of a step to its `landed` line.
- *Tokens or cost:* *owed*; the session transcripts under
  `~/.claude/projects/` hold the usage per turn; a script to sum them per
  session is owed.
- *Human decisions:* the customer's decisions per step (§9.5, §9.6, §9.7,
  the handover), counted, so the paper can say how much steering the agents
  needed.

## 9. The cross-hardware experiment

| Target | Why | What exists | What is owed |
|---|---|---|---|
| nazuna, Zen 5, KVM | the first target | everything in §6 | the final figure, Linux in the VM |
| An x86 PC with PCID (Intel or AMD), bare metal and KVM | the PCID step (3), H4b, and bare metal | nothing | a machine; seL4 with `KernelSupportPCID`; the hardware key |
| Pixel 7 (Tensor G2, AArch64), crosvm and bare metal | a second ISA, real hardware with ASIDs | Ferrix boots all 8 cores; native programs run; Ferrix self-hosts on it | AArch64 fast path (§9.5 "not in this plan"), `bench-ipc` on the phone, seL4 for AArch64 on the same VM (crosvm) — the phone is shared, and its use needs the PO |
| STM32MP157D-DK1 (Cortex-A7, ARMv7-A) | an embedded target with no speculation mitigations of note and two cores | Ferrix boots; QEMU runs at `--smp 2` | `bench-ipc` on the board, an ARMv7-A path, seL4 for the board if it builds |

The protocol per target is §4 unchanged; the hot-path instructions
(po7-hotpath) are given to a fresh agent session with only the target's
facts changed, and the effort is counted as in §8 so H3 compares like with
like.
