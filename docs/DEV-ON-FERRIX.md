# Developing Ferrix on Ferrix

Version 1, written on 2026-10-11, the night after the customer asked for it
(2026-10-10): *"allow us to develop for Ferrix under Ferrix, so that I can
boot into Ferrix and write the components while I am on Ferrix."* **Status:
a survey made by running it, the small fixes it found, a gate, and a design
for what is not small. Nothing below §5 is built, and §9's decisions are
open.**

It follows `docs/INSTALLER.md`'s shape: what it is, what exists, the design,
the tests, the landings and their points, what is left for the customer to
decide, and where it stands. Every claim in §2 and §3 was run on 2026-10-10
and -11 unless it says *assumed* or *not measured*; the logs are in
`~/ferrix-logs/dev-on-ferrix/` on the build host, named beside each.

## 1. What this is, and what it is not

A developer sits at a Ferrix desktop and, without leaving Ferrix:

1. gets the source: `git clone`, `fetch` and `push` against GitHub's
   `ferrix-os` organisation;
2. edits it in a real editor, with a language server;
3. builds a component with the project's own tooling, `cargo xtask ...`;
4. runs its tests;
5. tries the new component on the running system: replaces and restarts a
   program, a service or a driver live where the system can, and builds a
   new image and boots it where it cannot;
6. commits and pushes.

An AI coding agent, Claude Code, on Ferrix is part of the customer's way of
working, and §4.7 says what state it is in.

It is not stage 20. Stage 20's exit is that every image the test matrix
boots was compiled by Ferrix (`docs/roadmap/stage-20-self-hosting.md`); its
gates prove the compiler and the kernel under it. This is the person's loop
around the same toolchain: the network, the editor, credentials, the tools
`check` runs, and what "try it" means for each kind of component. The two
share the toolchain volume and `test-selfhost`, and §7 says where each ends.

Until Ferrix runs on the customer's PC, everything here is proved in a KVM
guest, `cargo xtask run-compositor --everything`. §8 lists what bare metal
adds.

## 2. The walk: the loop as it is today

`cargo xtask run-compositor --everything --no-gl --accel kvm --smp 4 --ssh
2351` on `main` e1520b0c0, 16 GiB, the host at a load of 35 to 48 the whole
night (seven other sessions and a game). Driven over `ssh` as root and as
`ferrix` (`su ferrix -c`), and through a pty for the editors. The clone is
of GitHub's `main`, 557b939e, read-only; every push went to a throwaway bare
repository `git daemon` served on the host's loopback, which the guest
reaches at `10.0.2.2`.

| Step | Today | Evidence |
|---|---|---|
| **Network**: DHCP, DNS, HTTPS to github.com | works | `walk-1-clone.log` |
| **Clone** `ferrix-os/zinc`, then `ferrix-os/ferrix` (148 MB work tree), over HTTPS, certificates verified | works | the same |
| **Components** cloned by xtask in the guest (`cargo xtask components`): five repositories over HTTPS | works; the first try stopped on one clone that timed out after 300 s (§3.3a, fixed), the second took 39 s for the rest | `boot1` session |
| **Clone and push over `git://`** to the throwaway remote; commit with a name and address set by `git config` | works, 7 s for the clone | `walk-5-commit-push.log` |
| **Push over ssh** | **fails: there is no `ssh`, `ssh-keygen`, `ssh-agent` or `scp`.** `sshdt` is a server | §4.2 |
| **Push over HTTPS with a token** | not measured: the clone proves TLS and libcurl, and nothing proves a credential | §4.2 |
| **Edit** with busybox `vi` through a terminal | works | `vi-capture.bin` |
| **Edit** with helix, the stock 25.07.1 linux-x86_64 release, downloaded with `curl` in the guest and unpacked | works: opened a file, inserted a line, `:w`, `:q`; `hx --health rust` finds its grammars and no language server | `walk-2-editor.log`, `hx-capture.bin` |
| **neovim**, the stock 0.11.4 release | **fails at start**: `ld-ferrousli: undefined symbol: _nl_msg_cat_cntr` | the same |
| **rust-analyzer**, the stock release | **failed**: answered its first request and ended with `SIGABRT` and no message at the second. **Fixed (§3.1)**: with the fix it indexes zinc and answers `workspace/symbol` in 22 s. It still has no standard library source (`rust-src`, §3.3) | `walk-2-editor.log`, `walk-9-unwind-fixed.log` |
| **`cargo build -p xtask`** in the clone | works, 113 s | §6 |
| **`cargo xtask build --arch x86_64`**: loader, kernel, natives, the FAT image | works, 293 s; the image is 140 MB of kernel and 26 MB of initramfs on `/home` | §6 |
| **`cargo xtask build-apps --app ferrofetch`** | works, 21 s; 19 s after an edit | `walk-6-app.log` |
| **Tests**: `cargo test -p ferrix-pkg` | works, 33 s with the build | `walk-4-tests.log` |
| **Tests**: `cargo test` in zinc | **failed: `SIGABRT` at the first failing test**, and every test after it unrun. **Fixed (§3.1).** With the fix it runs on to the two tests of `<(command)`, which **hang** (§3.5) | `walk-4b-zinc-parity.log`, `walk-9-unwind-fixed.log` |
| **`cargo xtask check`** | **stops at its second step**: `error: no such command: fmt`. The volume has no rustfmt and no clippy. **Fixed in the fetch script (§3.3)**; the shared volume has to be fetched again | `walk-8-gates.log` |
| **A gate that boots**, `cargo xtask test-boot` | **fails: `qemu-system-x86_64 is not on PATH`**, after building the image | the same, §4.5 |
| **Replace a program live**: `pkg remove ferrofetch && pkg install <the new .fxpkg>`, run it | works as root; `pkg install` alone refuses a package that is installed (`remove it first`), and `ferrix` may not write `/lib/ferrix/packages` | `walk-6-app.log`, §4.4 |
| **Restart a service**: `svc restart pulsed.service` | works as root, new main pid; refused for uid 1000, as designed | `walk-7-live.log` |
| **Replace a driver live** | **not on this image**: `/bin/drvupdate` and `drvupdated` are carried only by `test-restart --update`'s images, and only the display kind is built (`docs/DEVMGR.md` §4.1) | §4.4 |
| **Replace the compositor** | ends the session: `hyprix.service` is not restarted on a session's image (`docs/AUTH.md` §6.4) | §4.4 |
| **Boot the image just built** | **no way from inside**: the guest's boot image is not a disk Ferrix sees, there is no FAT writer and no second slot | §4.4 |
| **As `ferrix`**: `cargo new`, `cargo test`, `git` | works. `PATH` has `/bin`'s links to `cargo`, `rustc`, `cc` and `gcc`, and not `/data/usr/bin`: no `make`, `python3`, `rg` | `walk-10-as-ferrix.log`, §3.4 |
| **`ssh ferrix@`** the guest | logs in **as root**: `sshdt` gives every key root's shell | §4.2 |
| **Claude Code**: `claude --version` | works, 2.1.280. A turn against a real account: not measured, and no real credential was put in a guest | §4.7 |

So the loop closes today for a user-space program, as root, with `vi` or a
helix fetched by hand, over HTTPS for reading and with no credential for
writing. It does not reach the gates that boot, a driver, the compositor or
the kernel.

## 3. What the walk found, and what was fixed

### 3.1 A Rust panic could not unwind on the desktop (fixed)

The `--everything` desktop runs the toolchain volume's glibc programs on
ferrousli's loader and `libc.so.6` (`docs/CHROME.md`). There, every Rust
program built for `x86_64-unknown-linux-gnu` aborted at its first unwind:

```
$ cat u.rs
fn main() {
    let r = std::panic::catch_unwind(|| { panic!("boom"); });
    println!("caught: {}", r.is_err());
}
$ rustc u.rs && ./u; echo "status $?"
thread 'main' panicked at u.rs:2:43: boom
status 134
```

The same file built for `x86_64-unknown-linux-musl`, static, printed `caught:
true`. So `cargo test` ended with `SIGABRT` at the first failing or
`#[should_panic]` test, and rust-analyzer, which cancels a query by
unwinding, ended at its second request.

The cause was ferrousli's `_dl_find_object`, which answered -1 for every
address on the reading that libgcc's `_Unwind_Find_FDE` then walks
`dl_iterate_phdr` itself. Since GCC 12 it does not: where the C library
declares the function, that is the only question the unwinder asks. Nothing
C++ on Ferrix had met it, because Chrome carries its own unwinder and the
ported programs are static. ferrousli now answers with the object that holds
the address, its bounds and its `PT_GNU_EH_FRAME` segment (component commit
f0c1d55, branch `dev-on-ferrix` in the ferrousli checkout; the 64-bit
structure, measured against glibc's on the host; the 32-bit targets still
answer -1). `test-devloop` (§5) is its proof with libgcc itself, and its
negative control fires.

### 3.2 The gate (built)

`cargo xtask test-devloop`, §5.

### 3.3 The toolchain had no rustfmt, clippy, rust-analyzer or `rust-src` (fixed in the script)

`fetch-rustc-sysroot.sh` now pins and unpacks the four, for amd64 and arm64,
of the release it already takes, and requires them in the tree. A volume
made with it on a private path (`FERRIX_RUSTC_SYSROOT`) is 3241 MiB against
3125. **The shared volume on the build host is not rebuilt by this branch**:
doing so rebuilds `everything.img` under every running desktop, and is the
product owner's to schedule. Until then `test-devloop` says the volume is
from before the script fetched them and walks without its lint step.

### 3.3a A connection through xtask's gateway could stand still for good (fixed)

`test-devloop` found it in its first dozen boots: `git clone` over HTTP
stalled in three of nine, with the host at a load of 30 to 50. In the guest
the connection was `ESTABLISHED` with its 162-byte request unacknowledged
for two minutes, while a ping of the gateway answered in a millisecond.

The gateway is the far end of every guest's network
(`tools/common/xtask/src/gateway/tcp.rs`): it terminates the guest's TCP
connection and opens one of the host's. Its retransmission timer, 20 ms,
winds `snd_nxt` back to `snd_una` so that the bytes are walked again; while
the connection is still shaking hands nothing walks them, so a guest whose
acknowledgment of the SYN-ACK came later than 20 ms found the SYN-ACK's
sequence number looking never sent, its acknowledgment ignored as one of
something never sent, and every later segment of the gateway's numbered one
too low to take. A segment trace of one stalled boot shows it
(`gateway-synack-trace.log`). The handshake now has its own timer, which
sends the SYN-ACK again, and a second SYN gets the SYN-ACK again rather than
a bare acknowledgment. Two host tests hold it and fail without the fix. With
it the gate's clone stalled in none of the eleven boots that reached it
(`fixed*.log`, `run2/*.log`).

This is not the development loop's own defect: every boot with a network
talks through this gateway, so it is very likely the HTTPS clone of §2 that
hung for 300 s, and stalls under load in `test-net`, `test-steamcmd`, Chrome
and Steam are worth reading again (`docs/BACKLOG.md`, P1 flakes).

### 3.4 Programs of the volume that do not start on ferrousli (open)

Run with `--version` on the desktop, 23 of the volume's 380 programs end at
the loader, each on the first name it lacks (`walk-ferrousli-missing.log`):

| Name ferrousli lacks | Programs |
|---|---|
| `_libc_intl_domainname` | `getconf`, `getent`, `iconv`, `locale`, `localedef`, `gencat`, `pldd` |
| `futimesat` | `gzip` and its links |
| `timespec_get` | `cmake`, `cpack`, `ctest` |
| `getprotoent_r` | `perl` |
| `sigsetmask` | `dash`, `sh` |
| `mtrace` | `gawk`, `awk` |
| `sigabbrev_np`, `re_compile_fastmap`, `error_at_line`, `__fread_unlocked_chk` | `ps`, `m4`, `ptx`, `od`/`pinky`/`users`/`who` |
| `_nl_msg_cat_cntr` | neovim's release |

The loader stops at the first missing name, so each program may lack more.
`make`, `gcc`, `ld`, `python3`, `bash`, `rg`, `cargo` and `rustc` start. This
is why stage 20's builds of the C programs run on the volume's glibc and not
on the desktop's libc, and it is D2 below.

### 3.5 Three defects for their owners (open, each with a `docs/BACKLOG.md` row)

* **`cat <(echo hi)` hangs in zinc on Ferrix**, in the image's own zinc too:
  `cat /proc/self/fd/3` on the pipe never sees its end, and spins. zinc's
  two tests of process substitution hang there, and so does oh-my-zsh's
  `source <(...)` if it is reached. A pipe opened again through `/proc`
  is the kernel's.
* **`/bin/sh` is zinc, and zinc is not POSIX there**: `for f in /a/* /none/*`
  ends the script with `no matches found`, as zsh does and `sh` must not. A
  build script written for `sh` meets it; the walk's own scripts run under
  `busybox sh`.
* **`pkg install` cannot replace a package** (D5).
* **Stage 9's wake self-check panicked one boot** of the gate's thirty under
  KVM, at a load of 38 (`gate1-serial-wake-row-panic.log`); a P1 flake row.

### 3.6 What was slow

Nothing on the path is slow for its size (§6). Two waits were long for no
work: one HTTPS clone that hung for git's whole 300 s before it failed, once
in eight clones (§3.3a), and the image build, which is the whole kernel for
any kernel change.

## 4. The design

### 4.1 The editor

The candidates, by what each needs and what running them showed:

| Candidate | What it needs of Ferrix | Evidence |
|---|---|---|
| busybox `vi` | nothing | works; no language server, no syntax |
| **helix** | a terminal with 24-bit colour and the alternate screen (term and foot have both); a language server on `PATH` | **the stock release runs unchanged**, edits and saves; 17 MB download |
| neovim | `_nl_msg_cat_cntr` and whatever follows it in ferrousli (§3.4); a plugin manager and Lua plugins for a language server, each fetched from GitHub | the stock release does not start |
| VS Code | Electron on Ferrix: Chrome runs, but Code is Chromium plus Node with native modules (`node-pty`, the file watcher, `spdlog`, its own ripgrep), an extension host process per window, and the Marketplace over HTTPS | *not tried*. Chrome's own port took weeks (`docs/CHROME.md`) and Code's is on top of it; the Claude desktop app's assessment (`docs/CLAUDE-CODE.md` §7) is the same question |
| rust-analyzer, under any of them | unwinding (§3.1), `inotify` (the kernel has it), `rust-src` (§3.3), 1 to 2 GiB for a tree Ferrix's size | the stock release runs after §3.1's fix; on zinc, first answer in 0.3 s and the workspace indexed in 22 s. On the whole Ferrix tree: *not measured* |

**The design is helix with rust-analyzer, carried on the toolchain volume,
and `vi` always.** helix is the only candidate with a language server
client built in that already runs; it is one static-ish Rust program and a
directory of grammars; and what it needs of the terminal the desktop's
terminals have. It goes on the volume the way Claude Code does, a pinned
release unpacked by the fetch script with `/bin/hx` a link, and can become
an app built from source against ferrousli later without changing what a
person types. neovim comes for free when D2 closes ferrousli's gaps. VS
Code is a decision for the customer (§9, 1): it is weeks, not days, and
nothing else here waits for it.

### 4.2 Credentials

What exists: git with HTTPS through libcurl and Mbed TLS, Mozilla's CA
bundle, and git's built-in `credential-store` and `credential-cache`. No ssh
client. No keyring. `/home` is a btrfs volume of its own, unencrypted, with
each account's home 0700 (`docs/AUTH.md`).

* **First, HTTPS with a fine-grained token.** `git config --global
  credential.helper store` keeps it in `~/.git-credentials`, 0600. It works
  with what is on the image, GitHub scopes such a token to named
  repositories and to contents alone, and revoking it is one click. *Not
  measured*: no credential was put in a guest. The proof is a gate (D3)
  against a stub that asks for Basic authentication and runs the host's
  `git http-backend` behind it, with a throwaway token.
* **Then OpenSSH's client as an app**: `ssh`, `ssh-keygen`, `ssh-agent`,
  `ssh-add`, `scp`, ported onto ferrousli like curl and git, with Mbed TLS
  or its own crypto. Keys in `~/.ssh`, 0700; the agent started by the
  session (a unit of `user-1000.slice`, its socket in `/run/user/1000`), so
  a passphrase is typed once per login. This is also what signs commits
  (`gpg.format = ssh`), which a GPG port would otherwise be needed for.
* **Where secrets live.** In the user's home, readable by that uid and by
  root, on a disk that is not encrypted: what Linux gives without a keyring.
  A secret typed into a guest today survives `--reset-root` and goes with
  `home.img`. Two things would make that better and neither is in this
  design: an encrypted home, and `authd` holding secrets and releasing them
  to a session it has authenticated. The second is the natural owner
  (`docs/AUTH.md`), and §9, 3 asks whether it is wanted before the PC.
* **`sshdt` gives every key root.** `ssh ferrix@guest` is root's shell. It
  is how the gates drive a guest and it listens on the QEMU host's loopback
  alone; on a PC with a network card it must log a key in as its own
  account, through `authd`, before it is started there at all. A row for
  AUTH, and a condition of §8.

### 4.3 The tools `check` runs

`cargo xtask check` is the definition of done (`docs/CONVENTIONS.md`). On
Ferrix it needs, beyond Cargo: rustfmt and clippy (§3.3); `python3` for the
audits, which is on the volume and not on `PATH`; `git` with hooks; `bash`;
and for its component steps the musl targets and a pty. Its Miri and loom
steps need a nightly toolchain the volume does not carry.

* The session's `PATH` gains `/data/rust/bin` and `/data/usr/bin`, after
  `/bin`, so Ferrix's own programs win and the volume's 23 that do not start
  (§3.4) are behind busybox's of the same name. It is set where the
  session's environment is made, not in three programs' constants.
* D4 runs `check --fast` on Ferrix under `test-selfhost`'s volume and fixes
  what it finds, a step at a time. *How far it gets after the second step is
  not measured.*

### 4.4 The inner loop, by kind of component

**A user-space program or app.** Works today: `cargo xtask build-apps --app
<name>`, then `pkg remove` and `pkg install`. D5 (built 2026-10-11) adds
`pkg install --replace` (one transaction: the new files beside the old,
renamed over them, what only the old version had deleted, the record renamed
over the old one last, so running it again finishes a replacement that
stopped) and `cargo xtask install-app --app <name>`, which builds and
replaces. It stays root's: a person in `wheel` runs it through
`su`. 20 s from edit to running, today.

**A service.** Works today: replace the file, `svc restart <unit>`. This is
what `test-devloop` gates. A unit under `/etc/ferrix/units` is read again by
`svc daemon-reload`.

**The compositor and the X server.** yserver is a client of hyprix: stop it
and start the new one, and only X windows go. hyprix is the session: on a
session's image it is not restarted, by design, since a client that killed
it would get an unlocked desktop. Two ways to try a new hyprix without
losing the desk one is working at, in order of cost:

1. *A second seat on the same machine*: `run-compositor`'s own trick turned
   inward is not available, because Ferrix has no hypervisor. But a second
   hyprix on a second card or a second output with its own input devices is
   only a second `sessiond` instance; a developer's PC with two GPUs has the
   hardware. *Assumed workable, not tried.*
2. *hyprix nested*: a backend that draws into a window of the running
   hyprix, as Hyprland's Wayland backend does. hyprix has none
   (`src/user/system/linux/compositor/hyprix` has DRM and the headless path
   the gates use). This is the comfortable answer and it is a feature of its
   own: 8 points, a guess.

Until either, a compositor change is tried from a console login or over
`ssh` from a second machine, with the session given up: `svc stop
hyprix.service`, replace `/bin/hyprix`, `svc start`.

**A ring-3 driver.** The mechanism exists: `devmgr` restarts five kinds and
`drvupdate <driver> <image>` swaps the display driver for a new image with a
rollback after 15 s (`docs/DEVMGR.md` §4.1). It is on no image a person
runs, because D3 of that work, the signature and its review, is owed. For
development the opt-in is the image: a *developer image* carries
`drvupdated`, as a test image does. The design here adds nothing to that
mechanism; it asks for the remaining kinds (net, input, sound, block) and
for `cargo xtask update-driver <name>`, which builds the driver and sends
it. It depends on po10-drv's row.

**The kernel.** It cannot be replaced live: T1, the kexec-style handover, is
not started (`~/.local/share/ferrix/live-update/PLAN.md`), and function
patching was rejected. So a kernel change is tried by booting it. From
within Ferrix there are three ways, and the design takes the first and
measures the second:

1. **A second slot and a trial boot with a way back (A/B).** The installed
   disk gets two system slots, each what the ESP holds today (loader,
   kernel, initramfs, command line), and a one-sector control record on a
   small partition of its own: the good slot, the trial slot, and the tries
   left. `bootctl try <image>` on Ferrix writes the image into the idle slot
   -- a byte copy to a partition, which needs no FAT writer, as the
   installer's ESP is a byte copy today -- and arms one try. The loader
   reads the record through UEFI's block protocol, takes one try off and
   writes it back *before* it loads the trial slot, and boots the good slot
   when no try is left. The system marks the trial good (`bootctl good`)
   once it is up; `test-install` shows that a kernel which panics at boot
   comes back on the good slot by itself. A reboot on a PC is some tens of
   seconds, so the kernel's inner loop is: 5 minutes to build (§6), a minute
   to try, and never a machine that does not come back. The loader is in the
   certified item's load ring, so this starts with the consultant's design
   review. Root is shared by both slots: a kernel that damages the root
   volume is not survived by a slot, and §9, 4 asks about a second root.
2. **QEMU on Ferrix, emulating (TCG).** It is what makes the gates runnable
   from within (§4.5) and the only way to try a kernel without leaving the
   desk. Ferrix has no hypervisor and no KVM of its own, so the guest runs
   at emulation speed: on this host `test-devloop`'s boot and walk took 28 s
   of guest time under KVM; under TCG it was not measured. QEMU's
   `qemu-system-x86_64` is a glibc program with some forty libraries, POSIX
   timers (which Ferrix's kernel answers `ENOSYS` to today,
   `docs/CLAUDE-CODE.md` §5), `memfd`, signals on its vCPU threads and
   writable-then-executable mappings for its translator. *Whether it runs is
   not known.* D9 is a two-point measurement before anything is built.
3. **A second machine.** The DK1 board and the Pixel 7 are driven from the
   Linux host today (`flash`, `deploy`, `selfhost.sh`). From Ferrix they
   need USB host for the serial adapter and `adb`, which is the xHCI work
   under way. Later, and free once USB serial exists.

A hypervisor in Ferrix (VMX in the kernel) would make 2 fast. It is a
kernel feature inside the certified item and is not proposed here.

### 4.5 Running the gates from within Ferrix

| Gates | On Ferrix today | What it takes |
|---|---|---|
| `check`'s host steps: fmt, clippy, the host tests, the audits | stop at step 2 | §3.3's volume, §4.3's `PATH`, D4 |
| `build`, `build-apps`, `pkg` | work | -- |
| `test-selfhost`'s build half | works (it is `cargo xtask build` on Ferrix) | -- |
| every gate that boots a guest: `test-boot`, `test-shell`, `test-init`, ..., `test-devloop` itself | fail: no QEMU | D9: QEMU on Ferrix, TCG only. The KVM rows of *What a landing runs* cannot run on Ferrix at all |
| the gates that need the host's own tools: `btrfs check`, `mkfs.btrfs`, `dpkg-deb`, OVMF, `swtpm` | fail | each tool on the volume, or Ferrix's own (`ferrix-btrfs` has a checker) |
| Miri, loom | fail: no nightly | a nightly on the volume, 1 GiB more |

So a landing made on Ferrix is gated on the fleet's pool, as a landing made
anywhere is: push the branch, `batch.sh join`. What Ferrix must run itself
is the fast half, `check --fast` and the component's own tests, and that is
D4. Running the boot matrix on Ferrix is not a goal of this design.

### 4.6 The developer image

`--everything` is the developer's desktop today: the toolchain volume merged
with Chrome's, Steam's and Claude Code's, attached by QEMU as `/data`, with
`/home` on a disk of its own. On a PC there is no QEMU to attach it. The
installer copies a 1 GiB root and nothing else, and a root the size of its
partition is I3, not built (`docs/INSTALLER.md` §11). So the toolchain
(3.2 GiB), a clone with its components (0.5 GiB) and target directories
(2 GiB for the image alone, 10 for a week's work) have nowhere to live on an
installed machine. The design: the installer's summary gains *developer
tools*, which writes the volume to a `ferrix-data` partition the kernel
mounts at `/data` as it does today, sized by the person, with `/home` beside
it; nothing in the kernel changes, since both labels are already looked for
on any disk or partition.

### 4.7 Claude Code

Works today (`docs/CLAUDE-CODE.md`, `cargo xtask test-claude-code
--everything`): Anthropic's linux-x64 release 2.1.280 from the volume, on
ferrousli as the desktop runs it; `claude --version`; one `-p` turn against
a Messages API xtask serves, in which the model asks for a Bash command and
Claude Code runs it in bash on Ferrix; and the TUI on the console, typed at.
`claude` is in the launcher and in every terminal. Checked again on
2026-10-10: `claude --version` on the desktop says 2.1.280.

Missing, in the order a session meets it:

* **A real account**, never measured. `claude` logs in through a browser,
  which the desktop has (Chrome), or takes `ANTHROPIC_API_KEY`. The token
  lands in `~/.claude`, on `/home`, which is kept. The customer's step, with
  the customer's account (D10).
* **Its updater is off** and the volume is started afresh each boot: a new
  release is a new pin and a fetch.
* **What its tools run.** Its Bash tool runs the volume's bash, with
  ripgrep; `git` and `cargo` are on `PATH`. After §3.1 a `cargo test` it
  runs reports a failing test instead of dying. It will meet §3.4's
  programs and §3.5's `<(...)` as a person does.
* **Resizes of its terminal**, which the console gate cannot make, and
  linux-arm64 for the Pixel 7's VM.
* **Sub-agents and the fleet's tooling** (`gate.sh`, `batch.sh`, `land.sh`)
  are bash and ssh to the gate host: they need §4.2's ssh client.

## 5. The gate: `cargo xtask test-devloop`

Built (`tools/common/xtask/src/devloop.rs`). It boots Ferrix with init as
pid 1, the toolchain volume at `/data`, git from the apps, and the network
through xtask's gateway to the HTTP stub `test-net` uses, which serves a
fixture repository: a library, a program and three tests, no dependency.
One service, `devloop.service`, then does what §1 lists and says a line a
step:

```
devloop: cloned greeter: say version one          git clone over HTTP
devloop: tests passed (one)                       cargo build, cargo test
devloop: fmt and clippy clean                     where the volume has them (§3.3)
devloop: before greeter: hello from version one   installed, `svc start`, asked over TCP
devloop: tests passed (two)                       sed, cargo build, cargo test
devloop: pushed greeter: say version two          git commit, git push to a bare repository
devloop: after greeter: hello from version two    the new file moved over the old, `svc restart`
devloop: the service's main pid changed
devloop: done
```

Where the volume has helix (D1), two lines follow the lint step, `devloop:
hx finds rust-analyzer` and `devloop: rust-analyzer answered the editor`:
`hx --health rust` must name the server, and a language-server session
(`tools/common/xtask/src/devloop/lsp.py`, on the volume's python3) must have
rust-analyzer initialize, list the fixture file's symbols and find
`greeting` in the workspace, as an editor asks first.

It runs the toolchain on ferrousli's loader and `libc.so.6`, as the desktop
does, unless `--interpreter glibc`. One of the fixture's tests must panic
and one catches a panic, so the gate is §3.1's proof. 24 to 28 s of guest
time under KVM, four runs; the last, on the branch rebased onto main
a6d863da3 with a volume that has the lint tools, passed with `devloop: fmt
and clippy clean` (`run2/d0-1.log`).

**Negative controls**, both run on 2026-10-11 and again on the rebased
branch (`run2/ctl-unwind.log`, `run2/ctl-norestart.log`), both fired each
time:

* ferrousli's `_dl_find_object` answering -1 again, which is ferrousli at
  its current pin, 93063b1: `devloop: FAILED at the first tests`, after `signal: 6, SIGABRT` from the fixture's test program
  (`control-unwind.log`).
* the script's `svc restart greeter.service` replaced by `true`: `the walk
  never said devloop: after greeter: hello from version two ... it said
  devloop: after greeter: hello from version one`, and the pid line says the
  pid did not change (`control-no-restart.log`).

What it does not prove: a push over the network (the stub serves git's dumb
protocol, which takes none; D3), the editor (D1), `cargo xtask` on the real
tree (`test-selfhost`), and a driver or kernel (D6, D8).

## 6. Build times

One machine shape for both columns: four processors and the tree at GitHub's
557b939e, debug profile, a cold target directory. Ferrix is the KVM guest of
§2 (16 GiB, source and target on btrfs at `/home`, the toolchain on ferrousli);
Linux is the host itself held to four of its processors and four jobs
(`taskset`, `CARGO_BUILD_JOBS=4`), not a guest, with the host's own 1.97.1.
**The loads differ**: the Ferrix column was taken on 2026-10-10 at a host
load of 35 to 48, the Linux column on 2026-10-11 at 1.6 to 4
(`~/ferrix-logs/dev-on-ferrix/run2/linux-compare.log`). The guest's four
processors shared the host with all of that and Linux's did not, so the
ratios below are an upper bound on Ferrix's cost, not a measure of it; the
same Ferrix builds on a quiet host are owed before any figure leaves the
tree.

| Build | Ferrix | Linux | Ratio |
|---|---|---|---|
| `cargo build -p xtask` | 113 s | 17 s | 6.6, at unequal load |
| `cargo xtask build --arch x86_64` (xtask built) | 293 s | 46 s | 6.4, at unequal load |
| `cargo xtask build-apps --app ferrofetch`, cold / after an edit | 21 s / 19 s | not measured | |
| `cargo test -p ferrix-pkg`, with its build | 33 s | not measured | |
| `test-devloop`: boot, two builds, two test runs, restart | 28 s (KVM) | -- | |

For scale: stage 20 measured the same image build at about 90 s of guest
time on an idle host (`docs/roadmap/stage-20-self-hosting.md`), and the
opaque-kernel work measured a ring-3 round trip at 10 to 20 times Linux's.
That idle-host figure against Linux's 46 s is about two, which is the
better guess at the real gap until a quiet run is made. Where the rest of
it goes -- rustc itself, or the file system and process creation around it
-- is *not measured*.

## 7. Tests

* `test-devloop` (§5), with its two controls, in the image row's `--gate`
  file for any landing that touches the toolchain's path: ferrousli's
  loader, init's service start and stop, the gateway, git.
* `test-selfhost` stays stage 20's: the whole image, built by Ferrix,
  boots.
* Each landing below names what proves it. A landing that adds a tool to
  the volume adds its step to `test-devloop`'s walk, so the walk is always
  the loop a person has.

## 8. What bare metal adds

None of it is in this design's landings; each has an owner or needs one.

1. NVMe and xHCI drivers (being written), and the installer on real PCs
   (`docs/INSTALLER.md` H0 to H7).
2. **A network driver for the PC's own card.** Ferrix has `virtio-net` and
   nothing else. Without one there is no clone and no push; a USB Ethernet
   adapter on xHCI is the same gap.
3. A root the size of its partition (I3) and §4.6's data partition.
4. `sshdt` logging a key in as its account (§4.2), before it listens on a
   real network.
5. §4.4's second slot, so that a bad kernel is a reboot and not a rescue
   stick.
6. The time: a real-time clock driver or `ntpd` at boot, since TLS and
   `make` both read the date. The guest has firmware's time.
7. The GPU: NVIDIA's stack on the 3060 is its own stream (`docs/NVIDIA.md`);
   until it lands the PC's desktop draws through the firmware's framebuffer
   or not at all. *Assumed*; that stream says.

## 9. Decisions for the customer

1. **The editor.** *Recommended: helix with rust-analyzer now (D1), VS Code
   assessed later with the Claude desktop app, since both are the same
   Electron question.* The alternative is to start the VS Code port now: the
   honest size is weeks, and D1 is two days.
2. **Trying a kernel from inside.** *Recommended: the second slot with a way
   back (D8), and D9's two-point measurement of QEMU on Ferrix before
   deciding whether the boot gates should ever run there.* The alternative,
   a hypervisor in Ferrix, is a kernel feature in the certified item and
   months.
3. **Where secrets live before the PC.** *Recommended: tokens and keys in
   the home directory, 0600, as on Linux, and a fine-grained token scoped to
   the `ferrix-os` repositories; an `authd` keyring and an encrypted home as
   their own rows afterwards.* The alternative is to hold D3 until the
   keyring exists.
4. **A second root for the second slot.** *Recommended: no; one root, and
   btrfs's snapshot before a trial boot once snapshots are wanted for
   anything else.* It costs a gigabyte and an installer change to say yes.
5. **Live driver updates on the developer image** before the signature
   decision of `docs/DEVMGR.md` §4.1 is made. *Recommended: yes, on images
   built with a developer flag alone, which is the opt-in that section
   already describes for test images.*
6. **Refetching the shared toolchain volume** on the build host, which
   rebuilds `everything.img` under any desktop that is running. *The product
   owner's to schedule; it turns on §5's lint step and D1.*

## 10. The landings

Points are firm where the work was done or measured this night, and guesses
where marked.

| | Landing | Points | Proved by |
|---|---|---|---|
| D0 | **This branch**: ferrousli's `_dl_find_object`; `test-devloop`; rustfmt, clippy, rust-analyzer and `rust-src` in the fetch script; this document | 5, spent | `test-devloop` and its two controls; ferrousli's `cxxrt/link`; the fetch script run on a private path |
| D1 | helix on the volume (`/bin/hx`), rust-analyzer found by it, the session's `PATH` (§4.3). **Built 2026-10-11 but for the `PATH`**: helix 25.07.1's release under `helix/` on the volume, `/bin/hx` and `/bin/rust-analyzer` among the toolchain's links; `test-devloop` on a private volume says `hx finds rust-analyzer` and the session's three answers (`run2/d1-1.log`), and its control, a server that is not one, fires (`run2/d1-ctl-noserver.log`). Whether the `--everything` desktop's `/bin` gets the two links is not checked: they are the default system's | 3 | `test-devloop` gains: `hx --health rust` names the language server, and a scripted LSP session (initialize, `documentSymbol`, `workspace/symbol`) answers on the fixture |
| D2 | ferrousli's gaps in the volume's programs (§3.4), found by an audit of every ELF's imports against ferrousli's exports rather than by starting them | 5, a guess | the audit clean in `check --ferrousli`; `dash`, `gawk`, `perl`, `cmake`, `gzip` each run in `test-devloop`'s walk |
| D3 | Credentials: a push over HTTPS with a token through `credential-store`; then OpenSSH's client as an app, with an agent unit | 3 + 8, the second a guess | `test-devloop` pushes to a stub that asks for Basic authentication in front of `git http-backend`, and a control with the wrong token; `test-ssh` gains a client leg |
| D4 | `cargo xtask check --fast` on Ferrix, a step at a time | 5, a guess | `test-selfhost --check` runs it on the volume; the steps it cannot run are named, not skipped |
| D5 | `pkg install --replace`, `cargo xtask install-app`. **Built 2026-10-11**: `test-pkg` replaces a package with its next version and back, and its control (the old version's own files left behind) fires; `pkg`'s host tests; the walk's step for an app is not added | 3 | `test-pkg`; the walk's step 5 for an app |
| D6 | Live driver updates on a developer image, the remaining kinds, `cargo xtask update-driver` | with po10-drv's row (26 there) | `test-restart --update` per kind |
| D7 | hyprix nested in a window | 8, a guess | `test-compositor` boots one hyprix inside another and a client draws in it |
| D8 | The second slot: the control record, `bootctl`, the loader's trial boot, the installer's layout. Moved on 2026-10-11 to two streams of its own, `ab-boot` (the design, its review, the loader) and `ab-sys` (the record, `bootctl`, the installer, `test-install`), designed against the customer's SN720 as partitioned that day | 13, a guess; a design review first | `test-install`: a kernel that panics at boot comes back on the good slot; a good one is marked |
| D9 | QEMU on Ferrix under TCG: does it run, and how slowly | 2 for the measurement | `test-boot` run inside `test-selfhost`'s guest |
| D10 | Claude Code on a real account, on the customer's desktop | 2, the customer's step | a session that edits a file and runs `cargo test` |
| D11 | The developer image on an installed disk (§4.6) | 5, after I3 | `test-install` with developer tools: `cargo -V` from the installed disk |

The order for "the customer boots Ferrix and works on Ferrix from inside
it":

* **In a VM**: D0, D1, D3's first half, D5. Then D2 and D4 as they are met.
* **On the PC**: §8's 1 to 4 and 6, then D11, then D8.

## 11. Where it stands

2026-10-11: this document; D0 built on branch `dev-on-ferrix` and gated
there, with the ferrousli change on the branch of the same name in its
component checkout, commit f0c1d55, unpushed. On the branch rebased onto
main a6d863da3: `test-devloop` passed with the lint step on a private
volume fetched by this branch's script, both controls fired, and the
gateway's two host tests fail without its fix. The certification
consultant found nothing of the item in it (ledger line 722). Nothing is
landed. The component lands first (push ferrousli's `main`, `cargo xtask
pin-components`), since `test-devloop` fails on the pinned ferrousli, as
its control shows. D8 moved to the `ab-boot` and `ab-sys` streams the same
day.
