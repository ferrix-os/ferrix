# Claude Code — Anthropic's coding agent on Ferrix  ·  *the command line, 2026-09-30; the desktop app being assessed*

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

**Still to do:**

* A real account: the network, TLS and the browser hand-off are measured
  up to the sign-in page, which is the customer's own step (§5.2).
* An i386 signal frame with the `XSAVE` area: a first attempt backed out,
  what it takes in §3 (`docs/BACKLOG.md`).
* linux-arm64 on AArch64 (§6).
* The Claude desktop app, an Electron application Anthropic builds for
  macOS and Windows only: the customer chose it on 2026-09-30, and an
  assessment comes before any building (§7).
