# Ferrix — backlog and decisions

`docs/roadmap/` says what each stage is and how it knows it is finished.
This file says who is doing what right now, in what order, and which
decisions were taken along the way. It exists because a dozen sessions work on
the tree at once, and a decision that lives only in a message between two of
them is a decision the third one reverts.

The customer decides scope and priority; the product owner session acts for
the customer between the customer's words and keeps the landing order, and
the certification consultant reviews changes to the certified item
([AGENTS.md](../AGENTS.md) describes both roles). A session that lands a piece of this
file updates its row in the same landing, the way it updates the stage's
file under `docs/roadmap/` (and `status.md`'s status table when a row
changes). When a row is done it is deleted, not struck through;
the roadmap records what landed, and this file's own history
(`git log -p -- docs/BACKLOG.md`) keeps the investigations and the wind-down
records of earlier days.

**Keep this file short (customer, 2026-10-16).** Every session reads it, so
its length is paid in every session's context. A row says in about forty
words what is wrong or wanted, where it stands, its owner and its points,
and links to its detail in `docs/backlog/<section>.md` under its id
(`[more](backlog/flakes.md#fl-012)`): sightings, logs, measurements,
history and reasoning go there, never into the row. A new item that needs
more than a row gets a detail entry the day it is filed. Decisions are in
[backlog/DECISIONS.md](backlog/DECISIONS.md).

---

## Standing rules

These add to `docs/CONVENTIONS.md`, which still governs commits.

**What a landing runs.** Rebase onto `main`, then:

| The change touches | Gate |
|---|---|
| Only documentation: `docs/`, top-level Markdown, the skills and agent definitions under `.claude/` (Markdown nothing compiles), and the generators that write documents alone (`tools/common/gen/gen-roadmap-charts.py`, `gen-ipc-chart.py`, `gen-arch-doc.py`, `split-roadmap.py`, `build-roadmap-book.sh`) | `cargo xtask check-docs` only: the commit hooks, the audits and the generated-document checks, no cargo step (customer, 2026-10-04); it runs on any host, the Windows PC included. A generator that writes code or test data (`gen-xkb-tables.py`, `gen-wayland-protocol.py`, `gen-font.py`, `gen-btrfs-fixtures.py`, the fuzz-corpus seeds) keeps the row of what it writes |
| Only `src/user/system/linux/ferrousli/` | `cargo xtask check --ferrousli`, then `cargo xtask busybox` and, with the busybox it built, `test-shell` and `test-vfs` on x86_64 with `--init ferrousli`, so the binary the gates run never lags the library; then `cargo xtask uutils`, which links uutils/coreutils against it and is the larger consumer of the two, since it brings Rust's whole `std` with it; a change to `src/user/system/linux/ferrousli/tools/ports/` also runs `cargo xtask build-apps` for the ported apps it touches (curl and git on every architecture) and `test-net --arch x86_64 --init ferrousli`, which fetches with the curl it built, then `test-net --arch all` with the static busybox, which fetches and clones with them |
| Only `src/user/system/linux/zinc/` | `cargo xtask check --fast --zinc`: zinc's formatting, clippy, unit tests and the two pty tests (`src/user/system/linux/zinc/tests/pty_completion.py`, `src/user/system/linux/zinc/tests/pty_jobs.py`); a change to what zinc does at boot also runs `test-boot` on x86_64, and a change to how it starts, waits for or signals a process also runs `test-shell --arch all` and `test-jobs`, which is the only gate that types at a console |
| Only `src/user/system/linux/init/`, or `src/lib/init/svc` | `cargo xtask check`, which runs `src/user/system/linux/init/`'s formatting, clippy and tests by default and `src/lib/init/svc`'s with the host's, then `cargo xtask test-init --arch all`, which boots `/sbin/init` as pid 1 and types at the shell its getty gives |
| `src/user/system/linux/auth/`, or `tools/common/xtask/src/auth.rs` | The image row below, since `authd` goes into images, then `cargo xtask test-auth --arch all`, which boots init with `authd` and types each refusal of `docs/AUTH.md` at the getty; a change to what `authd` refuses also runs `test-auth --arch x86_64 --sabotage NAME` for the four names in `src/user/system/linux/auth/authd/src/sabotage.rs`, each of which must fail on the line it names |
| `src/lib/` only, and no crate the kernel builds | `cargo xtask check`, and one boot: `test-boot --arch armv7a --smp 2` |
| Anything the image contains: `src/kernel/`, `src/boot/common/uefi/`, a kernel-side crate in `src/lib/`, `xtask` | `cargo xtask check`, then `cargo xtask build --arch all --release`, since CI builds and boots the release profile and no other gate does, then `test-boot` on x86_64 (under KVM by default on an x86-64 Linux host since 2026-10-01, `docs/TEST-TIME.md` C3), aarch64, armv7a at four processors and armv7a at `--smp 2`; a stage 7 or 8 change also runs `test-shell` on x86_64 with no `--init`, which runs zinc, the image's shell, and then with the ferrousli busybox (`--init ferrousli`), the musl busybox *and* the host's glibc busybox (`/usr/bin/busybox`), and `test-vfs` on x86_64 with the ferrousli busybox and the musl one; a stage 7 change also runs `test-threads --arch all`, the Rust `std::thread` program (since 5fd2ab09); a change to the loader (`src/user/system/linux/ferrousli/ld`), to `exec` or to ferrousli also runs `test-shell` with Debian's dynamic busybox twice, on glibc's own `ld.so` and `libc.so.6` on all three architectures and on `--interpreter ferrousli --library ferrousli` on x86_64 (docs/ROADMAP.md, dynamic linking; `tools/common/fetch/fetch-debian-busybox.sh` fetches it). The whole of that, plus x86_64 under `--accel tcg` (or under `--accel kvm` where the plain boot was emulated, as in CI), is what moves `main` |
| User mode, page tables, TLB, SMP or the scheduler | The row above, and x86_64 under `--accel tcg` (the plain x86_64 boot is the KVM one where KVM is the default) |
| The page cache and its reclaim (`src/kernel/src/fs/pages.rs`, `src/kernel/src/user/cache.rs`, `src/kernel/src/user/vmo.rs`), the frame allocator and its watermark (`src/kernel/src/mm.rs`, `src/kernel/src/mm/`), btrfs (`src/kernel/src/fs/btrfs*`, `src/lib/fs/btrfs/`) or the job's memory charge (`src/kernel/src/object/quota.rs`) | The image row, and `test-btrfs-pressure --arch x86_64 --accel kvm --init <the musl busybox.static>`, about 3.5 min: three writers and two readers fill a 768 MiB machine through btrfs, then delete it all, and a second boot fills the cache and makes 8192 small files, which must not drop it (`docs/BTRFS-PRESSURE.md`; consultant ledger 2314 (7)). `gate-rows` adds it. The starved form (`--smp 2 --cpu-quota 40 --pin`) is a manual check, never a row; the cgroup form joins when F-86 to F-88 close. Not every batch: only a change to these paths |
| x86-64's machine check: `src/kernel/src/arch/x86_64/cpu.rs`, `paranoid.rs`, `paranoid/check.rs`, `trap.rs` or `smp.rs` | The image row, and `test-mce --arch x86_64` under `--accel kvm` and under `--accel tcg`: a machine check injected into every processor after stage 4 must stop the kernel as FX-9005 (F-73) |

**Name the accelerator.** Since 2026-10-01 the same `test-boot --arch
x86_64` runs under KVM on nazuna and under TCG in CI (`docs/TEST-TIME.md`,
C3), so a gate claim in a commit message or report names each x86_64 row's
accelerator, as each boot's `qemu: <arch> under <accel>` line and
`gate.sh`'s verdict (`[accel x86_64:kvm]`) say it. A negative control whose
result depends on the accelerator -- the side-channel and XSAVE controls,
whose processor controls are real under KVM and absent under TCG -- carries
`--accel` on its command line wherever it is written down, and a re-run of
an older control whose commit says TCG passes `--accel tcg` (certification
consultant, 2026-10-01).

**Landing on `develop`.** Since the switch of 2026-10-23 (customer;
`docs/REPO-FLOW.md`) the landing branch is `develop`, and `main` moves only
by promotion pull requests that merge themselves when CI and the
certification verdict pass. In the rules below, and wherever a landing step
says `main` -- rebase onto it, fast-forward it, boot it, push it -- read
`develop`; `fleet/land-branch` holds the branch `land.sh` and `batch.sh`
use. Before `batch.sh join`, a branch that touches the certified item runs
`tools/common/release/cert-verdict.py develop..<branch>`: every such commit
names its review or carries a `Certification:` line, or its promotion
waits. `develop` moves only
under the fleet landing lock, `~/.local/share/ferrix/fleet/land.sh`
(`queue`, `take`, `release`; `status` shows the queue), held by the fleet
coordinator's rules (customer, 2026-09-26):

* Gate outside the lock, on a base rebased onto `main`. Then `land.sh take`,
  `git rebase main`, and fast-forward: at once if `main` is still the gated
  base, or if it moved only in files the change does not touch and nothing
  cross-cutting (locks, the scheduler, the trap or system-call entry, memory
  management). Otherwise release, re-verify and queue again.
* A branch that writes new requirement ids reserves them first, in
  `tools/common/data/requirement-reservations.json`, as a docs-only landing
  under the lock, and deletes the entry in the commit that writes the ids
  (`docs/CONVENTIONS.md`, "Requirement ids are reserved before they are
  written"). `cargo xtask check` refuses an unreserved new id and an
  unreleased entry.
* Hold the lock only for the rebase and the fast-forward, and `land.sh
  release` at once. Two minutes is the target hold; a hold of fifteen is
  stale.
* Each green slice lands within the hour, one landing deep, about eight
  points. A live branch unlanded for more than four hours is asked for its
  plan.
* Move `main` from a clean root with `git merge --ff-only`, or by
  compare-and-set (`git update-ref refs/heads/main <new> <old>`) followed by
  syncing the root checkout; a root left behind shows the landing as staged
  deletions, and a commit from it would revert the landing.
* Run `land.sh take` on its own line and test its exit before anything that
  moves `main`; never pipe it. On 2026-10-01 a `take | tail` hid the refusal
  and a landing went in under another session's hold.
* When the root checkout holds another session's uncommitted edits in files
  the landing changes, or a `.git/MERGE_HEAD`, do not move the root's `main`:
  land on `origin` and the gate host only, and leave the root for its owner
  or the product owner. Never merge into another session's uncommitted file,
  and never clear a stale merge with `git merge --abort` or a reset, which
  discard those edits; `git merge --quit` keeps them.
* After the fast-forward, the lander boots `main` once on x86_64 under
  `--accel kvm` and reports the hash with that result, so a bad merge is
  seen by the one who made it; then pushes `main` to the gate host
  (`git push example-wg:Documents/projects/os/ferrix main:main`), so no
  worktree there is made from a stale `main`. A push to `origin` needs the
  customer's word, given in the session that pushes; pushing `develop` after
  a landing has it standing (2026-10-23), and the Promote workflow carries
  it on to `main`.

**Re-verifying after `main` moved.** When the commits that moved it touch
none of the files the change touches, the gate still stands: say so in the
report and land (`docs/CONVENTIONS.md`, splitting rule 3). When the files
overlap, or the change is cross-cutting, re-run `cargo xtask check` and two
boots, `armv7a --smp 2` and x86_64 under `--accel kvm`, or the whole row
when the overlap is in code the change depends on. A boot that fails is a
result to read, not a reason to retry: keep its log, rerun once, and give the
failure a row the day it is seen (CONVENTIONS rule 5).

**The busyboxes.** The busybox built against ferrousli is the primary one: the
userland Ferrix is measured with, and the one every `test-shell` and
`test-vfs` above names first. `cargo xtask busybox` builds and installs it and
`--init ferrousli` runs it; the flag is still given explicitly, since xtask
has no default program. Alpine's static musl busybox and the host's glibc
busybox stay required in every gate that names them, as the compatibility
checks: a failure on any of the three fails the gate, and nothing was dropped
when ferrousli's joined. `--init ferrousli` rebuilds the busybox first when
it is missing or older than anything under `src/user/system/linux/ferrousli/` it is built from, so
the binary a gate runs is the base's.

**Gates run on the Linux host, not in WSL (customer, 2026-09-16).** Builds,
tests and every gate run on example over `ssh example-wg`, in the session's own
worktree there; WSL on the Windows machine is a convenience for a look, never
the reference, and a row run there does not count.

**A failing gate's log is kept before any re-run (2026-09-16).** Copy it
aside first; a re-gate that overwrites it turns a result into a rumour — the
one full trace of FX-0701 was lost that way.

**One build directory per worktree on the Linux host (2026-09-16,
corrected 2026-09-26).** Every worktree sets
`CARGO_TARGET_DIR=~/.local/share/ferrix/target-<session>-<branch>` and never
grows its own `target/`; the directory goes when the worktree does. Two
worktrees never share one, not even two of the same session's: xtask finds
the repository through `CARGO_MANIFEST_DIR`, which is fixed when xtask
compiles, so a shared directory runs whichever tree last built xtask, with
that tree's `build/` images (a certification row was voided that way on
2026-09-26), and Cargo names a workspace member's artifacts without its path,
so a stale crate from the other tree passes for current. The root filesystem
filled twice on 2026-09-16 from build output; delete a directory the moment
its worktree is removed.

**Nobody works in the root checkout, and landings are small and often
(customer, 2026-09-16).** The root checkout keeps `main` checked out and
clean; a session that edited there blocked every other session's
fast-forward. Every session works in its own worktree under
`.claude/worktrees/`, and lands each stable, gated step on `main` the day it
is green — a worktree is never more than one landing deep, and a 40-point
milestone is ten landings, not one.

**Worktrees.** One landing, one worktree. `git worktree remove` it once its
branch is on `main`. Check `df -h /` before a landing; after a failed commit
read `git log -1 --stat` before the next step, because a failed commit leaves
its files staged for the next one. On 2026-09-13 the root filesystem filled and
every session's gates failed at once; 70 worktrees held 108 GB of build output.

**The host is shared.** One cargo build, clippy run or QEMU boot at a time
per session, agents included: a session with agents serialises them. Miri and
fuzz runs one at a time machine-wide, and never while a boot matrix runs
anywhere. No worktree under `/tmp`: it is a 30 GB tmpfs, so a build tree
there lives in RAM; the scratchpad is for logs and patches only. Run `free -g`
before a boot matrix and wait while available memory is under 12 GB. On
2026-09-13 the host reached 47 of 59 GB used with swap full, and the customer
reported it close to stalling: three worktrees on the tmpfs held 9 GB of build
output, and ten sessions were building and booting at once.

**Changes inside the certification item are reviewed first (customer,
2026-09-26).** The certification session (ferrix-55 at the time) stays as a
standing consultant. A change inside the item -- what
`tools/common/data/certification-item.json` puts in the `core` or `item` ring and
`tools/common/check/check-item-boundary.py` enforces; the `load` ring above it is
outside -- gets its one-line OK before the landing lock is taken: send it the
files, what changes and how it is tested. A structural change gets a design
review before the code. The consultant records found-and-closed findings,
coverage entries and threat updates in `docs/certification/` in small
batches, and flags item changes on `main` that skipped it.

**An item change carries its coverage anchors (2026-09-27).** Since the
coverage evidence landed (41456b68), a change that moves a line of the item
makes `gen-coverage-justification.py --check`, and so `cargo xtask check`,
fail on stale anchors. Before landing it, run `python3
tools/common/gen/carry-coverage.py && python3
tools/common/gen/gen-coverage-justification.py` on the rebased tree and commit
the result: it renumbers the anchors through the diff, drops and prints the
lines the change edited, and never re-measures.

**Every `unsafe` in the item names its obligation (2026-09-27).** Since
F-26 closed (1be610fe), `tools/common/check/check-unsafe-audit.py` fails `cargo
xtask check` on an `unsafe` site in the `core` or `item` ring that does not
name one of the obligations registered in
`tools/common/data/safety-requirements.json` and tabled in
`docs/certification/SAFETY-MANUAL.md` §2 (CONTEXT, SYSREG, SHARED, ENTRY,
TRANSLATE, DEVICE, FIRMWARE, PROTECT, KMEM, FRAME, PROBE, DMA, BOOT-DATA,
USER-COPY): `// SAFETY: (ID) prose` on a block, and `/// (ID) ...` as the
first line of an `unsafe fn`'s `# Safety`. A new obligation is added to the
register with the certification consultant, not invented in place.

**Files every landing appends to overlap by function, not by file
(2026-09-27).** `src/kernel/src/syscall/check.rs`, the panic catalog,
`docs/generated/*` and this file change in nearly every landing, and
counting any change to them as an overlap had a finished landing chase
`main` twice. After a clean rebase, changes on `main` only in other
functions or rows are no overlap for the re-verify rule above -- but
`check.rs` runs at boot, so such a landing still runs `cargo xtask check`
and one x86-64 boot on the rebased tree. A change to the landing's own
function, chain or files is an overlap as before.

**Agents.** Gates and boots in the foreground, never `run_in_background`; one
architecture per tool call; the brief says so.

**Milestones.** The customer tests from `main`, so testable progress is
tagged there rather than waiting for a stage to end: `stage-N` for a stage's
exit, `stage-N.k-<slug>` for a testable step after it, annotated, the tag
message carrying short release notes as a bullet list and saying what to test
and how; the same notes go into `docs/RELEASES.md`. A tag is placed only after
the whole matrix has passed on that commit from a clean worktree
(`~/.local/share/ferrix/po-verify.sh <commit>` on example): `check --ferrousli
--zinc`, `busybox`, the four boots, x86_64 under KVM, `test-shell` with the
ferrousli, the musl and the glibc busybox, `test-vfs` with the ferrousli and
the musl busybox, `test-net --arch all`, `test-display --arch all` and
`test-threads --arch all`. Owners say in one line what a person can test when
such a landing is on `main`. A release freeze (the customer's word) means:
each session lands what is gate-green, leaves the rest on its branch with a
row saying where it stands, deletes its target directory and reports; the
release notes land last, that head is verified and tagged, and the push to
`origin` is the customer's.

**Estimates are story points.** Since the evening of 2026-09-13 (customer) a
session says what is left in story points, never in hours or days: 1 is a
change whose pattern and tests already exist, 13 a new subsystem, Fibonacci
between. The product owner measures points into time afterwards, from the
landings, and never the other way round.

**Calling a stage done.** The exit criterion as written, on all three
architectures, and the marker moves in the same commit. A criterion met in a
weaker form is written down as such in the stage's section.

---

## Owners

Session names change on every restart, so this file no longer keeps a
roster. A row's owner cell names the session that took it; "open" means no
live session has it, and a session takes it by writing its name there in
its first landing. The product owner and the certification consultant name
themselves in their handovers (`AGENTS.md`). A session that has finished
everything it owns reports to the product owner, then asks the customer in
its own session whether it may be stopped. The last roster, of 2026-09-26
to 10-01, is in [backlog/OWNERS.md](backlog/OWNERS.md).

---

## Stage 13: where Ferrix differs from Linux

The differences from Linux that stage 13's landings knowingly left, each
with what closing it takes, are kept in
[backlog/stage13.md](backlog/stage13.md). A landing that leaves a difference
adds its row there, and the one that closes it removes it.

---

## Red on `main`

The customer's order of everything puts a working `main` first. These are
gates that fail on `main` itself, not flakes, and come before any row below.

| Item | Owner |
|---|---|
| `test-init --arch all` failed three ways plus a fourth on the fix's tip, root-caused 2026-10-05 by po5-red: reader-state race, typed-ahead answers lost to `TCSAFLUSH`, xtask serial reader ending on non-UTF-8. Fixed on `po6/red`; why the guest printed a non-UTF-8 byte there is open. [more](backlog/red.md#red-003) | po5-red |
| Stage 5 moving lock check panicked "tasks never moved between processors" under KVM: an idle processor stole another's only waiting task. Fixed on `po6/red` (`sched::steal_from`). Seen again 2026-10-06 in `test-ipc-equiv`, step 4's 27837766b. [more](backlog/red.md#red-004) | po5-red |
| **Open (2026-10-15, bs-a).** Stage 5's moving lock check: "a task of the moving lock check never finished" (FX-0502) in the ARMv7-A boot of `test-container --arch all` on `bs-a-coverage-profile`, load about 20; passed on rerun. Next: print the check's per-task state on give-up. [more](backlog/red.md#red-005) | open |
| **Open (2026-10-15, bs-a).** ARM coverage cannot run on nazuna: `libdrcov.so` is plugin API 5 but PATH's `qemu-system-aarch64`/`-arm` are 9.2.4 (API 4). Fix: build Arm emulators in the 10.2.1 tree or the plugin in a 9.2.4 tree. Ledger 1517 (3)(i). [more](backlog/red.md#red-007) | coordinator (the host's QEMU) |
| **Open (2026-10-15, bs-a).** x86-64 coverage boots exceed `test-boot`'s default 120 s under drcov on a loaded nazuna, so the evidence cannot be regenerated. Fix: `coverage` gives its gates its own timeout unless `--timeout` is given. Ledger 1517 (3)(ii). [more](backlog/red.md#red-008) | bs-a |
| **Open (2026-10-15, bs-a), F-75.** `tools/common/gen/carry-coverage.py` re-attaches anchors of functions moved to another file to same-text lines that stayed, faking reached lines. Fix: refuse to carry an anchor whose enclosing function changed, with a test. [more](backlog/red.md#red-009) | bs-a |
| H.SCHED.1 ("every runnable task runs") rests on an unstated rule: a task put on an idle processor's queue always kicks it (`enqueue`, `kick_after_wake`, `balance`, `pull`). Write the requirement and its check (consultant advisory, ledger 366). [more](backlog/red.md#red-010) | open |
| x86-64 stage 10 panics with FX-1012 (G3, `iommu/check.rs`) in about 1 of 27 boots, a console pump race that also made xtask's SIGTERM "QEMU terminating" failures. xtask half fixed on `po6/red`. Next: kernel half, G3 and R5 must not race the pump; consultant's OK first. [more](backlog/red.md#red-012) | open (N0g's, interrupt remapping) |
| **Fixed 2026-10-22 on `red013`, unlanded.** `test-compositor --arch x86_64 --accel kvm` failed at the display-driver restart since e06b456b9 (several opens of `card0`): `killgpu`'s `held` took a failed open of `card0` for the compositor holding it, and the open now succeeds. `held` now asks whether one of `hyprix`'s descriptors is the current `card0` (`-ef` through `/proc/<pid>/fd`); test-compositor and check PASSED [more](backlog/red.md#red-013) | red013 |
| **Fixed 2026-10-22 on `red-restart`, unlanded.** `test-restart --arch x86_64 --accel kvm` failed at the display driver since e06b456b9: `/bin/blank` now runs the DRM master check first, which reads a line on stdin, and as the test's background job that read stopped it (`SIGTTIN`) before it showed a buffer. The test now starts it with stdin from `/dev/null`; test-restart, test-compositor, test-boot and check PASSED, and test-restart again on a `main` holding the port_wait change (699ec9834 as e7977992d) [more](backlog/red.md#red-014) | red-restart |
| **Open (2026-10-25, sched-wake-latency).** `test-compositor --arch x86_64 --accel kvm` fails on develop itself: b5a59302d failed 3 of 3 gate runs (`logs/queue/f95r3-compositor-base.log`, `f95r6-comp-base1.log`, `f95r6-comp-base2.log`), at the hypridle step ("the guest never said [Running hyprctl dispatch dpms on, ...]") twice and once with "the demo was not drawn by gpu: llvmpipe". F-95's reaper branch fails the same ways, plus the hyprlock takeover once. Owner: the desktop (hypridle/tole). | — |

## Verification audit: claims without evidence

The verification auditor (`AGENTS.md`) checks what landings' messages claim
against the gate logs. Each row is a claim with no evidence behind it, or
evidence that went stale across a rebase. None of them shows `main` broken.
Close a row by running what is missing on `main`, keeping the log, and
naming its path here. The ledger, with every verdict and the log behind it,
is `~/.local/share/ferrix/verification-audit/ledger.md` on the gate host.
Audit 1 (2026-10-01) covered the 17 landings since 2026-09-30 that touch the
certified item's `core` or `item` ring.

| Item | Owner |
|---|---|
| **Done 2026-10-05 (po5-c1):** `render/**` in the load ring of `tools/common/data/certification-item.json` matched no file (c21abe11 meant to replace all seven interface cores' patterns with `interfaces/**`, and replaced six). It is gone, and `check-item-boundary.py` now refuses any ring member that matches no kernel file, with a self-test case; controls in `~/.local/share/ferrix/logs/po5-c1/ctl-ib-*.log` | done |
| **Ten item landings on 2026-09-30 moved `main` outside the landing lock** (1384e6e6, the discovery series, 318987db, bce1fc14, 8f7801de); the lock log does not show what landed on the Windows checkout. [more](backlog/audit.md#va-008) | the product owner |
| **37 code and xtask landings since 2026-09-30 name no gate in their message**, among them ferrousli's loader stack; those loader landings are the next audit. `cargo mutants` ran on four crates at 79826d1c: 23 missed of 491, untriaged; nine item crates not run. [more](backlog/audit.md#va-009) | open (the auditor's seat is empty) |

## The path to the goal, in order

The first goal, `rustc` on Ferrix (stage 16), was met on 2026-09-22. The
goal since is the Hyprland-shaped desktop (stages 17 to 19, decision of
2026-09-13) and then Steam (stage 22); what is being built toward it now is
the customer's order, and the owners table above lists it. The rows below are
what is left, ordered by what blocks what.

### P1 — required before a stage is called done

| Item | Owner | Stage |
|---|---|---|
| **Not yet production-ready (customer, 2026-10-24): the authentication prompt can be phished.** settingsd relays authd's PROMPT/RESPOND through the requesting app, so any program can show a password prompt and read the admin password. Fix: a trusted session prompt agent plus a per-area app allow-list; B4/B5 wait for it. [more](backlog/p1.md#p1-029) | queued (night 2026-10-24) | 15 |
| **Not yet production-ready: crash evidence can be lost** across kernel-ring overflow or a reboot. The identity/crash join lives in lode's memory; pstore import is not built. Fix: restart-join test, pstore import, tombstone step 2 (crash hold + stack capture). [more](backlog/p1.md#p1-030) | lode agent | 23 |
| **Not yet production-ready: bare metal and GPU acceleration are incomplete.** One bare-metal x86 box (N150) works; the WD SN720 PC hangs in ExitBootServices; GPU acceleration is Intel Xe-LP only (NVIDIA N1 open). Plan a hardware matrix and a GPU roadmap. [more](backlog/p1.md#p1-031) | unowned | 10, 19 |
| **Not yet production-ready: no system-wide security and reliability guarantees.** Per-item certification exists, but no whole-system threat model, security review, fuzzing or fault-injection campaign, or reliability targets. Plan one as a programme with the consultant. [more](backlog/p1.md#p1-032) | unowned (consultant) | all |
| **Open (2026-10-25, btrfs-crash).** On the N150 box (main-fc556f794) one failed disk write at 846 s (`device write at 0x1051d41000 failed`) aborted btrfs's transaction and left `/` read-only. The AHCI driver answered every failure at once with no retry; it now retries as libata does (three attempts, a COMRESET after a timeout, no retry of a medium error), and says each retry. The box's cause -- a timeout under load, a medium error, a link error or a DMA fault -- is not known: that boot's journal and pstore copy are lost. Owed: a box boot with the retries, and the driver's lines read. [more](backlog/p1.md#p1-033) | btrfs-crash | 10, 12 |
| **Open (2026-10-25, chardev-sigrace).** `sched_setaffinity` is accepted and not applied: no thread is pinned, and the mask reads back every processor (Wine's `SetThreadAffinityMask` included). Fix per the consultant's design OK IF D1-D12 (ledger 4446), ids `L.sched.96-101` reserved. [more](backlog/p1.md#p1-036) | chardev-sigrace | 7 |
| **Open (2026-10-15, mm-floor-livelock).** Endfield's install8 boot (16 GiB, 8 vCPUs) hung with every vCPU at 100% after memory hit the watermark. Likely cause fixed by mm-floor-livelock (`user::cache`'s hand, L.object.191); closes on a 16 GiB 8-vCPU run past the floor or a register dump. [more](backlog/p1.md#p1-001) | mm-floor-livelock | 23 |
| **Open (2026-10-15, mm-floor-livelock; consultant pc3-cert).** `oom::out_of_memory` with no job limit full answers Retry whenever a reclaim took any page, unbounded and with no machine-level kill. Bound it with a progress test, then a kill as Linux's global OOM killer does. [more](backlog/p1.md#p1-002) | owner | 13 |
| **Open (2026-10-16, pipe-epipe).** A TCP socket closed with unread received data keeps its full receive queue and the peer's writes block until timeout, where Linux answers with RST (`ECONNRESET`). Patches in `~/ferrix-logs/pipe-epipe/`. [more](backlog/p1.md#p1-004) | unowned | 9 |
| The init's later landings, L11 to L13 of `docs/INIT.md`. L1 to L10, L11, L12, L13a, L13b and L13c are all done or built; "Nothing of L13 is left". L13c is on branch `po10-l13c/l13c`, rebased on main. [more](backlog/p1.md#p1-005) | ferrix-15 | 15; 13 for L13 |
| btrfs on a full volume, consultant's leftovers (6a92d6c40, OK): (a) no test of a reload whose own log replay aborts; (b) `commit_reserve` and `Need::data` are estimates, derive them and test a full-size commit; (c) log replay and orphan cleanup at mount unmeasured. [more](backlog/p1.md#p1-008) | open | 12 |
| Clicks lost once, not reproduced (2026-10-02): six VNC clicks reached QEMU but not the page, on a maximized Chrome after a window closed and fullscreen toggled. Recorded click-to-flush latency: 11.6 ms in Chrome, 18 ms in Steam's login. [more](backlog/p1.md#p1-010) | open |
| Under `ferrix.devmgr=init` (L12) the kernel's disk checks of stages 10 to 12 do not run (AoU-13). Fix: the kernel starts check drivers of its own before pid 1, then hands the devices to `devmgr` (consultant's option (i)). [more](backlog/p1.md#p1-011) | unowned | 10, 12 |
| Authentication, phase 2 (`docs/AUTH.md` §7, 31 points): customer-raised 2026-10-03. Landed: home disk, `sessiond`, hyprlock grants, `login`, session end, `su`, `/dev/tty`. Left: procfs `PR_SET_DUMPABLE` (P2.1, 2), zeroed freed buffers (P2.2, 1), non-`--everything` images off root, logout policy. [more](backlog/p1.md#p1-013) | auth, with init and the compositor | 15 |
| zinc's `$?` read 0 after `su` failed, in `su …; echo $?` at the console (2026-10-03, `test-init`'s `su` stage, which therefore judges what `su` says and never its status). A script's `su -c x && y` would run `y` after a refusal. zinc lives in its own repository now (`components.toml`) | open | 15 |
| **Waits on the customer:** whether a user's processes end with their last session (`docs/AUTH.md` §6.4's residual, consultant's E6), i.e. systemd's `KillUserProcesses=yes`, which would also end `tmux` and `nohup` work. [more](backlog/p1.md#p1-016) | open | 15 |
| Authentication, phase 3 (`docs/AUTH.md` §7, about 32 points sized): the PAM shim in ferrousli (5), TOTP (3), ssh passwords through `authd` (4), privilege prompts (6), accounts with a generated `/etc/passwd` (4), a graphical greeter (8), `SO_PEERCRED` taken at `connect` (1), `mlock` as a no-op (1); FIDO2 and fingerprint unsized | open | after 15 |
| End-to-end user programs for what the `sigpaths` check proves at the kernel's decision: a `SIGSEGV` caught on the alternate stack, and a read interrupted by a handler and restarted under `SA_RESTART` (`SA_RESTART` and the driven signal paths have landed) | open | 7; `rustc` needs `SIGSEGV` on the alternate stack |
| Frame-counted checks that return before their task is dead. Left, 2 points: convert `exec::run` and every other such check site to `sched::wait_until_gone` (FX-0882, FX-0884). Seen again 2026-10-07 in stage 8's timerfd check on armv7a `--smp 2`. [more](backlog/p1.md#p1-020) | open | 6, 8 |
| Retire the 20 ms console polling. Receive by interrupt into a 4 KiB ring landed on all three architectures. Left: the console thread and readers waiting on `console::input::{waiters, has_input}` instead of sleeping (`fs/terminal.rs`). [more](backlog/p1.md#p1-021) | open | 7, 15 |
| `AF_UNIX` descriptor passing: `SCM_RIGHTS`, the in-flight cycle pass and `SO_PASSCRED`/`SCM_CREDENTIALS` landed. Left from landing 1's review: cap a receive's kernel buffer, drop a refused file outside the `FdTable` lock, `SO_SNDBUFFORCE`/`SO_RCVBUFFORCE`, Linux's error order. [more](backlog/p1.md#p1-022) | open | 7, 17 |
| POSIX.1-2024 interface sweep: from musl's implementation, list the Linux system calls and flags mandatory functions need; run each on all three architectures and file every `ENOSYS`, `EINVAL` or wrong result as rows. Sockets and threads excluded. [more](backlog/p1.md#p1-023) | open | 7, 8, 17 |
| Per-open windows onto the card VMO. **Done 2026-10-17 (E-02, branch `card0-buffer-vmo`):** each dumb buffer is a VMO of its own that only the open that made it maps; `test-display`'s `card-isolation` line requires it (`docs/DISPLAY.md` §2.1). Several opens and a DRM master that can be dropped, done 2026-10-18 (D1-D4, ledger 2327, `docs/DISPLAY.md` §2.5). Next: `sessiond` keeps a descriptor per card and drops one session's master before the next session gets the card, with E5's wording (`docs/AUTH.md` §6.5); comes back to the certification consultant before code. [more](backlog/p1.md#p1-025) | done | 17, 19 |
| `sessiond` drops the card's DRM master between sessions (2026-10-18, customer: backlog, not now; about 2-3 hours). `sessiond` keeps one descriptor per card it opened, `O_CLOEXEC`, passed only to that session's compositor, and calls `DROP_MASTER` on it before any other session's compositor gets the card; AUTH §6.4's E5 reworded as the certification consultant gave it (ledger 2290); closes `docs/DISPLAY.md` §2.5's one budget case (a dropped master holding more than half). The kernel half landed: e06b456b9 (several opens, a master that can be dropped, ledger 2327). Design to the consultant before code; cite the 3060 boot's log in the request. | open | 17 |
| Session switching on one seat (2026-10-18, customer: backlog; several days). Several sessions at once, a key combination or a greeter to switch, `sessiond` revoking the input devices (`EVIOCREVOKE`, there) and dropping the card's master (the row above) before handing the seat to the next session. `docs/AUTH.md` §6.5's "later"; each part a design review of its own. Needs the row above first. | open | 17 |
| Trusting a BAR firmware placed but did not enable. Started 2026-09-19: `fdt` reads host bridge `ranges` into `PciWindow`s. Left: windows on ACPI machines (loader and `BootInfo` bump), the vetting in `discovery/pci.rs`, decoding on at `IoMapping` creation. [more](backlog/p1.md#p1-026) | ferrix-d9 | 10 |
| **Stop-gap, to be replaced (2026-10-17, panic-reboot).** `ferrix.panic=reset` (`src/kernel/src/panic/reset.rs`, off by default): a panic resets the machine 10 s after its report so a warm reset keeps the pstore record for the next loader; and the pstore region goes 1 GiB above the lowest RAM where it fits, so the N150's inspection Linux keeps it with `nokaslr memmap=2M$0x40000000`. Replaced by the persistent panic/event log that survives a power cycle (design `~/.local/share/ferrix/n150-linux/panic-log-design.md`, stage 2: the loader appends the record to a raw event-log partition); closes when that lands and the reset becomes its reviewed panic action. Owes AB-BOOT §6.4's ids (H.FAIL.5, L.panic.1-4) and the customer's R1 decision before the docs/certification half | panic-reboot | 21 |
| **Event log, what PM4 left (2026-10-17, panic-eventlog).** `docs/POSTMORTEM.md` §4 logs panics only. Left: (a) the kernel's clean-shutdown mark: in RAM since crashlog's S1 (the pstore header's stop word, `docs/POSTMORTEM.md` §5, `L.console.46`), which answers for a warm reset only; the mark on disk that tells a clean power-off from a power cut is S2's, then kinds for a record without a panic and for no record, said only as "cause unknown"; (b) the TCO watchdog for hangs (stage 3 of the design, its own review; read-only box check first); (c) the partition on the N150 box (command list in `~/.local/share/ferrix/n150-linux/panic-log-design.md` §5, the customer's OK first); (d) identity across cold boots (consultant C1, ledger 2195): the pstore sequence restarts at 1, so two cold-boot panics with byte-identical pstore headers are Skipped, not Collapsed (a count one short); fix by a 64-bit id the loader writes into the pstore header, with (e) C2: a collapse written to the next slot, not in place, so a torn collapse keeps the run's record | panic-eventlog | 21 |
| CI's Miri step for `src/lib/fs/btrfs/format`: CI runs Miri over `src/lib/fs/common/block`'s request queue and not over btrfs. Under 15 minutes, with the whole-image tests ignored under Miri | open | 11 |
| Steam's bootstrapper (I5b, `docs/I386.md`): `test-steam-bootstrap` reaches "Unable to open X11 display" only with scout's requirements check set aside and an i386 semaphore stand-in; waits on branch `steam-sysv-sem` for System V semaphores. Display side needs GLX, i386 Mesa, RandR. [more](backlog/p1.md#p1-027) | open | 22 |
| **Open (2026-10-17, cr0-ne; F-79, consultant ledger 2098 (7)).** x86-64 control bits still taken from firmware: (a) `CR0.AM`, whether a program's `EFLAGS.AC` raises `#AC`/`SIGBUS` (Linux sets it, OVMF leaves it clear; the owner's decision); (b) `CR4.OSFXSR`/`OSXMMEXCPT`, which the switch assumes and without which a SIMD FP exception is `SIGILL`. Set and check each on every processor as `L.x86_64.178` does, or state it in SAFETY-MANUAL's hardware assumptions. | the owner (a); open (b) | 3 |
| **Owed (2026-10-17, stage10-pci; F-80, consultant ledger 2220 (6)).** The N150 boot with self-checks on, of a hash whose kernel source equals stage10-pci's landing tip. It must show stage 10 passing, and W7's `config` line must name its subject and passed-over count. From Linux's dump the first candidate found mastering is likely 00:1a.0; 00:04.0 was found not mastering there. If the line says "not run" or the boot stops at stage 10, take it back to the consultant before anything else rests on W7 for hardware. The reboot needs the PO's approval | n150 | 1 |
| **Owed (2026-10-17, blockring-n150; F-83, consultant ledger 2260 (6)).** The N150 boot with self-checks on, of a hash whose kernel source equals blockring-n150's landing tip. It must show stage 10 passing, and the block ring check's `ring` line must name its subject (`its rings made for <loc> (<ids>), after N`) with N of at least 1. It must also say whether that subject has a driver in the image and whether the driver came up (enable_dma turning bus mastering back on). If the line says "not checked" or the boot stops at stage 10, take it back to the consultant before F-83's hardware confirmation or any `L.quiesce` hardware claim rests on it. The reboot needs the PO's approval | n150 | 1 |
| **Open (2026-10-17, F-83, consultant ledger 2260 (1)).** W7's subject choice (`device::config_check::breach`) does not pass over a node holding the lent firmware framebuffer (`framebuffer_mode().is_some()`); only the display class is left out. The block ring check's `subject()` now does. Add the same filter to W7, with its boot rows re-run. Better still, give both checks one shared candidate walk, so the two policies cannot drift apart again | open | 1 |
| **Open (2026-10-17, F-81, consultant ledger 2220 Q2).** `ConfigState` records what the kernel wrote, not what reads back. An MSI mint on a function with INTx Disable hard-wired 0, or `enable_dma` on one with Bus Master hard-wired 0, gets refused at the next `verify_config`. `refuse` reports bus mastering off on a function with it fixed at 1. Fix on its own branch: record what reads back and say it aloud; `refuse` reads BME back. Needs a control for a hard-wired bit and a hardware row | n150 | 3 |
| **Open (2026-10-17, F-86, F-87, F-88; consultant ledger 2314 (6)).** `test-btrfs-pressure`'s memory.max boot (`FERRIX_PRESSURE_BOOTS=cgroup`, writers and readers in a cgroup at half a 768 MiB machine) fails on main and with btrfs-pressure alike: a busybox step ended by `SIGSEGV` at the limit with `oom_kill 0` (F-86); a program's own buffer refused `ENOMEM` while the job holds clean cache, a kernel charge with no reclaim first (F-87, name the refused call); with the fix a writer that never moves again (F-88, cause unknown). Each closes on that boot passing under `gate.sh` with a control; the cgroup form joins the rows only then (`docs/BTRFS-PRESSURE.md`, "At memory.max") | n150 | 13 |
| **Owed (2026-10-17, F-89; consultant ledger 2314 (6)).** The N150 box run: a Ferrix image of btrfs-pressure's landing hash (the box's Linux not needed) unpacks Endfield and the launcher, or a script, deletes a 17.5 GB tree, with the box answering throughout (ssh and ping). If it hangs, capture pstore and the per-processor run queues before the power cycle. Then tie the 2026-10-17 hang to the floor's `ENOSPC` or give it a fix of its own. Hardware use needs the PO's OK | the PO, on the box | 23 |
| **Owed (2026-10-18, intel-k4; consultant ledger 2352 K4).** The N150 box run of the screen hand-over, on an image of intel-k4's landing hash with checks on and `ferrix.efifb ferrix.intelgpu ferrix.fbcon`: the line `display  firmware framebuffer 800x600 lent to a driver (ferrix.efifb), write-combining` with no firmware framebuffer node in the device list; 00:02.0 not yielded and holding an aperture at the GOP framebuffer's address (`device_aperture`); `device_config_write` on 00:02.0 refused; the Intel driver given a copying card. QEMU has no Intel GPU, so these paths are checked on made-up nodes only (L.device.46-48). It runs with M6, at the earliest. Hardware use needs the PO's OK and BOX-HOLDER | the PO, on the box | 2 |
| **Owed (2026-10-18, intel-uapi).** (a) The Windows QEMU build (`tools/common/fetch/fetch-qemu-windows.sh`) carries patch 0006, pci-testdev's `x-vendor-id`, `x-device-id` and `x-class-code`, but nobody has built it: the Windows PC's F: was full. Until it is rebuilt, `test-intel-uapi` on Windows stops and says that its QEMU lacks the property. (b) The box checkpoint, `tools/common/n150/intel-checkpoint.sh`, on a box image with `ferrix.efifb ferrix.intelgpu` (the desktop's sshd needs a card, above) once M2's `intel` serves the render node through `serve.rs`. Before K2 it records the kernel's `Untranslated` refusal. Hardware use needs the PO's OK and BOX-HOLDER | (a) whoever next builds the Windows QEMU; (b) the PO, on the box | 1 |
| **Owed (2026-10-18, intel-k3; consultant ledger 2356, ruling 5).** The N150 box run of the three Intel GPU paths QEMU cannot reach (it has no Intel GPU), on an image of intel-k3's landing hash with checks on. (a) Without `ferrix.intelgpu`: the line `iommu    pci 0000:00:02.0, the Intel integrated GPU, is not driven: no ferrix.intelgpu`, and `device_config_read` on 00:02.0 answering BAD_STATE (the real node yielded, L.device.44). (b) With `ferrix.intelgpu ferrix.fbcon` and without `ferrix.efifb`: the line `iommu    DEGRADED: pci 0000:00:02.0, the Intel integrated GPU, driven under ferrix.intelgpu with its DMA untranslated ... pin budget N pages`, with N = min(1048576, (managed - watermark) / 4) (about 3.8 GiB), and `device_get_limit(PIN_PAGES)` on 00:02.0 answering the same N (DeviceNode::domain's Intel GPU arm, L.object.194). (c) Under the same command line, a render-only HELLO from the driver refused `Untranslated` while dmar0 does not translate (L.object.192). It runs with M1, at the earliest. Hardware use needs the PO's OK and BOX-HOLDER | the PO, on the box | 2 |
| **Owed (2026-10-19, igpu-hang).** The N150 box hard-hangs under `ferrix.intelgpu` (2026-10-18 22:47 on the box's own M1, 2026-10-19 17:04 on checkpoint 1 of `ef8325d2f`): no panic, no timed reset, the RAM log lost to the power cycle. Find the step with the driver's `ferrix.intelgpu.stop=N` and `ferrix.intelgpu.trace` (`docs/INTEL-GPU.md` §10, `steps.rs`), on a main image of the landing hash with sshdt, the timed-reset line and `ferrix.fbcon ferrix.fbcon.small`, and a photograph of the screen at every boot that does not reset within 90 s: (1) trace, no stop; (2) if (1) left no readable last line, `stop=1`, then halving. Then, for the kernel's owner (item, consultant first): (a) whether `device_quiesce` may turn bus mastering off on the function whose memory the firmware's lit plane scans out of while it is not lent (the driver now stays instead of exiting, which only avoids it); (b) a hardware watchdog (the PCH's TCO timer) under a kernel option for the box, since a hang that stops more than one processor stops the timed reset too; (c) `reboot(2)` `RESTART` and `RESTART2` (`syscall/system.rs`) write the pstore region back (`console::pstore::write_back`) before `arch::reset`, as `power::finish` does, so a restart a program asks for (the driver's `ferrix.intelgpu.reset`, usbdev's, busybox's `reboot`) keeps the log's last lines. Confirmed 2026-10-20: the two boots reset nothing because `/etc/reset-after.sh` is only on the drive, not in the tmpfs root, so init fell back to the desktop. Hardware use needs the PO's OK and BOX-HOLDER | the PO, on the box; (a) and (b) the kernel's owner | 2 |
| **Owed (2026-10-18, F-90; consultant ledger 2336 C2).** The N150 box run of the page cache under `tar`: with the cache filled to the floor by reading game files (about 16 GB `Cached`), a full-speed unpack of the 339 MB GE-Proton tar.xz over ssh onto `/data`, as in `~/.local/share/ferrix/n150-linux/FINDINGS.md`, "Page cache evicted whole under a write at the floor", on an image of pagecache-evict's landing hash. `Cached` must stay near full, and the box must answer throughout (a 0.5 s watcher, ping, a new ssh login), which shows whether the 16-27 s stall followed from the drop, as argued. Hardware use needs the PO's OK and BOX-HOLDER | the PO, on the box | 5 |
| **Open (2026-10-18, F-90's residual; consultant ledger 2336 (e), C1).** Release dentries kept for their files' cached pages when reclaim takes the pages, not only on a lookup's clock (two per miss): today a job's dentry and inode heap stays charged after reclaim emptied its files, and a workload whose lookups only hit never trims the set. That is memory reclaim cannot give back at `memory.max`. `user::cache` cannot call up into the VFS, so this wants a hook the VFS registers downward (say, a release the cache calls once an object it emptied has no pages), with its own review | unassigned | 8 |
| **Pressure runs for the gates (customer, 2026-10-18).** Run the gates' guest tests starved as well as at full speed: the guest on one processor with a CPU quota as low as 10 %, and under a memory limit, the way `test-btrfs-pressure`'s starved form already does (`--smp 2 --cpu-quota 40 --pin`). On Linux the same tests can run in a namespace with a cgroup's `cpu.max` and `memory.max`, so a failure that shows only under pressure can be compared with Linux's behaviour. The N150 box found the ENOSPC at the floor (F-86 to F-89) and the network drop only under load, which the full-speed rows never met. Choose which gates get a starved form, as manual checks first and as rows only where the time allows (gate time is priority one) | unassigned | 5 |
| **Gate slots without sshdt (2026-10-18).** `test-init --arch all` failed twice on 2026-10-17 (sudS4-r6 and a sud row) only because the gate slot it ran on had no `sshdt` built ("cargo xtask build-apps --app sshdt builds it"); it passed on a slot that had one. Make the row build what it needs (or `gate.sh` prepare each slot with `sshdt`), so a row's result depends on the change and not on the slot | unassigned | 2 |
| **Open (2026-10-17, fx1006; F-82, consultant ledger 2247).** Detection half of F-82: nothing short of a boot finds an image whose `lib/drivers` holds more than `ferrix_devmgr_proto::MAX_DRIVERS` (32), and `test-nvrm`, whose image went over 16 at ee2d5bf0f and left `main` red for two days, runs in no landing's row set. Preferred, no gate time: `check` builds the driver set of every image `xtask` makes (each `test-*`'s, `test-nvrm`'s and `test-restart --update`'s included) and holds each to `MAX_DRIVERS` without booting; otherwise the PO puts `test-nvrm` in a regular row set. Closes F-82 | n150 | 10 |
| The installer's live medium, I6 of `docs/INSTALLER.md` §9. I6a (the `.img` on GPT, `cargo xtask live`) done 2026-10-07, 5 points. Left: I6b, the hybrid `.iso` (`xtask/src/iso.rs`), and `test-install` on AArch64. [more](backlog/p1.md#p1-028) | po10-install | — |
| **Open (2026-10-19, install-guide).** `cargo xtask test-install` fails in the live medium's boot (`main` 8cb1dc24a plus the release-image commit, `--release`): q35's AHCI holds only QEMU's CD-ROM there, `ahci` exits 5 ("did not come up: NoDrive"), and xtask stops on "4 started, 1 failed". Not in `check`. Logs: nazuna `~/.local/share/ferrix/logs/install-guide/install/` | unassigned | 2 |

### P1 flakes — seen on a gate, each with its log

A flake that keeps firing is cheaper to fix than to rerun. Each row keeps its
log path and commit; a new sighting is added to its row the day it is seen.

| Item | Owner | Stage |
|---|---|---|
| **Open (2026-10-25, btrfs-crash).** Stage 13's cgroup freeze check stopped an x86-64 TCG boot: "cgroup.events did not poll POLLPRI when its cgroup froze" (`bo-x86tcg`, c88b9a1ff, a change the check does not reach). [more](backlog/flakes.md#fl-085) | unowned | 13 |
| **Open (2026-10-23, lode).** `check` in batch 20261023T203241Z (ccb0113b0) stopped 24 s in: xtask's `git fetch --quiet --prune origin` in the shared component mirror `~/.local/share/ferrix/components/tole.git` failed with `fetching ref refs/heads/main failed: incorrect old value provided` -- two gates fetching the same mirror at once, each moving `main` under the other. Passed on the batch's rerun on the same tip. Log: `~/.local/share/ferrix/logs/queue/batch-20261023T203241Z-1.log`. A lock around the mirror's fetch (or one fetch a run, before the slots start) would end it. | unowned | tools |
| **Open (2026-10-24, settings-prompt).** A local `test-settings --arch x86_64` boot on `feature/settings-b36` c7f467ab1 (develop ceb8d678c plus ring-3 settingsd changes) panicked in the kernel's stage 8 self-check, `FERRIX-PANIC stage 8 /dev and /proc self-check failed: a directory /proc listed did not open` (FX-0830, `src/kernel/src/main.rs:653`), under kvm; the same tree passed on the rerun. Serial log kept at `~/.local/share/ferrix/logs/settings-flakes/stage8-proc-dir-serial.log`. Not reproduced on purpose; a process that exits between `/proc`'s listing and the open would fit. | unowned | 8 |
| **Open (2026-10-23, sealed-job; seen 2026-10-21 too).** `test-container --arch all`'s AArch64 boot ended `container: FAILED memory.stat could not be read (Out of memory (os error 12))` on `mk7-container-3` (2f421e3ac, 2026-10-21), `sealed-container` (c664460af) and `sealed2-container` (0e624debf), each with the x86-64 half passing; `sealed3-container-aarch64` (1fb935294) PASSED on rerun. On main d1e9e9c79 `main-container-aarch64` ended differently (the shell's exit not printed within 120 s). Logs in `~/.local/share/ferrix/logs/queue/` under those tags. A read of `memory.stat` refused for memory under the container's own `memory.max`, not yet reproduced on purpose. Seen three times more on `feature/kernel-split-s3`, 5841bfa44 (`s3r-container`, `s3r-container2`) and 3903f1822 (`s3s-container`), 2026-10-23, the same AArch64 line, the x86-64 half passing; logs kept in `~/.local/share/ferrix/logs/s3r-flakes/`. | unowned | 13 |
| **Done 2026-10-16 (fl-x), same fix as the fl-010 row:** fuzzel's host test `exec::tests::an_application_in_game_mode_is_started_with_the_mark` flaked with the step cache on; fixed in the apps component (commit `30fa8a2ffe43`: child writes `out.part` and renames it); this tree's pin follows. [more](backlog/flakes.md#fl-006) | flake owner | tools |
| **Open (2026-10-15, mm-pcache).** `cargo xtask check` in gate slot 1 hit a rustc/clippy internal compiler error compiling `xtask` (`dep_graph/graph.rs:1269`); same tree passed in slot 2. Suspect a poisoned incremental cache; clear that slot's `incremental/` or use `CARGO_INCREMENTAL=0`. [more](backlog/flakes.md#fl-007) | flake owner | tools |
| **Open (2026-10-15, rig-fu2).** `test-frames` failed at host load about 40: one injected stall counted as 15 missed refreshes, 3 expected; rerun passed. fl-x found the guest renderer starved, no harness race; nothing changed. [more](backlog/flakes.md#fl-008) | unassigned | 24 |
| **Open (2026-10-15, pm-ab-boot).** `test-ab-boot` failed at host load about 40 on stage 4's contention self-check (FX-0407), which can misread a host that starves vCPUs. fl-x: a guest-side check, not a harness race; nothing changed. [more](backlog/flakes.md#fl-009) | unassigned | 4 |
| **Done 2026-10-16 (fl-x):** the apps component's fuzzel test `an_application_in_game_mode_is_started_with_the_mark` read a half-written file; fixed in the apps component (commit `30fa8a2ffe43`, child renames a finished `out.part`); this tree's pin follows the push. [more](backlog/flakes.md#fl-010) | ferrix-d5 (the desktop clients, fuzzel), as the owners table has it | 19 |
| **Done 2026-10-16 (po-fl-cg, branch `fl-cg`):** stage 13 cgroupfs freeze check was charged slice time after reading `cpu.stat`; it now reads usage only once every task is blocked and off its queue. Control still FIRES. Not reproduced on demand; watch. [more](backlog/flakes.md#fl-011) | flake owner | 13 |
| **Open (2026-10-15, endfield-esync).** `test-wine` notepad typing step: the double click (two QMP clicks 80 ms apart) was seen by Wine as two single clicks under load, so the clipboard lacked "hello". Nothing changed; compare Wine's double-click interval with yserver's timestamps. [more](backlog/flakes.md#fl-012) | flake owner | 23 |
| **Fixed by the commit that adds this row (branch `dev-on-ferrix`, 2026-10-11).** A guest TCP connection through xtask's gateway stalled when the SYN-ACK's ack took over the 20 ms retransmit timer (`resend_syn_ack` wound `snd_nxt` back). Two host tests hold the fix. [more](backlog/flakes.md#fl-013) | dev-on-ferrix | net |
| **Open (2026-10-11, dev-on-ferrix, D9).** QEMU 10.2.1 running on Ferrix does not end on TCG guest power-off under `-no-reboot` or on SIGTERM until SIGKILL, three threads asleep; look first at epoll wakes with `signalfd`/`eventfd`. [more](backlog/flakes.md#fl-015) | unowned | 7 |
| **Open (2026-10-08, po10-chrome-b).** `test-restart --boot blk` panicked "memory allocation of 4096 bytes failed" in `Source::fill_range` during the initramfs unpack onto btrfs root; seen again 2026-10-15. Find why it fails and make the zeroed buffer fallible (F-56). [more](backlog/flakes.md#fl-017) | flake owner | 11 |
| **Open (2026-10-15, mm-floor-livelock).** FX-0008 "memory allocation of 262144 bytes failed" in `syscall::exec::populate` at memory floor, free frames fragmented by the page cache; needs a fallible allocation (or no contiguity) so exec fails with ENOMEM. [more](backlog/flakes.md#fl-018) | flake owner | 7 |
| **Addressed by F-78 (2026-10-16, po-fl-cg, branch `fl-cg`); watch.** Throttled task woke every millisecond and was charged to its job, so stage 13's `cpu.max` check (FX-1301) measured too much; it now sleeps to its period's end. Not re-measured under load. [more](backlog/flakes.md#fl-019) | flake owner | 13 |
| **Open (2026-10-16, po-fl-cg; F-78's residual, consultant L2).** H.QUOTA.10's bound (650 thousandths for `cpu.max 20000 100000`) passes the old charged throttle; find a bound that fails it yet holds under load, with a control. Moved tasks and raised `cpu.max` wait out old periods. [more](backlog/flakes.md#fl-020) | open | 13 |
| **Open (2026-10-10, softgl-gap).** `run-compositor --everything`'s desktop has no host clipboard: vdagent runs as uid 1000 but `vport` serves only uid 0. Needs a decision on who may hold the port, then `test-clipboard` on a `--session` desktop. [more](backlog/flakes.md#fl-023) | open | 19 |
| **Open (2026-10-08, po10-l13c).** Gate pool slots lose sshdt's package when a slot is shed; `gate.sh` should seed it again on making a slot, or rows that need it should fetch it. Owner: product owner. [more](backlog/flakes.md#fl-024) | product owner | tools |
| **Open (2026-10-14, m0-ferrocene; consultant ledger line 1034).** `gate.sh control` has no mode for check-only steps that boot no guest, so such controls read `DID NOT FIRE`; add a no-guest mode that reads `control: FIRED` on its own. [more](backlog/flakes.md#fl-025) | fleet / product owner | tools |
| **Open (2026-10-08, os-07).** `nvrm`'s `nv_call` makes native calls through an x86-64 `syscall` without declaring vector-register clobbers, outside OPAQUE-KERNEL step 3a's contract; declare them as `ferrix_rt` does or show the compiler cannot hold one there. [more](backlog/flakes.md#fl-026) | os-07 | 21 |
| ARMv7-A at `--smp 2` boots panicked under TCG load (six sightings from 2026-10-08): stage 4's "no two processors' increments overlapped" and stage 9's "sync: no Sync write ... moved a reader" (FX-0901). Both need two vCPUs running at once. Not F-69. [more](backlog/flakes.md#fl-027) | flake owner | 4, 9 |
| `cargo xtask check` failed once (2026-10-08): `check-line-endings.py` hit `FileNotFoundError` on a temporary generated file in `compositor/protocol/src/generated/`. The scan should skip vanishing files, or the generator write outside the tree. Owner: CI steward. [more](backlog/flakes.md#fl-028) | CI steward | tools |
| Stage 3's tick-rate check panicked in 6 of 37 x86-64 TCG boots under load (2026-10-07): "timer and counter disagree about how long a second is". May measure host starvation, not the timer; check what bounds the rate and whether coalesced ticks count. [more](backlog/flakes.md#fl-029) | unowned | 3 |
| **Open (2026-10-09, os07-hold).** DK1 run of F-69's cheaper hold (consultant ledger 624, E4): its ARMv7-A gain is argued only from object code. Run `bench-ipc` on the board on main before and after landing, turn about, and record both. [more](backlog/flakes.md#fl-030) | os-07 | 5 |
| **Open (2026-10-15, h6-wc).** N150 run of H6's write-combining window (ledger 1303, condition 9): the full-frame copy figure must be under 16.7 ms, and a panic forced under `ferrix.efifb` must draw visibly. Netboot; hardware rules apply. Owner: h6-wc, then the PO. [more](backlog/flakes.md#fl-031) | h6-wc, then the PO | 10 |
| **Done (2026-10-16, po-n150k; ledger 1822, 1852, 1884): closed by the n150k landing; box evidence stays open in fl-033.** N150 kernel bring-up (BAR widening, memory decoding, console polling, clock, x2APIC) rebased as branch `n150k`; conditions (1)-(5) and L2 answered there. [more](backlog/flakes.md#fl-032) | the PO | 10 |
| **Open (2026-10-16, po-ahci2; ledger 1822 (8)).** N150 hardware-only evidence for fl-032's bring-up: boot with checks ON, decoding on 9 endpoints, 2 KiB ABAR widened, clock path and rate, APIC mode, MSI never arriving, COM1 transmit interrupt (since `serial-tx-fallback`, ledger 1980 (6): its WARNING "given up after" under 100 ms, against the 2.004 s gap before), CPUID 0x15/0x16, HPET fallback. [more](backlog/flakes.md#fl-033) | the PO, on the box | 10 |
| **Open (2026-10-17, efifb-size; ledger 1968 (7)).** N150 boot after systemd-boot at 800x600, whose GOP size ends inside a page, must show `display  firmware framebuffer 800x600 lent to a driver (ferrix.efifb)` and efifb's desktop; QEMU's GOP sizes are whole pages. Main session runs it after landing. [more](backlog/flakes.md#fl-068) | the PO, on the box | 10 |
| **Watch (2026-10-17, serial-tx-fallback; ledger 1980 (6)).** A `output   WARNING: the transmit interrupt sent nothing for 50 ms` line on any QEMU row may be host starvation: keep the log and load, and take it to the consultant before anyone changes the bound. [more](backlog/flakes.md#fl-069) | whoever sees it, then the consultant | 2 |
| **Open (2026-10-17, fbcon-wc-early; ledger 1994 (6), (7)).** N150 boot with `ferrix.fbcon`: the line times now the framebuffer window is write-combining from its first map (were 0.1 to 0.26 s a line), the `fbwindow` line's flush cost (under 50 ms), and photographs of efifb's first frame and a forced panic with no console pixel. [more](backlog/flakes.md#fl-070) | the PO, on the box | 10 |
| **Open (2026-10-17, hwp-turbo; ledger 2067 (7), 2281).** N150 evidence for L.x86_64.177 is in (`~/.local/share/ferrix/n150-linux/evidence/hwp-a06035a72-*`: HWP on, levels 1..36, EPP 0x80; 4 of 4 asked; the secondaries' control fired; md5 1500 MB in 7 s against 8 s and 5 s). Still owed: argued coverage entries ("hardware QEMU does not model") for `enable_hwp`'s HWP-on arms, `enable_hwp_and_say`'s three HWP lines and `check_hwp_processors`' HWP branch once a coverage measurement lists them in the residual; and a checks-on N150 boot past stage 10 once F-83's fix (blockring-n150) is in, showing the cpu and hwp lines. Since hwp-epp (2026-10-18, L.x86_64.177 restated, L.x86_64.182 new; ledger 2396 (9)) also owed: an N150 boot of the landing image whose cpu line says `EPP 0x00` and whose hwp line says 4 of 4 asked and read back. | the PO, on the box | 2 |
| **Open (2026-10-17, rfds-bhi; F-84, F-85, consultant ledger 2294 (9), 2305 (4)).** N150 hardware for RFDS and BHI, in this order. (1) The box's Linux capture into `~/.local/share/ferrix/n150-linux/box-2026-10-17/`: `grep . /sys/devices/system/cpu/vulnerabilities/*`, `rdmsr -a 0x10a` (ARCH_CAPABILITIES), `cpuid` leaf 7 sub-leaf 2 EDX, and dmesg's spectre, RFDS and BHI lines, the BHI line naming its mitigation. (2) A checks-on bare-metal boot of the landing image: the defences line with `VERW on exit` and `BHB loop on entry`, the exposure lines with `RFDS covered (VERW on exit, RFDS_CLEAR)` and `BHI covered (BHB loop on entry)` and nothing NOT covered on the second line, the defences read back on 4 processors, the pstore log tied to the hash. (3) The N150 cost, syscalls/s and stage 7's cost lines before and after, into SPECULATION.md §8. A stage 10 stop or a "NOT covered" goes back to the consultant; the boot closes F-84 and F-85 there | the PO, on the box | 3 |
| **Open (2026-10-17, vtd-n150; ledger 2122 (1), (4), (15), 2635 (L5)).** VT-d on the N150 landed with its box boot (`~/.local/share/ferrix/n150-linux/evidence/vtd-61327c9f1-n150-dmesg.txt`, provenance beside it): dmar0 remapping only under the RMRR, dmar1 translating four-level, 27 functions behind one with 1 untranslated, USB, the NIC's lease and sshd, 0 faults in the unchecked boot. Still owed: the PMR clearing arm on hardware whose firmware leaves EPM set (the box's units read `PMR off as found`, so it stays argued); the fault-record scan past record 0 (both units have one record, NFR 0; argued, host test `records_are_read_from_fri_round` and nc5); and the first box boot that takes / on the disk with VT-d on. [more](backlog/flakes.md#fl-072) | the PO, on the box | 10 |
| **Open (2026-10-20, vtd-n150; ledger 2635 B1).** The N150 desktop image `cargo xtask n150-install` writes keeps / in memory on the box: "vda2 could not take the system: the initramfs did not unpack onto it" (`src/kernel/src/fs/root_disk.rs` `install()`, whose `initramfs::unpack` error is dropped by `map_err(|_| ...)`), so `/data` is missing ("no gryphlink on /data"). The same line with `ferrix.vtd=off` (`vtd-61327c9f1-vtdoff-n150-dmesg.txt`, 61327c9f1, 2026-10-20 18:27 CEST), so not VT-d's. **Found and fixed (2026-10-21, n150-image; ledger 2729):** the drive holds `/etc/fonts` and `/usr/share/fonts` as directories with files where the archive has links into `/data`, so the rmdir failed with ENOTEMPTY (39); the unpack now moves such a directory aside to `<name>.ferrix-old` and says so, and a stopped unpack says its entry and errno (`docs/N150.md` §12). Owed: a box boot of the landing hash whose console shows the set-aside lines, / taken from vda2 and `/data` there, with VT-d on. | n150 | 12 |
| **Open (2026-10-21, n150-image; seen on the box 2026-10-19/20).** From ssh, `/bin/reboot` (`compositor-reboot`: `sync()`, then `reboot(RESTART)`) and a backgrounded one did not restart the box; `busybox reboot -f` (the same two calls) sometimes did. Not reproduced, and on that image `/` was in memory with the drive at `/sysroot`. Read in the code, not shown: `sys_reboot` commits `/`, `/data` and `/home`, then `arch::reset` drains the console, whose ring goes out through a UART the box reports polled since its transmit interrupt sent nothing (`write_byte` waits up to 100,000 status reads a byte), then the FADT reset register, the 8042 and a triple fault. Owed: on the box, with / on the drive, `/bin/reboot` from ssh, and the next boot's pstore read for `reboot: Restarting system` and `power ... could not be committed` | n150 | 3 |
| **Owed (2026-10-18, intel-k2; consultant ledger 2388 (6)).** The N150 box run of K2, dmar0's RMRR identity domain for 00:02.0, on an image of intel-k2's landing hash under `ferrix.checks=skip` (the pstore build). It must show: which region of Ferrix's own loader memory map (not Linux's e820) holds 0x7C000000-0x803FFFFF; dmar0's line `translating (four-level)` and `vt-d unit 0xfed90000: RMRR 0x7c000000-0x803fffff identity-mapped for pci 0000:00:02.0, 17408 pages, held for the boot and never detached`, not `translation left off`; the screen still lit after TE (a photograph); and no 00:02.0 fault in the unchecked-boot fault line (`L.iommu.72`). It is the open verification of identity-before-TE on a real RMRR (`L.iommu.73`, `L.iommu.75`): QEMU's DMAR has none, and stage 10's check (`check_rmrr_identity`) shows the kernel's tables and rules, not device DMA into an RMRR. A dark screen or a 00:02.0 fault goes back to the consultant before anything rests on K2. Hardware use needs the PO's OK and BOX-HOLDER | the PO, on the box | 2 |
| **Owed (2026-10-23, gpu-opaque; consultant ledger 3156 (8)).** The N150 box run of the Intel driver's opaque crash mode (`docs/INTEL-GPU.md` §12.8): Endfield running, `kill -9` of `intel-gt`; the game goes on, hyprix says nothing, the keeper says the stall's length; then M2's hanging batch under the game, only that context lost. Waits for §12.11's step 2 (the product worker): until then no product image carries `intel-gt` and `intel` is one process | the customer, on the box | 1 |
| **Owed (2026-10-22, intel-restart-box; consultant ledger 3148 (4)).** The N150 box run of the Intel driver's restart (`docs/INTEL-GPU.md` §11.3, branch `gpu-restart`): under `ferrix.intelgpu` with K2 taken (dmar0 translating) and Endfield running, `kill -9` of `intel` must print DIED, `was started again and published (restart 1)`, `intel: forcewake 0x… was held by a previous driver; released` before the `done` lines (never `forcewake failed` nor `could not be released`), `done` for each inherited engine, `render node published` and `card0 published` again, and hyprix's `the card is back`; no DMAR fault after the restart; the stall measured; `/proc/loadavg` and the busiest processes before the kill and 30 s after it (§11.1's sluggish box). Until it runs, the per-engine reset before HELLO is shown on the host model and `intel-fake` only. The first run (2026-10-23, headless, no game) stopped the restarted driver at `Forcewake`, the dead driver's render and GT forcewake still acknowledged; branch `intel-fw-reset` (consultant ledger 3186) releases it first, as i915's `forcewake_early_sanitize` does, shown on the host model only, since neither QEMU stand-in has the forcewake registers. The customer runs it | the customer, on the box | 1 |
| **Open (2026-10-08, os07-lazyvfp).** DK1 run of stage 9's `lazy` line (OPAQUE-KERNEL §9.15, ledger 606, L9): boot the lazy branch's image on the board at two processors and keep the serial log before L.armv7a.24 is claimed for hardware. [more](backlog/flakes.md#fl-034) | os-07 | 9 |
| **Addressed by F-78 (2026-10-16, po-fl-cg, branch `fl-cg`); watch.** Throttled task's per-millisecond looks were charged to its job, so stage 13's `cpu.max` check failed on ARMv7-A and AArch64 TCG; fixed by sleeping to the period's end. Not re-measured under load. [more](backlog/flakes.md#fl-035) | os-07 | 13 |
| **Addressed by F-78 (2026-10-16, po-fl-cg, branch `fl-cg`); watch.** Same throttle-charge cause as fl-035; stage 13's cgroupfs check (FX-1301) also failed on an x86-64 KVM boot at load 40 ("held to 911 thousandths"). Not re-measured under load. [more](backlog/flakes.md#fl-036) | open | 13 |
| **Fixed on branch `po9/fx0902` (2026-10-07).** FX-0902 allocation-sweep check saw 77-78 frames return from outside its window because `end_thread` left the exiting task counted nowhere; `end_thread` now counts it until `sched::exit_leaving` and the reaper wait waits for that. [more](backlog/flakes.md#fl-037) | po9-fx0902 | 9 |
| **Fixed by the commit that adds this row (branch `po10/arm-wait`, 2026-10-07; consultant OK IF C1-C3, ledger 433); row stays until 20 x86-64 TCG boots on main pass under load.** One-shot timer waits lost a wake between look and `hlt`; they now wait via `arch::wait_for_work`. [more](backlog/flakes.md#fl-038) | po10-win | 3 |
| `test-init`'s refusal checks can pass for a program that never ran (consultant advisories A1, A2, ledger 368): wrong-password `su`/`log_in` accept any failure text. Next: require their own answer and end marker; A2: `ferrix-auth-client`'s flush-before-prompt order lacks a negative control. [more](backlog/flakes.md#fl-039) | unowned | 15 |
| **Open, findings (fl-k, 2026-10-16):** stage 9's wake row: the blocker spins with its run-queue lock held up to 50 ms, three tries, waiting for the poster; a host stalling the poster that long fails it. The bound stays until the poster's path is shown independent of the blocker's processor. [more](backlog/flakes.md#fl-040) | unowned | 9 |
| The System V semaphore self-check stopped AArch64 (2026-09-30) and ARMv7-A (2026-10-12) boots: "sem: a waiter returned without waiting". Under load the waiter may find the value already there; check what it takes as proof of a wait. [more](backlog/flakes.md#fl-042) | open | 7 |
| FX-0309 audit self-check: "a DMA fault the IOMMU reported left no record" once in an x86-64 TCG `test-compositor` boot at load 33. Under load the fault may be reported after the check looks; check whether it waits or reads once. [more](backlog/flakes.md#fl-043) | open | 10 |
| `test-boot --arch armv7a --smp 2` failed once (2026-09-26): `0 entropy bytes read by DMA` from virtio-rng on init-l6's gate (no kernel change); rerun passed. Look at whether the virtio-rng completion can be missed or come after the line prints. [more](backlog/flakes.md#fl-044) | unowned | 10 |
| Stage 7's signal hand-off check flaked repeatedly (from 2026-09-24): "a signal sent to a process of two threads was given to no thread" (`syscall/check.rs`). Not reproduced in about 33 boots. Next: print each thread's mask and `is_gone` as `notify_signal` sees them. [more](backlog/flakes.md#fl-045) | open | 7 |
| Stage 10's entropy request is skipped under load on every architecture: "MSI-X interrupt never arrived" after a 2 s deadline (fifteen sightings, also behind FX-0309). Not a slow device but an untaken interrupt. Next: catch one with po10-flakes' `entdiag` line. [more](backlog/flakes.md#fl-046) | unowned | 10 |
| `test-compositor --arch x86_64` failed on its frame budget (slowest frame 7.9 s against 5 s) at host load 44-52. Host run-queue wait explains little (branch `frame-budget`); measure frames in guest work or drop the bound under load. [more](backlog/flakes.md#fl-047) | open | 17 |
| FX-1201, seen once (2026-09-26): stage 12's btrfs write check failed "a file written could not be read back" in an x86-64 TCG `test-compositor` boot at load 30-80. Check what the block ring answers a timed-out read and whether btrfs turns it into EIO. [more](backlog/flakes.md#fl-048) | unowned | 12 |
| `test-compositor` frame-budget failures (x86-64 TCG, 5 s; ARMv7-A, 10 s) in software drawing under host load 11-35, many sightings since 2026-09-26, every functional check passing. A budget the host's load decides, not a regression. [more](backlog/flakes.md#fl-049) | open | 15 |
| `test-jobs` flaked once (2026-09-24): "the session at the console did not answer"; the typed line echoed only when the next was typed, as if a console read woke late by one line. Cgroup landing G4's gate; rerun passed. [more](backlog/flakes.md#fl-050) | open | 15 |
| ferrousli's busybox on ARMv7-A fails `test-net --arch armv7a --init ferrousli`: 10 of 13 programs fail from `udhcpc: poll: Invalid argument`, so no address; AArch64 passes. Start at ARMv7-A `poll`/`ppoll` in ferrousli (time64 variant?). [more](backlog/flakes.md#fl-051) | open | 15 |
| **Fixed on branch `po10-selfhost/fx0001` (2026-10-07), stage 20 S-1.** FX-0001, "processor N never flushed its TLB for a shootdown", gave up on live but starved processors; fix adds a late bound (`L.smp.33`) and a 10 s stuck bound. Sighted again since. 2 points. [more](backlog/flakes.md#fl-052) | po10-selfhost | 20 |
| rustc on Ferrix panicked once reading its environment (`env.rs:876`: a variable's bytes were not UTF-8) in an unstarved `test-selfhost --accel kvm --smp 8` on 2026-10-07. Start at execve's copy of envp and the stack it builds. [more](backlog/flakes.md#fl-053) | unowned | 20 |
| Boot self-checks with wall-clock waits stop when the host starves one vCPU (FX-1011, FX-0882, a 120 s no-boot), seen 2026-10-07 beside FX-0001. Next: give each wait a count in the guest's units (`smp::patience`) or a late and a stuck bound. [more](backlog/flakes.md#fl-054) | unowned | 8, 10 |
| An x86-64 TCG boot hung before stage 3 with no panic (`input` line, then nothing for 120 s), seen 2026-09-26 to 09-28 in three gates (`test-shell`, `test-auth`); reruns passed. The hang is in stage 3's checks or just before. [more](backlog/flakes.md#fl-058) | open | 3 |
| `test-compositor` under TCG at load 16-40 took input seconds late (2026-09-27/28): `--boot lock` saw a bind in the locked session, `--boot animation` kept one picture or a frame over 11 s. Judge the lock bind by `started /bin/lswt`; time slides from the first changed picture. [more](backlog/flakes.md#fl-059) | open | 18 |
| AArch64's PCI self-check line flips between boots of one kernel: "seen **after**" vs "seen **before**" the fault, a race in the out-of-domain probe (`discovery/pci/virtio.rs`). Print the order only when stable, or compare it separately. [more](backlog/flakes.md#fl-060) | open | 10 |
| Host tests of the compositor (waybar `against_hyprix`, the toolkit's) fail `cargo xtask check` under load, passing on rerun, many sightings 2026-09-30 to 10-12, even at low load; hyprix `two_clients` made event-driven 2026-10-23 (0 of 100 starved runs failed). Diagnosing needs the step cache off and `FERRIX_SIDE_JOBS=1` (ledger 1461, 1684). [more](backlog/flakes.md#fl-061) Seen 2026-10-23 on `s3r-check` (5841bfa44): `a_floating_workspace_floats_and_cascades_new_windows` and `a_window_that_is_not_shared_is_a_black_box_in_a_screenshot`, passing on rerun (`s3r-check2`). | open | 18 |
| A wake goes missing on AArch64 with a GICv3 (`FERRIX_ARM_MACHINE=gic-version=3`): in 8 of 9 TCG boots a relay's next party stayed queued until a 30 s deadline. Start at a kick's SGI: `KICK_PENDING` left up, and the `isb` after `ICC_SGI1R_EL1`. [more](backlog/flakes.md#fl-062) | open | 5 |
| An AArch64 `test-boot` of unmodified main eeaa35cd timed out under load (41 to 51) on nazuna on 2026-10-01, while `os-35/ipc-ring` was measuring its baseline; not rerun. Kept: `~/.local/share/ferrix/logs/os35-ring/base-eeaa35cd-aarch64-timeout-serial.log`. Read the serial log's last lines for the stage it stopped in | open | 10 |
| `test-net --arch x86_64 --init <musl busybox>` stopped twice (2026-09-30) with a program that never exited (`git`, `wget -q`) while the host disk was at 98-100%; quiet runs passed. Look at what a networking program waits on when the host is slow. [more](backlog/flakes.md#fl-063) | open | 13 |
| `test-net --arch x86_64` hung once (2026-10-01) at its first `wget` for 120 s on net-throughput 931e553; gateway saw one SYN-ACK retransmit, then nothing. Not root-caused: a frame or wake lost between driver and net core. Unowned. [more](backlog/flakes.md#fl-064) | unowned | 4 |
| `test-auth --arch all` failed once on x86-64 (2026-10-03): init's journal line shared a console line with `passwd`'s output, so the check read neither. Match within a line rather than whole lines, or read the journal. [more](backlog/flakes.md#fl-067) | open | 15 |
| Stage 10's chardev check stopped an armv7a `--smp 2` TCG boot once (2026-10-19, load 15 to 25) with "the driver's reply was refused" (`interfaces/chardev/check.rs`); split from fl-071, whose refused admit is fixed. [more](backlog/flakes.md#fl-073) | open | 10 |
| The quota self-check stopped an x86-64 TCG boot once (2026-10-23, linux-abi-off 6064977d5) with "a job with many spinning tasks took more than its share from another job" (`object/quota_check.rs`); passed on rerun. Seen before on armv7a TCG (bsa-lo-st-a32-12, 2026-10-15). [more](backlog/flakes.md#fl-074) | open | 13 |
| `test-compositor`'s Settings step missed both its Returns once at host load 35 (2026-10-23, hyprix-gnome cad58e30c) after fl-001's fix; the next tip passed. [more](backlog/flakes.md#fl-075) | open | 18 |
| nvos's host test `timer::tests::a_timer_fires_once_and_a_cancelled_one_never` failed once in `check` at host load 42 (2026-10-24, minimized-restore ed553b158, a compositor change); 3 of 3 reruns passed. [more](backlog/flakes.md#fl-080) | open | 19 |
| Endfield's start on the N150 reaches the world 5 of 11 times (2026-10-24; debug-logging yserver worse), and under a game load woken programs on every processor waited 0.9-3.2 s while syscalls and the kernel's own timers kept time; the Intel driver was once out 65 s. Tally starts here. [more](backlog/flakes.md#fl-081) | open | 5 |
| The N150's YT6801 did not take its link back after a cable swap while Ferrix ran (2026-10-24): ARP to the box failed for 25+ min with the plug on, until a cold start. Detect link loss and renegotiate. [more](backlog/flakes.md#fl-082) | open | 3 |
| Stage 4's lock check stopped release boots four times with "no two processors' increments overlapped in any round" (2026-10-24, F-95's sched-wake-latency, `test-stall --release` at host load 35-80), with and without F-95's option A; never in a debug boot. Rerun once. [more](backlog/flakes.md#fl-083) | open | 4 |
| The System V semaphore check's deadline case failed boots under load, "a waiter returned without waiting" (FX-0702, 2026-10-25): the check's timing; fixed on sem-waiter. [more](backlog/flakes.md#fl-084) | fixed | 7 |

### P2 — quality and performance, on the "fast" half of the goal

* **`trap::ReturnPath` as a static trait seam** (the consultant, ledger 3934 (b)): the personality's return-path hooks, a struct of function pointers the load registers in the core at run time, would follow `docs/KERNEL-CRATES.md` §6.3's link-time binding. It is a core interface whose `fault_signal` changes under 3570 (2), so it comes after `CrashHook` and that change, alone, with its own design review. Open.

188 items: performance, hardening, certification follow-ups the
consultant asked to ride with a later landing, and compatibility gaps that
block nothing now. They are in [backlog/p2.md](backlog/p2.md). Read it
when you take P2 work, or before touching an area to see what is owed there
(search it for the file or subsystem); file a new P2 item there.

### The desktop and the GPU, what is left

| Item | Stage |
|---|---|
| hyprix draws the top layer (waybar) over a fullscreen window, and under `--no-gl` its blur is then the frame's largest phase (35 to 72 ms); Hyprland draws only the overlay. Fix at `over_windows` and `render_onto`'s `Phase::Over`, then re-measure `--no-gl` fullscreen frames on a quiet host [more](backlog/desktop.md#dg-001) | open |
| The video wallpaper stops decoding under a covering window only when its buffer is opaque. Unverified: which format yserver gives a game's window (Teeworlds case), and if ARGB whether a fullscreen X window's alpha is honoured. Run it and decide [more](backlog/desktop.md#dg-002) | open |
| Settings (`docs/SETTINGS.md`): app, game-mode page, config file and launcher mark landed; game mode is a mark with no effect. Left: AArch64/ARMv7-A boots, look on `--everything` as `ferrix`, first real effect (design round on branch `game-mode-design`, administrator privilege), live list, second page. Owner: open [more](backlog/desktop.md#dg-003) | 24 |
| `run-compositor --everything` must be everything (customer rule). Most done (volumes fetched, apps and ports built, launcher entries). Left: choosing Steam from fuzzel after closing the client, untried on a live desktop; vkgears needs Venus, so a Windows host cannot list it [more](backlog/desktop.md#dg-004) | 19 |
| `test-audio --arch x86_64`'s mix boot failed once at load 15 to 19 (1000 Hz at 7537, not 8192 within 5%) while gating the ports' move; passed on the rebased tip. Looks like a lost or late period under load [more](backlog/desktop.md#dg-006) | open |
| LEDs on virtio-input: its status queue takes STATUS and drops it, so QEMU's keyboard stays dark; `write` of types other than `EV_LED` and `EV_SYN`, which Linux injects and Ferrix answers `EINVAL`; the LED events passed to the node's readers. Done for USB keyboards on 2026-09-23 (`docs/INPUT.md` §3.3, §7.4) | 17 |
| A graceful STOP for an input driver, and a gate row: nothing sends STOP (the input core never does; devmgr kills the job), so `ferrix-driver`'s stop path has never run. Needs the core to send STOP on unbind, a `test-restart` variant with a negative control, and certification review before landing (os-9f) [more](backlog/desktop.md#dg-007) | 17 |
| Multi-touch axes (`ABS_MT_*`), force feedback (`EV_FF`, `EVIOCSFF`) and sound (`EV_SND`) on `/dev/input/eventN`. The input core publishes a device without them and its boot line says what was left out; QEMU's keyboard and tablet declare none (`docs/INPUT.md` §3.2, §6, os-f6 2026-09-16) | 17 |
| Input hotplug: devices exist from boot only. A device arriving or leaving later, and how a compositor learns of it without udev (`inotify` on `/dev/input`, not for devfs's own nodes, or a rescan) (`docs/INPUT.md` §3.4, §6) [more](backlog/desktop.md#dg-008) | 17 |
| `card0` is opened by one process at a time, standing in for DRM master (`docs/DISPLAY.md` §5, a written deviation from Linux): Linux's many opens with one master, `SET_MASTER`/`DROP_MASTER` arbitrating between them, and the render node beside it come in stage 19 | 19 |
| The Intel GPU's opaque crash mode (`docs/INTEL-GPU.md` §12, the customer's 2026-10-23 decisions in §12.9): step 1 (the `remote` protocol and journal, the keeper, devmgr's job and image at START, the fake worker, `test-restart --boot intel` and `--boot intel-keeper`) on branch `gpu-opaque`, 0 kernel lines. Left, §12.11: step 2 the product worker (`XelpBackend` over keeper memory, M1 and BAR 0 in the worker, card0 into the keeper, the ring-3 quarantine and budget headroom of ledger 3156 (2), (3), the (5) paragraphs), 16 points; step 3 context state at replay, 4; the box run, 4 | 18 |
| The GPU for clients: Mesa's virgl on ferrousli (`docs/GPU.md` §3a, 40 points or more) with `zwp_linux_dmabuf_v1` v4 feedback; zero-copy Venus presentation via dmabuf import (§6.1); the Khronos Vulkan loader, waiting on `dlopen` of TLS libraries. A4 (dmabuf v3, GBM allocator) is done [more](backlog/desktop.md#dg-009) | 19 |
| NVIDIA's own driver on the RTX 3060 (`docs/NVIDIA.md` §7). N0 complete, N1 done (2026-10-03; last three landed ahead of review conditions, ledger 297). Next N2 Vulkan offscreen (14), N3 frames to hyprix (30), N4 Chrome and Steam with DRI3/Present (20); CUDA N5 C0a landed, N6 waits for a monitor [more](backlog/desktop.md#dg-010) | 21 |
| Gears on the DK1's GC400 (`docs/GPU.md` §6.2, §6.3): G1 and G2 were done on the board on 2026-09-24. Left: G3 a clear resolved to the LTDC's buffer (8), G4 draws with host-compiled shaders (8), G5 gears on the board (5) | 19, P3 |
| The DK1's LTDC display and USB HID, done 2026-09-23 (`docs/DISPLAY.md` §6, `docs/INPUT.md` §7). Left: other modes than 720p60, display hotplug, and HID's absolute axes and vendor reports (§7.5) | 17, P3 |
| The DK1's LTDC on displayctl v9 (each ATTACH's buffer VMO pinned, E-02, 2026-10-17): built for armv7a and aarch64, never booted on the board; a DK1 boot showing the desktop is owed (ledger 2297, condition 6) | 17 |
| The DK1's USB host after U-Boot's `ums`: the PMIC's `vdd_usb` (STPMIC1 LDO4) is left off and the kernel never turns it on, so nothing enumerates; U-Boot's `regulator enable` is the workaround. The kernel's USB preparation should enable LDO4, with the RCC's care [more](backlog/desktop.md#dg-011) | 17, P3 |
| The desktop clients' foundation (`docs/DESKTOP-CLIENTS.md` §2): `src/user/system/linux/compositor/toolkit`, `src/user/system/linux/compositor/text`, `src/user/system/linux/compositor/hyprlang`, `src/user/system/linux/compositor/image`, and `run-compositor --config` carrying the user's dotfiles and the fonts they name. Owner: clients-base. Estimated 21 points | 19 |
| fuzzel in Rust (`docs/DESKTOP-CLIENTS.md` §4): core, window, hyprix keyboard for layer surfaces, entries and `--boot fuzzel` landed, and the user's SUPER R (b9cb64d5). Left: clipboard pastes, keyboard back on a click elsewhere, 30-second start with 20 host fonts, aarch64 boot, the `pkill -x fuzzel` toggle (P2), lock-file warning. Owner: fuzzel. 21 points [more](backlog/desktop.md#dg-012) | 19 |
| waybar in Rust for the user's `~/.config/waybar`: draws the user's bar on the desktop and on `run-compositor --everything` (toolkit, hover/clicks/tooltips, hyprix half, PulseAudio client, `--boot waybar`, `waybar-volume`, `--boot everything-desktop`); Python-script chips hidden, no load/wifi/tray. Owner: clients/waybar. 34 points, 30 spent [more](backlog/desktop.md#dg-013) | 19 |
| hypridle (`docs/DESKTOP-CLIENTS.md` §6): the hypridle app (ferrix-os/apps), which builds `/bin/hypridle` and `/bin/loginctl` over the foundation's hyprlang and toolkit, and the `idle` and `idle-user` boots. Green on x86_64 and aarch64 on 2026-09-26. Left for Ferrix: a suspend for the sleep hooks, and D-Bus inhibitors. Owner: clients-hypridle. Estimated 5 points | 19 |
| Moving wallpapers on nazuna were never looked at since the AV1 change: only two of 17 are `.ivf`, an orphaned `.fxvid` remains, ten `.fxwall` are single frames. Re-run `cargo xtask wallpapers --from <dir>`, check the 64 KB clip, delete the `.fxvid`, boot and look; no gate covers it. 1 point, wants whoever has nazuna's display [more](backlog/desktop.md#dg-014) | 19 |

### P3 — hardware variants and later stages

* **`LASTKERNEL.TXT` written through U-Boot's EFI FAT** (lode-pstore, the consultant's ledger line 3693 (2)): the loader keeps the previous kernel's log on the ESP through firmware's FAT write, gated only on QEMU's EDK2 (`test-postmortem`, `test-log-pstore`). The DK1 and the other U-Boot boards are unrun: a board run must show the loader's `kept in FERRIX/LASTKERNEL.TXT` line and the file read back on the host. If the board's U-Boot is built without EFI FAT write, the expected outcome is the loader's one `not kept in FERRIX/LASTKERNEL.TXT` line and a boot that goes on. Needs the board and the PO's OK. Open.

* **The N150 box's desktop is unreachable whenever hyprix does not run**: (intelgpu, 2026-10-18) its only `udhcpc` (`/etc/n150-desktop.sh`) and its `sshdt` are hyprix's `exec-once` lines, and hyprix's unit requires `dev-dri-card0.device`. A boot without a card (no `ferrix.efifb`, as the Intel GPU's M1 boot was) or a hyprix crash leaves the box with no lease, no sshd and a keyboard nobody reads: on 2026-10-18 the box needed three power cycles to come back. Reproduced in QEMU (`test-n150-desktop` with `ferrix.efifb` dropped: gateway `dhcp 0`). Fix: DHCP and sshd as units of their own under `multi-user.target`, independent of the compositor; the box image is built outside `main` (its `n150-bless.service` is not on `main`). On `main` since 2026-10-18 the box's desktop is `ferrix`'s session (`sessiond`): `/etc/n150-desktop.sh` is root's `n150-desktop.service` under `multi-user.target`, and `--ssh`'s `sshdt` becomes `sshd.service` (`crate::session`); the box's own image still has to take that. Owner: whoever owns the box image. Open.
* **YT6801's first run on the N150 box**: driver and core proven on host models only (`net-yt6801`, consultant's D7); the run must show device `1F0A:6801`, MSI-X Enable 0, BAR 0 aperture, one counting MSI vector, and MSI config across `SYS_RESET`. Re-mint after a cleared MSI capability needs design review. Needs PO approval and the box. [more](backlog/p3.md#p3-001)
* **RTL8111H's first bare-metal run on nazuna**: (`feature/rtl8169`, 2026-10-24) proven through vfio in a KVM guest (1 Gbit link, DHCP, ping, TCP 235-405 Mbit/s); bare metal owes the same lines, a lease and a ping, with `ferrix.unisolated` until AMD-Vi lands. Whether to ship Realtek's `rtl8168h-2.fw` PHY patch is the customer's and the consultant's. [more](backlog/p3.md#p3-018)
* **xHCI's first run on a PC**: gated against `qemu-xhci` and a model only (`docs/XHCI.md` §8, §9, `xhci-driver`); `test-usb` shows two input claims, not eight (X9, Y7); no AArch64 boot shows the driver. After INSTALLER.md's H0 on the customer's PC. Owner: product owner. [more](backlog/p3.md#p3-002)
* **A USB disk served to the block core**: (INSTALLER.md H4) `usbhid` reads a bulk-only disk and serves it to nothing; a block claim on a shared node needs a design review first (consultant ledger line 672). `test-install --usb-live` waits on it. Open. [more](backlog/p3.md#p3-003)
* **SuperSpeed hubs** (`docs/XHCI.md` §8): refused by design, so a USB 3
  device behind a USB 3 hub is not found (its USB 2 half works). Needs the
  hub descriptor of type 2A, `SET_HUB_DEPTH`, and the USB 3 port status
  bits. Open.
* **A PCI function left with memory decoding off gets no aperture**: QEMU AArch64 `virt` under EDK2 leaves `qemu-xhci` so, `usbhid` exits and `test-usb` is x86-64 only (`xhci-driver`). A rule to enable decoding for a vouched BAR would change the certified item. Open. [more](backlog/p3.md#p3-004)

* **`FS`/`GS` skip's `GS` base control on AMD without `NullSelectorClearsBase`**: (`docs/OPAQUE-KERNEL.md` §9.8 3c, po10-sel-cert, K2) cannot fire on nazuna's Ryzen 9 9900X or TCG; needs Zen 2 or older hardware. Owner: whoever next has such a machine. [more](backlog/p3.md#p3-005)
* **`k1-bringup` control did not fire**: (po10-sel-cert, K1, L2) compatibility-mode reads on processor 3 still faulted under KVM on nazuna; the load stands on argument. Owed: a run where it can fire (Intel KVM, hardware, or a free processor). [more](backlog/p3.md#p3-006)
* **DK1 run `cacheflush` owes**: (po10-chrome-a, consultant's D1.5) only the board shows whether `cacheflush` makes JIT-written code run; `jitflush flush` must find no wrong answer, `noflush` is expected to. Needs a board image; scheduled by the PO. [more](backlog/p3.md#p3-007)
* GICv3 and its redistributors, with a second AArch64 boot configuration
  (`gic-version=3`); x2APIC; TSC-deadline. Real AArch64 hardware is GICv3.
* Stage 13's namespaces and seccomp (its cgroups are in), stage 14
  (real-time domains), and global page-cache reclaim, which `rustc` on a
  small machine needs and no stage names today.
* `vfork` sharing memory rather than copying it.
* Stage 21, bare metal with an NVIDIA card driven by Ferrix itself: Path B
  of the GPU decision of 2026-09-18, `docs/GPU.md` §4. Opened when the
  customer wants Ferrix on real hardware; unsized, over 100 points.
* **Stage 22, Steam**: 32-bit x86 ABI being built (ferrix-41, `docs/I386.md`), ferrousli under the Steam runtime, bubblewrap on stage 13, XWayland (40), sound, Vulkan via Venus. Over 300 points; the roadmap's stage 22 is the list. [more](backlog/p3.md#p3-008)
* **Stage 23, Arknights: Endfield**: Windows game via Wine and DXVK on NVIDIA Vulkan, eight steps, about 205 points, unowned, not started. Step 0 (8) waits on the customer; steps 1 (42) and 2 (18) can then run side by side. [more](backlog/p3.md#p3-009)
* **Stage 24, the game path**: fullscreen game faster than on stock Linux (`docs/GAME-PATH.md`); about 68 points before the game domain, unowned, not started: rig R1 to R5 first, then the first cut (47). Game domain G1 to G5 (52) waits on the customer. [more](backlog/p3.md#p3-010)
* **Bad Apple!! and Doom**: Bad Apple!! with sound, `test-badapple` and the `--everything` desktop window landed (≈ 8 + 8 spent). **Doom in Rust is in the backlog**: D1 to D4, estimated 21, not started, unowned (`docs/MEDIA.md` §3). [more](backlog/p3.md#p3-011)
* Ferrix in 8 to 16 MiB, for an FPGA RISC-V board (Tang Nano 20K, 8 MiB):
  `docs/SMALL-MEMORY.md`. Phase 0 (`--strip-kernel`, the end-of-boot
  memory line, CI's 128 MiB row) landed; phases 1 to 3 are estimated at 29,
  unowned. The RISC-V port itself is unsized.
* Huge pages; frame share and release are order 0 by design.
* A panic report as a QR code: a port of Linux's `drm_panic_qr` as
  `src/lib/kernel/qr` (ferrix-qr), so a panic screen can carry the whole report. WIP
  on branch `worktree-agent-a33c10946b6721065` (5690f0e), unbuilt into the
  panic path.
* **SD card driver on the DK1**: (`sdmmc`, `docs/CHROME.md` §10.1, po10-chrome-b) built and host-tested, never run on the board. Owed with PO OK: (1) read-only hash check and write refusal, (2) writes to `ferrix-scratch`, (3) btrfs on it. Consultant's C5. [more](backlog/p3.md#p3-012)

* **S1 on hardware: `reboot(2)`'s write-back and the stop word** (crashlog, 2026-10-21, `docs/POSTMORTEM.md` §5, consultant ledger 2749 C1): `L.console.47`'s write-back is not observable under QEMU. On the N150 box, with the customer running the steps: `reboot` from the shell of an image of S1's landing hash, then the next loader's `pstore   boot N ended: a restart a program asked for (reboot(2))` (in BOOTLOG once PM2 lands, or from the inspection Linux's region dump), and the region's text ending at the `reboot: Restarting system` line.
* **PM3 on hardware: kernel log across a warm reset**: (pm3, `docs/POSTMORTEM.md` §3, Q13, P9) panic write-back (`L.console.44`) unobservable on QEMU; needs a deliberate panic then warm reset on the WD SN720 PC and the DK1. Also owed: `LASTKERNEL.TXT` to the ESP after PM2, to netboot after netboot-pm (P10). [more](backlog/p3.md#p3-013)

* **NVMe, what the driver leaves**: (nvme-driver, `docs/NVME.md`) (1) first run on the WD SN720 PC after H0; (2) `CC.SHN` on STOP unreachable; (3) `/dev/nvme0n1` naming; (4) HMB, timeouts, Abort model-only; (5) one I/O queue pair; (6) AArch64 shell gate; (7) one 100 ms late interrupt at load 50. [more](backlog/p3.md#p3-014)
* **A function below a PCIe root port has its DMA faulted by SMMUv3**: (nvme-driver) `test-nvme --boot up --arch aarch64` with `FERRIX_NVME_BELOW_PORT=1` ends `FX-1007`, 16 DMA faults; works on the root bus and on x86-64 behind VT-d. Fix is in the certified item's IOMMU code, so through the consultant. [more](backlog/p3.md#p3-015)
* **N4a, interrupts instead of polling on the N150, is the drivers' (vtd-n150, 2026-10-17)**: remapping does not gate MSI minting (`msi_allocate` mints a compatibility-format message without it), and the YT6801's one MSI is delivered on the box, rarely; what is owed is why the chip raises it rarely (it is in `MSI_PREFERRED`, while Linux runs it on six MSI-X vectors) and why xHCI raises none. Not closed by VT-d. [more](backlog/p3.md#p3-016)
* **AMD-Vi on nazuna, bare metal (amd-vi, 2026-10-24)**: the AMD-Vi driver (`docs/AMD-VI.md`, ledger 3867, 3923) is shown on QEMU's patched amd-iommu only. Owed (ledger 3867, condition 12): a bare-metal boot of the landing hash on nazuna under `ferrix.checks=skip`, its lines recorded here against `docs/AMD-VI.md`'s *As built* (unit 0xf7500000 translating, `1 root complex`, 0 bypassing, the unchecked-boot fault line). The PO's or the customer's call, since nazuna is the gate host; `ferrix.amdvi=off` is the recovery entry beside it. Also owed: a QEMU patch giving the AMD-Vi row's IVRS an IVMD (as O2 is for VT-d's RMRR), so a boot shows `L.iommu.94`'s honouring at bring-up and `L.iommu.95`'s hold, which stays recorded unverified until then (ledger 4108 (4)).
* **VT-d slice 2: honour RMRRs (vtd-n150, 2026-10-17)**: built as K2 on `intel-k2` (design `vtd-design.md` §3.9(b), consultant ledger 2388; `L.iommu.73`-`78`): an RMRR that is whole pages, in reach and over no RAM is identity-mapped in a kernel-held domain before TE, and dmar0 translates. Left open: the box run (P1 row above) and O2's QEMU patch giving QEMU's DMAR an RMRR, so a boot check can show identity-before-TE with device DMA (`vtd-design.md` §2; deferred, not unbuildable: Ferrix already carries a patched QEMU). [more](backlog/p3.md#p3-017)

---

## Fourteen programs stand between the userland and deleting busybox

The uutils family, zinc and the ports own `/bin` now. Fourteen names are
still busybox's and still gated: `sysctl`, `fdisk`, `top`, `mpstat`,
`iostat`, `pwdx` and `su` in `test-vfs`, and `ip`, `route`, `netstat`,
`nslookup`, `ping`, `ping6` and `udhcpc` in `test-net`.

None of it is blocked on the kernel. Netlink, raw sockets, `/proc`,
set-user-id and ferrousli's resolver are all there and gated; what is missing
is user-space programs over them. `docs/UUTILS.md` §8.1 breaks it down: 28
points, the largest single piece being `ip` over netlink at 8.

And read §8.3 before starting. Deleting busybox also takes `grep`, `sed`,
`awk`, `tar`, `mount` and every editor with it, none of which anything
replaces. Keeping busybox in the image as one program among others costs
1.2 MiB beside uutils' 14 MiB. Whether S8 is wanted at all is the customer's
call, and nothing after S6 depends on it.

## Velocity

The measurements of 2026-09-18 and their updates to 2026-10-05 are in
[backlog/VELOCITY.md](backlog/VELOCITY.md); the per-day figures the charts
draw are in `tools/common/gen/gen-roadmap-charts.py`.

## Decisions

Every decision taken along the way, with who took it and when, is in
[backlog/DECISIONS.md](backlog/DECISIONS.md). Read it before changing scope
or reverting anything that looks odd, and add a decision there the day it is
taken.

## Waiting on the customer

* **F-76, the core's and the item's unclassified libraries** (2026-10-23,
  filed with the kernel split, `docs/KERNEL-CRATES.md` §5.1; ledger 1585 Q1):
  the core links 21 first-party libraries and the item 16 that no ring
  classifies, 25 in all (`unclassified_deps` in
  `tools/common/data/certification-item.json`): classify each into a ring
  with its evidence, allowlist it as infrastructure with a reason, or
  re-scope the item. The customer's, through the product owner.
* **Settings' game mode**: (`docs/SETTINGS.md` §8) open questions: game scope beside the session's; one-way barrier towards `nvrm` for all or named jobs; speculation domain by admission or birth; who grants (`wheel` or root); whether the certified item takes it; the game path's question 4. [more](backlog/waiting.md#wc-001)
* **The game path, stage 24**: (`docs/GAME-PATH.md` §10) which Linux to judge against; opening stage 14 early; speculation domains for yserver, hyprix, `nvrm`; VRR; latency hardware; native render process; session defaults; a bare-metal run before any figure leaves the tree. [more](backlog/waiting.md#wc-002)
* **Developing Ferrix on Ferrix**: (2026-10-11) the six decisions of `docs/DEV-ON-FERRIX.md` §9: editor, trying a kernel from inside, secrets before the PC, second root, live driver updates, refetching the toolchain volume. [more](backlog/waiting.md#wc-003)
* **Stage 23, Arknights: Endfield**: before step 0: an expendable game account (ban risk), about 110 GB disk, the card. Before step 4: whether the EULA permits Wine; step 5: answering `Linux` to this program; whether the ≈ 205 points enter the burndown. [more](backlog/waiting.md#wc-004)
* The Steam-performance session's pushes (2026-10-02): local `main` on
  nazuna is ahead of `origin` by the night's landings (`docs/STEAM.md` §8),
  and yserver's `steam-perf` branch is not on the fork. Both are the
  customer's word; the fork's pin waits on both (P2 row above).
* The live installer's reference PC (`docs/INSTALLER.md` §10 decision 3):
  which machine H0 and H7 run on, and whether it may be booted from a stick.
  The VM path does not wait on it.
* F-43 (`docs/certification/FINDINGS.md`): O.WXN and ASR-2 claim no mapping is
  writable and executable, but a program's own `mmap(PROT_WRITE|PROT_EXEC)` is
  honoured, as on Linux. Enforce W^X for programs (and walk user roots in the
  sweep), narrow the claim to the kernel's mappings with an assumption of use,
  or make it switchable. The certification session drafts whichever is chosen.
* F-52: the Security Target names no Common Criteria version. A new
  evaluation starts under CC:2022, whose Part 2 restructured families the ST
  uses (FAU_STG among them). Name the version in §2.1, and the certification
  session re-reads §5 and §8.4 against it.
* Whether the Security Target claims FMT_SMF.1 (the management functions:
  a handle duplicated with fewer rights, a job's limits set, a starter or the
  audit handle given to pid 1) and FMT_MTD.1 (the job limits as TSF data).
  SECURITY-TARGET.md §8.4 justifies their absence today (F-47).
* The init design's open decisions, `docs/INIT.md` §14, 2 to 8: the unit
  syntax, hyprix leaving pid 1, `devmgr` under init, what init's death does,
  the names. Each has a draft answer the design assumes meanwhile; L11 to
  L13 wait on them.
* Whether deleting busybox (S8, `docs/UUTILS.md` §8.3) is wanted at all.
* ~~System V semaphores for Steam~~ **decided 2026-09-28: build.** The
  client does not survive without them -- it waits forever after "Thread
  synchronization object is unuseable", before any download (I5b); built on
  branch `steam-sysv-sem` (`syscall/sem.rs`, `test-sem`, `docs/I386.md`).
* **User namespaces for Steam (I5b)**: answered 2026-09-28: build user and mount namespaces (`docs/NAMESPACES.md`, branch `steam-userns`). N1 to N4 and NP landed; N5 to N7 (unprivileged mounts, bwrap as uid 1000, pressure-vessel) not built. [more](backlog/waiting.md#wc-005)
* Whether the customer's Python desktop scripts are rewritten for Ferrix:
  `hypr-workspaces` and `ba-calendar` (waybar's workspace chips and clock),
  `hypr-desktop-fx` (pointer effects) and `hypr-dock`. Without them those
  chips are hidden and the rest does not start, which hyprix reports once.
* Daytime hardware: F-44 on the Pixel 7's crosvm, and F-48 and F-50 on the
  DK1, each needing the product owner's word for the device.
* The WHPX panic: whether to report QEMU's MMIO emulator upstream.
* The Pixel 7 launcher helper on example (`tools/vendor/google/pixel7/helper.py`, port
  47707) was started before the relayout and still holds
  `bootloaders/pixel7/mkbootimg.py`, which is now `src/boot/vendor/google/pixel7/`: a boot it
  builds fails until it is restarted. Restarting it was refused to an agent,
  as interfering with a running workload; it needs the customer's hand.
* System V IPC for Steam (ferrix-41, I5 of `docs/I386.md`): **decided
  2026-09-28: build the semaphores**, which the Steam client cannot run
  without; built on `steam-sysv-sem`. Shared memory: **decided 2026-10-02:
  build** (see Decisions); built on `steam-sysv-shm`. Message queues are
  still `ENOSYS` and would be a new decision.

---

## Branches that still hold unlanded work

Every such branch on GitHub, grouped by line of work and with which one to
resume from, is in `docs/roadmap/open-branches.md` (2026-10-01, os-5d).
This section keeps the detail of the families below.

The wind-downs of 2026-09-13, -14 and -17 surveyed every branch; what is
left of them, each with its row above: `os-12/ports-autobuild` (the ports
built when stale) and
`worktree-agent-a33c10946b6721065` (the panic QR code). Those wind-down
records are in this file's history.

The microkernel program's wind-down on 2026-10-19 (PO os-e1; `docs/MICROKERNEL.md`
M1 and M0's leftovers, so the `max` and `fast` profiles can build). Every
branch is on origin and on nazuna. The ids, call numbers, panic codes and
audit numbers are reserved on main (f5c048964, its message lists them). The
five M1 designs and smt=off's are on main with the consultant's OK IF; each
row's code still owes its gates, controls and the consultant's diff check.
Spent about 33 points of about 70 estimated.
- `mk/k5` (56f1ddbac, WIP, partly built): wait on a user address, K5.md,
  OK IF ledger 2498. Next: build the native calls, sysml ids, FX-0911's check,
  controls C1-C7, the futex ABAB. About 12 points left.
- `mk/k6` (5dc0dc337, WIP, `cargo check` on three architectures, host tests
  pass, never booted): attested sender, K6.md, OK IF 2485 and 2524. Next: sysml
  ids, five boots, controls C1-C6, the round-trip ABAB with the fast path off
  and on. About 6 points left. K7 lands on it.
- `mk/k7` (no code yet): charging that follows the client, K7.md, OK IF 2525.
  About 9 points left; it lands on K6, and whichever of K7 and K12 lands second
  wires K12's lookup to `charge_target`.
- `mk/k10` (5509676d1, WIP, unbuilt): timer objects, K10.md, OK IF 2538.
  Next: the native handlers, `timer_check.rs`, sysml, boots, controls N1-N8,
  the ABAB. About 5 points left.
- `mk/k12` (8af71f23d, WIP, never compiled, most of K12a): kernel-memory
  reservation, K12.md, OK IF 2550, split into K12a (frames) and K12b (heap).
  About 9 points left.
- `mk/f73` (6b1be87bd): F-73 rebased, with `init_traps` split to stay under the
  complexity floor; every gate is owed again on this hash (queue script in the
  agent's report: check, release, x86_64 boots default and `--smp 2`, test-mce
  kvm and tcg, aarch64, armv7a and `--smp 2`, three controls), then the
  diff check (ledger 1057, 2103 (5)).
- `mk/f64` (2ed846793, on `mk/f73`): F-64 rebased with conflicts, so its
  controls fire again; no gates yet (ledger 1065). Restores F-65's heading in
  FINDINGS that the old commit overwrote.
- `mk/baselines` (984885f42): `bench-rows` tooling only; its gates were
  queued under `mkbl-*`. The baselines themselves are owed on an idle host.
- smt=off: design on main (M0-B-PROPOSALS §3.1, OK IF 2563, parent H.TRAP.19);
  no code. Lands after F-73. `mk/smt-off` (407b616c6) is superseded.
- The gate pool ran on 2 of 5 slots all afternoon: nazuna had 51 GB free,
  under `gate.sh`'s 65 GB open line. Freeing about 15 GB brings slot 3 back.

os-35's wind-down on 2026-10-01, cutting the ring-3 trip
(`docs/OPAQUE-KERNEL.md` § 8). Each branch is local on the Windows checkout
and pushed to nazuna as a side ref of the same name. Each goes to the
certification consultant (os-9f) before it lands.
- `os-35/ipc-lazytlb-on-ef206bb2` (b10f3c7e, "WIP:"). Lazy TLB, reworked to
  the consultant's three conditions:
  - lazy only where the processor has SMAP or PAN, eager elsewhere;
  - the drop path's rules at release strength (FX-0010);
  - requirements renumbered to L.user.108–110 after F-55 took 107.

  The same code gated green at 065e3cb3 on d86821bb. Left: on nazuna, run
  carry-coverage `--from ef206bb2`, the coverage justification, the panic
  catalog, the traceability record and `model-doc`. Then the full row, the
  commit message, the consultant's second look, and land. The AArch64
  lazy path with PAN never ran, because QEMU's default processor has no
  PAN.
- `os-35/ipc-ring` (7c52861e, "WIP:"), part B: the ring task off the data
  path. It is written, both negative controls fired, and it passed the full
  row on 5cc5ed38. Left: rebase onto main behind part A, the full row, the
  consultant, land.
* **`os-35/ipc-wake`**: two WIP commits: 87675432 part B (sync wake onto the waker's processor; left: rebase, re-apply `Wake::Sync`, full row, consultant) and dbd39280 part C (halt-poll, targeted SGIs; left: full row, F-50 confirmed). MSI affinity only noted. [more](backlog/branches.md#br-001)

  Part A, the MSI-X masking, landed as 1dcc433f, and the ring's part A as
  b7cab053, both on 2026-10-01.

os-86's wind-down on 2026-10-01 (it was os-c7 before the restart), the channel
round trip (`docs/OPAQUE-KERNEL.md` §9). The speculation domain landed as
bf9efba95. Each branch below is local on the Windows checkout and on GitHub.
- `os-ipc/zircon-trip` (a1379b25b, "WIP:"). The round trip's other changes,
  based on a `main` from before the domain landed. §9.4 says which of its
  commits to cherry-pick onto `main` and which to drop. Its review goes to the
  certification consultant.
- `os-ipc/prof` (41bdef2ca) and `os-ipc/prof2` (4b5be585c), timing builds
  that print a round trip's spans. They must never land.
- `os-ipc/spec-domain` (bf9efba95) is on `main`, and `os-ipc/winddown` is
  this record. Both can be deleted.

A Windows-checkout wind-down on 2026-10-01, before switching to another PC:
every local branch and worktree was checked for content worth keeping and
for whether it was already on GitHub. 91 branches had no upstream
configured; 23 of them had no commit `main` lacks (already landed, safe to
delete) and the other 68 did. All 68 are now pushed: 48 as brand new
branches, 19 that turned out to already match a branch GitHub had under the
same name (another session's concurrent work), and `guest-frame-time`,
below, which could not be pushed as a plain fast-forward. Ten worktrees also
held uncommitted changes; seven were real, finished-looking work and are
listed below, and three needed a human call, also below.
* **`guest-frame-time`**: diverged from GitHub: origin has six newer WIP terminal/scroll commits, local has `d33a86602` (guest compositor frame-cost fix); they overlap in `compositor/render/exact.rs` and `term/*`. Owner of the WIP decides and reconciles; `d33a86602` is on GitHub as `guest-frame-time-local`. [more](backlog/branches.md#br-002)
* **`clipboard-vdagent`**: diverged: origin landed a `user/vport` clipboard agent, local built a `compositor/vdagent` crate, kept as `wip/clipboard-vdagent-compositor-approach`. Left: decide which approach stands, or whether `vport` covers it. [more](backlog/branches.md#br-003)
* **`stage13-timens` (worktree `timens`)**: time namespaces mostly staged, blocked on a merge conflict in `panic/catalog.rs` (FX-0907, FX-0910). Left: take both entries, regenerate `docs/generated/PANICS.md` and two certification coverage JSONs, then commit. [more](backlog/branches.md#br-004)
* **Detached worktree `display-show` (`d9c5658ed`)**: one uncommitted line changes `compositor/blank/src/modeset.rs`'s `BACKGROUND` to pure blue while the comment says dark slate; looks like a debug probe. Left: confirm and discard, or finish and fix the comment. [more](backlog/branches.md#br-005)
* **102 idle branches archived (2026-10-23, second sweep)**: every `feature/`, `wip/`, `mk/` and `chore/` branch idle for seven days, one "idea:" line each with its archive ref `refs/archive/2026-10-23b/<name>`, grouped by stream. Pick one up by restoring its ref. [more](backlog/branches.md#br-006)

Seven pieces of real, uncommitted work were found and committed today, each
pushed rather than left only on this disk:
- `wip/windows-home-edid-remote-fix` (`019632ddb`): xtask's `HOME` lookup
  falls back to `USERPROFILE` on Windows; a monitor's EDID is saved under
  `~/.local/share/ferrix/edid/` and read back on a machine that never had
  it plugged in; `remote-desktop`'s teardown no longer stops a newer run's
  boot.
- `codex/posix-stdlib` (`f52a68966`): `quick_exit`, `at_quick_exit`,
  `secure_getenv`, `a64l`/`l64a`, `getsubopt`, and `crypt`'s traditional and
  BSDi DES.
- `display-design` (`fdd665dad`): `docs/DISPLAY.md`, Draft 1 of the stage-17
  ring-3 virtio-gpu driver, for the product owner and kernel review.
- `ferrousli-netcore` (`a3b2152e9`): the resolver's DNS, lookup and `inet`
  rework that `ferrousli-netdb` (above, this file's os-35/os-86 style
  entries notwithstanding) builds on.
- `init/l4` (`1f5f72870`): `libs/svc` gets a `manager/` subsystem — a
  dependency graph (`Kind::implied`), lifecycle and a restart policy — the
  init program's L4 step.
- `os5d/gate-rows` was pushed and has since landed on `main` as `7afed6fdc`
  by another session.

`docs/roadmap/status.md` and `docs/roadmap/where-it-stands.md` track stage
progress and finished work, not which branches are still open, and the other
mentions of an unlanded branch (`docs/AUTH.md`, `docs/CONVENTIONS.md`,
`docs/OPAQUE-KERNEL.md`, `docs/POSIX-2024.md`) each name one branch. The
list of every open branch is `docs/roadmap/open-branches.md`, landed the same
evening; this section keeps the detail of the families and the decisions
above, and the page links here.
