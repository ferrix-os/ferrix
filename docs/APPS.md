# Apps: optional programs, each in a folder of its own

Written on 2026-09-30 at the customer's asking: *a folder where all
standalone apps go, which xtask adds by itself, so that an app, when added,
only ever changes things in its own folder*. The same day the customer asked
that the design leave room for a package manager later, and agreed to the
answers §9 records.

## 1. The problem

Before this, a program outside the system's own touched up to nine places
beyond its folder: the root `Cargo.toml`, `native.rs`'s `PROGRAMS`, an xtask
module of its own (`statd.rs`, `zinc.rs`), `check.rs`'s `userland`, `ports.rs`'s
`PORTS` and `FILES`, three audits' `ROOTS`, `check-crate-layering.sh`,
`ci.yml`'s target caches and `LAYOUT.md`. Moving or deleting one meant
finding all of them. A fetch program was the first to ask for this, and it
needed none of the kernel.

## 2. The layout

```
src/user/
  system/            first party: what Ferrix needs to be useful
    native/          the runtime, devmgr, the drivers
    linux/           init, auth, zinc, ferrousli, the compositor, adbd, the installer
  apps/              optional, each self-contained, found by xtask
    <name>/
      app.toml       the one contract with xtask (§3)
      Cargo.toml     a workspace of its own, with its own Cargo.lock
      README.md
      src/
```

`src/user/system/` is phase 3's move (§8): until 2026-09-30 its two halves
were `src/user/native/` and `src/user/linux/`, which older documents and
commits name.

The line between the two is whether Ferrix is still useful without it. The
runtime, devmgr, the drivers, init, auth, the shell, the C library and the
compositor are the system. A fetch program, Bad Apple!!'s player, the stat
service and the ported programs (curl, btop, git, foot, vkgears, ALSA's
utilities) are apps. The native test programs `pong` and `channel-echo` are
neither: they are tests, and belong in `src/tests/`.

## 3. The contract: `app.toml`

```toml
[package]
name = "example"                # the folder's name
version = "0.1.0"
description = "One line: what it is."
license = "MIT"                 # an SPDX expression: "MIT OR Apache-2.0"
abi = "native"                  # native | linux
arches = ["x86_64", "aarch64", "armv7a"]
depends = []                    # "name", or "name >= 1.2"

[[package.files]]
from = "example"                # a cargo binary, or a path in a script's output
to = "bin/example"              # where it goes, from the root
mode = "755"

[[package.files]]
from = "share/example.desktop"  # with `source = "folder"`: kept in the folder
source = "folder"               # build (the default) | folder
to = "usr/share/applications/example.desktop"
mode = "644"

[[package.files]]
from = "usr/share/example"      # a directory, taken whole, links and all
tree = true                     # its files 755 when programs, 644 if not
to = "usr/share/example"

[build]
kind = "cargo"                  # cargo | script

[image]
default = true                  # in every image a person runs (§5)

[check]
host-tests = true               # the crate's lib target, tested on the host

[[smoke]]
run = "example --version"       # a command line, run in the guest
expect = "example 0.1.0"        # a line of its output starts with this
```

`[package]` is what travels: it goes into the built package and into the
record on the installed system (§6). `license` is required: an SPDX expression
for every file the package installs. For a ported program that is the
program's own licence and that of what is linked into its static binary or
shipped beside it, read from each upstream source's `COPYING` or `LICENSE`
(curl's is `curl AND (Apache-2.0 OR GPL-2.0-or-later) AND MPL-2.0`: Mbed TLS
and Mozilla's CA bundle with it). `[build]`, `[image]`, `[check]` and
`[[smoke]]` stay in the tree; they are how the package is made and judged.

The manifest is a small subset of TOML -- tables, arrays of tables, and
string, boolean and array-of-string values -- read by `ferrix-pkg`'s own
reader, because xtask takes nothing from crates.io and the package manager
on Ferrix will read the same records.

### 3.1 Building

* `kind = "cargo"`, `abi = "native"`: `cargo build --release --target
  <the kernel's target>` in the folder. The root's `.cargo/config.toml`
  applies, as it does to the system's native programs, and the ELF is held
  to the same shape (`native.rs`'s check) before it goes anywhere.
* `kind = "cargo"`, `abi = "linux"`: a static program against the target's
  musl, as zinc is built. xtask gives the flags and names `rust-lld` as the
  linker, so the app needs no `.cargo/config.toml`.
* `kind = "script"`: `bash build.sh <arch> <out>`, which installs into
  `<out>` the paths `from` names. For the ported programs, which source
  ferrousli's port toolkit (`src/user/system/linux/ferrousli/tools/ports/common.sh`)
  as a native app links the runtime; it needs a Linux host's tools, which
  on Windows are WSL's default distribution's. `check` runs `bash -n` over
  it. A port's script installs into the prefix the ports share, where the
  next one finds its library (git links the curl app's libcurl), and copies
  its app's files out of it into `<out>`; it holds a lock on its work
  directory while it builds, since every checkout's build shares it. The
  libraries that are no program -- libcxx, zlib -- stay ports, and an app
  that links one builds it first when it is not there.

Each app builds into `target/apps/<name>/`, never the system's target
directory. An app whose toolchain is missing, or that does not list an
architecture, is skipped with a line saying so, as a port is today.

A script build is a download and minutes of C, which no image starts on its
own, as no image starts a port: `run` and `run-compositor` take the package
built last, and say how to build it when there is none. Under
`--everything`, which leaves nothing out, one with no package is built, and
a build that fails stops the run. `cargo xtask
build-apps` builds every app's package, or `--app`'s, in dependency
order; `test-apps` builds a script app's package only when there is none,
since vkgears' is Mesa. A cargo build is incremental, and every image makes
it.

### 3.2 What an app may depend on

A native app links the runtime, `src/user/system/native/rt` -- the SDK -- by a
relative path. That path is the one reference an app makes outside its
folder, and apps never refer to each other's folders. A Linux app needs
nothing of the tree.

An app never has to grow the SDK to make a system call.
`ferrix_rt::linux::call` makes any Linux call by number, and
`ferrix_rt::linux::numbers` is the running architecture's table of them, from
`ferrix-linux-abi`. The app spells the calls it makes in its own folder.

## 4. The rules

1. **An app changes only its folder.** Adding one is adding a folder;
   deleting the folder removes it. `cargo xtask check` fails when a file
   outside the app's folder names that path -- in this tree, by its path
   here, and in the apps repository, by that path or a sibling's
   `../<name>/`.
2. **No `asm!` in an app.** The assembly allow-list and the unsafe and panic
   audits are the system's, and they never learn an app's name. An app's
   `unsafe` is its system calls, each with a `SAFETY:` comment, which the
   app lints below require.
3. **The lints are xtask's.** An app's manifest carries no lint table: xtask
   passes the same set to every app's clippy, so a new app is held to the
   rules without copying a hundred lines.
4. **Declarative installs.** An app's files are exactly its
   `[[package.files]]`, and nothing runs when it is installed. What needs
   registering -- a unit, a font -- is a file in a directory the system reads.
   That is what makes removing an app, later, a list of deletions.

## 5. What xtask does, by discovery

`tools/common/xtask/src/apps.rs` reads `src/user/apps/*/app.toml`, and nothing
in xtask names an app's folder. Since 2026-10-03 `src/user/apps` is the
ferrix-os/apps repository, checked out there at the commit `components.toml`
pins (the customer's decision): every program a person starts, Ferrix's own
and those ported onto ferrousli, which build with ferrousli's `tools/ports/`
from where they sit.
A system test that boots a program an app
is asks for the app by name (`apps::taken`): test-net's curl and git,
test-audio's ALSA, test-foot's foot, test-vkgears' vkgears, test-init's
sshdt, test-badapple's player.

| Command | For each app |
|---|---|
| `cargo xtask apps` | lists it, and checks its manifest |
| `cargo xtask check` | formatting (`bash -n` for a script); clippy on the host's lib target and the programs' targets; the host tests; rule 1 |
| `cargo xtask run`, `run-compositor` | builds the `default` ones for the architecture and installs their packages into the image; under `--everything`, every app |
| `cargo xtask build`, `test-boot` | installs only the ones `--app` names; an image with a program as init, and the tests' images, also carry the script apps already built (`apps::ported`), as they carried the ports |
| `cargo xtask build-apps` | builds its package, a script's too, in dependency order |
| `cargo xtask test-apps` | one boot that runs every `[[smoke]]` line and wants each `expect` |
| `cargo xtask new-app --app NAME [--abi linux]` | writes a new app's folder, which passes the rows above as it is |

`--app NAME` adds an app that is not `default`; `--no-apps` leaves them all
out. The images the test rows boot are unchanged: they are the system's
tests, and an app's is `test-apps`. `--statd` is `--app statd`, kept because
the phone's scripts say it.

## 6. Packages

Building an app makes a package, and an image is packages installed into a
root. This is the package manager's engine from the start (§7), so that
every image build exercises it.

A package is a newc cpio archive, `<name>-<version>-<arch>.fxpkg`, holding:

* each file at its `to` path, with its mode, and a tree's files, symbolic
  links and directories beneath its `to`, in name order;
* its record, `lib/ferrix/packages/<name>.toml`: the manifest's
  `[package]`, and a `[[files]]` entry for each file with its path, mode,
  size and BLAKE2b-256 digest (`ferrix-argon2`'s); a symbolic link's entry
  has its target as `link`, and its size and digest are the target's.

Installing is unpacking, so the record lands with the files, and a running
Ferrix knows what it was built with. `ferrix-pkg` (`src/lib/proto/pkg`)
holds the manifest reader, the record and the installer's plan -- the
dependencies met, no two packages owning a path, no path outside the root --
where `cargo test` reaches it. xtask builds with it on the host; the package
manager will be the same code on Ferrix.

## 7. The package manager

`/bin/pkg`, `src/user/system/linux/pkg`, first party and not an app: it
manages the apps. It runs on any root (`--root`), so its host tests run it
in a directory:

```
pkg list               the packages installed: their records
pkg info NAME          one package: what it is, and each file
pkg install FILE...    install packages (.fxpkg), together
pkg remove NAME        remove a package
```

An install reads each archive with the kernel's own cpio reader and refuses
it, before one file is written, when it is for another architecture, when an
entry is not what its record says or a file its record lists is missing,
when it is installed already, when what it depends on is neither installed
nor coming with it (`ferrix_pkg::plan`), or when it would put down a path
that is there -- another package's or the system's. The record is written
last and removed first, so a package half put down is not installed. A
removal is refused while an installed package depends on it; it deletes what
the record lists and the directories it leaves empty. `cargo xtask test-pkg`
boots it on every architecture: it installs, runs and removes the stat
service and refuses a changed file, a missing dependency and a needed
removal.

What this leaves room for, and deliberately does not build yet:

* **Upgrades.** `pkg install` of a package already installed is refused;
  remove it first. An upgrade is a removal and an install in one plan.
* **A repository.** An index of packages per architecture, built by CI and
  served statically (GitHub's releases or Pages), fetched with the curl
  app.
* **Signatures.** The index and every package signed; this needs an
  Ed25519 the tree does not have yet. Digests come first, and are in the
  records from phase 1.
* **Resolution.** Minimum versions and no solver, as apk has, until
  something needs more.
* **Atomic upgrades.** Ferrix writes btrfs; a snapshot before a transaction
  is the natural rollback.
* **Building on Ferrix.** Stage 20 wants the recipes run in the guest.
  `cargo` recipes can be; `script` recipes need gcc on the host until the
  ports build natively.
* **The system as packages.** Only once a broken install of init or the
  shell can be recovered from.

## 8. The phases

1. **The mechanism**: `apps.rs`, `ferrix-pkg`, the SDK's `linux::call`,
   the `check` steps, image installs, `test-apps`, rule 1, and the fetch
   program `ferrofetch` as the first app. Landed.
2. **The optional programs move in**: the stat service and btop first, one
   of each build that had no app, with `build-apps` and `new-app`; then Bad
   Apple!!'s player and the other ports -- which empties `ports.rs`'s
   `PORTS` and `FILES` of programs, and gives the ports' dependencies (git
   on zlib and curl) the `depends` they have implicitly today. A library
   only built against, as libcxx is for btop, stays a port: it installs
   nothing an image carries.
3. **`src/user/native` and `src/user/linux` move under `src/user/system/`**,
   a mechanical move of paths in xtask, the generators, CI and
   `LAYOUT.md`, done apart from the rest so it collides with as little
   other work as it can. Landed.
4. **The package manager** (§7): `pkg list`, `info`, `install` of local
   packages and `remove`, and `test-pkg`. Landed; a repository, signatures
   and upgrades are later.

## 9. The customer's decisions

Agreed on 2026-09-30, as the draft proposed them:

1. The names `src/user/system/` and `src/user/apps/`.
2. The split of §2; adbd and the installer stay in the system.
3. One generic `ferrix_rt::linux::call` is the one change to the system an
   app needs.
4. The test rows' images do not carry apps; `test-apps` does.

## 10. Where it stands

2026-09-30: phase 1. `ferrix-pkg`, `apps.rs`, `ferrix_rt::linux::call`,
and `ferrofetch` as the first app. On x86-64 `test-apps` boots zinc as init
and passes the app's three smoke checks, the first a native program started
from a shell by `execve` and reading `uname` and `/proc`; with one `expect`
changed to a line the program never prints it fails and names the check.

Phase 2's first half, the same day: the stat service is the `statd` app,
`abi = "linux"`, and `statd.rs` is gone; btop is the `btop` app, `kind =
"script"`, out of `ports.rs`; `build-apps` builds packages on demand, and
`new-app` writes a folder for either ABI.

Phase 3, the same night: `src/user/native` and `src/user/linux` are
`src/user/system/native` and `src/user/system/linux`, landed as 94401d20
(renames only) and 318987db (paths). The paths were rewritten by a script
that resolved every relative path against the tree before the move; by
hand, the climbs anchored on a shell variable (ferrousli's three reads of
`rust-toolchain.toml`). A branch rebases through it with `git -c
merge.directoryRenames=true rebase origin/main`, then replaces the old
paths its own diff adds; every active session was told so. Gated by the
full `check` with zinc, ferrousli's tests and six x86-64 boots, less one
ferrousli test that calls `sync(2)` on the host (BACKLOG).

2026-10-01: Bad Apple!!'s player is the `badapple` app, the customer's
choice of 2026-09-30 over leaving `src/user/system/linux/media/` to its
owner. The player and `bav` (its video format and the host's converter,
used by nothing else) are one workspace of two crates, `player` and `bav`,
playing through media's `pcm` and `resample` and the compositor's `drm` and
`toolkit` by relative path; the sound server and what it shares stay in the
system. The player's arithmetic is a library now, so its tests run as the
app's host tests. `xtask/src/badapple.rs` keeps its own builds -- the
negative control, `bav-pack` for the host, and on ARMv7-A the hard-float,
Cortex-A7 player an app package does not get -- and finds the folder with
`apps::folder("badapple")`, never its path. The app is opt-in; its smoke
check starts it from a shell with nothing to play. Since `--everything`
installs every app, the desktop takes the package's player and
`on_the_desktop` builds one only when no app is installed: on ARMv7-A that
is the soft-float build, which `test-badapple` does not boot. Gated by
`check`, `test-apps` and `test-badapple` on all three architectures.

Later that day: launcher entries. `[[package.files]]` takes `source =
"folder"` for a file kept in the app's folder rather than made by its
build, and btop and ferrofetch ship a `.desktop` entry and an icon in
`share/` -- btop with `Terminal=true`, ferrofetch run by zinc with a `read`
that holds the terminal open. fuzzel reads `usr/share/applications`
itself, so `fuzzel.rs` is unchanged, and its boot's `fuzzel: 3 entries`
stays, the test boot carrying no apps. badapple ships its icon; its entry
stays in `badapple.rs`, written beside the fetched video, since it names
the video. A host test holds every entry to the package: each program its
`Exec` names is one the package installs (or zinc), and its `Icon` is one
the package carries. With 211bc0e5 (every app under `--everything`, a
script app built through WSL on Windows), the customer's "`--everything`
IS EVERYTHING" holds for apps.

The same evening, a12c4bfb: a package carries symbolic links (a record's
`link`) and whole trees (`tree = true`), and apps build in dependency
order. Then the ported programs became apps: curl, git, sshdt, foot,
vkgears, alsa-lib and alsa-utils, each its port's script moved into its
folder as `build.sh`, which installs into the ports' prefix as before and
copies its files into the package. git depends on curl, whose libcurl it
links and whose certificates it verifies with; alsa-utils on alsa-lib.
`ports.rs` keeps the libraries, libcxx and zlib, which an app builds first
when it needs one. The system tests that boot one of them take the app by
name (`apps::taken`), and the images that carried every built port carry
the script apps already built (`apps::ported`). Every script build holds a
lock on its work directory, the BACKLOG row two gates' btop builds filed.

That landed as 4fd62d8a, and with it the last of phase 2. Then phase 4's
first part: `/bin/pkg` (§7), a first-party Linux program in
`src/user/system/linux/pkg` on `ferrix-pkg` and the kernel's cpio reader,
with `list`, `info`, `install` of local packages and `remove`, host tests
against a directory as the root, and `test-pkg`, which boots it on every
architecture to install, run and remove the stat service and to refuse a
changed file, a missing dependency and a needed removal; with `pkg`'s
digest check turned off for one boot, `test-pkg` fails and names the four
lines that refusal should have printed. Every image a person runs carries
`pkg`.

What is left is §7's list: upgrades, a repository with a signed index,
atomic transactions on btrfs, building on Ferrix, and the system as
packages. None is started; each waits on the customer's order.

The root checkout's `main`, left at c1bd87fb on 2026-09-30 because another
session had staged changes there, has since been moved past the rename.
