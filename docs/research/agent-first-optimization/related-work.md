# Related work

Status: draft, 2026-10-06 (session po7-paper). Keys refer to
[references.bib](references.bib). Every number quoted below was read in the
work's abstract, on its proceedings page or on the page the bib entry names.
Where a work's numbers were not read, none are given. Entries whose
bibliographic details are partly unverified are flagged in the bib file and
marked *(partly unverified)* here.

The paper's own contribution, to set each paragraph against: AI coding
agents, given written hot-path instructions, profile and rebuild one OS hot
path (the native IPC round trip of Ferrix, a Rust microkernel-style OS with
a certification programme). They specialize it to one processor, its
virtual CPU model and its speculative-execution defences, under a review
loop. Results are keyed by a hardware fingerprint so others can reuse them.

## 1. IPC cost and the microkernel debate

**Liedtke, *Improving IPC by Kernel Design* (SOSP '93)
\cite{liedtke1993improving}.** Liedtke designed the whole L3 kernel around
IPC, from its architecture down to the code. The abstract says most
µ-kernels then needed about 100 µs for a short message at 50 MHz, where L3
needed 5 µs, "a twentyfold improvement". Against Mach the factor ranged
from 22 for 8-byte messages to 3 for 4 KiB. The abstract also says that
"hardware specific details influence both the design and implementation".
*Difference:* our thesis is that this per-processor labour, which Liedtke
did by hand for one machine, can be repeated by agents for each target. Our
starting point is a design not built around IPC, and our protections
include Spectre-era barriers that did not exist in 1993.

**Liedtke, *On µ-Kernel Construction* (SOSP '95)
\cite{liedtke1995construction}.** Liedtke argued that the inefficiency and
inflexibility of µ-kernels "is not inherited from the basic idea but mostly
from overloading the kernel and/or from improper implementation". The paper's body
also argues that µ-kernels are inherently processor-specific, which is not
in the abstract and is to be quoted from the PDF before the paper relies on
it.
*Difference:* we accept the processor-specific conclusion and ask whether
agents make it affordable to have one tuned path per processor. A recorded
hardware key says which path applies where.

**Härtig et al., *The Performance of µ-Kernel-Based Systems* (SOSP '97)
\cite{hartig1997performance}.** The authors ran Linux on L4 (L4Linux) and
measured application throughput in the AIM benchmarks. L4Linux came within
5% of native Linux, while MkLinux on Mach paid 5 to 7 times that penalty.
This was the strongest evidence that a fast-IPC microkernel can host a
general-purpose system. *Difference:* their evidence is application-level
on a kernel tuned by its designers. Ours is so far a micro-benchmark on a
kernel tuned by agents, and the application-level figure is owed
(research-plan §7).

**Chen and Bershad, *The Impact of Operating System Structure on Memory
System Performance* (SOSP '93) \cite{chen1993impact}; Accetta et al.,
*Mach* (USENIX Summer 1986) \cite{accetta1986mach} *(partly unverified)*.**
Chen and Bershad compared memory-system traces of Ultrix with Mach 3.0 and
its UNIX server. Mach executed "substantially more non-idle system
instructions" and had a higher memory cycles per instruction. Mach itself is
the first-generation microkernel whose IPC cost gave microkernels their
reputation. *Difference:* Mach is the cautionary baseline. We do not
re-measure it, and we do not claim our method would have saved Mach's
design.

**Bershad et al., *Lightweight Remote Procedure Call* (SOSP '89 / TOCS 1990)
\cite{bershad1990lrpc}.** LRPC optimized calls between protection domains on
one machine for the common case of small, simple arguments. It reported "a
factor-of-three performance improvement over more traditional approaches".
*Difference:* LRPC is the classic "optimize the common case of the call"
result, and our step 4 fast path is in its tradition (24 bytes, no handles,
waiting peer). The new elements are the agents, the per-machine budget and
the matched speculative defences.

**Elphinstone and Heiser, *From L3 to seL4* (SOSP '13)
\cite{elphinstone2013l3}; Heiser and Elphinstone, *L4 Microkernels: The
Lessons from 20 Years of Research and Deployment* (TOCS 2016)
\cite{heiser2016l4}.** These are retrospectives on L4's design decisions.
They conclude that minimality and high IPC performance remain the main
drivers. §9.5 summarizes seL4's fast path from the 2016 paper: message in
registers, one capability lookup, a direct switch to a waiting receiver, no
run queue (to be checked against the paper's text before quoting).
*Difference:* that fast path is our design reference (§9.5, §9.7). What we
add is the question of who builds and re-tunes it, and evidence of what it
costs when agents do.

**Klein et al., *seL4* (SOSP '09) \cite{klein2009sel4}; Klein et al.,
*Comprehensive Formal Verification of an OS Microkernel* (TOCS 2014)
\cite{klein2014comprehensive}.** seL4 has a machine-checked proof of
functional correctness from the abstract specification to the C code. Its
performance is "comparable to other high-performance L4 kernels". The 2014
paper extends the proof to, among other things, a formally verified IPC
fast path. *Difference:* seL4 proved its fast path equal to the slow path.
Ferrix argues the same equivalence by test: a boot switch, an equivalence
suite, and a negative control per condition (§9.7 part 6). That is weaker,
and the customer accepted it as a residual risk. Our seL4 figures are our
own measurements on our host (§9.6), not seL4's published ones.

**seL4's published performance \cite{sel4perf}, whitepaper
\cite{heiser2020sel4wp}.** sel4bench reports one-way IPC in cycles. For the
i7-6700 Skylake at 3.4 GHz without its Meltdown defence, it gives a call of
741 cycles and a reply of 598 (page generated 2026-09-22, manifest
f1b98e62). *Difference:* we use these only to explain why we re-measured.
Our matched figure on nazuna under KVM is 440 ns a round trip. sel4bench's
own one-way figures there read a `cpuid` per sample, which exits the
virtual machine (§9.6).

**Fiasco and Fiasco.OC \cite{hohmuth1998fiasco, hohmuth2001pragmatic,
lackorzynski2009taming} *(partly unverified)*.** Fiasco reimplemented the
L4 interface in C++ at TU Dresden. The USENIX '01 paper describes lock-free
and wait-free synchronization with bounded execution times for all kernel
operations, and reports performance competitive with the original assembly
kernel. Fiasco.OC added capabilities as universal resource access control.
*Difference:* Fiasco showed that a high-level-language L4 can be fast. Rust
in Ferrix is the same bet, and the agents are the new variable.

**Steinberg and Kauer, *NOVA* (EuroSys '10) \cite{steinberg2010nova}.** NOVA
is a microhypervisor with a TCB "at least an order of magnitude smaller" than
existing systems, and it "outperforms contemporary full virtualization
environments". *Difference:* NOVA is a hand-built, minimal-TCB design. It
belongs here as evidence that a capability microkernel and performance can
coexist; it says nothing about how such kernels are tuned per machine.

**Zircon (Fuchsia) \cite{fuchsia_zircon, fuchsia_microbenchmarks}.** Zircon
is a capability-based kernel whose channels Ferrix's native channels
resemble (`channel_call`, `channel_write_read`). Fuchsia's microbenchmarks
time Zircon's syscalls and IPC primitives. No peer-reviewed Zircon IPC study
was found. SkyBridge \cite{mi2019skybridge} and XPC \cite{du2019xpc} both
evaluate on Zircon. *Difference:* Ferrix's starting point (37 µs; §9.1) was
a Zircon-like general path. The work described here is what it took to take
that path toward seL4.

**Redox OS \cite{redox}.** Redox is a Rust microkernel OS whose services are
user-space scheme daemons. No peer-reviewed paper was found. We measured
Redox 0.9.0 on our host: a scheme round trip of 1,965 ns with no
speculative-execution defence in its kernel (§9.6a). *Difference:* Redox is
the closest system in language and structure, and the second baseline. Our
claim to beat it holds only for the unlanded fast path (1.33 µs).

**Chen et al., *HongMeng* (OSDI '24) \cite{chen2024hongmeng}.** HongMeng is
a commercial general-purpose microkernel, compatible with Linux's API and
ABI and deployed on tens of millions of devices. It names the bottlenecks as
"IPC frequency, state double bookkeeping among OS services, and capabilities
that hide kernel objects". *Difference:* HongMeng is the strongest current
evidence that microkernels are practical. Its engineering is by a large
team, while ours tests whether agents can do the per-hardware part.

**Rust OS designs: Theseus (OSDI '20) \cite{boos2020theseus}, RedLeaf
(OSDI '20) \cite{narayanan2020redleaf}, Tock (SOSP '17)
\cite{levy2017tock}.** These use Rust's type and memory safety to restructure
the OS. RedLeaf isolates through language-based domains instead of address
spaces, and its drivers perform comparably to DPDK and SPDK. Tock combines
MPU protection with type safety on microcontrollers. *Difference:* they move
isolation into the language to avoid the cost of crossing address spaces.
Ferrix keeps hardware address spaces and speculative barriers and lowers the
cost of crossing them.

## 2. Hardware support for fast cross-domain calls

**SkyBridge (EuroSys '19) \cite{mi2019skybridge}.** SkyBridge uses VMFUNC
and a small Rootkernel beneath the microkernel, so that a call switches
address spaces without entering the kernel. It was built on seL4, Fiasco.OC
and Zircon. It "improves the speed of IPC by 1.49x to 19.6x for
microbenchmarks". *Difference:* SkyBridge changes the mechanism, and needs
virtualization features and a layer beneath the kernel. We keep the
conventional mechanism (`SYSCALL`, `CR3`) and tune it to the machine.

**XPC (ISCA '19) \cite{du2019xpc}.** XPC adds a hardware primitive for a
direct caller-to-callee switch, prototyped on a RISC-V core on FPGA and in
GEM5, and integrated into seL4, Zircon and Android Binder. It "can reduce
IPC call latency from 664 to 21 cycles". *Difference:* XPC needs new
hardware. We ask what software alone can do on hardware that exists, with
every mitigation the hardware asks for.

**UnderBridge (USENIX ATC '20) \cite{gu2020harmonizing}.** UnderBridge
moves OS components between user and kernel space at run time while
keeping isolation, using intra-kernel isolation. The USENIX page reports
IPC round trips "as low as 109 cycles" and 3.0 times faster than SkyBridge.
These figures come from the page summary and are to be checked against the
PDF before quoting. *Difference:* UnderBridge changes where servers run. We
keep servers in ring 3, in separate address spaces.

**dIPC (EuroSys '17) \cite{vilanova2017dipc}.** dIPC repurposes the CODOMs
architecture so that threads cross process boundaries in one shared address
space. It is "64.12× faster than local remote procedure calls (RPCs), and
8.87× faster than IPC in the L4 microkernel". *Difference:* dIPC needs
CODOMs hardware. Our baseline is commodity x86-64 and Arm.

## 3. Speculative execution and the cost of switches

**Spectre \cite{kocher2019spectre}, Meltdown \cite{lipp2018meltdown}, KAISER
\cite{gruss2017kaslr}.** These attacks made the barriers at every switch
between programs part of the IPC cost: `IBPB`, return-stack refills,
page-table isolation. KAISER, the basis of Linux's KPTI, reported "a runtime
overhead of only 0.28%" in its own evaluation. *Difference:* our matched
figure keeps every defence the processor needs (AutoIBRS and STIBP, the
return-stack refill or ERAPS) and skips `IBPB` only inside a speculation
domain that both programs' parent chose (§9.2). We measured `IBPB` at about
230 ns a switch under KVM on Zen 5 (§9.6), where §9.1 had assumed 2 µs.

**Ren et al., *An Analysis of Performance Evolution of Linux's Core
Operations* (SOSP '19) \cite{ren2019analysis}.** The authors measured seven
years of Linux core-operation latency. They found, for example, that "the
select system call is 100% slower than it was just two years ago", and that
"most of the slowdowns can be attributed to 11 changes", security
enhancements among them. *Difference:* their study is retrospective, over
one kernel's history. Ours budgets every span of one path on one machine
and keeps a figure per hardware key, so a regression from a new mitigation
shows up against its key.

**PCID and KPTI \cite{linuxpti, gregg2018kpti}; AMD's Automatic IBRS
\cite{amd2023apm2} *(unverified as a vendor document)*.** Linux's
documentation explains that without PCID "each CR3 write flushes the entire
TLB". Gregg estimated KPTI's overhead on one workload at about 2.1% without
PCID and 0.5% with it (a blog post, not peer reviewed). No public AMD
document for ERAPS was found; Linux patch postings describe it. *Difference:*
these are the hardware facts that make an IPC path hardware-specific. nazuna
has no PCID, so the `CR3` write costs 90 to 113 ns a direction there
(§9.10), and ERAPS is what allows dropping the refill on Zen 5. A result
keyed by hardware must name such facts.

## 4. Specializing the OS to its case

**Synthesis (Massalin and Pu) \cite{pu1988synthesis, massalin1989threads,
massalin1992synthesis} *(partly unverified)*.** The Synthesis kernel
generated specialized code at run time for the case at hand, for example a
read routine for one open file. It also used lock-free synchronization and
fine-grained scheduling. *Difference:* Synthesis specialized automatically,
at run time, by a code synthesizer inside the kernel. We specialize at
development time, by agents working on source, with review and evidence for
certification. Synthesis is the closest ancestor of the idea that the OS
should fit its exact case.

**Exokernel (SOSP '95) \cite{engler1995exokernel} and Xok/ExOS (SOSP '97)
\cite{kaashoek1997application}.** An exokernel exports hardware securely to
untrusted library OSes. The 1995 abstract reports primitive kernel
operations "ten to 100 times faster than in Ultrix", and application-level
VM and IPC "five to 40 times faster than Ultrix's kernel primitives".
*Difference:* exokernels move policy to applications. Ferrix keeps policy in
servers and makes crossing to them cheap. Both aim at the same cost.

**Library OSes: Drawbridge (ASPLOS '11) \cite{porter2011rethinking},
unikernels (ASPLOS '13) \cite{madhavapeddy2013unikernels}.** Drawbridge
refactored Windows 7 into a library OS. MirageOS compiles an application and
its OS into one single-address-space image. *Difference:* both avoid IPC by
collapsing domains. We keep the domains and tune the crossing.

**SPIN (SOSP '95) \cite{bershad1995spin}.** SPIN let applications load
type-safe Modula-3 extensions into the kernel. *Difference:* SPIN
specialized by letting applications extend the kernel. We specialize the
kernel's own hot path to the hardware, and applications see the same ABI.

**Optimistic incremental specialization (SOSP '95) \cite{pu1995optimistic};
Tempo and the specialization toolkit (TOCS 2001)
\cite{mcnamee2001specialization}.** Pu et al. specialized HP-UX's `read` for
quasi-invariants. The abstract says this "reduces the cost of a single byte
read by a factor of 3, and an 8 KB read by 26%". McNamee et al. built tools
(Tempo, TypeGuard, MemGuard) to make such specialization systematic.
*Difference:* these specialize to the workload's invariants, with tools. We
specialize to the machine's invariants (no PCID, ERAPS, the cost of
`XRSTOR`), with agents instead of a partial evaluator. Guarded
specialization (a boot switch, conditions checked at entry, a fall back to
the general path) is common to both.

## 5. Autotuning to the machine

**ATLAS \cite{whaley1998atlas, whaley2001automated}, PHiPAC
\cite{bilmes1997phipac}, FFTW \cite{frigo1998fftw, frigo2005fftw3}.** These
tune numerical kernels to the machine by empirical search: ATLAS at install
time, FFTW by a planner that measures alternative plans of generated
codelets at run time, PHiPAC as the first autotuned matrix-multiply
generator. *Difference:* they search a parameter space that people defined
in advance, for one kernel with a clean numerical specification. An OS hot
path has no such space. The design choices (a direct switch, a vector-state
contract, a selector skip) are structural, have security consequences and
need review. Agents propose and build them, and the "result per machine" of
ATLAS and FFTW becomes our hardware-keyed record.

**OpenTuner (PACT '14) \cite{ansel2014opentuner}.** OpenTuner is a general
autotuning framework that runs an ensemble of search techniques. It reported
"speedups over prior techniques of these projects of up to 2.8x".
*Difference:* the same as for ATLAS: a search over a defined space against
design work over source.

## 6. Superoptimization

**Massalin, *Superoptimizer* (ASPLOS '87)
\cite{massalin1987superoptimizer}; STOKE (ASPLOS '13)
\cite{schkufza2013stochastic}; Souper (2017) \cite{sasnauskas2017souper}.**
Massalin's superoptimizer searched exhaustively for the shortest
instruction sequence computing a function. STOKE used MCMC search over
loop-free x86-64 and matched or beat `gcc -O3` and `icc -O3` from `-O0`
input, sometimes beating expert assembly. Souper synthesizes LLVM IR
optimizations; as a fully automated pass it made a Clang binary "about 3 MB
(4.4%) smaller". *Difference:* superoptimizers search instruction sequences
with an equivalence guarantee for straight-line code. Our changes cut across
the scheduler, the ABI and the speculation model. Equivalence is argued by
test and review, not checked by a solver.

## 7. Profile-guided optimization

**Pettis and Hansen (PLDI '90) \cite{pettis1990profile}, AutoFDO (CGO '16)
\cite{chen2016autofdo}, BOLT (CGO '19) \cite{panchenko2019bolt}, Propeller
(ASPLOS '23) \cite{shen2023propeller}.** These lay out code by measured
profiles. AutoFDO reports a geomean of 10.5% improvement, reaching 85% of
traditional FDO's gains. BOLT reports up to 7.0% on top of PGO and LTO for
data-center applications. Propeller reports 1.1% to 8% beyond PGO and
ThinLTO. *Difference:* PGO keeps the program's semantics and changes its
layout or code generation. Our profile (§9.10's 40 spans) guides changes in
design that a compiler may not make: an ABI contract, skipping a barrier,
bypassing a scheduler. On the other hand, BOLT-style layout is one of step
5's own candidates ("fewer user pages a trip").

## 8. Machine learning for systems

**Learned index structures (SIGMOD '18) \cite{kraska2018case}, Decima
(SIGCOMM '19) \cite{mao2019learning}, device placement (ICML '17)
\cite{mirhoseini2017device}, MLGO (2021) \cite{trofin2021mlgo}, TVM
(OSDI '18) \cite{chen2018tvm}, Ansor (OSDI '20) \cite{zheng2020ansor}.**
These replace a hand-written system component or heuristic with a learned
model, or search a space of programs with a learned cost model. Learned
indexes beat cache-optimized B-trees "by up to 70% in speed". Decima
improved average job completion time over hand-tuned heuristics "by at least
21%". MLGO's inlining-for-size policy is upstream in LLVM. Ansor reports up
to 3.8×, 2.6× and 1.7× over the state of the art on Intel CPU, ARM CPU and
NVIDIA GPU. *Difference:* in these systems the learned artifact runs inside
the system. In ours nothing learned ships: the agents write ordinary,
reviewed code, and the model's part ends at the commit. That is what lets
the result enter a certified item at all.

## 9. Language models and agents that optimize code

**AlphaDev (Nature 2023) \cite{mankowitz2023faster}.** AlphaDev used deep
reinforcement learning over assembly to find faster small sorting routines,
which were merged into LLVM's libc++. DeepMind's blog gives "up to 70%
faster for shorter sequences" and about 1.7% for sequences over 250,000
elements. *Difference:* AlphaDev optimizes a small, fully specified routine
under a correctness check by testing. Our path spans a kernel subsystem with
concurrency and a security model. The search is replaced by agent design
and a human-like review.

**AlphaEvolve (2025) \cite{novikov2025alphaevolve,
deepmind2025alphaevolveblog}.** AlphaEvolve is an LLM-driven evolutionary
coding agent with automated evaluators. The arXiv abstract reports a more
efficient data-center scheduling algorithm and a faster training kernel for
its own LLM. DeepMind's blog gives the figures: the Borg heuristic
"continuously recovers, on average, 0.7% of Google's worldwide compute
resources", and a Gemini kernel ran 23% faster, cutting Gemini's training
time by 1%. *Difference:* AlphaEvolve needs a machine-checkable evaluator
and runs many candidates. Our evaluator is a benchmark plus correctness
gates plus a reviewer, because no automated check states what a kernel
change may not break. We run few candidates, each reviewed.

**KernelBench (ICML 2025) \cite{ouyang2025kernelbench} and agentic GPU-kernel
work \cite{chen2025nvidiadeepseek, baronio2025kevin, li2025cudal1,
lange2025robust}.** KernelBench measures whether LLMs write correct and fast
GPU kernels for 250 PyTorch workloads. Frontier reasoning models matched the
PyTorch baseline "in less than 20% of the cases". NVIDIA's blog reports a
closed loop of DeepSeek-R1 and a verifier producing numerically correct
kernels for 100% of KernelBench Level-1 and 96% of Level-2 problems. Kevin
(multi-turn RL) raised correctness from 56% to 82% and mean speedup from 0.53×
to 1.10× over PyTorch eager. CUDA-L1 reports a median speedup of 1.42×
across all 250 tasks on an A100. Sakana's AI CUDA Engineer withdrew its
first results after its evaluation was found exploitable; its follow-up is
about robust verification \cite{lange2025robust}. *Difference:* this is the
closest line of work: agents tuning kernels to specific hardware. GPU
kernels are pure functions with a reference output, so correctness is
checkable automatically. An OS IPC path is not. Sakana's episode is the
failure our review loop exists to catch (research-plan §5).

**LLM compilers \cite{cummins2023large, cummins2024meta}.** A 7B model
trained to choose LLVM pass lists achieved a 3.0% instruction-count
reduction over the compiler. Meta's LLM Compiler reaches "77% of the
optimising potential of an autotuning search" for code size. *Difference:*
these optimize compiler decisions on IR. Ours are source-level design
decisions in a kernel.

**SWE-agent (NeurIPS 2024) \cite{yang2024sweagent}, SWE-bench (ICLR 2024)
\cite{jimenez2024swebench}, PIE (ICLR 2024) \cite{shypula2024learning},
SWE-Perf (2025) \cite{he2025sweperf}, ECCO (EMNLP 2024)
\cite{waghjale2024ecco}.** SWE-agent showed that an agent-computer
interface lets a model resolve real repository issues: 12.5% pass@1 on
SWE-bench. PIE's 77,000 C++ pairs, measured in gem5, gave a mean speedup of
6.86 with eight generations, against 3.66 for individual programmers.
SWE-Perf finds a "substantial capability gap" to experts on real
performance pull requests. ECCO finds that most methods trade functional
correctness for moderate efficiency gains. *Difference:* these are
benchmarks of single tasks. Ours is a case study of a long, multi-session
engineering campaign: about a dozen agent sessions over six days, landing
under a review regime. The process data (sessions, gate runs, review rounds;
research-plan §8) are part of the contribution.

**LLMs and OS kernels: AutoOS (ICML 2024) \cite{chen2024autoos}, Kgent
(eBPF '24) \cite{zheng2024kgent}, SchedCP (2025) \cite{zheng2025agenticos},
KernelGPT (ASPLOS '25) \cite{yang2025kernelgpt}.** AutoOS uses an LLM loop to
choose among Linux's more than 15,000 configuration options, and comes out
"up to 25%" better than vendor configurations. Kgent synthesizes eBPF
programs from natural language and checks them with symbolic execution.
SchedCP lets an agent synthesize `sched_ext` schedulers, and reports up to
1.79× performance. KernelGPT has LLMs write syzkaller specifications for
fuzzing. *Difference:* these tune or extend Linux from outside, through
configuration, eBPF or policy plug-ins, within interfaces Linux already
guards. We change the kernel's own hot path, its ABI contract and its
barriers, inside a certification boundary. That is riskier, which is why the
review loop and the negative controls are part of the method, not
decoration.

## 10. What no prior work combines

No work found combines all four:
- per-hardware specialization of a microkernel's IPC path to its processor,
  virtual CPU model and mitigation set, measured against seL4 with matched
  protections on the same host;
- done by AI agents across many sessions;
- under certification-grade evidence (requirements, negative controls,
  coverage, a reviewer's ledger);
- recorded per hardware key for reuse.

Each element has precedent: L4 and seL4 for the first, KernelBench and
AlphaEvolve for agents tuning to hardware, seL4's proofs for evidence,
ATLAS and FFTW for results per machine. The paper's claim must rest on the
combination and on the measured outcome. Until C7 and RQ3/RQ4 are measured
(paper.md §5), the combination is the contribution and the outcome is
partial.
