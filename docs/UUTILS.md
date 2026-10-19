# Replacing busybox with the uutils family and zinc

Version 1. Written 2026-09-17 on the customer's order: *replace busybox with
uutils/coreutils*, and *btop, git and curl should link against ferrousli*.
The three scope decisions in §2 are the product owner's, taken the same day.
§3 is a spike that was run before any of this was written, and it is the
reason the plan below is a plan rather than a proposal.

## 1. What busybox is doing here, and why it goes

busybox is the userland Ferrix is measured with. One static binary lands in
the initramfs at `/bin/busybox`, every applet name is a symlink beside it, and
three gates drive it: `test-shell` runs `sh -c` with a script, `test-vfs` runs
stage 8's exit programs and a sweep of applets, and `test-net` configures
`eth0` and fetches a file. It is also ferrousli's main consumer: the busybox
built against ferrousli is the primary one, and `undefined-symbols.txt` from
its failed link has been ferrousli's to-do list since it was written.

Two reasons it goes. It is C, in a tree whose case is that an operating system
is better written in Rust, and the exception has never been argued for — only
tolerated because nothing else could boot. And it drags the kernel's UAPI
headers in with it: `src/user/system/linux/ferrousli/tools/busybox/` needs the host's `<linux/*>` on
Linux and Alpine's pinned copy on Windows, and ferrousli carries three
pass-through headers for it. Both go when busybox goes.

**This document does not claim uutils is a busybox replacement.** It is not.
uutils/coreutils is a *GNU coreutils* replacement: 105 utilities, no shell, no
networking, no `grep`, `sed`, `awk` or `tar`. What replaces busybox is a set:
the uutils family for the utilities, zinc for the shell, and — for a while —
nothing at all for a list this document names in §5 and does not hide.

## 2. The product owner's decisions, 2026-09-17

| # | Question | Decision |
|---|---|---|
| D1 | uutils has no shell, and the gates all run `sh` | **zinc becomes the shell.** `/bin/sh` is zinc; `test-shell`'s script and every `sh -c` in the gates run it |
| D2 | What happens to the applets uutils/coreutils lacks | **Bring in the uutils family**: findutils, diffutils, procps and util-linux beside coreutils |
| D3 | uutils is Rust, so it links Rust's `std`, not ferrousli | **ferrousli only.** One build, `x86_64-unknown-linux-gnu` with `+crt-static`, `libferrousli.a` in glibc's place. No musl build |

D3 is the consequential one. It costs the AArch64 and ARMv7-A userland that a
musl build would have given for nothing, because ferrousli is built for x86-64
alone; and it puts every one of these programs on ferrousli's correctness,
with no second implementation to compare against when one misbehaves. It buys
the thing that made busybox worth having: a large, real, hostile consumer of
the C library, which is what has driven ferrousli's POSIX work all along.
**On AArch64 and ARMv7-A the image carries zinc and nothing else** until
ferrousli is built for those architectures. That is a new backlog row, not a
thing this plan solves.

## 3. The spike: Rust `std` links against ferrousli, and so does uutils

Run on example, 2026-09-17, before the plan was written, because everything
here rests on an unproven claim: that Rust's `std` for a glibc target can be
linked against ferrousli the way busybox's C is.

**Part one — a `std` hello-world.** Built for `x86_64-unknown-linux-gnu` with
`-C target-feature=+crt-static -C link-self-contained=no` and a linker wrapper
giving ferrousli's `crt1.o` and `libferrousli.a` and nothing from the host's C
library — the shape `src/user/system/linux/ferrousli/tools/busybox/build.sh` already uses. Three
things stopped it:

1. `rust_eh_personality` is defined by ferrousli *and* by `std`. ferrousli is
   itself a Rust staticlib, so it exports its own. The spike made ferrousli's
   copy local with `objcopy --localize-symbol`; the real fix is ferrousli's.
2. `dlsym` is missing. `std`'s weak-symbol machinery calls it. A static
   program has nothing to look up and musl's static `dlsym` returns null.
3. `_dl_find_object` is missing. The host's `libgcc_eh` calls it first and
   falls back to `dl_iterate_phdr`, which ferrousli has, when told "not found".

With those three worked around, the program **linked and ran**: arguments,
environment, current directory, a file written and read back, a spawned
`std::thread` that returned a value, and an exit status of 7 the shell saw.

**Part two — uutils/coreutils 0.9.0 itself**, `--no-default-features
--features feat_os_unix_musl` (the feature set uutils already maintains for
targets that cannot produce the `cdylib` that `stdbuf` needs). The whole tree
compiled — ~340 crates, `nix`, `crossterm`, `jiff`, `onig_sys`'s C — and the
link named **17 undefined symbols and nothing else**:

```
cfsetispeed@GLIBC_2.2.5   posix_spawnattr_init          posix_spawn_file_actions_init
cfsetospeed@GLIBC_2.2.5   posix_spawnattr_setflags      posix_spawnp
gnu_get_libc_version      posix_spawnattr_setpgroup     pthread_atfork
lutimes                   posix_spawnattr_setsigdefault __res_init
__memcpy_chk              posix_spawn_file_actions_adddup2  splice
posix_spawnattr_destroy   posix_spawn_file_actions_destroy
```

Four notes on that list. `posix_spawn` is nine of the seventeen, and
`include/spawn.h` already declares all of it — musl's header is there and the
implementation is not; `src/spawn.rs`'s own comment says so. The two `termios`
functions exist in `src/termios.rs` but the `libc` crate asks for them by
their versioned glibc names, so ferrousli owes a `.symver` alias, not a
function. `__memcpy_chk` comes from `onig_sys`'s fortified C and brings the
rest of the `_chk` family with it. `__res_init` is a no-op in musl.

**This is the result that makes the rest of the document a plan.** The risk
that mattered — that Rust's `std` and ferrousli were simply incompatible —
is retired.

**Part three — S1, and the link with nothing stubbed.** The seventeen went
into ferrousli, and the spike ran again against that library with no shim
archive and no localized symbol. uutils/coreutils 0.9.0 **links with zero
undefined symbols** and the 14 MiB static binary runs on the host. That is
S1's exit, and it is met.

## 3a. The one thing this plan did not expect, and what it changed

`coreutils echo hi` prints uutils' usage and says `<unknown binary name>`. It
is not a ferrousli fault, and the control says so: the same probe built as a
**static glibc** binary, no ferrousli anywhere, behaves identically, while the
ordinary dynamic glibc build works.

The cause is `src/common/validation.rs` in uutils. On Linux, when the target
environment is not musl, it does not trust `argv[0]`; it asks
`rustix::param::linux_execfn()` for the kernel's `AT_EXECFN` and prefers that,
to stop `env -a` bypassing AppArmor and SELinux on hard-linked binaries. In
rustix 1.1.5's `linux_raw` backend that value is read through `prctl`'s
`PR_GET_AUXV` or `/proc/self/auxv`, never through the C library, and in a
static binary it comes back empty. uutils then has no name, and the multicall
dispatch — which is the whole of how `/bin/ls` becomes `ls` — never happens.

ferrousli's `getauxval(AT_EXECFN)` answers correctly; a C program against this
library prints the path. rustix simply does not ask it.

Three ways out were on the table:

1. **Build for the `x86_64-unknown-linux-musl` triple, still linking
   ferrousli.** uutils' `#[cfg(not(target_env = "musl"))]` then falls away and
   it uses `argv[0]`. It also suits ferrousli, whose headers *are* musl's.
2. **Carry a patch to uutils' `validation.rs`.** Smallest change, and a patch
   to maintain for ever against a moving upstream.
3. **Give one binary per utility instead of a multicall one.** No dispatch to
   get right, and roughly a hundred copies of a 14 MiB binary, which the
   initramfs cannot hold.

**It is (1), and it was measured rather than argued.** uutils/coreutils builds
for the musl triple against ferrousli with **zero undefined symbols and no
duplicate ones**, and every part of the dispatch then works: a symlink named
`ls` lists, `sort`, `wc`, `seq`, `head` and `uname` all run as themselves,
`coreutils echo hi` takes the second-argument form, and `env` starts another
program.

That was on a Linux host. Since S3 it is also true **on Ferrix**: `test-vfs`
runs the multicall form, the symlink dispatch and a file read there, and
`uname -sm` prints `Ferrix x86_64`.

Two things had to be got right, and neither was `rust_begin_unwind`, which
does not in fact clash on this triple:

* **The unwinder.** The musl target names `-lunwind` where the gnu target
  named `-lgcc_eh`, and `std`'s panic machinery really calls into it. rustc
  ships the answer — `self-contained/libunwind.a` — for this target on Linux
  **and on Windows**, which is what lets the Windows build work the same way.
  It is copied rather than reached with `-L`, so the linker cannot satisfy
  `-lc` from musl's `libc.a` sitting beside it.
* **The toolchain.** uutils is unpacked and built outside the repository,
  where rustup falls back to the machine's default — older than ferrousli's
  `rust-version`, and with no musl target installed. Both scripts read the
  channel out of `rust-toolchain.toml` and name it.

So D3 stands as the product owner set it: one build, against ferrousli, no
musl C library anywhere. Only the *triple* changed, and with it the reason for
the triple, which is now written down where it is used.

## 3b. What S4 found

**zinc's output is busybox's, to the byte.** Of the eighteen `sh -c` scripts
in `test-vfs`, seventeen passed unchanged when `/bin/sh` became zinc, and
`test-shell`'s transcript did not move at all. The one that changed was the
permissions row, and what changed there were five *utilities'* messages, not
the shell's. `test-vfs` now carries two rows of its own that say which shell
`/bin/sh` is — `$ZSH_VERSION` from inside it and `readlink /bin/sh` from
outside — because nothing else in the file would have noticed if it quietly
went back.

**uutils says what GNU says, and that is a real difference, not only
wording.** Three examples from the one row that moved:

| busybox | uutils |
|---|---|
| `rm: can't remove 'X': Operation not permitted` | `rm: cannot remove 'X': Permission denied` |
| `chmod: X: Operation not permitted` | `chmod: Operation not permitted (os error 1)` |
| `hostname: sethostname: Operation not permitted` | `hostname: failed to set hostname: Permission denied` |

The first is not a rewording: the kernel refuses that removal with `EPERM`,
because `/tmp` is sticky and the file is another user's, and busybox said so.
uutils says "Permission denied", which is `EACCES`. The second and third name
no file at all, and the second reads as a Rust error rather than a C one.
They are recorded as they are rather than tidied behind a `*`, so that the
day they improve, the gate says so.

**uutils asks the kernel for calls busybox never did.** `splice` (275) and
`copy_file_range` (326) were both answered `ENOSYS`, and uutils fell back and
got the right answer — `cat` and `cp` reach for them first. This is §7's
prediction arriving on schedule: `std` is a much larger consumer than busybox
was, and what it needs shows up at run time rather than at link time. Both
are answered since 2026-09-24, through a kernel buffer as `sendfile` is.
GNU grep forced it: writing to `/dev/null` from a pipe it drains its input
with `splice` and falls back only on `EINVAL`, so under Ferrix it failed, and
curl's `configure`, built on Ferrix for stage 20, said there was no grep.

## 4. Where each part of the userland comes from

| Source | Pinned at | Gives |
|---|---|---|
| zinc | in tree | `sh`, and the init the kernel starts |
| uutils/coreutils | 0.9.0 | the 105: `cat ls cp mv rm mkdir chmod chown stat head tail wc od dd ln readlink truncate touch env id printf echo test sleep seq sort uniq cut tr df du mknod mkfifo tty stty uname hostname nproc kill nohup timeout base64 md5sum sha256sum shuf split comm paste expr factor yes tee sync chroot nice pwd true false …` |
| uutils/findutils | 0.9.1 | `find`, `xargs`, `locate`, `updatedb` |
| uutils/diffutils | v0.5.0 | `diff`, `cmp` |
| ~~uutils/procps~~ | — | **Nothing. It does not build here** — §6a |
| ~~uutils/util-linux~~ | — | **Nothing. It does not build here** — §6a |
| ports, against ferrousli | as pinned | `curl`, `btop`, and **`git`**, the new one the customer named |

**That table was wrong when it was written, and §6a says how.** The rows for
`procps` and `util-linux` listed what their *main branches* have; what they
have released is 0.0.1 in both cases, and neither the release nor main builds
for this target. `sysctl`, `ps` and `top` stay busybox's.

## 5. Where `/bin` stands, and what is still busybox's

Measured from the tree on 2026-09-18, not remembered. An image that carries a
program carries four userlands, and `/bin` gives each name to one of them:

| Program | Owns | Names |
|---|---|---|
| zinc | the shell | `sh`, `zsh` |
| coreutils | the utilities | 106, from `cat` and `ls` to `stty` and `numfmt` |
| diffutils | comparison | `diff`, `cmp` |
| findutils | searching | `find`, `xargs`, `locate`, `updatedb` |
| busybox | **everything else in `APPLETS`** | the rest, and it is still most of the list |

The ports ride beside them: `curl`, `btop` and `git`, each built against
ferrousli, each under its own name.

**What is still busybox's, and gated, is this and only this:**

| Gate | Programs that are busybox's |
|---|---|
| `test-vfs` | `sysctl` (three rows), `fdisk`, `top`, `mpstat`, `iostat`, `pwdx`, `su` |
| `test-net` | `ip`, `route`, `netstat`, `nslookup`, `ping`, `ping6`, `udhcpc` |

Fourteen programs. Everything else either moved or was never gated.

**A note on the networking row, because a summary of this document got it
wrong once.** Ferrix's networking is done and gated: the stack in `src/lib/network/net`,
the net ring, netlink, `AF_INET` and `AF_INET6`, raw and packet sockets, DHCP.
`test-net` passes. What is missing is the seven *commands* above, which
configure and inspect it. uutils is GNU coreutils, which has never contained a
networking tool, so there was never a chance of them arriving with the family.

## 5a. What is not gated, and would still be lost

busybox also answers names nothing checks. Deleting it takes these with it,
and that is a decision rather than an oversight:

| Names | Worth |
|---|---|
| `grep`, `sed`, `awk` | the three every real script needs. zinc's own pattern matching covers some of `grep`'s uses and none of `sed`'s or `awk`'s |
| `tar`, `gzip`, `cpio`, `unzip` | nothing in the tree unpacks an archive on the guest yet |
| `mount`, `umount`, `swapon`, `losetup` | stage 11 will want `mount` from a program rather than from the kernel |
| `adduser`, `passwd`, `login`, `getty` | the multi-user slices; `su` is the gated one and is in §5 |
| `vi`, `less`, `more`, `watch` | a person at the prompt. `more` is uutils', the rest are not |
| `dmesg`, `free`, `ps`, `uptime`, `w` | `uptime` is uutils'; procps has the rest and does not build (§6a) |

## 6. The slices

Points, not time, per the customer's rule. Each lands on `main` with the gate
its table row in `docs/BACKLOG.md` names.

| # | Slice | Points | Gate |
|---|---|---|---|
| S1 | **Done.** ferrousli gained the 17: `posix_spawn` and its eight, `pthread_atfork`, `splice`, `lutimes`, `__res_init`, `gnu_get_libc_version`, three `_chk` functions, the two versioned `termios` names, `dlsym` and the rest of `dlfcn.h`, `_dl_find_object`, and a weak `rust_eh_personality`. `execvpe` and `errno::get` came with `posix_spawn`, and the `.init_array` constructors now get `argc`, `argv` and `envp` as glibc and musl pass them | 8 | `check --ferrousli` passes; uutils links with 0 undefined symbols |
| S2 | **Done.** `src/user/system/linux/ferrousli/tools/uutils/` (`sources.sh`, `build.sh`, `build-windows.sh`) and `cargo xtask uutils`. What it shares with busybox moved to `tools/common/xtask/src/ferrousli.rs`, which is what stays when S8 deletes `busybox.rs`. The staleness rule is written and unused until S3 calls it | 5 | builds on Linux and on Windows; the Windows-built binary runs on Linux |
| S3 | **Done.** Every image that carries a program carries `/bin/coreutils`, with each of the 106 utility names linked in `/usr/bin` — not `/bin`, which stays busybox's, so no gate runs a different program than it did. Three `test-vfs` rows, reported as a group of their own, run uutils on Ferrix for the first time | 3 | the three rows pass on Ferrix: the multicall form, the symlink dispatch, and a file read |
| S4 | **Done, and it did split.** Three commits, each green: init resolves a command's program through `/bin` instead of always running busybox; `/bin/sh` is zinc; the hundred names uutils provides are uutils'. busybox keeps what neither has | 13 → **6** | the whole matrix; `test-vfs` gained a group that says which shell `/bin/sh` is |
| S5 | *Folded into S4, and nearly free.* One row of eighteen needed re-recording | — | — |
| S6 | **Done, in half.** findutils and diffutils are built and installed the same way, and six programs ride in the image where one did. procps and util-linux do not build for this target at all, at their release or at their main branch — §6a | 5 → 4 | `test-vfs` runs `find`, `xargs`, `diff` and `cmp` on Ferrix |
| S7 | **Done by another session** (os-12, 2026-09-17): git 2.55.0 over a new `zlib` port and curl's libcurl, without Perl, Python, Tcl, gettext or iconv | 8 → 12 | landed gated by the whole matrix |
| S8 | busybox deleted: `tools/busybox/`, `tools/common/xtask/src/busybox.rs`, the UAPI headers, ferrousli's three `<linux/*>` pass-throughs, the applet list | 3 | the whole matrix, once §5 is empty |

S1 to S7 have landed. uutils and zinc **are** the userland now, not
passengers in the image, and git builds against ferrousli. What is left is
S8, and §8 says what it needs: 28 points of user-space programs over
interfaces the kernel already has, before 3 points of deletion — and a
decision, in §8.3, about whether the deletion is wanted at all.

**Nothing left but S8's 3 points, and those wait on §5.**

S4 was quoted at 13 and cost about 6, for one reason worth writing down: I
said it could not be split, and it split into three. The claim that held
was the narrow one — a flip and its re-recording belong in one commit —
and I read it as the whole of S4. Flipping the shell and flipping the
utilities are two flips, each self-consistent.

**Total, S1–S7: 42 points**, of which S1, S2 and S3 are done. S8 is 3 more, whenever §5 empties.

S3 was quoted at 2 and cost 3: the two `test-vfs` groups became three, which the runner and the judging both had to agree about, per architecture.

## 6a. procps and util-linux do not build here

Four projects were named in D2 and two of them are in the image. The other two
were tried at their release and at their main branch, and neither builds:

| | 0.0.1, the only release | `main` |
|---|---|---|
| procps | its pinned `time` does not compile on this toolchain; unpinning it pulls a `uucore` whose API its own code no longer matches | `uu_top`'s build script wants **libsystemd** through `pkg-config`, and refuses to cross-compile |
| util-linux | the same shape of dependency rot | `blockdev` and `fsfreeze` pass `ioctl` a request of the type glibc declares, and musl declares a different one, so neither compiles for a musl target |

So this is not a choice about how to pin them. Two of the four are blocked
upstream.

What it would take, if the utilities are wanted: for procps, building the
multicall binary with `top` excluded by feature, and pinning a main commit
rather than the release; for util-linux, the same with `blockdev` and
`fsfreeze` excluded. Both then run on unreleased code, and both give up the
utility that made them interesting — `top` for procps, and for util-linux the
two that touch the kernel directly. `sysctl`, `ps` and `top` are the ones
`test-vfs` would have moved off busybox, and they are exactly what is missing.
Perhaps 5 points, and worth doing when either project releases again rather
than now. The backlog has the row.

## 7. Risks

* **`procps` and `util-linux` are `0.0.1`.** `test-vfs` checks what a program
  printed, not only that it exited. Early versions will differ from busybox in
  ways that are not bugs. Each such row is re-recorded, not deleted, and the
  commit says which program's output it now holds.
* **One C library, no control.** D3 leaves every utility resting on ferrousli
  with nothing to compare against. When a utility misbehaves on Ferrix the
  question "ferrousli or the kernel?" loses the answer busybox's musl and
  glibc builds used to give. The mitigation is that uutils runs on the Linux
  host too, against the host's glibc, which is the comparison that remains.
* **`std` is a much larger consumer than busybox was.** 17 symbols is what
  *linking* needs. What ferrousli must get *right* for `std` — threads, TLS,
  `posix_spawn`, signals — is a larger surface than busybox ever touched, and
  the failures will be at run time, not at link time.
* **zinc is on the critical path early.** D1 puts a shell that has no job
  control, no line editor and no completion in front of every gate. The `sh
  -i` and controlling-terminal checks stage 7 owns have no home until zinc has
  job control; they keep busybox's `sh` until it does.

## 8. What S8 needs

S8 is one line of work — *delete busybox* — and it cannot start until
something answers the fourteen names in §5. This is what that means.

### 8.1 The programs, and who could write them

| Group | Names | What it takes | Points |
|---|---|---|---|
| Interfaces and routes | `ip`, `route`, `netstat` | A native Rust program over **netlink, which Ferrix already implements and gates**. `ip` is the only one the image really needs: `route` and `netstat` are `ip route` and `ss` in other clothes, and `test-net` could ask `ip` for what it asks them for | 8 |
| Reachability | `ping`, `ping6` | Raw `AF_INET`/`AF_INET6` sockets, which the kernel has and `test-net` already exercises directly | 3 |
| Address assignment | `udhcpc` | Done (branch `dhcp-client`, 2026-10-19): `dhcpc`, `src/user/system/linux/dhcp`, applies its lease over rtnetlink with no lease script, and every caller runs it. `udhcpc` and `default.script` stay only as `/etc/profile`'s fallback on an image without `dhcpc`, until busybox leaves the image | 5 |
| Name lookup | `nslookup` | A front end to ferrousli's resolver, which is complete | 2 |
| `/proc` readers | `sysctl`, `top`, `mpstat`, `iostat`, `pwdx` | procps' job. It does not build (§6a); writing them here instead is five small programs over `/proc`, which the kernel serves | 5 |
| Partitions | `fdisk` | `test-vfs` only asks it to read `/proc/partitions` and exit 0 without dying. A reader is small; a partition *editor* is not, and is not wanted | 2 |
| Becoming another user | `su` | set-user-id, which the `dac` slices built the kernel side of | 3 |

**28 points before S8 can begin**, and none of it is blocked on the kernel:
every one of these is a user-space program over an interface Ferrix already
has. That is the useful shape of the answer — the hard half is done.

### 8.2 Then the deletion itself

Mechanical, once §8.1 lands. 3 points:

* `src/user/system/linux/ferrousli/tools/busybox/` — `sources.sh`, `build.sh`, `build-windows.sh`,
  `config.sh` and `hostcompat/`;
* `tools/common/xtask/src/busybox.rs`, and the `busybox` command in `main.rs`;
* `--init`, which exists to name a busybox. `test-shell` keeps a way to run
  somebody else's binary, or stage 7's exit criterion loses its point
  (`docs/ROADMAP.md` says why);
* `initramfs::APPLETS`, the union of two busybox builds' applet lists, and
  `PROGRAM_PATH`;
* the kernel UAPI headers busybox's Windows build pins, and **ferrousli's
  three `<linux/*>` pass-through headers**, which exist for busybox alone;
* the gate rows in `docs/BACKLOG.md` that name a busybox, and the three
  busyboxes (ferrousli's, Alpine's musl, the host's glibc) the matrix runs.

### 8.3 What is lost the day it goes

§5a, in one sentence: no `grep`, no `sed`, no `awk`, no `tar`, no `mount`, no
editor. **That is the argument for not scheduling S8 as a deletion at all**,
but as the last step of a plan that replaces those too — or for keeping
busybox in the image indefinitely as a program among others, which costs one
binary of 1.2 MiB beside uutils' 14 MiB and gives up nothing.

The customer decides which. Nothing in the tree forces the choice, and
nothing after S6 depends on it.
