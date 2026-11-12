# The kernel built without its extensions

The customer, 2026-11-12, of `src/kernel/core/Cargo.toml` after the crate
split's S5: "clean up the core/Cargo.toml in the kernel, so that it is clearer
what is actually an extension and what is actually a core component", then "i
wanted actual groups in actual toml syntax", "it still looks like the linux
personality is required", and "your job is to fix that, so that everything in
extensions can be built without".

So the goal is not a comment. **A kernel builds with nothing under
`src/kernel/extensions/` and none of the libraries that exist only for the
Linux personality, and each manifest says which of its dependencies those are,
in a table of its own.** This continues `docs/LINUX-ABI-SEPARATION.md` §11.8
(the customer, 2026-10-23: "only make the include the switch surface") and
`docs/microkernel/CRATE-SPLIT.md`, which moved the parts out of
`src/kernel/src` and left the question of who still needs them.

## 1. Where it stands (measured on develop c29cca330, x86-64)

`linux_abi=off` (with `personality=none`, `fs=none`) is a build today
(LINUX-ABI-SEPARATION §11.11): it compiles the Linux code out by `cfg` and
links no symbol of it. But every crate is still an ordinary dependency, is
still compiled, and three kinds of code still name them:

| Who | What it names | Measured |
|---|---|---|
| The binary, `main.rs` | the parts, through one glob: `use ferrix_kernel_raised_checks::*`, which re-exports the load's, the item's and the core's trees, and the named flag words | With the seven extension packages taken out of `src/kernel/Cargo.toml`, an `off` build fails on 11 errors, all in `main.rs`, all from those two `use` lines |
| The core's product files | `ferrix-linux-abi` (56 names), `ferrix-vdso` (13), `ferrix-vfs` (6), `ferrix-cgroupfs` (2), in 27 files | With the four taken out of `src/kernel/core/Cargo.toml`, an `off` build stops at 47 resolution errors in 21 files, the first pass only |
| The item's product files | `ferrix-linux-abi` (14), `ferrix-vfs` (1), in 10 files | not built apart |
| Check files of both | 77 names in about 55 files | most are cut by `off` already |

What the core and the item name, by kind
(`~/.local/share/ferrix/po-2026-11-03/notes/nolinux/inventory.txt`):

* **`Errno`**, about 30 sites: `trap.rs`, `user/{vmo,space}.rs`,
  `object/pager.rs`, `syscall/uaccess.rs`, `audit.rs`, the two
  `restricted.rs`, `arch/*/mod.rs`, and the item's `boot_authority.rs`,
  `init.rs`, `power.rs`, `discovery/devmgr.rs`, `syscall/{mod,native,program}.rs`,
  `syscall/native/region.rs`, `main.rs`. `ferrix-vfs`'s is a re-export of
  `ferrix-linux-abi`'s, a struct over a number in a crate with no dependency.
* **Signal numbers and frames**: `SIGBUS` in `trap.rs`, `arch/mod.rs`,
  `arch/x86_64/mce.rs` (handed to the personality's `fault_signal` hook);
  the signal frames themselves in `arch/{x86_64,aarch64,armv7a}/signal.rs` and
  `signal/compat.rs`.
* **System call numbers** in the three entries (`Syscall`, `RtSigreturn`,
  `ArchPrctl`, `RESTART_SYSCALL`), **TLS descriptors** (`arch/x86_64/gdt.rs`),
  **hwcap** (`arch/{aarch64,armv7a}/mod.rs`).
* **The vDSO**: `arch/*/vdso.rs` or `mod.rs`, `timer.rs`'s `counter_nanos`.
* **cgroupfs** in `object/job.rs` (`Limit`, `controllers`); `ferrix-vfs` in
  `panic/catalog.rs` (one name) and `syscall/program.rs` (`OpenFile`).

## 2. The mechanism: a table per flag, keyed by the flag's `cfg`

`docs/BUILD-FLAGS.md` rule 1 stands: a flag is a key-value `--cfg`, never a
Cargo feature. Cargo has a table for exactly that:

```toml
[dependencies]
# what this crate is built from under every flag
ferrix-kernel-core.workspace = true

# The Linux personality built into ring 0: only a personality=kernel build.
[target.'cfg(ferrix_personality = "kernel")'.dependencies]
ferrix-kernel-load.workspace = true
ferrix-kernel-raised-checks.workspace = true
```

Cargo decides such a table from the `cfg`s rustc is given on its command
line, not from what a build script prints. Spiked on Cargo 1.99.1: with the
load under that table, `cargo tree -p ferrix-kernel --target
x86_64-unknown-none` lists it when `--config
'target.x86_64-unknown-none.rustflags=["--cfg","ferrix_linux_abi=\"on\""]'`
is given, and does not without. So:

* **xtask also passes the flags a manifest keys on as `--cfg` in the target's
  `rustflags`** (appended with `--config`, which joins the array in
  `.cargo/config.toml`; `RUSTFLAGS` would replace it). Today that is
  `ferrix_linux_abi` alone. `build.rs` stays the one place the kernel's code
  gets its `cfg`s from, and **refuses a build where `CARGO_CFG_FERRIX_LINUX_ABI`
  differs from the flag it was handed**, so the manifest and the code cannot
  be built under two values.
* The workspace declares the `cfg` to Cargo (`[workspace.lints.rust]
  unexpected_cfgs`' `check-cfg`), generated from the flag file as rustc's
  list is.
* A plain `cargo build -p ferrix-kernel` with no flag is then an `off` build
  of the manifest and whatever `build.rs` is handed: `build.rs` refuses it
  as above. Only xtask builds the kernel, as today.

No `cfg(feature = ...)` enters the kernel; `kernel_cargo_features` stays 0.

**Two flags, two keys** (ledger 9361 K1). The parts are keyed on
`ferrix_personality = "kernel"`, not on `linux_abi`: the load is in ring 0
exactly there, which is what `build.rs` derives `ferrix_load_built = "ring0"`
from, so the kernels built without an extension are `personality=user`
(`fast`, `max`) and `personality=none`. The core's and the item's Linux
libraries (E6) are another matter: they are needed wherever `linux_abi=on`,
`personality=user` included, so their tables are keyed on
`ferrix_linux_abi = "on"`. That is why xtask gives both flags to rustc, and
`build.rs` holds both to the flags it was handed.

## 3. The steps

Each is a landing with a design or diff check of its own. E1 is small and
makes the customer's sentence true for the extension crates; E2 to E5 are the
code that makes it true for the libraries; E6 is the manifests.

| Step | What | Touches | Size (guess) |
|---|---|---|---|
| **E1** | The transport (§2). The binary's five Linux parts (`ferrix-kernel-load`, `-raised-checks`, `-settings`, `-logctl`, `-net-ring`) move to the `personality = "kernel"` table; `main.rs`'s glob of the raised checks' crate and the load's flag word go under `cfg(ferrix_load_built = "ring0")`, with the item's glob otherwise. A check: a `personality=user` and a `personality=none` build's `cargo tree` names no package under the parts' place but the board glue, with a control. | xtask, `build.rs`, `src/kernel/Cargo.toml`, `main.rs` (item, the composition root), the workspace manifest | 3 |
| E1b | The board glue (`board/stm32mp1`, `board/gs201-usb`): an `off` kernel installs it today. A flag of its own, or the two keyed by `target_arch` (they are Arm's), is a question (§5 Q1). | as E1 | 2 |
| **E2** | `Errno` out of the Linux crate: a crate of its own with no dependency, `ferrix-errno`, holding the struct and its constants as they are; `ferrix-linux-abi::errno` and `ferrix-vfs` re-export it, so no caller's type changes. The core and the item name `ferrix_errno::Errno`. | about 30 `use` sites, no body | 3 |
| **E3** | The architecture's Linux code under the flag: the signal frames, the Linux system call numbers of the three entries, TLS descriptors, hwcap, the vDSO and `counter_nanos`. Where `off` already answers `ENOSYS` (`NoLinux`), the entry needs no Linux number; where the trap path hands a signal number to `fault_signal`, it hands a fault kind and the personality picks the number. | `arch/*`, `trap.rs`, `timer.rs`; core ring, three architectures | 13, the uncertain one |
| E4 | `object/job.rs`'s `Limit` and `controllers`, `panic/catalog.rs`'s one name | core | 2 |
| E5 | The item's remaining names (`Syscall` in `syscall/mod.rs` and `main.rs`, `OpenFile` in `program.rs`) | item | 3 |
| **E6** | `ferrix-linux-abi`, `-vdso`, `-vfs`, `-cgroupfs` move to the `linux_abi = "on"` table in the core's and the item's manifests; the `off`-only `unused_crate_dependencies` allow goes. The check of E1 then refuses every Linux-only library in an `off` tree. | manifests, the allow | 2 |
| E7 | The screen (`ferrix-fbtext`, `ferrix-qr`): CRATE-SPLIT's P5 | | P5's |

## 4. What each landing shows

* **An `on` kernel is the kernel it was.** E1, E2 and E6 change no body: the
  symbol comparison of S1 (release, `present` and `desktop`, x86-64 and
  AArch64) against the base, no differing body unexplained. E3 to E5 change
  core and item bodies and are reviewed as code, with their requirements.
* **An `off` kernel builds, on three architectures**, as `check` builds it
  today, and after E1 its tree holds no extension package (the new check),
  after E6 no Linux-only library.
* The rows a kernel change owes; `check-item-boundary`, `check-lift`,
  `check-crate-layering` unchanged in what they refuse.

## 5. Questions

1. **The board glue under `off`** (E1b). *Default:* key the two by a new flag
   `board_glue = on | off`, `on` in every profile, so nothing changes until a
   profile says `off`.
2. **`Errno` as a crate of its own** (E2) rather than a core-owned error type
   translated at the personality's edge. The first changes no type and no
   body; the second is the cleaner end and touches every signature. *Default:*
   the first now.

## Where it stands

Design reviewed **OK IF, ledger 9212** (M1-M7, E1.1-E1.8; Q1: a flag
`board_glue`; Q2: `ferrix-errno` re-exported; E3 needs a design note, N1-N12,
and five landings; E6 goes library by library).

**E1 is on develop ahead of its review** (49bd1ae9e, 2026-11-12): the customer
ordered it there at once. It made `check` and `test-ring0-budget` red, and was
fixed forward the same evening on the consultant's ruling (ledger 9361: the
key on personality accepted, K1; 3623 (R1) amended, S1-S3; the budget's rise,
B1-B3). What is built: the transport, the binary's five Linux parts in the
keyed table, `main.rs`'s globs. What is owed is 9212's conditions as 9361 K1
reads them and the diff check (BACKLOG); nothing further of §3 lands before
that verdict.

The comment-only grouping of the core's manifest landed as c29cca330 and is
replaced by E6's tables.
