# Write domains: a program changes only its own files

The customer asked on 2026-10-01 how Ferrix could stop encryption malware,
and how to make sure a program can only encrypt its own data. This document
answers that question. The answer is not to detect encryption. It is to take
away the authority encryption malware needs: the right to change files the
program did not make and was not given.

Status: **design, no code.** Written 2026-10-01 on `main` at 2ec36dbbc.
Nobody has reviewed it yet: the certification consultant's seat was empty
when it was written (§11).

**In one paragraph.** Every process gets a *write domain*: the folders whose
files it may change. The VFS checks it after the uid/gid check, and no uid
gets past it, root included. The domain lives beside the process's
credentials. It can only be narrowed, and fork and exec inherit it. Linux
programs reach it through Linux's Landlock calls, which Chrome already asks
for and gets `ENOSYS`.

Launchers make the protective domains: init for services and the desktop's
launcher for apps. They build them from the unit file and from `app.toml`
before exec, so a program cannot decline one. A packaged app gets its own
data folders and nothing else unless its manifest names more. The user
hands it any other file through a trusted chooser, which passes an open
descriptor. A domain cannot protect a file the user deliberately handed over,
or anything a shell with the user's full authority runs. For those, read-only
btrfs snapshots provide undo.

There are six landings, W1 to W6 (§9).
* W1 to W3, **18 points** (about 6 to 9 hours of one session), are enough
  that a packaged app or service cannot change anything outside its own
  folders.
* The whole set is **47 to 60 points** (about 16 to 30 hours).

---

## 1. The property, and why not detection

To the kernel, encryption is just writing bytes that look random. gzip, zstd,
a video encoder, `git gc`, a password manager and a backup tool all write the
same kind of bytes. The detectors people build all have known ways around
them:
* **Entropy of written data.** Encoding the output as base64 lowers it.
* **Rate of files touched.** Slow encryption stays under it.
* **Renamed extensions.** Encrypting in place avoids them.
* **Partial encryption.** Encrypting the first megabyte of each file avoids
  most of them.

The same detectors also fire on a package upgrade or a `git checkout`. A
detector gives a probability, and the customer asked for a guarantee.

**The property.** A process may *change* a file in five ways: write it,
truncate it, delete it, rename something over it or move it away, or change
its attributes. It may do so only if one of these holds:

1. the file is inside the process's write domain; or
2. the process holds an open descriptor for the file, opened for writing by
   something that was allowed to write it.

This design leaves reading unchanged (§10, Q2).

**What it asks of each way to the data.** Encryption malware has to change
the victim's files somehow. These are all the routes, and §3 and §5 close
each one:

* opening for writing, or with `O_TRUNC`;
* `truncate`;
* writing an encrypted copy, then deleting or renaming over the original;
* linking or renaming the file into a folder it may write, then writing it
  there;
* a shared writable mapping;
* the raw block device;
* a mount that puts the victim's folder under its own;
* borrowing another process's authority: tracing it, writing its memory,
  asking a service to write for it, or faking the input of a trusted
  program's window;
* being root.

**What it cannot stop.** A domain cannot protect a file the user deliberately
handed over, or anything a program with the user's whole authority does, such
as a script run from a terminal. No access-control scheme can stop those,
because the user granted the authority. §4's undo is the answer there.

## 2. Ferrix today

| What | Where | What it means here |
|---|---|---|
| The only file check is the uid/gid check | `src/lib/fs/vfs/src/namespace.rs:1006-1017` (Linux's `may_open`), `access.rs:98-114` | No check by program |
| uid 0 passes every read and write check | `src/lib/fs/vfs/src/access.rs:110-113` | Any check this design adds must not be the uid/gid check |
| The identity a check is made as | `Context { root, cwd, who, ns }`, `namespace.rs:419-431`; built in `src/kernel/src/syscall/path.rs:185-193` | The domain goes in the same place as `who` |
| The desktop and every program it starts run as root | `docs/AUTH.md` §6.1; `hyprix.service` has no `User=` (`tools/common/xtask/src/init.rs:487-497`) | Today every GUI app can rewrite every file |
| Landlock | `ENOSYS` (`docs/CHROME.md:638`) | No program can restrict itself |
| seccomp | No filter can be installed yet, and a filter never sees a path (`docs/SECCOMP.md` §3.2) | Cannot say "only under this folder" |
| init's sandboxing keys | `NoNewPrivileges=`, `PrivateTmp=` and `ProtectSystem=` carried out since L13a (`docs/INIT.md` §4.5); the rest warned about and ignored (`src/lib/init/svc/src/kind/sandbox.rs`, `NOT_BUILT`) | `ProtectHome=`, `ReadOnlyPaths=` and `ReadWritePaths=` do nothing yet |
| `app.toml` | Declares files, not permissions (`docs/APPS.md` §3) | No place to say what an app may write |
| Extended attributes | No filesystem keeps them: get is `ENODATA`, set is `EOPNOTSUPP` (`src/kernel/src/syscall/fsctl.rs:101-106`) | Nowhere to store a per-file tag (§10, Q3) |
| btrfs snapshots | The writer refuses a volume with any (`src/lib/fs/btrfs-write/src/lib.rs:42-51`); planned as Stage C (`docs/ARCHITECTURE.md`, `BtrfsSubvolumes` in `docs/sysml/09-storage.sysml`) | No undo |
| Virtual keyboard and pointer | hyprix serves `zwp_virtual_keyboard_v1` and `zwlr_virtual_pointer_v1` to any client (`src/user/system/linux/compositor/hyprix/src/state.rs:353`, `:5514`) | Any client can type and click into another client's window (§3.4) |

What already helps:

* **An open descriptor is a capability.** Write permission is checked once,
  at open, and after that only the descriptor's mode is checked. A shared
  writable mapping needs a descriptor opened for writing
  (`src/kernel/src/syscall/memory.rs:273`).
* **Descriptors can be passed** over a Unix socket with `SCM_RIGHTS`
  (`src/kernel/src/fs/socket.rs:34-40`).
* **Mount namespaces** (N1 to N3, `docs/NAMESPACES.md`) give a service a
  private `/tmp`.
* **`Location::parent`** (`namespace.rs:332`) walks up across mounts to the
  top of the namespace's tree, which is the walk §3.1 needs.
* **Several bypass routes do not exist yet.** Ferrix has no `ptrace`, no
  `process_vm_writev`, no `/proc/<pid>/mem` and no io_uring. §5 says what each
  must check when it arrives.

## 3. The design

### 3.1 The domain

A `WriteDomain` is a stack of up to 16 layers, as in Landlock.
* **A layer** is a set of *handled* rights plus rules. Each rule maps a
  directory `Location` to the rights it grants beneath it.
* **Domains never change.** A domain is shared behind an `Arc`, so fork
  shares it and exec keeps it.
* **Narrowing makes a new domain** with one more layer. No call removes a
  layer or widens one.
* **Where it lives:** on the process beside its credentials. `path::context`
  copies it into `Context` the same way it fills in `who`
  (`path.rs:185-193`).
* **When there is no domain** (`None`), the check is skipped and costs
  nothing. That is every process today.

**The rights** are Landlock's filesystem rights, minus the reading ones:
* writing: `WRITE_FILE` and `TRUNCATE`;
* deleting: `REMOVE_FILE` and `REMOVE_DIR`;
* creating: `MAKE_REG`, `MAKE_DIR`, `MAKE_SYM`, `MAKE_FIFO`, `MAKE_SOCK`,
  `MAKE_CHAR` and `MAKE_BLOCK`;
* linking or moving into another folder: `REFER`.

Ferrix adds one right of its own, `SET_ATTR`, which covers `chmod`, `chown`
and `utimes`. Landlock does not restrict those, but making a stranger's files
mode 000 is a smaller version of the same attack. Programs that use the
Landlock calls get Linux's behaviour exactly, so `SET_ATTR` is only for domains
a launcher makes (§10, Q1).

**The check.** `may_change(ctx, at, want)` asks every layer. For each layer:
* Walk from `at` up through `Location::parent`, to the top of the namespace's
  tree, and union the rights the rules along the way grant.
* The layer allows the change if `want` is covered by that union, or is not
  in the layer's handled rights.

The change goes ahead only if every layer allows it. For a new or removed
name, `at` is the parent directory. For a change to an existing file's
contents, it is the file.

Because the walk goes to the top of the tree and not to `ctx.root`, a
`chroot` neither adds a grant nor removes one. A rule names a `Location` taken
from an open directory descriptor (`O_PATH` will do), never a path string. A
renamed folder therefore keeps its rule, and spelling the path differently
changes nothing.

**Rules.** Each rule carries a number, and §8's checks each name the rule
they test.

| Rule | Statement |
|---|---|
| W-R1 | A change of any kind (the table in §3.2) is checked against the domain after the uid/gid check, and fails with `EACCES` when a layer refuses it. |
| W-R2 | The domain check never asks for a uid. Root in a domain is bound like anyone else. |
| W-R3 | A domain is only ever narrowed. Fork and exec, set-id programs included, keep it. |
| W-R4 | Making a domain sets `no_new_privs`, as Landlock requires, so a set-id program run inside it gains no ids. |
| W-R5 | A process in a domain gets `EPERM` from every call that changes the mount tree (`mount`, `umount2`, `pivot_root`, and any later one), in every user namespace. |
| W-R6 | `link` and `rename` into another folder are refused when the file would gain a right there that it lacks where it is (Landlock's `REFER` rule). |
| W-R7 | Whether a descriptor may be `ftruncate`d is decided at open, as in Landlock ABI v3, and stored on the open file. |

### 3.2 Where the check goes

| Operation | Site | Rights |
|---|---|---|
| Open for writing, or with `O_TRUNC` | `Namespace::open_resolving`, after the uid/gid check (`namespace.rs:1006-1017`); this also covers reopening through `/proc/<pid>/fd/<n>`, which resolves to a `Location` | `WRITE_FILE`, plus `TRUNCATE` with `O_TRUNC` |
| `O_CREAT` of a new file | The create helper, beside `may_create` (`namespace.rs:1068`) | `MAKE_REG` on the parent |
| `mkdir`, `mknod`, `symlink` | `namespace.rs:1088`, `:1105`/`:1127`, `:1151` | `MAKE_DIR`, `MAKE_*` by kind, `MAKE_SYM` |
| `link` | `namespace.rs:1170` | `MAKE_*` on the new parent, then W-R6 |
| `unlink`, `rmdir` | `namespace.rs:1203`, `:1231` | `REMOVE_FILE`, `REMOVE_DIR` on the parent |
| `rename` | `namespace.rs:1264` | `REMOVE_*` on the source's parent, `MAKE_*` on the destination's, `REMOVE_*` for a name it replaces, W-R6 across folders; `RENAME_EXCHANGE` both ways |
| `truncate` | The system call layer, `src/kernel/src/syscall/fsctl.rs:410-422` (`Namespace::truncate` takes no `Context`, `namespace.rs:1364`) | `TRUNCATE` |
| `chmod`, `chown`, `utimes` | `set` in `src/kernel/src/syscall/path.rs:719-725` | `SET_ATTR` (launcher domains only) |
| `write`, `pwrite`, `writev`, `copy_file_range`, `sendfile`, `fallocate`, shared writable mappings | None: the descriptor's mode was decided at open (`memory.rs:273` for mappings) | — |
| `ftruncate` | `src/kernel/src/syscall/fd.rs:608` reads W-R7's bit | — |
| `setxattr` and friends | None today: no filesystem keeps attributes (`fsctl.rs:101-106`). When one does, here | `SET_ATTR` |
| `mount`, `umount2`, `pivot_root` | `fsctl.rs:550`, `:668`, `:726` | Refused under any domain (W-R5) |

The check costs the depth of the walk times the number of layers. It runs only
on opens that change something, and only for a process that has a domain.

### 3.3 Who makes a domain: the launchers

A program can narrow itself through Landlock, and Chrome and sshd will. But
malware will not narrow itself, so the domain that protects the user's files
is made by the program's launcher, before exec.

**init, for services.** In L13, `ProtectHome=`, `ProtectSystem=`,
`ReadOnlyPaths=` and a new `ReadWritePaths=` become write-domain layers. They
no longer need mount namespaces for their write half. `InaccessiblePaths=`
needs the reading rights and stays with the namespaces (§10, Q2).

**One path for starting an app.** Today a desktop entry is exec'd by
whichever program reads it. Apps instead start through one launcher, which
reads the installed package's record (`docs/APPS.md` §6) and builds the
domain from it. A program started any other way runs under the domain of
whatever started it, which is never wider.

**The base domain every app gets:**
* its own folders: `$XDG_DATA_HOME/<name>`, `$XDG_CONFIG_HOME/<name>`,
  `$XDG_CACHE_HOME/<name>` and `$XDG_STATE_HOME/<name>`;
* a private `/tmp`, from a mount namespace;
* `$XDG_RUNTIME_DIR` for sockets;
* the character devices every program writes: `/dev/null`, `/dev/zero`,
  `/dev/full`, `/dev/tty` and `/dev/pts/*`;
* `/dev/shm`.

No block device is ever in it, which closes the raw-disk route.

**The manifest says what more an app may write.** It says it in
`[package.access]`, because `[package]` is the part of `app.toml` that travels
into the installed record:

```toml
[package.access]
writes = ["downloads"]   # named places, never paths
```

The named places are `downloads`, `documents`, `music`, `pictures`, `videos`,
`desktop`, and `home`, which means the whole home. The installer shows the
list, and shows `home` as a warning. `home` is the way out for a file manager
or an editor until the chooser (§3.4) exists for it.

**The terminal** is an app whose manifest names `home`: what the user types is
the user's own authority. For a command the user does not trust, `confine`
gives one command a narrower domain:

```sh
confine --writes . -- ./build.sh
```

`confine` is a small tool over the same calls, in the spirit of `landrun` on
Linux.

### 3.4 Handing over a file: the chooser

`ferrix-chooser` is a user service started by init. It is outside every
app's domain, and it `Offers=` the name `file-chooser` in init's directory
(`docs/INIT.md` §6).

**How a hand-over works:**
1. An app asks the chooser to open or save a file.
2. The chooser shows its own window, and the user picks the file.
3. The chooser opens the file with the mode asked for, and returns the
   descriptor with `SCM_RIGHTS`.

For "save as", the chooser creates the file and returns it opened for writing.
The app can write that one file. It cannot rename it, delete it, or touch the
files beside it, and its domain never grows.

**The user's click is the grant, so the click must be real:**
* hyprix delivers no `zwp_virtual_keyboard_v1` or `zwlr_virtual_pointer_v1`
  input to the chooser's window;
* no other client can capture the window or place itself over it;
* the window keeps focus while it waits.

Today any client may bind both virtual-input protocols, so without these
three, any app could pick files for itself.

**Linux GUI programs** ask for a chooser through xdg-desktop-portal over
D-Bus, and Ferrix has no D-Bus (`docs/CHROME.md:640`). Until a portal
front-end exists, a Linux program that edits documents needs a named place in
its manifest. Native apps come first.

## 4. Undo: what a domain cannot stop

A domain cannot protect three things:
* a file handed over through the chooser;
* anything a program run from the terminal does;
* anything an app whose manifest names `home` does.

For those, the answer is to make the damage reversible: read-only snapshots
of the home subvolume.

**What it needs from btrfs.** btrfs-write's Stage C: subvolumes and
snapshots, with the reference bookkeeping for shared tree blocks whose
absence is why the writer refuses them today (`lib.rs:42-51`). Then the
calls to create a read-only snapshot, list snapshots and delete one.

**The policy:**
* **When.** An init timer unit takes a snapshot every hour, and keeps a
  ladder of 24 hourly, 7 daily and 4 weekly snapshots.
* **Where.** `/home/<user>` is a subvolume, and the snapshots live in
  `/.snapshots`, mode 0700 root, outside every domain.
* **Who may delete one.** Only root outside any domain. A domain never grants
  it.
* **Full disk.** The policy never deletes a snapshot to make room while the
  disk fills. Writes fail with `ENOSPC` instead. That fails closed: at worst
  the malware fills the disk, and every earlier version stays.
* **Restoring** is a reflink copy out of a snapshot.

Until Stage C lands, Ferrix has no undo, and a backup off the machine is the
only one.

## 5. Ways around a domain, and what closes each

| Route | What closes it | Evidence (§8) |
|---|---|---|
| Being root | W-R2: the check asks for no uid | A boot check as uid 0 in a domain; its negative control adds a root bypass, and the check must fire |
| Hard-linking or renaming a stranger's file into the app's own folder | W-R6 | Host tests; the reparenting cases of Linux's `fs_test.c` in `test-landlock` |
| A hard link made before the domain existed, already inside the app's folder | Open: a domain judges by place, and the place is the app's. Q3's per-file tags would close it | — |
| Bind mount, `pivot_root`, or a new user namespace and then a mount (unprivileged mounting is N5, `docs/NAMESPACES.md`, not yet on `main`) | W-R5, which holds in every user namespace, so N5 opens no route | A boot check that tries each, in a fresh user namespace too |
| Reopening through `/proc/<pid>/fd/<n>` | The check in `open_resolving` sees the target's `Location` | A host test and a boot check |
| A raw block device | Never in a base domain; `MAKE_BLOCK` never granted | A boot check |
| A descriptor opened for writing that the launcher leaked | Launchers pass only 0 to 2 and what they mean to | A launcher test |
| A set-id program | W-R4 | A boot check with a set-id helper |
| `ptrace`, `process_vm_writev`, `/proc/<pid>/mem` | None exist today. When `ptrace_may_access` lands (NP, `docs/BACKLOG.md`), it refuses a caller whose domain is not the same as or narrower than the target's, as Landlock does | Owed with NP |
| A service that writes for its client (a file manager service, a print spooler) | Services take descriptors from clients, never paths. hyprix's drag-and-drop already moves data through pipes | Review of each service |
| Faked input to the chooser | §3.4's three conditions in hyprix | A compositor test that sends virtual-keyboard input to the chooser and sees none arrive |
| Shared writable mappings, `fallocate`, `copy_file_range` | The descriptor's mode, fixed at open (`memory.rs:273`) | Existing |

## 6. Detection, last

After W1 to W5, what is left is a program with the user's whole authority,
and §4's undo for it. A tripwire can shorten how much of the disk such a
program gets through before the user notices:

* **The counter.** Count, per process, the distinct existing files it
  truncates, renames over or deletes outside its own folders, over a sliding
  window. inotify carries no process (`src/kernel/src/fs/inotify.rs`), so the
  counter lives in the same VFS hooks as §3.2.
* **The action.** Past a threshold, stop the process with `SIGSTOP` and ask
  the user through a notification whether to let it continue or end it.

It is a heuristic. Partial, slow or in-place encryption stays under any
threshold, and a package upgrade goes over it. It belongs only after
snapshots, which are what make a miss recoverable.

## 7. Where it lives: the certified item

**Nothing in the security target covers user files today:**
* The VFS, btrfs and tmpfs are outside the TOE
  (`docs/certification/SECURITY-TARGET.md:39-44`).
* No asset is a user's file (`:95-101`).
* No threat is tampering with file data (`:111-117`).
* FDP_ACC.1's objects are VMOs, channels, ports, jobs, interrupts and I/O
  mappings (`:174-178`).

**This design adds no item code.** W1 is in `src/lib/fs/vfs`, and W2 is in
the Linux personality. A claim about it is a decision, with two ways to make
it:

* **(a)** An objective for the operational environment, argued from the VFS
  as an untrusted component the TOE contains.
* **(b)** Files and directories become handle objects, so that the existing
  Capability Access Control SFP covers them. That is a large change, and the
  shelved ring-3 filesystem plan (`docs/OPAQUE-KERNEL.md`) would be its home.

Which one, and whether to make a claim at all, is the customer's decision
with the certification consultant (§10, Q5). Until then nothing goes into
`docs/certification/`.

## 8. Checks

* **Host tests in the vfs crate:**
  * the domain's algebra: narrowing only, union along the walk, intersection
    across layers, `chroot` changing nothing;
  * every row of §3.2 with an allowed case and a refused one;
  * W-R6's cases;
  * a renamed rule folder keeping its grant.
* **A boot check** in `src/kernel/src/fs/` that runs as uid 0 in a domain:
  * every change outside the domain is refused;
  * the same change inside it succeeds;
  * mounts are refused (W-R5).

  Its negative control is a one-line sabotage, `if who.privileged() { return
  Ok(()) }` at the top of the check. The boot must stop on the check's own
  message, shown with a marker, and then the line is restored.
* **`cargo xtask test-landlock`:** Linux's own `tools/testing/selftests/landlock`
  file-system tests, built against ferrousli, as `test-seccomp` runs Linux's
  seccomp tests. Tests of rights this design does not cover are listed as
  skipped, by name.
* **Launcher tests:**
  * a unit with `ProtectHome=read-only` cannot write in `~`;
  * an app installed from an `app.toml` with no `[package.access]` can write
    only its own folders;
  * no descriptor beyond 0 to 2 is passed to an app.
* **The encryption test.** A test program, never shipped, walks a populated
  home and XORs every file it can open for writing.
  * Run as an app with the base domain, it must leave every file outside its
    folders byte-identical, compared by hashes before and after.
  * Run from the terminal's domain, it changes them all. After W5, the files
    are then restored from the last snapshot and compared again.

## 9. The landings

| Landing | What | Points | Needs |
|---|---|---|---|
| W1 | `WriteDomain` in the vfs crate, `may_change` at every site in §3.2, W-R1 to W-R3 and W-R5 to W-R7, host tests | 8 | — |
| W2 | Landlock in the Linux personality (`landlock_create_ruleset` with the version query, `landlock_add_rule` for `PATH_BENEATH`, `landlock_restrict_self`), W-R4, the boot check and its negative control, `test-landlock` | 5 | W1 |
| W3 | Launchers: init's write keys in L13, `[package.access]` in `app.toml` and `ferrix-pkg`, the one app launcher, `confine` | 5 | W2 |
| W4 | The chooser, and hyprix's three conditions for its window | 8 to 13 | W3 |
| W5 | btrfs-write Stage C, subvolumes and snapshots (13 to 21), then the snapshot unit and its policy (3) | 16 to 24 | — (the btrfs stream) |
| W6 | The tripwire | 5 | W5 |

* **W1 to W3** are 18 points, about 6 to 9 hours of one session. Together
  they answer the customer's question for packaged apps and services: such a
  program can encrypt only its own data.
* **The whole set** is 47 to 60 points, about 16 to 30 hours. The range comes
  from W4's protocol work and from W5. W5 is the largest part, and it can run
  in parallel with the others.

The session off root (`docs/AUTH.md` §6) is not a prerequisite, because
W-R2 needs no uid. It is still owed, so that users are kept apart from one
another.

## 10. Open questions

* **Q1. How a launcher asks for `SET_ATTR`.** The Landlock calls must keep
  Linux's meaning, so the right needs another way in: a native call, or a
  flag bit that Linux refuses. To be decided in W2's review.
* **Q2. Reading.** Malware often copies a victim's data out before encrypting
  it, so it can threaten to publish it. The same walk handles `READ_FILE` and
  `READ_DIR`. But a domain that limits reading must list every library, font
  and configuration file an app reads. That is a much larger manifest, and it
  is left for after W3.
* **Q3. Per-file tags.** Tagging each inode with the app that created it
  would make files an app created anywhere its own, and would close §5's
  pre-existing-link gap. It needs attributes kept on disk, and today no
  Ferrix filesystem keeps any.
* **Q4. The terminal's default.** Whole home, as §3.3 has it, or a narrower
  default with `confine --writes ~` as the way out. This is the customer's
  call.
* **Q5. The certification claim** (§7). The customer's decision, with the
  consultant.
* **Q6. Linux GUI programs and the chooser.** Either a portal front-end
  without D-Bus, or a D-Bus.

## 11. Where it stands (2026-10-01)

**State.** A design only: no code and no branch. `main` was at 2ec36dbbc
when this was written.

**Review.** Nobody has reviewed it. The certification consultant's and the
product owner's seats were both empty, according to the owners table in
`docs/BACKLOG.md`. Neither review is required to land a document outside
`docs/certification/`. Both are owed before W1's code: the consultant's for
§7 and Q5, and the product owner's for the order of the landings.

**Requirement ids.** This document writes none. W1 reserves its ids first
(`docs/CONVENTIONS.md`, *Requirement ids are reserved before they are
written*).

**Its row** is in `docs/BACKLOG.md` P2, beside seccomp's.
