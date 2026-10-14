# Claude Code — Anthropic's coding agent on Ferrix  ·  *the command line, 2026-09-30; the desktop app assessed, 2026-10-14*

Placed after Chrome without a number of its own. The customer asked on
2026-09-30 for Claude Code on Ferrix, the command line first and then the
desktop app; `docs/CLAUDE-CODE.md` is the account. The program is
Anthropic's prebuilt native release for linux-x64, 2.1.280, on Debian 13's
glibc, from a btrfs volume `tools/common/fetch/fetch-claude-code.sh` makes
from pinned downloads, as Chrome's is.

**Exit:** Claude Code runs on Ferrix and uses a tool there, and the desktop
has it. Met for the command line on x86-64 (2026-09-30): `cargo xtask
test-claude-code` runs `claude --version`, then one turn of `claude -p`
against a Messages API xtask serves, in which the model asks for a Bash
command and Claude Code runs it in bash on Ferrix and sends its output
back; `--everything` runs the same on the `run-compositor --everything`
desktop's volume, whose terminals, and `remote-desktop`'s, have `claude`.

**Done (2026-09-30).** In the order running it found them:

* AVX for programs. Claude Code's Bun runtime is built for AVX2 with no
  fallback, and spun forever in its first string conversion on a Ferrix
  that neither saved AVX nor offered it. The kernel now sets `CR4.OSXSAVE`,
  enables x87, SSE and AVX in `XCR0`, saves them with `XSAVE` at every
  switch, and carries them in signal frames as Linux does; xtask's x86-64
  model has x86-64-v3's instructions, and `XSAVEOPT`, without which QEMU's
  TCG loops on the write of `CR4` (`docs/CLAUDE-CODE.md` §3).
* The volume, the gate and its API, and `claude` on the `--everything`
  desktop (§2, §4, §5).

**Done (2026-10-01).** `claude` on the `--everything` desktop had ended at
start since 5f89f107, on ferrousli's loader, which that desktop runs every
glibc program on: `ld-ferrousli: undefined symbol: timer_create`. The gate
had booted with glibc's links. ferrousli now has the POSIX timers and
`__cxa_at_quick_exit`, and `test-claude-code --everything` runs Claude Code
on ferrousli as the desktop does, with a negative control (§5).

**Done (2026-10-11).** A developer's whole session by hand, in term on
the desktop and over `ssh -tt`, against a scripted API: onboarding, a
turn of Bash, Write, Read, Grep, Glob, Edit and `cargo test`, permission
prompts, Esc and Ctrl-C (the running command's process group goes too),
resizes and `/exit`, as on the host. It found `rustdoc` missing from the
desktop's `PATH` and ten of the TUI's characters drawn as boxes by term;
both are fixed (§5.1).

**Done (2026-10-15).** linux-arm64 on AArch64, under QEMU TCG: `cargo
xtask test-claude-code --arch aarch64` runs the same script, turn and TUI
on Claude Code's linux-arm64 build from a volume of its own
(`fetch-claude-code.sh arm64`), with no kernel change (§6).

**Still to do:**

* A real account: the network, TLS and the browser hand-off are measured
  up to the sign-in page, which is the customer's own step (§5.2).
* An i386 signal frame with the `XSAVE` area: a first attempt backed out,
  what it takes in §3 (`docs/BACKLOG.md`).
* The Claude desktop app, the Electron application the customer chose on
  2026-09-30. Assessed on 2026-10-14 (§7). Anthropic now publishes a Linux
  build (a beta, 2.37337.1 on Electron 44.7.0), and the recommendation is
  to run that natively, as Chrome runs. It is C1 to C4, 12–18 points as
  first guesses, with sign-in by hand at every start. A kept sign-in and
  notifications (C5 to C7) make it 21–33, and they need D-Bus and a Secret
  Service, which the customer decided on 2026-10-14 `authd` will serve, not
  gnome-keyring. C1 to C4 are ordered; C5 to C7 are not yet. Wine and repackaging the
  Windows bundle are rejected. claude.ai in Chrome, 1 point, is the
  stopgap. Not started.
