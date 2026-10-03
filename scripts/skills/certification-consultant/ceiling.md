# The ceiling: the highest level of each standard

Customer, 2026-10-03: one day Ferrix should reach the highest level of
assurance there is, in every standard that rates an operating system kernel.
The required targets haven't changed: EAL5+, DAL C, Class C and SIL 2
(`docs/BACKLOG.md`, Decisions, 2026-10-01). The ceiling is where the work
heads after them, so each review also says what would carry the change
toward it.

This file is a map. It isn't evidence. Before anything here is cited in
`docs/certification/`, check the clause or table against the standard's own
text, because the summaries below are from memory of the standards.

## The levels

| Standard | Domain | Required now | Ceiling | What the ceiling adds, in short |
|---|---|---|---|---|
| DO-178C / ED-12C (DO-178B was superseded in 2011, and authorities accept C for new work) | Airborne software | DAL C | **DAL A** | MC/DC coverage; verifying source-to-object code where the compiler adds code the source doesn't trace to; the most objectives that must be met with independence; DO-330 tool qualification at TQL-1 for tools that could insert errors; DO-332 for object-oriented features and dynamic memory; DO-333 formal methods, which may replace some testing; AMC 20-193 (formerly CAST-32A) on multicore interference |
| ISO 26262:2018 | Road vehicles | (none) | **ASIL D** | MC/DC highly recommended at unit level; freedom from interference in space, time and communication; semi-formal and formal notations and verification; tool confidence level TCL1 or qualified tools (part 8 §11); qualification of software components (part 8 §12) |
| IEC 61508:2010 | Generic functional safety (machinery, process, and the base EN 50716 and ISO 26262 derive from) | (none) | **SIL 4** | Formal methods highly recommended; MC/DC; independent assessment by an independent organisation; a fully defined, restricted language subset |
| EN 50716:2023 | Railway | SIL 2 | **SIL 4** | Formal methods and formal proof highly recommended; verifier, validator and assessor independent of the designer, and the validator independent of the project manager |
| IEC 62304 | Medical device software | Class C | **Class C** (already the highest) | No higher class. Add IEC 81001-5-1 (health software security) to cover security as well as safety |
| Common Criteria (ISO/IEC 15408) | Security evaluation | EAL5+ | **EAL7** | A formal security policy model (ADV_SPM); formal functional specification and design (ADV_FSP.6, ADV_TDS.6); minimally complex internals (ADV_INT.3); vulnerability analysis against high attack potential (AVA_VAN.5); complete independent testing (ATE_IND.3); full configuration management (ALC_CMS.5). Which CC version applies is open (F-52) |

### Worth tracking, not yet targets

Each of these is the ceiling in its own domain. Name one in a review only
when a change bears on it directly.

* **ISO/SAE 21434 CAL 4**: automotive cybersecurity, paired with ASIL D.
* **IEC 62443-4-1 ML4 and 4-2 SL 4**: industrial control systems.
* **DO-326A / DO-356A (ED-202A / ED-203A)**: airworthiness security, paired
  with DAL A.
* **ECSS-Q-ST-80C criticality A** (space), **NASA NPR 7150.2 Class A**.
* **IEC 60880 category A**: nuclear instrumentation and control.
* **ISO 13849 PL e, ISO 25119 AgPL e**: machinery and agriculture. Both lean
  on IEC 61508, so SIL 4 work covers most of them.

Precedents for kernels at this level: seL4 (a formal proof of functional
correctness down to the binary), INTEGRITY-178 (DAL A, and EAL6+ against the
SKPP), PikeOS, VxWorks 653 and QNX (DAL A or ASIL D, SIL 4 for some
configurations). Ferrocene, the Rust toolchain, is qualified for ASIL D,
SIL 4 and Class C, which is where F-17 points.

## What the ceiling asks for, across all of them

The same demands come back in every standard. These are the directions a
review nudges in.

1. **MC/DC on the item.** Decision coverage is measured already
   (`docs/certification/decision-coverage-*.json`). Don't assume a tool
   measures Rust MC/DC: find out what can measure it before asking anyone
   for it. Until then, prefer conditions a reviewer can enumerate: small
   `match`es over long `&&`/`||` chains, and early returns with one condition
   each.
2. **Formal methods.** These are a formal security policy model (EAL7),
   formal design (SIL 4, DO-333) and proofs for small modules. Loom models
   already exist. Nudge toward invariants stated so they could be proved
   (Kani, Verus, Prusti, or a TLA+ model of a protocol), and toward
   interfaces small enough to model.
3. **Low-level requirements for every function**, each tested with normal
   and robustness cases, and requirements-based tests rather than tests
   written to reach coverage. A change that adds behaviour without a
   requirement narrow enough for one check is already a finding today.
4. **Independence.** The person who verifies is not the author (DAL A,
   ASIL D, SIL 4). Reviews today are internal (F-27, F-29). Nudge toward
   records that show who wrote and who verified each piece.
5. **A qualified toolchain and object code you can trace** (F-17). Code the
   compiler adds, such as bounds checks, panics, drop glue and
   monomorphisation, is untraced object code at DAL A. Nudge toward
   panic-free paths (`check-panic-audit.py`) and fewer generic expansions in
   the item.
6. **Time as well as space.** This means worst-case execution time, bounded
   lock hold times, deterministic scheduling and multicore interference
   (AMC 20-193). Nudge every new loop under a lock toward a stated bound in
   `MEMORY-AND-TIMING.md`.
7. **No dynamic allocation after initialisation** at DAL A and SIL 4, or a
   strong argument for it. F-23 made allocation fallible. The next step is
   static budgets and pools fixed at boot.
8. **A defined coding standard.** Candidates are the Ferrocene Language
   Specification as the language definition, MISRA's Rust guidance and the
   Safety-Critical Rust Consortium's guidelines. Nudge toward rules a tool
   can check.
9. **Minimal complexity** (ADV_INT.3). The complexity baselines may only
   shrink. Nudge toward splitting a file or function rather than growing it.
10. **Configuration management and problem resolution** (ALC_CMS.5,
    IEC 62304 §8 and §9): every pinned outside dependency matches
    `SOUP.md`, and every problem report ends in a fix, a finding or a
    recorded decision.

## How a review nudges

* **The verdict is decided against the required targets only.** A ceiling
  objective never turns an OK into "not yet". The customer sets what is
  required, and the ceiling isn't required yet.
* After the verdict, add at most two **ceiling advisories**, tagged
  `[ceiling]`. Each one names its standard and objective, for example
  `[ceiling] DAL A MC/DC: split the 4-term condition in claim_up_to`. Pick
  the ones that are **cheap now and expensive later**: a requirement split, a
  stated bound, a smaller interface, a removed allocation. Leave out the
  ones that only restate the list above.
* Record the advisories in the ledger with the verdict. When the same
  advisory comes up three times, turn it into a gap: add a row to
  `docs/certification/CEILING.md` (create it in your next docs landing if it
  doesn't exist yet), with the standard, the objective, where Ferrix stands
  and the smallest next step.
* When a design review comes in, ask the ceiling question first: *would this
  design still work at DAL A / ASIL D / EAL7?* A design that would have to be
  rebuilt for the ceiling is worth one more paragraph of argument now.
* Never claim a ceiling level, or progress toward one, as met. Write "toward
  DAL A", never "DAL A ready".
