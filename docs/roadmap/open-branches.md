# Open branches: where to pick up

Every branch on GitHub that still holds work `main` does not have, as of
2026-10-01 evening (`main` 7afed6fdc), written at os-5d's wind-down, after
the product owner's and the certification consultant's. It is where the next
session starts.
The *what* of each line of work is in its design document and in
`docs/BACKLOG.md`; this page says which branch carries it and which of a
family's branches to resume from.

**Unlanded** counts the branch's commits whose change is not on `main` under
any hash (`git cherry origin/main <branch>`), so a branch that was landed by
cherry-pick or rebase counts 0 and is not listed below. **Kind**:

- *work*: the branch to resume from;
- *history*: an earlier state of a line of work kept for reference (a
  pre-squash or pre-rebase copy, a `-hist` series); resume from its *work*
  sibling;
- *snapshot*: uncommitted work saved before a PC switch or a wind-down
  (`backup/…`, `wip/…`), one commit on whatever the worktree had; compare it
  with the family's *work* branch before using it;
- *never land*: diagnostics, profiling builds, scratch and negative-control
  copies. Read them, never land them.

To refresh this page: `git fetch --prune origin`, then for every
`origin/<branch>` not merged, `git cherry origin/main origin/<branch> | grep -c '^+'`.

Every landing still follows `docs/CONVENTIONS.md` and *What a landing runs* in
`docs/BACKLOG.md`: rebase onto `main`, `cargo xtask gate-rows --since main`
for the rows, gate on nazuna (`fleet/gate.sh`), the certification consultant
for the item, `land.sh`.

## Stage 13: namespaces, cgroups, seccomp

os-7c's [stage 13 handover](stage-13-handover.md), landed the same evening,
is the fuller account of these branches and what each owes; read it first.
The stage's exit is the container test. State on 2026-10-01: N4, S1, S2,
smallns and pidns landed; the rest are chains of os-7c's agents. Resume from
the plain `stage13-<part>` branch of each; the `-presquash`, `-hist`, `-v2`,
`-v3`, `-onmain` and `-dbg` branches are its earlier states.

- `stage13-cgctl`: the consultant accepted its fixes; it waits for the gate
  and 18 controls. It holds L.object.106-112, L.sched.3-4 and H.QUOTA.10-12,
  and its landing deletes its entry in
  `tools/common/data/requirement-reservations.json`.
- `stage13-s3`: fixes c779b29c accepted; needs its gate and 19 `--expect`
  controls. Then `stage13-s4` (design OK with conditions: an L row, a VA row,
  native calls fail closed, a restart-code case), `stage13-s5`, `stage13-s6`.
  `stage13-s3-wip` does not build.
- `stage13-netns` **landed 2026-10-04**, rebased onto def906ba2 and gated
  on 2428b0d95 (`l13ns-*`; the consultant's ledger line 333). Next is
  `stage13-timens`, which must rebase onto it.
- `stage13-fdinfo` (NP), then `stage13-n5`, then `stage13-bwrap-user`: NP is
  not yet cleared: B1 armv7a `Newfstatat`, B2 dumpable bypass on the capability
  path, C1 the newborn window failing closed, C2-C5.
- `stage13-container`: the exit criterion as one program, never run.
- `stage13-timens`'s worktree held its work staged with a conflict in
  `panic/catalog.rs` (FX-0907 beside FX-0910): take both, then regenerate
  `docs/generated/PANICS.md` and the coverage JSONs that read it.

Every namespace and seccomp branch got the same blocker once:
`launch::load_native` must give a native child the creator's whole namespace
set, pid namespace, seccomp chain and `no_new_privs`. The consultant's ledger
is `~/.local/share/ferrix/cert-consultant/reviews.md` on nazuna.

| Branch | Last commit | Unlanded | Kind | Tip |
|---|---|---:|---|---|
| `timens-presquash3` | 2026-10-01 | 8 | history | wip timens: native-child VA row, roadmap |
| `timens-presquash2` | 2026-10-01 | 2 | history | wip timens on netns |
| `timens-presquash` | 2026-10-01 | 29 | history | BACKLOG: the time namespace's differences from Linux |
| `stage13-timens` | 2026-10-01 | 9 | work | Roadmap: the time namespace's state at the wind-down |
| `stage13-s6` | 2026-10-01 | 16 | work | WIP: wind-down state of stage13-s6 |
| `stage13-s5` | 2026-10-01 | 14 | work | WIP: wind-down state of stage13-s5 |
| `stage13-s4` | 2026-10-01 | 11 | work | WIP: wind-down state of stage13-s4 |
| `stage13-s3-wip` | 2026-10-01 | 4 | history | WIP, does not build: S3's filters, half written when the customer asked to stop |
| `stage13-s3-onmain` | 2026-10-01 | 7 | history | WIP: wind-down state of s3-onmain |
| `stage13-s3` | 2026-10-01 | 7 | work | WIP: wind-down state of stage13-s3 |
| `stage13-n5` | 2026-10-01 | 15 | work | Roadmap: where N5 stands at the wind-down |
| `stage13-fdinfo-v3` | 2026-10-01 | 6 | history | NP: the small-namespace check's people say they are dumpable, as bubblewrap does |
| `stage13-fdinfo-v2` | 2026-10-01 | 4 | history | NP: the dumpable test applies on the capability path, mountinfo is Linux's, the check look |
| `stage13-fdinfo-presquash` | 2026-10-01 | 12 | history | NP: clippy, and the procacc line names what it reads |
| `stage13-fdinfo` | 2026-10-01 | 6 | work | Roadmap: where NP stands at the wind-down |
| `stage13-container` | 2026-10-01 | 17 | work | WIP: test-container, stage 13's exit criterion as one program, never run |
| `stage13-cgctl` | 2026-10-01 | 19 | work | WIP: record where stage13-cgctl stands at the wind-down |
| `stage13-bwrap-user` | 2026-10-01 | 16 | work | Roadmap: where test-bwrap as uid 1000 stands at the wind-down |
| `smallns-presquash` | 2026-10-01 | 13 | history | Write down the consultant's smallns findings: controls fired, rows restated, differences f |
| `s6-hist2` | 2026-10-01 | 30 | history | WIP S6: docs |
| `s6-hist` | 2026-10-01 | 22 | history | WIP S6: clippy |
| `s5-hist` | 2026-10-01 | 17 | history | WIP S5: docs |
| `s4-hist` | 2026-10-01 | 13 | history | WIP S4: docs say what Linux does with a blocked SIGSYS |
| `s3-onmain` | 2026-10-01 | 6 | history | S3: the early endings borrow what they are given |
| `s2-backup` | 2026-10-01 | 11 | history | S2: record the review, the deviations, the gates and the controls |
| `pidns-dbg` | 2026-10-01 | 12 | never land | DEBUG3 |
| `netns-presquash` | 2026-10-01 | 21 | history | The small namespaces' flag check no longer expects network namespaces to be refused |
| `cgctl-dbg` | 2026-10-01 | 11 | never land | DEBUG kmem |
| `backup/2026-10-01/wip-timens` | 2026-10-01 | 9 | snapshot | Backup of uncommitted work in timens (stage13-timens) before a PC switch, 2026-10-01 |
| `backup/2026-10-01/wip-n5` | 2026-10-01 | 15 | snapshot | Backup of uncommitted work in n5 (stage13-n5) before a PC switch, 2026-10-01 |
| `backup/2026-10-01/stage13-n5` | 2026-10-01 | 14 | snapshot | N5: a plain remount asks for privilege over the filesystem's owner; the bottom's flags loc |
| `backup/2026-10-01/stage13-fdinfo` | 2026-10-01 | 5 | snapshot | NP: the stat address-field residual in the vulnerability analysis; the proc_fd_link row na |
| `stage13-seccomp` | 2026-09-30 | 18 | history | seccomp: the design and records |
| `stage13-np` | 2026-09-30 | 18 | history | NP: the check lists the fd directory, which is what is guarded |
| `stage13-integ` | 2026-09-30 | 17 | history | N4: nested 32 deep, EUSERS, and the errnos CLONE_NEWUSER may answer (SECCOMP R3, R5) |

## The channel round trip (IPC fast path)

The plan is `docs/OPAQUE-KERNEL.md` §9.5 to §9.8, and §9.9 says where it
stood at the 2026-10-03 wind-down; the session's account is
[the IPC handover](../handover/2026-10-03-ipc.md). The target is seL4's own
figure or better (440 ns a round trip on nazuna, the customer, 2026-10-02).
Step 1, 2a to 2e and F-60's fix are on `main`; these branches hold the rest.
Each lands only with the certification consultant's OK. **These three are
local branches on nazuna, not yet on GitHub**: pushing them is the
customer's call.

- `step2f` (bc256748a, on the `step2f-ids` reservation, now on `main`): 2f,
  no global or locked writes in the switch. **WIP, ungated, not reviewed.**
  Built for x86-64 only; the weight arithmetic's host test passes. Owed, in
  order: the rows L.sched.50-53 and L.object.160-161 (until they exist,
  `check` fails on their `Verifies:` tags); the condition-6 table re-read
  against the code, with F-60's `answer_leaving` as a remote reader of
  `LAST_DOMAIN`; a stage-5 case that a running processor never reads as idle;
  five controls (the idle clear skipped, `take_resched` on the wrong
  processor, `LAST_DOMAIN` stored after the compare, the host test's division
  left at `u32`, and the new `regroup` loom model's control, which has not
  run); SPECULATION.md §3, coverage, the full gate and `bench-ipc`; then the
  consultant. About 3 to 4 hours. Worktree `.claude/worktrees/ipc-step1`
  (warm target dir); logs `~/.local/share/ferrix/logs/step2a/`.
- `bench-exact` (81409fd7a): step 0's exact `bench-ipc`, a fenced counter,
  p50 from sorted samples, one pinned processor and `--alternate <ref>`.
  Outside the item. Its gate was not confirmed at the wind-down: run a
  full `cargo xtask check` on its rebased head before landing it. Land it
  first, since today's p50 moves in 233 ns steps and hides 2a's, 2c's and
  2e's savings.
- `step4-prep` (fe424cd81, three commits on the `step4-ids` reservation,
  now on `main`): step 4's groundwork. `ferrix.fastpath` (off by default,
  printed at stage 9, L.x86_64.150); the `loom` model of the park protocol
  (§9.7 condition 9, three controls); `ipc-equiv` and `cargo xtask
  test-ipc-equiv`, passing on the general path, with the cases that need
  threads, signals, affinity or 3a printed as owed. No fast-path code. Not
  reviewed; §9.7 on the branch says what it holds.
- Not started: 3a (the vector-state contract) and 3b (FS and GS kept in the
  task, the write-skip dropped), with the consultant's conditions 7 and 8 in
  §9.8; step 4's fast path itself, which needs 2f and 3a; step 5; step 4b.

Older branches of this work: `os-ipc/zircon-trip` is landed in substance (its
six commits went in as step 1), and `os-ipc/prof` and `os-ipc/prof2` are
timing builds that never land. The `os-35/ipc-*` branches below are os-35's
lazy TLB, ring B and sync wake; step 1 took the sync wake, and the lazy TLB
still needs its own landing.

| Branch | Last commit | Unlanded | Kind | Tip |
|---|---|---:|---|---|
| `step2f` | 2026-10-03 | 1 | work | WIP: 2f, no global or locked writes in the switch (wind-down, ungated) |
| `bench-exact` | 2026-10-03 | 1 | work | bench-ipc made exact: fenced counter, sorted samples, one pinned processor, alternation |
| `step4-prep` | 2026-10-03 | 3 | work | Say where step 4 stands (OPAQUE-KERNEL.md §9.7) |
| `os-ipc/zircon-trip` | 2026-10-01 | 10 | history | WIP: save a task's vector state with XSAVEOPT at a switch, where the processor has it |
| `os-ipc/prof2` | 2026-10-01 | 12 | never land | PROFILE: user-state sub-spans |
| `os-ipc/prof` | 2026-10-01 | 7 | never land | PROFILE, not for landing: spans of a round trip |
| `os-35/ipc-wake` | 2026-10-01 | 3 | work | WIP: poll before an idle halt, and interrupt one core with a targeted SGI (os-35 part C) |
| `os-35/ipc-ring-land` | 2026-10-01 | 2 | work | WIP: Hold the seam's trip trace down outside the hop check |
| `os-35/ipc-ring` | 2026-10-01 | 2 | work | WIP: Take the block ring's task off the data path (os-35 part B) |
| `os-35/ipc-lazytlb-on-ef206bb2` | 2026-10-01 | 1 | work | WIP: renumbered onto ef206bb2, generated docs not yet regenerated |
| `os-35/ipc-lazytlb-land` | 2026-10-01 | 1 | work | Keep the last program's space loaded under kernel threads where SMAP or PAN refuses user p |
| `backup/2026-10-01/os-35/ipc-lazytlb-land` | 2026-10-01 | 1 | snapshot | Keep the last program's space loaded under kernel threads where SMAP or PAN refuses user p |
| `os-35/ipc-lazytlb-wip` | 2026-09-30 | 1 | history | WIP lazy TLB for kernel threads |
| `os-35/ipc-lazytlb-rnc5` | 2026-09-30 | 2 | never land | NC5 scratch |
| `os-35/ipc-lazytlb-rnc4` | 2026-09-30 | 2 | never land | NC4 scratch |
| `os-35/ipc-lazytlb-rnc3` | 2026-09-30 | 2 | never land | NC3 scratch |
| `os-35/ipc-lazytlb-rnc2` | 2026-09-30 | 2 | never land | NC2 scratch |
| `os-35/ipc-lazytlb-rnc1` | 2026-09-30 | 2 | never land | NC1 scratch |
| `os-35/ipc-lazytlb-pre-measure` | 2026-09-30 | 1 | history | Keep the last program's space loaded under kernel threads |
| `os-35/ipc-lazytlb-on-eeaa` | 2026-09-30 | 1 | history | Keep the last program's space loaded under kernel threads |
| `os-35/ipc-lazytlb-nc3` | 2026-09-30 | 2 | never land | NC3 scratch |
| `os-35/ipc-lazytlb-nc2` | 2026-09-30 | 2 | never land | NC2 scratch |
| `os-35/ipc-lazytlb-nc1` | 2026-09-30 | 2 | never land | NC1 scratch |
| `os-35/ipc-lazytlb-571ea292` | 2026-09-30 | 1 | history | Keep the last program's space loaded under kernel threads |
| `os-35/ipc-lazytlb` | 2026-09-30 | 1 | history | Keep the last program's space loaded under kernel threads where SMAP or PAN refuses user p |

## Verification audit

`audit-2`: the second verification audit, WIP, with its mutants untriaged.
The ledger is `~/.local/share/ferrix/verification-audit/ledger.md` on nazuna;
read its last section first.

| Branch | Last commit | Unlanded | Kind | Tip |
|---|---|---:|---|---|
| `audit-2` | 2026-10-01 | 1 | work | WIP: audit 2 rows (N4 down to four controls; the queue's control mode keeps no diff) |

## Desktop, display, input, clipboard

- `display-design`: the ring-3 virtio-gpu display driver design, a draft for
  review. `display-show` (snapshots only) followed it.
- `clipboard-vdagent` and `wip/clipboard-vdagent-compositor-approach`: two
  implementations of the clipboard agent (`docs/CLIPBOARD.md` §8). Origin's
  `clipboard-vdagent` is the `user/vport` one; the other is a
  `compositor/vdagent` crate. Decide which to keep before resuming either.
- `edid-home-fix`, `wip/windows-home-edid-remote-fix`
  and `backup/2026-10-01/wip-root-main`: HOME on Windows, carrying a monitor's
  EDID across machines, and the remote-desktop tool. They are the uncommitted
  changes still in the Windows root checkout on 2026-10-01 (README, BACKLOG,
  roadmap charts, `xtask` `edid.rs`, `remote.rs`, `dotfiles.rs`,
  `compositor/run.rs`); compare before committing either.
- `hyprlock` and `hyprlock-full-4e813ac4`, `fuzzel-window-*`: the desktop
  clients (`docs/DESKTOP-CLIENTS.md`); hyprlock is on `main`, and these hold
  commits that are not: compare before resuming. `hyprlock`'s four WIP
  commits (P1.5) landed on 2026-10-03, replayed onto the relaid tree as
  `hyprlock-p15`, so that branch is history now.
- `guest-frame-time` and `guest-frame-time-local`: diverged. The first has
  six WIP commits on terminal and scroll rendering; the second is d33a86602,
  a guest frame-cost fix (rounding and `memcmp` without the C library).
  Both touch `compositor/render/exact.rs` and `compositor/term/*`; reconcile
  by hand, never force-push one over the other.
- `display-show` (snapshots only): its worktree's one uncommitted line turns
  `compositor/blank`'s background blue against its comment; it reads as a
  debug probe.
- `dk1-console/tx-fix` and `dk1-test`: the STM32MP157 board's console and
  input; the board needs a human at it.

| Branch | Last commit | Unlanded | Kind | Tip |
|---|---|---:|---|---|
| `wip/windows-home-edid-remote-fix` | 2026-10-01 | 1 | snapshot | Find HOME on Windows, carry a monitor's EDID across machines, and let a newer remote-deskt |
| `wip/clipboard-vdagent-compositor-approach` | 2026-10-01 | 1 | snapshot | Give the guest a clipboard agent, carried into the initramfs and started by hyprland |
| `edid-home-fix` | 2026-10-01 | 2 | work | Start the README's desktop with --everything, and count the points of 09-27 to 10-01 |
| `display-design` | 2026-10-01 | 1 | work | Draft the ring-3 virtio-gpu display driver design, for the product owner and kernel to rev |
| `backup/2026-10-01/wip-root-main` | 2026-10-01 | 1 | snapshot | Backup of uncommitted work in ROOT (main) before a PC switch, 2026-10-01 |
| `backup/2026-10-01/wip-display-show` | 2026-10-01 | 1 | snapshot | Backup of uncommitted work in display-show (detached) before a PC switch, 2026-10-01 |
| `backup/2026-10-01/wip-display-design` | 2026-10-01 | 1 | snapshot | Backup of uncommitted work in display-design (display-design) before a PC switch, 2026-10- |
| `backup/2026-10-01/wip-clipboard` | 2026-10-01 | 1 | snapshot | Backup of uncommitted work in clipboard (clipboard-vdagent) before a PC switch, 2026-10-01 |
| `readme-gui` | 2026-09-27 | 2 | work | WIP: uncommitted work found in the readme-gui worktree at the 2026-09-27 wind-down |
| `hyprlock` | 2026-09-27 | 4 | work | WIP hyprlock: rustfmt the auth backend and its tests |
| `frame-budget` | 2026-09-27 | 2 | work | File the lock boot that counted a bind after the unlock as one during it |
| `default-wallpaper` | 2026-09-27 | 1 | work | Stop Chrome asking to sign in on the desktop run-compositor starts |
| `clipboard-vdagent` | 2026-09-27 | 1 | work | WIP: uncommitted work found in the clipboard-vdagent worktree at the 2026-09-27 wind-down |
| `hyprlock-full-4e813ac4` | 2026-09-26 | 7 | work | WIP hyprlock: lib.rs as slice 1 has it |
| `fuzzel-window-prerebase` | 2026-09-26 | 12 | history | fixup! Say in the desktop clients' design what fuzzel does with the user's file |
| `fuzzel-window-pre-split` | 2026-09-26 | 3 | history | Say in the desktop clients' design what fuzzel does with the user's file |
| `wip/display-show` | 2026-09-24 | 1 | snapshot | WIP snapshot of worktree display-show, 2026-09-24, before switching PCs |
| `wip/display-design` | 2026-09-24 | 1 | snapshot | WIP snapshot of worktree display-design, 2026-09-24, before switching PCs |
| `wip/clipboard` | 2026-09-24 | 1 | snapshot | WIP snapshot of worktree clipboard, 2026-09-24, before switching PCs |
| `dk1-console/tx-fix` | 2026-09-24 | 12 | work | Let a quiesce wait out a dead driver's end the kernel still holds |
| `os-d1/armv7-compositor` | 2026-09-23 | 1 | work | Say in the docs that the display and input gates run on ARMv7-A |
| `dk1-test` | 2026-09-23 | 4 | work | hyprix: follow_mouse focuses the window under the pointer |
| `worktree-bridge-cse_017NVB1xDHbNpY7HnGonPpen` | 2026-09-20 | 7 | work | Let the runtime say which Linux table it is, not the library |
| `ivf-row` | 2026-09-19 | 1 | work | File the host's missing IVF wallpapers |
| `guest-frame-time-local` | 2026-09-18 | 1 | history | Round and compare without the C library, and paint a terminal's rows |
| `guest-frame-time` | 2026-09-18 | 6 | work | WIP scroll fix |
| `backup/2026-10-01/guest-frame-time` | 2026-09-18 | 1 | snapshot | Round and compare without the C library, and paint a terminal's rows |
| `worktree-bridge-cse_0134i6GzHzTtV9KMyY6YsBQh` | 2026-09-17 | 2 | work | Say in the handoff that the blur is done |
| `os-e5/input-l4` | 2026-09-17 | 1 | work | WIP: libs/virtio-input, docs/INPUT.md L4, written but never compiled |
| `os-e5/compositor-render` | 2026-09-17 | 1 | work | WIP: a tiny-skia renderer for the compositor, not yet built |
| `develop` | 2026-09-17 | 2 | work | Say in the handoff that the blur is done |

## ferrousli, ports, zinc

- `codex/posix-stdlib`, `ferrousli-netcore`, `ferrousli-netdb`: the POSIX
  gap work (quick_exit, secure_getenv, crypt, the resolver, name resolution).
  The `wip/` and `backup/` copies are their snapshots.
- `ferrousli-math`, `ferrousli-math-stubs`, `ferrousli-misc`,
  `ferrousli-patterns`, `ferrousli-threads`, `os-a8/long-double`: stopped
  unfinished at the 2026-09-13/-14/-17 wind-downs; much has landed since by
  other routes, so diff against `main` before resuming.
- `os-12/ports-autobuild` (also `ports-autobuild`): the ports built only
  when stale, Phase 3 A4 of `docs/TEST-TIME.md`. The other `os-12/*` branches
  are the curl, git and btop ports' earlier states; those ports are on `main`.
- `zinc`: the first zinc commit; zinc is on `main`.

| Branch | Last commit | Unlanded | Kind | Tip |
|---|---|---:|---|---|
| `ferrousli-netdb` | 2026-10-01 | 4 | work | Finish name resolution with ether_*, getifaddrs/if_nameindex, and addrinfo/hostent |
| `ferrousli-netcore` | 2026-10-01 | 3 | work | Rework the resolver's DNS, lookup and inet pieces, and drop what stubs.rs covered |
| `codex/posix-stdlib` | 2026-10-01 | 1 | work | Add quick_exit, secure_getenv, a64l/l64a, getsubopt, and crypt's DES ciphers |
| `backup/2026-10-01/wip-ferrousli-netdb` | 2026-10-01 | 1 | snapshot | Backup of uncommitted work in ferrousli-netdb (ferrousli-netdb) before a PC switch, 2026-1 |
| `backup/2026-10-01/wip-ferrousli-netcore` | 2026-10-01 | 3 | snapshot | Backup of uncommitted work in ferrousli-netcore (ferrousli-netcore) before a PC switch, 20 |
| `backup/2026-10-01/wip-codex-posix-assert` | 2026-10-01 | 1 | snapshot | Backup of uncommitted work in codex-posix-assert (codex/posix-stdlib) before a PC switch,  |
| `wip/ferrousli-netdb` | 2026-09-24 | 1 | snapshot | WIP snapshot of worktree ferrousli-netdb, 2026-09-24, before switching PCs |
| `wip/ferrousli-netcore` | 2026-09-24 | 3 | snapshot | WIP snapshot of worktree ferrousli-netcore, 2026-09-24, before switching PCs |
| `wip/codex-posix-assert` | 2026-09-24 | 1 | snapshot | WIP snapshot of worktree codex-posix-assert, 2026-09-24, before switching PCs |
| `ports-autobuild` | 2026-09-17 | 3 | work | wip: build the ports with a pinned clang where gcc is older than 15 |
| `os-a8/long-double` | 2026-09-17 | 1 | work | WIP: the x87 long double foundation, unbuilt |
| `os-12/ports-autobuild` | 2026-09-17 | 3 | work | wip: build the ports with a pinned clang where gcc is older than 15 |
| `os-50/dev` | 2026-09-16 | 1 | never land | dev build |
| `os-12/probe` | 2026-09-16 | 3 | never land | probe: https in the guest (not for landing) |
| `os-12/ports-curl-btop` | 2026-09-16 | 1 | history | Port git onto ferrousli, and clone over HTTP inside the guest |
| `os-12/ports` | 2026-09-16 | 1 | history | Port git onto ferrousli, and clone over HTTP inside the guest |
| `os-12/https` | 2026-09-16 | 1 | history | Check curl's HTTPS inside the guest, against Mbed TLS's own test server |
| `os-12/curl` | 2026-09-16 | 1 | history | Port curl onto ferrousli, and carry it in every x86-64 image with a busybox |
| `os-12/crng` | 2026-09-16 | 1 | work | Start the clock at firmware's time, and seed a ChaCha20 generator for getrandom |
| `os-12/btop4` | 2026-09-16 | 2 | history | Port btop onto ferrousli, over LLVM's C++ runtime built against it |
| `os-12/btop3` | 2026-09-16 | 3 | history | Port btop onto ferrousli, over LLVM's C++ runtime built against it |
| `os-12/btop2` | 2026-09-16 | 3 | history | Port btop onto ferrousli, over LLVM's C++ runtime built against it |
| `os-12/btop` | 2026-09-16 | 4 | history | Port btop onto ferrousli, over LLVM's C++ runtime built against it |
| `os-12/autobuild` | 2026-09-16 | 1 | work | wip: build the ports when stale |
| `zinc` | 2026-09-14 | 1 | history | Start zinc, a zsh-compatible shell in Rust (WIP) |
| `os-50/ferrousli-patterns` | 2026-09-14 | 2 | work | WIP: dirname from ferrousli-misc and regex, stopped unfinished at the 2026-09-14 wind-down |
| `os-50/ferrousli-math-stubs` | 2026-09-14 | 1 | work | WIP: sin, cos, exp, log, pow, atan2 and the __fpclassify/__signbit helpers, stopped unfini |
| `ferrousli-patterns` | 2026-09-14 | 2 | work | WIP: dirname from ferrousli-misc and regex, stopped unfinished at the 2026-09-14 wind-down |
| `ferrousli-math-stubs` | 2026-09-14 | 1 | work | WIP: sin, cos, exp, log, pow, atan2 and the __fpclassify/__signbit helpers, stopped unfini |
| `ferrousli-threads` | 2026-09-13 | 1 | work | Keep the unbuilt barriers, semaphores, sched.h and C11 threads |
| `ferrousli-misc` | 2026-09-13 | 2 | work | Keep the unfinished search.h, libgen, fnmatch, glob and regex work |
| `ferrousli-math` | 2026-09-13 | 2 | work | Keep the unfinished math library and fenv as builds are halted |
| `enosys-sweep` | 2026-09-13 | 2 | work | Let the sweep's shell print more unanswered lines before init stops it |

## Steam, Chrome, GPU, the phone

- `wip/steam-*` and `ferrousli-steam`: Steam's store and game steps
  (`docs/roadmap/stage-22-steam.md`); `test-steam-game` landed as 99bfb7222,
  not passing yet.
- `os-ac/vulkan` and `os-ac/venus-wip`: vkgears through Venus (G3-G5 left).
- `chrome-groundwork`: the Chrome assessment's re-check.
- `pixel7-vm-try`, `pixel7-chromium`: the Pixel 7's VM desktop and Chromium's
  AArch64 signal return.

| Branch | Last commit | Unlanded | Kind | Tip |
|---|---|---:|---|---|
| `ferrousli-steam` | 2026-09-30 | 1 | work | File test-xwindow failing on main under Red on main |
| `backup/2026-10-01/ferrousli-steam` | 2026-09-30 | 1 | snapshot | File test-xwindow failing on main under Red on main |
| `wip/steam-y6` | 2026-09-29 | 1 | snapshot | WIP snapshot: WM_NORMAL_HINTS, a fixed-size window floats |
| `wip/steam-userns` | 2026-09-29 | 2 | snapshot | WIP snapshot: N2 bind mounts, host tests in progress |
| `wip/steam-e2e` | 2026-09-29 | 2 | snapshot | WIP snapshot: Steam window harness into the repo |
| `pixel7-vm-try` | 2026-09-27 | 2 | work | Give a console named by its address the console's getty |
| `pixel7-chromium` | 2026-09-26 | 2 | work | Return from AArch64 signal handlers through a vDSO trampoline |
| `os-ac/vulkan` | 2026-09-24 | 1 | work | WIP: vkgears port |
| `os-ac/venus-wip` | 2026-09-24 | 14 | history | WIP: dispatch patch |
| `chrome-groundwork` | 2026-09-20 | 2 | work | Re-check the Chrome assessment, and mark what is done |

## Kernel, init, storage, boards, certification work

- `init/l4`: libs/svc's manager (cgroups and init, L4 next).
- `btrfs-write`: stage 12's power-fail work.
- `stage8-filemmap`, `vmo-map-wip`, `stage10-ring`,
  `stage11-kernel-mount-design`: mm and storage work stopped on 2026-09-13/-14;
  check `main` first, much of it was later done differently.
- `os-8d/selfhost`: stage 20, self-hosting.
- `boot-21b` (W-8 boot, full gate green on cf13918f; left: the review, a
  rebase, the carry) and `w8-armv7a`: certification requirement work.
- `f46-power-off`: F-46, parked with its controls run; message, rebase, gate
  and the consultant left.
- `unix-creds` (also `os-02/unix-creds`): `SCM_CREDENTIALS`, never compiled.
- `worktree-agent-a33c10946b6721065`: the panic QR code.

| Branch | Last commit | Unlanded | Kind | Tip |
|---|---|---:|---|---|
| `init/l4` | 2026-10-01 | 1 | work | Give libs/svc a manager: dependency edges, lifecycle, and a restart policy |
| `backup/2026-10-01/wip-init-l4` | 2026-10-01 | 1 | snapshot | Backup of uncommitted work in init-l4 (init/l4) before a PC switch, 2026-10-01 |
| `w8-armv7a` | 2026-09-27 | 1 | work | wip: W-8 file 24 drafts (arch/armv7a, arch/arm_common), parked for the wind-down |
| `f46-power-off` | 2026-09-27 | 2 | work | WIP F-46: controls run, qemu.rs .get() fix, evidence edits; not reviewed, not rebased |
| `boot-21b` | 2026-09-27 | 1 | work | Write the boot's low-level requirements, and count ferrix.checks as product (W-8 boot, 21b |
| `cov-before-rebase4` | 2026-09-26 | 4 | history | Re-measure coverage on every architecture, and list what needs a test |
| `wip/init-l4` | 2026-09-24 | 1 | snapshot | WIP snapshot of worktree init-l4, 2026-09-24, before switching PCs |
| `sysml-studio-submodule` | 2026-09-23 | 4 | work | Move SysML Studio to its settings and shortcuts |
| `os-8d/stage20-before-rebase` | 2026-09-23 | 11 | history | wip: plan test covers reads |
| `os-8d/selfhost` | 2026-09-23 | 12 | work | wip: boot check that mmap waits for the layout |
| `btrfs-write` | 2026-09-21 | 8 | work | WIP stage 12 powerfail |
| `unix-creds` | 2026-09-17 | 2 | history | WIP 3c SCM_CREDENTIALS/SO_PASSCRED, written but never compiled |
| `os-26/fx1151-diag` | 2026-09-17 | 1 | never land | DIAGNOSTIC: net ring exit reasons (not for main) |
| `os-02/unix-creds` | 2026-09-17 | 2 | work | WIP 3c SCM_CREDENTIALS/SO_PASSCRED, written but never compiled |
| `win-parity/gateway` | 2026-09-16 | 1 | work | Boot `run` under the host's hypervisor unless told otherwise |
| `os-02/unix-gc` | 2026-09-16 | 1 | work | Collect AF_UNIX sockets that only each other's queues keep alive |
| `os-02/fx0701-diag` | 2026-09-16 | 1 | never land | WIP FX-0701 diagnostics |
| `os-02/dead` | 2026-09-16 | 1 | work | Name a disk driver that died, and how, instead of a disk that never came |
| `fx0701` | 2026-09-16 | 1 | never land | WIP FX-0701 diagnostics |
| `stage9/log-header` | 2026-09-14 | 1 | work | WIP: Name the tree a gate ran on as the first line of its log |
| `stage8-filemmap` | 2026-09-14 | 4 | work | WIP: file mmap, what is left of MAP_SHARED and MAP_PRIVATE (8 + 8 points) |
| `worktree-agent-aa3e651b1a5027598` | 2026-09-13 | 3 | never land | WIP: temporary hang instrumentation for single-CPU Arm, not for landing |
| `worktree-agent-a33c10946b6721065` | 2026-09-13 | 1 | work | WIP: port Linux's drm_panic_qr as ferrix-qr, for drawing a panic report |
| `worktree-agent-a293f44fe214785df` | 2026-09-13 | 4 | work | WIP: check the leak counters the fuzz-hardened handlers only printed |
| `vmo-map-wip` | 2026-09-13 | 2 | history | WIP: keep vmo_map's check and remap under clippy's length limit |
| `stage8-diag` | 2026-09-13 | 2 | never land | DIAGNOSTIC, not for landing: count frames by release route across two self-check windows |
| `stage11-kernel-mount-design` | 2026-09-13 | 1 | work | WIP: keep stage 11's kernel-mount design where a restart cannot lose it |
| `stage10-ring` | 2026-09-13 | 2 | work | WIP: the block ring's kernel side, unbuilt |
| `sched-diag` | 2026-09-13 | 1 | never land | Charge the running task before a queue insert, honour a reschedule owed from an empty reap |
| `os-c4/diag` | 2026-09-13 | 2 | never land | DIAGNOSTIC, not for landing: count frames by release route across two self-check windows |
| `board-reset-3c-pre-rebase` | 2026-09-13 | 2 | history | Reset the machine when asked, so a board run needs no hand at the board |

## os-5d: superseded by main

os-5d's work landed as 7afed6fdc on 2026-10-01 (`--iterate`, `gate-rows`, the
compositor's first-boot checks, `docs/TEST-TIME.md`'s Windows host). These are
its older copies; delete them.

| Branch | Last commit | Unlanded | Kind | Tip |
|---|---|---:|---|---|
| `os5d/gate-rows` | 2026-10-01 | 1 | superseded | Record the Windows host's first cold gate runs, and why neither finished |
| `backup/2026-10-01/wip-os5d-gate-rows` | 2026-10-01 | 1 | superseded | Backup of uncommitted work in os5d-gate-rows (os5d/gate-rows) before a PC switch, 2026-10- |

## Already on main

Every change on these branches is on `main` under another hash; they can be
deleted when the user says so (`branch-cleanup-method`: by patch-id, only the
surveyed tips): `backup/2026-10-01/os5d/gate-rows`,
`backup/2026-10-01/os5d/profile-iterate`, `backup/2026-10-01/po-winddown`,
`os5d/compositor-skip`, `stage13-n4-userns`, `stage13-s1`, `stage13-todos`,
`wip/steam-procfs-fd`, `pixel7/bootloader`, `os-d1/monitor-transform`, `omz`,
`omz-local-2026-09-27`, `wallpaper-settings`, `os-a4/compositor-damage`,
`os-02/smmu`, `os-50/batch3b`, `os-50/batch3c`, `os-50/ci`, `os-50/land`,
`os-a8/kept-os02-stub-462f7714`, `os-50/wscanf`, `os-12/btop5`,
`os-50/ferrousli-link-breakers`, `os-50/ferrousli-netdb`,
`pre-pull-backup-2026-09-13`.

## Not on a branch

- **The gate pool's deadlock fix** for `~/.local/share/ferrix/fleet/gate.sh`
  on nazuna, which is not in the repository: a run held to one slot
  (`GATE_SLOT`) must not hold the queue (`docs/TEST-TIME.md`, *Deadlock*).
  Not installed.
- **The Windows host as a gate** (`docs/TEST-TIME.md`, *The Windows host as a
  gate*): a cap on WSL's target dirs or the WSL disk off C:, a C: disk guard,
  then a cold and a warm `check` measured to the end.
