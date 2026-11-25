# DO-178C objectives for `max`

A skeleton, started 2026-11-26 (night run, agent sebastian-27d, on
`origin/develop` at 88f5215d2). It is the file `AGENTS.md`'s *Auditing against
the standards* asks for, and it is not yet the audit.

**The standard's text is not held on this host.** Everything here about
DO-178C's Annex A, the table names, the table numbers and the shape of an
objective, is from the author's memory and not checked against the text, as
`FINDINGS.md` says of its own clause numbers. So no row below is written, and
no objective is numbered or worded: writing one from memory and calling it
read is what `AGENTS.md` forbids. Each table has one placeholder row. The
first audit that holds the text replaces it with one row per objective (the
standard's own number and wording; the evidence in the tree by file and
section; a status of met, partly, not met or not assessed), and checks the
table names too.

## Whose claim, and what stands today

Only the `max` profile carries DO-178C, at level A's objectives, on AArch64,
from M7's evidence (the customer, 2026-11-26; `docs/backlog/DECISIONS.md`,
`docs/BUILD-FLAGS.md` §1 and §4). `max` does not build yet
(`docs/microkernel/FLIP.md`), and a profile that builds carries no claim
(`docs/BUILD-FLAGS.md` §2.1). Between the flip and M7's evidence no profile
carries DO-178C. So **no objective is stated as met on `max` in this file**,
and none can be until the profile builds and M7's evidence is taken on it.
The evidence in the tree today is `present`'s, taken at level C as the interim
claim until the flip, on an image `max` will not be; it is cited below only as
what a later row may start from, never as `max`'s.

## Status values

* **met**: the evidence is read and is on `max`'s image or process.
* **partly**: the evidence is read and covers some of what the objective asks.
* **not met**: an open finding says so (cited), or the evidence was read and is absent.
* **not assessed**: nobody has read the objective's text against the tree.
* **not read from the text**: the row is a placeholder; the objective itself
  has not been read, so no status is possible.

## Annex A, as remembered

Table names are from memory. The last column names an open finding in
`FINDINGS.md` that the author knows bears on the table and has read in this
register; it is the author's reading of which table a finding touches, not from the text, a pointer and not a status for any objective.

| Table | Subject (from memory) | Objectives | Evidence in the tree | Status | Open findings that bear on it |
|---|---|---|---|---|---|
| A-1 | Software planning process | not read from the text | not assessed | not read from the text | F-28 (no quality management system) |
| A-2 | Software development processes | not read from the text | not assessed | not read from the text | none read for this table |
| A-3 | Verification of the outputs of the requirements process | not read from the text | not assessed | not read from the text | F-27 (no independence) |
| A-4 | Verification of the outputs of the design process | not read from the text | not assessed | not read from the text | F-27 |
| A-5 | Verification of the outputs of the coding and integration process | not read from the text | not assessed | not read from the text | F-17 (the compiler is unqualified), F-27 |
| A-6 | Testing of the executable object code | not read from the text | not assessed | not read from the text | F-11 (coverage measures the debug profile; reopened 2026-11-25) |
| A-7 | Verification of the verification process results | not read from the text | not assessed | not read from the text | F-10, F-11 (statement coverage), F-13 (no decision or MC/DC coverage), F-118 (no data and control coupling analysis) |
| A-8 | Software configuration management | not read from the text | not assessed | not read from the text | F-120 (the review record, narrowed 2026-11-26, still open) |
| A-9 | Software quality assurance | not read from the text | not assessed | not read from the text | F-27, F-28 |
| A-10 | Certification liaison | not read from the text | not assessed | not read from the text | none read for this table |

F-119 (the toolchain's libraries in the item) bears on several tables and is
not assigned to one here. Which tables carry which objectives at level A, and
which of those the standard asks to be met with independence, is part of what
the text must be read for; this file asserts neither.

## What the first audit that holds the text does

1. Replaces each placeholder with one row per objective, from the text, and
   corrects the table names above.
2. For each row, reads the evidence it names and gives the file, the section
   and the gate; or cites the open finding; or says why the objective does
   not apply. A row with none of the three is a new finding.
3. Takes the evidence for `max` from `max`'s image and from M7. Until `max`
   builds, the rows can say what `present`'s evidence is and that it is not
   `max`'s.
4. Names the text it was written from (title, edition, who supplied it), so
   the next audit knows what was read.
