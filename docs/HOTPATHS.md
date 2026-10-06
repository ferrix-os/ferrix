# Hot paths as agent instructions

The customer's idea of 2026-10-06, *agent-first optimization*: the hot paths
of the operating system are also written as instructions for an agent, so
that an agent can optimize one for a given architecture and machine, and the
results are keyed by a hash of the hardware so that agents on other machines
can find and reuse them. This page is the format. The first instance is the
IPC round trip: the skill `optimize-ipc-round-trip`
(`.claude/skills/optimize-ipc-round-trip/SKILL.md`) and its data file
`docs/hotpaths/ipc-round-trip.json`. A paper on the idea is the research
session's (po7-paper), coordinated through the product owner.

## 1. What a hot-path instruction is

Everything an agent needs to take one hot path from "here is the figure" to
"here is a faster figure, proven no less correct", without reading the
history that produced it:

1. **The path:** its entry and exit points, one direction of it, the files
   and functions it runs through, and on which architectures.
2. **The figure of merit** and the exact command and protocol that measure
   it (§4).
3. **The correctness envelope:** the requirement ids it touches, the checks
   and negative controls a change runs, the equivalence test where there are
   two implementations, the certification consultant's standing conditions,
   and a list of what may never be traded for speed.
4. **The knobs per architecture:** what each optimization on one ISA maps to
   on another, and what has no equivalent.
5. **The budget:** a span table of where one direction's time goes, now and
   after each planned step, against the target and the reference system.
6. **What has been tried**, each with its measured effect or the reason it
   is a guess, dead ends included.
7. **The hand-off:** what a result returns to the product owner (§8).

Every figure carries its evidence: *measured* (with the log or section it
comes from), *guessed* (an estimate to be measured), or *argued* (follows
from a document, not a run).

## 2. Where it lives: a skill backed by a data file

**Recommendation: one project skill per hot path, backed by one data file,
with results in a directory per path.** This is how the IPC round trip is
written.

| What | Where | Why |
|---|---|---|
| The instructions | `.claude/skills/optimize-<path>/SKILL.md` | An agent loads it by name, and its `description` makes the harness offer it when the work matches, as the `product-owner` skill is offered. Prose is what an agent reasons from: the lessons of a dead end do not fit a table. |
| The facts | `docs/hotpaths/<path>.json` | Files, the figure's command, the budget, every attempt with its evidence and source, the requirement ids, the never-traded list. Machine-checked by a unit test (§3), so a renamed file or a missing evidence label fails `check` instead of rotting; a tool or a paper can tabulate it; an agent updates one entry rather than rewriting prose. |
| The results | `docs/hotpaths/results/<path>/<hw hash>/` | One file per measurement (§6), filed by hardware so an agent finds its own machine's results by computing its fingerprint. |

The alternatives were weighed. A plain document under `docs/` is not offered
to an agent by the harness and has no description to match. A skill alone
puts machine-readable facts in Markdown, where nothing checks them. One
skill for all hot paths would load every path's history to work on one.

The skill states the protocol and the envelope in prose and points to the
data file for the full lists; the data file names the skill. Both name the
history (`docs/OPAQUE-KERNEL.md` §9 for IPC) as the source of truth: when
the two disagree, the history wins and the instruction is fixed.

## 3. The data file

`docs/hotpaths/<id>.json`, JSON within the canonical limits of §5 (whole
numbers only, so figures with ranges are strings). Its members:

| Member | What |
|---|---|
| `schema` | `ferrix-hotpath/1` |
| `id` | the file's name, and the results directory's |
| `title`, `skill`, `spec`, `history` | what it is, and the documents it is written from |
| `as_of` | the date, `main`'s commit, and which branches hold unlanded work |
| `definition` | `entry`, `exit`, `direction`, and `files`: every source file the path runs through |
| `figure` | `command`, `ab_command`, `line`, `statistic`, `unit`; what is `gated` and what is `reported_not_gated`; the `target`; the `pin` |
| `budget`, `budget_source` | rows of `piece`, `now_ns`, `after_<step>`, `target`, the reference system's, and `evidence` |
| `tried` | each attempt: `id`, `arch`, `what`, `status` (landed with its commit, branch, dropped, blocked, not started, finding), `effect`, `evidence` (`measured`, `guessed` or `argued`), `source` |
| `requirements` | requirement ids and the consultant's standing conditions |
| `never_traded` | what no optimization may give up |

The unit test `hotpath::tests::the_trees_hot_paths_check_clean`
(`tools/common/xtask/src/hotpath.rs`) holds every data file to this: it
parses, its id is its name, its skill, spec and every file it names exist,
and every attempt has its fields and an evidence of the three. It runs in
`check` with the rest of xtask's tests.

## 4. Measuring

The protocol each instruction states for its own figure, and the rules every
one follows:

- **ABAB.** A change is measured turn about with its base in one run, this
  tree first, and quoted as the median of the per-round ratios with their
  spread, the rounds, and the host's load. Never a lone absolute figure on a
  shared host: they move from hour to hour, the ratio holds.
- **p50** of the sorted samples, taken after an untimed warm-up. Other
  percentiles are reported; the tail has its own causes (the IPC p90 is the
  timer's reprogram) and its own work.
- **Load reporting.** Each run prints the host's one-minute load before and
  after, and the pinned core's SMT sibling's busy share over the run.
- **The quiet-host rule.** A figure that is cited is taken at load under 4
  with the pinned core's SMT sibling under 20% busy. A run outside that is
  reported with its load and retaken before anything depends on it. A busy
  sibling moved the IPC figure by 15 to 30%.
- **Pinned, one virtual processor,** to the core the reference system was
  measured on. One session's benchmark at a time on that core: look before
  starting, and never kill another session's run.
- **Resolution.** State the counter's step (the guest TSC on nazuna steps in
  10 ns) and how the percentile is taken. Savings below the bench's
  resolution are not measured, and an attempt judged on a coarser bench is
  marked so, not called a dead end.
- **Outliers** are reported, not dropped; if the median depends on one,
  more rounds are taken.
- **Profiling is separate from measuring.** A timing build stamps the path,
  subtracts the stamp's cost, and is never landed; ablations (one piece
  skipped at a time, measured ABAB) give a piece's cost without the stamps'
  distortion. Its spans rank costs; the figure comes from the uninstrumented
  bench.
- **Under emulation** (TCG) only instruction counts mean anything; TLB,
  cache and switch costs are measured under a hypervisor or on hardware.

## 5. The hardware fingerprint

`cargo xtask hw-fingerprint [--arch A] [--accel X]` prints a canonical JSON
description of the measuring machine and its hashes:

```
hw-fingerprint x86_64: <sha256> (directory <12 hex>); host <sha256>; cpu <sha256>
```

**What it holds.**
- `host`: the processor -- vendor, family, model, stepping and brand string
  by CPUID on x86 (implementer, part, variant, revision, and every distinct
  part of a big.LITTLE host on Arm), the microcode revision; the features a
  hot path's knobs depend on, each `true` or `false` (absence is a fact: "no
  PCID" decided step 3) -- PCID, INVPCID, FSGSBASE, the XSAVE family and
  `XGETBV 1`, AVX/AVX2/AVX-512F, SMEP/SMAP/UMIP, PKU, LA57, FRED, the
  speculation controls (IBRS, IBPB, STIBP, SSBD, AutoIBRS, `ARCH_CAPABILITIES`,
  `MD_CLEAR`), ERAPS, TCE, `NullSelectorClearsBase`, `LFENCE` serialising,
  RDTSCP, invariant TSC; on Arm Linux's `Features`; processor 0's caches
  (level, type, size, ways, line, sets, how many processors share it); the
  TLBs (AMD's leaves decoded, Intel's leaf 0x18 as its words); the topology
  (logical processors, cores, threads a core, packages); whether the host is
  itself a virtual machine and whose; the host kernel's release; and the
  kernel's `vulnerabilities` lines, which say what the host pays on every VM
  exit.
- `vm`: what the measured guest sees -- the architecture, the accelerator,
  QEMU's version and package, the `-cpu` model string the gate passes, and
  the realised processor read back over QMP. QEMU is started with the same
  machine, accelerator and model, stopped before its first instruction
  (`-S`), and each feature property is read with `qom-get`, so what KVM
  filtered out is seen as absent (asking for `+pcid` on nazuna reads back
  `false`). No guest code runs.
- `schema`: `ferrix-hw-fingerprint/1`.

The kernel's own account of the processor it found -- its boot log's `cpu`
lines, such as which speculation defences it turned on -- is not in the
fingerprint: it depends on the build as much as the hardware. It is in each
result record as `kernel_view`.

**Canonical form.** Object keys in byte order; no white space; whole
numbers only, no `-0`, no leading zeros; strings escaped only where JSON
must. Before that, the fingerprint normalises what it reads: free text
(the brand string, the kernel's lines, QEMU's version) with runs of white
space squeezed and trimmed, the microcode as lower-case hex without leading
zeros, and lists whose order the machine does not define (caches, Arm parts)
sorted. This is
RFC 8785 restricted to integers. The parser that reads records back
(`tools/common/xtask/src/hotpath/json.rs`) refuses anything outside these
limits, so a record that parses has one canonical text. The unit tests hold
it: two layouts of one value give one text, the inputs' order does not
change it, and the pretty form written to files hashes as the canonical.

**Three hashes**, each the SHA-256 of a canonical text:
- `hw_hash`, of the whole fingerprint: the key results are filed under, by
  its first 12 hex digits. Any change -- microcode, host kernel, QEMU, the
  CPU model -- is a new machine, because each can move a figure.
- `host_hash`, of `host` alone: the same machine under another QEMU or model.
- `cpu_hash`, of the processor, features, caches, TLBs and topology without
  the microcode: the same silicon on someone else's machine. An agent looks
  for its `hw_hash` first and falls back to its `cpu_hash`.

**Privacy.** Only named fields are copied, never a whole file. No serial
number, MAC address, host name, user name or path under a home directory
reaches a fingerprint or a record (a test feeds an Arm `cpuinfo` with a
`Serial`, a `Hardware` and a board `Model` and requires none of them in the
text; `bench-ipc` records `--init`'s file name, not its directory). The one
free-text field is the host kernel's release string, which its builder
chose; a published record's should be read before it goes out.

## 6. The results record

`bench-ipc --record` writes one JSON file per measurement:

```
docs/hotpaths/results/<path>/<hw hash, 12 hex>/<UTC time>-<commit, 12 hex>-<configuration hash, 8 hex>.json
```

so the four keys -- hot path, hardware, Ferrix commit, configuration -- are
all in its name, and two measurements never collide. Its members:

| Member | What |
|---|---|
| `schema` | `ferrix-hotpath-result/1` |
| `path`, `figure` | the hot path's id; the figure's line, statistic and unit |
| `hw_hash`, `host_hash`, `cpu_hash`, `hardware` | the fingerprint whole, and its hashes |
| `ferrix` | `commit`, and `dirty` if the tree differed from it outside the results directory |
| `configuration`, `configuration_hash` | every option that changes what is measured: release, accelerator, processors, memory, pin, mitigations, kernel options, `--init`'s file name, rounds, the alternated ref |
| `runs` | each boot's summary statistics (n, min, p50, p90, p99, mean of each line) and its host line |
| `reference` | the alternated tree (ref, commit, its runs) or the other kernel's p50s, or `null` |
| `summary` | rounds, the median p50, the reference's, and the per-round ratios' median, least and most in thousandths |
| `kernel_view` | the kernel's boot-log statements about the processor and its defences |
| `log` | `FERRIX_HOTPATH_LOG`, the path of the run's full output, if given |
| `taken` | UTC |

Raw samples are not kept: `ipc-bench` prints six statistics of its 20,000,
and the log holds the run. A later benchmark that prints a histogram adds it
under `runs`.

The unit test `hotpath::record::tests::the_trees_records_check_clean`
checks every record in the tree: it parses, names its schema, its
`hw_hash` is the SHA-256 of its `hardware` and its `configuration_hash` of
its `configuration`, and it sits under its own path and hash. A record moved
or edited by hand fails it.

Records are committed on the branch with the change they measure, and land
with it. A rebase or squash rewrites the commit a record names, so before one
keep the measured commit reachable with a side ref
(`<session>/measured-<commit>`), or measure again on the final tip; the
landing's report says which.

## 7. Publishing (design only; the customer decides)

Nothing here is built, no repository is made and nothing is pushed: results
leaving this repository is outward-facing, and the customer's call.

**The proposal.**
- A public repository in the ferrix-os organization, for example
  `ferrix-os/hotpath-results`, with the same layout as
  `docs/hotpaths/results/`: `<path>/<hw hash>/<record>.json`, plus a copy of
  each path's data file at the commit its records were taken at.
- **Submission by pull request.** CI runs the same record check as §6 and
  refuses a record whose hashes do not recompute, whose `ferrix.commit` is not
  a commit of `ferrix-os/ferrix`, whose `dirty` is true, or that holds a
  field the privacy rules exclude. A human or the product owner merges.
- **Records are claims, not proofs.** An agent that finds a result for its
  `hw_hash` reproduces it with the record's own configuration before relying
  on it, and adds its own record either way. A `cpu_hash` match is a hint
  about which knobs to try first, not a figure.
- **Licence:** CC0 or CC BY 4.0 for the records (data); the tree's own
  licence for the data files, which are documentation of its code.
- **This tree keeps its own records** under `docs/hotpaths/results/` as the
  development history; the public repository is a mirror of what was chosen
  to be published, pushed by whoever holds the customer's word to push.

Questions for the customer: whether to publish at all; the repository's name;
the records' licence; whether submissions from outside the fleet are
accepted; whether the host kernel's release string stays in published
fingerprints.

## 8. Handing a result back

What an agent returns when it has optimized a hot path, to the product owner
and, for a change inside the certified item, to the certification consultant
first:
- the branch, its tip, and each change in one line;
- the record's path, and the ABAB line: figure, base figure, median ratio,
  spread, rounds, load; the log's path;
- the gate logs and every negative control with its verdict;
- the consultant's ledger line, or "not reviewed";
- the data file's changes: a `tried` entry for every attempt, the dead ends
  included, with evidence and source, and the budget rows it moves.

The skill and the data file are updated in the same branch as the change,
so the next agent starts from the result.

## 9. Adding a hot path

1. Pick a path whose figure a benchmark already prints, or write the
   benchmark first; a hot path without an exact, repeatable figure is not
   ready for an agent.
2. Write `docs/hotpaths/<id>.json` from the path's history, with every
   figure's evidence.
3. Write `.claude/skills/optimize-<id>/SKILL.md`: the path, the protocol,
   the envelope, the budget's reading, the lessons of what was tried, the
   per-architecture map, the hand-off. Its `description` says when to load it.
4. Teach the benchmark `--record` (`tools/common/xtask/src/hotpath/record.rs`
   takes any benchmark whose lines read `<prefix> <name> n=… p50=…`).

Candidates after the IPC round trip: the block ring's 4 KiB read
(`seam-trip`, against Redox's 80 us), boot time per architecture, and the
compositor's frame time.
