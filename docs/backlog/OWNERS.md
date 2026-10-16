# Backlog: the session roster as last kept (2026-09-26 to 10-01)

Moved out of [`docs/BACKLOG.md`](../BACKLOG.md) on 2026-10-16: every session in it has wound down.

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
| ferrix-41 | Stage 22's 32-bit x86 ABI (`docs/I386.md`): I1 to I4 on main, I5a (steamcmd logging in, `test-steamcmd`), and steamcmd in the `--everything` desktop's terminal. **Wound down 2026-09-27.** F-46, parked on branch `f46-power-off`, landed on 2026-10-11 from `land-f46`. The yserver feasibility pass was met on 2026-09-28 (`test-yserver`), and its design is `docs/YSERVER.md`. Y1 to Y4 and Y5a are done (`test-yserver`, `test-xwindow`, which sees xev's window on hyprix, gives it keys, clicks, the wheel and its cursor, has hyprix size and close it, and floats a transient xev as a dialog). Y5b, menus as popups, landed; so did Y7, yserver on `run-compositor --everything` (`test-xwindow --everything`). Y6, then I5b |
| ferrix-90 | **Test and gate run time**, the customer's priority one (`docs/TEST-TIME.md`; phase 1 measured, phase 2 the cuts, of which the Arm firmware waits and cut 2 are done, the latter by the PO session on 2026-09-28; handover `~/.local/share/ferrix/ferrix-90/HANDOVER.md`). Landed 2026-09-27: Chrome playing through `pulsed` on ferrousli, as the desktop runs it (0c55ae64); F-10's `syscall/native.rs` slice with F-42, a refused write's handles kept (288043a9), and `object/`'s (5792cc5c); W-8's claim and device requirements, part 20, with a function's bus mastering read back after a quiesce (dec60319); x86-64's coverage re-measured, 90.1%, floor 89.0 (11532464). Still owns audio (`docs/AUDIO.md`: L1-L7, U1 and U2 done; next U3, SDL and games finding what is missing by running) and `src/user/system/linux/media/`; `test-badapple`, the `badapple` app's gate since 2026-10-01, is in the gate of any slice that touches `pcm` or `resample` |
| ferrix-d5 | The Rust desktop clients (`docs/DESKTOP-CLIENTS.md`: the clients-base crates, waybar, fuzzel, hyprlock, hypridle), the EDID override, `docs/AUTH.md` and its phase 1; at most three streams running at once |
| ferrix-55 | Standing certification consultant (reviews item changes before they land; keeps `docs/certification/` current). **Wound down 2026-09-27 ~15:30.** Its open review queue -- F-46 (landed 2026-10-11), F-48's diff, F-49's design, W-8 21b's diff and 21c, ferrix-90's VERIFICATION §3 note, the Arm firmware-wait change -- with every condition already set, is at the end of `~/.local/share/ferrix/cert-consultant/reviews.md`, and the handover is `~/.local/share/ferrix/cert-consultant/HANDOVER.md` |
| ferrix-e1 | The repository relayout (`docs/LAYOUT.md`), landed 2026-09-26 as 26303ad5. **Winding down** after its post-landing rows and cleanup |
| ferrix-9c | The Pixel 7's USB CDC-ACM device driver (`docs/vendor/google/pixel7/USB-HANDOVER.md`): the phone showed up as `/dev/ttyACM0` on example on 2026-09-26. **Winding down** once `usbdev` with the kernel log channel has landed |
| ferrix-d4 | The Pixel 7's GUI, option A: a desktop in the launcher app's crosvm VM, with Chromium on it; landed 2026-09-27. **Winding down** after that landing |
| ferrix-8e | This file's cleanup (landed 6b942df1); two_clients' GPU-frame flake. **Winding down** after that |
| ferrix-b0 | Bad Apple!! with sound (`docs/MEDIA.md`, the customer's order of 2026-09-26), landed; now Bad Apple on the `--everything` desktop (a toolkit window, `SUPER M`), then stopping; Doom is in the backlog, unowned |
| open | The Pixel 7 bring-up (`src/boot/vendor/google/pixel7`, statd, `tools/vendor/google/pixel7`; was ferrix-0a); Chrome's extensions bubble and `test-chrome-window`'s context-menu step (was ferrix-a8); every row below owned by an `os-*` session before 2026-09-26 |

---
