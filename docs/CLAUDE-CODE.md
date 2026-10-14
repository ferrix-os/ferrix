# Claude Code on Ferrix

Asked by the customer on 2026-09-30: "add Claude Code (CLI first, then the
desktop) to Ferrix". This is the account of the first half -- the command
line -- and of what running it found. The desktop app is §7.

## Where it stands, 2026-10-11

**Claude Code runs on Ferrix, since 2026-09-30.** Anthropic's prebuilt
native release 2.1.280 for linux-x64 -- not one built here -- starts on
Ferrix, takes a prompt with `claude -p`, sends it to a Messages API, runs
the Bash command the model answers with in bash on Ferrix, and sends back
what it printed. `cargo xtask test-claude-code` requires all of it, against
an API xtask serves itself (§4), in about two seconds of the guest's time.

| | state |
|---|---|
| Claude Code's native linux-x64 build on Debian's glibc, from a pinned volume | **done** 2026-09-30, `tools/common/fetch/fetch-claude-code.sh` (§2) |
| AVX for programs: `XSAVE` in the kernel, and x86-64-v3 on the QEMU model | **done** 2026-09-30, the one thing running it found missing (§3) |
| `claude --version`, and a turn of `claude -p` that runs a tool on Ferrix | **done** 2026-09-30, `cargo xtask test-claude-code` (§4) |
| `claude` in the terminals of `run-compositor --everything`, and so of `remote-desktop`'s desktop | **done** 2026-09-30; broken there until 2026-10-01, when the gate learned to run it on ferrousli as that desktop does (§5) |
| On ferrousli's loader and `libc.so.6` in glibc's place, as the `--everything` desktop runs every glibc program | **done** 2026-10-01, ferrousli's POSIX timers; `cargo xtask test-claude-code --everything` (§5) |
| The interactive TUI, typed at on the console: a prompt, the tool's answer drawn, `/exit` | **done** 2026-09-30, the same gate's second half (§5) |
| Claude Code in the `--everything` desktop's launcher, in a terminal of its own | **done** 2026-09-30, `claude.desktop` (§5) |
| Zenbleed and Gather Data Sampling: AVX withheld where nothing covers them | **done** 2026-09-30, `speculation::vector_leak` (§3) |
| A developer's session, by hand: onboarding, a ten-step turn with Bash, Write, Read, Grep, Glob, Edit and `cargo test`, permission prompts, Esc and Ctrl-C, resizes, `/exit`, in term and over `ssh -tt` | **works** 2026-10-11; two fixes, rustdoc on `PATH` and term's glyphs (§5.1) |
| A real account | the network, TLS and the browser hand-off work up to the sign-in page; the sign-in itself is the customer's (§5.2) |
| AVX state in a 32-bit program's signal frame | a first attempt backed out; what it takes is in §3 |
| On AArch64 with the linux-arm64 build | not started (§6) |
| The Claude desktop app | **C1 to C4 built** 2026-10-15, on branch `claude-desktop`: Anthropic's Linux build (2.37337.1, Electron 44.7.0) on the `--everything` desktop, natively on ferrousli, to its sign-in screen; `cargo xtask test-claude-desktop`. Sign-in by hand at every start; C5 to C7 not ordered (§7.8) |

**Certification review (consultant, 2026-09-30): OK**, on b0db74ef, after a
first round on 672611b6. The change is core ring (arch/x86_64: cpu.rs,
switch.rs, signal.rs, smp.rs, mod.rs) and adds no upward reference. Its
unsafe is core::arch's XSAVE64, XRSTOR64, FXSAVE64, FXRSTOR64 and XSETBV
intrinsics, each with a (CONTEXT) obligation, and there is less assembly
than before. rt_sigreturn takes only XSTATE_BV and the YMM upper halves
from a frame: XSTATE_BV is masked to AVX, x87 and SSE and then to XCR0,
and XCOMP_BV and the reserved header bytes come from the kernel's own
XSAVE, never from user memory. So no program can make XRSTOR fault in ring
0. The xstate boot line (check_a_poisoned_xsave_header_comes_back_safely,
L.x86_64.122) returns a frame whose whole header is ones; with both masks
removed, the boot stopped on #GP under TCG and KVM. A thread starts, and
an exec restarts, with AVX outside XSTATE_BV, so no YMM state crosses
processes. Enabling AVX exposes Zenbleed (CVE-2023-20593) and GDS
(CVE-2022-40982). SPECULATION.md §3 and §9 record that the kernel
mitigates neither today, and that under KVM the host's microcode and
chicken bit do. A BACKLOG row owes keeping XCR0 at x87 and SSE on an
unmitigated processor when the speculation switch is on. L.x86_64.121,
enabling the state on every processor, is baselined: every SMP boot
exercises it, since a processor without it faults on its first user
switch. The i386 frame stays FXSAVE-only (BACKLOG).

**Zenbleed and GDS (certification consultant, 2026-09-30): OK**, on 18873ab8.
This met the condition the XSAVE review left. speculation::vector_leak
decides before XCR0 is written, from CPUID, IA32_ARCH_CAPABILITIES, the AMD
patch level and IA32_MCU_OPT_CTRL, with Linux's model lists. A bare-metal
Zen 2 gets DE_CFG[9] on every processor unless its microcode is at the fixed
revision. An affected Intel part gets the microcode mitigation turned back
on where it is off and unlocked. Where nothing covers the leak (no
microcode, the mitigation locked off, or a Zen 2 guest) programs do not get
AVX, which is Linux's gds=force. Each MSR is read only where CPUID or
ARCH_CAPABILITIES says it exists, each secondary re-applies and reads back
its setting, and a mismatch stops the boot. The decision is checked for
twelve processors (L.x86_64.123), with a negative control. The writes
themselves run only on affected hardware, never under the gates' QEMU. In a
guest the verdict rests on the CPU model the hypervisor presents, so there
the host's mitigation is what protects (AoU-11).

**Both negative controls, re-run with their logs (certification consultant
os-ad, 2026-10-01).** Neither landing kept a log of the control its message
claims, which the verification auditor (os-db) found. Both were run again on
`main` 503d65aa, x86_64 under TCG, each a throwaway tree booted once, and both
fired. With `restore_fpu`'s `set_xstate_bv(components)` and `set_xstate_bv`'s
`kept = components` (both masks out), the boot stopped at 7.51 s on
"FERRIX-PANIC general protection fault", vector 13 in ring 0 with the
frame's all-ones `XSTATE_BV` in `rbx`, before any `xstate` line. With the
guest rule taken out of `speculation::zenbleed`, it stopped on "side-channel
defence self-check failed: a processor was given the wrong verdict on
Zenbleed or GDS". That check is a pure function of the described processors,
so TCG shows it as well as KVM. The logs are `osad-ctl-xstate.log` and
`osad-ctl-zenbleed-guest.log` in the gate host's queue logs.

## 1. What Claude Code is, to an operating system

Since 2.1 Claude Code ships as a native program rather than an npm package:
Bun, a JavaScript runtime built on JavaScriptCore, with Claude Code's
JavaScript and its ripgrep bundled into one 234 MB executable. The linux-x64
build is a non-PIE glibc program that needs nothing of the system's but
glibc itself -- `libc`, `libm`, `libpthread`, `libdl`, `librt` -- and reads
its own bundle out of its executable. What it does with the system is what
Chrome does, less a display: threads, `futex`, `epoll`, `eventfd`,
JavaScriptCore's JIT, TCP through the resolver, `fork` and `execve` of a
shell for every tool call, and pipes back from it.

Everything a browser needed had been built for Chrome (`docs/CHROME.md`):
`execve` of a program past 64 MiB mapped from its file, the vDSO,
`madvise`, `timerfd`, the network. So the expectation was a short list of
missing calls. The list had one entry, and it was not a call.

## 2. The volume

`tools/common/fetch/fetch-claude-code.sh` makes a btrfs volume, as
`fetch-chrome.sh` does, from pinned downloads:

* `claude` 2.1.280 for linux-x64, the stable channel's release on
  2026-09-30, from `downloads.claude.ai`, the URL Anthropic's `install.sh`
  uses, checked against the SHA-256 that release's `manifest.json`
  publishes;
* Debian 13's `libc6` -- the pin Chrome's volume takes -- `libgcc-s1`,
  `bash`, `libtinfo6`, `ripgrep` and `libpcre2-8-0`.

bash because Claude Code's Bash tool runs every command in bash or zsh and
in nothing else, and `SHELL` points it at the volume's. The script checks
that every library each program on the volume needs is on it, which caught
ripgrep's PCRE2 the first time it ran.

The volume is mounted at `/data`, under QEMU's `snapshot=on`, and the
initramfs links glibc's paths into it (`claude_code::LINKS`), as Chrome's
test does.

## 3. What running it found: no AVX

`claude --version` answered at once. `claude -p` never made a request: its
main thread ran at full speed, in state R, for as long as it was given, and
made no system call after its first 1.5 seconds. It wrote nothing, not even
its configuration directory.

A trial kernel, never landed, sampled the thread's user instruction pointer
at each timer interrupt and walked its frame pointers through the user
stack. Every sample was in the same place:
JavaScript calling Bun's conversion of a string to UTF-8, which called,
through a lazily built object's table, a function that did nothing but
return the pair `(11, 0)`. That is simdutf's *unsupported implementation*:
error `OTHER`, nothing converted. The conversion loop asks again for the
rest, and there is always all of it left.

simdutf chooses its implementation at run time from what the processor
offers. Bun's x86-64 build -- the one Claude Code ships -- is compiled for
Haswell and carries no implementation older than AVX2, and a program may
use AVX only when `CPUID` says the operating system saves it (`OSXSAVE`) and
`XGETBV` says it is enabled. On Ferrix neither was true twice over:

* **The kernel** saved a program's floating-point state with `FXSAVE`, which
  holds x87 and SSE and not the upper halves of the `YMM` registers, and so
  never set `CR4.OSXSAVE`. A program could not use AVX under it even on a
  processor that had it.
* **QEMU's model**: xtask booted every x86-64 guest on `qemu64`, which has
  SSE3 and nothing after it -- no SSSE3, no SSE4, no `XSAVE`, no AVX.

Chrome ran on both because it chooses its code at run time all the way down
to SSE2. Anything built for x86-64-v3 without a fallback -- which is most of
what distributions now ship for "modern" x86 -- could not.

### The fix

**In the kernel** (`src/kernel/src/arch/x86_64/`):

* `cpu::enable_extended_state` sets `CR4.OSXSAVE` and puts x87, SSE and AVX
  in `XCR0` on the boot processor; each secondary loads the same `XCR0` as it
  starts (`smp::secondary_start`), having taken `CR4` from the boot
  processor already. Only those three components, even on a processor with
  AVX-512 or AMX: each further one grows every thread's saved state and
  every signal frame, and a program asks `XCR0` before using them.
* The switch saves and restores a program's registers with `XSAVE64` and
  `XRSTOR64` into an 832-byte area in the standard form, 64-byte aligned, in
  place of the 512-byte `FXSAVE` area (`switch::UserState`). A processor
  without `XSAVE` keeps `FXSAVE`; the boot log says which:
  `program register state: x87, SSE and AVX, saved with XSAVE`.
* A signal frame carries the `XSAVE` area as Linux lays it out: the
  software-reserved bytes of the `FXSAVE` part say so
  (`FP_XSTATE_MAGIC1`, the sizes, the components), the header and the
  `YMM` upper halves follow, then `FP_XSTATE_MAGIC2`, and `uc_flags` has
  `UC_FP_XSTATE`. `rt_sigreturn` takes AVX back only from a whole area,
  and otherwise returns it to its initial state, as Linux does. So a
  handler that uses AVX -- glibc's `memcpy` does -- no longer hands the
  code it interrupted different registers.

Without the switch saving the upper halves, the first run with AVX turned
on and nothing else changed printed Claude Code's answer and then crashed as
Bun left, at address 0, with four threads taking turns on one processor.
With it, the crash is gone.

**In xtask** (`qemu::x86_cpu`): every x86-64 guest gets x86-64-v3's
instructions, under every accelerator -- SSSE3 to SSE4.2, `XSAVE`, AVX and
AVX2, BMI1 and 2, FMA, F16C, `MOVBE`, `LZCNT`, `PCLMULQDQ` and AES. Every
x86-64 processor sold in the last decade has them, and TCG emulates them
all since QEMU 7.2. The coverage suite's own model, which has none, keeps
`FXSAVE`'s path exercised.

And `XSAVEOPT`, for a reason that is QEMU's: its TCG takes `CR4.OSXSAVE` for
a reserved bit unless the model has one of `CPUID` leaf 0xD's sub-leaf 1
features (`cr4_reserved_bits` tests `FEAT_XSAVE`, not leaf 1's `XSAVE`).
On `qemu64,+xsave` the kernel's write of `CR4` then became an SVM exit
outside any guest, which restarts the instruction: the boot stopped on one
`mov cr4`, forever, with no exception logged, under QEMU 9.2 and 10.2
alike, and ran under KVM, which checks `CR4` itself. Every processor with
AVX has `XSAVEOPT`, so the model is still one that exists.

The i386 signal frame (`signal/compat.rs`) still carries `FXSAVE` alone: a
32-bit program's handler that uses AVX would leave the interrupted code's
upper halves changed. Steam's 32-bit programs are the only ones, and none
seen so far uses AVX in a handler; the row is in `docs/BACKLOG.md`.

**A first attempt at it was backed out (2026-09-30).** It gave both ia32
frames the 64-bit frame's `XSAVE` area, at the `_fpstate_32`'s `FXSAVE`
image, grew the space `compat::place` reserves from 512 bytes to the 836 the
area and `FP_XSTATE_MAGIC2` take, and put `UC_FP_XSTATE` in an
`rt_sigframe_ia32`'s `uc_flags`; `rt_sigreturn` already restores through
the shared `restore_fpu`, which reads such an area. The existing i386
signal check still passed with it, but a 32-bit program whose `SA_SIGINFO`
handler uses AVX took `SIGSEGV` on entering the handler, and the cause was
not found. What doing it properly takes:

1. **The layout, on paper first.** Compare `compat::place` and the frame
   builders, line by line, with Linux's ia32 path -- `get_sigframe`,
   `fpu__alloc_mathframe` with `XSAVE` in use, `copy_fpstate_to_sigframe`,
   `ia32_setup_rt_frame` -- for where each part goes when the math frame
   is no longer 624 bytes: the `fsave` environment below the image, the
   64-byte alignment of the image, the frame below it with `sp + 4`
   16-byte aligned, and the `fpstate` pointer the `sigcontext_32` carries.
2. **Both frames**: `sigframe_ia32`, for a handler without `SA_SIGINFO`, as
   well as `rt_sigframe_ia32`, and `UC_FP_XSTATE` only in the latter.
3. **The check**, in `arch/x86_64/trap/check.rs` beside the i386 signal
   check: an i386 program puts all ones in `YMM1` (`vxorps` then
   `vcmpeqps`, AVX alone), signals itself, has its handler zero `YMM1`, and
   after `rt_sigreturn` compares `YMM1` with all ones (`vptest`), exiting
   79 when they match and 77 when they do not; run only where programs
   have AVX. Negative control: the frame writing `FXSAVE` alone again must
   make it exit 77. A requirement, `L.x86_64.124`, names the frame's units
   and this check, and `test-boot` on xtask's own model requires the
   check's line, as it requires the `xstate` line.
4. **The certification consultant's review**, since `signal/compat.rs` is
   core ring.

## 4. The gate: `cargo xtask test-claude-code`

One boot, x86-64, with the network. The script takes a lease for `eth0`,
runs `claude --version`, then one prompt:

```
claude -p 'ferrix-gate: run the check' --allowedTools Bash --max-turns 3 < /dev/null
```

`ANTHROPIC_BASE_URL` points at a Messages API in a thread of xtask
(`claude_code::Api`), on the host's loopback, which the guest reaches as
`10.0.2.2` through the gateway, as `test-net`'s servers are reached. It
requires the made-up key the script gives, and answers:

* the prompt, when it comes with tools, with a call of the Bash tool
  running `echo ferrix-bash-$((6*7)); uname -s`, streamed as the API
  streams one;
* the request that carries that command's output back as a tool result --
  `ferrix-bash-42`, then `Ferrix` -- with the text
  `claude-code-gate: bash on Ferrix said ferrix-bash-42`;
* anything else Claude Code asks on its own with a word, and its keyless
  reachability check, `HEAD /api/hello`, with an empty 200.

The gate passes when `--version` printed Claude Code's version, the guest
printed that last text, the API saw the tool result, and the script got to
its end. The text exists only if bash ran the command on Ferrix: the
script's source holds neither `42` nor the reply. No model is called, no
account is needed, nothing leaves the host, and the result is the same on
a machine with no internet. `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC`
keeps the telemetry, error reports and update checks at home.

`-p` reads its input as more of the prompt until the end of it, whenever
the input is not a terminal, and the console is not one to it: the script
gives it `/dev/null`.

**The negative control** (2026-09-30): the same gate with
`FERRIX_X86_CPU` set to the model before this change fails -- the boot log
says `saved with FXSAVE; no XSAVE, so no AVX`, and `claude -p` spins until
the timeout.

## 5. On the desktop, and what is left for the command line

**`claude` in the desktop's terminals** (2026-09-30). `run-compositor
--everything` merges this volume's tree into the one data volume that
desktop mounts, beside rustc's, Chrome's, steamcmd's and the Steam
window's (`everything.rs`), and runs `fetch-claude-code.sh` first when the
volume is missing, stopping if the fetch fails; the only path
two of them share with different contents is `libgcc_s.so.1`, which the
merge already settles for Chrome's copy of the same file. The image then
carries `/bin/claude` (`claude_code::WRAPPER`), which runs
`/data/claude-code/claude` with `SHELL` naming the volume's bash -- the
terminals' shell is zinc -- and Claude Code's updater off, since the volume
is started afresh every boot. `cargo xtask remote-desktop` boots
`run-compositor --everything` on the machine it names, so its desktop has
`claude` wherever the volume has been fetched there.
`cargo xtask test-claude-code --everything` runs the gate's script on that
merged volume, through the same `/bin/claude`, and on the same libc as the
desktop (below), so what it passes is what a terminal on the desktop runs.
The plain gate goes through `/bin/claude` too, on the volume's glibc.

**On ferrousli, as the desktop runs it** (2026-10-01). The `--everything`
desktop runs Chrome, and with it every glibc program on the data volume, on
ferrousli's loader in `/lib64` and its `libc.so.6` in `/lib`, unless
`--interpreter glibc` asks for the volume's own (`docs/CHROME.md`).
5f89f107 made `/bin/claude`'s files leave `/lib64` to that loader, so
`claude` in the desktop's terminals ran on ferrousli -- and ended at once,
`--version` too, with `ld-ferrousli: undefined symbol: timer_create`. The
gate had passed all along because it booted with glibc's links, which the
desktop does not have. Claude Code imports four names ferrousli lacked
without a weak binding: `timer_create`, `timer_settime`, `timer_delete` and
`__cxa_at_quick_exit`. ferrousli's loader binds every name at start, so the
first missing one ends the program, though a session calls none of the
timers (traced on the host). ferrousli now has all five POSIX timer
functions, each its system call as in musl -- Ferrix's kernel answers them
`ENOSYS`, as it answers glibc's, and a Linux host runs ferrousli's C test of
them for real -- with `SIGEV_THREAD` refused by `ENOTSUP` until ferrousli
starts timer threads; and `__cxa_at_quick_exit`. The weak ones it lacks
(`pidfd_spawnp`, `pidfd_getpid`, `posix_spawn_file_actions_addchdir`,
zstd's trace hooks) resolve to null, and Claude Code runs without them.
The gate with `--everything` now carries ferrousli's loader and
`libc.so.6` as the desktop does, with its search path, and passes: the
`-p` turn runs bash and the TUI answers. **The negative control**: the same
gate with `main`'s ferrousli fails with the desktop's own words,
`ld-ferrousli: undefined symbol: timer_create`, and the shell exits 3.

**The TUI, gated** (2026-09-30). After its `-p` turn, the gate's script
writes `~/.claude.json` -- onboarding done, the gate's key approved by its
last 20 characters, `/tmp` trusted -- and starts `claude` on the console,
which is a terminal to it. xtask types a prompt and Enter once the TUI has
made its startup check of the API, and `/exit` and Enter once the API has
sent the reply to the tool's run, then requires the shell to go on. The
TUI places every word with a cursor movement and ends no line, so the
console reader sees its screen only after it exits; the verdict reads that
screen with the escape sequences and spaces taken out, and requires Claude
Code's name and its shortcut hint, and the reply. Bun passes descriptors to
`ioctl` NaN-boxed (`0xfffe000000000001` for 1); `fd::arg` narrows every
descriptor to 32 bits, as Linux's `unsigned int` does, and the TUI's raw
mode on a real terminal is the proof.

**In the launcher**: `claude.desktop`, carried with `/bin/claude` where the
volume has been fetched, `Terminal=true`, so fuzzel opens it in a terminal.

### 5.1 A developer's session, measured (2026-10-11)

On `run-compositor --everything --accel kvm`, against a scripted Messages
API on the host reached as `10.0.2.2` (streamed replies, tool calls, a
dummy key; no account, nothing leaving the host), Claude Code was driven
as a developer drives it: in term with zinc on the desktop, as `ferrix`,
over VNC, and over `ssh -tt` through a host pty rendered by a terminal
emulator. The same binary ran on the host against the same API as the
reference. Everything below behaved as on the host:

* start-up and onboarding (theme, the key, the security notes, the trust
  dialog), drawn whole; `~/.claude.json` on the home disk survives a
  reboot, so the second start goes straight to the prompt;
* one turn of ten tool calls in a fresh crate under git -- Bash
  (`git init`, `uname`), Write, Read, Grep, Glob (both through the bundled
  ripgrep), `cargo test` failing, Edit, `cargo test` passing, a `git
  commit` -- with each permission prompt answered;
* Esc and Ctrl-C while a Bash command runs: the turn is interrupted and
  the command's whole process group is gone; Esc while a reply streams;
  Ctrl-C clears the input, two end the program with status 0 and the
  terminal restored; `/exit`;
* the window resized (a second terminal tiled beside it, then closed; and
  `TIOCSWINSZ` over ssh): the TUI redraws at the new width each time;
* `!` shell mode, the `/` command menu, `@` file completion, `claude
  doctor` (whose only warnings are about an install it did not make).

Two things failed, both outside Claude Code and the kernel, and both are
fixed: `cargo test` on any library crate stopped at its doctests, because
the desktop's `/bin` had `rustc` and `cargo` but not `rustdoc`
(`rustc::DEFAULT_LINKS`); and term drew ten of the TUI's characters as
boxes -- the elbow under each tool call (⎿), the mode line's ⏸ and ⏵,
⏺, ✔ and every frame of the spinner -- because Hack has none of them.
term now draws each with a stand-in Hack has (`gen-font.py`'s
`STAND_INS`), the spinner's frames with different ones so it still turns.

### 5.2 A real account

From the guest, TLS to `api.anthropic.com`, `platform.claude.com`,
`claude.ai` and `claude.com` works: `claude -p` with a made-up key gets the
API's own 401 and says `Invalid API key`. Started with no key in a
terminal on the desktop, `claude` offers its three login methods; the
first (a Claude subscription) runs `xdg-open`, which opens Chrome on the
same desktop at the claude.ai sign-in page, while Claude Code listens on a
loopback port for the browser's redirect and also prints the URL with a
"Paste code here" field for the manual path. Nothing past the sign-in
page was done: that is the customer's own step. What they do: in a
terminal on the desktop run `claude`, pick a text style, choose "Claude
account with subscription" (or "Anthropic Console account" for API
billing), sign in in the Chrome window that opens, and approve; Chrome
returns to Claude Code on `localhost` and the session starts. If the
redirect does not arrive, they copy the code the page shows into "Paste
code here". The credential is then kept in `~/.claude` on Ferrix's home
disk, across reboots, and in no other place. `ANTHROPIC_API_KEY` in the
environment is the other way, without a browser.

Once, right after another session, the start-up check reported
"Connection to platform.claude.com timed out after 10 seconds"; three
fresh starts after it reached the login menu in about a second, and curl
reached the same host at once. Not reproduced; noted in case it returns.

What is left:

* **The sign-in itself**, the customer's (§5.2).

## 6. Elsewhere

* **ferrousli in glibc's place**: done on the `--everything` desktop,
  2026-10-01 (§5). The plain gate stays on the volume's glibc.
* **AArch64**: Anthropic publishes linux-arm64, for the Pixel 7's VM; its
  Bun build needs no AVX.

## 7. The desktop app (assessed 2026-10-14)

Asked for second. The Claude desktop app is an Electron application. On
2026-09-30 the customer chose the Electron app itself over Claude Code in
a desktop window or claude.ai in Chrome. At that time the app had no Linux
build, so the plan was to run its Windows bundle's JavaScript on Linux's
Electron. This section is the assessment that was to come first, before
anything is built. Nothing was built, nothing of Anthropic's was run, and
no account was used.

**The first finding changes the question: Anthropic now publishes a Linux
build.** It is a beta for Debian 12 or later and Ubuntu 22.04 or later, on
x86-64 and arm64, from an apt repository of Anthropic's own
(`code.claude.com/docs/en/desktop-linux`; its `postinst` says the
repository has served a signed index since a July 2026 launch). It is the
same release as the Windows one, 2.37337.1, on the same Electron. So the
Electron app on Ferrix is now the same job as Chrome on Ferrix (§7.3, (d)),
and it is the recommendation. Repackaging the Windows bundle, (b), is
moot.

### 7.1 What was fetched, and how it was checked

Both downloads are on nazuna, under `~/.local/share/ferrix/claude-desktop/`,
each in a new directory of its own. They were read as data only: no
maintainer script ran, no program ran, and nothing was started on the host.

| | Linux (`fetch-2026-10-14/`) | Windows (`fetch-win-2026-10-14/`) |
|---|---|---|
| Where from | `downloads.claude.ai/claude-desktop/apt/stable`: the `InRelease` file, the `binary-amd64/Packages` index, the key at `downloads.claude.ai/claude-desktop/key.asc`, and `pool/main/c/claude-desktop/claude-desktop_2.37337.1_amd64.deb` | `claude.ai/api/desktop/win32/x64/msix/latest/redirect`, which redirects to `downloads.claude.ai/releases/win32/x64/2.37337.1/Claude-ba51ab579cc4029765083ec9f9f0bd23d37a32ee.msix`. The download page's `setup` installer link answers 403 to curl, so the MSIX was fetched instead |
| File | `claude-desktop_2.37337.1_amd64.deb`, 182,483,892 bytes, SHA-256 `41745b0914f0ff51631a1ff1bb403b836bf5c995845cca6742dd39bca3a2da14` | `Claude-2.37337.1-x64.msix`, 296,634,861 bytes, SHA-256 `30dcf402755d734c5a06d7f1061ed80d1e0b33d0fed6cfc048e5164a46f3b704` |
| Checked | **The whole chain.** The key's fingerprint is `31DDDE24DDFAB679F42D7BD2BAA929FF1A7ECACE` ("Anthropic Claude Code Release Signing"), the one Anthropic's page names. `gpgv` against that key alone gives `GOODSIG` on `InRelease`, signed 2026-10-13. The `Packages` index matches the SHA-256 `InRelease` lists for it, and the `.deb` matches the SHA-256 the index lists for it | **The signer only.** `AppxSignature.p7x` holds Anthropic, PBC's EV code-signing certificate under DigiCert's Trusted G4 code-signing CA, and the manifest's publisher is the same. The signature over the block map, and the block map over the files, were not verified |
| Version | `claude-desktop` 2.37337.1. Electron 44.7.0 (`version`), whose Chromium is 152.0.7977.130 (the user agent in the binary) | the package's `Version="2.37337.1.0"`, Electron 44.7.0 (`app/version`) |

The index lists 2.31226.0, 2.31226.1 and 2.37337.1, so earlier versions
stay in the pool and a fetch script can pin one.

### 7.2 What the Linux build is, to Ferrix

* **`claude-desktop`**: a 229 MB position-independent glibc program on
  `/lib64/ld-linux-x86-64.so.2`. It is Chromium plus Node, like Chrome for
  Testing's `chrome`, with SwiftShader, `libvulkan` and `libffmpeg` beside
  it. Unlike Chrome, it links GTK 3 directly.
* **Its libraries against what Ferrix already carries.** A closure check
  read the `NEEDED` entries of every ELF file in the package (39
  libraries) and looked for each on the `--everything` volume's tree on
  nazuna. Four are missing:
  * `libgtk-3.so.0`, for the program itself;
  * `libpipewire-0.3.so.0`, for the `@ant/claude-native` binding;
  * `libcap-ng.so.0` and `libseccomp.so.2`, which only `virtiofsd` needs,
    and so only Cowork.

  Against Chrome's own volume, `libstdc++.so.6` is missing too, for
  node-pty. GTK 3's own dependencies come on top of these and were not
  counted: gdk-pixbuf, epoxy, Xcursor, Xinerama, its schemas and an icon
  theme. That list is an assumption until the fetch script's check runs.
* **`resources/`.**
  * `app.asar`, 47 MB: the main process's JavaScript.
  * `ion-dist/`, 198 MB: the interface, served locally.
  * Native modules: `@ant/claude-native` for Linux (a PipeWire stream and
    reads of the process tree) and node-pty's `pty.node`, for the
    terminals of the Code tab.
  * A static `github-mcp-server`.
  * The helpers: `chrome-native-host` (Claude in Chrome's native
    messaging), `disclaimer` and `claude-browser-shim.js`.
  * Cowork's virtual machine: `cowork-linux-helper`, `virtiofsd` and a
    28 MB disk image, `smol-bin.x64.img`.
* **Claude Code is not in the package.** For the Code tab, the app fetches
  it from `downloads.claude.ai/claude-code-releases`, checking it against
  digests written in the app's own code. That is the linux-x64 native build
  §4 gates.
* **The maintainer scripts** write an AppArmor profile, register
  Anthropic's apt repository and unattended upgrades, register a GNOME
  search provider and the MIME types for `.mcpb`, `.dxt` and `.skill`, and
  link `/usr/bin/ccd` when the build ships one (this one does not). Ferrix
  runs none of them (`docs/APPS.md` §4, rule 4). What matters of them is
  declarative: `com.anthropic.Claude.desktop`, with
  `MimeType=x-scheme-handler/claude;…` and `StartupWMClass=com.anthropic.Claude`,
  and the MIME file.
* **What the main process asks of the platform**, counted in its code.
  * Uses: `safeStorage` 138, `Notification(` 100, `powerMonitor` 79,
    `autoUpdater` 38, `globalShortcut` 16, and `setAsDefaultProtocolClient`
    once.
  * Secret storage: the app reads `safeStorage.getSelectedStorageBackend()`
    and counts `basic_text` as having no secure store. Anthropic's page
    says that with no unlocked keyring "your sign-in isn't saved and you
    sign in again each time you launch the app".
  * Updates: the Linux build does not update itself, by design. Updates
    come through apt.
  * Account: the app takes no API key. It signs in with a claude.ai
    subscription or SSO only.
  * Not in the Linux beta at all: computer use and dictation. The Quick
    Entry hotkey needs X11, or a GlobalShortcuts portal on Wayland.

### 7.3 The ways compared

| | What runs | What it gets | What Ferrix lacks for it | Size | Verdict |
|---|---|---|---|---|---|
| **(a)** The Windows build under Wine 10 or GE-Proton11-7 | Chromium 152's Windows build and Node, from the unpacked MSIX, under Wine on yserver | The app, without Cowork, notifications or a sandbox | Wine installs no MSIX, so the zip is unpacked by hand and the app runs outside its package identity (`getCurrentPackageFamilyName`, the `Claude-Data` move). Its Chromium sandbox has to be off. ANGLE's D3D11 goes through DXVK or SwiftShader. Cowork is `cowork-svc.exe`, a LocalSystem service on Hyper-V's compute service, and there is nothing to run it on. The Code tab runs `conpty.node` on Wine's pseudo-console, with Claude Code's Windows build and a Windows bash under it. Wine on Ferrix still lacks `ptrace` and `/proc/<pid>/mem` (`docs/WINE.md` §10; stage 23's E5.2) | 30–60, a guess with the widest spread here | **No.** Two translation layers to reach what Ferrix runs natively, and the Chromium under Wine is 65 versions newer than the QtWebEngine Wine was proven with (87, `docs/WINE.md` §8) |
| **(b)** The Windows `app.asar` on Linux's Electron 44.7.0 | GitHub's `electron-v44.7.0-linux-x64.zip`, which exists (SHA-256 `3ae7d5bd…555a` in its `SHASUMS256.txt`), with the Windows bundle's JavaScript | What (d) gets, less what the stubs remove | The Windows native modules (`claude-native`, `conpty`, `msal-node-runtime`) replaced by stubs, and the code that branches on the platform patched, again for every release | 15–25, then some at every release (a guess) | **No.** (d) is the same `app.asar`, built for Linux by Anthropic with Linux native modules. (b) would also mean modifying Anthropic's proprietary code (§7.5) |
| **(c)** claude.ai in Chrome | Chrome 154, which runs on the desktop | Chat, projects, artifacts and claude.ai's Claude Code on the web. Not: local Code sessions, local MCP servers and `.mcpb` extensions, Cowork, Quick Entry | Nothing. Chrome reaches claude.ai's sign-in page on Ferrix already (§5.2) | 1, a launcher entry with `--app=https://claude.ai` (a guess) | **The baseline**, and the fallback while (d) is built |
| **(d)** Anthropic's Linux build, natively | `claude-desktop` 2.37337.1 on the volume's glibc, or on ferrousli in its place, as a Wayland client of hyprix (or an X client of yserver) | The app as on Debian, less Cowork (§7.4) | §7.4 | 12–18 to run it with sign-in by hand; 21–33 with a saved sign-in and notifications (§7.6) | **Recommended** |

### 7.4 What Ferrix lacks for (d)

* **The kernel.** Nothing is known to be missing. Chromium 154 runs on
  Ferrix already, and this app is Chromium 152. What Electron adds is Node
  in the main process. Its libuv uses `io_uring` where the kernel offers
  it (assumed to fall back on `ENOSYS`), `inotify` (Ferrix has it) and
  ptys (term uses them). Chrome's first run found four small calls (§8 of
  `docs/CHROME.md`), so expect a few here too. That is the guess in C2.
* **Libraries.** GTK 3 and its closure, and PipeWire's client library
  (§7.2). The binding is loaded at start, so the library must be present
  even with no PipeWire running. That is assumed, not tried.
* **The GPU.** Software rendering, as Chrome draws on the desktop:
  `--ozone-platform=wayland` and `wl_shm`. The app's own switch
  (`disable-gpu`) is set only when its settings turn acceleration off.
  When Chrome's GPU path lands, it serves this app too.
* **Sandboxing.** `--no-sandbox`, as Chrome runs. Anthropic's page names
  the switch, for running as root. Stage 13 has user, pid and network
  namespaces and seccomp filters, but Chromium's own sandbox does not run
  on Ferrix yet (`docs/SECCOMP.md`, S7 and S8). Until it does, a
  compromised renderer of claude.ai's content has the user's rights, as in
  Chrome on Ferrix today. When Chrome's sandbox runs, this app's does.
* **Auto-update.** None is needed: the Linux build has no updater.
  Ferrix's fetch script pins a version and its SHA-256, as
  `fetch-claude-code.sh` does. It could also follow `stable` by checking
  `InRelease` against the pinned key fingerprint, as apt does, so a new
  release needs no Ferrix commit.
* **Secret storage.**
  * What the app needs: Electron's `safeStorage` on Linux wants a Secret
    Service (GNOME Keyring or KWallet) over D-Bus. Ferrix has no D-Bus
    (`docs/WRITE-DOMAINS.md` §3).
  * What happens without one: the backend is `basic_text`, the app runs,
    and the sign-in is not kept (§7.2). The person signs in at every start.
  * What would fix it: a session bus and a Secret Service (C6). That is a
    decision for the customer, since `docs/DEV-ON-FERRIX.md` §9 points
    secrets at an `authd` keyring later.
* **Notifications and the tray.** libnotify needs
  `org.freedesktop.Notifications`, and the tray needs StatusNotifierItem.
  Both go over D-Bus, and hyprix has no notification daemon. Without them
  the app shows no notifications and no tray icon. It is assumed to run
  regardless.
* **Protocol handlers.** `claude://` carries the desktop actions, the
  return from MCP servers' OAuth, and claude.ai's "open in the app" links.
  Electron registers the scheme on Linux through `xdg-settings`, which
  Ferrix lacks. Ferrix's `/bin/xdg-open` (`chrome.rs`, `opener`) hands
  every argument to Chrome. It has to dispatch by scheme: `claude:` to
  `claude-desktop`, the web to Chrome. A second start hands its URL to the
  first through Electron's singleton, a Unix socket in the profile, which
  Ferrix has.
* **The file chooser.** No portal, so Electron falls back to GTK's own
  dialog (assumed).
* **Cowork.** Cowork needs QEMU with `/dev/kvm` and `/dev/vhost-vsock` on
  the machine the app runs on, meaning a hypervisor in Ferrix
  (`docs/DEV-ON-FERRIX.md` §9, 2: months, in the certified item). It is
  not in this plan. Without it the tab says "Cowork requires hardware
  virtualization (KVM)".
* **The account.** The desktop app takes no API key, so no gate can get
  past its sign-in screen without an account. The sign-in is the
  customer's, as in §5.2.

### 7.5 Licensing and terms

* **The licence.** The package's `copyright` says `Files: *`,
  `Copyright: 2026 Anthropic PBC`, `License: Proprietary`. Electron is
  under the MIT licence, and Chromium's licences are in
  `LICENSES.chromium.html`. Nothing in the package grants a right to
  redistribute the app.
* **So Ferrix carries none of it.** It goes in no image, no Ferrix
  repository and no `.fxpkg` Ferrix publishes. A fetch script downloads
  it on the person's own machine from `downloads.claude.ai`, verified as
  in §7.1, the way Claude Code's volume and Steam come. The person agrees
  to Anthropic's terms when they sign in.
* **Not modifying it.** Patching the app, as (b) needs, is assumed not to
  be permitted. That reading was not checked by counsel.
* **Not taking over the host.** Ferrix does not run the `postinst`. That
  script would make the machine follow Anthropic's apt repository.

### 7.6 The plan for (d), sized

| Step | What | Points |
|---|---|---|
| C1 | `fetch-claude-desktop.sh`. It pins the `.deb` by version and SHA-256 and checks it through `InRelease` against the pinned key fingerprint. It adds Debian 13's GTK 3 closure, PipeWire's client library and `libstdc++6`, pinned, and runs the ELF closure check `fetch-chrome.sh` has. The tree is merged into the `--everything` volume, as Claude Code's is | 3 (a guess, from `fetch-claude-code.sh`) |
| C2 | First start on hyprix: `claude-desktop --no-sandbox --ozone-platform=wayland`, through the gateway, to the sign-in screen, plus whatever running it finds (§7.4) | 4–10 (a guess) |
| C3 | `cargo xtask test-claude-desktop`: the window by its app id `com.anthropic.Claude`, and its sign-in screen on the screen, with a negative control | 3 (a guess) |
| C4 | The desktop: `/bin/claude-desktop` with its flags, the launcher entry and icon, `xdg-open` dispatching `claude:` to the app, and the MIME file | 2 (a guess) |
| C5 | After the customer signs in: the Code tab fetches Claude Code and runs it in a pty, and a chat works | 1, and the customer's sign-in (a guess) |
| C6 | A sign-in that is kept. Either a D-Bus session bus and Debian's `gnome-keyring` on the volume, started and unlocked with the session, or a Secret Service on `authd`. The customer decided on 2026-10-14: `authd` | 6–10 (a guess) |
| C7 | Notifications: a daemon on that bus, drawn by hyprix or a client | 3–5 (a guess) |
| C8 | Chromium's sandbox on, once Chrome's runs (`docs/SECCOMP.md` S7, S8) | 1 (a guess) |

C1 to C4 (12–18) give the app on the desktop, with sign-in by hand at
every start. With C5 to C7 the total is 21–33. Every number here is a
first guess, and C2's spread is the one to watch. The GPU comes with
Chrome's GPU path, and the sandbox with Chrome's (C8). Cowork is not
planned. The `.deb` also exists for arm64, for the Pixel 7 later.

**Recommendation.** Start with (c) at once (1 point), so claude.ai is in
the launcher. Then build (d), C1 to C4. The customer ordered C1 to C4 on
2026-10-14 and chose `authd` for C6's Secret Service. Do not build (a) or (b).

### 7.7 What was verified, and what was assumed

**Verified, 2026-10-14, on nazuna, reading files only:**
* Anthropic's Linux build exists, and its documentation was read.
* The apt chain verifies, from the key fingerprint down to the `.deb`.
* The version, Electron and Chromium of both builds.
* The MSIX's signer.
* The Linux build's files, its native modules and helpers.
* Its 39 libraries, and the four the `--everything` volume lacks.
* The maintainer scripts.
* The counts of platform APIs in the main process's code.
* That the app fetches Claude Code at run time.
* That Electron 44.7.0's Linux build is published.
* Ferrix's `xdg-open`, which sends everything to Chrome.

**Assumed, and not tried:**
* Everything about running either build. Neither ran anywhere, and no
  Ferrix boot was made: (a) is not cheap to try, and (d) needs C1 first.
* Wine's behaviour with this Chromium.
* GTK 3's closure.
* That the app runs without D-Bus, a keyring, PipeWire running or a
  notification daemon.
* libuv's `io_uring` fallback.
* The reading of the terms.
* Every size above.

### 7.8 Where C1 to C4 stand, 2026-10-15

**The app runs on Ferrix and shows its sign-in screen.** On the
`--everything` desktop, under KVM, it draws claude.ai's "Claude for
Linux" screen, its mark and its "Get started" button, about four
seconds after hyprix comes up. It runs as Chrome does there: a Wayland
client of hyprix, drawing in software, without Chromium's sandbox, on
ferrousli's loader and `libc.so.6`. No account was used and nothing was
typed. Built on branch `claude-desktop` (the coordinator lands it).

| Step | State |
|---|---|
| C1 | **done**. `tools/common/fetch/fetch-claude-desktop.sh` checks the key's fingerprint, `InRelease`'s signature, `Packages` and the `.deb` against pinned SHA-256s, and adds 21 pinned Debian 13 packages (GTK 3, PipeWire's client library and their closure). Run on nazuna on 2026-10-14; a wrong fingerprint and a wrong `.deb` SHA-256 each stopped it |
| C2 | **done**. One thing was missing. The app's binary carries its search path, `$ORIGIN`, as `DT_RPATH`, not `DT_RUNPATH` (it is linked without `--enable-new-dtags`), and ferrousli's loader read only `DT_RUNPATH`, so the first start stopped at "library not found: libffmpeg.so". ferrousli 113c587 reads `DT_RPATH` when an object has no `DT_RUNPATH`, as glibc does; glibc's inheritance of a program's `DT_RPATH` by the libraries it loads is not followed. Nothing else had to change |
| C3 | **done**. `cargo xtask test-claude-desktop` requires a window of class `com.anthropic.Claude` showing the sign-in screen: half of it or more the screen's background, 1000 or more pixels of Claude's orange mark (the app draws 3783), and 5000 or more of the black sign-in button (14017). Negative controls: the wrapper starting a program that is not there, and the mark made unreachable |
| C4 | **done**. `/bin/claude-desktop` with its flags, the launcher entry `com.anthropic.Claude.desktop` with the app's icon, `mimeapps.list` and the MIME types for `.mcpb`, `.dxt` and `.skill`, and `/bin/xdg-open` handing `claude:` links and those files to the app |

**What the app asks for and does without**, from its log: a D-Bus system
and session bus (the keyring, notifications and power events are C6, C7),
`/proc/sys/fs/inotify/max_user_watches`, a DRM render node (it draws in
software), `libnotify`, and GTK's pixbuf loaders for one symbolic icon.

**The gate is network-dependent.** The sign-in screen is claude.ai's
page, fetched through the QEMU gateway, as `test-steam-store` fetches
Steam's store. Its verdict depends on claude.ai being reachable and on
Anthropic's page still matching `signed_out`'s thresholds, neither of
which Ferrix controls. So it is a landing row only for changes to
`claude_desktop.rs`, `fetch-claude-desktop.sh` and the app's carry on the
`--everything` desktop; when it goes red while the app's window is
listed, treat it first as possibly external. It is not evidence for any
requirement, and nothing in `docs/certification` cites it (the
certification consultant's OK IF, ledger line 1113, 2026-10-15). It uses
no account, and the app starts on a fresh profile under `/dev/shm` each
boot.

**Gates** (`~/.local/share/ferrix/logs/queue/<tag>.log` on nazuna): see
the handover note `~/.local/share/ferrix/handover/claude-desktop.md`.

**Spent:** about 6 points of the 12–18, C2's spread mostly unused: the
loader's `DT_RPATH` was the only gap.
