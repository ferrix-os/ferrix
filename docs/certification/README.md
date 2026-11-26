# Certification

A theoretical assessment of Ferrix against four assurance ratings, conducted
2026-09-25. Theoretical because no accredited laboratory, notified body or
independent assessor has been engaged — the audit is internal, and says so
everywhere it matters.

**Reaching all four is a requirement (customer, 2026-10-01).** The verdicts
below say how far each is. They are not an invitation to drop one. What this
directory asks for is required evidence: requirements and their traces,
structural coverage, checks with negative controls, findings, the
vulnerability analysis and the safety manual. So are the steps only outside
parties can take: an assessor, a quality management system, independent
reviewers, and a position on AI-authored code (§5). The order in which the
targets are pursued is open ([CLAIM.md](CLAIM.md)).

**The element is developed out of context.** It has no application of its own,
so it is analysed against *assumed* safety requirements and ships the
conditions an integrator must discharge — ISO 26262's SEooC, EN 50716's generic
software, DO-178C's reusable software component. See
[SAFETY-MANUAL.md](SAFETY-MANUAL.md). This is how QNX, PikeOS and VxWorks 653
are certified, and it is why F-20 and F-22 are no longer blocked on a device
that does not exist.

| Target | Standard | Verdict |
|---|---|---|
| EAL5+ | Common Criteria (ISO/IEC 15408) | **Not met.** Security Target and vulnerability analysis written, and the boundary has no upward reference left; blocked on design evidence at module granularity and an accredited laboratory. |
| DAL C | DO-178C / ED-12C | **Not met.** Only `max` will carry DO-178C, at level A's objectives, and it does not build yet (the customer, 2026-11-26; `docs/microkernel/FLIP.md`); DAL C is `present`'s interim claim. Coverage of the item's kernel files is measured on every architecture: 85.2% on x86-64 (the `coverage` profile, 2026-10-23), 89.9% on AArch64 and 84.5% on ARMv7-A (debug, 2026-09-27), all under rustc 1.97.1 and carried since, with 1,241, 162 and 215 unreached statements that still need a test (COVERAGE-RESIDUAL.md) and the rest argued or put down to absent hardware; the btrfs crates are not measured (F-56), the release image's coverage is one x86-64 run with no argued residual (F-11, reopened), data and control coupling are not analysed (F-118) and the toolchain's libraries in the image are counted nowhere (F-119); planning data, requirements traceability and those tests are not done. |
| Class C | IEC 62304 | **Closest of the four.** No external crate in the item, and the toolchain's `core`, `alloc` and `compiler_builtins` in it unevaluated (F-119); element-level safety analysis written. Blocked on a QMS and the integrator's risk file. |
| SIL 2 | EN 50716:2023 | **Reachable.** Most of Annex A satisfied; generic software argument and application conditions written. Blocked on independent assessment. |

None of the four can be claimed today. What changed is that the reasons are now
specific, measured, and mostly documents rather than code.

* [IMPLEMENTATION.md](IMPLEMENTATION.md) — **start here to build**: fourteen work orders, ordered
* [TODO.md](TODO.md) — start here to re-audit: what to re-measure, and what not to write
* [CLAIM.md](CLAIM.md) — **PROPOSED, not decided**: what "Linux software on a kernel that can be assured" can mean, the first standard, freedom from interference for the ring-0 load, and the milestones; the customer's two decisions are open
* [SAFETY-MANUAL.md](SAFETY-MANUAL.md) — the out-of-context argument: assumed requirements, safe state, eleven assumptions of use, element failure analysis
* [ITEM.md](ITEM.md) — what the ratings attach to, and why it is not all of Ferrix
* [FINDINGS.md](FINDINGS.md) — the audit register, 37 open findings and 85 closed, of 122
* [OBJECTIVES.md](OBJECTIVES.md) — DO-178C's Annex A tables for `max` at level A's objectives: a skeleton, no row read from the standard's text yet
* [AUDIT.md](AUDIT.md) — the security audit of the TSF's own decisions, built and claimed (F-21b)
* [SECURITY-TARGET.md](SECURITY-TARGET.md) — EAL5+ claim, SFRs, and where it would fail evaluation
* [VULNERABILITY-ANALYSIS.md](VULNERABILITY-ANALYSIS.md) — AVA_VAN.4 over the seven threats; six residual vulnerabilities
* [SPECULATION.md](SPECULATION.md) — the side-channel defences, per architecture, behind one build switch, and what they cost
* [MEMORY-AND-TIMING.md](MEMORY-AND-TIMING.md) — what the item allocates, and what it promises about time
* [SOUP.md](SOUP.md) — generated; the item links no external crate, and the toolchain's three libraries it does link (F-119)
* [VERIFICATION.md](VERIFICATION.md) — what exercises the item, and the traceability gap
* [TRACEABILITY.md](TRACEABILITY.md) — generated; the item's requirements, their parents, and the checks that verify each on each architecture
* [COVERAGE-RESIDUAL.md](COVERAGE-RESIDUAL.md) — generated; the uncovered statements per architecture, sorted into argued and gap
* [COVERAGE-WORKLIST.md](COVERAGE-WORKLIST.md) — generated; the statements that need a test, by module, for whoever writes them
* [TOOLS.md](TOOLS.md) — tool classification under EN 50716 §6.7 and DO-330
* `coverage-<arch>.json`, `coverage-residual-<arch>.json` — statement coverage evidence per file, and the unreached lines; `coverage-floor.json` — the ratchet

**Not written, and why.** A *hazard analysis* and a *risk management file*
(F-20) need an application: hazards belong to a device or a train, not to a
kernel, and a generic list would be a document rather than evidence. A *safety
case* (F-22) needs the same. The *DO-178C planning set* — PSAC, SDP, SVP, SCMP,
SQAP — needs an organisation to describe (F-28). Writing any of them from here
would produce paperwork that an assessor would reject and that would make this
directory look more finished than it is.

---

## 1. What the item is

Not Ferrix. Ferrix's acceptance test is that it hosts `rustc` and builds
itself, which requires a general-purpose OS with a browser, a compositor and a
self-hosting toolchain — the opposite of a frozen, analysable configuration.

The item is **79,079 lines of product code: a 64,788-line subset of the
kernel and, since 2026-10-02, the two btrfs crates' 14,291**, defined in
[`tools/common/data/certification-item.json`](../../tools/common/data/certification-item.json) and
enforced on every build by `tools/common/check/check-item-boundary.py`. Memory protection,
scheduling, capability objects, the trap and syscall entry paths, the IOMMU,
SMP, device enumeration and the btrfs reader and write path are inside; the
VFS with the btrfs glue on it, the network stack, the Linux personality -- its
dispatcher, `mmap`, `futex` and threads included -- and the drivers are
uncertified load above it, 64,036 lines of it in the kernel (ITEM.md §2,
re-measured 2026-10-02).

The boundary is nested so it can ratchet inward: a 56,644-line `core` ring is
named now as the destination for a later EAL6+ or ASIL D effort, so that
raising the target does not mean rewriting every artifact scoped to the old
boundary.

## 2. What was measured, not asserted

| | |
|---|---:|
| Item product code | 79,079 lines: 64,788 in the kernel, 14,291 in the btrfs crates (2026-10-02) |
| Uncertified load | 64,036 lines in the kernel (2026-10-02) |
| In-kernel self-tests | 42,634 lines |
| Statement coverage, certified item's kernel files | **85.2%** x86-64 (11,148 of 13,080, the `coverage` profile, 2026-10-23, which supersedes 2026-09-27's 90.1% of a smaller item), **89.9%** AArch64, **84.5%** ARMv7-A (debug, 2026-09-27), measured under rustc 1.97.1, carried; the release image 83.4% on x86-64, once (F-11); the btrfs crates are not measured (F-56) |
| Statement coverage, core ring | 85.9% x86-64 (8,934 of 10,396, 2026-10-23), 90.8% AArch64, 84.1% ARMv7-A (2026-09-27), measured under rustc 1.97.1, carried |
| Unreached statements, kernel files (COVERAGE-RESIDUAL.md on 4d954b671) | x86-64 1,907 — 395 argued, 271 hardware absent, **1,241 need a test**; AArch64 627 — 308, 157, **162**; ARMv7-A 970 — 409, 346, **215** |
| External crates in the item | **0**; the toolchain's `core` and `compiler_builtins` are in every image and `alloc` in the kernels, and are not counted (SOUP.md §1a, F-119) |
| External crates, host-side | 21 |
| Upward boundary references | **0**, from 94 at the start of the work (29 and 62 before the gate could resolve module paths) |
| `unsafe` blocks, all documented | 800 |
| `unsafe` sites in the item traced to the requirement they serve | **663** of 663, under 14 obligations; a new untraced one fails the build (F-26) |
| Directly recursive functions in the item | **0**, of 2,173 in its kernel files and of 635 in the btrfs crates |
| Allocations in the item that stop the machine when memory runs out | **0** in its kernel files; 73 at bring-up, by design; 13 in the load `process_create` and `process_start` run, recorded, and the load's callees beyond those (F-23, MEMORY-AND-TIMING.md §1.3). **0** in the btrfs crates, held there by `cargo xtask check` (`L.btrfs.22`): the write path's 166 made fallible on 2026-10-02, and the reader's 19 name matches argued `NOALLOC:` (MEMORY-AND-TIMING.md §1.8) |
| Job quotas the ST claims (FRU_RSA.1) that are built | **3** of 3, as refined: a job's memory -- its programs' frames and page tables and the kernel heap the Linux personality holds for them -- native objects and tasks, each refused at its limit with a sibling going on, and a processor shared by job weight (one task alone kept 50.0% against eight) (F-35, F-37 closed) |
| Kinds of kernel heap a program can make and keep through the Linux calls, charged to its job | **13** of 13 the audit found, each refused at a 32 KiB limit at boot; per-task state and machine-wide tables with fixed bounds argued (F-37) |
| SMEP + SMAP (x86-64) | **on** |
| PAN (AArch64) | **implemented**; absent from the reference CPU |
| Side-channel defences | **on** by default, per processor; one switch, `--mitigations off`, takes them out |
| KASLR | kernel image, direct map and vmap arena **moved every boot**: 18, 16 and 17 bits on the 64-bit pair, 11, 8 and 9 on ARMv7-A; off with the same switch |
| Writable mappings of the kernel's text | **0**, every mapping of its frames swept each boot; the direct map's alias was one until 2026-09-26 (F-34) |
| Assembly | 500 lines, 22 allow-listed sites, outside the Pixel 7 loader |
| Cargo features in `src/kernel/`/`src/boot/common/uefi/` | 0 |
| Requirements of the item, each with a statement, a pass/fail criterion and its parent, gated | *On aecf83e44 (2026-11-25) the gate counts 196 high-level and 1,552 low-level, 1,412 named by a check and 336 in the baseline; the breakdown that follows is 2026-10-02's.* **122** high-level (`H.*`) decomposing all 10 objectives and all 9 ASRs; **742** low-level (126 `L.x86_64.*`, 116 `L.btrfs.*`, 109 `L.object.*`, 107 `L.user.*`, 64 `L.mm.*`, 52 `L.aarch64.*`, 44 `L.iommu.*`, 41 `L.console.*`, 32 `L.smp.*`, 21 `L.device.*`, 9 `L.claim.*`, 7 `L.trap.*`, 5 `L.quiesce.*`, 3 `L.syscall.*`, 3 `L.armv7a.*`, 2 `L.sched.*`, 1 `L.discovery.*`); **542** named by a check that proves each whole, 322 in the baseline of the unverified (TRACEABILITY.md, W-8; 2026-10-02) |
| Product functions of the item no low-level requirement names | **0** of 261 in `object/`, of 96 in `iommu`, of 299 in `mm`, `vmap`, `early` and `user`, of 57 in `smp` and of 74 in `console`, where a new one fails the build (object/: 175 named, 84 accessors, 2 check code; iommu: 77 named, 19 accessors; memory: 238 named, 49 accessors, 12 check code; smp: 38 named, 17 accessors, 2 check code; console: 62 named, 10 accessors, 2 check code); 890 of 3,131 item-wide on 2026-10-02, the subsystems still to write (F-15), 294 of them among the btrfs crates' 669 (162 named, 213 accessors; F-56) |

The coverage rows were measured on 2026-09-27 over the item's kernel files as
W-5 left them, the Linux dispatcher's routing and five of the personality's
files in the load ring ([ITEM.md](ITEM.md) §2); the other rows are measured on
the same tree but for the btrfs crates' figures, of 2026-10-02. The crates are
exercised by host tests, which no coverage run measures (F-56).

Two of these were unknown before this audit and are the reason it was worth
doing. The kernel had **no structural coverage measurement at all** — the
assurance model records why, since a fuzzer cannot drive a page-fault handler
and Miri cannot interpret a privileged instruction. And nobody had established
what third-party code ships inside ring 0.

## 3. The four verdicts

### EAL5+ — not met

EAL5 wants a semiformally designed and tested TOE: `ADV_FSP.4`, `ADV_TDS.3`,
`ADV_IMP.1`, `ADV_INT.2`, `AVA_VAN.4`, `ALC_CMC.4`, plus `ALC_FLR` for the `+`.

*In place:* a defined TOE boundary, machine-checked. A semiformal design
notation already in use — `docs/sysml/`, 5,890 lines of SysML v2 across twelve
packages, with a gate that fails the build when the model and the generated
document disagree. Well-structured internals supported by eleven gates.
Reproducible byte-identical builds. A complete implementation representation,
since the item is 100% first-party source.

*Written since this audit began:* [SECURITY-TARGET.md](SECURITY-TARGET.md),
which closes F-21 — TOE scope, assets, seven threats, four assumptions, eight
objectives, SFRs from CC Part 2 (FDP_ACC/ACF, FDP_IFC/IFF, FDP_RIP.2, FMT_MSA,
FPT_FLS/STM/TDC, FRU_RSA), and a summary specification mapping each objective to
the code and the test that exercises it.

*Also written:* [VULNERABILITY-ANALYSIS.md](VULNERABILITY-ANALYSIS.md),
closing F-21a — `AVA_VAN.4` over all seven threats, with five residual
vulnerabilities. It found that **no SMAP, SMEP or PAN was enabled** (F-32), so
one software bound check was the only barrier between a user pointer and kernel
memory, and it corrected this ST's own claim to the contrary.

*Fixed since:* F-32. SMEP and SMAP are on for x86-64, and PAN is implemented
for AArch64. The reference `cortex-a72` is ARMv8.0 and lacks PAN, and ARMv7-A
cannot have it at all, so V-01 still stands on Arm (AoU-6). And F-34: the
direct map aliased the kernel's text writable, which the W^X sweep could not
see. The alias is read only now, every boot sweeps for it, and no interface
that maps a caller's physical address will map the image.

*Missing:* Design evidence at module granularity for `ADV_TDS.3`; the SysML model
describes Ferrix, not the TOE (F-15). The TSF no longer names the load ring anywhere
(`ADV_INT.2`; F-07, F-09 and F-33 closed), but the gate reads names, not types, and
the crate root is exempt by file (SECURITY-TARGET §9.6). The side-channel defences and KASLR are built and on by
default (F-31, closed), but there is no cache partitioning and no KPTI, so a
program with a timer can still find the kernel and time a neighbour (V-06). And the TOE claims no
authentication, which is the environment's (OE.AUTH), and is why no
Protection Profile is claimed; it claims audit of its own decisions (F-21b,
closed). FRU_RSA.1 is built as refined -- a job's
memory, the kernel heap the Linux personality holds for it included, its
native objects and tasks capped, and a processor shared by job weight rather
than capped -- so T.EXHAUST is resisted; what remains is per-task state the
task limit bounds and a few machine-wide tables with fixed bounds (V-05,
low).

*Nearest credible claim:* EAL4+ looks defensible on this evidence with an ST
written, which is also where RHEL and SUSE sit. EAL5 needs the design
decomposed to the item's modules.

### DO-178C DAL C — not met

62 objectives, 5 requiring independence.

*In place:* statement coverage is now measurable and measured (F-10, at 85.2% on x86-64, 89.9% on AArch64 and 84.5% on ARMv7-A as last measured, carried),
which was the objective everyone assumes is impossible for a kernel. 492 lines
of assembly across 22 allow-listed sites makes the source-to-object question
tractable. Zero Cargo features in the item, and one kernel profile, `present`, whose
build flags are each fixed (`docs/BUILD-FLAGS.md`), mean the configuration
space is enumerated in a sentence. Deactivated code is confined to three
architectures behind `cfg`, and to what the ad-hoc flag values compile out,
the side-channel defences behind `ferrix_mitigations = "off"` among them.

*Missing:* every planning document — PSAC, SDP, SVP, SCMP, SQAP — none drafted.
Low-level requirements (F-15) and requirements-to-test traceability (F-14),
which are the spine of the standard: the gate, 196 high-level
requirements and 1,552 low-level ones exist, 1,412 of the 1,748 named by a check and 336 in the baseline of the
unverified, and 1,173 of the item's 6,417 product functions are named by no requirement (`check-traceability.py` on aecf83e44, 2026-11-25). Tool qualification (F-17 to F-19).
The analysis of data coupling and control coupling that table A-7 asks at this level beside statement
coverage (F-118). Coverage of the image that ships: the evidence is the debug and `coverage` profiles', and
the release profile's is one x86-64 run with nothing argued (F-11, reopened). The toolchain's libraries in
the image, as previously developed software (F-119). Review records under configuration management (F-120).
And the level itself: DAL C is assumed here, where the standard takes it from a system safety assessment
the integrator owns (SAFETY-MANUAL.md). Clause and table numbers in this section are from memory, not
checked against the text.

*Whose claim it is:* only `max` carries DO-178C, from M7's evidence, at level A's objectives, on AArch64 (the
customer, 2026-11-26; `docs/BUILD-FLAGS.md` §4). `present`'s DAL C is the interim until the flip, `fast`
and `desktop` claim none. Level A asks decision coverage and MC/DC (F-13) and independence for many more
objectives than five, so this verdict is of `present`, and `max`'s is M7's to write. Between the flip and M7's evidence no profile carries DO-178C at all (`docs/BUILD-FLAGS.md` §1).

*Honest gap:* this is the furthest of the four, because DO-178C wants a
document set that does not exist rather than a property the code lacks.

### IEC 62304 Class C — closest

*In place:* **no external crate in the item**, measured across all three architectures
and the loader, and now gated; the toolchain's `core` and `compiler_builtins` are in every image and
`alloc` in the kernels, SOUP in IEC 62304's sense whose anomaly lists nobody has read (F-119). This is the obligation that dominates a Class C
submission built on Linux — where the kernel is always SOUP, because §5
lifecycle evidence cannot be produced retroactively — and here it is those three libraries.
§4.3(c) segregation is satisfied architecturally: ring-3 drivers behind an
IOMMU, with the boundary enforced at build time.

*Written since:* the element failure analysis (F-20) and the safety argument
(F-22), both at the element level, with the system half exported as
assumptions of use.

*Missing:* ISO 13485 QMS (F-28, blocking); §5.4 detailed design to unit level
and §5.5.3 unit verification acceptance criteria (F-15). The device
manufacturer's own ISO 14971 risk file is AoU-1 and is by construction not
ours to write.

*Why it is closest:* the blockers are an organisation and a document set. The
technical position — first-party code, gated boundary, measured coverage,
memory-safe language, no external crate — is unusually strong, and none of it has to be
retrofitted.

### EN 50716:2023 SIL 2 — reachable

*In place:* most of the Annex A technique table. Strongly typed language (HR) —
Rust. Defensive programming (HR) — the panic audit plus a lint table denying
`unwrap`, `expect`, `panic`, indexing and slicing in production code. Static
analysis (HR) — clippy at ten configurations, Miri over 19 crates, 30 fuzz
targets with committed corpora. Structured methodology (HR) — the SysML model.
Role independence is permissive at SIL 2, where roles may be combined with
justification, so F-27 is survivable here as it is not elsewhere.

*Written since:* the generic software argument and its application conditions
(F-22), tool classification and operational requirements (F-18, F-19), and the
complexity and recursion gate (F-25, closed).

*Missing:* independent assessment, which EN 50716 permits to be less
independent at SIL 2 than above but not absent (F-27). Dynamic memory remains
pervasive, and Annex A still discourages it. Its failure is now reported at
every site in the item's kernel files, where it used to stop the machine
(F-23, closed); the btrfs write path, in the item since 2026-10-02, is being
converted (F-56). What is left is an exported application condition: no
bound, and a load whose allocation failure is fatal (AoU-5).

## 4. What would actually move the needle

In order of value per unit of effort:

1. **Trace tests to requirements (F-14, F-15, W-8).** The boot gates already
   assert rich properties; they need requirement ids attached and low-level
   requirements to attach them to. This one piece of work unblocks DAL C,
   62304 §5.4 and `ADV_TDS.3`. Since 2026-09-27 the format, the gate, the
   95 high-level requirements, the pilot subsystem, `object/` (103
   low-level, 81 verified), `iommu` (43, 16 verified), memory, `mm` and
   `user` (166, 111 verified), `smp` (32, 9 verified) and `console` (41,
   12 verified), are in place; the other subsystems' low level and
   tags, copying the pilot's corrected format, are what is left.
2. **Cover the 1,241, 162 and 215 statements that still need a test (F-10; COVERAGE-RESIDUAL.md)**, on
   x86-64, AArch64 and ARMv7-A. Every gate counts on every architecture, and
   `cargo xtask coverage` ratchets the union; what is left is tests.
   COVERAGE-WORKLIST.md splits them by module, `syscall/native.rs`,
   `user/space.rs`, AArch64's `trap` and `arch/aarch64` and `object/` the
   largest, and `coverage-argued-<arch>.json` takes a statement that cannot be
   reached with its reason.
3. **Adopt Ferrocene (F-17).** A qualified toolchain is the difference between
   "written in a memory-safe language" as a talking point and as evidence.
   Whether it covers `armv7a-none-eabi` and the UEFI targets is the first
   question.
Done since the audit began: the vulnerability analysis (F-21a), SMEP, SMAP and
PAN (F-32), both Arm architectures measured (F-12; the release profile's
measurement, F-11, was reopened on 2026-11-25), the complexity and recursion gate (F-25), board support, bring-up
and power registering with the item (F-04, F-08), and the split of `Process`
into a core object and a POSIX extension (F-01, F-06, W-1), which took the
boundary from 36 references to 29 by the gate's count of the day and left `object/` and `sched/` naming
nothing above the core; the native ABI's subsystems and the Linux dispatcher
registering with the item, and the rest of the personality moved out of it
(F-07, F-09, F-33, W-5), which left the boundary with no upward reference by
the resolving gate's count, from 56; the side-channel defences and KASLR
behind one build switch (F-31, [SPECULATION.md](SPECULATION.md)); the
direct map's writable alias of the kernel's text, sealed (F-34); allocation
failure made an error the item reports rather than a stop, at every site in
its own source, with a gate that counts the rest and now reads the load files
the item's own calls run (F-23, [MEMORY-AND-TIMING.md](MEMORY-AND-TIMING.md)
§1); user and IOMMU page tables given back only after the shootdown that
covers them, not before (F-36); and the job quotas FRU_RSA.1 claims, charged
per job and refused at their limits, over a native handle and cgroup2's
`pids`, `memory` and `cpu` controllers (F-35, W-13); and the kernel heap
the Linux personality holds for a job charged to its memory limit, kind by
kind, with five leaks the same audit found fixed (F-37, W-15).

## 5. What cannot be fixed from here

Two things, down from five.

Independence needs people who do not work on the code (F-27). A QMS needs an
organisation (F-28). Field history needs years (F-30) and is unavailable rather
than unfixed.

The hazard analysis and the safety case are no longer on this list. Treating
the element as developed out of context — which is what it is — puts the
element's half inside reach and exports the system's half to whoever integrates
it. That was a mistake in the original audit worth naming: it assumed a
certified kernel needs a known application, when the whole SEooC / generic
software / reusable component apparatus exists because it does not.

And one genuinely unsettled question, worth raising with a certification body
early rather than at assessment: **no scheme has decided how to treat
AI-authored code in a certified item** (F-29). Every commit in this repository
has that provenance.
