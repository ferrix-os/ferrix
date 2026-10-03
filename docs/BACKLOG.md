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

---

## Standing rules

These add to `docs/CONVENTIONS.md`, which still governs commits.

**What a landing runs.** Rebase onto `main`, then:

| The change touches | Gate |
|---|---|
| Only documentation: `docs/`, top-level Markdown, the skills and agent definitions under `.claude/` (Markdown nothing compiles), and the generators that write documents alone (`tools/common/gen/gen-roadmap-charts.py`, `gen-arch-doc.py`, `split-roadmap.py`, `build-roadmap-book.sh`) | `cargo xtask check-docs` only: the commit hooks, the audits and the generated-document checks, no cargo step (customer, 2026-10-04); it runs on any host, the Windows PC included. A generator that writes code or test data (`gen-xkb-tables.py`, `gen-wayland-protocol.py`, `gen-font.py`, `gen-btrfs-fixtures.py`, the fuzz-corpus seeds) keeps the row of what it writes |
| Only `src/user/system/linux/ferrousli/` | `cargo xtask check --ferrousli`, then `cargo xtask busybox` and, with the busybox it built, `test-shell` and `test-vfs` on x86_64 with `--init ferrousli`, so the binary the gates run never lags the library; then `cargo xtask uutils`, which links uutils/coreutils against it and is the larger consumer of the two, since it brings Rust's whole `std` with it; a change to `src/user/system/linux/ferrousli/tools/ports/` also runs `cargo xtask build-apps` for the ported apps it touches (curl and git on every architecture) and `test-net --arch x86_64 --init ferrousli`, which fetches with the curl it built, then `test-net --arch all` with the static busybox, which fetches and clones with them |
| Only `src/user/system/linux/zinc/` | `cargo xtask check --fast --zinc`: zinc's formatting, clippy, unit tests and the two pty tests (`src/user/system/linux/zinc/tests/pty_completion.py`, `src/user/system/linux/zinc/tests/pty_jobs.py`); a change to what zinc does at boot also runs `test-boot` on x86_64, and a change to how it starts, waits for or signals a process also runs `test-shell --arch all` and `test-jobs`, which is the only gate that types at a console |
| Only `src/user/system/linux/init/`, or `src/lib/init/svc` | `cargo xtask check`, which runs `src/user/system/linux/init/`'s formatting, clippy and tests by default and `src/lib/init/svc`'s with the host's, then `cargo xtask test-init --arch all`, which boots `/sbin/init` as pid 1 and types at the shell its getty gives |
| `src/user/system/linux/auth/`, or `tools/common/xtask/src/auth.rs` | The image row below, since `authd` goes into images, then `cargo xtask test-auth --arch all`, which boots init with `authd` and types each refusal of `docs/AUTH.md` at the getty; a change to what `authd` refuses also runs `test-auth --arch x86_64 --sabotage NAME` for the four names in `src/user/system/linux/auth/authd/src/sabotage.rs`, each of which must fail on the line it names |
| `src/lib/` only, and no crate the kernel builds | `cargo xtask check`, and one boot: `test-boot --arch armv7a --smp 2` |
| Anything the image contains: `src/kernel/`, `src/boot/common/uefi/`, a kernel-side crate in `src/lib/`, `xtask` | `cargo xtask check`, then `cargo xtask build --arch all --release`, since CI builds and boots the release profile and no other gate does, then `test-boot` on x86_64 (under KVM by default on an x86-64 Linux host since 2026-10-01, `docs/TEST-TIME.md` C3), aarch64, armv7a at four processors and armv7a at `--smp 2`; a stage 7 or 8 change also runs `test-shell` on x86_64 with no `--init`, which runs zinc, the image's shell, and then with the ferrousli busybox (`--init ferrousli`), the musl busybox *and* the host's glibc busybox (`/usr/bin/busybox`), and `test-vfs` on x86_64 with the ferrousli busybox and the musl one; a stage 7 change also runs `test-threads --arch all`, the Rust `std::thread` program (since 5fd2ab09); a change to the loader (`src/user/system/linux/ferrousli/ld`), to `exec` or to ferrousli also runs `test-shell` with Debian's dynamic busybox twice, on glibc's own `ld.so` and `libc.so.6` on all three architectures and on `--interpreter ferrousli --library ferrousli` on x86_64 (docs/ROADMAP.md, dynamic linking; `tools/common/fetch/fetch-debian-busybox.sh` fetches it). The whole of that, plus x86_64 under `--accel tcg` (or under `--accel kvm` where the plain boot was emulated, as in CI), is what moves `main` |
| User mode, page tables, TLB, SMP or the scheduler | The row above, and x86_64 under `--accel tcg` (the plain x86_64 boot is the KVM one where KVM is the default) |

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

**Landing on `main`.** There is one landing branch, `main`; `develop` has
not been used since 2026-09-15 and nothing lands on it. `main` moves only
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
  customer's word, given in the session that pushes.

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

Session names change on every restart. `ListAgents` shows what is alive; this
table is the roster of 2026-09-26, 18:00, from the fleet coordinator. At 19:15 the
customer cut the number of sessions running at once, because the host ran at
load 50 to 80 and every gate took one to two hours: a session marked **winding
down** finishes the task named in its row, lands it, files what is left as open
rows, removes its worktrees and target directories, and takes nothing new. Any
session that has finished everything it owns, winding down or not, reports to
the coordinator and then asks the customer in its own session, with a question
the customer has to answer ("ferrix-xx is done: ... May I be stopped?"), so
that a waiting question shows which session is finished; a question only the
customer can decide is asked the same way. A row
below whose owner is "open" has no live session; take it by putting your
session's name in its owner cell in your first landing.

| Session | Area |
|---|---|
| the customer | Owner: scope, priorities, decisions, what is stable enough for `main` |
| (empty) | Product owner (`AGENTS.md`). Held from 2026-10-01 16:40 by os-db, named os-0a after the 20:10 restart, which **wound down 2026-10-01 ~21:00**; its handover is `~/.local/share/ferrix/po-2026-10-01/HANDOVER.md` on the gate host. The one before was the gate host's bridge session of 2026-09-28 and 29 (`~/.local/share/ferrix/po-2026-09-29/HANDOVER.md`). The customer names the next one |
| (empty) | Certification consultant (`AGENTS.md`). Held from 2026-10-01 by os-ad, os-bd after the 20:10 restart, which closed at ~21:50; the customer names the next. Its ledger is `~/.local/share/ferrix/cert-consultant/reviews.md` |
| os-86 | The native channel round trip and speculation domains (`docs/OPAQUE-KERNEL.md` §9), as os-c7 before the 2026-10-01 restart. The domain landed as bf9efba95. **Wound down 2026-10-01**; its branches are under *Branches that still hold unlanded work* |
| (empty) | Verification auditor (`AGENTS.md`). Held by os-db on 2026-10-01 alongside the product owner's seat, dropped at the customer's word ("just be the po") during audit 2. Its ledger is `~/.local/share/ferrix/verification-audit/ledger.md`; read its last section, *Left for the next auditor*, first. Its rows are *Verification audit* below |
| ferrix-2c | Fleet coordinator: landing order, the landing lock, shared hot files, unblocking, pushes. Named ferrix-f6 after the 2026-09-27 restart; the fleet **wound down 2026-09-27** with everything finished on `main` and every unfinished branch pushed to GitHub (`docs/roadmap/where-it-stands.md`, *Where we left off*) |
| ferrix-15 | The init (`docs/INIT.md`), L13 parked; F-21b's audit record, closed 2026-09-27. **Wound down 2026-09-27**, handover in `~/.local/share/ferrix/ferrix-15/HANDOVER.md`. Parked: W-8 boot 21b on branch `boot-21b` (full gate green on cf13918f; left: ferrix-20's diff review, H.BOOT.15 is new, a rebase with carry and re-count, H.BOOT.14 as a parent of L.aarch64.44-46, the L.boot.35 control), then 21c (devmgr). Not committed: the devmgr=init coverage boot, one `Gate::new("test-init", "init", false)` line in `tools/common/xtask/src/coverage.rs`'s SUITE (passes under drcov on the Arm pair in 141 s and 132 s; needs ferrix-20's OK) |
| ferrix-55b | T0 of the live kernel update plan (cf265506, debe8998, 742fdaeb) and S0 of the opaque-kernel plan, both seam rows measured (d6974a66, c4c8b186); the plan is shelved by the customer (2026-09-27), so nothing further is owned here |
| ferrix-c7 | Chrome and `rustc` on ferrousli's loader (the customer's ask, 2026-09-26): landed d7b0709a and beffed20; last, the git and foot port reds (weak obstack, `random_r` declared); then **winding down**. ferrousli has had no owner since; last touched by ferrix-90 on 2026-09-27 for libpulse (`backtrace_symbols`, and a mutex inheriting priority refused with `ENOTSUP` as glibc does, since Ferrix has no PI futexes) |
| ferrix-41 | Stage 22's 32-bit x86 ABI (`docs/I386.md`): I1 to I4 on main, I5a (steamcmd logging in, `test-steamcmd`), and steamcmd in the `--everything` desktop's terminal. **Wound down 2026-09-27.** Parked: F-46 on branch `f46-power-off` (the change, plus a WIP commit with the `qemu.rs` `.get()` fix; controls run and logged in `~/ferrix-logs/f46`; the message needs correcting, then rebase, gate and ferrix-20's OK). The yserver feasibility pass was met on 2026-09-28 (`test-yserver`), and its design is `docs/YSERVER.md`. Y1 to Y4 and Y5a are done (`test-yserver`, `test-xwindow`, which sees xev's window on hyprix, gives it keys, clicks, the wheel and its cursor, has hyprix size and close it, and floats a transient xev as a dialog). Y5b, menus as popups, landed; so did Y7, yserver on `run-compositor --everything` (`test-xwindow --everything`). Y6, then I5b |
| ferrix-90 | **Test and gate run time**, the customer's priority one (`docs/TEST-TIME.md`; phase 1 measured, phase 2 the cuts, of which the Arm firmware waits and cut 2 are done, the latter by the PO session on 2026-09-28; handover `~/.local/share/ferrix/ferrix-90/HANDOVER.md`). Landed 2026-09-27: Chrome playing through `pulsed` on ferrousli, as the desktop runs it (0c55ae64); F-10's `syscall/native.rs` slice with F-42, a refused write's handles kept (288043a9), and `object/`'s (5792cc5c); W-8's claim and device requirements, part 20, with a function's bus mastering read back after a quiesce (dec60319); x86-64's coverage re-measured, 90.1%, floor 89.0 (11532464). Still owns audio (`docs/AUDIO.md`: L1-L7, U1 and U2 done; next U3, SDL and games finding what is missing by running) and `src/user/system/linux/media/`; `test-badapple`, the `badapple` app's gate since 2026-10-01, is in the gate of any slice that touches `pcm` or `resample` |
| ferrix-d5 | The Rust desktop clients (`docs/DESKTOP-CLIENTS.md`: the clients-base crates, waybar, fuzzel, hyprlock, hypridle), the EDID override, `docs/AUTH.md` and its phase 1; at most three streams running at once |
| ferrix-55 | Standing certification consultant (reviews item changes before they land; keeps `docs/certification/` current). **Wound down 2026-09-27 ~15:30.** Its open review queue -- F-46 parked, F-48's diff, F-49's design, W-8 21b's diff and 21c, ferrix-90's VERIFICATION §3 note, the Arm firmware-wait change -- with every condition already set, is at the end of `~/.local/share/ferrix/cert-consultant/reviews.md`, and the handover is `~/.local/share/ferrix/cert-consultant/HANDOVER.md` |
| ferrix-e1 | The repository relayout (`docs/LAYOUT.md`), landed 2026-09-26 as 26303ad5. **Winding down** after its post-landing rows and cleanup |
| ferrix-9c | The Pixel 7's USB CDC-ACM device driver (`docs/vendor/google/pixel7/USB-HANDOVER.md`): the phone showed up as `/dev/ttyACM0` on example on 2026-09-26. **Winding down** once `usbdev` with the kernel log channel has landed |
| ferrix-d4 | The Pixel 7's GUI, option A: a desktop in the launcher app's crosvm VM, with Chromium on it; landed 2026-09-27. **Winding down** after that landing |
| ferrix-8e | This file's cleanup (landed 6b942df1); two_clients' GPU-frame flake. **Winding down** after that |
| ferrix-b0 | Bad Apple!! with sound (`docs/MEDIA.md`, the customer's order of 2026-09-26), landed; now Bad Apple on the `--everything` desktop (a toolkit window, `SUPER M`), then stopping; Doom is in the backlog, unowned |
| open | The Pixel 7 bring-up (`src/boot/vendor/google/pixel7`, statd, `tools/vendor/google/pixel7`; was ferrix-0a); Chrome's extensions bubble and `test-chrome-window`'s context-menu step (was ferrix-a8); every row below owned by an `os-*` session before 2026-09-26 |

---

## Stage 13: where Ferrix differs from Linux

Each row is a difference from Linux that a stage 13 landing knowingly left, with
what closing it takes. Rows are added by the landing that leaves the
difference, and removed by the one that closes it. A difference that is
stricter than Linux and deliberate says so; it stays a row because a program
may one day need the relaxation.

| Item | Owner | Stage |
|---|---|---|
| **Set-id bits are ignored in any child user namespace** (U7), where Linux honours an owner mapped in the caller's namespace. `sudo`, `su`, `newgrp` and a set-group-id `ssh-agent` run inside a container as their caller. Deliberate (customer, 2026-09-28; consultant). Relax by `bprm_fill_uid` with `kuid_has_mapping`, on a mount without `nosuid` and without `PR_SET_NO_NEW_PRIVS`, as its own landing with a boot check (`docs/NAMESPACES.md` §4) | open, relax on demand | 13 |
| **A child user namespace honours five capabilities** (`CAP_SETUID`, `CAP_SETGID`, `CAP_SETPCAP`, `CAP_SYS_CHROOT`, `CAP_SYS_ADMIN`), where Linux honours the rest over files whose owner is mapped: `CAP_DAC_OVERRIDE`, `CAP_FOWNER`, `CAP_CHOWN`, `CAP_KILL`, `CAP_MKNOD` do nothing there (U8). `unshare -r` then `chown`, rootless podman and buildah do not work. Deliberate. Relax one capability at a time by `capable_wrt_inode_uidgid` (both uid and gid mapped), never `CAP_MKNOD` | open, relax on demand | 13 |
| `CLONE_NEWNS` by a holder of `CAP_SYS_ADMIN` in a child user namespace is allowed: it copies, and every mount, unmount and remount beneath still needs the mount namespace owner's capability (N5). Linux is the same; the row is here so a later change to `Namespace::copy_as` does not assume otherwise | open | 13 |
| `verify_root_map` (Linux's `CAP_SETFCAP` to map root) is not built: Ferrix has no file capabilities. If it ever has, `userns::write_map` needs the check | open, with file capabilities | 13 |
| The audit record never carries a uid: `Subject::of` answers `NO_UID`. `userns_check` holds the subject to the kernel's id or none, so the day a personality supplies one it fails on the inside id. The supplier and its record-level check are not built | open | 13, 15 |
| **A boot check cannot run a real set-id `execve`, or a process that shares its fs context or has several threads**, so three rules stand on code and on `Credentials`/`namespaces_asked` checks: a set-uid file under a namespace (`Credentials::exec`), `unshare(CLONE_NEWUSER)` with a shared fs context, and the multithread `EINVAL` of `unshare` and `setns`. A gate that runs a program for each (`test-container`'s family) closes all three | open | 13 |
| `max_mnt_namespaces` is not a sysctl and mount namespaces are not counted; F-37's fill bounds them by the job's memory, and `mount-max` bounds one namespace's mounts. Linux has a per-user count. `max_user_namespaces` is a global 4096 in N5, not per user | open | 13 |
| N5's plain `MS_REMOUNT` rule approximates Linux's superblock owner by "every mount of the filesystem is in the caller's namespace" (`Namespace::sole_filesystem`). Linux asks `CAP_SYS_ADMIN` over the user namespace that made the superblock, so a child can still remount read-only a tmpfs it made and then bound into a namespace it shares. Close by recording the creating user namespace on `Superblock` | open | 13 |
| N5's detach of another namespace's mounts on a removed name (`detach_elsewhere`) takes each other table's lock but not that namespace's change lock, so a mount made in that namespace at that instant can race it. Linux takes `namespace_sem` for all. Close by giving `Tree` a handle to its namespace's change lock | open | 13 |
| Mount locks (M3, M4) are set by `Namespace::copy_as` alone. `MS_MOVE`, `open_tree`, `fsopen` and `mount_setattr` are not built (`EINVAL`/`ENOSYS`); whichever is built first must honour the locks and set them where a mount crosses into a less privileged namespace | open, with the new mount API | 13 |
| **M5 (a bind's source must be in the caller's namespace) has no boot check**: `Namespace::owns` gives it, and a descriptor kept across an `unshare` is the test to build, with its control | open | 13 |
| A device node on a user `tmpfs` (`nodev` forced) has no check that the open is refused | open | 13 |
| `/proc`'s private links (`root`, `cwd`, `exe`, `fd`, `fdinfo`, `maps`, `ns/*`) ask `ptrace_may_access`. `mountinfo` and `mounts` do not, as on Linux (the design's M8 listed `mountinfo`; the landing follows Linux, the certification consultant's review, 2026-10-01). `environ`, `stat` and `io` are not guarded. `/proc/<pid>/mem` does not exist | open | 13 |
| `CAP_SYS_PTRACE` is not among the capabilities a child user namespace honours (`userns::HONOURED`), so root inside a namespace cannot read the `/proc` private entries of a process its namespace owns that has another uid or is not dumpable, where Linux lets it (NP, 2026-10-01; stricter than Linux) | open | 13 |
| `/proc/<pid>/stat` gives another process's `start_stack`, `start_brk` and the other address fields that Linux zeroes for a reader without `PTRACE_MODE_READ` (NP's review, 2026-10-01): an address-layout leak against a set-id target's ASLR | open | 13 |
| `/proc/<pid>/fdinfo/<n>`'s `flags:` carries the access mode alone; Linux adds `O_CLOEXEC`, `O_APPEND`, `O_NONBLOCK` and the other file status flags (NP's review, 2026-10-01) | open | 13 |
| `userns::acting` answers a context with no current task (a `sched::current_id()` of `None` matches a setting made with `None`) the process a check set from another such context, e.g. a second processor's boot thread; today only the boot checks set it, one at a time (NP's review, 2026-10-01) | open | 13 |
| A set-id `execve` clearing dumpable stands on code (`exec.rs`); the boot harness cannot run a real set-id file. Linux also clears it when the file is not readable by the caller | open | 13 |
| **Seccomp** (`docs/SECCOMP.md`): `TRACE` and `USER_NOTIF` answer `ENOSYS` (no tracer, no supervisor); `SECCOMP_FILTER_FLAG_NEW_LISTENER` and `WAIT_KILLABLE_RECV` are `EINVAL`; `SECCOMP_FILTER_FLAG_LOG` and the `LOG` action write one rate-limited line (pid, `arch`, number, instruction) on the console and not Linux's audit log (Q4: `audit.rs` records the item's decisions, a filter's is the personality's); `SPEC_ALLOW` is accepted and changes nothing; `TRAP` is a kill until S4 and `TSYNC` is `EINVAL` until S5. User notification would need an fd type and a supervisor protocol | open, S4 and S5 | 13 |
| Seccomp: x86-64 `uretprobe` (335) and `uprobe` (336) pass through Linux's filter unfiltered; Ferrix, with no probes, lets a filter judge them like any unknown number, which is stricter and safe. Recorded in `tests/chrome.rs` | open, deliberate | 13 |
| Seccomp, SR2: a call whose number register has bits above the 32nd set is judged by a filter as its low half and dispatched as no call (`ENOSYS`), where Linux masks the number to its low half and runs that call. Stricter and deliberate: a number is judged as the value dispatched or not at all. Relax by masking in each entry, which makes the filter and the dispatcher agree on the low half | open, deliberate | 13 |
| Seccomp, S2: a call in the native range carries the `arch` token `0xC000_0F1F`, which Linux never uses, and it has the 64-bit flag on ARMv7-A too, where no native register is 64 bits wide. A filter that allows the native token names that value on every architecture | open, deliberate | 13 |
| Seccomp, S2: a thread with no filter pays the `Once` load and one load of the boot check's probe word, not only the `Once` load `docs/SECCOMP.md` §3.3 prices. The probe is a test seam that shows what a filter was shown, which a filter program cannot say; remove it by moving the check to filter programs and a ring-3 reader | open, deliberate | 13 |
| Seccomp's design keeps the filter per thread with `TSYNC` as Linux has it (`docs/SECCOMP.md` §3.4); `KILL_THREAD` ends the process, by `SIGSYS`, when the thread is the process's last live one, and only the thread otherwise, as Linux does; no boot check has a process of several started threads, so the several-thread case stands on the code until S6's guest program kills one thread among several | open, S6 | 13 |
| Seccomp, S3: `execve` keeps a thread's filter by never touching the thread's state, and no boot check runs an `execve` (the check processes are never started); S6's guest program runs bubblewrap's path, `prctl(PR_SET_SECCOMP, 2, &prog)` then `execve` of a program that finds itself filtered | open, S6 | 13 |
| Seccomp, S3: releasing the longest chain a program can build (6,554 filters) takes 8.1 ms in the guest, where the last reference goes: in production the reaper, with preemption off. Close by handing a long chain to a deferred work item of the reaper's own, a few filters at a time | open | 13 |
| Seccomp, S3: the `LOG` rate limit is one for the machine, not one for each process, so one process in a loop under a logging filter can use up the lines another's kill would write; Linux's audit log has its own ratelimit. The boot's own checks set the machine-wide "a thread has held a filter" flag, so on a booted machine with checks the no-filter fast path is the flag's load and the thread look, not the flag alone | open | 13 |
| Seccomp, S3: a filtered thread's calls look for the running thread (`sched::current`, which takes the run queue's lock) once any thread in the machine has held a filter; Linux reads `current`. The flag that no thread has ever held one keeps this off every other machine; a per-processor cache of the running thread would take it off the rest | open | 13 |
| **UTS, IPC and cgroup namespaces belong to the process, not the thread** (`syscall/nsproxy.rs`). `clone(CLONE_THREAD)` with one of the three flags, and `unshare` or `setns` into one from a multithreaded process, are `EINVAL`; Linux gives a thread its own. Close by moving the proxy onto `Thread` | open | 13 |
| `setns` into a mount or user namespace is `EINVAL` while the fs context is shared (`CLONE_FS`), where Linux copies the struct. A `Process` holds its context for life; closing it means letting a process swap its context | open | 13 |
| Opening a `/proc/<pid>/ns/*` link asks the same user, or root in the first namespace, and not dumpability (Linux asks `ptrace_may_access`). Fold into NP's `credentials::may_access` | open | 13 |
| **Kernel root keeps its file override in a user namespace it made**: `Access::privileged` is `uid == 0` with no namespace in it, so root of the first namespace reads a 0600 file of an id the namespace does not map, where Linux (`capable_wrt_inode_uidgid`) refuses it. U8 holds for a process whose kernel uid is not 0; `userns_check.rs`'s `kernel_root_keeps_override` records it. Close by making `Access` carry its namespace's map | open | 13 |
| **A pidfd of an exited process may still be joined by `setns`** where Linux answers `ESRCH`; the namespaces it names are held by the pidfd's process object. Close by refusing a pidfd whose process has ended | open | 13 |
| nsfs: `NS_GET_MNTNS_ID`, `NS_GET_ID` and the `NS_MNT_GET_*` ioctls answer `ENOTTY`; there is no per-namespace `/proc/sys/kernel/sem` and no `/proc/sysvipc/sem` at all | open | 13 |
| The hostname sysctl is judged by the file's mode only, where Linux also asks `CAP_SYS_ADMIN` over the UTS namespace's owner at the write; the syscall does | open | 13 |
| Two boot controls of the small namespaces show an older check's message, not their own (`nameable` letting everyone in fires the `userns` line first), one control's message only roughly matches its rule, and the namespace files' `kmem` fill has no control (the detached mount's charge bounds it). The pidfd `setns` all-or-none property stands on the code's shape | open | 13 |
| The composite `CLONE_NEWUSER\|CLONE_NEWIPC\|CLONE_NEWCGROUP` has no end-to-end check; `test-container` (`src/tests/container`) is the place, once pid namespaces land | open | 13 |
| The namespaces' numbers: user from 0xF800_0000, UTS, IPC and cgroup from 0xF900_0000, mount from 0xF000_0000, where Linux draws from one counter. Nothing compares across kinds; a program that does sees them apart | open | 13 |
| **A network namespace belongs to a process, not a thread**: `CLONE_NEWNET` with `CLONE_THREAD`, and `unshare` from a multithreaded process, are `EINVAL` (the latter by one line, not boot-checked). Close with the other namespaces' move onto `Thread` | open | 13 |
| A moved network interface gets the next free index, and a clashing name is `EEXIST`; Linux keeps the index when free and renames to `dev%d`. Coming home to the first namespace renames to `devN` on a clash | open | 13 |
| Network namespaces: reassembly is capped at 32 KiB outside the first namespace; the ceilings (64 interfaces, 256 addresses, 1024 routes) also bind the first one; only `veth` exists (`dummy`, `bridge`, `macvlan`: `EOPNOTSUPP`) with a fixed MTU of 1500 and no notifications; `RTM_NEWNSID`, `RTM_GETNSID`, `IFLA_LINK_NETNSID` and `IFLA_TARGET_NETNSID` are not built; there is no `/proc/<pid>/net` | open | 13 |
| Nothing forwards or translates between two interfaces of one network namespace: the stack is a host, so a bridge or NAT between containers needs IP forwarding, which it has none of | open | 13 |
| **No limit on the number of network namespaces**: each is charged to its job, and the net task ticks every one that exists, so a user who may make namespaces (any user, through a user namespace) makes the tick slower without bound but their job's memory. Add `max_net_namespaces` like N5's other sysctls (global or per user), with a boot check and control (consultant's F3) | open | 13 |
| The neighbour cache (256 entries x 3 queued packets, about 1.1 MB per namespace) and the reassembler (32 KiB) grow on traffic and are charged to the job only at the namespace's next change (`fit`), against NETNS section 5's "charged as it fills". Close by charging on fill or per tick; until then the per-namespace bound is the limit (consultant's C2) | open | 13 |
| `setns(CLONE_NEWNET)` is not wired: `net/netns_file.rs` is the seam, to become the network arm of the namespace files in `fs/nsfs.rs` | open | 13 |
| Not boot-checked for network namespaces: the `/sys/class/net` listing per reader, IPv6 over a veth pair, ARP growth past the table limit, `unshare` from several threads | open | 13 |
| Raw and packet sockets are reachable by a user who owns a network namespace (`CAP_NET_RAW` over an owned namespace is honoured): safe Rust with no ring-buffer interface, recorded as a residual in the vulnerability analysis | open, accepted | 13 |
| **Pid namespaces** (`docs/PIDNS.md` §8): one machine-wide pool of 32768 kernel numbers with each namespace also numbering to 32768 locally, `pid_max` neither writable nor per namespace; an init's namespace is killed when its last thread ends, not after it reaped its children; only `SIGKILL` and `SIGSTOP` from an ancestor are forced through to a protected init, and faults bypass the protection; procfs file contents tell numbers for the reading process, not the instance's namespace; `setns` into a pid namespace and `clone3`'s `set_tid` are not built | open | 13 |
| **Pid namespaces, from the consultant's review (2026-10-01):** `si_pid` and `SO_PEERCRED` are translated when read (`pidns::show_pid`), not stamped when sent, so a sender that has ended reads 0 and a reused kernel number reads as its new owner; `process_give` takes machine pids, and its `NO_PROCESS` against `NOT_CHILD` tells a native caller in a namespace whether a machine pid exists; `PidNamespace` has no owning user namespace (harmless until `setns` into a pid namespace is built) | open | 13 |
| **A frame budget for the fork path:** nothing stops `sys_clone` (3,712 bytes), `Process::with_context` (1,592), `shared_with_context` (3,728, transient), `forked_into` (600) and `fork_into` (80) growing again on a four-page kernel stack; `xtask check` could read the `sub rsp` of each from the built kernel with objdump, as the assembly budget counts lines, and fail past a budget per function and per path (a double fault in `test-vfs` on x86-64 was this, 2026-10-01; `docs/PIDNS.md` §10) | open | 13 |
| Time namespaces and the cgroup controllers (M2's reclaim, `cgroup.freeze`, `cpu.max`, `io`): their own differences are added here when those landings report | open | 13 |

## Red on `main`

The customer's order of everything puts a working `main` first. These are
gates that fail on `main` itself, not flakes, and come before any row below.

| Item | Owner |
|---|---|
| **Done 2026-10-05, landed as a6116e822 (po6-g3), post-landing KVM boot PASSED:** FX-1012, x86-64 stage 10's checks G3 (0xF5, "a byte the port received while its line was masked was not read by the service after the conversion") and R5 (0xF6) raced the `console` pump thread, which drains the receive ring every 20 ms and echoed the check byte before the check took it from the ring; the echo (not UTF-8) ended xtask's line reader and showed as "QEMU terminating on signal 15" (the xtask half: `po5/sigterm`). On `main` 39e520e31, 3 of 61 x86-64 KVM test-boots four at once (2 G3, 1 R5; `~/.local/share/ferrix/logs/po6-g3/main/runs.txt`). Fix: the receive path, armed by the check (`console::input::arm_check_byte`), takes the check byte once and keeps it out of the ring, and G3 and R5 require that it took it (`disarm_check_byte`), so they observe the service's read rather than the ring and no reader sees the byte. On the fix, 0 of 240 the same way, 240 passed (`logs/po6-g3/fix/runs.txt`). Controls: the conversion's service dropped, G3 fires (`po6-g3-ctl-noservice`); the console's entry not present, R5 fires (`po6-g3-ctl-r5-absent`); R5's wait cut, xtask's positive-line check fires (`po6-g3-ctl-r5-unobs`) | po6-g3 |
| **Done 2026-10-05, landed as 39e520e31 (po5-gw's branch, landed by po6):** `a_lost_segment_is_sent_again_alone` passed on CI's first run on 39e520e31 (37331100176); that run's one red test was another, timing-dependent one, now a P1 flake row. Was: CI's `Test (windows-latest)` was red on `main` since at least 67efb9fb1 (2026-10-04): `gateway::tests::a_lost_segment_is_sent_again_alone` (`tools/common/xtask/src/gateway/tests.rs`) failed with "268436529+536 came again (sent up to 268448857)" on most runs (37234989867 and six more) and "the segment sent again is the one asked for" (left 268448321, segment 24, on 37229234877 and 37231319281). Not Windows-only and not the gateway: the test raced the gateway's 20 ms retransmission timer, which starts at the gateway's last send or ACK, before the test's window began. The test drained for 5 ms of quiet after its first ACK (a 15.6 ms tick on Windows); with any scheduling delay on top the timer ran out before the third duplicate reached the gateway, which then, correctly, sent everything in flight again. Reproduced on nazuna with the receive timeout rounded to Windows' tick (an `LD_PRELOAD` shim) and ten busy loops on two CPUs: the old test failed 16 of 100, and an instrumented build printed the timer's rewind 20 to 28 ms after the ACK and before the third duplicate in every failure. The fix gives the gateway a per-instance timer (`Gateway::start_retransmitting`, `tcp::RETRANSMIT` for every caller but this test) and runs the test with it never firing, which makes it stricter: every segment is accounted for, and only the lost one may come twice. New: 200 of 200 under the same load, 100 of 100 alone; a gateway resending everything on three duplicates fails it with the CI message, one ignoring them fails "the lost segment comes again". Shim, loop script and logs: `~/.local/share/ferrix/logs/po5-gw/` | po5-gw |
| `test-init --arch all` failed on `main` three ways, and a fourth on the fix's own first tip, read as load in the batches of 2026-10-04 and 2026-10-05; each is the test's or xtask's, root-caused 2026-10-05 by po5-red. (1) AArch64, "the revoke stage's reader was not waiting (state R)" (`~/.local/share/ferrix/logs/queue/batch-20261004T190141Z-b0-2.log`, cb872a732): the guest's wait looked at the reader's `stat` until it said `S`, then looked again for the line it echoed. A console read sleeps 2 ms at a time between looks for a keystroke (`fs::terminal::POLL_NANOS`), so a reader waiting in it reads `R` while it waits its turn after each one: 4 looks in 300 at load 6 (`po5-red-diag-rlooks-a64.log`), more under load. The same stage missed a real revoke when the new shell's prompt came first on the line holding `cat`'s EIO. (2) AArch64, "`login plain` did not log plain in", then `/dev/tty` and "revoke reader never started" after it (`batch-20261004T190141Z-5.log`, 99c5166cd; the consultant's ledger line 339 saw it too): the answers were typed blind 2 s apart, and the password prompt flushes typed-ahead input (`TCSAFLUSH`), so one was lost and the next prompt took the gate's next command. (3) x86-64, QEMU stopped 7.5 s into a boot (`batch-20261005T114127Z-b0-3.log`, main 3c08d5657; its "Login incorrect" is the stage's own wrong guess on AArch64): xtask's serial reader ended at the first byte that was not UTF-8, and the watch took that for the guest closing the port. **Fixed on `po6/red` (from `po5/red-main`):** the reader judged on the one look that ended the wait, bounded at 500; the EIO looked for anywhere in a line; the reader decodes lossily (a host test and its control). (4) x86-64 only, on po5/red-main's first tip ac6701e56, "su with ferrix's password did not give root in every id" (`po5-red-tip-init-all-1.log`): that tip typed each answer after a marker line and again every 3 s, before the prompt; QEMU's serial ports take 16 bytes at a time, so `chosen at the console` came in two, `su`'s `TCSAFLUSH` threw away the first 13 bytes (the echo shows `chosen at the` before `Password: `) and `su` read ` console` as the password. **Fixed on `po6/red` (po6-red):** an answer is typed once, when its prompt is the console's unfinished line (xtask now keeps the line the guest has not ended, `Watching::unfinished_line`, host test `a_prompt_with_no_newline_is_the_unfinished_line`), and `ferrix-auth-client` turns the echo off and flushes before it shows the prompt, as `getpass` does, so what is typed after the prompt is all read. Why the guest printed a byte that is not UTF-8 there is open: on a good boot the next line is the conversion of the console's own interrupt line to remapping (6df835956) | po5-red |
| Stage 5's moving lock check panicked "the moving lock check's tasks never moved between processors" in `test-foot --arch x86_64` under KVM on main 77783565a (`~/.local/share/ferrix/logs/queue/batch-20261004T194624Z-b0-1.log`, slot 1, load 10 to 13), "8 tasks x 100000 lock pairs, 0 moves, 52 ms"; one in about 9,900 recorded runs, every other with 17 or more moves. **The scheduler, root-caused 2026-10-05 by po5-red.** An idle processor stole the only waiting task of another idle processor that had been handed it and kicked but not yet woken: the checker's processor, idle as soon as the checker slept after spawning, took all seven tasks placed elsewhere within 0.1 ms, before any had run; the robbed processors woke to empty queues, halted, and nothing woke them again; eight tasks that sleep 50 us in every 60 are less than one processor's work, so neither balancing rule saw anything to move. Reproduced by starving virtual processors 0 and 3 of host time (two vCPU threads on one host core beside two busy loops) with the check run 400 times a boot: 7 rounds of 400 with no move, every task first run on processor 0. **Fixed on `po6/red` (from `po5/red-main`):** an idle victim keeps its last waiting task (`sched::steal_from`): 0 rounds in about 370 the same way. The check is unchanged | po5-red |
| H.SCHED.1 ("every runnable task runs") now rests on a rule no low-level requirement states: a task put on the queue of a processor running its idle task always kicks it (`enqueue`, `kick_after_wake`, `balance`'s push, `pull`), since po5/red-main's `steal_from` no longer lets a neighbour take an idle processor's last waiting task, which used to cover a lost kick. Write the requirement and its check (the certification consultant's advisory, ledger 366, 2026-10-05) | open |
| **Done 2026-09-30 (os-98):** `test-compositor --arch x86_64` under KVM failed in its submap boot (709072 of 786432 pixels off after `L`), on `main` and alone, about one run in two. Not the layout: `movefocus l` and `movewindow r` both did their work. hyprix's loop read `animating` before the frame, and the move's animation only starts inside the frame, when `Animations::follow` meets the new place; so the frame drew the window where it started, and the loop slept with no frame owed until some other input woke it. A frame now keeps the loop drawing while an animation it started is busy (`src/user/system/linux/compositor/hyprix/src/state.rs`). Ten runs of `--boot submap` under KVM passed at loads of 6 to 13; the negative control, `main` without it, failed the first run with the row's message. | os-98 |
| x86-64 stage 10 panics with FX-1012 in about one x86-64 test-boot in 27 (os7c's night, four at once), 1 in 160 (po5-sig, four at once) and none in 40 run alone, "a byte the port received while its line was masked was not read by the service after the conversion" (`src/kernel/src/iommu/check.rs` G3), on `main` 209f037c4 and every tip since N0g (6df835956). This is the "QEMU terminating on signal 15 from pid N" that failed `test-shell --arch all --init ferrousli` six times (batch 20261004T190141Z b0-1, 20261004T175923Z b1-8 and b2-7, 20261004T194624Z-14, 20261004T135748Z-2), `test-shell --init …/busybox.static --arch x86_64` in batch 20261005T114127Z-b0-2 on 3c08d5657 (6.6 s, same last line, same empty-named sender), and 340 of os7c's 9234 night boots (`~/.local/share/ferrix/logs/os7c-night/SUMMARY.txt`); 330 of the 342 such kills in `logs/queue` stop right after "iommu    no vector minted in compatibility format". **Who sent the SIGTERM: xtask itself** (po5-sig, 2026-10-05). G3 loops 0xF5 into the console's port while its line is masked for the conversion; when the `console` pump thread (`fs/terminal.rs` `run_pump`, every 20 ms once anything has read the console) drains the ring before `require_masked_byte_served` takes the byte, the line discipline echoes 0xF5 to the serial port and the boot panics. 0xF5 is not UTF-8, so xtask's `read_lines` (`BufRead::lines()`) ended one line before the panic; the watcher took the closed channel for QEMU's exit, sent `kill -s TERM` (`qemu.rs` `ask_to_stop`) and reported "QEMU exited (exit status: 0) ... before the guest printed". Evidence in `~/ferrix-logs/po5-sig/`: `direct-3-7.bin`, the raw serial of a plain QEMU boot of 209f037c4's image (one of 160 at four in parallel) holding `...1 I/O APIC inputs converted\r\n\xf5\r\nFERRIX-PANIC a byte the port received...`; and a QEMU wrapper (`wrap.c`, SIGTERM sender by `siginfo`) that, with 0xF5 injected after that line, names the sender `comm [kill]` whose parent is `target-po5-sig/debug/xtask test-boot --arch x86_64` (`inject-main.log`, `sig.log`). **xtask half fixed** on branch `po6/red` (e11e8b2f5's reader, kept by po6-red; `po5/sigterm`'s ee2cd25f3 was the same fix): lines are read as bytes, so the same injection boots to `FERRIX-BOOT-OK` (`inject-fixed.log`) and a real G3 failure now reports its FX-1012 panic. **Next, the kernel half:** G3 (and R5's 0xF6 in `check_console_line`, which takes the same ring) must not race the pump thread -- for example hold the terminal's pump off, or count the check byte in the service instead of looking for it in the ring; a certified-item change, so the consultant's OK first | open (N0g's, interrupt remapping) |

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
| **N4 (d171ffe5): four of its ten negative controls have no log anywhere**: the range test narrowed, the writer's check, the chroot test, and set-id honoured in a namespace. Five more are in `logs/os7c/control-n4-*.log`, on the earlier tip 5bc2affc. The tenth, `privileged()` ignoring the namespace, was re-run by the certification consultant on be17d882 (`queue/osad-ctl-n4-privileged.log`: "kernel root inside a namespace it made set the host name"). Run each of the four on `main` with `gate.sh control --expect`, and keep the logs. No armv7a boot at four processors and no `build --arch all --release` ran on any N4 tip | os-03 (stage 13; was os-7c, os-79) |
| **318987db (programs under `src/user/system`): rows that ran only on a pre-rebase work in progress**. ferrousli's 770 debug and 770 release tests, and `test-shell`, `test-apps`, `test-init` and `test-badapple`, ran on bbe00b25 over 9da6c78d. The rebase then took `main`'s changes to moved files (`ferrousli/src/pthread.rs`, `tests/c_linux.rs`, the drivers, xtask's compositor). Re-run them on `main`. `docs/APPS.md` §10 says the rename was "gated by the full `check`", but that check, run with ferrousli, failed at `c_mount`'s 30-second limit; the line should say so  2026-10-05 (po5-c1): `test-shell` (zinc and ferrousli) and `test-init --arch all` ran on `main` since, PASSED (batch 20261004T194624Z on f55e8ab28 (on `main`, after it; `~/.local/share/ferrix/logs/queue/batch-20261004T194624Z-N.log`), rows -1, -5; `os7c-po-four-shell`); ferrousli's own tests (`check --ferrousli`), `test-apps --arch all` and `test-badapple` have not. `docs/APPS.md` went to the apps repository on 2026-10-03, so its sentence is that repository's to correct | open (landed by os-3c) |
| **c5c92781 (F-55): its one behavioural claim ran only before its rebase.** `test-xwindow` (`f55-land/xwindow.log`), the armv7a boot at four processors and the release build all ran over caa9e3ea. The rebase to 9d56ea93 took `main`'s change to `panic/catalog.rs` and the generated files. Re-run `test-xwindow` and the armv7a boot on `main`  The armv7a boot at four processors ran on `main` since, PASSED (batch 20261004T194624Z on f55e8ab28 (on `main`, after it; `~/.local/share/ferrix/logs/queue/batch-20261004T194624Z-N.log`), row -18). `test-xwindow` has not: still owed (2026-10-05, po5-c1) | open (landed by os-fd) |
| **1dcc433f (MSI-X storm bound): its release build and its x86_64 boot under TCG ran only before its rebase** (`os35-wake/A-release.log`, `A-boot-x86_64.log`). The rebase took `main`'s changes to `device.rs` and `stages_check.rs`, both files it touches; the four boots after it were KVM and Arm. Its review, recorded only by its author at landing, was confirmed by the certification consultant on 2026-10-01, and both negative controls are logged (`os35-wake/nc-a-{lost,storm}.log`). Run `build --arch all --release` and `test-boot --arch x86_64` on `main`  **Closed 2026-10-05 (po5-c1):** both ran on `main` since, PASSED: `build --arch all --release` and `test-boot --arch x86_64 --accel tcg` in batch 20261004T194624Z on f55e8ab28 (on `main`, after it; `~/.local/share/ferrix/logs/queue/batch-20261004T194624Z-N.log`), rows -2 and -16; again on c19aeefcb (`po5-docs-build`, `po5-docs-boot-x86-tcg`) | closed |
| **1384e6e6 (N3) landed outside the landing lock**, by a plain push, and its busybox `test-shell` rows (ferrousli, musl, glibc), both `test-vfs` rows and `test-init --arch all` ran only on 992fc6e6, before the amend that changed `family.rs` and `fd.rs` on the clone and `openat` paths. Re-run them on `main`. The namespaces paragraph under *Waiting on the customer* still ends at N2  2026-10-05 (po5-c1): `test-init --arch all` and ferrousli's `test-shell` and `test-vfs` (`--arch all`) ran on `main` since, PASSED (batch 20261004T194624Z on f55e8ab28 (on `main`, after it; `~/.local/share/ferrix/logs/queue/batch-20261004T194624Z-N.log`), rows -5, -1, -4); the musl and glibc busybox `test-shell` and the musl `test-vfs` have not | open (landed by os-98) |
| **cc4e14af (the discovery Finder) claims a coverage carry it does not contain**: "Coverage: carried from caa9e3ea", but the commit has no evidence file. The carry landed a minute later in 65639967, from d86821bb. Its "five arguments" are seven, and two line numbers were off by one; da45a113 corrected both in `docs/certification/TODO.md`. Its boots were not re-run after the rebase over d86821bb, which changed five kernel files in comments and path strings only | open (landed with the Finder) |
| **d5896c75: its boots ran only before a rebase that changed `main.rs`**, with only `check` re-run after it. `docs/roadmap/stage-10-userspace-drivers.md` credits it with "ACPI or the device tree, decided once", which is fa4e88a6. The same roadmap's "os-9f reviewed each step" for the Finder series is contradicted by da45a113, which records them as reviewed after the fact | open (landed with the Finder) |
| **Done 2026-10-05 (po5-c1):** `render/**` in the load ring of `tools/common/data/certification-item.json` matched no file (c21abe11 meant to replace all seven interface cores' patterns with `interfaces/**`, and replaced six). It is gone, and `check-item-boundary.py` now refuses any ring member that matches no kernel file, with a self-test case; controls in `~/.local/share/ferrix/logs/po5-c1/ctl-ib-*.log` | done |
| **Ten item landings on 2026-09-30 moved `main` outside the landing lock**: 1384e6e6; the discovery series c21abe11, aa78061e, f8ab2970, d5896c75, fa4e88a6, c2477dc1, cc4e14af and 2be909b3; 318987db, which went in inside another session's 39-second take; and bce1fc14 and 8f7801de. The lock log's `main` field reads the gate host's `main`, which moves only when a lander pushes it there, so its hashes do not show what landed on the Windows checkout | the product owner |
| **37 code and xtask landings since 2026-09-30 name no gate in their message**, among them ferrousli's loader stack, whose row in the gate table asks for Debian's dynamic busybox twice. Nothing required the gates in the message until `AGENTS.md` (2026-10-01). Those loader landings are the next audit; a first run was stopped before it reported. Mutation testing (`cargo mutants`, installed on the gate host) ran on four crates at 79826d1c: 23 missed of 491, untriaged (seccomp 9, objects 6, kmem 5, crng 3; `logs/os-db-mutants`, triage notes in the ledger); nine item crates not run | open (the auditor's seat is empty) |
| **Closed 2026-10-01: 278a2f9d (`pkg`) landed outside the landing lock** at 18:24, while os7c-s2 held it; its `land.sh take` was piped through `tail`, which hid the refusal. S2 (ff5e45ef) landed on top; the two share no file. The combined tip was gated afterwards: `check`, `test-pkg --arch x86_64` and a KVM boot of ff5e45ef, all PASSED (`logs/queue/os3c-ff5e-*.log`, fleet log 18:33:38). Run `land.sh take` on its own line and test its exit before any push | closed |
| **bf9efba95 (speculation domains) landed ahead of its full gate**, on the customer's word (Decisions, 2026-10-01). Logs are `~/.local/share/ferrix/logs/queue/<tag>.log` on the gate host, and `fleet/gate.sh`'s INDEX holds each verdict. At the wind-down these were in: all eight controls, `osc7-sd4-c1` to `-c8`, `FIRED (panic)`; `osc7-sd4-check`, `-build`, `-boot-x86-kvm`, `-boot-x86` (which ran under KVM, not TCG), `-boot-a64` and `-boot-a32` PASSED on 96f0d28c8, the tree before the rebase; `osc7-sd5-check` PASSED on 9c04ac274; and `osc7-land-check`, `-boot-kvm`, `-boot-tcg` and `-boot-a64` PASSED on bf9efba95 itself. Still owed, queued by `~/ferrix-logs/osc7-sd4-row.sh` and still running on the gate host at the wind-down: `osc7-sd4-boot-a32-2`, `-shell`, `-threads`, `-vfs-ferrousli` and `-vfs-musl`. A control reading DID NOT FIRE, or any row failing, is red on `main`: tell the product owner and the consultant, and fix forward. 2026-10-05 (po5-c1): of the five, `-boot-a32-2`, `-shell`, `-threads` and `-vfs-ferrousli` ran on `main` since, PASSED (batch 20261004T194624Z on f55e8ab28 (on `main`, after it; `~/.local/share/ferrix/logs/queue/batch-20261004T194624Z-N.log`), rows -15, -6, -4; `os7c-po-four-shell`); `-vfs-musl` has not | the product owner |

## The path to the goal, in order

The first goal, `rustc` on Ferrix (stage 16), was met on 2026-09-22. The
goal since is the Hyprland-shaped desktop (stages 17 to 19, decision of
2026-09-13) and then Steam (stage 22); what is being built toward it now is
the customer's order, and the owners table above lists it. The rows below are
what is left, ordered by what blocks what.

### P1 — required before a stage is called done

| Item | Owner | Stage |
|---|---|---|
| The init's later landings, L11 to L13 of `docs/INIT.md`. **L1 to L10 are done (2026-09-26)**: `/sbin/init` over `src/lib/init/svc` is pid 1 of `cargo xtask run` and of every desktop image (hyprix is `hyprix.service`, each client in a scope of its own); getty, `svc`, readiness, socket activation, resource limits, bootstrap channels, native services and the directory are in, gated by `cargo xtask test-init` on all three architectures, `test-compositor` and `test-jobs`. **L11 is done (2026-09-26, ferrix-55b)**: `devmgr` restarts by the policy, now its own no-alloc crate `src/lib/init/restart` with systemd's fixed-window start limit, and reports each death's status through K6. The init is done as far as the customer counts it (L1 to L11, 2026-09-26). `docs/AUTH.md`'s P0b is done too (2026-09-26): a `Type=native` service with `User=` is made by a helper that has become the user, and runs as it. **L12 is done (2026-09-27)**: under `ferrix.devmgr=init`, which every image booting init sets, pid 1 starts `devmgr` through a kernel starter and `/` switches after it. **L13a is built (2026-10-04, branch `l13-init`)**: all five sandboxing keys read, `NoNewPrivileges=`, `PrivateTmp=` and `ProtectSystem=` carried out in a mount namespace per service (`docs/INIT.md` §4.5). **L13b is built (2026-10-04, branch `l13b`)**: `PrivateNetwork=`. Left: **L13c** `SystemCallFilter=` once seccomp's S3 lands (4); until then a unit asking for it is refused with the reason | ferrix-15 | 15; 13 for L13 |
| **Done 2026-09-28 (steam-y4):** `accept` handed the accepted socket the listening socket's `O_NONBLOCK`, where Linux gives it only what `accept4`'s `SOCK_NONBLOCK` asks, on `AF_UNIX` (`Socket::accept` in `src/kernel/src/fs/socket.rs`) and TCP (`wrap` in `net/socket.rs`). hyprix answered `hyprctl` with `unknown request` or a `Broken pipe`, and yserver dropped X clients started in quick succession ("unable to open display"), found by steam-y5 and, independently, by steam-y4 (3 of 13 `test-xwindow` runs, proved with a yserver build logging the setup thread's `WouldBlock`). Fixed by "Give an accepted socket O_NONBLOCK only when accept4 asks for it", with the certification consultant's OK: the listener's flag decides only whether `accept` waits, and boot checks in `syscall/check.rs` and `net/check.rs` ask both families. After it, 12 of 12 `test-xwindow` runs and `test-compositor --arch x86_64` were clean. Logs: `~/ferrix-logs/steam-y4/` (`gate-xwindow-fail1-wmclass.log`, `rep-3.log`, `diag-6.log`, `acc-xw-*.log`), `~/ferrix-logs/steam-y5/xwininfo-no-display-1.log`, `run4.log` | steam-y4 | 7 |
| **Done 2026-10-02 (btrfs-super):** two ways a writable btrfs lost data or the mount. (1) The writer allocated from the free-space tree, which lists the superblock mirrors' stripes as free, so a tree block or file data could sit at 64 MiB or 256 GiB on the device and be overwritten by the next commit's superblocks; on every volume `mkfs.btrfs` makes that is 27 MiB into the first DUP metadata chunk, and the steam-perf volume lost an fs tree leaf's first copy there (`btrfs check`: checksum failed on 58720256). Each block group now keeps its superblock stripes out of allocation as Linux's `exclude_super_stripes` does, and the free-space tree still lists them free. The reader now reads a mirrored node or data sector from its other copy when the first fails (`btrfs_read_extent_buffer`, read repair). (2) `/data` turned to `EIO` for good twice after Steam was killed while writing there: the block ring ended a disk wait early when the caller's process was killed, the commit running in that thread saw a failed write, and the transaction was aborted for every process. The wait is now uninterruptible, as on Linux. Repro images in `~/.local/share/ferrix/btrfs-super-overlap/` | btrfs-super | 12 |
| btrfs on a full volume, what the certification consultant left open on 2026-10-02 (6a92d6c40, OK; reviews.md): (a) no test of a reload whose own log replay aborts, which `reload_read_only` refuses (its sabotage fired nothing); (b) the commit's share of the tree reservation (`commit_reserve`, dirty/16 + 128 nodes) and a data write's (a node per MiB, `Need::data`) are estimates, shown only on the 128 MiB fixture: derive them, and test that a full-size transaction commits on full trees, before btrfs enters the certified item; (c) log replay and orphan cleanup at mount run unmeasured, so on a full volume they can still abort and fail the mount, as before | open | 12 |
| `hyprctl clients` leaves out windows hidden behind a fullscreen one (2026-10-02: with a maximized Chrome, only it was listed while Steam's login window and the terminal were still mapped; `fullscreen 1` off listed all three). Hyprland lists every mapped client and says `hidden`. Scripts that look for a window by listing see it gone | open |
| Clicks lost once, not reproduced (2026-10-02): six VNC clicks 8 s apart on a maximized Chrome reached QEMU (`input_event_btn` traced) and never reached the page (`pointerdown` not counted), right after a window was closed and the fullscreen toggled under a pointer that had not moved; Steam's floating login window covered the spot, hidden behind the fullscreen one. About thirty clicks after the same steps all arrived. For the record: a click QEMU takes reaches the guest's flushed frame in 11.6 ms median in Chrome and 18 ms in Steam's login window at 120 Hz (QEMU trace `input_event_btn` to `virtio_gpu_cmd_res_flush`, `~/ferrix-logs/yt-perf/qlat.py`); over VNC add QEMU's refresh, up to 3 s after idle | open |
| Under `ferrix.devmgr=init` (L12) the kernel's disk checks of stages 10 to 12 do not run, since `devmgr`'s drivers come after pid 1 (`docs/certification/SAFETY-MANUAL.md` AoU-13). Bringing that configuration into the certified one needs those checks to run anyway: the kernel starting check drivers of its own before pid 1, as the stage 10 driver check already can where `devmgr` did not, then handing the devices to the `devmgr` pid 1 starts (the certification review's option (i), 2026-09-27) | unowned | 10, 12 |
| Authentication, phase 1 (`docs/AUTH.md` §7, 27 points, and K-E's 3): **done 2026-10-03**, P1.5 the last of it. On `main`: `src/lib/crypto/argon2`, `src/lib/proto/auth-proto`, K-E (`SO_PEERCRED` at `connect` and `listen`), `authd`, `passwd`, `authctl`, the client library, `cargo xtask test-auth` with its four negative controls, and the Security Target's OE.AUTH and A.AUTH. **P1.5 landed 2026-10-03**: hyprlock's lock loop and its backend over the client library, `authd` in every desktop's image, and `test-compositor --boot hyprlock` and `--boot hyprlock-unset` (the parked `hyprlock` branch replayed onto the relaid tree as `hyprlock-p15`). What follows is phase 2's P2.5 grants, on the row below | done | 15 |
| Authentication, phase 2 (`docs/AUTH.md` §7, 31 points besides init's L10). **Raised 2026-10-03 by the customer** (Decisions), for the `--everything` desktop as `ferrix`. **Landed 2026-10-03**, each with the certification consultant's OK: the home disk (`build/home.img` at `/home`, `--reset-flash`); `sessiond`, seat0's owner, starting hyprix as the user with its devices (P2.4); hyprix unlocking only on `authd`'s grant, a new locker taking over a dead lock (P2.5, `--boot hyprlock-session`); `login` on the getty with the first password on a local console (P2.3, `run --login`); the session ending with its compositor, and the gates that play the compromised client (P2.7, `--boot session-end`); `su` with the wheel rule (P2.6); `/dev/tty` as the caller's own terminal. the console revoked when getty starts (2026-10-04, the row below). **Left**: procfs honouring `PR_SET_DUMPABLE` (P2.1, 2) and freed socket, pipe and tty buffers zeroed (P2.2, 1); the desktop images other than `--everything` off root; and the customer's call on ending a user's processes at logout | auth, with init and the compositor | 15 |
| **Done 2026-10-03:** `/dev/tty` is the caller's controlling terminal -- the console, a pty slave, or `ENXIO` (`docs/AUTH.md` §1; the certification consultant's OK IF, ledger line 313). It was the console to anyone, `0666`, in the load ring (`src/kernel/src/fs/devfs.rs`), found by P2.6. A session leader's end now lets go of its terminals. `test-init`'s `/dev/tty` stage and the kernel's self-checks show it | done | 15 |
| **Done 2026-10-04 (the certification consultant's OK IF, ledger line 317): the console is revoked when getty starts** (`docs/AUTH.md` §1, `test-init`'s revoke stage). Was: **The console is not revoked when a session ends** (`docs/AUTH.md` §1's "what remains", the certification consultant's D3, recommended as the next AUTH item). getty hands the console to the login shell as fds 0 to 2, and every process of that session keeps them after logout, a program that moved into a scope of its own (§6.4) among them: it can read the next person's login password as typed. The fix, as Linux: when getty starts on a terminal, revoke the open files of it that earlier sessions hold (`vhangup`: their reads and writes fail from then on), and stop the previous console session's scope; a boot check with a lingering reader that must read nothing of the next login, and its negative control | done | 15 |
| zinc's `$?` read 0 after `su` failed, in `su …; echo $?` at the console (2026-10-03, `test-init`'s `su` stage, which therefore judges what `su` says and never its status). A script's `su -c x && y` would run `y` after a refusal. zinc lives in its own repository now (`components.toml`) | open | 15 |
| **Waits on the customer:** whether a user's processes end with their last session (`docs/AUTH.md` §6.4's residual, the certification consultant's E6). Today a program of the user's may move itself into a scope of its own under `user-<uid>.slice` and outlive the session, keeping the user's files and network but no device, no lock channel and no later session's socket. Stopping `user-<uid>.slice` when the last session ends is systemd-logind's `KillUserProcesses=yes`; it would also end `tmux` and `nohup` work, which is why systemd leaves it off by default | open | 15 |
| Authentication, phase 3 (`docs/AUTH.md` §7, about 32 points sized): the PAM shim in ferrousli (5), TOTP (3), ssh passwords through `authd` (4), privilege prompts (6), accounts with a generated `/etc/passwd` (4), a graphical greeter (8), `SO_PEERCRED` taken at `connect` (1), `mlock` as a no-op (1); FIDO2 and fingerprint unsized | open | after 15 |
| Threads, the one piece left after the exit of 2026-09-16: credentials are kept per process, a written deviation from Linux, and that is safe under musl's and glibc's `set*id` broadcast only while setting an id to a current one is permitted in every form. Owed: a static musl program of two threads calling `setuid(1000)` as root that survives | open | 7 |
| A two-last-threads exit check that provably races: spin-meet on two processors, with its negative control -- the old last-thread decision put back -- failing by name. Today's check passes that control too, so it shows only that such a process ends with its first thread's status (from stage 9's review of threads commit 4). 1 point | open | 7 |
| End-to-end user programs for what the `sigpaths` check proves at the kernel's decision: a `SIGSEGV` caught on the alternate stack, and a read interrupted by a handler and restarted under `SA_RESTART` (`SA_RESTART` and the driven signal paths have landed) | open | 7; `rustc` needs `SIGSEGV` on the alternate stack |
| `kill(pid, 0)` answers 0 for about 2 ms after `wait4` has reaped `pid`, where Linux answers `ESRCH`: the ended task is reaped lazily and keeps its process findable (`registry::find` upgrades it). Programs that test a recorded pid for life -- Steam's single-instance check after `GETPID` -- can see a reaped process as alive. A real signal sent in that window reaches only the ended process, which takes no more signals: the pid table keeps its number until the last reference goes, and numbers are handed out cyclically, so no other process can hold that pid yet. Seen by `test-sem`'s reopen step, 2026-09-28 (steam-sysv-sem), which waits for `ESRCH` up to 3 s and prints how long it took | open | 7 |
| Frame-counted checks that return before their task is dead. `sched::wait_until_gone` waits until a task is dead and the reaper quiet, and the rmap, eventfd and exec checks wait on it (FX-0882, FX-0884). Left, 2 points: `exec::run` and every other check site that returns before its task is gone converted, each named in the commit, so no frame-counted check meets this shape again | open | 6, 8 |
| Retire the 20 ms console polling. Receive by interrupt into a 4 KiB ring has landed on all three: the PL011, the STM32 USART (through ST's EXTI on the DK1), and x86-64's 16550 through an I/O APIC input found from the MADT, with `console::input::{waiters, has_input}` for the console thread to wait on. Left: the console thread and readers waiting on it instead of sleeping (in `fs/terminal.rs`) | open | 7, 15 |
| `AF_UNIX` descriptor passing, after names (bf4eec48). `SCM_RIGHTS` has landed: files travel with the first byte of a message, a receive installs what fits and closes and flags the rest, and a peek installs nothing (a written deviation). The in-flight cycle pass has landed too: sockets that only each other's queues refer to are emptied at the next close, process exit or exec. `SO_PASSCRED` and `SCM_CREDENTIALS` landed on 2026-09-26 (909ce1aa). Left, carried from landing 1's review: cap a receive's kernel buffer at the receive capacity (`MSG_WAITALL` in chunks of it), drop a refused file outside the `FdTable` lock before descriptors travel, `SO_SNDBUFFORCE`/`SO_RCVBUFFORCE` needing `CAP_NET_ADMIN` and skipping the cap, and Linux's error order in `sendmsg` and `socketpair`. On the compositor's path (stage 17) as well as POSIX's. | open | 7, 17 |
| POSIX.1-2024 interface sweep: from musl's implementation of every mandatory POSIX.1-2024 function, the list of Linux system calls (and flags) they need; the stage 7 sweep tooling runs each on all three architectures and files every `ENOSYS`, `EINVAL` on a mandatory flag, or wrong result with the area that owns it, as rows here. Sockets and threads are known and excluded; the `epoll`, `eventfd`, `timerfd` and `signalfd` families are in scope because stage 17 needs them | open | 7, 8, 17 |
| `memfd_create`'s exec seal (`MFD_NOEXEC_SEAL`, `F_SEAL_EXEC`); the rest of sealing landed with the FX-0880 boot check | open | 8, 17 |
| Per-open windows onto the card VMO: a program that mapped `/dev/dri/card0` and closed it can still read what the next opener draws, since the exclusive open does not end a mapping (`docs/DISPLAY.md` §2.3). Until then `card0` is `0660` and root's, accepted for iteration 1 by os-f6 2026-09-16. Each open gets its own view of the card's pages, which comes with stage 19's render node | open, with stage 19's render node | 17, 19 |
| Trusting a BAR firmware placed but did not enable. **Started 2026-09-19:** `src/lib/platform/fdt` now reads a host bridge's `ranges` into `PciWindow`s that keep the bus and CPU addresses apart, with `holds`/`translate` and seven host tests (QEMU `virt`'s own three entries among them). Left, in order: the same windows on ACPI machines, which needs the loader to call `EFI_PCI_ROOT_BRIDGE_IO_PROTOCOL.Configuration()` before `ExitBootServices` and carry them in `BootInfo` (a version bump); the vetting itself in `src/kernel/src/discovery/pci.rs` -- whole BAR in one window, window kind, prefetchable one way only, every bridge upstream forwarding and decoding, decoding-on BARs admitted first, unassigned BARs reported as unassigned; and turning decoding on at `IoMapping` creation rather than at enumeration | ferrix-d9 | 10 |
| CI's Miri step for `src/lib/fs/btrfs`: CI runs Miri over `src/lib/fs/block`'s request queue and not over btrfs. Under 15 minutes, with the whole-image tests ignored under Miri | open | 11 |
| Steam's bootstrapper (I5b, `docs/I386.md`): `cargo xtask test-steam-bootstrap` reaches the client's "Unable to open X11 display" on Ferrix only with scout's requirements check set aside and an i386 semaphore stand-in preloaded (`FERRIX_STEAM_PRELOAD`, 2026-09-28, branch `steam-i5b`, run log `~/ferrix-logs/steam-i5b/run8.log`). Without the stand-in the client waits forever at its System V semaphores, which branch `steam-sysv-sem` is building; the gate passes once they land. Without namespaces the check stays set aside, and the client's UI (`steamwebhelper`, in pressure-vessel) will need them after the display. What the display side needs: GLX from the X server and i386 Mesa beside the client (`glXChooseVisual` failing is fatal), RandR | open | 22 |

### P1 flakes — seen on a gate, each with its log

A flake that keeps firing is cheaper to fix than to rerun. Each row keeps its
log path and commit; a new sighting is added to its row the day it is seen.

| Item | Owner | Stage |
|---|---|---|
| `test-init`'s refusal checks can pass for a program that never ran (the certification consultant's advisories A1 and A2, ledger 368, 2026-10-05, on po6/red): in `po6-red-ctl-nounfinished-2.log` (`~/.local/share/ferrix/logs/queue/`, the unfinished line sabotaged so no answer was typed) su's right-password prompt took the gate's next line, the wrong-password su's own command, as its password, and "su: a wrong password was refused" still printed though `su-wrong` was never seen: `su` and `log_in` drop `answer_prompts`' false and accept any `Authentication failure` / `Login incorrect` since their line. Main's blind typing had the same hole. **Next:** the wrong-password su and the login wrong guess require their own answer taken and their own end marker seen. A2: `ferrix-auth-client`'s flush-before-prompt order (05a0968ec) has no negative control; with the gate typing only after the prompt, reverting it shows only as a rare race. Where to start: a host or guest check that types the moment the prompt appears | unowned | 15 |
| Stage 9's wake row panicked an ARMv7-A `test-init --arch all` boot at 10.03 s on 2026-10-04: "the wake row: the poster never reached its wake while the blocker held its lock", on 99c5166cd in the batch that also failed AArch64's login (`~/.local/share/ferrix/logs/queue/batch-20261004T190141Z-5.log`, armv7a section; filed 2026-10-05 by po5-red, not looked into). The check wants a poster to reach its wake while the blocker holds its own run-queue lock; under load the poster may not be scheduled inside the blocker's hold. Where to start: what the check bounds the hold by | unowned | 9 |
| ferrousli's `c_mount` test `file_system_statistics_and_the_calls_that_mount_swap_and_sync` ran past its 30 s ("mount/filesystems-O0: still running after 30s") in the `src/user/system` rename's `check --zinc --ferrousli` on nazuna, 2026-09-30 (os-3c), and a rerun alone hung past 8 minutes. Not the tree: the program calls `sync(2)` on the host, and a bare `sync` on nazuna took 14 min 21 s at that moment, the disk at 99% and the load at 30–43; another session's run of the same test (`target-ns-land`) was stuck the same way, and ferrousli's 770 other tests passed in debug and release. A unit test that waits on the host's whole page cache is at the mercy of whatever else the host writes: call `syncfs` on the test's own directory, or bound the wait by the sync rather than by 30 s. Log: `~/.local/share/ferrix/logs/os-3c/check-cmount-fail.log` on nazuna | open | ferrousli |
| The System V semaphore self-check stopped an AArch64 boot of `test-init --arch all` at 6.55 s: "sem: a waiter returned without waiting", on main 70ad8969 at a load of 54, 2026-09-30 (os-98, timing the architectures in turn against at once); the same step's next run passed. The check requires a `semop` that must block to block; under load the waiter may have found the value it waited for already there, or been woken for another reason and counted as not waiting. Where to start: what the check takes as proof of a wait. Kept: `~/ferrix-logs/os98-par/sem-waiter-aarch64-70ad8969.log` on nazuna. | open | 7 |
| FX-0309, the audit self-check: "a DMA fault the IOMMU reported left no record", at 11.29 s of a `test-compositor --arch x86_64` boot under TCG on main 43323d3b, at a load of 33, 2026-09-30 (os-98, running main as the control for the animation fix); the fix's own run of the same row just after passed. The check wants an audit record for each fault the VT-d out-of-domain probe causes; under load the fault may be reported after the check looks, or its record dropped. Where to start: whether the check waits for the fault's report or reads once. Kept: `~/ferrix-logs/os98-submap/main-x86-tcg-panic-43323d3b.log` on nazuna. | open | 10 |
| Stage 7's signal hand-off check flaked on 2026-09-24: "a signal sent to a process of two threads was given to no thread" (`src/kernel/src/syscall/check.rs`, the check that blocks `SIGUSR1` in the first thread and requires the second to be handed it), at 10.01 s in `test-shell` on x86-64 under TCG with zinc, in cgroup landing G4's first gate (6c553b8c) on a loaded example. The rerun of the row on the same commit passed, as did every other boot of that gate and of G4's final one. G4 changes `clone3` and cgroup moves, not signal delivery; `take_handed_to` reading 0 means `kill::send` picked no thread at all, so start at how the send judges a thread that is between its wake and its return, beside FX-0701's `return_to_user` fix. Kept: `~/ferrix-logs/stage7-handoff/2026-09-24-shell-zinc-6c553b8c.log` on example (the panic at its line 106). **Again 2026-09-26**, at 12.81 s in one of 28 x86-64 TCG `test-boot`s of the FX-1004 channel fix (branch `fix-fx1004-endpoint`, before its commit; it changes only `object/channel.rs`), QEMU pinned to two cores shared with two busy loops. Kept: `~/ferrix-logs/stage7-handoff/2026-09-26-boot-x86_64-fx1004-channel-fix.log` on example. **Once more, 2026-09-24**, in `test-input` on x86-64 under a load of 37, the night of FX-0001 below: `~/ferrix-logs/signal-no-thread/2026-09-24-input-x86_64-454eaab.log` on example **Twice more on 2026-09-26 under the drcov coverage plugin**: at 11.69 s in x86-64 `test-powerfail` (`~/ferrix-logs/fqs5-flakes/sighting-handoff-cov2-x86_64.log`, line 2701) and at 8.85 s in ARMv7-A `test-shell --smp 2` (`sighting-handoff-cov4-armv7a.log`, line 1682). Tried on branch fix-queued-small-5: about 33 boots that run the check, QEMU pinned to two cores beside two pinned busy loops with drcov on -- 27 x86-64 `test-sysfs`, 3 x86-64 `test-boot`, 3 ARMv7-A `test-boot --smp 2` -- and it did not recur. Read: `take_handed_to` answering 0 means `notify_signal` found no thread in `threads()` that does not block `SIGUSR1`, or was not called (`post_into` answering other than `Pending`); the second thread spins in user mode and never changes its mask, and only the check's own `take_handed_to` clears the word, so neither is explained yet. Next: print, in a failing boot, each thread's mask and `is_gone` as `notify_signal` sees them. | open | 7 |
| `test-boot --arch armv7a --smp 2` failed once on 2026-09-26 with "the kernel read no entropy by DMA": the boot reached `FERRIX-BOOT-OK`, but its `pci` line said `0 entropy bytes read by DMA` from the virtio-rng function. The tree was init-l6's gate (47494211), which changes no kernel code; the same row passed on its rerun, and the other three boots of the gate passed. Where to start: the virtio-rng read at boot on ARMv7-A with two processors, whether its completion can be missed or come after the line is printed. Kept: `~/ferrix-logs/init-l6/2026-09-26-boot-armv7a-smp2-47494211-no-entropy.log` on example | unowned | 10 |
| `test-compositor --arch x86_64` is the one image row not yet seen green on the regrouped tree (docs/LAYOUT.md). On main 26303ad5 every functional check passed (the window slid through its three pictures and ended in the blessed one) and the row failed only on its frame budget, the slowest frame 7.9 s against 5 s, with the host at load 44–52 on 24 cores; its rerun on 7322d68a stopped earlier, in FX-0905, since fixed by "Change a task's state and its job's load in one step". Every other row passed on the regrouped tree: check, `build --arch all --release`, the x86_64 (TCG and KVM), aarch64 and armv7a (one and two cores) boots, `test-shell` with ferrousli and with busybox, `test-vfs`, `test-sysfs`, `test-init`. Rerun it on a quiet host; a failure that names a path is the relayout's. Kept: `~/ferrix-logs/relayout/2026-09-26-compositor-x86_64-26303ad5.log` on example **Measured 2026-09-27 (ferrix-9b, branch `frame-budget`, not landed): the host's run-queue wait explains little of it.** That branch reads each virtual processor's `/proc/<tid>/schedstat` through the boot and takes off only the wait inside each frame's own stretch, with a SIGSTOP of the boot's own QEMU as the negative control (which failed as it must, 2.3 to 3.8 s past the bound). At loads of 25 to 40 frames still took 5.4 to 20.6 s on x86-64 and AArch64 while the busiest processor waited 0.7 to 2 s of the whole boot: the 20.6 s frame was 9.6 s of shadow and 5.6 s of surface drawing, against 1.5 s a frame on a quieter host. So the guest ran and did a tenth of the work -- more than a busy hyperthread sibling explains, which is worth its own look inside the guest -- and a wall-clock bound under TCG will keep failing on this host until frames are measured in guest work (`-icount` for this boot, or instructions counted) or the bound is dropped from the gate under load. Kept: `~/ferrix-logs/frame-budget/` on example, the branch's logs among them. | open | 17 |
| FX-1201, seen once on 2026-09-26: stage 12's btrfs write check stopped the boot with "stage 12 self-check failed: a file written could not be read back" at 8.64 s, in one boot of `test-compositor --arch x86_64` under TCG (the one after the screen-lock boot), in the gate of init-l10 at 2f664b4e on main 865c62c8, with the host at a load of 30 to 80. The message is `read_pattern`'s (`src/kernel/src/fs/btrfs_write_check.rs:147`): `read_at` on a file the check had written, synced, unmounted and mounted again answered an error, so a read of the writable test disk (the third virtio-blk disk, a fresh `blank` fixture) failed through the block ring rather than reading back wrong bytes. It runs before init, and L10 changes no kernel code; the 33 boots of that row before it passed, the panic ended the row, and every other row of the gate passed. Where to start: what the block ring answers a read that times out or comes back short under a stalled host, and whether the btrfs read path turns a retryable ring status into EIO. Kept: `~/ferrix-logs/fx1201/2026-09-26-compositor-x86_64-2f664b4e.log` and `-serial.log` on example | unowned | 12 |
| `test-compositor` on x86-64 under TCG failed once, on 2026-09-26: "the slowest frame took 5715709 us, past the 5000000 us a frame under emulation is allowed", in the window-sliding boot of the gate of feb1d6cf (virtio-gpu `ioeventfd=on`). The frame's time was software drawing -- shadow 2.0 s, surface 1.5 s, border 0.9 s, blur 0.4 s, the flip 0.4 s -- which the change does not touch, with the host at a load of 11 to 26 from other sessions' guests. Two reruns of the whole suite on the same commit passed, with slowest frames of 0.99 s and 1.59 s. Kept: `~/ferrix-logs/compositor-slowframe/2026-09-26-compositor-x86_64-feb1d6cf.log` on example. **Twice more the same evening (edid-override, fc4a5230 before the relayout), host load about 35:** the same window-sliding boot, first with its end picture 710248 of 786432 pixels off after a 16.0 s frame (shadow 6.1 s, behind 3.3 s, blur 2.2 s), then on its rerun "the slowest frame took 9117465 us"; the branch does not touch drawing or animation, and every boot before and the ones after it that ran passed. Kept: `~/.local/share/ferrix/logs/edid-override/compositor-full-animation-fail1.log` and `compositor-animation-rerun.log` on example. **Three more on 2026-09-26, 23:10 to 23:30, after the fleet's restart, at a load of 20 to 35**: slowest frames of 7.7 s and 8.1 s in two runs of the badapple-desktop branch (a74b0b42, which touches no hyprix drawing), and 11.8 s on unmodified main e00def08 (init L10). Each was a single frame, spent mostly on shadow, blur and backdrop in software, with every functional check passing. It is a budget the host's load decides, not a regression of either. Kept: `~/.local/share/ferrix/logs/b0/test-compositor-x86_64-slow-frame-*.log` on example. **Seen on ARMv7-A, 2026-09-30:** `test-compositor --arch armv7a`, the decorations boot, one frame of 15.2 s against the 10 s allowed (border alone 3.75 s, where the frames beside it took under 1 s whole), at a load of 8 to 27 with the AArch64 row and KVM boots beside it, gating a8a82845; the same boot on that tree and on main 6d186140, twice each right after, passed with slowest frames of 1.2 to 2.5 s. Kept: `~/ferrix-logs/os98-submap/armv7a-decorations-frame-budget-a8a82845.log` on nazuna. **And x86-64 the same evening**, the same boot, one frame of 6.85 s against 5 s at a load of 20, gating 2749b5a1; that boot alone, alternating that tree with main 43323d3b, three runs each at loads of 8 to 11, passed every time with worst frames of 0.55 to 0.85 s on both. Kept: `~/ferrix-logs/os98-submap/x86-decorations-frame-budget-2749b5a1.log`. | open | 15 |
| `test-jobs` flaked once, on 2026-09-24: "the session at the console did not answer" after typing `echo jobs-gate: the shell reads the console`, on x86-64 in cgroup landing G4's gate (6551ea89). The transcript shows the typed line echoed only once the next line was typed, as if a console read woke late by one line; every other expectation of the run was met, and the rerun on the same commit passed. G4 does not touch the console or the terminal. Kept: `~/ferrix-logs/jobs-console/2026-09-24-jobs-6551ea89.log` on example. | open | 15 |
| ferrousli's busybox on ARMv7-A fails `test-net --arch armv7a --init ferrousli`: 10 of 13 programs fail, starting with `udhcpc: poll: Invalid argument`, so the guest never gets an address; AArch64 passes all 13. Found on 2026-09-23 when the cgroup gate first ran `--init ferrousli` on all three architectures, and reproduced on main 90b0a1de without the cgroup work. ARMv7-A's `poll`/`ppoll` in ferrousli (the time64 variant?) is where to start. Kept: `~/ferrix-logs/ferrousli-arm/2026-09-23-net-armv7a-90b0a1de.log` on example | open | 15 |
| FX-0001, "processor N never flushed its TLB for a shootdown", stopped `test-selfhost --plan` twice in a row on 2026-09-24 (os-8d), with eight virtual processors in a parallel build on example at a load of 42 to 52 on 24 cores: once in `munmap`, once in `execve`'s `vmap::free`. A waiting lock holder gets four seconds (`TURN_TIMEOUT_NANOS`) because a preempted virtual processor can lose whole seconds; the processors a shootdown waits on got one. **Advanced 2026-09-26:** both waits now also need a count of the waiter's own polls (`smp::patience`), which a slow emulator stretches and a descheduled waiter does not spend: 1.8 s under KVM, 5.1 s under `tcg`, 32 s under the coverage plugin. A host that stops running the processor waited for while it runs the waiter still ends the wait, only later; KVM's steal time is the next thing to try if the plan stops on FX-0001 again. Logs: `~/ferrix-logs/fx0001/2026-09-24-selfhost-plan-smp8-454eaab{,-2}.log` on example. 2 points **Again 2026-09-26, in the evening**, in `test-chrome-window --accel kvm --interpreter ferrousli --library ferrousli` on x86-64, with Chrome running, on branch `chrome-window-ferrousli` (main f670d5ad), at a load of about 40; the rerun passed. Kept: `~/.local/share/ferrix/logs/cwf-gate-98c9740f-on-f670d5ad/chrome-window-ferrousli.log` on example. **Again 2026-09-30**, in `test-compositor --arch x86_64 --accel kvm --boot submap` on `main` ba9c4388 at a load of about 13: "processor 0 never invalidated its TLB for a scoped shootdown: no answer in 1000 ms, asked 25763072 times", on processor 1, while the compositor ran; the next runs of the same boot passed. It showed as a picture that was not the one blessed, since the screen stopped. Kept: `~/ferrix-logs/os98-submap/fx0001-kvm-submap-ba9c4388.log` on nazuna. And in the same boot of the full `test-compositor --arch x86_64 --accel kvm` the same evening, gating the fix above at a8a82845 at a load of 13 to 15, the waiter on processor 0 in `sys_mprotect` (`AddressSpace::protect` -> `shoot` -> `flush_tlb_pages`, the space's lock already dropped) and processor 1 never answering; both times the moment `L` started two `hyprctl`s at once. 40 runs of the boot alone right after did not show it. Kept: `~/ferrix-logs/os98-submap/fx0001-kvm-gate-a8a82845.log`. xtask now asks QEMU where every processor is when a compositor boot's kernel panics (`panic-registers.txt` beside the screen), which is what the next sighting should read. | open | 20 |
| **Done 2026-09-27:** FX-0502's "waited for an unrelated interrupt", in all three wordings (a task spawned onto its creator's processor, woken onto its waker's, pulled by balancing), seen six times under host loads of 20 to 42 and rerun away each time, once on AArch64, which rules out the x86 clock the checks read. It was the scheduler, not the host and not a strict check. A yield moved the running task's deadline a slice further out even when it was alone and picked itself again, so a task in a yielding loop -- the checks' own `reap_to`, which loops longer on a loaded host -- came to hold a request of hundreds of slices; a task then woken or moved behind it ahead of its share was not eligible and waited for the next decision, and `CpuQueue::arm_timer` armed that decision for the running task's whole remaining request. A diagnostic build caught it in two of four loaded boots, the LAPIC armed for 1.585 s and 2.02 s while the checking processor polled with no gap over 9 ms; 600 lone yields made the request 1.8 s every time. Fixed as Linux does both halves: a lone yield asks for nothing, and while anything waits the next decision is at most a slice away (`RunQueue::yield_curr`, `RunQueue::decision_in_ns`, L.sched.1 and L.sched.2 in `docs/sysml/15-sched-requirements.sysml`). Host tests reproduce each half and fail with its fix reverted. The checks are unchanged. Outside them, a program that `sched_yield`s in a loop while alone could hold a woken task off for seconds the same way. Diagnostics, logs and the patch: `~/.local/share/ferrix/logs/fx0502-diag/` on example; the sightings' logs are where this row listed them (`~/ferrix-logs/fx0502-unrelated-interrupt/`, `~/ferrix-logs/fx0905/loads/`, `~/.local/share/ferrix/logs/stall/fx0502/`). | ferrix-55b | 5 |
| **Done 2026-10-05 (po6-seam):** the block ring's indices were torn. `ferrix-driver`'s ring memory (`src/user/system/native/driver/src/block.rs`, `Ring`) gave only byte accessors, and `ferrix-blkring`'s `RingMemory` composed every `u32` index from them, so the driver wrote its completion tail a byte at a time and read the kernel's submission tail the same way. A tail stepping from 0x..ff to 0x..00 was then seen as a ring and more ahead (the kernel: "the peer's tail ran more than a ring ahead", ring ended, disk parked) or behind (the driver: exit status 20, a ring fault, restarted by `devmgr`). When the kernel ended the ring and the driver lived on, nothing took the parked disk up, and the deep run's readers waited out the read patience: the 30 s stall, then EIO. Shown with a diagnostic build (not landed) looping 64-read deep runs for 150 s per x86_64 KVM boot and naming why each ring ended: before, 3 of 3 boots failed (the first stall after 27,616, 33,144 and 17,936 runs; the last boot also showed 4 driver exits with status 20 that `devmgr` recovered from, and the kernel's `TailOverrun` at 48.05 s); after, 2 of 2 boots passed, 137,056 runs, no ring ended, worst run 289 ms. Fixed: the driver's `Ring` reads and writes each aligned `u16` and `u32` in one access, and `RingMemory` no longer provides them, so no implementation can inherit torn ones again (a `compile_fail` doctest holds it), as `ferrix-virtio`'s `QueueMemory` already does. The same branch fences `SplitQueue::device_wants_notification` (a lost-notification race Linux fences against too); the diagnostic showed it was not this flake's cause. The net ring has the same shape and is its own row below. Logs: `~/.local/share/ferrix/logs/queue/po6-seam-diag-base-1.log`, `po6-seam-diag-fix-1.log`, `po6-seam-diag2-1.log` (before), `po6-seam-diag3-1.log`, `po6-seam-diag3-2.log` (after), on nazuna. History: Stage 10's driver self-check panicked once on 2026-09-27, x86-64 under TCG: "a read from the seam's deeper run failed" (`src/kernel/src/interfaces/block_ring/hop_check.rs`, the depth-32 timing ec6d3988 added), with QEMU saying "virtio-blk missing headers" and tracing VT-d faults beside it, at 10.04 s of `test-shell` with Debian's busybox on ferrousli's loader, gating i386 I4 (3045d416, system-call layer only), host load about 20. The rerun passed; every other row of that gate did too. Kept: `~/ferrix-logs/seam-deeper-run/2026-09-27-debian-ferrousli-x86_64-3045d416.log` on example. QEMU's message says a request reached the device without its header descriptor, so start at how the deeper run builds its chain. **Where it stands, 2026-09-27 (ferrix-55b):** not reproduced in about 290 deep runs of 32 reads (a diagnostic build looping the deep run, 5 `test-boot` and 10 exact-row boots at host loads 13 to 32; `~/.local/share/ferrix/logs/seam-diag/` on example). Proven from the log and QEMU's source (`hw/block/virtio-blk.c`, `hw/virtio/virtio.c`): the chain QEMU popped had no readable or no writable descriptor, and every descriptor in it was non-empty and translated, since a zero-length or an unmapped one is reported otherwise; the driver then saw NEEDS_RESET and exited with 8. The VT-d lines are the kernel's own out-of-domain probe. Ruled out by reading: the chain's layout (`blk::publish`), descriptor and header-slot accounting (`count_chains`, `take_header`), page splitting (`blk::plan` joins only contiguous pages), device addresses aliasing (a pin maps each page at its own address, once), and two drivers on one device. Found on the way, not this signature: `QueueMemory`'s default `u16` accessors publish `avail.idx` and read `used.idx` a byte at a time, which faults the driver or makes QEMU report the index moving, and is being fixed. Waiting for the next sighting, with two diagnostics landed for it: a failed run now names any VT-d fault the kernel did not provoke (it used to drop them), and the driver's exit status names the fault (23 the device refused a request, 30 and up a virtqueue check). **Seen again 2026-09-29 (os-d9):** `test-procfs --arch x86_64` under TCG, gating 89cda0d1 (the `tests/` → `src/tests/` move, no kernel change), host load about 3. Unlike the first sighting there was no driver exit-status line and no "virtio-blk missing headers": the deep run stalled from 7.67 s to 38.23 s and then panicked, and QEMU's only remarks were the VT-d faults at iova 0x1000 from 00:02.0, the kernel's own probe. The rerun passed. Kept: `~/ferrix-logs/layout-tests-procfs-panic-89cda0d1.log` on nazuna. **Seen again 2026-10-02:** `test-shell --arch x86_64` under KVM, gating branch `steam-sysv-shm` (a99665dda, System V shared memory), host load about 3: `devmgr` started its drivers at 6.17 s, then nothing until the panic at 36.32 s, the same stall as the second sighting. The rerun passed. Kept: `~/.local/share/ferrix/steam-sysv-shm-gates/shell-x86_64-panic-serial.log` on nazuna **Seen 4 more times 2026-10-05** in the overnight hunt (main 634c7ed0e, 9,234 `test-boot --arch x86_64 --accel kvm` boots four at once, host load about 3): `~/.local/share/ferrix/logs/queue/os7c-night-boot-1-2037.log`, `-3-2154.log`, `-3-439.log`, `-4-1842.log` on nazuna, each the same: `devmgr 13 devices, 10 drivers, 3 started` at about 7.5 s, nothing for 30.08 s, then the panic at `src/kernel/src/main.rs:612:25`, with no driver exit-status line and only the kernel's own VT-d probe faults from QEMU. | po6-seam | 10 |
| The net ring has the block ring's torn-index shape, found 2026-10-05 (po6-seam) beside the seam's deeper-run fix above, not yet seen failing: the virtio-net driver's `Ring` and `Data` (`src/user/system/native/drivers/net/virtio-net/src/main.rs`) give only byte accessors, `ferrix-netring`'s `RingMemory` provides its `u16` and `u32` accessors from them, and the kernel's `Pages` in `src/kernel/src/interfaces/net_ring/mod.rs` gives `u16` but composes `u32` from two. A `u32` index crossing a 0x100 or 0x10000 boundary can then be read as a ring ahead or behind and end the ring, which may be behind some of the gateway flakes. Fix as the block ring was: single aligned accesses on both sides and the accessors required by the trait (the kernel half touches the item and needs the consultant). | unowned | 3 |
| An x86-64 boot hung before stage 3 on 2026-09-26, with no panic: `test-shell --arch x86_64 --init /usr/bin/busybox` (the host's glibc busybox), third boot (`ferrix.init=/etc/k7-read ferrix.onexit=panic`, reading `/data/k7` back), under TCG, printed its `input` line ("the port receives by interrupt 32") at 2.14 s and then nothing until xtask's 120 s timeout. The next line on a good boot is `stage 3` (breakpoints, page faults, 251 ticks at 999 Hz), so the hang is in stage 3's own checks or just before them: no filesystem, no pseudoterminal and no init yet. Gating btop-fix (0778232c on main 6f5090f6, which touches `fs/pty.rs` and the compositor only) at load about 20; the rerun passed at load 36, and so did the other three `test-shell` inits and five boots of the same gate. Not FX-1201, which panics at stage 12. Kept: `~/ferrix-logs/btop-fix-early-stall/2026-09-26-test-shell-glibc-x86_64-0778232c.log` on example. **Seen again 2026-09-27 at 05:47**, the same shape: `test-shell --arch x86_64` with the glibc busybox, third boot (`ferrix.init=/etc/k7-write`), TCG, `input` at 1.73 s and then nothing for 120 s, at a load of about 20, gating branch `cgroup-busy` (8e8e381d on a5bf4a1d: init's cgroup removal and nanosleep, neither reached before stage 3); every other step of the gate passed. Kept: `~/ferrix-logs/cgroup-busy/2026-09-27-test-shell-glibc-x86_64-8e8e381d-stage3-stall.log` on example. **Again 2026-09-28 at 19:12**, the same shape in a different gate: `test-auth --arch x86_64`, its one boot (`ferrix.init=/sbin/init`), TCG, `input` at 1.39 s and then nothing for 120 s, load about 10, measuring branch `tt-cut2-stop-guests` on a0772269 (xtask only: what happens after the marker); the next two runs passed. Kept: `~/ferrix-logs/gate-time/tt2/auth-fail-1.log`, the whole stamped transcript | open | 3 |
| `test-compositor` under TCG at load 35 to 40 took input seconds late, twice on 2026-09-27 in the frame-budget branch's runs (15de1447 on main 100c7621, xtask-only): `--boot lock` said "a bind fired while the session was locked" because hyprix said `the session is unlocked` before it started `lswt close one` for the K bind -- the four-second lock, timed in the guest, ran out before the key arrived -- and `--boot animation` kept one picture for `follow`'s 30 s, the slide's frames reported only after it gave up. Both reruns passed. Seen again on 2026-09-27 at 03:00 and 03:10, load 16-32, on `fuzzel-window`: `--boot animation` said `the window jumped: 2 pictures, none of them between the two states`, then `the slowest frame took 11435595 us`; the third run passed. Kept: `~/.local/share/ferrix/logs/fuzzel/w2-tc-animation-jumped.log`, `w3-animation-fail2.log`. Where to start: judge the lock's bind by where `started /bin/lswt` falls against `the session is unlocked`, or hold the lock until xtask has pressed K; and time a slide from its first changed picture, not from the keypress. Kept: `~/ferrix-logs/frame-budget/2026-09-27-compositor-x86_64-cca78363-lock-bind.log` and `2026-09-27-animation-x86_64-15de1447-slide-late.log` on example. **Twice more on 2026-09-28**, `--boot animation` in whole `test-compositor --arch x86_64` runs of branch `tt-cut2-stop-guests` (xtask only) at loads of 20 to 28, the host saturated by other sessions: `the slowest frame took 12322396 us`, and on the next run the slide's last picture 710248 pixels off; six `--boot animation` runs, three on main and three on the branch in turn at loads of 20 to 36, all passed. Kept: `~/ferrix-logs/gate-time/tt2/compositor-animation-slow-frame.log` and `compositor-after3-fail.log` | open | 18 |
| AArch64's PCI self-check line flips between boots of one kernel: "the device completed the faulted write anyway: 64 bytes that never reached the page, seen **after** the fault" in one boot and "seen **before**" in the next, on d5480aee's baseline, booted twice on 2026-09-30 (`~/.local/share/ferrix/logs/discovery-lines/baseline/aarch64-{1,2}.lines` on nazuna). Whether the device's write lands before or after the SMMUv3 records its fault is a race in the out-of-domain probe (`discovery/pci/virtio.rs`), so the line is evidence of the fault and not of the order. It makes a byte-for-byte comparison of discovery lines need an exception; either print the order only when it is stable, or compare it separately. Seen by os-80 comparing the Finder landing's boot lines | open | 10 |
| Host tests of the compositor failed `cargo xtask check` under load on nazuna, three times on 2026-09-30 in os-35's gates, each passing on its rerun. First, hyprix `two_clients` (`a_plugin_adds_a_dispatcher_and_the_compositor_hands_it_over`) at load about 33, on WIP b214c717 of `os-35/ipc-measure`. Second, hyprix `two_clients` again, on da2c0306 of `os-35/ipc-lazytlb`. Third, waybar `against_hyprix`, on 571ea292 of the same branch. None of the three changes touches the compositor or waybar. Kept on nazuna: `~/.local/share/ferrix/logs/os35-measure/check-hyprix-fail-1.log`, `~/.local/share/ferrix/logs/os35-lazytlb/gate-check-hyprix-flake-1.log` and `final-check-waybar-flake-1.log`. Seen again on 2026-10-01 in `os-35/ipc-ring`'s gates: hyprix `two_clients` twice, at load 58 on e779da4e and about 50 on 52b22de8, each passing on rerun; and the toolkit's `against_hyprix` with a ConnectionReset on 4902c4e5, not rerun. Logs in `~/.local/share/ferrix/logs/os35-ring/`: `ring-check-hyprix-flake-e779da4ea.log`, `ring-check-hyprix-flake-52b22de8.log`, `ringb-check-toolkit-flake-4902c4e5.log`. Again on 2026-10-01 at load 9 to 17, hyprix `two_clients` (`a_bar_on_the_event_socket_is_told_what_happens`) on b55df634, the MSI-X change, passing on rerun: `~/.local/share/ferrix/logs/os35-land/check-hyprix-flake-b55df634.log`. It fails at low load too, so load is not the whole story. Related to the load-only hyprix bar flakes seen before | open | 18 |
| A wake goes missing on AArch64 with a GICv3 (`FERRIX_ARM_MACHINE=gic-version=3`), found on 2026-09-30 by os-35's sync-wake relay (`src/kernel/src/sched/sync_check.rs` on `os-35/ipc-wake`), whose waits then looked again only at a 30-second deadline: in 8 of 9 TCG boots one traveller's relay stopped, its next party -- an anchor pinned to another processor, or the traveller -- never taken off its queue by the wake at home the hand-over made, until the deadline. It reproduces with every wake made at home (the branch's moves turned off, broadcast IPIs, no idle polling) 3 times of 3, and never showed on the GICv2 `virt` both Arm gates boot, on ARMv7-A or on x86-64. Ordinary waits look again every 5 ms, which is why nothing else notices; the relay now does the same and prints how many hand-overs the recheck rather than a wake found. Where to start: a kick's SGI on a GICv3 -- whether `KICK_PENDING` can be left up by one that is never taken, and the `isb` Linux puts after `ICC_SGI1R_EL1`. Kept: `~/.local/share/ferrix/logs/os35-wake/KEEP-c3-aarch64-gicv3.log`, `KEEP-rep-B-gicv3-*.log` and `nc-exp-B-nosync-*.log` on nazuna | open | 5 |
| An AArch64 `test-boot` of unmodified main eeaa35cd timed out under load (41 to 51) on nazuna on 2026-10-01, while `os-35/ipc-ring` was measuring its baseline; not rerun. Kept: `~/.local/share/ferrix/logs/os35-ring/base-eeaa35cd-aarch64-timeout-serial.log`. Read the serial log's last lines for the stage it stopped in | open | 10 |
| `test-net --arch x86_64 --init <musl busybox>` stopped with a program that never exited, twice on 2026-09-30, both while nazuna's disk was at 98 to 100% or a second gate was running beside it: command 17 (`git` over the network) on 3042aa0e's first gate, and command 8 (`wget -q`) with every later command never started on 3042aa0e's third. Four runs on a quiet machine, two on the base 79672efd and two on 3042aa0e, all passed. Kept: `~/ferrix-logs/finder-net-musl-hang-c69b7f0d.log` and `~/ferrix-logs/finder-net-musl-hang2-3042aa0e.log` on nazuna. Look first at what a networking program waits on when the host is slow: the 600 s timeout was not near the programs' own time | open | 13 |
| `test-net --arch x86_64` hung once on 2026-10-01 at its first `wget` (command 8, `wget -q -O - http://ferrix.test:PORT/hello`) for the full 120 s, in 1 of about 13 runs under KVM on nazuna, on net-throughput 931e553 plus the 128-entry ring and queue change (the other runs, three dozen boots of the same image at 8 to 44 segments in flight, passed). The gateway saw one TCP flow open and one retransmitted SYN-ACK 27 ms later (`GWDBG rto after 26.9ms outstanding 1`), and nothing after; the guest log stops at the command line. The VT-d fault lines at the end are the driver dying when QEMU was killed, not the cause. Not root-caused: a frame or wake lost between the driver and the net core on a connection's first segments is the lead, and whether the older 32-entry ring does it too is not known. Where to start: a loop of `test-net` x86_64 with the guest's net-ring counters (`Serve::counters`, `dropped`) printed on exit. Kept: `~/.local/share/ferrix/logs/net-throughput-hang/nt-44-hang.log` and `-serial.log` on nazuna | unowned | 4 |
| **Done 2026-10-05 (po6-gw2): not the refusal's time but a lost frame, a gateway bug on Windows.** The refusal takes 2.0 s on Windows (2.04-2.06 s with 24 at once; 2-4 s on CI's passing runs), and the failed CI run waited its whole 15 s. The gateway never received the test's one SYN: its serving loop received with a 5 ms read timeout, and on Windows a receive that times out can lose the datagram arriving as it does. Both the timeout and the sender's own sleeps and timeouts end on the 15.6 ms tick, so under load they meet often. Instrumented, every failure showed the serving loop turning and no datagram received, and no connect started. A probe with a 5 ms receive timeout under 48 busy loops on this 24-thread PC lost 39 of 90 lone datagrams and 12 of 1,200 in a stream; peeking first (`peek_from`, then `recv_from`) lost 0 of 90 and 0 of 1,200. Fixed in the gateway (`gateway::next_frame` peeks before it takes), and the tests' `Guest::frame` peeks too. The test is unchanged in what it checks. Before: 84 of 96, 42 of 48 and 19 of 24 runs failed (24 at once under 48 busy loops). After: 192 of 192 passed. Negative control, gateway receive without the peek: 40 of 48 failed. Was: The gateway host test `gateway::tests::resets_a_connection_to_a_port_nothing_listens_on` (`tools/common/xtask/src/gateway/tests.rs` around line 850) failed once on CI's `Test (windows-latest)` on 2026-10-05, on `main` 39e520e31 (run 37331100176, first attempt): "no frame of EtherType 0x0800 arrived" at `tests.rs:140`, after the test's wait of `CONNECT_TIMEOUT` (10 s) + `PATIENCE` (5 s) for the reset. A rerun of the same job passed, and the test passed on every earlier run, so it is timing, not the retransmission change that landed in the same commit (which does not reach the connect path). On Windows a refused loopback connect is answered only after the host stack has sent the SYN again and given up; under CI's load that, plus the gateway's own `REDIAL` and the frame's way back, can outlast the 15 s. Where to start: time the refusal on a Windows runner (`connect_timeout` to a closed port, under load), and either make the test's wait follow the gateway's own deadline or have the gateway refuse at its first `ConnectionRefused` | po6-gw2 | 9 |
| **Done 2026-10-05 (po5-gw): the same cause as the *Red on `main`* row, fixed on the same branch:** the retransmission timer ran out before the third duplicate. Was: The gateway host test `gateway::tests::a_lost_segment_is_sent_again_alone` failed once on 2026-10-01 in a full `cargo xtask check` on Windows under load: `left 268435457, right 268435993` at `tools/common/xtask/src/gateway/tests.rs:977` (on 1ff875fc), a sequence number 536 bytes off. It passed 5 of 5 alone. Kept: `~/.local/share/ferrix/logs/os-c8-gateway-flake/check-win.log`. A test that depends on timing under load: likely a retransmit timer firing twice, or two segments coalesced. Root-cause it before the gateway's throughput work widens the window **Seen again 2026-10-03** on nazuna (Linux) under load about 15, in step2b's full check on fc7bcf766 (a kernel-only change): the same `left 268435457, right 268435993`, now at `tests.rs:1082`; it passed alone at once and the check's rerun was green. Kept: `~/.local/share/ferrix/logs/step2b/ld-check-red-gateway-flake.log` | po5-gw |
| `test-auth --arch all` failed once on x86-64 on 2026-10-03, in stage 20's matrix record run on 184fa4c7d at a load of 10 to 26: init's log line `auth.service[533]: auth: service=passwd ... result=changed` reached the console on the same line as `passwd`'s own "the password for ferrix is changed", so the check read neither, and it saw no `authd: running as auth (uid 90)`. A rerun of the same tree right after passed on all three architectures. The check reads the console, which init's journal and the shell share; matching within a line rather than whole lines, or reading the journal, would end it. Kept: `~/.local/share/ferrix/selfhost-matrix/run-2026-10-03/record/auth.log` on nazuna | open | 15 |

### P2 — quality and performance, on the "fast" half of the goal

| Item | Owner |
|---|---|
| The Windows QEMU build (`tools/common/fetch/fetch-qemu-windows.sh`, 11.1.0) does not carry N0g's intel-iommu patch (`tools/common/data/qemu/0002-*`, written against 10.2.1; `docs/NVIDIA.md` §12.3), so xtask refuses an x86-64 QEMU without `(ferrix-cfi)` on Linux hosts only. Until 2026-10-03 the script also read its patches from the pre-relayout `scripts/data/qemu/` and so applied none; it now applies 0001 from `tools/common/data/qemu/`, untested on Windows. Port 0002 to 11.1.0, build with `--with-pkgversion=ferrix-cfi`, run it on a Windows host, then drop the `cfg!(target_os = "linux")` in `qemu::qemu_command` | open |
| CI's `cargo xtask check` cannot run the requirement-reservation refusal for a new id. A pull request's shallow checkout has no `main`, and `check-traceability.py` says so on its summary line rather than failing (`docs/CONVENTIONS.md`, "Requirement ids are reserved before they are written"). Fetch it in the check job, e.g. `git fetch --depth=1 origin main:main`, so the merge-base and `main`'s reservations file resolve; the certification consultant's note on 2026-10-01 | open |
| Seven ring-3 drivers still repeat what `ferrix-driver` (`src/user/system/native/driver`) does: net, snd, vport and gpu, then stm32-ltdc, gc400, usbhid and usbdev. One landing each, adding the subsystem module for its class (net, sound, console, display) and integrating the class's logic crate through an optional cargo feature, as `block` does; gate each with its `test-*` row and `test-restart --boot <kind>`. virtio-input (d5b03142) and virtio-blk (31b16d72) show the pattern. A change that touches `src/lib/proto`, `src/kernel/src/interfaces/`, a gate the certification cites or `tools/common/data` goes to the certification session first | open |
| Zenbleed (CVE-2023-20593) and Gather Data Sampling (CVE-2022-40982), exposed since programs may use AVX (2026-09-30): keep `XCR0` at x87 and SSE on an affected processor that is not mitigated -- AMD Zen 2 without the fixed microcode unless `DE_CFG[9]` is set, Intel Skylake to Ice Lake without GDS microcode (`GDS_NO` / `GDS_CTRL` in `IA32_ARCH_CAPABILITIES`) -- and say so on the boot line. Owed before `docs/certification/SPECULATION.md` §3's claims are cited again (the certification consultant's condition on the `XSAVE` landing). **Done 2026-09-30**: `speculation::vector_leak`, its boot line and a boot check of twelve processors' verdicts (`L.x86_64.123`) | done |
| A 32-bit program's signal frame still carries `FXSAVE` alone (`src/kernel/src/arch/x86_64/signal/compat.rs`), since x86-64's gained its `XSAVE` area with AVX on 2026-09-30 (`docs/CLAUDE-CODE.md` §3): an i386 handler that uses AVX hands the code it interrupted different upper `YMM` halves. Linux's `sigframe_ia32` carries the same `XSAVE` area after the `fsave` environment. A first attempt was backed out on 2026-09-30 (a 32-bit handler using AVX took `SIGSEGV` on entry, cause not found); `docs/CLAUDE-CODE.md` §3 lists what doing it properly takes: the layout against Linux's on paper, both frames, a YMM round-trip check with its negative control, and the certification review | open |
| Claude Code's interactive TUI has no gate (`docs/CLAUDE-CODE.md` §5): drive `claude` in a pseudo-terminal (`sshdt`'s, or foot's) and require its prompt box. Check there that `ioctl` takes only the low 32 bits of its descriptor, as Linux's `unsigned int fd` does: Bun passes NaN-boxed descriptors (`0xfffe000000000001` for 1). **Done 2026-09-30**: the gate types at the TUI on the console, which is a terminal to it, and `fd::arg` already narrowed descriptors | done |
| Linux gates `/proc/<pid>/fd/<n>`'s magic link (`proc_fd_link`) with `ptrace_may_access`, which refuses a same-uid caller against a non-dumpable or set-user-id target; Ferrix only checks the uid, through the directory's 0500 (found in the certification review of the I5b `/dev/fd` change, 2026-09-28) | closed by NP: `credentials::may_access` on `root`, `cwd`, `exe`, `fd`, `fdinfo`, `maps`, `ns/*`; the `procacc` line (FX-0894) |
| `vfs::Access::privileged` is a filesystem uid of 0 with no namespace, so a process of kernel uid 0 that made a user namespace keeps root's file override over a file whose owner its namespace does not map, where Linux refuses it (the N4 review, 2026-10-01; no escalation, the process was root). Make `Access` namespace-aware, with a check of a 0600 read by that process | open |
| `get_robust_list` of another thread checks no permission: Linux requires `PTRACE_MODE_READ_REALCREDS`, and Ferrix answers any caller with the target's robust list head, an address in its memory, which matters against a set-id target's ASLR. It predates NP. os-9f's NP condition in `docs/NAMESPACES.md` §12: NP's `ptrace_may_access` gates it, with a check (recorded in the certification review of the per-thread robust list, f16ab27a, 2026-09-30; no ad-hoc check in the meantime, since it would be a second rule for NP to replace) | closed by NP: `sys_get_robust_list` asks `credentials::may_access` with the real ids, `EPERM`; the `procacc` line asks it through the system call |
| seccomp-bpf, `docs/SECCOMP.md` (designed 2026-09-30, for review by os-9f and os-98). S1 to S6, 20 points: the classic-BPF interpreter in `src/lib/kernel/seccomp`, a filter check registered into the core's system call entry, per-thread chains, `ERRNO`/`KILL`/`LOG`/`TRAP`/`TSYNC`, `test-seccomp` with Linux's own selftest. Stage 13's exit clause is met at S3, and init's L13 `SystemCallFilter=` is unblocked there. S7 and S8 (5 more) retire Steam's `-no-cef-sandbox`, but only with N4 and pid namespaces: Chrome 151 with its sandbox on does not start when `CLONE_NEWPID` is refused (measured, §1.2). Pid namespaces and a loopback-only network namespace are os-98's after N4 (customer, 2026-09-30) | S1 to S6 os-7c (2026-09-30); S7 and S8 open |
| Write domains, `docs/WRITE-DOMAINS.md` (designed 2026-10-01 at the customer's question of how to stop encryption malware; not reviewed, the consultant's seat was empty): each process gets a write domain, the folders whose files it may change, checked in the VFS after the uid/gid check with no root bypass, only ever narrowed. Launchers make it from the unit file and `app.toml`'s `[package.access]`, a trusted chooser hands other files over as descriptors, and read-only btrfs snapshots give undo for what a domain cannot stop. W1 to W3, 18 points (the domain and its checks, Landlock in the personality, init's and the app launcher's domains), stop a packaged app or service from changing anything outside its own folders. W4 is the chooser (8 to 13), W5 btrfs-write Stage C and a snapshot unit (16 to 24), W6 a tripwire (5). The certification claim, if any, is the customer's decision (§7) | open |
| The negative control owed for 2625f09f's `check_a_packet_pipe_keeps_writes_apart` (`src/kernel/src/fs/check.rs`), which the certification consultant (os-9f) asked for, to ride with the next `fs/` landing: a one-line sabotage that lets a packet pipe's read merge two writes must fail the boot on "a packet pipe's read did not stop at the end of one write", shown with a marker, then restored | done with NP: the read merging two writes (`PipeBuffer::read` taking the rest of the buffer) stops the boot with "a packet pipe's read did not stop at the end of one write" |
| Owed by the discovery Finder (cc4e14af, fa4e88a6), from the certification consultant's after-the-fact review (os-ad, 2026-10-01), to ride with the next landing in `src/kernel/src/discovery/` or `device.rs`: (1) the refusals the Finder added have no requirement and no check -- `Stopped::Again` (a second `device::publish`), a tree or board finder's failure halting under `STAGE10_DEVICES`, and the fixed precedence pci, tree, boards (L.device.5 orders nodes, not finders); add the L.* rows and a self-check of a second publish, with a negative control (drop the `PUBLISHING` swap) that stops the boot on the check's own message; (2) L.discovery.1's evidence is host tests of `ferrix_description::choose`, outside the counted lines, with no negative control: show one (`choose` preferring the tree) failing; (3) a gate that only `device.rs` and `discovery/` call `DeviceNode::{empty,mint,pci}` and only `discovery/devmgr.rs` and the checks build `Object::Device(` -- today they are crate-visible, so a load-ring defect could mint a device handle over any physical range, and no gate would notice (the first step of `docs/certification/CLAIM.md` §3.3's narrow item facade); (4) the PCI walk's catalogue entry is chosen by matching the string `"pci"` (`main.rs:724`), and `device.rs`'s `unwrap_or(&OutOfMemory)` reports "no memory" for a finder that fails without a reason: make both typed; (5) `discovery/mod.rs` and `interfaces/mod.rs` are item- and load-ring files through which core code names core modules (`crate::discovery::acpi`), accepted by the consultant (os-ad, 2026-10-01, after the fact: f8ab2970 cited an acceptance nobody recorded) on the term that each holds `mod` declarations, attributes and documentation only -- no item and no `pub use`. No gate enforces that term: add one to `check-item-boundary.py` with a self-test case | open |
| Owed by N4 (d171ffe5), from the certification consultant's after-the-fact review (os-ad, 2026-10-01; `docs/NAMESPACES.md` §12), to ride with the next landing in `syscall/userns.rs`, `credentials.rs` or `fs/userns_check.rs` (N5 or NP at the latest): (a) the negative control N3's review asked for the `EROFS` write through a read-only bind of `/proc/sys` -- run it and record its message; (b) `VULNERABILITY-ANALYSIS.md`'s U8 row says no `DAC_OVERRIDE` in a child namespace "at all", but `vfs::Access::privileged` is a filesystem uid of 0 with no namespace, so a namespace kernel root made keeps root's file override (no escalation; the process was root): restate the row, and add a 0600 read to the `root_made` check, or make `Access` namespace-aware; (c) `userns::ACTING` answers for any task while a check runs, after the secondaries and devmgr have started: answer only when `sched::current()` is the check's task; (d) the row for the control "`privileged()` ignoring the namespace" quotes `userns_check.rs:330`'s message, and the boot stops at `:326` first: quote the message that fires | **Done 2026-10-01** with the small namespaces (`docs/NAMESPACES.md` §12, *Settled by the small namespaces' landing*): (a) the `EROFS` control fires "a write through a read-only bind of /proc/sys was not refused EROFS"; (b) U8 restated, `kernel_root_keeps_override` checks the difference; (c) `ACTING` answers only for the task that set it; (d) the control's message on today's tree is `root_made`'s "privileged in the whole system's sense", the host name's only before the UTS namespace. Marked here 2026-10-05 (po5-c1) | done |
| `check-complexity.py`'s baseline keys a function by `file::name`, so functions of the same name in one file share one entry: the last one measured wins, and a new function over a floor can hide behind a baselined one of the same name (pre-existing; seen with the several `fmt` in `src/lib/fs/btrfs/src/lib.rs` when btrfs joined the item, branch `btrfs-cert-boundary`, the certification consultant's note of 2026-10-02). Key by `file::name@n`, the n-th of that name, or by the enclosing `impl`'s type, with a self-test case of two same-name functions | open |
| Owed by NVIDIA's N1e, landed ahead of its conditions on 2026-10-03 (the certification consultant's ledger 294 and 297), due before N2 lands or before `nvrm` goes into any image but `run-nvidia`'s: ~~N10, the chardev core's bounds in MEMORY-AND-TIMING~~ done 2026-10-05 (po5-c1, po6-c1): the text (`docs/certification/MEMORY-AND-TIMING.md` §2.2k) found the queue to `nvrm` unbounded, F-63 (the consultant's ledger 361), now closed: abandoned requests leave the queue and admission counts it against its room; ~~N12, boot self-checks with a kernel-side fake driver (the HELLO rules, a request round trip with copies, an abandoned request's copies refused, driver death waking waiters with ENODEV, the 257th request EBUSY, a forbidden minor refused, a copy naming another control's request, a copy after the reply, abandonment during an in-flight copy, a duplicate minor, a duplicated driver handle, and L1's drain after a reply); N13, gate.sh controls for each (the lookup ignoring the control, abandonment not removing the request, the drain skipped, the name allowlist skipped, the 257th request admitted, `DUPLICATE` left on the driver end, and the reply-time drain skipped)~~ done 2026-10-05 (po5-c1, po6-c1): stage 10's chardev self-check (`src/kernel/src/interfaces/chardev/check.rs`, FX-1013) with the queue's bound added, and the seven controls plus F-63's two fired (FINDINGS F-63); ~~N15, NVIDIA.md §4.4's text on what is deferred and an ITEM.md recount~~ done 2026-10-05 (po5-c1): §4.4's "As built in N1e", and ITEM.md §2's composition-root count, 28 with `chardev`; N5, the copies switched to the window-refusing mode when fault windows land | open |
| Owed by `PIN_CONTIGUOUS`, landed ahead of its conditions on 2026-10-04 with N2–N6 (the certification consultant's ledgers 293 and 318, O1): D7, requirement ids reserved and released at landing, split so one check proves each of (a) a contiguous pin's addresses are one ascending run or it is refused `BAD_STATE` with nothing changed, (b) a committed page in the range is `BAD_STATE`, past the cap `INVALID_ARGS`, a file VMO refused, (c) a refused pin leaves the VMO's committed count and the job's charge unchanged, (d) the run is charged to the running job, each with its self-check; D8, the four gate.sh controls (page-by-page commit, give-back skipped, charge skipped, anonymous test removed) on check, the x86 KVM boot and armv7a `--smp 2`; D10, the coverage justification for the new core and item lines (coverage-owed.json's row); due before N3b lands or before `nvrm` goes into any image but run-nvidia's | open |
| Owed by `PIN_CONTIGUOUS` (ledger 293 D2, ledger 318): the display core's `commit_contiguous` (`interfaces/display/mod.rs`) still takes its frames with `allocate_frames`, uncharged; charge them to the job as `mm::allocate_user_run` does, or say in DISPLAY.md why the card's buffers stay uncharged | open |
| Owed by init's `.device` units, landed ahead with N2–N6 on 2026-10-04 (ledgers 310 and 318, O2): E1, a device start waits a bounded time, says so in a named failure line and fails its dependents (today it waits with no timeout, `src/lib/init/svc/src/manager/kinds.rs`); E2, Requires= or BindsTo= stated for a device that goes away and what an `nvrm` restart does to hyprix; E3, rescan on inotify queue overflow and `IN_IGNORED`; E4–E5, the host tests named in ledger 310; E6, a test-init stage with a device that appears late and one that never does, with a gate.sh control | open |
| Owed by `devfs::announce`, landed ahead with N2–N6 on 2026-10-04 (ledgers 310 and 318, O3): D2, the first-namespace-only note in INIT.md §4.2; D3, a debug assertion that preemption is enabled where `resolve` may sleep; D5, a check that a watch on `/dev/dri` sees card0 created and deleted across a display driver restart, with the `announce` call removed as its control | open |
| Owed by displayctl v8's copying driver, landed ahead with N2–N6 on 2026-10-04 (ledgers 310 and 318, O4): C3, kernel self-checks (copies from a device that does not qualify refused, a qualifying driver gets exactly `READ | MAP`, its write mapping refused, a driver without the flag gets `CARD_VMO_RIGHTS`) and the host test of a 676-byte v7 HELLO refused; C4, gate.sh controls (eligibility check skipped, `WRITE` added) on check, x86 KVM and armv7a `--smp 2`; C10, DISPLAY.md §2.1 restated; F-61's fix; the GPU copy engine as the way back to a driver that cannot read the pixels; a comment at `chardev::publishes_for` saying why the device, not the process, is enough (one driver per device under CLAIMS) | open |
| Owed by NVKMS in nvrm's core, landed ahead with N2–N6 on 2026-10-04 (ledgers 310 and 318, O5): K1, provenance of `os/kept/nvidia-modeset.c` and `nv-modeset-interface.c` (upstream tag, each file's sha256 and diff, the third-party notice); K2–K3, ledger 289's loader checks and controls re-run on the 15 MB core; K4, `/dev/nvidia-modeset` not registered, or only through ledger 294's allowlist; K5, every rectangle `kms_copy` takes checked against the card mapping and the VRAM surface, with a host test; K7, the first-light log cited by hash | open |
| Owed by N2's Vulkan path, landed ahead with N6 on 2026-10-04 (ledgers 294, 300 and 318, O6): ledger 300's M1–M12 (chardev mmap self-checks and controls, the loader's boundary pages, `map_window_typed`), with the N1e row above (its N10, N12, N13 and N15 done 2026-10-05) | open |
| Owed by NVIDIA's N1f: a `test-nvidia-smi` gate on the RTX 3060 (`run-nvidia`'s boot, judged by `nvidia-smi` exiting 0 and listing the card), with the shared-domain guard; watch for EINTR from an ioctl abandoned on a handled signal (the consultant's advisory, ledger 297) | open |
| FX-1006, "a driver the manifest names is not in the image", is printed for any failure of the kernel's read of a driver's image, including an image larger than the read's one heap block of 4 MiB (`MAX_ORDER` 10): N1b's 10 MB `nvrm` stopped stage 10 with it while every file was unpacked (`docs/NVIDIA.md` §10, N1b and N1c). The message should name the cause (too large, missing, read error). `nvrm`'s build now refuses an image at or over 3670016 bytes so it is never the symptom there. Changing `panic/catalog.rs` and the read's error is an item change and goes to the certification consultant; raised by its N1c review (2026-10-03, ledger 289, condition 8) | open |
| Classify `ferrix-pci`, `ferrix-acpi` and `ferrix-paging` as `item` crates in `tools/common/data/certification-item.json` (`crates`). The item's DMA isolation and interrupt masking rest on them (`ferrix_acpi::dmar`'s scopes, `ferrix_pci::topology`'s own-requester rule, `ferrix_pci::msi`'s mask registers, `ferrix_paging::coherence`'s record of VT-d table writes not yet cleaned, F-58, and `ferrix_paging::vtd::queue`'s invalidation descriptors, N0g; ITEM.md §2), yet the complexity, fallible-allocation, unsafe-trace and coverage gates do not read them. Classifying them puts them under those gates; raised by the certification consultant's review of `nvidia-n0` (2026-10-02, condition 5), `ferrix-paging` added by its review of the F-58 fix (condition 4) | open |
| Classify `ferrix-fallible` and `ferrix-sync` as `core` crates in `tools/common/data/certification-item.json` (`crates`), under F-56. Both are linked and named by core-ring files (`fallible.rs`, `mm.rs`, `mm/reserve.rs`, `sync.rs`), hold `unsafe`, and are read by none of the item-scoped gates; since 2026-10-02 they sit in `crates.infrastructure_allowlist`, where their own dependencies are checked only once an item crate depends on them (the consultant's control K6). Classifying them puts them under the complexity, fallible-allocation and unsafe-trace gates and the crate dependency rule unconditionally | open |
| Steam's GPU process (`docs/STEAM.md` §6, designed 2026-10-01 with os-9f's conditions, no code yet): user copies through a device window's own mapping through a per-CPU slot (F-55's second landing, 5 to 8 points), the render node for a `render` group with a per-open bound and an audit of `interfaces/render` (5 to 8), and the web helper's ANGLE-on-Vulkan flags (4 to 7). Retires `-cef-disable-gpu` | open |
| `test-steam-game` (`docs/STEAM.md` §1 and §7, branch landed 2026-10-01, not passing yet): Teeworlds is claimed for the test account, the gate asks the client for the install, presses Install, and Steam downloads all of it in about 90 s, then stands still while it stages the files: no progress in its logs for half an hour, the 32-bit client busy on about three of four processors. The cause is not known. A `find`/`stat` over `steamapps/downloading` from another process never returns, on the btrfs volume and with the library on tmpfs alike (`game-watch.sh` links it to `/tmp`), so a directory listed while a program writes into it is a lead of its own; whether the staging itself stalls on tmpfs was not known at the wind-down (§7, item 7). `du` on the volume said 0 KiB with 79 MB staged. Then: whether Steam starts a `native` game in the Steam Linux Runtime 1.0 (scout) container it installs beside it, which needs pressure-vessel, and the game's window drawn. Logs on nazuna: `~/.local/share/ferrix/logs/steam-game/` | open |
| Steam's volume has no `xdg-utils`: with the Install dialog's "Create an application shortcut" ticked the client ran `xdg-icon-resource`, found none, and aborted ("pure virtual method called", exit 134, 2026-10-01); `test-steam-game` unticks it (`docs/STEAM.md` §3) | open |
| Guest downloads past one connection's 10 to 15 MB/s (3b1b1de7, 020dc9b2), toward the host's 300 Mbps: window scaling and SACK in xtask's gateway TCP (it advertises a fixed 32 KiB window), a shorter turn than 5 ms, and the in-flight budget swept with many connections, as Steam's dozen are; then a `test-net` floor in MB/s so it stays. About 14 points, 5 to 7 hours | open |
| Two rules for later work on this landing's calls (os-9f, 2026-09-30), kept here so nobody builds past them: a uevent broadcast must never pass a program's send off as the kernel's (`docs/SYSFS.md` §6); and `name_to_handle_at`, which answers `EOPNOTSUPP` for every name (`src/kernel/src/syscall/path.rs`), must not return a real handle before `open_by_handle_at` requires `CAP_DAC_READ_SEARCH` in the first user namespace only, since a container holding it can open any file on the filesystem by handle, past its mount namespace (the "shocker" escape) | open, a rule rather than work |
| What `fork` and `execve` still do not do to a process's attributes (left by the no-new-privs landing, 2026-09-30, which made `fork` and a native `process_create` pass on `no_new_privs` and dumpability and `execve` decide dumpability as Linux does): a fork child starts from the defaults for the nice value, the resource limits, the personality, the `PR_SET_NAME` name and the I/O priority, all of which Linux's `copy_process` copies (`inherit` in `src/kernel/src/syscall/attributes.rs`), so `ulimit` or `renice` in a shell does not reach the programs it starts; `execve` keeps a `PR_SET_NAME` name, which Linux replaces with the program's; and `execve` of a program the caller may run but not read leaves it dumpable, where Linux's `would_dump` makes it not dumpable. And (os-9f's follow-up to that landing, not blocking) a child is findable with the default attributes between its publication and `attributes::inherit` -- after `registry::publish_forked` for a fork, after `registry::register` in `exec::load_native` for a native child -- so for that moment it reads dumpable while its parent is not. Nothing reads another process's dumpability for an access decision today, but NP's `ptrace_may_access` on `/proc` links will: before NP, give the child its attributes as its entry is made (the inheritance done at publication), or show the window cannot be observed | closed by NP: `Process::attributes_pending`, true from a fork child's making (and a native child's with a creator) until `attributes::inherit` ends it, makes `may_access` read the child as not dumpable, so the window refuses and does not leak; the `procacc` line has the case and a control (the flag ignored) |
| The user's own launcher script's toggle never takes fuzzel away: in `~/.local/bin/hypr-launcher`, a second `SUPER R` runs `pkill -x fuzzel`, but on Ferrix it starts a second fuzzel instead (seen 2026-09-27 in `test-compositor --boot fuzzel-user`). The desktop image's busybox has `pkill` with `-x`. `Process::comm` (`src/kernel/src/syscall/process.rs`) should read `fuzzel`, and `/proc`'s process list (`list_root` in `src/kernel/src/fs/procfs.rs`) is not scoped by job. Neither explains the miss yet. Where to start: a boot that reads the running fuzzel's `/proc/<pid>/stat` and `/comm` and compares them with what `pkill -x` matches. `fuzzel-user` checks only that the bind opens fuzzel. A lead, seen 2026-10-04 in `test-init`'s revoke stage: a busybox applet run through its link (`/bin/cat` -> `busybox`) shows `(busybox)` in `/proc/<pid>/stat`, where Linux names it after the path it was run by, `cat`; if `comm` is taken from the resolved file, `pkill -x` misses any program run through a link. Also check fuzzel's single-instance lock: its path comes from the absolute `WAYLAND_DISPLAY` and cannot be created (`/tmp/fuzzel-/tmp/wayland-1.lock`) | ferrix-d5 |
| Chrome drops video frames with the host only moderately loaded. `cargo xtask bench-chrome-video --gl --accel kvm` dropped 13–27% of a 720p30 video's frames at a load of 16–27, glibc and ferrousli alike, after the futex buckets (df446dc3), with the guest 14–18% busy. The compositor drew about 45 frames a second, each taking 6–15 ms, mostly its `flip` to QEMU's GPU. Where to start: whether Chrome's `BeginFrame`s follow hyprix's frame callbacks late, so its video compositor misses deadlines, and whether a flip must finish before the next frame callback goes out. Logs: `~/.local/share/ferrix/logs/yt-ab/after-glibc-lowload.log` and `after-ferrousli-1.log` on example; `docs/CHROME.md` §9 **2026-10-02 (the Steam-performance session):** at a load of about 2 on nazuna, on the `--everything` desktop, a 720p30 VP9 video drops 0.5–5% of its frames and a 720p60 one 7–9% on main of the morning; hyprix sending its frame callbacks before the flip, not after (a73c4357e), brought the 60 fps video to 2–6%, and the GPU driver no longer stalls on a status read per interrupt (445b38333; the guest 24% busy where it was 16%). Chrome's BeginFrames run at 30 Hz for a 30 fps video by its own choice (`FrameRateDecider`). hyprix never said `wp_presentation`'s clock and stamped presentations with the wall clock, so Chrome's BeginFrames jittered 1.25 ms frame to frame; with the clock said and the pace's refresh grid as the time they are exactly 16.67 ms apart (branch `present-clock`), and a 1080p60 video maximized drops 1.6–5.1%, mean 3.8% (4.5% before). What is left of the drops is not the vsync timeline: viz draws 590 of 600 BeginFrames. Next: Chrome's media log (DevTools `Media` domain) for why the rest are dropped, a real video rather than `testsrc2`, and an asynchronous flip in hyprix (its loop still waits 5–15 ms on each). **Later the same day:** with the flush on a thread of its own (cd6cab079) and the presentation clock said (297e40931), real YouTube (Big Buck Bunny, 1080p60, embedded from a page on the host, Chrome maximized) drops 0 of 3,602 frames in four 15 s probes at a host load under 3, page `requestAnimationFrame` at 60.0 with no interval over 16.8 ms; under the load of a gate run beside it, 0–2.5% with 150–300 ms hitches. What is left is the host's load, as the row says. **2026-10-02, 120 Hz:** with the screen at 120 Hz (`monitor = , 1920x1080@120` on the `--everything` desktop) Chrome heard of each frame 3.1 ms after its time, because hyprix queued the frame callbacks and wrote them only after drawing the frame; sending them at the frame's start (d7df65cff) brought that to 1.0 ms. Maximized, a page scrolled every frame with a 60 fps video in it now runs at 119.1-119.6 frames a second (720p60 and 1080p60 players, 0.2-0.4% of the video's frames dropped), 117.0 with a 1080p video scaled into a smaller player (1.8%; viz's software draw of the scaled frame is 4.9 ms), and youtube.com's own watch page at 111-117 (0.6-0.7%), its hitches YouTube's script while the page loads below. Tools: `~/ferrix-logs/yt-perf/measure.sh`, `bfgaps.py`. | open |
| yserver's Steam-performance commits are on the fork's local branch `steam-perf` (`~/.local/share/ferrix/yserver-perf/src` on nazuna: 9c9be67 a client's writer fd in the poller only while output waits; c8dffec a window handed to hyprix straight from the readback, damaged by rows; a10fc63 depth-24 alpha set a word at a time; the workspace's 3,113 tests pass) and not on GitHub: `docs/STEAM.md` §8. They need the toolkit's `draw_pixels` (17f8591ef) on the fork's pinned `compositor-toolkit`, so: push Ferrix's `main`, move the fork's toolkit pin to it, push the branch to `ferrix-os/yserver` `ferrix`, then move `YSERVER_COMMIT` in `fetch-yserver.sh` and gate `test-yserver` and `test-xwindow --everything`. Waits on the customer's word for the pushes. **Done**: all six were on the fork's `ferrix` by the pin `57e57efc`; on 2026-10-03 upstream's `master` was merged into `ferrix` (`43598ceb`) and the pin moved there | done |
| **Done 2026-10-02 (cd6cab079):** hyprix's loop waited for each flip (`DIRTYFB`, 5–15 ms under VNC, up to a repaint under QEMU's GL window). It now flushes what the GPU drew from a thread of its own with a second descriptor on the card (`crate::flush`, compositor-drm's `Flusher`), at most one flush waiting and later frames merged into it; a copied dumb buffer is still flushed in the loop. hyprix's frames went from 6–7 ms to 2.5 ms, and Chrome's 1080p60 video from 4% dropped to 0.1–0.3%; real YouTube at 1080p60 maximized dropped 0 frames in 60 s with the host quiet | done |
| **Done 2026-10-03 (branch `mincore`):** `mincore` was `ENOSYS`: Chromium's memory dumps log `CountResidentBytes failed` dozens of times a minute, each line through the serial console. Answering it (every mapped page resident, as a page cache that never evicts would) quiets them. It now does, with Linux's `EINVAL`, `ENOMEM` and `EFAULT`; `test-procfs`'s `mincore` step holds it | done |
| A job's processor load can still take in a task after its last leave, found reading the FX-0905 fix (2653b567): a task woken from another processor is joined to its job's load in `Task::set_state`, and if that processor is stalled mid-`join_group` while the task has already exited and left, the join lands after the leave. The quota check's spinners never block, so FX-0905's check cannot see it; the processor share would stay counted for a gone task. Separately, `quota::adjust`'s busy/idle flip can drift across processors, which W-13 (`docs/certification/IMPLEMENTATION.md`) already records. Where to start: make the join and the task's liveness one step, as the FX-0905 fix did for the leave | open |
| `run-compositor --everything` (and so `--chrome`): Chrome's window sometimes never paints, and sometimes goes blank once it is clicked and typed into, seen on 2026-09-26 on x86-64 under KVM, on main 24fb419b plus the host-layout default. Of seven boots on the btrfs root and on tmpfs, four painted the welcome page; one had no window at all after 40 s, its network service ending with "Terminating current process after 15 seconds with no connection"; one showed an empty window before any input; and every one that was clicked into and typed at (two of two, with `us` and with `de`) went blank and stayed blank, though Chrome still asked for cursor shapes and printed no error. Not the keyboard layout: both `us` and `de` did each. Chrome's binary names `/usr/share/X11/xkb`, which neither volume carries; a boot with XKB data under `XKB_CONFIG_ROOT` painted and then went blank on the click like the rest, so that is not it either. `test-chrome-window` does not click. Kept: `~/.local/share/ferrix/logs/b3-chrome-blank-2026-09-26/` on example, the five serial logs and their screenshots. | open |
| Chrome on the desktop, what ferrix-a8 left on 2026-09-26: the toolbar's extensions (puzzle) bubble never appears, and hyprix logs only `window 2 is urgent`; a client that never stops being flooded still overflows the 1 MiB socket queue (971a03f4; the scratch control that put the old drag flood back queued 1,052,588 bytes and then dropped Chrome); and `test-chrome-window` has no context-menu step, so the fixes for Chrome dying after a menu's Copy and after a drag (343409d5, f4ce9a7f) have no gate | open |
| `two_clients`' `a_window_moves_through_the_frames_between_two_layouts` ("only 7 frames") and `a_plugin_adds_a_dispatcher_and_the_compositor_hands_it_over` ("the plugin's dispatcher did not swap the windows", 704220 channels, the frame before the swap) failed together on 2026-09-26 (clients/waybar) in `cargo test --workspace` of branch `waybar` rebased on the relayout (4ae00e73, a new crate hyprix does not link), at a host load of 73 while about twelve sessions rebuilt after the move; `cargo test -p hyprix --test two_clients` passed at once at the same load. Both count frames or take a screenshot after a fixed sleep, as the other load-sensitive tests of that file do. Kept: `~/.local/share/ferrix/logs/waybar/flake-load73-4ae00e73.log`. Seen again on 2026-09-27 at 01:15, alone, 709072 channels, at load 24-28 on `clients-base` (main 83bcd5e7 plus xtask's dotfiles and the caption client, which hyprix does not link); the rerun passed. Kept: `~/.local/share/ferrix/logs/clients-base/g4-check-plugin-flake.log`. Seen again on 2026-09-27 at 02:16 and 04:30, alone, at load 26-29 on `fuzzel-window` (which gives an interactive layer surface the keyboard; the test makes none); the reruns passed. Kept: `~/.local/share/ferrix/logs/fuzzel/w-test-plugin-flake.log`, `w4-test-fail.log`. | unowned, found by clients/waybar |
| hyprix stops drawing two windows' opening animation when they map within a few milliseconds of each other: it draws three or four frames and no more, so the last frame is caught half-way (both windows at an earlier size: the checkerboard's squares inverted, the gradient 4 off). Found 2026-09-26 while fixing `two_clients`' swapped-window flake: with the second client started the moment the first was drawn, `a_turned_monitor_is_tiled_tall_and_drawn_turned` failed 4 of 12 whole-binary runs and 4 of 6 alone, always 374456 channels, reporting `frames 3` or `4` where a passing run draws 30 to 52; a quarter of a second between the two maps passes every time, which is why `until_drawn` keeps it. A separate observation from the same work: with every two-client test in that file waiting on its first window and then the quarter-second, `a_plugin_adds_a_dispatcher_and_the_compositor_hands_it_over` failed 5 of 10 (709072 channels, the windows not swapped), so its chain of fixed sleeps is the next wait to replace. Logs: `~/ferrix-logs/fix-gpu-frame/` on example (`turn-loop-*`, `final-loop-*`, `turned3-*.ppm`). A likely cause, not yet shown to be this one: hyprix's loop owed no frame after one that started an animation, which left the submap boot's swap undrawn (fixed 2026-09-30, the red row above); with that fix in, try the maps without the quarter-second first. Both tests pass 10 of 10 alone with and without it while the waits stay. 3 points | open |
| Chrome on ferrousli, what ferrix-c7 left on 2026-09-26 (`docs/CHROME.md` §8): nothing but Chrome loads more than 64 objects, so the loader's 256 has no test of its own -- a program with seventy `DT_NEEDED`s in `ld/tests/link.rs` would give it one (1); ferrousli's `ld.so` cannot be run as a command, only reached by `PT_INTERP` (3); `getcontext`, `setcontext`, `swapcontext` and `makecontext` on AArch64 and ARMv7-A, which only x86-64 has (`cargo` imports them) (3); and why the GPU process's fallback stops at the sandbox's `proc_util.cc:115` with `ENOENT` on Ferrix (§8, item 8; unsized). the GPU is the roadmap's Chrome row (`inotify` landed 2026-09-27) | open |
| `cargo xtask check --ferrousli` failed twice in a row on 2026-09-26 in `c_mount`'s `file_system_statistics_and_the_calls_that_mount_swap_and_sync`: "mount/filesystems-O0: still running after 30s". The program calls `sync()`, which flushes every file system on the host, and example had 2 GB of dirty pages from other sessions' builds: a bare `sync` there took 19.7 s at the same moment, at a load of 37, and a third run passed once the host had flushed. Not the library: the tree was branch `chrome-window-ferrousli` (main f670d5ad plus the loader and `posix_fadvise64`, which the program does not call). The test's time limit, or its host-wide `sync()`, needs to allow for a host that is flushing; `syncfs` on the scratch directory proves the same wrapper without waiting on everyone's disks. Kept: `~/.local/share/ferrix/logs/cwf-gate-98c9740f-on-f670d5ad/check-ferrousli.log` and `~/.local/share/ferrix/logs/cwf-gate-98c9740f-on-f670d5ad/rerun/check-ferrousli.log` on example. **Twice more the same evening, around 23:30**, gating the futex buckets (ferrix-41b, 7c12061b, which reaches nothing of ferrousli's), at loads of 28–45; a bare `sync()` took 2.7 s just after. Kept: `~/.local/share/ferrix/logs/yt-check-c_mount-timeout-7c12061b+.log` on example | open |
| hyprix advertises `wp_fifo_manager_v1` and `wp_commit_timing_manager_v1` and ignores their requests (`server/src/client/frames.rs`, `Role::Fifo \| Role::CommitTimer`): Mesa's Vulkan WSI then stops using frame callbacks and a FIFO client runs unthrottled -- vkgears drew 3600 fps against a headless hyprix. Implement the barrier or stop advertising both; also `wp_presentation` never sends `clock_id` after bind, and `now_monotonic()` (`hyprix/src/state.rs`) is the wall clock, so `presented()` carries it. Repro: `~/ferrix-logs/os-ac/hyprix-host.sh` on example | open, found by os-ac |
| zinc can exec a pipeline's last stage in place again: orphans are reaped now -- init is pid 1 since L10, and hyprix reaps what it starts and, when it is pid 1, every orphan (`src/user/system/linux/compositor/hyprix/src/children.rs`, ferrix-e4 2026-09-26) -- so zinc's "Run a subshell's last external command in place" workaround and the fork a prompt it costs agnoster's `$(jobs -l \| wc -l)` can go. 1 point | open |
| The DK1 desktop, what 2026-09-24 left (`docs/ROADMAP.md`, the desktop at the speed of a hand): pointer motion off hyprix's frame thread, since a frame being drawn still holds the pointer (5); the first blur of a translucent window, 0.6 to 1.9 s in f32 (3); the Cortex-A7 at 800 MHz with VDDCORE raised through the STPMIC1 first (5); U-Boot's saved `bootdelay` of 2 s and OP-TEE's 1.4 s finding its device tree, which are firmware | open |
| The cost of the 20 µs one-shot armed on every wake onto the caller's processor, measured on pipe and futex paths | open |
| `EPOLLRDHUP` and `EPOLLPRI` are never reported, a written deviation of the epoll landing (d047480d): `Readiness` carries neither a half-closed peer nor urgent data | open |
| Per-CPU frame and heap caches, deferred since stage 2 | open, once a workload can measure them |
| The board's `HDMI-A-1` has no `EDID` property: `src/user/system/native/drivers/display/stm32-ltdc` reads its monitor's EDID and never hands it to the display core, so hyprix has no description there unless `drm.edid_firmware=` names a file. A displayctl message carrying the blob (protocol 7) for the core to serve beside the override, and `GETCONNECTOR`'s `mm_width`/`mm_height` from whichever EDID the connector has (`docs/DISPLAY.md` §7, "Not done"). 3 points | open, display |
| `Inode::ioctl`: `sys_ioctl` special-cases the console, sockets and `/dev/dri/card<N>` by the open object's type; a hook on the inode replaces the three branches (os-02's review of the display stack, 2026-09-16). `/dev/input/eventN` (`docs/INPUT.md` §3.3, L6) would be a fourth special case | open, kernel VFS owner |
| Checked register offsets in the ring-3 virtio drivers: `Block::read`/`write` in `src/user/system/native/drivers/block/virtio-blk` and `src/user/system/native/drivers/display/virtio-gpu` assert on a device-controlled `notify_off` × multiplier, and on ARMv7-A `offset + size_of::<T>()` can wrap past the bounds check; one shared checked-offset accessor for both (os-02's review, 2026-09-16) | open, driver owner |
| devmgr's `await_published` kills a driver that exits without publishing and marks it dead, but never quiesces its device, as it did not before that change either. The IOMMU mappings go with the process, so this is hygiene rather than safety: quiesce the device once its driver is gone. Stage 10 (os-02's review of the display stack, 2026-09-16) | open, devmgr owner |
| ASIDs and PCIDs, so a switch stops invalidating every user entry | open, after threads |
| A gate for btop: a program in `test-net` or `test-vfs` on x86_64 that runs `btop` under `timeout` on the console and requires its panels' titles in the output, with a negative control that shows the check fails when btop cannot start (the 4 MiB `execve` limit it hit is the obvious sabotage). 2 points | open |
| A gate for sshdt: a `test-net` program on x86_64 that starts `sshdt` with a key `xtask` generates, and a `--forward` through which the host's `ssh` runs a command and requires its output, with a negative control (no forward, or a key the server was not given) that shows the check fails. Needs an `ssh` client on the gate host and on CI. 3 points | open |
| `cargo xtask run`'s serial shell has nothing to start sshdt from; `run-compositor --ssh <port>` does, since 2026-09-21 | open |
| No `/etc/os-release` in the image: `cat /etc/os-release` over `ssh` says the file is not there, and it is what a client asks a machine what it is with. A line or five -- `NAME`, `ID`, `VERSION_ID`, `PRETTY_NAME` -- carried like the other `/etc` files. 1 point | open |
| `mlock` and `munlock` are `ENOSYS`; sshdt (through `russh-cryptovec`) warns once per run. A resident-only kernel can accept them as no-ops within `RLIMIT_MEMLOCK`. 1 point | open |
| uutils' `tty` on a PTY prints `/dev/pts/0` with no newline after it; busybox's `tty` prints both, and so does every other program over the same sshdt session. Found 2026-09-21 through `ssh -tt`; not yet narrowed to uutils or the terminal. 1 point | open |
| `test-vfs` on AArch64 and ARMv7-A fails command 17, the permission check, whatever the busybox: it expects uutils' `cat: /tmp/dac-private: Permission denied`, and the Arm images carry no uutils, so `cat` is busybox's, which says `cat: can't open '/tmp/dac-private': Permission denied`. Seen 2026-09-23 with Alpine's musl busybox and with ferrousli's alike; no gate runs `test-vfs` on Arm. Accept either wording, or carry uutils there. 1 point ferrousli's busybox on Arm fails the same command the same way (`~/ferrix-logs/ferrousli-arm/2026-09-23-vfs-aarch64-90b0a1de.log` on example) | open |
| ferrousli's Arm suites in CI: they run by hand under qemu-user (`src/user/system/linux/ferrousli/README.md`), and CI runs x86-64's alone. Needs the cross gcc, QEMU's user mode and two rustup targets on the runner, about ten minutes each. 2 points | open |
| The ports built natively on Windows: `cargo xtask ports` runs `src/user/system/linux/ferrousli/tools/ports/` in WSL there, since they need gcc and the host's UAPI headers; `build-windows.sh` beside each, as busybox has, with clang and Alpine's pinned headers, would need no WSL. libc++ with clang is LLVM's own configuration. 5 points | open |
| `/dev/rtc`, and keeping the clock right after boot: `CLOCK_REALTIME` starts from firmware's `GetTime` and then drifts with the counter, with no NTP and no RTC driver to correct it. The random generator is seeded (firmware, `RDSEED`/`RDRAND`, `RNDR`); a virtio-rng driver would reseed it while running | open |
| The debt the roadmap names: Miri for `frame`, `heap`, `paging`; fuzz targets for `cpio`, `fdt`, `acpi`, `virtio`, `linux-abi` | open |
| Every gate's log names the tree it ran on: `xtask` prints `HEAD`, the branch and whether the tree was clean as the first line of every `check`, `test-boot`, `test-shell` and `test-vfs` log, so a row's evidence pins its commit by itself rather than by the runner's word (asked for by a review of the frame-window evidence, 2026-09-13) | open, cross-cutting |
| The host-test table in the roadmap generated from `cargo test --list` with a gate, instead of counted by hand | open |
| The POSIX measure: musl's libc-test functional and conformance programs built static against musl and against ferrousli, run under `test-shell` on all three architectures, with the pass count in the roadmap's host-test table and every failure filed with its owner | open |
| Zero-copy block reads: pin the page-cache pages themselves as the block ring's buffers, removing the data-VMO and scratch copies of stage 11's first read path (ARCHITECTURE §3) Re-costed against the seam measured, 1 (2026-09-27): the two copies are about a microsecond of a 4 KiB read's 300 us round trip under KVM, so this row saves under 1% until the hop and the depth-32 stall are cut | open |
| **Done 2026-09-27:** the block ring's depth-32 stall. It was not a missed wake-up: instrumentation showed every slow read waiting in `ferrix_block`'s queue while the ring task ran, passed over by the one-way elevator until `mq-deadline`'s 500 ms read expiry, a spinning disk's setting. Ring disks now use `Config::fast_device()`, a 25 ms read expiry. Depth-32 p99 went from 112–228 ms to 28–35 ms under KVM, with the median unchanged at 2–3 ms. A host test is the negative control: a read behind the elevator starves under the default and is bounded under the new config. Logs and the instrumentation patch are in `~/.local/share/ferrix/logs/stall/` on example | ferrix-55b |
| Cut the trip to ring 3: a 4 KiB disk read through the block ring and its driver costs 300 to 844 us on x86-64 under KVM against stock Linux's 27 to 48 us, which is an estimated 17 to 56% of a cold `rustc` run (`docs/OPAQUE-KERNEL.md`, *The verdict of S0*). The depth-32 stall is fixed; next, find where the time goes (the driver's `device_ticks` splits off the device's share), PCIDs so a switch keeps the TLB, then remeasure with `cargo xtask bench-seam` and the `seam` boot line, aiming for within twice Linux's per trip. It pays off whatever is decided about the opaque kernel. **Advanced 2026-09-30 (os-35):** the trace landed (5cc5ed38, `seam-trip` and `seam-count`). Baseline, KVM at two processors: p50 230 us; per read 13 switches, 2.4 roots written, 3.2 IPIs, no recheck rescue, 60 to 85% of wakes on another processor. What found it, the eleven-step plan, and which branch holds each step: `docs/OPAQUE-KERNEL.md` § 8 | open |
| A native channel round trip under a microsecond (`docs/OPAQUE-KERNEL.md` §9, the customer's goal of 2026-10-01). On `main`: the speculation domain (bf9efba95), which removes `IBPB` between two programs of one marked job. On `os-ipc/zircon-trip`, WIP and not reviewed: `bench-ipc`, the lazy timer and the decision at a call's end, the sync wake, `channel_write_read` (0x1013) and the clock; §9.4 lists what to cherry-pick and what to drop. Measured p50 on one processor under KVM: 37 us on `main` before, 6.5 us on the branch, and 2.8 to 3.0 us inside a domain, all with mitigations on. Left for under 1 us: land the branch's items with their two owed checks (items 3 and 4); PCIDs; a direct switch from caller to callee; and a shorter system-call path (464 ns floor). Wound down 2026-10-01 by os-86. **Beyond it, as good as seL4 or better** (the customer's ask, 2026-10-01 for 1.5 times, raised 2026-10-02): the plan is §9.5. Step 0 measured seL4 on nazuna under the gate's KVM (§9.6, 2026-10-02): 440 ns a round trip with protections matched. The customer then set the target to seL4's own figure or better (2026-10-02), not 1.5 times it, so about 440 ns, and the perf row fails above 1.10 times. nazuna has no PCID, so step 3's PCIDs cannot be measured here, and `IBPB` costs about 230 ns a switch, not 2 us. Step 5's means to it (§9.6, *what seL4 does not do*): ERAPS in place of the return-stack refill (about 40 ns, needs the consultant), fewer user pages per trip and 2 MiB pages, global pages for the runtime's shared text (design to the consultant), FSGSBASE. The customer answered the four decisions on 2026-10-02 (end of §9.5): the matched figure, the vector contract, a fast path subject to the consultant, and a consultant subagent of the session that runs the plan. It has five steps after measuring: land the branch; a cheap common path (locks, `current()`, the way out); a cheap switch (PCIDs, a vector-state contract, FS/GS bases); the direct switch and a fast path for 0x1013; then squeeze and a perf row. 80–129 points, of which 50–80 are on the critical path with three sessions.  **Where it stands (2026-10-03 wind-down, §9.9):** step 1, F-60's fix and 2a to 2e are on `main`; the round trip inside a domain is 2,556 ns p50 with every mitigation on, against seL4's 440 ns (the target) and Redox's 1,965 ns without speculative defences. On branches, local on nazuna: `step2f` (WIP, ungated), `bench-exact` (land first), `step4-prep` (step 4's groundwork, not reviewed). Not started: 3a, 3b, step 4's fast path, step 5, step 4b. Next: `docs/handover/2026-10-03-ipc.md` | open |
| The `loom` model owed by 2c, the pending-work word (`docs/OPAQUE-KERNEL.md` §9.8, 2c's case 9; the consultant's condition 4 on landing, 2026-10-03): a host model of a poster's write then post (C then D, a `Release` read-modify-write) against the task's clear then read (A, an `Acquire` read-modify-write, then B), checking that the task either reads the state or keeps the bit, with the control "the clear made after the read". `loom` is not a dependency of the tree, so the model adds one (`deny.toml`, the host-test crates). **Must exist before step 4's code review**: step 4's condition 9 extends it to the park protocol, and 2e's condition 5 adds the listed-but-not-blocked window to it. Until it does, 2c's order rests on its argument and the check-mode audit (FX-0520) | **Done 2026-10-03** with 2e: `src/tests/loom`, `cargo xtask loom` | done |
| A licence and advisory check over the lock files the root workspace's `cargo deny` does not read: `src/tests/fuzz/Cargo.lock` (`libfuzzer-sys` and its 52) and `src/tests/loom/Cargo.lock` (`loom` 0.7.2 and its 30). Run `cargo deny check advisories licenses sources` in each with a tests-only config (the root `deny.toml` bans `libc`, which both need, so it cannot be shared), as a step of `cargo xtask check` or a CI job, so a vulnerable or relicensed test-tool crate is seen rather than reviewed once at its landing (the certification consultant's advisory on 2e, 2026-10-03) | open |
| F-45 on the driver's side of the block ring. The new `ferrix-driver` ring (`src/user/system/native/driver/src/block.rs`, since 31b16d72) reads and writes the ring's shared u32 indices a byte at a time, through its trait's default methods. The kernel can then see a torn `comp_tail` and end the ring as corrupt. The kernel's side was made whole-word by `os-35/ipc-ring` part A. Found by os-35 on 2026-10-01 | open |
| **Block I/O faster than Redox** (the customer, 2026-10-03). Redox's ring-3 virtio-blk driver reads 4 KiB in 80 us p50 (109 us p90) on nazuna, under the gate's KVM and CPU model (`docs/OPAQUE-KERNEL.md` §9.6a); Ferrix's block ring takes 300 to 844 us for the same read, and the host reads the NVMe with `O_DIRECT` in 59 us. Target: a 4 KiB read through the block ring and the ring-3 driver under 80 us p50, measured the same way and alternated with Redox's image in one run. What is known to cost: the two copies (the zero-copy row above), only four commands in flight (the row below), and the hop to ring 3 and back, which steps 2 to 4 of §9.5 make cheaper. Measure where the 300 us goes first, as §8 did for the trip | open |
| Only four block commands fit in flight: the ring's data VMO is 512 KiB in four 128 KiB regions, so depth 32 queues behind four. Allocating the data VMO by page, or regions sized to the request, would lift it. Seen by `os-35/ipc-ring` part A, where a region now stays reserved until the reader has copied it out | open |
| The trace's Linux side: `bench-seam` prints a mean only; a static per-read `clock_gettime` + `pread(O_DIRECT)` program for p50/p99 on x86-64 and AArch64, the same hops from ftrace (`block_rq_issue`, `block_rq_complete`, `irq_handler_entry`, `sched_wakeup`, `sched_switch`), and a host-side count of VM exits per read with `trace-cmd` between port-0x80 markers (`docs/OPAQUE-KERNEL.md` § 8) | open |
| Virtqueue barriers on real Arm (F-44, 2026-09-27). The kernel's `Rings::barrier` and the doorbell after a publish are now `arch::dma_barrier` (`dmb osh`), closed on the argument because no emulator reorders as an Arm core does. Left: a run of the stage 10 virtio check in the Pixel 7's crosvm VM, when the product owner allows the hardware, which shows AArch64 only (the DK1 has no PCI and no virtio device, so ARMv7-A's `dmb osh` stays on the argument and the disassembly); and, only if a virtio device in hardware or one offering `VIRTIO_F_ORDER_PLATFORM` is ever driven, the ring-3 drivers' `fence(SeqCst)` (`dmb ish`, enough toward a hypervisor's emulated device) moved to `dmb osh` through the native runtime | open |
| The GICv2 distributor lock on the DK1 (F-50, 2026-09-27): stage 10's `gic` line on the board, whose two Cortex-A7 cores and GICv2 are the hardware the finding is about, when the product owner allows the hardware. QEMU showed it (64 of 64 rounds lost without the lock, 0 with it, on ARMv7-A at 2 and 4 cores and on AArch64) | open |
| ARMv7-A's signal return page on the DK1 (F-48, 2026-09-27): `/returning` in stage 3's fault check on the board's Cortex-A7s, when the product owner allows the hardware; QEMU ran it (139 before the page, 55 with it) | open |
| ARMv7-A's plain `sigframe` return restores the alternate stack from the frame, which Linux's `sys_sigreturn` does not (only `rt_sigreturn` does); and `uc_mcontext`'s `trap_no`, `error_code` and `fault_address` are left zero where Linux fills them. Found reading `arch/armv7a/signal.rs` for W-8 (2026-09-27); neither is a failure, both are divergences to match or state | open |
| F-49, found 2026-09-27 reading `arch/armv7a/cpu.rs` for W-8 (numbered by the consultant, not yet in FINDINGS): `psci_system` (PSCI `SYSTEM_OFF` and `SYSTEM_RESET`) does not declare `r12` clobbered, where `psci_call` does under the same SMC calling convention, so firmware that returns from a failed reset or off may leave `r12` corrupted under the compiler's feet on the way to the next attempt or `halt`. Design, agreed: add `lateout("r12") _` to both `asm!` arms in `psci_system`; the check is a disassembly note that the caller does not keep a live value in `r12` across the call, since no emulator's PSCI fails its reset. Record F-49 found-and-closed with the fix, tally re-counted. Parked at the 2026-09-27 wind-down (ferrix-b5) | open |
| W-8 file 24, `arch/armv7a` and `arch/arm_common`'s low-level requirements (2026-09-27, ferrix-b5): parked at the wind-down on branch `w8-armv7a` (WIP commit, drafts in `docs/drafts/w8-armv7a/`), handover in `~/.local/share/ferrix/w8-armv7a-drafts/HANDOVER.md` on example. Four area drafts cover all 163 unnamed units with 80 draft requirements and a verification table each. File 24 claims all of `arch/arm_common` (agreed with the AArch64 slice). Next: write `docs/sysml/24-armv7a-requirements.sysml` in 18/19's pattern against the code as F-50 and F-48 left it, move `speculation::check` into a child check file, mark both subsystems complete, consultant review. The handover also lists three drafter flags still to confirm (ARMv7 Linux fixes up a misaligned user `ldm` where Ferrix sends `SIGBUS`; mask/unmask accept identifiers past the controller's count; the speculation plan comes from the boot core only) | open |
| F-46, parked (2026-09-27): xtask's power-off gates took a triple fault after the run's marker, which ends QEMU under `-no-reboot` with status 0, as a power-off. Branch `f46-power-off` (1694e217) has the fix: every `arch::shutdown` prints `FERRIX-POWER-OFF` before its power-off write and `FERRIX-POWER-OFF-FAILED` a second after one that returned, and `watch_to_power_off` requires the line, no failure line after it, and the architecture's own status (33 on x86-64, 0 on Arm), with a host test. The certification consultant found no fault in the code; the commit message claims three negative controls (a triple fault after the line, a debug exit without it, AArch64's SYSTEM_OFF returning) which were run before its session exited, each rejected by the check meant to catch it (logs in `~/ferrix-logs/f46`); a WIP commit on the branch holds the `qemu.rs` `.get()` fix and evidence edits, and the message wrongly names `announce_off` in power.rs for `console::announce_power_off`. Next: squash, correct the message, rebase, rerun the gates, record F-46 found and closed with the tally re-counted, and ask the consultant for the one-line OK | open |
| W-8, what is left (`docs/certification/IMPLEMENTATION.md`, *Where W-8 stands*): the boot slice's 21b (requirements for the crate root, init, power, random and the self-check switch, F-51) and 21c (`devmgr`), both ferrix-15's; file 24, `arch/armv7a` and `arch/arm_common`, ferrix-55b's after F-48 and F-49; then the modules the gate does not yet hold complete (`sched`, `syscall/native.rs`, `audit`, `pci`, `irq`, `timer`, `panic` and the rest, which `check-traceability.py --report` lists). Each slice's design goes to the certification consultant before code | open |
| Small certification notes, parked by the consultant at the 2026-09-27 wind-down (`~/.local/share/ferrix/cert-consultant/next-batch.md`): `irq.rs`'s header says nothing is unregistered "before stage 10" where nothing is ever unregistered (a comment, with the coverage carried); `smp`'s `synchronize` and `read_section` are kept with no product caller, since T0's driver restart removes no handler (irq has no unregister, a delivery holds its `Line` by `Arc` under `BOUND`'s lock); `L.smp.17`, the stuck-processor shootdown bound, is proven only by commit 94dee288's negative control, not by a boot check; and `H.SCHED.8` names only x86-64's registers, so AArch64's FP/SIMD state waits under `H.SCHED.1` until it is widened | open |
| Re-measure the item's coverage to take in `space.rs`'s shared-code test (F-10, 2026-09-27). `alloc_check` now maps a two-page object as shared code, as the vDSO is -- the one-page refusal, both regions' flags, the unmap -- and runs it once per allocation it makes, so `map_shared_code` (`src/kernel/src/user/space.rs` 2690-2744, unreached on every architecture in 978fead6's run) is exercised on all three; its one unreachable line, 2701's `NotUserRange`, is argued as 2554's is, and `find_free`'s doc now states the window bound both rest on. Landed with the evidence carried, not regenerated, and the floors unchanged: a run of the suite on each architecture under the drcov plugin, then `--json --residual` (VERIFICATION.md §3.3), and a floor raised only from what that run measures | open |
| **Done 2026-09-27:** the Arm firmware's two waits, cut from every boot (the test-time work, ferrix-9a's survey). AArch64: EDK2 waited 5.3 s at its boot menu (guest clock `Tpm2SubmitCommand` → `BdsDxe: loading`, 5.31 s cold and 5.34 s warm on main 11532464); the fresh variable store xtask writes now holds `Timeout`=0 (`tools/common/xtask/src/uefi_vars.rs`), and the same span is 0.37 s. QEMU's `-boot splash-time=` does not reach the AArch64 build. ARMv7-A: U-Boot counted 2.0 s before autobooting (`Net:` → the countdown, 2.0 s cold, 2.16 s warm); it now loads its own default environment with `bootdelay=0` from the second flash bank (`tools/common/xtask/src/uboot_env.rs`), and the countdown takes under 0.02 s, the reset boot's second start too. Same firmware path, same boot command and command line; `test-kaslr` passes on both. Not done: x86-64 has no such wait (EDK2 starts the loader at 0.9 s) | ferrix-e4 |
| Re-measure the item's coverage to take in `space.rs`'s shared-code test (F-10, 2026-09-27). `alloc_check` now maps a two-page object as shared code, as the vDSO is -- the one-page refusal, both regions' flags, the unmap -- and runs it once per allocation it makes, so `map_shared_code` (`src/kernel/src/user/space.rs` 2690-2744, unreached on every architecture in 978fead6's run) is exercised on all three; its one unreachable line, 2701's `NotUserRange`, is argued as 2554's is, and `find_free`'s doc now states the window bound both rest on. Landed with the evidence carried, not regenerated, and the floors unchanged: a run of the suite on each architecture under the drcov plugin, then `--json --residual` (VERIFICATION.md §3.3), and a floor raised only from what that run measures. **Also since 2026-09-27 (W-8 boot slice 21a):** 49 stage checks moved from `main.rs` into `stages_check.rs`, which the item counts as verification, so their statements left the product residual and `main.rs`'s call sites were re-anchored: 99 anchors, 96 of them arguments, were dropped as unmeasured by the carry, and the moved checks' run-time column in TRACEABILITY.md reads *not measured* until this run. Coordinate with ferrix-9a's x86-64 re-measure, which needs a re-carry if it lands after | open |
| **Bring down test and gate run time** (the customer's priority one, 2026-09-27). Phase 1, measure, is done in `docs/TEST-TIME.md`: a standard item gate took 828 s at load 12-30; a compositor boot averages 31.9 s, of which 12.6 s is the guest; `test-compositor --arch x86_64` ran past 1,107 s. The costs, in the order to cut: the 5 s power-off grace and fixed settles on every boot left running (cut 2, done), `--arch all` sequential (cut 3), then KVM by default on x86-64; the Arm firmware waits were cut by ferrix-79 the same day. The kernel rebuilt per init flavour was measured on 2026-09-28 and **dropped by the product owner**: a debug switch costs about 2 s (incremental), only a release switch pays the whole crate (about 70 s, as much as a cold build), and no gate switches flavours in release, so a target dir per flavour would only make first uses cold (`docs/TEST-TIME.md`, *Measured, 2026-09-28*). **Cut 2 done 2026-09-28** (PO session; `docs/TEST-TIME.md`, *Cut 2*): a guest that never powers off is stopped when its hook is done rather than given the 5 s grace, the compositor's trailing 2 s reads stop once the guest is quiet, seven compositor boots no longer wait 30 s for `hyprctl` answers their keys never asked for, and the settles before a first keystroke wait for a shell's answer. `test-compositor --arch x86_64` ran to its end on both sides: 900 s before, 512 to 616 s after at loads up to 18; test-restart, test-sysfs, test-jobs and test-init 2 to 10 s shorter each, and a warm `test-init --arch all` 56 s to 39. Left of cut 2: the other gates' hooks (input, pty, seat, display, clipboard, adb, badapple, foot, vkgears, video, chrome, `--gl`) still take the grace until each is read and run. Still to measure: test-net. Handover: `~/.local/share/ferrix/ferrix-90/HANDOVER.md`. **Cut 3, the architectures at once, done 2026-09-30 (os-98)**: `--arch all` of test-boot, test-init and test-audio runs a child an architecture (test-compositor stays in turn: at once, x86-64's frame budget failed under load) (`docs/TEST-TIME.md`, Cut 3): test-boot 136-139 s to 46 s and test-init 171-182 s to 72-86 s at loads of 34 to 60. Next: KVM by default on x86-64. **Phase 3 from 2026-10-01 (os-5d; `docs/TEST-TIME.md`, *Phase 3*)**: the customer asked again on 2026-10-01; eight sessions' answers and the gate logs put most of a verification outside the guest: waiting for the one gate worktree (34 of a 40-minute batch), cold target dirs (`check` about 15 min cold), steps the change does not reach (a docs landing runs the whole `check`), and self-checks re-run on each of `test-compositor`'s ~33 boots an architecture. Plan A1-A4 (gate host), B1 (`xtask gate --since main`), C1-C5 (`check` at once, compositor with `ferrix.checks=skip`, KVM default, `--profile iterate`, saved guests), 22 points. A1-A3 in place 2026-10-01: `~/.local/share/ferrix/fleet/gate.sh` (three warm slots, slot 1 the stage-13 queue's, slot 3 given back under 50 GB free; a disk guard; controls logged with their diff and a FIRED line; `logs/queue/INDEX`, a line a run). `gate.sh control` is the fleet's way to run a negative control from now on: its log keeps the commit and tree, the sabotage's diff and the verdict, which the audit's finding that the queue's control mode kept no record of its sabotage asked for. Its first verdict searched the header too, so any failure read FIRED (os-ad, D1); fixed the same day and shown each way, a sabotage that does not compile but quotes the text reading DID NOT FIRE (`docs/TEST-TIME.md`, *A1-A3 in place*); the consultant accepts its `control: FIRED: <line>` as control evidence. | os-98 (was the PO session, ferrix-90); phase 3 os-5d |
| Tell a line that stopped being a statement from one reached (certification consultant, 2026-09-27). `coverage-report.py --json` records each file's statement lines in the image, and `gen-coverage-justification.py` then says "no longer a statement in this image" apart from "reached", and flags an argued line whose code vanished. On 11532464 three trimmed trap rows were the first kind, and the landed message says they were the second (VERIFICATION.md §3, "A line can stop being a statement"). After the test-time table. | ferrix-90 |
| `crypt`'s `$2*$` blowfish hash, which gives `"*"` so a blowfish entry in `/etc/shadow` matches no password. The stubs this row once listed, regex, `awk`'s math and `dirname`, were all replaced by 2026-09-16 and `src/stubs.rs` is gone | open |
| The rest of ferrousli's POSIX.1-2024 gap, by area in `docs/POSIX-2024.md`: 106 interfaces, 51 points, as measured again on 2026-09-30 and less the five `timer_*` functions of 2026-10-01, none written on a branch. The largest parts: realtime (20 interfaces, 10 points: `aio.h`, `mqueue.h`, `SIGEV_THREAD` for `timer_create`); the 28 `long double` forms of `math.h` the x87 has no instruction for (5), the only part of the math library left, and `complex.h`'s 22 `long double` forms after them (2); locales and messages (3, 8: `.mo` catalogues, `iconv`'s other sets, `strfmon`); users and databases (`ndbm.h`, 9, 3); processes and the system (4, 4); spawning (4, 3: `addchdir`, `addfchdir`, `_Fork`, `fexecve`); signals (4, 2); time (3, 3); the `clock` waits (4, 2); terminals (2, 2); `wordexp` (3); `posix_getdents` (1); and POSIX.1-2024's declarations in musl 1.2.5's headers (3). Each landing updates the document's tables | open |
| **Done 2026-09-27:** the seam measured, 1. The `seam` boot line times a 4 KiB read through the ring at depths 1 and 32, with the driver's own submit-to-drain in each completion's `device_ticks` (x86-64). `cargo xtask bench-seam` reads the same disk from a stock Linux kernel on the same QEMU machine. On x86-64 under KVM, depth 1, over four pairs: Linux 27 to 48 us, Ferrix 300 to 844 us. On AArch64 under TCG: Linux 145 us, Ferrix 453 us. The table is in stage 11's roadmap section, and the 2026-09-16 decision cites it | ferrix-55b |
| **Done 2026-09-27:** the seam measured, 2. The kernel counts syscalls, page-cache pages served against pages filled from disk, and block-ring crossings (`/proc/ferrix-seam`). `test-vfs` prints the counters, and `test-rustc` prints them cold and warm. A warm compile crossed once in 4,345 syscalls; the numbers are in stage 11's roadmap section, and the 2026-09-16 decision cites them | ferrix-55b |
| `net_ring::run()` unclaims its device twice when a ring is refused or never begins: `unclaim(&start.device)` inside `if outcome.is_none()` and again after it (`src/kernel/src/interfaces/net_ring/mod.rs`). `unclaim` removes the node from `CLAIMED` by pointer, so the second is harmless alone; but a ring created for the same device between the two, by another task on another processor, has its claim removed by the second call, and a third ring could then be created beside it for one device. Found reading FX-1151's path on 2026-09-17; not shown to cause it. Fix: one unclaim on every path, with a check that creates a ring in that window. 1 point | open |
| `test-boot --net` on Windows answers "the x86_64 kernel reported success but QEMU exited 1" after a clean boot: the xtask gateway's teardown, seen on 2026-09-16 with QEMU 11.1.0 under both WHPX and TCG. 1 point | open |
| The WHPX panic's QEMU half (root-caused 2026-09-16: QEMU 11.1's own MMIO emulator under WHPX walks the guest's page tables and answers "not mapped" for a mapping that exists, with more than one vCPU). The workaround landed: `xtask` gives the guest one processor under `whpx` unless `--smp` is given. Whether to report it upstream is the customer's (below); `git log -S whpx_handle_mmio -- docs/BACKLOG.md` finds the full analysis | open |
| The shared ring index discipline: `src/lib/proto/netring` and `src/lib/proto/blkring` keep the same private indices, checked reads of the peer's and want-bell handshake, written twice. Extract it into one crate both depend on, in a landing of its own, with both rings' tests and both fuzz targets as the evidence; it was deliberately not done in the landing that added the second copy, since a mistake there is a disk that stops reading. 3 points | open |
| uutils' procps and util-linux, which hold `sysctl`, `ps` and `top`: their only release, 0.0.1, does not compile on this toolchain, and their main branches do not compile for this target (procps' `top` wants libsystemd through pkg-config, util-linux's `blockdev` and `fsfreeze` pass `ioctl` glibc's request type). 5 points, better spent when either project releases again (`docs/UUTILS.md` §6a) | open |
| The ports built only when stale, not on every `cargo xtask ports` (5). Work is on branch `os-12/ports-autobuild` (1dd0e503: staleness per port, a build lock, the Linux path, Windows through WSL), state unreported; read its commits before trusting them. Since 2026-10-01 the ported programs are apps, which `test-apps` and `--everything` build only when there is no package, and every port's script takes a build lock, so what is left of it is a package that is older than its `build.sh` | open |
| zinc-next's remaining 21 points, as sized on 2026-09-17: the builtins B1 to B4 (B1 starts with the `BIN_FG` numbering fix), the history ring and file, ZLE, completion and modules, and the swap to `zinc` | open |
| The `AF_PACKET` gaps: frames this host sends copied to `ETH_P_ALL` sockets (`PACKET_OUTGOING`), packet sockets on the loopback, and classic BPF (`SO_ATTACH_FILTER`, `SO_DETACH_FILTER`) | open |
| **Done 2026-09-26:** the Pixel 7's USB serial port, live. During a native boot Ferrix presents a CDC-ACM port (`1209:0001`, "Ferrix console") on the phone's USB-C port, and `tools/vendor/google/pixel7/monitor` streams the kernel log from `/dev/ttyACM*`: the boot's stages and `ferrix-statd`'s samples, live. Proven on the phone in run `usblog2`; `docs/vendor/google/pixel7/USB-HANDOVER.md` §8 has the three writing runs, the PO's standing write list and what is left. The survey, the `TREE_GS201_DWC3` binding, `src/lib/drivers/usb/dwc3`, `src/lib/drivers/usb/usb-device`, `src/user/system/native/drivers/usb/usbdev`, the kernel log (`console/log.rs`, `syslog(2)`, `logctl`) and the monitor's watcher | ferrix-9c |
| The Pixel's USB port at SuperSpeed: `src/lib/drivers/usb/dwc3` holds `DCFG` at high speed and leaves the combo PHY (`0x110F_0000`) alone. SuperSpeed means writing that PHY's window, a new block the PO has to approve | open |
| Input over the Pixel's USB port: `usbdev` reads what the host sends and drops it. A shell or tty over the port needs the console's input side to take bytes from a ring-3 driver | open |
| **Done 2026-09-27:** every driver kind with a core restarted by devmgr (T0 of the live kernel update plan, customer 2026-09-26): display, sound, network, input and disk. A net interface is parked with its addresses and a disk with its requests queued for the next driver, so a btrfs root survives its disk drivers being killed; the quarantine's pins are given back at HELLO; `cargo xtask test-restart --boot all`. Left: `Port`, `Host`, `Engine` and `Gadget`, which have no core that waits for its claim | open |
| Measure L12's start of `devmgr` on the Arm pair: add a boot under `ferrix.devmgr=init` to `cargo xtask coverage`'s suite (`tools/common/xtask/src/coverage.rs`). The suite keeps the kernel's start, the reference configuration, so the `devmgr.rs` statements only init's start takes -- 78 on AArch64 and 82 on ARMv7-A in the 2026-09-27 measurement -- are counted as needing a test (docs/certification/VERIFICATION.md §3.1, COVERAGE-WORKLIST.md) | ferrix-15 |
| `usbdev`'s adb socket is the path `/tmp/adbd-usb`, bound in the initramfs's `/tmp` because `devmgr` starts the driver before `/` moves onto a root volume (`docs/INIT.md` §7.3). Under `ferrix.devmgr=init` with a root volume, an `adbd` started from the volume would not find it -- the bug `vport` had until 2026-09-27 (`docs/CLIPBOARD.md` §6). The fix is `vport`'s: an abstract name gated on `SO_PEERCRED` (`src/user/system/native/rt`'s `sockaddr_un_abstract` and `peer_uid`) | open |
| devmgr does not restart `usbdev` (its `Gadget` kind, like `Port` and `Engine`): a driver that dies leaves the Pixel with no port until the next boot. The log core ends the claim when the channel closes, so a restart could reclaim it | open |
| The Pixel's DWC3 runs with the PHY's suspend (`SUSPHY`, `ENBLSLPM`) and USB 2 LPM off, costing the power they save; turning them on needs Linux's save and restore around endpoint commands | open |
| Read the Pixel's DWC3 release (`VER_NUMBER`, `0xC1A0`, `DWC_usb31`) in the loader's survey: it decides the soft-reset timing quirks `src/lib/drivers/usb/dwc3` now covers by always waiting 50 ms more | open |
| The log core's REFUSED can go missing: once, on an aarch64 `test-boot` under a host at load ~50, `src/kernel/src/interfaces/logctl`'s boot check sent DATA as a driver and found the channel closed with no REFUSED queued, though the core's task had ended the claim (`FERRIX-PANIC log control self-check failed: a driver that sent DATA was not refused`, tree 566ca8be). The rerun passed. The check now requires the claim to end, with or without the REFUSED, and prints `logctl   SIGHTING: ...` on every boot where the REFUSED is missing, so the boot logs count it; why it was not there is open. Log: `~/ferrix-logs/pixel7-usb-log/boot-aarch64-logctl-refused-566ca8be.log` | open |
| adb for Ferrix: `adbd` over TCP first (gated in QEMU with the host's `adb`), then over USB as a function beside the Pixel's serial port, for `adb shell`, `push`/`pull`, `reboot` and `forward`. Chosen with the owner on 2026-09-26 over fastboot (a bootloader's protocol) and SSH over USB networking. `docs/ADB.md` is the handover; the USB half needs the PO's OK for new DWC3 endpoint registers. **Done 2026-09-27:** adbd over TCP (`cargo xtask test-adb` green on x86_64, aarch64 and armv7a) and over the Pixel's USB port (run `adbusb4`: devices, shell, 1 MB push and pull byte for byte, reboot). Open: authentication, `shell,v2`, starting adbd from init (`docs/ADB.md` §6) | ferrix-9c |
| `reboot bootloader` from Ferrix on the Pixel: Android's `pixel-reboot` writes `0xfc` to the PMU's reboot word (`0x1806_0810`) through an EL3 SMC (`set_priv_reg`) and also stores the mode with `gbms_storage_write` in the battery-management chip's persistent storage, which Ferrix must never write. Without it ABL may ignore the mode. Not planned; the lap through Android works (`docs/ADB.md` §4) | open |
| **Done 2026-09-27:** the Pixel 7's GUI, option A. The launcher app (`tools/vendor/google/pixel7/android`) runs a Ferrix desktop in the phone's crosvm VM: the compositor on crosvm's display (a root `app_process` bridge asks virtualizationservice for it by cid and hands it to the app), a touchscreen, keyboard and mouse over sockets, `--scale 2`, and the soft keyboard reserving its height at the bottom (`addreserved` through the serial console's shell), so that the desktop shrinks rather than pans. Chromium 154 (Debian's arm64 build, `tools/common/fetch/fetch-chromium-arm64.sh`) runs on it from `chromium.img` at `/data`; typing into its address bar with the keyboard open was seen on the phone on 2026-09-27, on Android CP3A.260905.009. For it: CAM PCI with INTx, crosvm's doubled `TRANSFER_TO_HOST_2D` offset told apart by its PCI subsystem, getty for `console=uart8250,...`, and four AArch64 kernel fixes (SCTLR UCT/UCI, CNTKCTL EL0VCTEN, a vDSO with `__kernel_rt_sigreturn`, and F-41: `mprotect` marking a private region copy-on-write) | ferrix-d4 |
| The Pixel 7 launcher's VM leans on two undocumented Android internals that the CP3A.260905.009 update already changed once: `waitDisplayService(int cid)` as `IVirtualizationServiceInternal` transaction 17, and crosvm's `--android-display-service cid:N`. After an Android update, a guest with no screen ("FERRIX-VM-BRIDGE failed") means these moved again; the Terminal app's inlined call in `VmTerminalAppGoogle.apk` (dexdump, `DisplayProvider`) shows the current code | open |
| Memory exhaustion without a cgroup limit ends the wrong processes the wrong way (ferrix-ea's black-box pass, 2026-09-26, `~/ferrix-logs/break-ea/`, `hog.c`): a process that faults a page in with the machine out of frames dies of `SIGSEGV`, with no `oom` line, where Linux's OOM killer picks a victim (Chrome's tab shows "Aw, Snap! 11", not V8's OOM page); meanwhile `read` returns `ENOMEM` in bystanders -- the terminal and the wallpaper client exit on it and nothing restarts the wallpaper, and Chrome's zygote logged 870 of them in 1.5 s. And tmpfs ignores `size=` (`-o size=4m` took 10 MB; `/tmp` took 2.5 GB until fork failed everywhere), with `statfs` always 0 used | open |
| Resource limits are accepted and read back but not enforced (ferrix-ea, 2026-09-26): `RLIMIT_CPU` sends no `SIGXCPU`, `RLIMIT_FSIZE` no `SIGXFSZ`/`EFBIG` (`ulimit -f 10` then a 100 KB `dd` wrote it all), `RLIMIT_AS` stops no allocation. cgroup `memory.max` and `pids.max` do work | open |
| A `/dev/pts/<n>` path node keeps only the number, and `open_slave` resolves it again after the DAC check (`src/kernel/src/fs/devfs.rs` `Place::Slave`, `src/kernel/src/fs/pty.rs` `open_slave`): a user who closes its own master between the check and the open, while root opens `/dev/ptmx` and unlocks the same number, is handed root's slave. Reachable since 2026-10-03, when a slave became its opener's so a uid-1000 terminal works; narrow, since a new pair starts locked. Fix: a per-pair serial in `Place::Slave`, `ENXIO` on a mismatch (interim reviewer, 2026-10-03) | open, low |
| Mounts, from userspace (ferrix-ea, 2026-09-26): `umount /data` succeeds while Chrome runs from it (Linux: `EBUSY`), and `/data` leaves the table with Chrome still running on it; a btrfs device mounted twice gives a second mount that is silently read-only (`EROFS`) while `mount` lists it `rw` -- `/home`'s disk too since 2026-10-03, and the interim reviewer asks that a second mount of a mounted disk be refused with `EBUSY` or share the volume; ~~`mount -o remount,ro` of `/` and `/data` is `EINVAL`~~ (remounts work since `docs/NAMESPACES.md`'s N1, 2026-09-28, closing F-53); `mount --bind` and `pivot_root` `EINVAL`, `swapon` `ENOSYS`, no loop devices, `blkid` finds nothing | open |
| Linux ABI gaps seen from userspace (ferrix-ea, 2026-09-26): ~~`mincore`~~ (2026-10-03, branch `mincore`) and `mlock` `ENOSYS` (sshdt warns); `/proc` lacks `loadavg`, `interrupts`, ~~`self/mountinfo`~~ (N1, 2026-09-28), `self/limits`, `self/stack`, `<pid>/environ`, `<pid>/ns/*`, `modules`, `sys/fs/inotify/*`, and `/proc/sys/vm/drop_caches` is `ENOTDIR`, not `ENOENT`; `readlink` of a directory's `/proc/self/fd` entry is `ENOENT`; `pmap` fails; no `/dev/kmsg`, no `/dev/rtc`; `unshare(CLONE_NEWNET)` `EINVAL`; `FS_IOC_GETFLAGS` `ENOTTY`; `/proc/stat`'s system column always 0; a zombie still reports its RSS | open |
| The desktop's own programs, from ferrix-ea's pass (2026-09-26): term leaks its Wayland descriptors (two memfds and a pipe) into the shells it starts after the first -- the kernel honours `CLOEXEC` in every form tried (`cloexec.c`), so term does not set it; the tiling layout gives 0x0 tiles after about eight splits of the newest tile (20 terms, 8 at 0x0, `u3.png`), which xdg-shell reads as "choose your own size"; `hyprctl workspaces` lists only the active workspace; zinc's `ulimit` does nothing, `exec 10</dev/null` runs `10`, `kill -SEGV` is unknown, `trap … 40` misses real-time signals, and errors print Rust's `io::Error` text. And once the whole guest wedged with no panic during `timeout 1 sleep 99999999999` after signal tests, not reproduced (`hang4-sleep-huge.log`) | open |
| btrfs, from ferrix-ea's second pass (2026-09-26, `~/ferrix-logs/break-ea/` `btrfs.sh`, `bis.sh`, `seq.sh`, on the writable test disk): no single file grows past 32 MiB -- `dd bs=1M count=33` stops at 33550336 bytes with an I/O error and the file reads back 33554432; and eight concurrent writers (`dd … bs=64k count=100 &` ×8) each report success, leave no file, and from then on every create, truncate, link or mkfifo on that mount is `EIO` until a remount, with nothing in the serial log. A write-protected block device mounts and lists as `rw`, then every write is `EROFS` (Linux mounts it read-only). (`umount` losing what was written since the last commit was fixed on 2026-09-27.) | open |
| Processes and signals, from ferrix-ea's second pass (2026-09-26, `dig.c`, `spawn.c`, `posix2.c`; the host as the reference): `vfork` does not share the parent's memory, so glibc's `posix_spawn` of a missing program reports success and the child exits 127 where Linux returns `ENOENT` (Rust's `Command` on glibc spawns through it); `rt_sigqueueinfo`/`sigqueue` and `timer_create` `ENOSYS`; `RLIMIT_NPROC` not enforced (201 forks after `setrlimit(NPROC, 20)` and `setuid`); `PR_SET_NAME` not shown in `/proc/self/comm`; `getrusage` reports zero user time and `ru_maxrss`; `F_GETPIPE_SZ`/`F_SETPIPE_SZ` `EINVAL`; `sendto` an abstract `AF_UNIX` datagram address `EOPNOTSUPP`; `setitimer` at 10 ms and `sched_setaffinity`+`sched_getcpu` each failed once. (Real-time futex deadlines never expiring, and zinc taking `sh -c --`'s `--` for the command, were fixed on 2026-09-27.) | open |
| Terminals and sessions, from ferrix-ea's second pass (2026-09-26, `ptyt.c`, `ctty.c`): closing a pty master sends no `SIGHUP` to the session it controls; sshdt never closes a dropped session's master, so with the first every lost interactive ssh session leaks a shell; the input side of a pty has no bound (4 MiB taken with no newline, Linux stops near 4 KiB with `EAGAIN`); `/proc/<pid>/stat`'s `tty_nr` is always 0; zinc's `$?` is 0 after `^C` kills the foreground job (130 on Linux); and zinc ignores `errexit` in every form (`-e`, `set -e`, `setopt errexit`) | open |
| The rest of ferrix-ea's second pass (2026-09-26): ~~busybox `poweroff` does nothing (it signals pid 1 with `SIGUSR2`, which init does not take; only `kill -TERM 1` powers off)~~ (fixed by b9c825ba, 2026-10-01: `/bin/poweroff`, `/bin/reboot` and `/bin/shutdown` are links to `svc`, which busybox then does not link), ~~and at shutdown init remounts only `/` and `/data` read-only, both `EINVAL`~~ (fixed by N1, 2026-09-28); an ELF whose `e_type` is `ET_REL` runs (Linux: `ENOEXEC`) -- 89 other malformed ELF and `#!` files were all refused; sshdt moves bulk data at about 400 KB/s; Chrome's Ctrl+O opens no file chooser | open |
| What btop showed on the desktop after its fixes (3e0a94b8, 2026-09-27) that is still wrong, none of which it depends on: `/proc/self/mounts` lists the initramfs's mounts from before the switch to the btrfs root (`tmpfs /`, `/dev`, `/proc`, `/tmp`, `/sys` twice) beside the new ones, where Linux shows only mounts reachable from the reader's root, so `mount` and `df` name `tmpfs` for `/`; `statfs` answers `f_type` 0 for btrfs and tmpfs alike (`stat -f` prints `UNKNOWN`), where Linux gives `BTRFS_SUPER_MAGIC` and `TMPFS_MAGIC`; and the terminal's Hack face has no U+2074, so btop's panel number `⁴proc` is the replacement box. Seen in `run-compositor --release --no-gl --ssh` on x86-64 | open |

### The desktop and the GPU, what is left

| Item | Stage |
|---|---|
| `run-compositor --everything` is everything (the customer's rule, 2026-10-01): nothing on that desktop may be conditional or silently left out. Done: every volume is fetched when missing, and a failed fetch stops the run (f7777c17); Steam's launcher entry (cb21ecd8); every app installed, the opt-in ones too, and a script app with no package (btop) built, through WSL on Windows (211bc0e5, os-24); the ports built on Windows through WSL, with LLVM's clang where WSL's gcc is older than 15 (503d65aa, os-df); a missing port or busybox built and a missing Bad Apple!! fetched rather than left out (os-24, 2026-10-01); launcher entries and icons for btop and ferrofetch, and Bad Apple's icon (48f440dc, os-3c); the ported programs (curl, git, sshdt, foot, vkgears, ALSA's) are apps, so `--everything` builds a missing one as it builds btop, and `ports::everything` is gone with them (os-3c, 2026-10-01). Left: choosing Steam from fuzzel after closing the client, which no live desktop has tried yet. vkgears needs Venus, which WHPX does not give, so a Windows host's desktop cannot list it | 19 |
| Two btop builds at once wreck each other: every checkout's btop app builds, by its `build.sh`, in the one `$FERRIX_PORTS/btop/build`, which each starts with `rm -rf`, so a second build deletes the first's `obj/` mid-compile (`opening dependency file obj/btop.d: No such file or directory`). Seen twice on nazuna on 2026-10-01, os-24's `--everything` control beside os-3c's apps-launcher `build-apps`; alone, the same script built 6 of 6. Since 211bc0e5 `--everything` builds btop when it has no package, and `test-apps` always does, so two gates at once keep meeting here. **Fixed 2026-10-01 (os-3c, with the ports' move into apps):** ferrousli's port toolkit has `lock_port`, a `flock` on `$builds/<port>.lock` held until the script exits, and every port's and script app's `build.sh` takes it before it empties its work directory, so a second build waits | done |
| `test-audio --arch x86_64`'s mix boot failed once, on 2026-10-01 at a load of 15 to 19, gating the ports' move into apps on 1c4dbcf4: "the mix has 1000 Hz at 7537, not 8192 within 5%, and 440 Hz at 8188". That boot is pulsed mixing two tones, which the branch does not touch; the same boot on the rebased tip 4be395fa passed at a load of about 9. Kept: `~/ferrix-logs/apps-ports/b-audio.log` on nazuna. A mix level 8% low on one tone looks like a lost or late period under load, not noise | open |
| LEDs on virtio-input: its status queue takes STATUS and drops it, so QEMU's keyboard stays dark; `write` of types other than `EV_LED` and `EV_SYN`, which Linux injects and Ferrix answers `EINVAL`; the LED events passed to the node's readers. Done for USB keyboards on 2026-09-23 (`docs/INPUT.md` §3.3, §7.4) | 17 |
| A graceful STOP for an input driver, and a gate row for it. Nothing sends one: the kernel's input core (`src/kernel/src/interfaces/input`) never sends STOP, and devmgr answers a sysfs `unbind` by killing the driver's job, so `ferrix-driver`'s stop path -- STOP, `Driver::shutdown`, the transport's `Stopped`, `Dma::free` -- has never run since d5b03142 moved virtio-input onto it (`test-restart --boot input` kills with `kill -9`, which the kernel's quarantine covers). Needs the core to send STOP when devmgr unbinds a device gracefully, then a `test-restart` variant that unbinds instead of killing and requires STOPPED, the device's node gone and a restarted driver serving again; with a negative control that frees a `Dma` before the reset (the one-line sabotage `Stopped` exists to prevent) and shows the row noticing. The core change touches `interfaces/`, so it goes to the certification session before landing (os-9f's suggestion, 2026-09-30) | 17 |
| Multi-touch axes (`ABS_MT_*`), force feedback (`EV_FF`, `EVIOCSFF`) and sound (`EV_SND`) on `/dev/input/eventN`. The input core publishes a device without them and its boot line says what was left out; QEMU's keyboard and tablet declare none (`docs/INPUT.md` §3.2, §6, os-f6 2026-09-16) | 17 |
| Input hotplug: devices exist from boot in the input iteration. A device that arrives or leaves later, and how a compositor learns of it without udev (`inotify` on `/dev/input`, in the kernel since 2026-09-27 but not for devfs's own nodes, which it makes without a call and so without `IN_CREATE`; or a rescan) (`docs/INPUT.md` §3.4, §6, os-f6 2026-09-16) | 17 |
| `card0` is opened by one process at a time, standing in for DRM master (`docs/DISPLAY.md` §5, a written deviation from Linux): Linux's many opens with one master, `SET_MASTER`/`DROP_MASTER` arbitrating between them, and the render node beside it come in stage 19 | 19 |
| The GPU for clients: A4 `zwp_linux_dmabuf` and a GBM-shaped allocator, and Mesa's virgl on ferrousli (`docs/GPU.md` §3, 3a), for clients that render on the GPU themselves, 8 points and 40 or more; zero-copy presentation for Venus through the same dmabuf (§3 step 4); and the Khronos Vulkan loader, which waits on `dlopen` of a library with thread-local storage | 19 |
| NVIDIA's own driver on the RTX 3060 (`docs/NVIDIA.md` §7, decided 2026-10-02). **N0, the kernel prerequisites, is complete** (N0a–N0d, N0f, N0g, each through the consultant; F-57, F-58 and F-59 closed; 2026-10-03). **N1 is done (2026-10-03)**: N1a fetch, N1b the skeleton, N1c RM in `nvrm` with its core from the volume (af8d242ab), N0e `run-nvidia`, N1d GSP boot on the RTX 3060, N1e the chardev core and N1f `nvidia-smi` listing the card, the last three landed ahead of their review conditions (ledger 297; owed rows below). Then N2 Vulkan offscreen (14), N3 frames to hyprix, copy layer first (30), N4 Chrome and Steam on the card, with yserver's DRI3 and Present (20). CUDA (N5): C0a landed with `test-uvm`; N6 waits for a monitor on the 3060 | 21 |
| Gears on the DK1's GC400 (`docs/GPU.md` §6.2, §6.3): G1 and G2 were done on the board on 2026-09-24. Left: G3 a clear resolved to the LTDC's buffer (8), G4 draws with host-compiled shaders (8), G5 gears on the board (5) | 19, P3 |
| The DK1's LTDC display and USB HID, done 2026-09-23 (`docs/DISPLAY.md` §6, `docs/INPUT.md` §7). Left: other modes than 720p60, display hotplug, and HID's absolute axes and vendor reports (§7.5) | 17, P3 |
| The DK1's USB host after U-Boot's `ums`: ending mass-storage mode switches the PMIC's `vdd_usb` (STPMIC1 LDO4, on I2C4) off, and the kernel never turns it on, so the USB PHY is unpowered and nothing enumerates; U-Boot's `regulator dev vdd_usb; regulator enable` before `bootefi` is the workaround (`docs/vendor/st/stm32mp157-dk.md`, 2026-09-23). The kernel's USB preparation should turn LDO4 on itself -- a write to the PMIC every rail of the board hangs off, so with the care the RCC gets | 17, P3 |
| The desktop clients' foundation (`docs/DESKTOP-CLIENTS.md` §2): `src/user/system/linux/compositor/toolkit`, `src/user/system/linux/compositor/text`, `src/user/system/linux/compositor/hyprlang`, `src/user/system/linux/compositor/image`, and `run-compositor --config` carrying the user's dotfiles and the fonts they name. Owner: clients-base. Estimated 21 points | 19 |
| fuzzel in Rust (`docs/DESKTOP-CLIENTS.md` §4): the pure core landed 2026-09-26 (`a29407d6`, `02afa6c8`); the window, hyprix giving an interactive layer surface the keyboard, the image's `.desktop` entries and icons, and `test-compositor --boot fuzzel` landed with this row. Left: the clipboard pastes, taking the keyboard back from an `on-demand` layer surface on a click elsewhere, a 30-second start with the user's 20 host fonts, an aarch64 boot. **The user's SUPER R landed 2026-09-27** (b9cb64d5): `crate::dotfiles` carries `~/.local/bin/hypr-launcher` unchanged and links the fuzzel it names to `/bin/fuzzel` (`--boot fuzzel-user`). Still left: the script's `pkill -x fuzzel` toggle (its own row, P2), and fuzzel's lock file, which it builds from the absolute `WAYLAND_DISPLAY` (`/tmp/fuzzel-/tmp/wayland-1.lock`, a warning). Handover: `~/.local/share/ferrix/ferrix-d5/HANDOVER.md`. Owner: fuzzel. Estimated 21 points | 19 |
| waybar in Rust for the user's own `~/.config/waybar` (the waybar app in ferrix-os/apps, the desktop clients' design). **Draws the user's bar on the desktop (2026-09-27):** the config, formats, GTK3 stylesheet, layout and painter (landed 2026-09-26); the bar on the toolkit with hover, cursor, clicks, scrolls and tooltips as layer-surface popups, tested against hyprix in-process; hyprix's half (layer popups, pointer on layer surfaces, a virtual pointer's seat); a PulseAudio-protocol client; `waybar --render`; `/bin/waybar` on every desktop; and `test-compositor --boot waybar`, whose screen must show the host's drawing of the user's style pixel for pixel. Whole-pixel text as Pango places it, and the volume chip reading, setting and following `pulsed` (`--boot waybar-volume`), landed too. On Ferrix the user's three Python scripts are not there, so the desktop chips and the clock are hidden, as upstream hides them; no sound server but `pulsed` beside Chrome's sound (else `vol 0%`), no load average, no wifi, no D-Bus tray (`docs/DESKTOP-CLIENTS.md` §3). **On `run-compositor --everything` (2026-09-27):** the desktop starts this machine's own `hyprland.conf` with its dotfiles, fonts and monitor EDID, and the user's config matches its bar to that EDID's description (b9cb64d5). Chrome's `HOME=/dev/shm` now goes on Chrome's own command instead of a global `env =` line, which had hidden `~/.config/waybar` from waybar (17822571; `--boot everything-desktop`, and the customer's own command run headless). Owner: clients/waybar. Estimated 34 points, 30 spent | 19 |
| hypridle (`docs/DESKTOP-CLIENTS.md` §6): the hypridle app (ferrix-os/apps), which builds `/bin/hypridle` and `/bin/loginctl` over the foundation's hyprlang and toolkit, and the `idle` and `idle-user` boots. Green on x86_64 and aarch64 on 2026-09-26. Left for Ferrix: a suspend for the sleep hooks, and D-Bus inhibitors. Owner: clients-hypridle. Estimated 5 points | 19 |

### P3 — hardware variants and later stages

* GICv3 and its redistributors, with a second AArch64 boot configuration
  (`gic-version=3`); x2APIC; TSC-deadline. Real AArch64 hardware is GICv3.
* Stage 13's namespaces and seccomp (its cgroups are in), stage 14
  (real-time domains), and global page-cache reclaim, which `rustc` on a
  small machine needs and no stage names today.
* `vfork` sharing memory rather than copying it.
* Stage 21, bare metal with an NVIDIA card driven by Ferrix itself: Path B
  of the GPU decision of 2026-09-18, `docs/GPU.md` §4. Opened when the
  customer wants Ferrix on real hardware; unsized, over 100 points.
* Stage 22, Steam (decided 2026-09-18): the 32-bit x86 ABI, being built
  (ferrix-41, `docs/I386.md`), glibc's place taken by ferrousli under the
  Steam runtime (13 priced, the rest unsized), bubblewrap's needs on top of
  stage 13 (13), XWayland (40 as a first guess), sound (current work,
  `docs/AUDIO.md`), and Vulkan through Venus on a KVM host (landed for
  vkgears). Over 300 points; the roadmap's stage 22 is the list.
* Bad Apple!! and Doom, beside stage 22 (games with sound; the customer's
  order of 2026-09-26, ferrix-b0, `docs/MEDIA.md`). Bad Apple!! with sound
  and `test-badapple`: not estimated before it started, ≈ 8 spent, landed.
  Bad Apple!! on the `--everything` desktop, as a window, with the
  toolkit's `Client::toplevel` it needed: estimated 8, ≈ 8 spent.
  **Doom in Rust is in the backlog** (the customer, 2026-09-26: stop after
  Bad Apple): D1 to D4, estimated 21, not started and unowned.
  `docs/MEDIA.md` §3 has the plan and what reading room4doom found.
* Ferrix in 8 to 16 MiB, for an FPGA RISC-V board (Tang Nano 20K, 8 MiB):
  `docs/SMALL-MEMORY.md`. Phase 0 (`--strip-kernel`, the end-of-boot
  memory line, CI's 128 MiB row) landed; phases 1 to 3 are estimated at 29,
  unowned. The RISC-V port itself is unsized.
* Huge pages; frame share and release are order 0 by design.
* A panic report as a QR code: a port of Linux's `drm_panic_qr` as
  `src/lib/kernel/qr` (ferrix-qr), so a panic screen can carry the whole report. WIP
  on branch `worktree-agent-a33c10946b6721065` (5690f0e), unbuilt into the
  panic path.

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

## Velocity, measured on 2026-09-18

Points are what a session says before it starts (the rule of 2026-09-13,
`docs/ROADMAP.md`'s opening) and velocity is what landed, counted
afterwards. This is the first count over the whole points era, from the
evening of 2026-09-13, when the first estimates were written, to the morning
of 2026-09-18. Sources: the product-owner ledger kept at the time (which
measured 2026-09-14 at the time), the landings recorded in this file, and
the roadmap's stage totals -- stage totals for stages 17, 18 and 19 rather
than their rows, so that nothing is counted twice.

| day | points landed | what |
|---|---|---|
| 2026-09-13 | 0 | the first estimates written that evening; the day's landings were sized before the rule and are not counted |
| 2026-09-14 | 131 | measured by the product owner at 23:20: 19 landings across ten sessions -- stages 7, 8, 9, 10, 11, the mm stack, the board -- ≈ 21 points a queue-hour |
| 2026-09-15 | 34 | 00:20–03:00: memfd 5, threads 5 (7), the POSIX gap document 3, the native Windows busybox 5, the branch cleanup 3, netwire 8, inet ABI 3; then the fleet stopped until the evening of the 16th |
| 2026-09-16 | 66 | from 19:40: threads 6 (3), the zinc gate 2, the compositor's parser 5, display L1 3, ferrousli's threads 8, a busybox fix 1, and the rest of networking (44 of its 50 estimated), whose exit was met at 22:50 |
| 2026-09-17 | 214 | stage 17 (74) and stage 18 (96) less the 8 above, both exits met; stage 19's first ≈ 44 (rules, dispatchers, layouts, groups, monitors, plugins, blur and shadows); static PIE 3, the threads exit 2, ferrousli-misc 3 |
| 2026-09-18 | unpointed | the compositor's frame time, pacing, the kernel's fault fix, the cores, 1080p and wallpapers, the GPU and Steam decisions: none was sized before it started, so none counts |

**About 445 points in four calendar days, 2026-09-14 to -17: ≈ 111 a
calendar day, and ≈ 150 a day the fleet was actually running** (the 15th was
three hours). The finer number the ledger measured on the 14th, 21 points a
queue-hour, held on the 17th too by this count. Ten sessions ran on the 14th
and about eight on the 17th, so a session-day is 15–20 points, and every
estimate under 8 held both days.

What the number is good for and what it is not:

* It sizes the pointed remainder: stage 19's ≈ 100 and dynamic linking's 39
  are a fleet-day each at the 17th's pace *if the work is of the kind that
  was measured* -- protocol tables, syscalls, a renderer -- which the GPU
  path (unknowns in every step) and XWayland (a server) are not. Stages 21
  and 22 are unsized and the number says nothing about them.
* Three calibrations are mixed in it: session estimates against each other's
  yardsticks, the product owner's "unmeasured" sizing of stages 17–19 on
  the 13th, and the rows that carry their own points. The stage totals for
  17 and 18 came in where they were sized (74 and 96), which is the one
  check the mix has passed.
* The 18th's landings are counted at zero, not because they were small
  (sized afterwards they would be about 35: the backdrop 8, pacing and the
  card's damage 5, the fault fix 3, the cores and the shadow 8, 1080p and
  wallpapers 8, the two decisions 3) but because a size given after the fact
  is not an estimate. The next count should not have such a row.

### Update, 2026-09-24

**The code base** (`git ls-files` on `main` at 046ea79a): 702,675 lines of
Rust in 2,080 tracked files of all kinds, plus 47,769 lines of C, headers and
assembly (ferrousli's and the ports' glue). By tree: libs 188,154, compositor
169,567, kernel 122,989, ferrousli 114,688, zinc 52,011, xtask 34,449, user
9,152, fuzz 7,970, boot 3,474. The docs are 26,714 lines of Markdown, 25,061
of them under `docs/`. Nothing is excluded or generated-filtered, so read the
Rust total as an upper bound.

**Today.** The 66 commits that landed on 2026-09-24 add 54,477 lines and
remove 4,309, of which 45,560 added and 2,054 removed are Rust: a net
+50,168 lines in the day, ≈ 7 % of everything above.

| landing | points | of |
|---|---|---|
| sysfs (`b65b5e41`) | 26 | 26 |
| the init push: G3, G4, L1, L2, L3, G5 | ≈ 23 | 22 |
| vkgears through Venus (V1–V5) | 39 | 39 |
| the GC400's first two steps (G1, G2) | 11 | 11 |

That is **≈ 99 points landed on 2026-09-24**, against the ≈ 54 a day the
backfill below gives for 09-18 to 09-23. The landings ran from about 18:15
to 22:45, so about 4.5 hours: **≈ 22 points an hour**, level with the 21 an
hour measured on the 14th and the 17th. It is a few sessions, not a
fleet: four agents ran in parallel on the init push and two other sessions
landed the rest.

Not counted, because nothing was sized before it started (the rule above):
Chrome's headless run, foot on the compositor, timerfd and signalfd, the
control queue and the debug FPS overlay, ferrousli's glibc names for Chrome,
stage 20's build recording, splice and copy_file_range. Sized afterwards they
would be perhaps 60–80 more, which is why 99 is a floor. The day's cost was
not in the code: the sysfs landing was gated three times and the gears
landing four times because `main` moved under every gate, and FX-1004 flaked
in two of the rows.

**The days between, 09-18 to 09-23**, are not in the table above because no
session reported them. They were sized afterwards from `git log`, in clusters
against the same scale, and are lower in confidence: ≈ 50, 50, 55, 96 and 20
for 09-18 to 09-22, about 324 in six days (btrfs write's 60 included), ≈ 54 a
calendar day. With the first count's 445 and today's 99 the running
total is ≈ 870 points in 11 calendar days, ≈ 79 a day. The drop from the
first week's 111 a day reads as a change in the kind of work (the zsh
compatibility tail, and GPU, dynamic linking and btrfs write, whose steps
each have unknowns) and not as a stall.

### Update, 2026-09-26

Counted from the 2026-09-24 count (88cd2740) to 02afa6c8, 356 commits, by
the same rule: a landing counts at the estimate written before its work
started, and what had none is sized afterwards from `git log`, in
clusters, on the same scale, and said to be so.

| day | estimated before | sized afterwards | what |
|---|---|---|---|
| 2026-09-24, after the count | 0 | ≈ 27 | the DK1's desktop at the speed of a hand, its cursor plane, its HDMI modes, the console sending by interrupt, zinc's start |
| 2026-09-25 | 0 | ≈ 26 | the certification audit: the item's boundary, coverage, the Security Target and the rest of its set, SMEP, SMAP and PAN, the trap return and `StatLayout` inverted; one or two sessions |
| 2026-09-26 | 103 | ≈ 175 | estimated: init L4 to L9 45, audio L1 to L7 24, cgroups P1, M1 and S1 28, `SCM_CREDENTIALS` 2, the vDSO ≈ 4; afterwards: the certification's F-23, F-31 with KASLR, F-34, F-36, F-37, the item's split and its coverage suite ≈ 70, the Pixel 7 ≈ 39, Chrome on ferrousli, its speed and its desktop ≈ 33, and the terminal, fuzzel and Linux-compat fixes ≈ 33; about twelve sessions |

So **≈ 26 on 2026-09-25 and ≈ 278 on 2026-09-26**, 103 of the latter
estimated before it started. S1 counts its 13 although it was built
differently from its plan, and the vDSO's 4 is its share of a joint
estimate; both are soft. The running total is **≈ 1,200 points in 13
calendar days, ≈ 92 a day**, or ≈ 973 (≈ 75 a day) counting after
2026-09-24 only what had an estimate. What was estimated landed at ≈ 67 a
day over 2026-09-24 to -26, and that is the rate the roadmap's sized scope,
≈ 442 points, burns at: the unsized work beside it takes nothing off it.
The roadmap's *Burndown* lists that scope.

### Update, 2026-10-01

Counted from 02afa6c8 (the 2026-09-26 count, 17:37) to ae4c0d89a, by the same
rule, over the calendar days 2026-09-27 through 10-01: a landing counts at
the estimate written before its work started, and what had none is sized
afterwards from `git log`, in clusters, on the same scale, and said to be
so. The evening of 2026-09-26 itself, 17:37 to midnight -- which the desktop
clients' foundation, the EDID monitor rules and the first `waybar` and
`fuzzel` work sit in -- falls between this count and the one before it and
is in neither.

| day | estimated before | sized afterwards | what |
|---|---|---|---|
| 2026-09-27 | 90 | ≈ 150 | estimated: the 32-bit x86 ABI's I1 to I4, 42 (`docs/I386.md`), the sound stack's alsa-lib and `pulsed`, 21 (`docs/AUDIO.md`), authentication's phase 1, 27 (`docs/AUTH.md`); afterwards: the certification item's unsafe-site trace closed (F-26), its high-level requirements (51) and low-level requirements across object/, memory, the architectures, SMP, console and the IOMMU, F-41 through F-52 and W-8's design ≈ 70, the host-guest clipboard both ways ≈ 8, waybar's bar and PulseAudio module with fuzzel's window ≈ 13, the adb-over-USB bridge finishing ≈ 5, driver restart-on-death ≈ 5, the Linux personality's `inotify` and pidfds ≈ 5, hyprlock's library ≈ 5, the stripped 128 MiB boot row ≈ 5, Windows build and QEMU fixes ≈ 8, the opaque-kernel seam measurement proposed and shelved ≈ 8, and a dozen small kernel fixes ≈ 13; well over a hundred commits from more sessions than any day before it |
| 2026-09-28 | 0 | ≈ 44 | the installer's MVP (`ferrix-install`, `test-install`) ≈ 13, System V semaphores for Steam ≈ 8, test-time's cut 2 ≈ 5, Steam's bootstrapper and its semaphore permission fix ≈ 5, CI and Miri infrastructure fixes ≈ 8, small VFS and `/proc` fixes ≈ 5 |
| 2026-09-29 | 36 | ≈ 26 | estimated: yserver, the X server, 36 (`docs/YSERVER.md`); afterwards: Steam's sign-in window through yserver ≈ 8, the repo layout move to `src/`, `tools/` and `docs/` ≈ 5, packet pipes with `pipe2(O_DIRECT)` dropping Steam's last preloaded shim ≈ 5, ferrousli's glibc names for Steam's 64-bit side and multiarch loader search ≈ 5, `/proc`'s 32-bit inode and offset fixes ≈ 3 |
| 2026-09-30 | 19 | ≈ 83 | estimated: N1 to N3 of Steam's mount namespaces, 19 (`docs/NAMESPACES.md` §9); afterwards: Claude Code on Ferrix, its gate, `XSAVE` and the AVX withholding for Zenbleed and GDS ≈ 21, the bottom mount under every namespace ≈ 3, the red submap fix ≈ 3, test-time's cut 3 ≈ 3, the discovery Finder ≈ 8, `ferrix-driver`'s start (input and `virtio-blk` moved onto it) ≈ 8, seccomp's S1 (the verifier and interpreter) ≈ 8, hyprix's `client.rs` split into modules ≈ 8, the stat/`btop`/`ferrofetch` apps ≈ 5, the repo layout's `interfaces/` and `discovery/` module declarations ≈ 3, and a dozen more kernel fixes (the robust futex list, `get_robust_list`'s missing check filed as P2, the MSI-X storm bound, `no_new_privs` and dumpability, `libudev`'s uevent socket) ≈ 13 |
| 2026-10-01 | 0 | ≈ 94 | the speculation domain landed ahead of its full gate (`bf9efba95`) ≈ 21, the seL4 channel-round-trip plan recorded (`75c03a259`, docs only) ≈ 5, pid namespaces and the small namespaces (UTS, IPC, cgroup, `setns`) ≈ 13, seccomp's S2 (a registered filter at all four entries) ≈ 8, the gate pool and test-time phase 3 design ≈ 5, `pkg`, the package manager, and `test-pkg` ≈ 8, the apps launcher's icons, Bad Apple!!'s fetch, and building the ports and `btop` on Windows through WSL ≈ 8, more WSL build fixes ≈ 5, the certification's after-the-fact reviews and the verification audit's first findings ≈ 8, the stage 13 handover and branch-audit docs at the product owner's wind-down ≈ 5, and smaller items (the gateway's segments in flight, the net-throughput hang filed, booting under KVM by default, `--iterate`) ≈ 8 |

So **≈ 240 on 2026-09-27, ≈ 44 on 2026-09-28, ≈ 62 on 2026-09-29, ≈ 102 on
2026-09-30 and ≈ 94 on 2026-10-01**, 145 of the 542 estimated before the
work started. The running total is **≈ 1,742 points in 18 calendar days,
≈ 97 a day**, or ≈ 1,118 (≈ 62 a day) counting after 2026-09-24 only what
had an estimate. Two things keep this count softer than the one before it:
the certification item's requirements-and-trace work on 09-27 is the
largest single cluster in it (≈ 70) and is sized the way a whole subsystem
would be, against commit messages rather than a design doc's own number,
because the item has no points column of its own; and the evening of
2026-09-26 noted above fell between the two counts and is counted in
neither.

### Update, 2026-10-04

Counted from ae4c0d89a (the 2026-10-01 count) to 67efb9fb1, by the same rule,
over the calendar days 2026-10-02 through 10-04. The twelve commits dated
2026-10-01 that reached `main` after the last count (the channel round trip's
first steps, network namespaces' first commits) are counted on the day the
work landed, not the day it was written. Work that is done but not on `main`
-- NVIDIA's N2 to N4 and N6, S3, L13, `np-land`, `selfhost-matrix` -- is not
counted until it lands.

| day | estimated before | sized afterwards | what |
|---|---|---|---|
| 2026-10-02 | 22 | ≈ 128 | estimated: NVIDIA's N0, the kernel prerequisites other than N0g (`docs/NVIDIA.md` §7, 28 with N0g); afterwards: the channel round trip's first steps (`channel_write_read`, the sync wake, `bench-ipc`, the timer's skip) ≈ 21, hyprix and yserver's frame pacing, refresh rates and the flush thread ≈ 21, btrfs in the certified item and its fallible allocation ≈ 20, btrfs's ENOSPC and mirror fixes ≈ 8, System V shared memory ≈ 8, UVM's self-tests on Ferrix (C0a) ≈ 8, F-58 and F-59 and their fixes ≈ 8, NVIDIA's feasibility, CUDA and N0d/f/g designs ≈ 13, the opaque kernel's §9.7 and §9.8 designs and the seL4 measurement ≈ 13, and smaller items ≈ 8 |
| 2026-10-03 | 38 | ≈ 115 | estimated: N0g, 6, and N1, 32 (`nvrm` boots the RTX 3060's GSP, `nvidia-smi`); afterwards: the channel round trip's steps 2a to 2e ≈ 21, login on the console, `sessiond`, `/home` on its own disk, `pulsed`'s socket and the desktop as a user ≈ 16, ferrousli, zinc, the Pixel 7 tools, the website and the apps moving into repositories of their own ≈ 13, Ferrix building its AArch64 image on the Pixel 7 ≈ 13, the patched QEMU's interrupt-remapping block and its CI cache ≈ 8, F-60's fix ≈ 5, hyprix's window drag, damage and terminal fixes ≈ 13, Chrome's CJK fonts, `xdg-open` and a persistent profile ≈ 8, the brand's logo ≈ 3, and smaller items ≈ 15 |
| 2026-10-04 | 0 | ≈ 55 | network namespaces (`CLONE_NEWNET`, veth pairs, a stack per namespace) ≈ 21, hypridle, hyprlock, waybar, fuzzel and term as apps ≈ 13, foot taking keys ≈ 5, `getty` revoking the console before every login ≈ 5, the product owner's and the gate pool's rules and the wind-down roadmap ≈ 8, and a gateway test fix and coverage carried ≈ 3 |

So **≈ 150 on 2026-10-02, ≈ 153 on 2026-10-03 and ≈ 55 on 2026-10-04**, 60 of
the 358 estimated before the work started. The running total is
**≈ 2,099 points in 21 calendar days, ≈ 100 a day**, or ≈ 1,178 (≈ 56 a day)
counting only what had an estimate. This count is softer than the one before
it: 2026-10-04 is a half day, and nearly all of it is sized from commit
messages. The roadmap's charts (`tools/common/gen/gen-roadmap-charts.py`) are
redrawn from it.

---

## Decisions

Dated, newest first. A decision here is final until the customer says
otherwise; one a later decision replaced is deleted, and the history keeps it.

* **2026-10-04 (customer)** **A docs-only change runs `check-docs`, not
  `check`.** A change that touches only `docs/`, top-level Markdown, the
  skills and agent definitions under `.claude/`, or the generators that write documents alone (`gen-roadmap-charts.py`,
  `gen-arch-doc.py`, `split-roadmap.py`, `build-roadmap-book.sh`) owes
  `cargo xtask check-docs` and nothing more, to save the 3.5 to 9 minutes of
  a full `check` (*What a landing runs*). It needs no gate host. Owed:
  `cargo xtask gate-rows` (`tools/common/xtask/src/gate_rows.rs`, `is_docs`)
  still counts those four generators as code; teaching it is an xtask change
  with its own row.
* **2026-10-04 (customer)** **The roadmap's queue is built first, then
  shortest.** The Gantt's forecast queue orders the sized rows that are built
  and only need landing first, smallest first, then every other sized row,
  smallest points first; ties keep the table's order. The rule is the sort
  key in `tools/common/gen/gen-roadmap-charts.py`, and the status table's
  sized rows follow it. The date does not move at the same rate; more items
  finish sooner and the large ones (Chrome on the DK1, CUDA) last.
* **2026-10-04 (customer)** **The Gantt splits wait from work.** Each
  in-progress row shows, after today, a faint bar for its wait in the queue
  and a light dashed bar for its work, with "N pts left, ~MM-DD" at the end,
  so a row's bar length is its size.
* **2026-10-04 (customer)** **Each session gets its own certification
  consultant.** A session that needs a review briefs its own consultant
  subagent with `AGENTS.md`'s *The certification consultant* and the ledger on
  the gate host, instead of sending to one standing consultant session.
* **2026-10-04 (customer)** **The product owner lands batch stacks.** A
  stack `batch.sh` PASSED is landed by the product owner, under `land.sh
  take` with `git merge --ff-only <tip>`; the sessions whose branches are in
  it join, wait for the verdict and leave `main` alone.
* **2026-10-04 (customer)** **Full runs are batched.** Branches ready to
  land share one run of the image row instead of paying it each:
  `~/.local/share/ferrix/fleet/batch.sh` stacks them on `main`, runs the
  union of their gates once across the gate pool's slots, finds a failing
  entry by running its failed gates on the stack's prefixes at once, and the
  stack lands whole. How the product owner runs it is in `AGENTS.md`, *The
  product owner*, *Batching full runs*.
* **2026-10-03 (customer)** **The `--everything` desktop runs as the user
  `ferrix`, and the machine keeps the users' files apart from the system.**
  The session is started the way `docs/AUTH.md` §6 plans it, not as an
  interim: `sessiond` owns seat0 and hands hyprix its devices, and hyprix
  and every client it starts run as `ferrix` (P2.4, P2.5), the device nodes
  staying `0660 root`. The users' files are on a disk of their own,
  `build/home.img` labelled `ferrix-home` and mounted at `/home`.
  `--reset-root` starts only the system's root over, keeping `/home` and
  `--persistent`'s `/data`; the new `--reset-flash` starts all three over.
  The host's `hyprland.conf`, waybar and fuzzel settings are copied into
  `/home/ferrix` once, when absent, so edits made in Ferrix are kept;
  `--reset-flash` copies them again. With the consultant's seat empty, an
  interim reviewer subagent of the session doing the work reviews its
  kernel changes, and its verdicts go in the consultant's ledger.
* **2026-10-02 (customer)** **GPU support is NVIDIA's own driver.**
  NVIDIA's open-gpu-kernel-modules are ported so that their OS-agnostic core
  (RM, NVKMS, GSP boot for Ampere) runs as a Ferrix driver. NVIDIA's
  unmodified userspace (`libnvidia-*`, the Vulkan and GL ICD, later CUDA)
  talks to `/dev/nvidia*` as on Linux. Not nouveau and not NVK. The
  hardware is nazuna's RTX 3060, through libvirt, and only while the
  customer's domains sharing it are shut off. The order follows yserver's:
  a feasibility pass and a written design (`docs/NVIDIA.md`), then the
  customer's decisions, then small landings. The customer answered the
  design's questions the same day (`docs/NVIDIA.md` §9). The core runs in
  `nvrm`, a ring-3 Linux-personality program. GL for X clients is DRI3 and
  Present in yserver. CUDA is wanted now, alongside the graphics, with
  `nvidia-uvm` designed by a separate session. A monitor on the 3060 comes
  later, and N6 waits for it. The sources are fetched at 580.173.02 and
  never committed. The copy layer comes before dmabuf. Each platform change
  inside the item goes through the consultant on its own. Agents may start
  `ferrix-3060` whenever the customer's domains sharing the card are shut
  off; the 3090 is never touched.
* **2026-10-02 (customer)** **Build System V shared memory for Steam's web
  helper's MIT-SHM.** Asked by the session making Steam smooth: the web
  helper's Chromium presents its software-composited frames to yserver with
  MIT-SHM, whose classic path is `shmget`/`shmat`/`shmctl(IPC_RMID)` while
  both sides are attached; with those calls `ENOSYS` every 8 MiB frame went
  through the socket as a core `PutImage`. Built on branch `steam-sysv-shm`
  (`syscall/shm.rs`, the boot's `shm` line, `test-shm`), modelled on the
  semaphores: per IPC namespace, `ipcperms`, the per-job bound, and i386's
  `ipc` and direct numbers. Message queues stay `ENOSYS`.
* **2026-10-01 (customer)** **The speculation domain lands ahead of its full
  gate.** os-c7 (os-86 after the restart) asked whether to wait for the last
  controls and rows. The customer answered "speculatively merge what you
  have", and in the same session gave its word for the push to `origin`.
  It landed as bf9efba95. The consultant (os-bd) recorded it in its ledger as
  landed ahead of its evidence. What was still running is a row under
  *Verification audit*.
* **2026-10-01 (customer)** **The barrier between programs is skipped only
  inside a speculation domain, and a domain is a job marked at its
  creation.** Asked by os-c7, whose native round trip is 6.5 us with every
  barrier and 2.6 us without, about 4.4 us of it the `IBPB` and return-stack
  refill at each switch between programs. The certification consultant
  recommended it, and the customer chose it over keeping the barrier at every
  switch and over Linux's opt-in mode. The rules:
  - Only the holder of the parent job's MANAGE right may mark a job as one
    speculation domain, and only when creating it. The mark can't be added
    later, a job that already has processes can't be marked, child jobs don't
    inherit it, and it is off by default. Marking a job writes an audit
    record.
  - A switch between programs of the same marked job skips the predictor
    barrier. Every other switch keeps it, on every architecture.
  - The claim narrows to "no program can read one outside its speculation
    domain" (`docs/certification/SPECULATION.md` §3, the Security Target's
    O.ISOLATE). A new assumption of use says the integrator places in one
    domain only programs that may read each other's memory.
  - What it takes to land, set out in the consultant's conditions to os-c7:
    a design note first; the requirements; a check that counts barriers on
    each processor; negative controls (the domain compare always true, the
    MANAGE test removed); and the vulnerability-analysis row.
  Option (b), the barrier by credentials, was rejected: credentials belong to
  the Linux personality, and the job is the item's isolation unit.
* **2026-10-01 (customer)** **The certification targets are a must, and the
  goal is to reach them.** That is all four in
  `docs/certification/README.md`: Common Criteria EAL5+, DO-178C DAL C, IEC
  62304 Class C and EN 50716 SIL 2. The work that produces their evidence is
  required, not optional: traceability, coverage, checks with negative
  controls, the findings register and the vulnerability analysis. Changes to
  the certified item go to the certification consultant before they land
  (`docs/CONVENTIONS.md`). `docs/certification/CLAIM.md`'s two questions,
  the claim's wording and which standard is pursued first, stay open until
  the customer settles them.
* **2026-09-28 (customer)** **A live installer, as Linux distributions
  have one** (`docs/INSTALLER.md`). The customer chose: real PCs as well as
  virtual machines (so NVMe, AHCI, xHCI, USB mass storage, i8042 and a
  firmware-framebuffer display join the plan); a graphical installer on
  hyprix from the start; installing beside another operating system, not
  only on a wiped disk; and a raw `.img` and a hybrid `.iso`, both attached
  to GitHub releases. The design (21 slices, 162 points) was **approved the
  same day**, with §10's recommendations taken for decisions 2 and 5: an
  auto-login `ferrix` live user and a text-mode fallback. **Secure Boot is
  supported** through Ubuntu's Microsoft-signed shim and a Ferrix key the
  owner enrolls once in MokManager (§5.7, I13, 13 points). For decision 1 the customer wants **the full setup for
  shrinking the other system's partition**: the installer shrinks NTFS,
  ext4 and btrfs itself (§4.6, S1–S6, 76 points; 251 in all). Order: the
  VM path I1–I12 first, S1 beside it, then the PC slices.
* **2026-09-28 (customer)** The yserver Wayland backend's design
  (`docs/YSERVER.md`, 7 slices, 36 points) is approved. Its code lives on a
  **fork of yserver on the customer's GitHub account**, pinned by commit in
  `tools/common/fetch/fetch-yserver.sh`, not as a patch series here and not
  upstream first. The backend speaks Wayland through **Ferrix's own**
  `compositor-wire`, `compositor-protocol` and `compositor-shm`, not the
  `wayland-client` crate.
* **2026-09-27 (customer, relayed by ferrix-f6)** **Steam's X server is
  yserver** (github.com/joske/yserver, Rust, MIT), not C Xwayland with
  xwayland-satellite. Ferrix adds the rootless Wayland backend yserver
  lacks, behind its backend trait: each top-level X window an
  `xdg_toplevel` on hyprix, input and the clipboard bridged.
  xwayland-satellite (MPL-2.0) is read as a reference, not copied. Order:
  a feasibility pass on a pinned release (headless backend on Ferrix, an X
  client against it), then a written design sent to the coordinator, then
  the build in small landings (`docs/roadmap/stage-22-steam.md`). Owner:
  ferrix-c3 (was ferrix-41).
* **2026-09-27 (customer)** The opaque-kernel plan (`docs/OPAQUE-KERNEL.md`,
  option B: the net stack, btrfs and the device cores as supervised servers
  behind the page cache) is measured before it is decided. S0 is the two
  "seam measured" rows in P2, then S1, the in-kernel refactors that pay off
  either way. The decision of 2026-09-16 stands until S0's numbers are read,
  and nothing past S1 starts before that. Owner: ferrix-55b. **Later the
  same day, S0 measured, the customer shelved the plan: off the table for
  now.** S1 is not started, and the 2026-09-16 decision stands. A trip to
  ring 3 costs 10 to 20 times Linux's in QEMU, which is what would have to
  change first (`docs/OPAQUE-KERNEL.md`, *The verdict of S0*).
* **2026-09-27 (customer, ferrix-55)** `src/kernel/src` is grouped by what each
  file is (`docs/LAYOUT.md`): `arch/<isa>/`, `arch/arm_common/`,
  `platform/<vendor>/<soc>/`. Every file kept its ring in the move.
  `#[cfg(target_arch)]` stays under `arch/` alone -- the certification
  consultant declined an exception for driver selector files -- so
  architecture-bound drivers stay under `arch/`.
* **2026-09-26 (customer)** **Authentication is a ring-3 service,
  `authd`, as `docs/AUTH.md` proposes, with all eleven of its decisions as
  recommended.** Among them: Argon2id written in the tree, root locked and
  `wheel` members becoming root with their own password, a throttle and no
  permanent lockout, programs that read `/etc/shadow` refused and PAM
  programs given a shim later, and a lock screen that will not lock an
  account with no password. Phase 1 locks the desktop with root's
  password until phase 2 moves the desktop off root.

* **2026-09-26 (customer)** **Doom is room4doom, fetched at build time,**
  never committed. It is labelled MIT but calls itself a transliteration of
  id's GPL-2.0 C source, so Ferrix treats it as GPL: cargo fetches it as a
  git dependency pinned to one revision, the way Chrome is fetched, and the
  repository holds only Ferrix's MIT backend for it (`docs/MEDIA.md` §1).
* **2026-09-26 (customer)** The fleet has a coordinator, ferrix-2c, which
  keeps the landing order under the landing lock (standing rules above); the
  customer stays product owner for scope. No new scope starts without the
  customer.
* **2026-09-26 (customer)** Audio moved forward to current work:
  `/dev/snd` over a ring-3 virtio-snd driver, then a sound server
  (`docs/AUDIO.md`).
* **2026-09-26 (customer)** The desktop's clients are Rust programs that
  read the real dotfiles unchanged: waybar, fuzzel, hyprlock and hypridle
  over shared clients-base crates (`docs/DESKTOP-CLIENTS.md`).
* **2026-09-26 (customer)** The 32-bit x86 ABI is Steam's next step
  (`docs/I386.md`).
* **2026-09-26 (customer)** The kernel takes an EDID override,
  `drm.edid_firmware=`, so `run-compositor`'s screen can be the host's
  monitor (55aca9d1).
* **2026-09-26 (customer)** Certification engineering: F-23 at every site,
  F-31 behind an on/off switch, F-35's quotas built into Jobs, and coverage at
  100 % or justified line by line (`docs/certification/`).
* **2026-09-26 (customer)** The repository is laid out again
  (`docs/LAYOUT.md`), in one short freeze.
* **2026-09-26 (customer)** A USB CDC-ACM device driver for the Pixel 7
  (`docs/vendor/google/pixel7/USB-HANDOVER.md`).

* **2026-09-24 (customer)** **Gears, as the 3D demo: real vkgears through
  Venus, and GLES2 gears on the DK1's own GPU.** vkgears runs in QEMU on the
  Linux host, where Venus carries Vulkan to the host's GPU. Mesa's Venus
  driver is built as a static archive against ferrousli and presents over
  `wl_shm` until dmabuf lands. The DK1's Vivante GC400T is an OpenGL ES 2.0
  core with no Vulkan in any driver, so the board gets gears drawn by that
  core through a ring-3 driver of Ferrix's own, rather than vkgears on a CPU
  Vulkan that would test no GPU. `docs/GPU.md` §6 has the reasons and the
  steps: 39 points for Venus, 32 for the GC400.

* **2026-09-23 (customer)** **Stage 13's cgroups come before init.** The
  real init that is the rest of stage 15 is planned as if cgroup v2 exists,
  and stage 13's cgroup half is built first to make that true. Its namespaces
  and seccomp follow and are not init's prerequisites. `docs/INIT.md` is the
  design, §0.1 what it needs from stage 13. **And stage 13 builds every
  cgroup over a `Job`** (C8): one container object, seen as a cgroup by the
  Linux ABI and as a job by the native one, so that a microkernel can keep
  the jobs if it drops cgroupfs. The rest of `docs/INIT.md` §14 is still
  open.
* **2026-09-18 (customer)** **Steam is on the roadmap, as stage 22**, the
  step after the GPU decision below: the client starts and logs in, a
  native game installs to btrfs and plays with sound through the GPU path,
  and a Windows game runs through Proton. It is a guest's stage first and
  does not wait for bare metal. What it stands on that the roadmap did not
  stage until now -- the 32-bit x86 ABI, XWayland, sound, Vulkan through
  Venus -- is written into the stage as a list of what has to be true, with
  first guesses that sum past 300 points; the rest is stages 12, 13 and
  dynamic linking, which were already there.
* **2026-09-18 (customer)** The GPU: **Path A first, Path B in a later
  stage.** Path A is GPU acceleration for Ferrix as a guest through
  virtio-gpu's 3D commands, which uses the host's NVIDIA driver without
  porting it: the ring-3 driver's 3D commands, a render node with the
  `virtgpu` ioctls and 3D scanout, the host half in xtask, and a Rust virgl
  encoder as the compositor's renderer behind a renderer trait -- 52 points,
  in that order, `docs/GPU.md` §3. Path B is a driver for an NVIDIA card
  under Ferrix itself, for the day Ferrix runs on bare metal: stage 21,
  unsized, `docs/GPU.md` §4. This settles the third of the three choices
  left open on 2026-09-13 below: neither Mesa on ferrousli nor Vulkan for
  the compositor, but a Rust encoder of virgl's command stream, with Mesa
  kept for the day clients render on the GPU themselves. The Windows QEMU
  already offers `virtio-gpu-gl`; the gate host's has to be rebuilt for it.
* **2026-09-17 (customer, delegated to the GUI session) The compositor
  server is written from scratch, not on Smithay.** The customer asked for
  the work to go on without stopping for questions, which settles the open
  choice of 2026-09-13 below. Smithay's value is its backends -- udev,
  libinput, libseat, GBM, EGL and its DRM session handling -- and every one
  of those is C, which `src/user/system/linux/compositor/README.md`'s no-C-device-stack rule
  already forbids and which this tree has already replaced: `blank` drives
  `/dev/dri/card0` itself, `src/lib/drivers/input/virtio-input` is the input driver, and
  `src/user/system/linux/compositor/render` is the renderer Smithay's own pixman would have been.
  What would be left of Smithay is `wayland-server`'s marshalling and its
  protocol handlers, and taking those means every Ferrix system call they
  make is a dependency's choice rather than this tree's -- on a kernel whose
  Linux surface is still being filled in, that turns a compositor bug into a
  hunt through someone else's crate. Writing the wire protocol is about 20
  points more before the first client, and buys a `no_std`-shaped,
  host-tested, fuzzed crate in the same shape as `src/lib/network/netwire`, `src/lib/fs/cpio`
  and `src/lib/proto/inputctl`. The protocol XML this is written from is on this
  machine (`/usr/share/wayland/wayland.xml`,
  `/usr/share/wayland-protocols/`), and so is Hyprland 0.56.2's own source,
  the behaviour reference. `xkbcommon` stays the one C library allowed at
  stage 18 and is unaffected; the stage 19 GPU choice was settled on 2026-09-18.
* **2026-09-16 (customer)** The architecture stays what `docs/ARCHITECTURE.md`
  §1 says: a monolithic core, capability seams, device drivers in ring 3.
  Asked whether Ferrix should be a monolith, a microkernel or a hybrid, the
  answer is that the seam sits at devices because the goal puts it there:
  `rustc`'s system calls are `open`, `stat`, `read` and `mmap` on files the
  page cache already holds, and those stay function calls; only disk traffic
  crosses to ring 3, batched through a ring behind the page cache, where a
  hop is amortised over a queue. The hybrid that is "the worst of both
  worlds" keeps message passing between subsystems and compiles them into
  one address space; Ferrix passes messages only across the privilege
  boundary, and in-kernel subsystems call each other. Rust confines the
  core's memory bugs to its audited `unsafe`, and the IOMMU confines what
  Rust cannot, a device's DMA. What the shape gives up is restarting a
  kernel subsystem, which the goal does not need. A full microkernel would
  make the Linux ABI an emulation layer over servers, against §2; a full
  monolith would delete stages 9 and 10 and gain nothing on the compiler's
  path, which crosses no seam. The decision is closed by evidence rather
  than by argument: the two "seam measured" rows in P2, the seam gate and
  the per-platform isolation table in P1. The second row was measured on
  2026-09-27: a warm `rustc` compile crossed to ring 3 once in 4,345 system
  calls, and a cold one about once per three, while the page cache filled
  (stage 11's roadmap section). The first row was measured the same day: a
  crossing, one 4 KiB read, costs ten to twenty times stock Linux's on
  x86-64 under KVM (300 to 844 us against 27 to 48) and three times on
  AArch64 under TCG, paid on cold reads only. Drivers stay in ring 3 and no
  kernel disk path is built for the measurement (2026-09-13 below).

* **2026-09-15 (customer)** The customer holds the product owner seat:
  there is no product-owner session. A session lands when the gate its
  change's row names has passed on the commit being landed, and commits to
  `main` directly rather than through `develop`, which is retired. What does
  not change: the gate table, judging a gate by its output, and that a
  landing carries its own documentation.

* **2026-09-14** The busybox built against ferrousli is the primary busybox:
  the userland Ferrix is measured with, first in every `test-shell` and
  `test-vfs` the gates run. The musl and glibc busyboxes stay required as
  compatibility checks. A `src/user/system/linux/ferrousli/` landing rebuilds it and runs both with
  it. This carries out the customer's 2026-09-13 order below once the binary
  passed both, at 5e9b0b6 with no stub reached.

* **2026-09-13 (customer)** The goal after `rustc` is a Hyprland-shaped
  Wayland compositor, written in Rust, running on Ferrix. Roadmap stages 17
  (display and input), 18 (the compositor) and 19 (Hyprland fidelity and the
  GPU) carry it; self-hosting moves to stage 20. Pulled onto the path by it:
  `AF_UNIX` with `SCM_RIGHTS` (from networking), `memfd_create` with sealing
  and `MAP_SHARED` file mappings (stage 8), and the `epoll`, `eventfd`,
  `timerfd` and `signalfd` families. `xkbcommon` is the one C library
  allowed at stage 18; the other two choices named that day were settled on
  2026-09-17 (from scratch, not Smithay) and 2026-09-18 (the GPU's Path A).
* **2026-09-13 (customer)** POSIX.1-2024 compatibility is a goal, on the
  condition that it never breaks Linux compatibility. Ferrix takes POSIX
  through its libc over the Linux ABI (ARCHITECTURE §2), so the goal costs
  the kernel nothing new in kind: every mandatory POSIX.1-2024 interface is a
  Linux system call the kernel must answer as Linux does, and the libc side is
  ferrousli's. Where POSIX and Linux differ, Linux wins. Rows: threads (P0),
  the POSIX interface sweep and `AF_UNIX` sockets (P1), libc-test as the
  measure (P2). `AF_INET` stays with the networking stage.
* **2026-09-13 (customer)** Ferrousli is to replace the musl and glibc
  busyboxes as the userland Ferrix is measured with, as fast as it can be
  done: once its busybox passes `test-shell` it becomes the primary binary of
  that gate, with musl and glibc kept as the compatibility checks. Static musl
  stays the goal path to `rustc`, whose `std` targets it.
* **2026-09-13 (PO)** `vmo_map` refuses executable mappings in its first
  landing, and the roadmap records that under stage 9's "Left for later
  stages", not as done: an EXECUTE right on the VMO handle comes with
  `process_create`'s native loader, its first consumer.
* **2026-09-13 (PO)** The DK1 reset follow-ups, as specified: the loader reads
  `bootargs` from a `CMDLINE.TXT` on the ESP so `ferrix.onexit=reset` survives
  a reset without U-Boot's `saveenv`; `test-boot --reset` boots with that option
  and requires QEMU to show a reset, not a power-off. Both go into
  `docs/vendor/st/stm32mp157-dk.md` with the landing.
* **2026-09-13 (customer)** Order of everything: first a working `main`, second
  ferrousli on the build with busybox rebuilt against it, third being surer
  that `main` is stable before it moves. Ferrousli is on the roadmap, and
  its busybox is a `test-shell` target.

* **2026-09-13** Stage 10's exit criterion requires the out-of-domain DMA fault
  on x86-64 and AArch64 only; ARMv7-A's virtio-pci runs in degraded trusted
  mode because U-Boot forces the SMMU bypass. A ring-3 driver reading sectors
  through an untranslated domain may land so stage 11 can proceed, but stage
  10 is not done until domains translate and the fault is shown.
* **2026-09-13** Stage 11's read stage refuses a volume with a log tree, with a
  clear message; log replay is stage 12's. The default subvolume, data
  checksums and a node cache are required before stage 11 is done.
* **2026-09-13** The page-cache interface, agreed between stages 8 and 11: a
  `PageSource` in `src/lib/fs/vfs` with `fill_range(first, pages)` filling at least
  one page, stopping at an extent boundary, zeros past the file's size, called
  under no lock and allowed to block; a checksum failure is `EIO`, never a
  zeroed page; the kernel VMO allocates first, fills with no lock held and
  inserts only if the page is still absent; `read` reports `EIO` and a fault
  `SIGBUS`. Eviction and writeback are stage 12's. Order: the VMO reverse
  map, then `PageSource` with tmpfs over it, then file-backed `mmap`.
* **2026-09-13** A ring-3 driver serving a disk must never fault on a file
  mapping of that disk, or it waits on its own completion: its image and data
  come from initramfs, tmpfs, anonymous or ring VMOs, or are committed before
  any pivot onto btrfs. Stage 10's `devmgr` enforces it.
* **2026-09-13** The page cache is the inode's VMO, on tmpfs and on btrfs
  alike, and file-backed `mmap` and `read` share its pages. One interface,
  agreed between the stage 8 and stage 11 owners before either writes it.
* **2026-09-13** No doc gate compares quoted boot lines with a live log; they
  are illustrations and the roadmap says the numbers move. The host-test table
  becomes a generated document with a gate instead.
* **2026-09-13** CI's 120 s boot timeout stays. A quiet boot takes about 10 s;
  local runs on a loaded host use `--timeout 600`.
* **2026-09-13** (customer, relayed) Drivers stay in ring 3; no interim
  kernel-side disk path, even as a test harness.

---

## Waiting on the customer

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
* User namespaces for Steam (I5b of `docs/I386.md`): scout's requirements
  check refuses a kernel without them ("Steam now requires user namespaces
  to be enabled", exit 71, fatal in `steam.sh`), and the client's UI,
  `steamwebhelper`, runs in a pressure-vessel (bubblewrap) container.
  Ferrix answers every namespace flag `EINVAL` (`syscall/namespace.rs`);
  stage 13 parks the init's L13 on namespaces too. `test-steam-bootstrap`
  sets the check aside meanwhile, and says so.
  **Answered 2026-09-28:** the customer decided to build user and mount
  namespaces, enough for Steam; designed in `docs/NAMESPACES.md` (landings
  N1 to N7 and NP, 39 points to N6), reviewed by the certification
  consultant, branch `steam-userns`. N1, per-mount flags and `MS_REMOUNT`,
  landed 2026-09-28; N2, binds and `MNT_DETACH`, 2026-09-30.
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
- `os-35/ipc-wake`, two "WIP:" commits.
  - 87675432, part B, a sync wake onto the waker's processor. Its relay and
    affinity checks and controls fired. The effect is bimodal until the
    ring's spurious `wake_all` is gone (part A removes it). Left: rebase
    onto the ring's parts, re-apply `Wake::Sync` at the new bell and
    completion wakes, the full row, the consultant.
  - dbd39280, part C: an adaptive halt-poll under a hypervisor (at most
    50 us, idle-task time), and targeted SGIs on GICv2 and GICv3. Checks
    and controls fired. Left: the full row, and F-50's line confirmed.
  - MSI affinity is noted only: it needs a native call to set a thread's
    affinity, vector retargeting, and devmgr to choose the processor.

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
- `guest-frame-time`: diverged from its own name on GitHub. Origin carries
  six newer `WIP` commits on terminal and scroll rendering
  (`c1534cfaa`..`3f382700b`); this checkout has one unreviewed commit,
  `d33a86602`, a guest-compositor frame-cost fix (musl's `f32::round` and
  `memcmp` costing 30-40x glibc's in a guest). The two overlap in
  `compositor/render/exact.rs` and `compositor/term/*`, so neither a
  force-push nor a blind merge is safe. Left: whoever owns the current WIP
  decides which stands and reconciles the files that overlap. `d33a86602`
  is on GitHub as `guest-frame-time-local`.
- `clipboard-vdagent`: also diverged under its own name — origin had landed
  a `user/vport`-based clipboard agent (`c7d7b93fe`, from the 2026-09-27
  wind-down) while this checkout independently built a `compositor/vdagent`
  crate for the same feature. Rather than merge two different
  implementations of one feature, the `vdagent` attempt is kept as
  `wip/clipboard-vdagent-compositor-approach` (pushed), and the
  `clipboard-vdagent` branch itself now matches origin. Left: decide which
  approach to keep, or whether `vport` already covers what `vdagent` did.
- `stage13-timens` (worktree `timens`): most of stage 13's time namespaces
  staged (30 files, `fs/timens_check.rs`, `syscall/timens.rs`, vdso, futex,
  timerfd and procfs integration), blocked on an unresolved merge conflict
  in `src/kernel/src/panic/catalog.rs` — both sides add an adjacent catalog
  entry, FX-0907 and FX-0910. Left: take both entries, then regenerate
  `docs/generated/PANICS.md` and the two certification coverage JSONs that
  depend on it, before this can be committed.
- A detached-HEAD worktree (`display-show`, at `d9c5658ed`) has one
  uncommitted line: `compositor/blank/src/modeset.rs`'s `BACKGROUND`
  changed from a dark slate to pure blue, while the adjacent comment still
  reads "a dark slate, so a screen that is merely black is not mistaken for
  success." Reads as a leftover debug probe, not an intended change. Left:
  confirm and discard, or finish it and fix the comment.

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
