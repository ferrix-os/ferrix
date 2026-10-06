# Paper skeleton: agent-first, hardware-specific optimization of OS hot paths

Status: skeleton and draft text, 2026-10-06 (session po7-paper). Markdown,
so it builds with nothing new in the repository; it converts to the venue's
LaTeX class when a venue is chosen (pandoc, or by hand, outside the tree).
Citation keys are those of [references.bib](references.bib). Every claim in
§5 names its evidence or is marked **owed**. The research design is
[research-plan.md](research-plan.md); prior art is
[related-work.md](related-work.md).

## 1. Title options

1. *Fast Paths for Every Machine: Agent-Driven Specialization Makes
   Microkernel IPC Competitive Again*
2. *Liedtke's Labour, Automated: Per-Hardware IPC Fast Paths Written by
   Coding Agents*
3. *Hot-Path Instructions: Letting AI Agents Specialize a Certifiable
   Microkernel to Its Hardware*
4. *From 37 µs to seL4: An Agent-Built IPC Path for a Rust Microkernel*
   (only if the final figure reaches seL4's)

Option 1 states the thesis; option 2 places it in the literature; option 4 is
the honest title for an experience paper if the cross-hardware results are
not in.

## 2. Abstract (draft; figures in brackets are owed)

Microkernels pay for isolation at every server call, and the history of
microkernel performance is the history of making that call cheap: Liedtke's
L4 made IPC an order of magnitude faster than Mach's by designing it for one
processor, and seL4 keeps a hand-written fast path to this day. That labour
is specific to an ISA, a microarchitecture, a hypervisor's CPU model and a
set of speculative-execution defences, and it must be redone when any of them
changes. We ask whether AI coding agents can do it per machine. In Ferrix, a
Rust operating system with user-space services and a certification
programme, agent sessions guided by written hot-path instructions profiled,
budgeted and rebuilt the native channel round trip over [N] days and [N]
review rounds. With every mitigation on and both programs in one
speculation domain, the round trip under KVM on an AMD Zen 5 host went from
37 µs to [1.33 µs today; target under 400 ns], against 440 ns for seL4 with
matched protections and 1.97 µs for Redox measured on the same host. A
40-point per-direction profile attributes the remaining time; [N]% of the
savings after the generic steps came from decisions that hold only for this
processor and its virtual model (no PCID, ERAPS, `VZEROALL` against `XRSTOR`,
the cost of `IBPB` under KVM). We key each result by a fingerprint of the
hardware and the virtual CPU model, and show [when it transfers to a second
machine and when it does not]. A certification consultant, itself an agent,
reviewed every change to the kernel and found defects the gates did not.

## 3. Section outline

1. **Introduction.** The microkernel's tax is the round trip; the fix has
   always been a per-processor fast path; the bottleneck was people. Our
   claim and contributions: (i) a method — hot-path instructions, a
   hardware-keyed result record, profile-budget-ablate, matched baselines,
   review — that agents execute; (ii) the first case, the IPC round trip,
   from 37 µs to [x]; (iii) a decomposition of the gain into generic and
   hardware-specific; (iv) the process data: what it cost and what the
   review caught. One figure: the waterfall (F1).
2. **Background.** Why IPC cost decided the microkernel debate
   \cite{liedtke1993improving, liedtke1995construction, haertig1997performance,
   chen1993impact}; seL4's fast path and its verification
   \cite{klein2009sel4, heiser2016l4}; Spectre-era barrier costs on switches
   \cite{kocher2019spectre, lipp2018meltdown, ren2019analysis}; recent hardware
   proposals \cite{mi2019skybridge, du2019xpc, gu2020underbridge,
   vilanova2017dipc}. Ferrix in one paragraph: native channels, jobs,
   speculation domains, the certified item.
3. **The method.**
   - 3.1 Hot-path instructions: what an agent is told (the target figure,
     the matched baseline, the profile's format, the budget table, the rules
     for ablations and alternation, the review it must pass). Built by
     po7-hotpath; cite the format.
   - 3.2 The hardware fingerprint and the result record: what goes into the
     key (CPUID leaves, microcode, hypervisor, CPU model flags, kernel
     mitigation set), what a record holds (tree, figure, ratio, spread,
     load), and when a record is re-measured.
   - 3.3 The loop: profile → budget → design → consultant review → build →
     gates with negative controls → ABAB measurement → land.
   - 3.4 Roles: the customer decides targets and contracts; agents do the
     rest; the consultant is an agent with a ledger.
4. **Case study: the channel round trip on x86-64.**
   - 4.1 The starting point (§9.1) and where the time went.
   - 4.2 Generic steps: step 1 (sync wake, 0x1013, the clock), step 2 (the
     common path: `current()` by borrow, preemption count without a locked
     operation, the pending-work word, decode once, one state word, no
     global writes in the switch).
   - 4.3 Hardware-specific steps: the vector-state contract and the reset
     mechanism chosen by measured cost (`XRSTOR` 70 ns whatever it restores,
     `VZEROALL` 15 ns), FS base kept, no PCID on this host, ERAPS in place
     of the refill, the selector loads, the timer reprogram as a VM exit.
   - 4.4 The direct switch and the fast path; equivalence by test, not proof.
   - 4.5 The budget at each step (T1) and where it is now.
5. **Evaluation.**
   - 5.1 Setup and protocol (research-plan §3–§4).
   - 5.2 The waterfall (F1, T2).
   - 5.3 Against seL4, Redox and Linux (T3, F2).
   - 5.4 Attribution: spans vs uninstrumented half trip; ablations (F3).
   - 5.5 Generic vs hardware-specific share (T4).
   - 5.6 Cross-hardware [owed]: a PCID x86, the Pixel 7, the DK1 (T5).
   - 5.7 The tail (p90/p99) and its cause (F4).
6. **The agent process.** Sessions, points, gate runs, review rounds, wall
   time per step (T6); defects found by review (T7); where the agents
   erred (unreproduced gains, noise read as signal) and what caught it.
7. **Discussion.** What generalizes (the method; the budget discipline);
   what does not (a fast path is a second implementation in a certified item;
   the review cost per step rises as gains shrink); hardware keys and their
   limits; what "practical" needs beyond a micro-benchmark.
8. **Related work** (from related-work.md, condensed to one page).
9. **Conclusion.**

Acknowledgments must disclose the use of generative AI as the venue's
policy requires (§6 of this file).

## 4. Figures and tables

| Id | What | Data | State |
|---|---|---|---|
| F1 | Waterfall of the matched round trip, step by step, 37 µs → now, log scale, each bar labelled with its commit | research-plan M1–M12 | have, but two instruments (owed: re-measure steps 1–2e with the exact bench) |
| F2 | Bars: Ferrix (each stage), seL4 matched / defaults / `IBPB`, Redox, Linux null call and pipe (in the VM) | M12, M14, M16, M17 | Linux in the VM owed |
| F3 | Stacked bar of one direction by span, today vs after step 4 vs after step 5 vs seL4 | §9.10's budget, M13 | have (after-step-5 column is a budget, not a measurement) |
| F4 | CDF of round-trip latency before and after the timer fix, fast path on/off | `logs/po7-step4/bench-*` | have raw logs; plot owed |
| F5 | Effort per step: points and gate runs vs ns saved | §8 metrics | owed (extraction) |
| T1 | The per-direction span budget (§9.10 "One direction today" and "The budget against 400") | `logs/po7-ipcM/prof-7.log`, `prof-8.log` | have |
| T2 | Each step: commit, what changed, p50, ratio to the previous with spread, rounds, load | M3–M12 | have through M12 |
| T3 | Comparison kernels with configuration and protections | M14–M18 | Linux-in-VM owed |
| T4 | Each saving tagged generic or hardware-specific, with the property it depends on | §9.4–§9.10 | classification owed (draft below) |
| T5 | Cross-hardware: key, figure, ratio to seL4 on that machine, effort | — | owed |
| T6 | Agent process: sessions, points estimated/spent, gate runs, review rounds, wall time | fleet log, INDEX, ledger | owed (extraction) |
| T7 | Defects found by review, with whether a gate would have found them | ledger lines 192–398 | owed (classification) |

Draft of T4 (to be measured, not asserted):

| Saving | Generic or hardware-specific | The property it depends on |
|---|---|---|
| Sync wake, 0x1013, the deferred decision (step 1) | generic | — |
| `current()` by borrow, preemption count, pending-work word, decode once, one state word (2a–2e) | generic (cost of locked operations is per-core) | — |
| No global or locked writes in the switch (2f) | mostly generic | cost of a locked RMW on Zen 5 |
| Vector-state contract (3a) | generic contract, hardware-specific value | the XSAVE area size and `XRSTOR`'s cost |
| `VZEROALL` + `XINUSE` instead of `XRSTOR` (step 5) | hardware-specific | the CPU model enables x87, SSE, AVX only; `XRSTOR` costs 70 ns here regardless of what it restores |
| FS base kept, not read by `rdmsr` (3b) | x86-specific | no `FSGSBASE` in the gate's model |
| Selector loads skipped null to null (step 5) | x86-specific | descriptor-load cost under KVM, 140–160 ns a direction here |
| ERAPS instead of the return-stack refill (step 5) | Zen 5-specific | CPUID 0x8000_0021 EAX[24] and the hypervisor exposing it |
| Timer not re-armed for the same deadline | VM-specific | a local APIC timer write is a VM exit |
| 64-bit divisions where operands fit | generic | the cost of 128-bit division calls |
| PCIDs (step 3, deferred) | hardware-specific, absent here | CPUID.1:ECX[17] |
| Direct switch and fast path (step 4) | generic design, x86-64 entry | — |

## 5. Claims and their evidence

| # | Claim | Evidence | State |
|---|---|---|---|
| C1 | Before the work, the native round trip was 37 µs under KVM on nazuna | §9.1; `logs/os-ipc/base-smp1.log` | have (note: general calls, no domain) |
| C2 | seL4 with matched protections does a round trip in 440 ns on the same host, VM and core | §9.6; `~/.local/share/ferrix/sel4/logs/` | have |
| C3 | Redox 0.9.0 does a scheme round trip in 1,965 ns there, with no speculative defence | §9.6a; `redox-bench/logs/` | have |
| C4 | Landed work (steps 1–3b) brought the matched round trip to about 2.26–2.29 µs | M8 | have |
| C5 | Step 4's fast path brings it to about 1.33 µs | M12 | have, but unlanded and unreviewed; **owed**: the consultant's code review and the equivalence run on the measured tree |
| C6 | Ferrix's round trip is faster than Redox's with every defence on | C3, C5 (1.33 vs 1.97 µs) | have once C5 lands; today's landed figure (C4) is slower than Redox |
| C7 | Ferrix matches or beats seL4 with matched protections | — | **owed**; the budget (§9.10) projects 420–520 ns after step 5, and under 400 only with every item at its best |
| C8 | A per-direction profile explains the trip to within 10% | spans 1,209–1,300 vs 1,130 ns half trip | have, at 7–15% over; within 15%, not 10% |
| C9 | A substantial share of the late savings is hardware-specific | T4 draft; M9, M10, M11 | partly: M10 and M11 are measured, M9 is within noise; **owed**: ablations |
| C10 | Hot-path instructions transfer to another ISA with less effort | — | **owed** (RQ3) |
| C11 | Results keyed by a hardware hash are reusable on a same-key machine | — | **owed** (RQ4) |
| C12 | The key must include the virtual CPU model | nazuna has no PCID; ERAPS needs `+eraps`; `RDCL_NO` cleared by the model (§9.6) | have as observations; **owed** as an experiment |
| C13 | The review loop found defects the gates did not | F-60, the spec domain's F1 blocker, 3b's write-skip, the 65-signal cap, a lost close wake (handover 2026-10-03; ledger) | have, uncounted; **owed**: T7 |
| C14 | The process took [N] sessions, [N] points, [N] gate runs, [N] review rounds | fleet log, INDEX, ledger | **owed** (extraction, research-plan §8) |
| C15 | Every landed step kept the certification evidence: rows, checks, negative controls, coverage carried | landing lines in `fleet/log`; ledger | have |
| C16 | The `IBPB` barrier costs about 230 ns a switch under KVM here | §9.6 | have |
| C17 | The round trip is below a Linux pipe ping-pong in the same VM | Linux measured only natively (2,105 ns) | **owed** |
| C18 | Faster IPC changes an application-level figure | — | **owed** |

## 6. Venues

Checked on 2026-10-06 against the venues' own pages unless marked.

| Venue | Deadline | Fit | Source |
|---|---|---|---|
| **HotOS XXI** (2027), Burghausen, May 24–26, 2027 | paper or panel proposal by **Feb 1, 2027** | Best first venue: a position plus early results ("agents make per-machine fast paths affordable") does not need the cross-hardware data yet | sigops.org/s/conferences/hotos/2027 (verified) |
| **OSDI '27**, Baltimore, July 7–9, 2027 | abstracts **Dec 1, 2026**, papers **Dec 8, 2026**; notification Mar 16, 2027; 12 pages | Full paper; needs C7 or a clear account of why not, plus RQ3/RQ4 data. The CFP calls submission of work "wholly or largely generated by AI" fraud, while allowing AI editing tools: the paper's text must be written by its human authors, and the agents' role described as the system under study | usenix.org/conference/osdi27 CFP (verified) |
| **ASPLOS '27**, Heraklion, April 11–15, 2027 | September cycle closed (Sep 9, 2026); the next cycle is ASPLOS '28's | Architecture angle (hardware-keyed specialization); ACM policy requires disclosing generative-AI use in the acknowledgments | asplos-conference.org CFP (verified) |
| **EuroSys 2027**, Rabat, April 19–23, 2027 | fall cycle closed (papers Sep 24, 2026) | The EuroSys 2028 spring cycle (about May 2027) is the realistic EuroSys slot | 2027.eurosys.org (fall dates verified; 2028 dates **unverified**) |
| **SOSP 2027**, October 2027 | **unverified** (TBD at this writing; SOSP's deadline is usually in April) | The strongest venue for a full paper with cross-hardware results | getpaperpilot.com aggregator only |
| **ACM SIGOPS ATC 2027** | **unverified** | USENIX ended ATC after 2025; SIGOPS continues it as ACM SIGOPS ATC | news reports (Wikipedia, admin-magazine); no CFP found |
| **APSys 2027** | **unverified** (APSys 2026's was May 20, 2026) | Workshop; good for the process data alone | aggregator only |
| **ML for Systems workshop @ NeurIPS** | 2026 deadline passed (Aug 30, 2026) | The agent-process angle; 2027 dates unverified | mlforsystems.org |

Recommended order: HotOS (Feb 1, 2027) with the x86 case and the method;
then OSDI '27 (Dec 8, 2026) only if C5 lands and C7 is measured by mid-November,
otherwise SOSP 2027 or EuroSys 2028 with the cross-hardware experiment.
