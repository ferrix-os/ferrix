# The claim, and the path to it

> **PROPOSED, not decided (2026-09-30).** This is the certification
> consultant's plan, written down at os-35's request so that it does not
> live only in a message thread. Two decisions in it are the customer's and
> are **open**:
>
> 1. **The claim's wording** (§1).
> 2. **IEC 61508 SIL 2 as the first standard** (§2).
>
> **Decided (customer, 2026-10-01):** certification is required. The four
> targets in the [README](README.md) are a must, and the goal is to reach
> them. That settles *whether*, not the two questions above: the claim's
> wording and which standard comes first stay open.
>
> Until the customer decides both, the plan below is not a commitment, and
> no other document should cite it as one. Figures marked *guess* are the
> consultant's estimates, not measurements. Points are for the product owner
> to convert.

The customer has named Ferrix's differentiator: run Linux software on a
kernel that can be assured. That means unmodified Linux binaries (Chrome,
rustc, git, Steam) on a memory-safe Rust kernel whose drivers are
restartable, IOMMU-confined ring-3 processes, backed by the SysML model and
this directory. The markets are automotive, industrial, medical and
avionics, where teams want Linux's software but cannot certify Linux. This
document says what that claim can honestly mean, which standard to aim at
first, what stands between today and a claim an assessor would accept, and
in what order to close it.

---

## 1. What the claim can mean

"Linux software on a kernel that can be assured" will be read as "Linux
software becomes assured". It cannot. [SAFETY-MANUAL.md](SAFETY-MANUAL.md)
AoU-3 already excludes the Linux personality, the VFS, the network stack and
every ring-3 driver from any claim, and forbids a safety function there.

btrfs is the one part of the Linux layer that moved: on 2026-10-02 the
customer decided that the btrfs reader and write path (`ferrix-btrfs`,
`ferrix-btrfs-write`) are certified for as long as they run inside the
kernel, and they joined the item. What that adds to the claim is narrow
and is ASR-9's: data on a volume is returned as stored or refused, and what
a commit reported durable survives a power cut, on a single-device volume
of the shape the writer maintains, against accident rather than an
attacker, on storage that honours flush (AoU-15, AoU-16). It does not make
files assured as a program sees them: the VFS glue above the two crates --
the page cache, when a write is committed, what `fsync` does -- stays load.

The claim that can be made true is **mixed criticality**. Unmodified Linux
software runs in non-safety (QM) partitions beside safety functions that run
as native processes on an assured item, and the item keeps the two apart:
memory, devices, processor time and failure. Every public sentence should
have that shape, and carry "not assessed" until it is (AoU-9). In these
markets, implying a certification not held is a legal problem, not only a
credibility one.

**Open decision 1:** the customer confirms this wording, or gives another
that this document is then fitted to.

## 2. The first standard

**Proposed: IEC 61508-3, SIL 2, as a compliant item developed out of
context** (the SEooC route of [SAFETY-MANUAL.md](SAFETY-MANUAL.md)).

* **It is the parent standard.** EN 50716 (rail) derives from it, ISO
  26262's SEooC route and IEC 62304 accept much of the same evidence, and
  industrial buyers ask for it directly. One assessment is reused the most.
* **It is where this directory already stands closest.** The
  [README](README.md)'s verdicts rate EN 50716 SIL 2 "reachable" and IEC
  62304 Class C "closest", and 62304 needs an ISO 13485 QMS first (F-28),
  which is an organisation, not code.
* **SIL 2 tolerates what Ferrix cannot soon remove:** dynamic memory
  justified rather than forbidden (AoU-5), and roles combined with
  justification (F-27). Neither survives at SIL 3 and above.
* **A qualified Rust toolchain exists** for IEC 61508 and ISO 26262
  (Ferrocene). Whether it covers every target the item builds for,
  `armv7a-none-eabi` and the UEFI targets among them, is the first question
  for the vendor (F-17).

After it, in order: ISO 26262 ASIL B as SEooC; IEC 62304 Class C once a
customer QMS exists; Common Criteria EAL4+ (the README's "defensible", where
RHEL and SUSE sit) or IEC 62443-4-2 for security. DO-178C DAL C comes last
(the README calls it the furthest: a missing document set, independence and
tool qualification), and only for a named avionics customer. SIL 3 and ASIL
C/D are the reason the `core` ring exists ([ITEM.md](ITEM.md) §3). They are
not a near target.

**Open decision 2:** the customer confirms SIL 2 first, or names another
standard.

## 3. The item and the Linux layer: freedom from interference

### 3.1 Where the Linux layer sits

The item today is 79,079 lines of product code by the item-boundary gate on
5625d22f (2026-10-02): 64,788 in the kernel, 56,644 of them `core`, and
14,291 in two library crates. It holds memory protection,
scheduling, capability objects, trap and system-call entry, the IOMMU, SMP,
device discovery, and the native ABI a safety process uses, and since
2026-10-02 the btrfs reader and write path, those two crates (without their
8,278 lines of host tests). Their
interface below is the `Device` and `WriteDevice` traits the kernel's block
layer answers, and above it `Volume` and `WriteVolume`. The Linux layer (the
personality, VFS, the btrfs glue on the VFS -- `ferrix-btrfs-vfs` and the
kernel's `fs/btrfs*.rs` --, net, namespaces, procfs) is outside, as the
`load` ring: 64,036 kernel lines, plus the libraries it calls, which the
gate does not count but for `ferrix-btrfs-vfs`'s 2,185 (network about 16k,
vfs about 13k, by `wc` with tests).
It stays outside.

The two btrfs crates are an unusual member of the item: the item's code,
called only by the load. They run in ring 0 on the load's threads, under the
load's locks, parsing a volume whose bytes come from a ring-3 block driver
and whoever wrote the medium. So their own claims are about their interface
-- total over any image, damage refused, a commit all or nothing -- and
(a) to (d) below apply to how the load calls them, not to them: they are
`forbid(unsafe_code)`, their allocations are being made fallible (TODO.md),
and their time is spent inside the load's calls.

The problem is that the load runs **in ring 0, in the item's address
space**. AoU-3 says "the element's own enforcement is what bounds their
failure", but no hardware enforces anything against ring-0 code. The claim
therefore rests on a **freedom-from-interference argument** (IEC 61508-3
Annex F: spatial and temporal) that is not yet written. It has four parts:

* **(a) Memory.** Safe Rust carries it, if `unsafe` in the load is confined.
  There are about 20 `unsafe` blocks in `fs/`, `syscall/` and `net/` today.
  A gate forbids `unsafe` in the load outside a traced allowlist, as F-26's
  does for the item.
* **(b) Resources.** The load's allocations are not fallible and share the
  item's heap. A Linux program that drives the load to exhaustion takes the
  whole machine to the safe state (FX-0008, AoU-5). In mixed criticality a
  QM partition must not halt the safety partitions. The fix is per-job heap
  arenas for the load's work, or fallible program-driven load paths, so that
  exhaustion answers `ENOMEM` to the partition that caused it.
* **(c) Time.** Load code holds spin locks with preemption off (FX-0503)
  across btrfs, net and VFS work: unbounded latency for a real-time
  partition. This needs stage 14 (ASR-8 is "partially met", AoU-4 has no
  WCET), long load sections made preemptible, and ring-0 time spent for a
  partition charged to its job's budget.
* **(d) Failure.** A panic in the load is a kernel stop. The panic lint and
  audit must be shown to cover the load as they cover the item, and the load
  failures that end in the safe state listed.

### 3.2 Options between today and the opaque kernel

[OPAQUE-KERNEL.md](../OPAQUE-KERNEL.md), with the whole Linux layer in
ring 3, is shelved because a ring-3 trip costs 10 to 20 times Linux's
in-kernel path (S0: 300 to 844 µs for a 4 KiB read on x86-64 under KVM,
against 27 to 48 µs). The options in between, as the consultant judged them
on 2026-09-30. Acceptance is the consultant's judgement, to be confirmed at
pre-assessment (§5, M2).

| Option | Isolates | SIL 2 | SIL 3 | Cost (*guess*) | Architectures |
|---|---|---|---|---|---|
| 1a. PKS / FEAT_S1POE domains in ring 0 | stray writes (and reads) | strengthens | doubtful: the load can rewrite the key register itself | 60–120, mostly partitioning data by owner | x86-64 with PKS only; AArch64 with S1POE, which no target has; not ARMv7-A (LPAE has no domains) |
| 1b. Page-table switch on entry to the load | the same | strengthens | the same caveat | as 1a, and costly until PCIDs and ASIDs exist (none are used today) | all three |
| 2a. Item at EL2 / VMX root, Ferrix's load as a guest kernel | memory, time, faults, resources | yes | yes | 150+: the load becomes a second kernel | x86-64, AArch64 (nested virtualisation to develop on) |
| 2b. Item as a separation kernel, **real Linux** as the guest | all four | yes | yes, with industry precedent | 80–150 | as 2a |
| 3. Partial opaque: btrfs, then net, in ring 3 | all four, for the parts moved | yes, for those parts | yes, for those parts | btrfs 20–35 after 4; net 25–40 after 4 | all three |
| 4. IPC fast path first | nothing by itself; the enabler for 3 and the full opaque kernel | — | — | 25–45 | all three |
| 5. Software compartments in ring 0 | resources, memory (by language), time if the load is made preemptible | plausible, with a qualified compiler | unlikely alone | 50–90 with (a)–(d) of §3.1 | all three |

Notes:

* **3, btrfs first,** removes about 25k lines of parser for untrusted disk
  images from ring 0. It costs only cold fills and metadata writes, because
  the page cache takes the warm path (S0: a warm rustc crossed once). The
  network stack crosses on every send and receive, so it waits for 4.
* **4's known causes** (OPAQUE-KERNEL.md): missed wake-ups rescued by the
  block ring's 50 ms recheck timer, no PCIDs or ASIDs so every switch
  flushes the TLB, and data copies. The customer approved option 4 on
  2026-09-30. `ipc-measure` landed its instrument (5cc5ed38). Its baseline
  on x86-64 under KVM: trip p50 230 µs, 13 switches, 2.4 root writes and
  3.2 IPIs per read. `ipc-lazytlb` (reviewed with conditions), `ipc-ring`
  and `ipc-wake` follow.
* **2b is a strategic choice, not a technical one.** It is the proven route,
  but it moves Linux compatibility out of Ferrix's own code, and "Linux
  software" then means a Linux VM.

### 3.3 Proposed combination

* **SIL 2 first:** 5 (heap arenas, the load `unsafe` gate, a narrow
  capability-typed item facade, no shared mutable statics, a preemptible load
  with its ring-0 time charged), plus 4, plus 3 for btrfs once a trip is
  within about twice Linux's. About 90–160 points (*guess*), before stage
  14's own size.
* **SIL 3, later:** either the full opaque kernel, made affordable by 4 and
  reached by continuing 3, or 2b. The customer decides once 4's
  measurements are in, because they decide whether the opaque route is
  affordable.

When the customer has decided, the freedom-from-interference argument
becomes a section of [SAFETY-MANUAL.md](SAFETY-MANUAL.md) beside AoU-3,
the assumption it replaces with an argument, and the work becomes orders in
[IMPLEMENTATION.md](IMPLEMENTATION.md).

## 4. The gaps, ordered by what blocks most

### What the code lacks

1. Freedom from interference across the ring-0 load (§3.1 (b), (c)).
   Without it the mixed-criticality claim does not hold.
2. Temporal partitioning: stage 14, not started (ASR-8, AoU-4, F-24).
3. Traceability to finish (F-14, F-15): 1,182 of 2,411 item functions are
   named by a low-level requirement, and several subsystems are not yet
   complete. About 20–30 points.
4. Coverage residuals (F-10): 94, 144 and 163 statements still need a test.
   About 15–25 points. Decision coverage (F-13) is 28–38% by object-code
   decisions; higher levels will ask for more.
5. The security features (authentication, AoU-7 and OE.AUTH; seccomp,
   `docs/SECCOMP.md`; namespaces N4 to N7) matter for Common Criteria and
   IEC 62443, and for containing Linux programs from each other. They are
   not the safety claim's blocker, because the partition the item enforces
   is the job, not the namespace.
6. F-43 (W^X claimed, not enforced for a program's own pages), waiting on the
   customer, and F-52 (no CC version named).

### What the process lacks

1. **Independence and assessment** (F-27, AoU-9). SIL 2 still needs an
   independent functional safety assessment, and independent verification of
   requirements and design by a person outside the development. *Outside
   party.*
2. **AI-authored code** (F-29). No scheme has settled it, and every commit
   here has that provenance. Raise it at pre-assessment (M2), before evidence
   is spent. A position likely to be acceptable treats the author as
   untrusted: the tool evidence (gates, coverage, traceability, negative
   controls) plus an independent human review of every item change. That
   needs a human who is not the author.
3. **The planning set** (61508-1 §6, -3 §7.1: safety plan, software
   lifecycle, configuration management, verification plan). About 8–12
   points of writing, but it needs organisational facts only the customer
   has.
4. **Configuration management and change impact.** `main` moves several
   times an hour. A claim needs a frozen, tagged item baseline and an impact
   analysis per change against it. About 5–8 points for a gate that reports
   the item files and requirements touched since the baseline.
5. **Tool qualification** (F-17 to F-19): Ferrocene for the compiler
   (*outside: vendor*), and the in-house coverage tool and evidence gates
   given operational requirements and validation as T2 tools
   ([TOOLS.md](TOOLS.md)). About 8–13 points.
6. **A Rust coding standard.** The rules exist as lints and gates (panic
   audit, unsafe audit, fallible allocation, complexity, the assembly
   allowlist). They need writing as a standard mapped to 61508-3's Annex A
   and B tables. About 3–5 points.
7. **A QMS** (F-28), for 62304 and for many buyers. *Outside: the customer's
   organisation.*

## 5. Milestones

| | Milestone | Needs | Outside party | Points (*guess*) |
|---|---|---|---|---|
| M0 | What can be said today (below) | nothing | — | 0 |
| M1 | An assessment-ready SIL 2 element on one architecture (x86-64 or AArch64; not ARMv7-A, AoU-6) | §3.1 (a), (b) with its Annex F analysis; §4 code 3 and 4; the coding standard; the change-impact gate; the planning set drafted | the customer, for the organisational facts | 70–120 |
| M2 | External pre-assessment: the plan and the element argument reviewed, F-29's position agreed | M1 far enough to show; best **early**, before M1 is finished | an accredited assessor | — |
| M3 | Temporal partitioning | stage 14; the load's non-preemptible bound (§3.1 (c)); a WCET measurement method for integrators | — | stage 14's own size, plus 5–10 |
| M4 | SIL 2 compliant-item certificate; then ASIL B SEooC, 62304 Class C, CC EAL4+ | all of the above | assessor; toolchain vendor; CC lab; the customer's QMS | 20–30 for 26262's own work |

**M0, statable today** (every number measured and in this directory):
"Ferrix runs unmodified Linux programs above a 63,000-line kernel item that
its build defines and enforces. It contains no third-party code and has no
upward dependency, statement coverage is measured at about 90% on three
architectures, and an element-level safety argument is written against IEC
61508 SIL 2 and IEC 62304 Class C. Not assessed."

## 6. What in the roadmap works against the claim

* **Growth of the ring-0 load** for desktop and Steam needs (procfs quirks,
  namespaces, the kernel ends of the display, render and input interfaces).
  Every such line enlarges §3.1's surface. Compatibility work goes to ring 3
  where it can. In ring 0 it adds no `unsafe` and allocates fallibly.
* **Item and core changes driven by desktop workloads.** The XSAVE and AVX
  change of 2026-09-30 was core ring, driven by Bun. That is fine when
  reviewed with requirements and checks, as it was, but it is the pattern to
  watch: performance and compatibility pressure on `sched/`, `mm/` and
  `arch/`. The baseline and impact gate (§4 process 4) make each visible
  against a frozen item.
* **Change velocity against baselines.** The fleet lands faster than any
  assessment can follow. A certification baseline (tagged, impact-analysed)
  must be separate from `main`.
* **Configuration.** The desktop, Steam, Chrome, the GPU and the compositor
  are never part of the certified configuration (AoU-8). They are other
  configurations of the same kernel binary, above the item.
* **Wording.** Steam and the desktop are good evidence that Linux software
  runs. They stay out of any sentence that also says "assured".
