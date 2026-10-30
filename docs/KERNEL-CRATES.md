# The kernel in four crates

**Design for review, 2026-10-15 (bs-a).** The kernel is one crate of 237k
lines, `ferrix-kernel`. Whatever file a kernel edit touches, rustc re-hashes
the whole crate, writes its whole incremental state again (178 MB of dep
graph and 100 MB of query cache) and runs clippy's lint pass over all of it.
After the dev profile lost its full DWARF and its crate-local ThinLTO (cuts C1
and C2 in `docs/TEST-TIME.md`), that part is most of what is left of a kernel
edit, and it grows with the crate, not with the edit. This document splits the
crate along the certified item's rings, so that an edit recompiles only its
ring's crate and those above it.

The certification consultant reviewed the shape on 2026-10-15: **OK IF (1) to
(12)**, ledger line 1477 of `~/.local/share/ferrix/cert-consultant/reviews.md`,
with (1) to (4) to be answered here before any code. §3 answers them; §4 is
the plan for (5) to (12).

## 1. The shape

| Package | Crate root | What it holds | Depends on |
|---|---|---|---|
| `ferrix-kernel-core` | `src/kernel/src/kernel_core.rs` | the core ring's files (104k lines) | the libraries it uses today |
| `ferrix-kernel-item` | `src/kernel/src/kernel_item.rs` | the item ring's files but `main.rs` (14k lines) | core |
| `ferrix-kernel-load` | `src/kernel/src/kernel_load.rs` | the load ring's files (122k lines), and the check files of §3 (3) | core, item |
| `ferrix-kernel` (the binary) | `src/kernel/src/main.rs` | the composition root, as today, and `stages_check.rs` | core, item, load |

**Every source file stays where it is.** The rings are keyed by path, so are
the traceability anchors (`path::function` under `src/kernel/src`), the
coverage evidence and its residual, and the panic catalog's `see` lines; none
of them changes. The three new root files sit beside `main.rs` and are classed
in `certification-item.json` with their ring. The packages' manifests are in
`src/kernel/core/`, `src/kernel/item/` and `src/kernel/load/`, each naming its
root with Cargo's `[lib] path` (a manifest key, not a `#[path]` attribute).
`build.rs` stays with the binary: it emits only link arguments for binaries,
and the one `env!` is in `main.rs`.

**`crate::` paths keep working.** Each upper root re-exports the lower crates'
top-level modules (`pub use ferrix_kernel_core::{arch, mm, object, ...}`), so a
load file's `crate::mm::zero_frame` names the same function it names today.
Only the files that declare modules change (§3 (1)), plus visibility (§3 (4)).

**What an edit then costs** (estimates; measured per step, §4): an edit in the
load ring, where most feature work lands, recompiles `ferrix-kernel-load`
(about half of today's crate) and the small binary; an item edit about 57% of
today; a core edit all four crates, each incrementally, about today's cost. The
kernel clippy pass after a load edit lints the load crate and the binary only.
It is also the shape `docs/MICROKERNEL.md` §4's lift needs: the load built as
its own unit against the core's and item's interface.

## 2. The facts it rests on

Measured on `main` f61906141 with the item-boundary gate's own classifier and
resolver (`~/ferrix-logs/bs-a/split/closure.py` and `surface.py`):

* **No product file reaches upward.** `known_violations` is empty: no core
  product file names the item or the load, and no item product file names
  the load except `main.rs`.
* **Check files do.** Taking each check file to the lowest crate that can see
  everything it names, transitively (a check that uses another raised check's
  helpers is raised with it), **26 check files are raised**: 17 core files to
  the load (the three architectures' `trap/check.rs` and `switch/check.rs`,
  `paranoid/check.rs`, `device/config_check.rs`, `iommu/check.rs`,
  `object/{alloc,check,device_copy,domain,edge,fast_path,format,quota,write_read}_check.rs`
  and `object/pin/check.rs`, `user/{madvise,rmap}_check.rs`), one core file
  to the item (`arch/x86_64/apic/check.rs`, for `crate::checks::run`), four
  item files to the load (`service_check.rs`, `syscall/{native,program,unmap}_check.rs`)
  and `stages_check.rs` to the binary. They need the load to run Linux
  programs (`syscall::exec`, `syscall::image`, `syscall::process`) and native
  calls (`syscall::native`).
* **Eleven lower product files name a raised check file** -- the conflicts
  §3 (3) resolves:
  * **wrappers**: `arch/{aarch64,armv7a,x86_64}/mod.rs`'s
    `check_exception_entry` and the stage 9 switch checks' reporting
    (`check_switch_state` on x86-64, its ARMv7-A counterpart), which only
    call the raised checks and print what they found; and `arch/mod.rs`'s and
    `arch/aarch64/mod.rs`'s and `arch/armv7a/mod.rs`'s re-exports of
    `drive_native_words` and `drive_system_call`;
  * **checks inside a bring-up**: `arch/x86_64/smp.rs` calls
    `apic::check::put_first_in_x2apic` and `require_every_processor`;
    `iommu.rs` calls `check::check_firmware_left_on` while it opens each VT-d
    unit, and re-exports `check_dma_faults`, `check_iommu` and `run`;
    `iommu/remapping.rs` calls `check::plant_masked_byte` and
    `require_masked_byte_served` around the console line's conversion.
* **The cross-crate surface** after the split: 1,210 distinct core and item
  items named by a higher crate's paths (884 `pub(crate)` today, 51 modules,
  15 `pub(super)`, 27 private, the rest enum variants, associated items and
  re-exported names the regex does not place); 302 of them are named only by
  raised check files. Method calls and field reads do not show in paths; the
  compiler names them (§3 (4)).
* **Nothing in the kernel is `pub` today**: 5,666 `pub(crate)` and no bare
  `pub` item. So after the split, the set of `pub` items in core and item
  files *is* the crates' interface, and can be listed exactly.

## 3. The consultant's conditions (1) to (4)

### (1) No `#[path]` and no `include!`

None is used. Every module is found by position:

* Each library root is a file in `src/kernel/src`, so its `mod mm;` is
  `src/kernel/src/mm.rs` or `mm/mod.rs`, as `main.rs`'s is today.
* A directory whose files belong to two rings is declared in each crate that
  owns some of it with an **inline module**, which also resolves by position:
  `kernel_load.rs` writes `pub mod syscall { pub use ferrix_kernel_item::syscall::*; pub mod eventfd; ... }`,
  and `syscall/eventfd.rs` is found where it is. `main.rs` already declares
  `platform` this way. The mixed directories are the crate root's own (19
  core and 9 item files), `discovery/` (`mod.rs` item; six core children, two
  item), `syscall/` (`mod.rs` item; 52 load children, five item, one core:
  `uaccess.rs`) and `platform/` (core and load). Their `mod.rs` files stop
  declaring the children another crate owns and re-export them instead
  (`pub use ferrix_kernel_core::discovery::acpi;`), so `crate::discovery::acpi`
  still names one module.
* A raised check file is declared the same way by the crate that holds it:
  `kernel_load.rs` writes `pub mod object { pub use ferrix_kernel_core::object::*; pub mod check; ... }`,
  and the core's `object/mod.rs` no longer declares `mod check;`.

`check-item-boundary.py` keeps refusing both attributes, unchanged.

### (2) Each file is reached by exactly one crate

The boundary gate walks the module trees from all four roots (it walks one from
`main.rs` today) and fails a file reached by none or by more than one, naming
it and the roots that reach it. Cargo would build a file declared twice as two
copies with two sets of statics and locks and say nothing, which is why the
gate, not the compiler, holds this. Negative controls through `gate.sh control`:
a raised check declared again in the core's `mod.rs` (reached twice), and a
`mod` line removed (reached by none).

### (3) Crate equals ring, with a ratcheted list of exceptions

The gate checks that every file's crate is its ring's, except:

* `main.rs`, the composition root, item ring, in the binary (as named in
  `composition_root` today);
* the **raised check files**, listed in `certification-item.json` under a new
  key, `raised_checks`, each with its ring and its crate. The list is a
  ratchet like `known_violations`: it may shrink freely; it may grow only with
  a reason; an entry whose file is back in its ring's crate, or that no longer
  needs raising, fails as stale. **Only files matching `test_file_patterns`
  may be listed**: a product file is never raised.

The eleven conflicts of §2 are resolved before the split, in one change of
their own that moves no file between crates (step S1 of §4), so the list
starts at the files above and nothing product is raised:

* **The wrappers move up.** They are verification code that lives in product
  files today: each calls checks and prints their findings. Each moves, word
  for word, into the raised check file it wraps (`check_exception_entry` into
  the architecture's `trap/check.rs`, the switch reporting into
  `switch/check.rs`), and its callers (`main.rs`, `stages_check.rs`) name it
  there. The `drive_*` re-exports move from the core's `arch` modules to the
  load crate's `arch` module, which re-exports the raised files' functions
  under the same names. Product line counts fall by what moved; the panic
  catalog's and traceability's anchors for those functions move with them and
  are carried.
* **The checks inside a bring-up stay in the core**, with what they use:
  * `apic/check.rs` and `iommu/check.rs`'s bring-up hooks name the item only
    for `crate::checks::run()`. The flag moves down: the core keeps it
    (`SKIPPED` and `run()` move to a core module), and the item's
    `checks::init` still parses `ferrix.checks` and sets it, a downward call
    as `sched::work::set_auditing` is today. `apic/check.rs` then needs nothing
    above the core and stays there whole.
  * `iommu/check.rs` is **split in two files**: what a bring-up calls
    (`check_firmware_left_on`, `plant_masked_byte`,
    `require_masked_byte_served`) and what needs nothing above the core stays
    in `iommu/check.rs`; `check_iommu` and its isolation-mark case
    (`check_isolation_mark`, the one function that drives a native process,
    through `object::check::Side`) move to a new `iommu/domain_check.rs`,
    raised to the load, and `iommu.rs`'s re-export of `check_iommu` moves to
    the load crate's `iommu` module. `check_dma_faults` and the gate run that
    `service_check.rs` calls (`run`) need nothing above the core and stay.

So the choice is **both, by kind**: a wrapper that only calls checks moves up
with them; a check a bring-up calls stays down, and a file holding both kinds
is split along that line. No product code calls a raised check after S1, and
the gate fails one that does.

### (4) The interface, and what only checks may use

**(a) The list.** Every core and item item a higher crate needs becomes `pub`
(the module path to it too); everything else stays `pub(crate)` or narrower.
Because nothing is `pub` today, the interface is exactly the `pub` items in
core and item files. `tools/common/gen/kernel-interface.py` lists them in
`docs/generated/KERNEL-INTERFACE.md` -- per crate, per module: the item, its
kind, its visibility before the split, the files above that name it, and
whether it is checks-only -- and `--check` in `cargo xtask check` fails when
the list and the tree disagree. A new `pub` in the core is then a visible line
in a diff, which a reviewer sees as a change to the item's interface. The
`pub(crate)`-to-`pub` edits are made by a script driven by rustc's own errors
(E0603, E0624, E0616: private item, method, field), one declaration at a time,
so nothing is raised that no upper crate uses; the script and its log are kept.
The about 230 such declarations without a `///` gain one that says what the
item does (`missing_docs` applies to `pub`).

**(b) Checks-only.** A raised check reaches internals of the module it checks
through `super::` today: on the resolver's count 18 private and 15
`pub(super)` items (`arch::x86_64::paranoid::{NMIS, USER_NMIS, BREAKPOINTS,
debug_hook, ...}`, `arch::armv7a::switch::{fpexc, set_vfp_record, ...}`,
`arch::x86_64::apic::IA32_APIC_BASE`, ...), plus the methods and fields the
compiler will name. They must become `pub` for a check in another crate to
reach them, and must not become API the load's product code calls. So:

* each such item is marked where it is declared with `#[doc(hidden)]` and the
  line `// CHECKS-ONLY: <the raised check file(s)>`, and
  `kernel-interface.py` lists it as checks-only;
* the boundary gate refuses any reference to a checks-only item from a product
  file (it resolves paths already), with a planted reference from a load
  product file as its negative control;
* what paths cannot show -- a checks-only **method** or **field** used from a
  product file -- is refused by clippy instead: the generated list writes the
  checks-only methods into the workspace's `clippy.toml` as
  `disallowed-methods`, and each raised check file carries one
  `#![allow(clippy::disallowed_methods, reason = "a raised check")]`; the gate
  refuses that `allow` anywhere but a raised check file and the module that
  defines the item. Fields are not lintable this way, so no checks-only field
  is made `pub`: a check that reads a private field gets a `#[doc(hidden)]`
  checks-only accessor function instead.

The expected size, to be confirmed by the compiler: about 35 checks-only items
and a few dozen methods. The design review gets the real list with S2's diff.

## 4. The steps, and (5) to (12)

Each step is a branch of its own with before and after figures, gated through
`gate.sh`, and reviewed by the consultant before `land.sh take`:

* **S1, inside one crate: the conflicts out.** The flag moves to the core, the
  wrappers move into their check files, `iommu/check.rs` splits. No crate yet;
  behaviour unchanged: the same boot lines on every architecture, traceability
  and the panic catalog regenerated, coverage carried.
* **S2, the tools learn four roots**, failing closed, before any code moves:
  `check-item-boundary.py` (four roots, (2), (3), the checks-only rule),
  `check-ferrocene-rust.py`'s package list, the kernel clippy step
  (`-p ferrix-kernel` and the three libraries, with a control that a lint in
  a core file fires), `check-crate-layering.sh` ((5): the core may depend on
  none of the other three, the item not on the load or the binary,
  `ferrix-boot` and every user crate on none of the four, with a
  `gate.sh control` that adds an item dependency to the core's manifest),
  `gate_rows.rs`'s `image_crates`, `gen-soup.py`, `workspace.rs`, and
  `decision-coverage.py`'s guard symbols (`ferrix_kernel::panic::explain`,
  `ferrix_kernel::trap::fatal` become `ferrix_kernel_core::`; the script fails
  when it finds no guard symbol at all). Each learns the new names while the
  old crate still exists, so its control can run on today's tree.
* **S3, the split.** The roots, the manifests, the inline modules, the
  visibility edits and `KERNEL-INTERFACE.md`. Shown unchanged, (7): the gate's
  per-ring file lists and line counts, the unsafe audit, `PANICS.md`,
  `TRACEABILITY.md`, `microkernel-surface.py`'s 401 items, function bodies
  byte for byte except the listed mechanical edits, and the boot lines of
  every architecture's boot. Regenerated, (8): `docs/generated/` (the
  architecture document and model), the SysML kernel structure, the SOUP list,
  `certification-item.json`'s `composition_root` and `crates` text, `ITEM.md`
  §2, `MICROKERNEL.md` M5, `docs/RELIABILITY.md` and the symbolizer's tests,
  which quote `ferrix_kernel::` names. Coverage, (9): the debug build changes
  as proposal 2's did -- with the coverage profile's crate-local ThinLTO now
  local to each crate, it no longer inlines across them -- so the coverage
  profile is re-measured on three architectures (the Arm QEMU row the
  coordinator holds comes first) and `coverage-x86_64-release.json` too,
  since fat LTO over four crates need not produce the same code. Release
  performance, (10): the IPC round trip on x86-64 release before and after by
  the `optimize-ipc-round-trip` protocol, the release boot's cost lines, and
  SPECULATION §8's debug GOT figure dated. Rows, (11): `check`; `test-boot`
  on x86-64 under KVM and TCG, AArch64, ARMv7-A, ARMv7-A with two processors,
  and release; `test-threads`; `test-container --arch all`; `test-btrfs
  x86_64 --accel kvm`. The landing cites ledger line 1477, (12).
* **S4, measured**: the edit figures of `docs/TEST-TIME.md`'s compile section
  again, for an edit in each ring, and the kernel clippy step.

Estimate: S1 3 points, S2 4, S3 10 to 15 (most of it the visibility edits'
review and the regenerated evidence), S4 1.

## Where it stands

* **Shape**: the certification consultant's design review, OK IF (1) to (12),
  ledger line 1477; this document's answers to (1) to (4) and the step order,
  OK IF (D1) to (D9), ledger line 1531. Each step's landing message cites both.
* **S1** (branch `bs-a-s1`; the consultant's code review OK IF (1) to (4),
  ledger line 1549, which also opened F-75 for the carry tool): the
  conflicts out, inside one crate. The checks
  flag is the core's (`boot_checks.rs`; `checks.rs` parses the option and
  sets it); the stage 3 and stage 9 wrappers moved into the check files they
  run, reached by one name through `arch/stage_check.rs`; `iommu/check.rs` is
  split, stage 10's domain and remapping check now `iommu/domain_check.rs`,
  and the pin budgets' ceiling is counted at the end of `iommu::report`, which
  `check_iommu` ran straight after. Product lines per ring: core 70,874 to
  70,706, item 9,417 to 9,418, load unchanged; check lines core 32,743 to
  32,984, item 4,281 to 4,275.
* **S2** (branch `bs-a-s2`; the consultant's code review OK IF (1) to (3),
  ledger line 1565, (2) owed before S3 lands): the tools know the kernel's crates, from one
  list, `kernel_crates` in `certification-item.json` (today the binary,
  holding all three rings), with `raised_checks` beside it (empty). The
  item-boundary gate walks every crate root's module tree and refuses a file
  reached by no crate or by two, a file in a crate that does not hold its ring
  but for the crate's root and a raised check, a stale or product raised
  entry, a checks-only item named from product code (directly or through a
  re-export), a path through a kernel crate's name that names nothing, an
  allow of clippy's `disallowed_methods` outside a raised check (and, per
  item, outside a checks-only item's module or without a reason), a package
  under `src/kernel` missing from the list, and a core or item kernel crate
  depending above its ring or without the workspace lints (D1, D2, D6's gate
  half, D7); each rule has fixture cases in its self-test, which fail when
  the rule is taken out. The Ferrocene check reads the list; the kernel's
  clippy step lints every package under `src/kernel`, which `workspace.rs`
  sorts as kernel, not host; `check-crate-layering.sh` keeps every rule's
  "the kernel" to all of the kernel's packages and orders the three library
  crates (inert until they exist); `decision-coverage.py` knows the guard
  symbols under any of the crates' names and refuses an image with neither.
* **S3a** (branch `bs-a-s3`; the consultant's code review OK IF (1), ledger
  line 1575): ledger 1565's (2), before the first
  CHECKS-ONLY marker: a marker the gate cannot place (no item after it, or
  inside braces: an inline module, an `impl`) fails the gate; the
  upward-reference rule is shown refusing a core file and an item file that
  name `ferrix_kernel_load::...` (the gate now also reads a path from the
  crates' root, `::ferrix_kernel_load::fs`, which it used to skip); and
  `check-crate-layering.sh` fails when a crate `kernel_crates` lists is not
  in the workspace instead of passing over its rules.
* **Where the raised architecture checks get their `cfg`** (ledger line
  1579): shape A, no exception to the layering rule. `kernel_load.rs`
  declares the positions without a conditional and without per-architecture
  re-exports (`pub mod arch { pub use ferrix_kernel_core::arch::*; pub mod
  x86_64 { pub mod trap { pub mod check; } ... } ... }`); each raised
  architecture check starts with `#![cfg(target_arch = "...")]` and reaches
  the core through `ferrix_kernel_core::arch::<isa>::...`, a mechanical edit
  S3 lists. An exception in `check-crate-layering.sh` for crate roots (shape
  B, conditions B1 to B5) only if shape A does not compile, with the error in
  the S3 commit.
* **S3 redo, assessed 2026-10-23** (linux-abi session; LINUX-ABI-SEPARATION.md
  §11.8 and ledger 3220 (20) put this split before the personality trait).
  S3 as built is on `bs-a-s3b` (also on origin): code review OK IF
  (S3.1)-(S3.8), ledger 1777 and 1795, S3.2a/b built, then stopped mid-rebase
  on 2026-10-15 (WIP 166cdb0e5 does not build), 440 commits behind with 83
  of its kernel files moved on `main`. It is not rebased; it is redone with
  its scripts in `~/ferrix-logs/bs-a/s3b/`. A dry run on `main` with
  `linux_abi` (a copy of `split.py` and `crates.py`, nothing kept) shows the
  recipe still works: `crates.py` maps 154 core, 20 item, 198 load files
  (was 138, 13, 193); `split.py` writes the three roots, and the core crate
  then fails on 23 errors only, all of `manual-edits.txt`'s kind (the
  console macros not yet `#[macro_export]`ed; one raised check new since,
  `arch/x86_64/framebuffer/check`). **New since S3, and a design question
  for the consultant first:** the build flags are now `cargo::rustc-cfg`
  from the kernel crate's build script alone (M0, BUILD-FLAGS rule 6 b),
  and 18 core files and 2 item files read a `ferrix_` cfg (the
  speculation, checks and `linux_abi` cfgs). Each of the three library
  crates needs the same flags, so the flag emission becomes one build
  script shared by the four kernel crates (the dry run used one, it
  builds), and `check-build-flags.py`'s rule 1 reads "the kernel's crates"
  for "the kernel crate". The `linux_abi` cfgs on module declarations move
  into the roots with them (`split.py` carries a declaration's
  attributes). *Guess*: 8 to 12 points: the shared flags script and its
  review, the raise loop and the manual edits again, the tools commits of
  `bs-a-s3b` rebased (their files moved little on `main`), the S3.2a/b
  registers regenerated, then (S3.1)'s full row set, the coverage floor's
  proof (S3.4), the Arm residuals (S3.6) and the quiet-host IPC run
  (S3.3). F-76 is still unfiled on `main` (the register goes to F-89 with
  76 skipped), so the number stays reserved.
* **S3** (first built on branch `bs-a-s3b`; the consultant's rulings during
  it, ledger lines 1585 and 1630; the code review, ledger line 1777, NOT YET
  on one point, narrowed at line 1795 and met, OK IF (S3.1)-(S3.8)): the
  split, §5. **Redone on branch `feature/kernel-split-s3`** (2026-10-23) on
  `main` 3a4a45e88 with that branch's scripts, then rebased onto `develop`
  92b8fbd17: the design reviews of what is new
  since are ledger lines 3282 (one build script for the four crates and
  the flag equality, OK IF (K1)-(K10)), 3294 and 3331 (the item's
  discovery policy handed to the core's device checks, OK IF (P1)-(P6));
  its code review, ledger line 3377, OK IF (1)-(9). What the redo changed
  against §5 is §5.1. The kernel lines: `src/kernel` +4,389 / -3,499 across
  227 files against `develop`, almost all visibility; new code is only the
  flag word macro with the crates' words and the binary's asserts,
  `Policy` and `DecodingRefusal`, and the three crate roots, while
  `arch/x86_64/cpu/stage4_check.rs` and `discovery/pci/held_off_check.rs`
  hold moved bodies.
* S4: the edit figures per ring are in `docs/TEST-TIME.md` with S3's; the
  kernel clippy step after an edit in each ring too.

## 5. S3 as built

**The crates.** `src/kernel/{core,item,load}/Cargo.toml`, each `[lib] path`
to its root beside `main.rs` (`kernel_core.rs`, `kernel_item.rs`,
`kernel_load.rs`, each classed in its ring); the binary keeps `main.rs` and
declares only `stages_check`. `kernel_crates` in the manifest says which
crate holds which ring; `raised_checks` lists the 26 check files of §2,
each with its own ring (`file_ring`) and the crate that builds it. The
roots declare their crate's top-level modules, and the mixed directories
by inline modules (§3 (1)): the core's root holds `discovery`'s six core
children, `syscall::uaccess` and `platform::google::gs201::watchdog`; the
load's root the raised checks and the load's children of `syscall` and
`platform`. An upper root and each of its inline modules re-export the
crate below with a glob, so `crate::mm::x` in a load file names what it
named before; an item `mod.rs` re-exports only core children
(`discovery`'s `board`, `description`, `fdt`, `finder` and `tree`, and
`syscall::uaccess`).

**Shape A for the architecture checks** (ledger line 1579): the load's root
declares `arch::{x86_64,aarch64,armv7a}::{trap,switch,paranoid}::check`
with no `cfg` and no re-export of an architecture's module; each of those
files starts with `#![cfg(target_arch = "..")]` and names the core through
`use ferrix_kernel_core::arch::<isa> as core_<isa>;` and
`use ferrix_kernel_core::arch::<isa>::<parent> as core_<parent>;`, its
`super::super::` and `super::` paths edited to those names (the listed
mechanical edits; doc comments that only mention `super::` kept). It
compiled; no exception to the layering rule was needed. The drivers of the
native call checks, `drive_native_words` and `drive_system_call`, are
re-exported per architecture by `arch/stage_check.rs` (a raised check under
`arch/`), and the load's root names them once, without a `cfg`: `pub use
stage_check::{drive_native_words, drive_system_call};` (ledger line 1585,
Q2). The core's `arch/mod.rs`, `aarch64/mod.rs`, `armv7a/mod.rs` and
`x86_64/mod.rs` no longer re-export them.

**Visibility, by the compiler's errors.** Nothing in the kernel was `pub`;
2,609 declarations are now, 2,362 of them `pub(crate)` before and 37
`pub(super)`, the rest private items the crates above use, the roots'
module lines and the re-export lines a split `use` list gained. Each was
raised by `~/ferrix-logs/bs-a/s3b/raise.py`, which reads one crate's
`cargo check` errors (E0603, E0624, E0616, E0451, E0364, E0365, the
private-type error and unresolved paths through a crate below), raises the
declaration the error names -- for a `use` list only the names asked for,
split into a `pub use` of their own -- and logs it with the file that
needed it (`raise-log.jsonl`, 2,470 entries, 43 reverted: a first pass read
the compiler's "consider importing" spans as definitions, which the log
marks); it ran on each crate and each architecture until it raised nothing.
The 15 items with no `///` that `missing_docs` then asked for are
documented; 17 types `missing_debug_implementations` asked for derive
`Debug`, two (`ConfigWrites`, `Stopped`) allow it with a reason; clippy's
lints for exported items (`new_without_default`, `len_without_is_empty`)
are allowed per item with a reason. Two things were not mechanical:
`syscall/stat.rs`'s inherent `impl StatLayout`, an enum the core's facade
defines, became a trait, `StatEncoding`, with the same bodies (Rust allows
an inherent `impl` only in the type's crate); and 4 `expect(dead_code)`
attributes on items now `pub` were removed, since `dead_code` does not look
at a `pub` item, and one `expect(clippy::unnecessary_wraps)`, which clippy
does not apply to exported functions.

**The interface** (§3 (4); ledger line 1531, D5): `tools/common/gen/kernel-interface.py`
lists every `pub` of a core or item file in
`docs/generated/KERNEL-INTERFACE.md` -- 2,290 declarations, 65 checks-only
-- with the crates above that name each, and `--check`, in `cargo xtask
check`, fails on a list that differs from the tree, on a `pub` no crate
above names and no named item's signature carries (a dead interface), and
on a `pub` item that is not checks-only naming a checks-only type or a
kernel trait implemented for one (ledger line 1630, F2.2).

**Checks-only** (§3 (4)(b)): 44 items -- free functions, statics,
constants and types made `pub` from private or `pub(super)` for a raised
check alone -- carry `// CHECKS-ONLY:` and `#[doc(hidden)]`; the gate
refuses a path to one from product code outside the item's own module.
21 methods and foreign functions of that kind carry `#[doc(hidden)]`, and
the generator writes them into `.clippy.toml`'s `disallowed-methods`; the
raised checks that call them allow the lint for the file, the defining
modules per function with a reason. Fields (ledger line 1630): a
`DeviceNode`'s six fields the configuration check reads, and the `wait` of
a `Pin` the pin check sets, stay private behind seven checks-only accessors
(`device::node_bars` and the rest, `object::pin::set_wait`; F1); the 11
fields of the nine checks-only types are `pub` (F2). Modules made `pub`
only for raised checks are not marked (F3); the layering rule keeps load
product code out of an architecture's own module. A product file naming a
raised check, or a check file of another crate, fails the gate (Q2.2,
F3.2), the composition root excepted.

**Narrower items and the register** (ledger lines 1777, 1795): an item that
was private or `pub(super)` before the split, is `pub` now and is named above
only by checks is CHECKS-ONLY with its home scope -- `// CHECKS-ONLY: ...;
home: super` for a `pub(super)` one, and `// CHECKS-ONLY-METHODS: home:
super` in a file for its methods -- so its old siblings still use it and
nothing else may (USER_SPSR, USER_CPSR, bring_up_vfp, six x86-64 and one
ARMv7-A `UserState` methods among them). The 483 declarations that were
`pub(crate)` before the split and are named above only by checks are listed
in `tools/common/data/kernel-formerly-crate-wide.json`: core and item
internals the load's checks reach, which load product code could reach
before the split exactly as now, so none is newly exposed (1531 C1's debt).
`kernel-interface.py --check` fails on a declaration named above only by
checks that is neither marked nor listed, and on a list entry that is no
longer pub, is now named by product code above, is named by no check, or
was narrower than `pub(crate)`; the list only shrinks.

**F-76** (ledger line 1585, Q1): the core's and the item's manifests name
25 libraries no ring classifies; each crate lists exactly those under
`unclassified_deps`, F-76's debt register, which the gate holds to the
manifest and to what the crate's files name.

**Shown unchanged against `main`** (line 1477 (7); `main` 814a95809 after
the rebase, S2's baseline 89a7cf74c for the boots): no function moved
between files, and of 8,281 function bodies 36 differ, every one in a
raised architecture check (the path edits above), in
`device/config_check.rs` or `object/pin/check.rs` (the accessors); the new
functions are the accessors. The boundary gate's three ring lists differ by
the three roots; its counts move by lines added and removed, not by code
moved: item product 94,954 to 95,109, core 70,706 to 70,950, load 73,057
to 73,244, upward references 0, composition-root edges 28, hooks 2. The unsafe audit's
output is byte-identical; the panic audit's 8 exemptions and the catalog
are the same; TRACEABILITY.md's statuses are main's, but for the accessor count (916 to
923, the seven accessors) and x86-64's re-measure below; the four boots' lines are the same but for the
two that vary from boot to boot (the thread hand-off's wording, the seam
trip's figures). The coverage evidence was carried with the line matching
blind to visibility (`~/ferrix-logs/bs-a/s3b/carry_vis.py`), and every
carried line checked to stay in its function (`span_check.py`, F-75).

**Release performance** (line 1477 (10)): the IPC round trip on x86-64
release, ABAB against 89a7cf74c, read 0.988 with a spread of 0.713 to 1.164
over 5 rounds on a busy host (load 18 to 37) -- inconclusive, and owed on a
quiet host (`docs/BACKLOG.md`); the release boot's cost lines are within the
load's noise.

**Coverage** (line 1477 (9)): x86-64's evidence is measured again on the
split, debug and release (`docs/certification/VERIFICATION.md` §3.1 and
§3.2): 143 x86-64 requirement statuses moved to *reached*, none away. The
Arm pair's is carried until an Arm QEMU loads the plugin (`docs/BACKLOG.md`). The x86-64 floor is 87.0 from this
measurement, with the statement-level comparison in
`docs/certification/VERIFICATION.md` §3.1.

### 5.1 The redo on `main` (2026-10-23)

`bs-a-s3b` stopped mid-rebase on 2026-10-15, 440 commits behind; the split
was redone on `main` 3a4a45e88 with its scripts (`~/ferrix-logs/bs-a/s3b/`:
`crates.py`, `split.py`, the raise loop `raise.py`, `carry_vis.py`,
`span_check.py`, `mkregister.py`), the tools commits of `bs-a-s3b` applied
three-way, and what `main` had grown since handled by the rulings below.
It was then rebased onto `develop`, last onto 92b8fbd17, which had
grown the kernel log's records (`console/log.rs`, `console.rs`,
`console/output.rs`, `console/log_check.rs`, `fs/kmsg.rs`,
`syscall/system.rs`), `ETXTBSY` (`syscall/exec.rs`, `syscall/process.rs`)
and `/proc/cpuinfo`'s identification readers (`arch/mod.rs`'s `cpuid`,
`user_extended_state` and `midr_of`). Each file went where its path's ring
puts it: the `console` files to the core, `fs/kmsg.rs` and the three
`syscall` files to the load; three conflicts, all visibility lines beside
new ones, were resolved by hand, and two more in `console/log.rs` and
`console/log_check.rs` on the last rebase (722824a8d's whole lines: two new
`Report` fields raised, one of them used above by a check alone and so in
the register). The raise loop then raised 23
declarations, all `pub(crate)` before and all named by load product code:
the kernel log's readers (`Line` and its fields, `line_from`,
`first_line`, `lines_begun`, `capacity`, `size_from`), `console::write_record`,
`fallible::try_format` and the three identification readers; `log::CAPACITY`,
`pub` since S3, went back to `pub(crate)` because nothing above the core
names it now. No check file moved, and none of the new code needed a
policy or an accessor.

**What is new against §5, each a non-mechanical edit (S3.7):**

* **One build script for the four crates** (ledger line 3282, (K1), (K2)).
  Since M0 the build flags are `cargo::rustc-cfg` from the kernel's build
  script, which reaches its own crate alone, and 18 core and 2 item files
  read a `ferrix_` cfg. `src/kernel/{core,item,load}/Cargo.toml` say
  `build = "../build.rs"`; `build.rs` makes the flags every kernel crate's
  `cfg`s, refuses any other package, and emits the link arguments and the
  link script's rerun for the binary alone. `check-item-boundary.py`'s
  `build_script_problems` (four self-test cases) fails a kernel crate with
  another script or none and any other package naming it;
  `check-build-flags.py` rule 1 reads "the kernel's crates".
* **The flag words** (K4): `kernel_core.rs`'s `build_flag_word!` lists the
  `cfg!` of every declared value (42), `check-build-flags.py` holds the list
  to the flag file (three self-test cases); each crate expands it into
  `CORE_FLAG_WORD`, `ITEM_FLAG_WORD`, `LOAD_FLAG_WORD`, and `main.rs`
  const-asserts each equal to its own word, naming the crate that differs.
* **The item's discovery policy handed to the core's device checks** (ledger
  lines 3294, 3331, (P1)-(P6)). Since S3's base the core's
  `device/check.rs`, which the core's `device::publish` runs, named
  `discovery::pci::decoding_refusal` and `discovery::firmware::{may_lend,
  refuses, KERNEL_MAPPING}`, the item's -- what stopped `bs-a-s3b`'s rebase.
  The checks stay where they are and run when they did; `device::check`
  gains `type DecodingRefusal` and `struct Policy` (four fields, no
  `Default`, no constructor), `publish` takes `policy: &check::Policy` and
  hands it to `check_together`, and the composition root binds
  `DISCOVERY_POLICY` to the four item paths. Body edits, all of them:
  `use crate::discovery::pci::decoding_refusal;` became `let
  decoding_refusal = policy.decoding_refusal;`, `use
  crate::discovery::firmware::refuses;` became `let refuses =
  policy.refuses;`, `crate::discovery::firmware::may_lend()` became
  `(policy.may_lend)()`, `KERNEL_MAPPING` became `policy.kernel_mapping`, and
  four signatures gained `policy`. `composition_root.policy` in the manifest
  records each binding, and `check-item-boundary.py`'s `policy_problems`
  (four self-test cases) fails another binding, a `Policy` built outside the
  composition root, an unrecorded field and a `Default`. The four calls
  through the policy are in `device/check.rs` alone; the item's own product
  code calls the functions directly.
* **`held_off_check` into a check file of its own** (P5): item product
  `discovery/pci.rs` called the core's check code (`check_held_off`,
  `HELD_OFF`), which S3's rule refuses across crates; the function moved,
  its body byte-identical, `fn` to `pub(super) fn` and its imports, into
  `discovery/pci/held_off_check.rs`, `pci.rs`'s child. `HELD_OFF` stays in
  the core's `device/check.rs`, and `check_vectors` fails stage 10 on it as
  before (P6).
* **x86-64's x87 and machine-check checks into the core** (3294 (L6)):
  `check_x87_controls` and `check_machine_check_enabled` with their statics
  moved byte-identically from the raised `trap/check.rs` and
  `paranoid/check.rs` into the new core check file
  `arch/x86_64/cpu/stage4_check.rs`, since `smp`'s stage 4 check, a core
  check, calls them. They name nothing above the core and no Linux, so the
  `linux_abi` cfgs the landing of 3a4a45e88 put on `smp/check.rs`, the x86-64
  wrappers and `arch/mod.rs`'s re-exports and stubs are REMOVED: both checks
  now run in `linux_abi=off` too.
* **x86-64's gate check into the core** (the rebase onto `develop`
  a2623409d, int3 and into through Linux's system gates): its
  `check_user_gates` (`L.x86_64.81`) reads the trap's private `IDT` and
  vector constants and runs no program, so it moved byte-identically from
  the raised `trap/check.rs` into the new core check file
  `arch/x86_64/trap/gate_check.rs`, the trap's child, where `super::IDT`
  still names the table and nothing in `trap.rs` is widened. The function
  is `pub`, `#[doc(hidden)]` and CHECKS-ONLY; the raised check calls it
  where it called it before, after its programs. `check_breakpoint` and
  `check_overflow` (`L.x86_64.209`, `.210`) run programs and stay raised.
* **The framebuffer window's check raised to the item** (L7), shape A:
  `arch/x86_64/framebuffer/check.rs` starts with
  `#![cfg(target_arch = "x86_64")]` and names the core through
  `core_x86_64`; `object/check.rs` reaches it by one name through
  `arch/stage_check.rs`, which holds the Arm no-op that `arch/mod.rs` held.
* **The `linux_abi=off` allow in each crate root**: the three library roots
  carry the binary's `cfg_attr(ferrix_linux_abi = "off", allow(..))` with
  its reason, since each crate is now compiled with the flag (3257 (3)).
* **Checks-only accessors** beside S3's (`node_bars`, `node_msi`,
  `node_msix`, `node_writable`, `node_dma_on`, `node_refused`, `set_wait`,
  taken from `bs-a-s3b`): `Domain::id` and `Domain::detach_translation` for
  the raised `iommu/domain_check.rs`, which since S3 reads them.

**The `linux_abi` cfgs** (3282 (K5)): every `cfg(ferrix_linux_abi = ..)` on
a module declaration moved with its declaration into the crate root that
now declares it (`split.py` carries a declaration's attributes): the load
root holds 82, those of the Linux personality's modules of `syscall` (load
files, which `syscall/mod.rs` declared and the load root's `syscall` module
declares now), of `fs`, `net`, `interfaces`, `service_check`, `stage_check`
and of every raised check; the statements' cfgs in `main.rs` are unchanged
(41, the three that declared `fs`, `net` and `interfaces` being the load
root's now); the ones on `smp/check.rs`, x86-64's wrappers and `arch/mod.rs`
are removed as above. Counted over every `ferrix_linux_abi` site in
`src/kernel` (`~/ferrix-logs/kernel-split-s3/linux-abi-cfgs.txt`, by
`abi_cfgs.py` beside it, against `develop` 92b8fbd17): 148 there, 141 here.
14 are removed: the six on the x87 and machine-check checks' path (above),
the six per-architecture `drive_*` re-exports that the load root's one line
replaces, and two `expect(clippy::unnecessary_wraps)` on the Arm check
wrappers that no longer apply. 7 are new: that load-root line, `main.rs`'s
`use ferrix_linux_abi as _`, the `off` allow in the three library roots and
the flag word's two `cfg!`. Every other one sits on the same declaration as
before; the core's and the item's roots declare no `linux_abi`-gated
module, so the `linux_abi=off` cut is the load root's and the binary's
alone. Its control is therefore planted in a load file: `use crate::fs`
in `platform/st/stm32mp1.rs`, refused `E0432` by the `off` clippy
(`s3r-s3r-ctl-k5-load`, ledger 3377). A plant in a core or item file
fails in `on` too and cannot show the cut, which is why `-k5-item` did not
fire; `-method` was replaced by `-method2`, the disallowed-method control on
the checks-only `config_writes`.
Clippy with `-D warnings` passes for `on` and `off` on all three
architectures over the four packages.

**The figures** (on `develop` 92b8fbd17). `crates.py` puts 157 files in the core's crate, 21 in the
item's, 201 in the load's and 2 in the binary (S3: 138, 13, 193, 2); 28
check files are raised (S3: 26; new are
`arch/x86_64/framebuffer/check.rs`, raised to the item, and
`user/alloc_check.rs`). The raise loop logged 2,753 declarations (2,578
`pub(crate)` before, 93 private, 37 `pub(super)`), of which 45 raised for a
version of the policy change that was dropped were reverted; the rebases
added 33 entries. `KERNEL-INTERFACE.md` lists 2,549 `pub` declarations of
the core and the item, 64 checks-only (2,551 and 65 after the rebase onto
a2623409d, with `gate_check.rs` a 158th core file); `kernel-formerly-crate-wide.json`
547 (S3: 483), the difference being what `main` and `develop` grew since.
`src/kernel`: +4,389 / -3,499 lines across 227 files against `develop`,
almost all visibility and declarations.

**The controls** (3282 (K6), 3331 (P2), 3294 (L3), 3377 (1), (2)), each
through `fleet/gate.sh control`, every one `FIRED`; the tags are the
landing's. On 5841bfa44, and carried to the landing by the consultant's
ruling at 3377 because each planted file and the gate code it triggers are
byte-identical there: `bsa-s3b`'s -twice, -none, -loaddep, -cycle,
-checksonly, -unclassified, -facade, -drive, -corelint, -cpsr, -spsr;
(K1) -k1-build, (K4) -k4-word, (K5) -k5-load, -method2, M0's -libcfg; and
the device criteria's (L.device.1, .35-.41, .43, .44, .46-.48)
-n150k2-decode-reserved, -h6-unasked-4, -h6-mark-4, -fbwc3-console,
-fbwc4-d, -fbwc4-hold10, -intelk3-yield, -intelk4-target, -intelk4r-copy,
-intelk4b-undo, -s10pci-verify2 and -yt2-a; the landing message maps each
criterion's control to its line. On the landing tree (`s3s-`), the rest
of those criteria's controls, from the landings that wrote them: `yt2-ctl-g`
(L.device.37), `h6-ctl-any-4`, `-contain-4`, `-index-4` (.38),
`-reaches-4`, `-pin-4`, `-hold-4` (.39), `-yield-4` (.40) and
`intelk4b-ctl-option` (.46); P2's lend binding (host),
the x87 controls' CR0.NE (`cr0ne-ctl-a`) and CR4.MCE (`m0f73-ctl-nomce-r4`)
now in the core's `stage4_check.rs`, and four the criteria name that had no
recorded firing: L.device.35's table still withheld (`l35-table`) and MSI
vector not offered (`l35-msi-b`), which `check_held_off`'s offer test
catches first, as "a held-off function offered its MSI-X table", since a
node with no MSI has no vector; L.device.38's aperture minted not
prefetchable (`l38-prefetch-b`), which `check_lending`'s exact-aperture
test catches first, as "the framebuffer was not lent exactly its visible
lines", before `check_combining` reads the type; and L.device.47's refusal
dropped (`l47-refusal`).

**The release build, measured** (1477 (10), 3377 (6)): `test-boot --arch
all --release` on the landing tree (`s3s-relboot`) and on `develop`
49bdf0d73 (`s3s-dev-relboot`), one boot each, both `run: PASSED`, every
boot line of the same shape on all three architectures and the build line
equal. x86-64's process cost line, `develop` against the split, ms per
check: `handlers` 666 / 659, `fork` 269 / 272, `signals` 49 / 68,
`execve` 35 / 35, `futex` 1,185 / 1,186; the AArch64 and ARMv7-A lines move
by as much either way (`~/ferrix-logs/kernel-split-s3/bootcmp-release.txt`).
One boot under a shared host's load, within its noise. The GOT counts are
in `docs/certification/SPECULATION.md` §8: the release kernel makes 5,524
calls through it against 5,544, the debug kernel 77,756 against 35,413.
The IPC round trip on a quiet host is owed (`docs/backlog/p2.md`).

**What an edit costs, measured** (x86-64 dev build of the binary and,
for clippy, of the four packages; `main` 3a4a45e88 against this branch,
alternating, host load 1 to 3, user CPU seconds; three rounds for the build,
two for clippy):

| Edit in | Build, before | Build, after | Clippy, before | Clippy, after |
|---|---:|---:|---:|---:|
| the load (`syscall/eventfd.rs`) | 2.3-2.4 | 1.6 | 5.6-6.2 | 4.0-4.3 |
| the item (`random.rs`) | 2.6-3.4 | 1.5 | 5.6-6.0 | 4.2-4.6 |
| the core (`smp.rs`) | 2.0-3.0 | 2.2-4.5 | 5.7-6.0 | 5.7-5.8 |

An edit above the core rebuilds less: about a third less CPU for the build
and a quarter less for clippy. An edit in the core costs what it did, since
every crate above it rebuilds. The release build, which the gates run,
links with fat LTO over the four crates and is not faster.

**Coverage** (1477 (9), 1777 (S3.4)-(S3.6)): x86-64 re-measured on the
redo, debug (`coverage` profile) and release, `docs/certification/VERIFICATION.md`
§3.1 and §3.2: debug, the certified item 85.2% of 13,080 statements, 202
requirement statuses to *reached*, none away; release, 83.4% of 8,819, below
the debug floor the release run is also held to, its residual not yet
argued (`docs/backlog/p2.md`); the floor 84.0 with the statement proof
(28 statements reached on 2026-09-27 and not now, all timed or machine
paths). AArch64 and ARMv7-A are carried, not measured, until an Arm QEMU
loads the plugin (`docs/backlog/p2.md`).

## 6. Removable crates: a feature whose code a build can leave out

**Built 2026-10-24 (settings-crate).** The certification consultant's
rulings: ledger line 3558 (run-time settings per profile, (5) to (14)),
amended by 3595 (crash capture its own feature, `crash_capture`, crate
`ferrix-kernel-crash`); 3564 (fast's signed configuration, a later
`config_updates` key, not built); and this design's review, **OK IF
(R1)-(R21), ledger line 3623**. This section is the pattern both crates
follow, and the first one, `ferrix-kernel-settings`, built with it.
`BUILD-FLAGS.md` §3 has the flag and the image key, §5.1 the image
manifest.

### 6.1 The pattern

A **removable crate** is a kernel library crate in the load ring that a
build flag puts in or leaves out of the kernel binary. It is how a flag
value that means "this code is absent" (3558 (5), Q2: absent code is
neither dead nor deactivated, DO-178C §6.4.4.3c/d) is realised, and the
flag is the configuration record of it (3558 addendum (a)).

1. **The crate depends on `ferrix-kernel-load`, never the reverse** (and
   on the core, for the flag word). No kernel crate but the binary lists
   it, and only the binary's root names it (3558 (9)). Its root is
   `src/kernel/src/kernel_<name>.rs`, its files under
   `src/kernel/src/<name>/`, both in the load ring of
   `certification-item.json`; it is a fifth entry of `kernel_crates`,
   builds with the one build script (`src/kernel/build.rs`), and its flag
   word is const-asserted equal to the binary's (K4). It defines no
   `#[no_mangle]`, `export_name`, `#[used]` or `#[link_section]` item and no
   link-time hook (3623 (R2)), so a link that does not name it keeps
   nothing of it.
2. **Load defines one small typed interface**: a trait, generated with
   everything its binding needs by `seam!` (`seam.rs`) from one list in a
   load file of its own (`setters.rs`: trait `Setters`, twelve entries, each
   a name, its symbol `__ferrix_setters_<entry>` and its signature), with the
   load's implementation `Absent` beside it, written against the trait. The
   load's call sites call the module's function of an entry,
   `setters::<entry>(..)`, whose one `unsafe` block calls the symbol; the
   symbols are declared `extern "Rust"` and defined only by the binding
   macro (§6.3, ledger 3934).
3. **The composition root binds it** (`main.rs`), once, statically:
   ```rust
   #[cfg(all(ferrix_linux_abi = "on", ferrix_runtime_settings = "live"))]
   ferrix_kernel_load::bind_setters!(ferrix_kernel_settings::Live);
   #[cfg(all(ferrix_linux_abi = "on", ferrix_runtime_settings = "fixed"))]
   ferrix_kernel_load::bind_setters!(setters::Absent);
   ```
   Each invocation defines every symbol as a `#[unsafe(no_mangle)]`,
   out-of-line Rust-ABI function calling its type's implementation, from the
   same tokens as the declarations, so the two cannot differ. There is no
   run-time registration: a root with no invocation leaves the symbols
   undefined and one with two defines them twice, and neither links -- the
   registration check is the linker's, which closes ledger 3762 (2). The
   crate's flag-word assert is under `cfg(ferrix_runtime_settings = "live")`
   too. Under `fixed` the binary names the crate nowhere, so rustc never
   loads it and the linker never sees it; the crate-level
   `#![cfg_attr(ferrix_runtime_settings = "fixed",
   expect(unused_crate_dependencies, ..))]` is the one lint attribute that
   build needs (3623 (R3)): `expect` is fulfilled there, and `live` builds
   keep the lint under `-D warnings`.
4. **The manifest records it**: `composition_root.removable`, one entry
   per crate: `flag`, `present_when`, `interface`, `defined_in`, `absent`,
   `binding` (one type per value of the flag), `bind_macro`, `methods`,
   `seam_symbols`, `calls` (the load files calling through the interface),
   `lint_expect`, `symbol` and `absent_symbols` (the load's mechanisms only
   the crate calls).
   `ferrix-kernel-crash` adds its own entry (`crash_capture`, `hold`, its
   `CrashHook`, ...) and a line in xtask's `REMOVABLE`, and no new check code.
5. **The checks are generic over that record** (3558 (11), 3595 (C4)):
   * `check-item-boundary.py`'s `removable_problems` (eleven self-test
     cases): the crate depends on the load and only the binary on it, the
     binary's manifest lists it; no kernel file but the root names it; the
     crate exports nothing a link would keep; every reference to it in the
     root sits under `cfg(ferrix_<flag> = "<present_when>")`; the
     `lint_expect` attribute is there; each value of the flag has exactly
     one invocation of the binding macro, of the recorded type; the seam's
     list has exactly the recorded entries and symbols; only `Absent` in
     `defined_in` and the crate's type implement the trait; the files
     calling through it are the recorded ones; and every profile file,
     buildable or not, is refused if its value is not `present_when` and
     its binary would name the crate, or if it is and would not. And
     `seam_scan_problems` (four self-test cases) over every Rust file of
     every path crate `ferrix-kernel` reaches in the cargo metadata graph,
     `src/lib` included (3934 (2), (3)): no file defines a `__ferrix_`
     symbol by name -- `no_mangle`, `export_name` or an assembly `.globl` --
     and none but the seam's own list and macro names one.
   * **The ELF is the dependency evidence of record** (3623 (R1)): Cargo
     lists the crate in every build, since it has no way to make a
     dependency conditional on a build-script `--cfg` without a feature,
     which (10) excludes; whether a profile's binary depends on it is
     defined by the root's cfgs, checked statically per profile above, and
     proved by the linked ELF. `check-removable-symbols.py --elf K --crate
     C --value V` (four self-test cases) reads `nm --demangle`: no symbol
     of the crate, nor any of `absent_symbols`, in a kernel built with an
     absent value; at least one of the crate's in one built with the
     present value. And each of the record's `seam_symbols` is defined
     exactly once, and in a present build each one's code calls into the
     crate (its implementation's methods are out of line for this; llvm's
     objdump reads every architecture's ELF) (3934 (4)). `cargo xtask test-removable` links the release kernel
     with each value on the three architectures, keeps each ELF under
     `target/removable/` and runs the check on it.
   * `check-build-flags.py` rule 6e (one self-test case): a cfg of a
     recorded flag anywhere under `src/kernel` but `main.rs`,
     `build_flags.rs`'s table and `kernel_core.rs`'s flag word is refused.
   * The negative controls, each through `fleet/gate.sh control --expect`
     and each to read `FIRED`, are listed with their logs in the landing
     message: (a) a planted dependency, `use ferrix_kernel_settings as _;`
     unconditional in `main.rs`; (b) a planted reference pulling the crate
     into a fixed link, the ELF check; (c) a planted
     `cfg(ferrix_runtime_settings = "live")` in a load file, rule 6e; (d)
     the live binding bound to `ABSENT`; (e) `runtime_settings = "live"` in
     `max.toml`, the profile rule, and `settings_writer = "present"` in
     `fast.toml`, the image-key rule; (f) a planted `settingsd` in a
     `settings_writer = "absent"` image, xtask's refusal; (g) a manifest
     line that disagrees with the profile, `check-boot-line.py`; (h) a
     planted `#[unsafe(no_mangle)]` item in the crate; (i) a live build
     with the registration removed, the boot (the first boot check that
     sets something through `bound()`, stage 8's `memfd_noexec` write, is
     then refused `EPERM` and stops it); (j) one `ABSENT` field answering `Ok`, the stage
     check below (3623 (R20)).
6. **Cargo features stay at zero** (`kernel_cargo_features`, (10)), and
   there is no second binary package.

### 6.3 Design: the seams as traits, chosen statically (for review, 2026-10-24)

**The customer's decision, 2026-10-24**: a composition-root seam -- where
one implementation of an interface is chosen for a build -- is a trait,
and the binary's root chooses its implementation statically: no `dyn`, no
table of function pointers, no run-time registration. Not every `pub`
function becomes a trait: only the seams where an implementation is
chosen (removable crates, policies, platform hooks). Three are in scope:
`setters::Setters` (§6.1, landed ceb8d678c), `device::check::Policy`
(ledger 3294), and the coming `CrashHook` of `ferrix-kernel-crash` (ledger
3595), which is built on this from the start. No behaviour changes: the
same answers byte for byte, the same boot lines.

**Two shapes, by who names the implementation.**

1. **A caller that names it: a generic.** `device::publish` is the core's;
   its caller, the binary's `publish_devices`, names the item's policy. So
   the core gains `pub trait Policy` (in `device/check.rs`, where the
   struct is): `fn decoding_refusal(..) -> Option<&'static str>`, `fn
   may_lend() -> bool`, `fn refuses(..) -> bool`, `const KERNEL_MAPPING`,
   associated functions with no `self`. `publish<P: Policy>` and the
   checks it calls take `P` as a type parameter, and `main.rs` has `struct
   Discovery;` with `impl device::check::Policy for Discovery`, forwarding
   to `pci::decoding_refusal` and `firmware`'s three, and calls
   `device::publish::<Discovery>(..)`. The core still names nothing of the
   item; the monomorphised `publish` is in the binary. The struct, its
   const `DISCOVERY_POLICY` and the `&Policy` arguments go.

2. **A caller that cannot name it: a link-time binding.** The load's call
   sites reach `Setters` (and `CrashHook`), and the load is compiled before
   the binary that chooses, so it can neither name the crate's type nor be
   made generic up to its system-call tables (fixed `fn` items). Here the
   load defines, in `setters.rs`:
   * `pub trait Setters` with the twelve entries as associated functions
     (`fn sethostname(p: &Process, name: u64, len: i32) -> Result<usize,
     Errno>`, ...), and `pub struct Absent;` with `impl Setters for
     Absent`, each answering `EPERM` as `ABSENT`'s fields do today;
   * one `unsafe extern "Rust"` declaration a method,
     `__ferrix_setters_<entry>`, with the method's signature, and one safe
     load function a method, `setters::<entry>(..)`, which calls it: the
     call sites call these, one call a site, as they call through `bound()`
     now;
   * `#[macro_export] macro_rules! bind_setters { ($chosen:ty) => { .. } }`,
     which defines each `__ferrix_setters_<entry>` as a
     `#[unsafe(no_mangle)]` Rust-ABI function calling `<$chosen as
     Setters>::<entry>`.

   The root invokes it once a flag value, under that value's cfg:
   ```rust
   #[cfg(all(ferrix_linux_abi = "on", ferrix_runtime_settings = "live"))]
   ferrix_kernel_load::bind_setters!(ferrix_kernel_settings::Live);
   #[cfg(all(ferrix_linux_abi = "on", ferrix_runtime_settings = "fixed"))]
   ferrix_kernel_load::bind_setters!(ferrix_kernel_load::setters::Absent);
   ```
   Every call is a direct call the linker resolves to the chosen
   implementation's function: static dispatch, nothing chosen at run time.
   The crate's `SETTERS` static and the load's `ABSENT` static, `bind`
   and `bound` go; `ENTRIES` stays, generated by `seam!` with the rest
   (the entries' count, which the absent setters' check holds its calls
   to); the crate gains `pub struct Live;` and its
   `impl Setters for Live`, forwarding to the moved entries, which stay as
   they are. The `#[unsafe(no_mangle)]` items are the binary root's, from
   the load's macro, never the crate's: 3623 (R2) stands (the crate exports
   nothing a link would keep).

**The registration check becomes the linker's, which closes ledger 3762
(2).** Today `bind_setters` checks `bound()`'s identity inside itself, so a
root that never calls it is caught only through `bound()`'s users. With the
binding at link time there is no run-time bind to forget: a root that
invokes no `bind_setters!` leaves `__ferrix_setters_<entry>` undefined and
the kernel does not link; one that invokes it twice defines each twice, and
does not link either. The SAFETY argument of each `unsafe` call is that the
macro defines the symbol with the declaration's own signature, from the
trait's method; a check (`removable_problems`) holds that the declaration
list and the trait's methods are the same names and that the macro is the
only definer of `__ferrix_setters_*` (no other `no_mangle` of that prefix
anywhere under `src/kernel`).

**`CrashHook`** (3595 (C2)): the same shape. The load's `crash_hook.rs` has
`pub trait CrashHook` with the three calls 3595 names -- the fault's hold
(`hold(thread, signal, code, regs) -> bool`), a held process's end
(`held_status(process) -> Option<u32>`), the held park
(`parked(process, thread)`) -- and `pub struct Absent;` ("never held":
`false`, `None`, nothing), `bind_crash_hook!`, and the root's two
invocations under `cfg(ferrix_crash_capture = "hold")` / `"absent"`. The
three call sites stay in the load (`syscall/deliver.rs`,
`syscall/process.rs`), each listed in `composition_root.removable`.

**The checks, generic as §6.1's, adapted:**

| Check | Today | With traits |
|---|---|---|
| the interface's shape (`removable_problems`) | the struct has exactly the recorded fields, `ENTRIES` their count, no `Default`, no value built outside the crate and `defined_in` | the trait has exactly the recorded methods; the extern declarations are the same names; `Absent` implements it in the load and the crate's type in the crate; no other type implements it |
| one binding a flag value | each value's `const SETTERS` binds the recorded path | each value's cfg holds exactly one `bind_<name>!` invocation, of the recorded type |
| the registration | `bind_setters` checks `bound()` by address at boot | the link: none or two invocations do not link (control (i) becomes "the invocation removed: the build fails to link", FIRED on the linker's `undefined symbol: __ferrix_setters_sethostname`) |
| the ELF (`check-removable-symbols.py`) | no crate symbol in an absent build | unchanged; and every `__ferrix_setters_*` symbol present in every build, the absent one's calling the load's `Absent` |
| the boot | stage 8's `memfd_noexec` write through `bound()` | unchanged: a `fixed` build's `EPERM`, a `live` build's answer, through `setters::<entry>` |
| `kernel-interface.py` | lists the struct's fields | lists the trait, its methods and the macro; the removed `bind`/`bound`/statics drop out |

The policy's checks (ledger 3294) are unchanged: `check_decoding_refusals`
and `check_lent_framebuffer` hold the nodes to `P`'s answers instead of the
struct's fields, and their controls run as before.

**What is not changed**: the core's `trap::ReturnPath`, the personality's
return-path hooks the load registers at run time, is a seam too, but
outside this decision's scope until the consultant says otherwise; the
ordinary core and item `pub` functions stay functions.

**Reviewed**: OK IF (1)-(9), ledger line 3934 (verdict 3936). The link-time
binding is sound evidence of static selection under (1)-(5): one load
macro generates the trait's methods, the extern declarations, the safe
wrappers and `bind_*!`'s definitions from one signature list (a mismatch
across crates would link and be undefined behaviour); the one `unsafe` is
each wrapper's call; the scan for definers of the `__ferrix_` prefixes
covers every crate the kernel links, `src/lib` included, and
`export_name` and assembly symbols too; the ELF check shows each symbol
defined once, reaching the chosen type; and the two link controls, the
invocation removed and made twice, read `FIRED`. `trap::ReturnPath` does not
join now: it is a core interface whose `fault_signal` 3570 (2) changes
under its own requirement, so it comes later and alone, after `CrashHook`,
with its own review (`docs/BACKLOG.md`).

**Code review**: OK IF (1)-(4), ledger line 4406 (heading 4404), for the
landing of Setters and Policy: (1)-(8) met in the code; the rows and
controls run on 84b80b931 count for the rebased commit, whose rebase
conflicted only on derived evidence pages, so check re-ran; boot lines
need no base runs, since the change adds and alters no output statement
(it removes only `bind_setters`'s two never-reached `fatal!` calls,
whose job the link now does); `CrashHook` (9) follows in its own landing.

**Order**: this design, reviewed; then Setters and Policy ported and landed
(no behaviour change; rows: check, test-boot on the four boots, test-removable,
test-init; the controls (a)-(j) of §6.1 re-run, (i) as the link); then
`ferrix-kernel-crash` on `CrashHook`, with 3595's conditions.

### 6.2 `ferrix-kernel-settings`: what moved

The rule: **the entry by which a program changes a system setting,
whole** -- the system call's handler or the file's write function, from
its first argument check to its last effect -- so that in a `fixed` build
the call answers `EPERM` at its dispatch, before any argument check, and
none of the entry's code is in the binary (3623 (R5), (R10)). Kernel
lines are on the landing tree against `develop` 6a60444a6, with doc
comments; "code" counts lines that are neither blank nor comment (3623
(R12)).

| From (load) | What moved | To (`settings/`) | Lines (code) | Setters field | Live answers, where `fixed` answers `EPERM` first |
|---|---|---|---:|---|---|
| `syscall/system.rs` | `read_name`, `nameable`, `sys_sethostname`, `sys_setdomainname`, whole | `uts.rs` | 54 (34) | `sethostname`, `setdomainname` | `EPERM` without `CAP_SYS_ADMIN`, `EINVAL`, `EFAULT` |
| `syscall/system.rs` | `sys_syslog`'s `CLEAR` (5) arm, and `READ_CLEAR` (4) whole: its buffer check, the read and the clear `read_all` did under its `clear` argument | `log.rs` | 29 (17) | `syslog_clear`, `syslog_read_clear` | after the call's privilege check: `EINVAL`, `EFAULT`, `ENOMEM` for 4 |
| `syscall/system.rs` | `sys_reboot`'s `RESTART2` arm, whole, with the commits and the audit record it made before the `match` | `reboot.rs` | 42 (26) | `restart_with_word` | after the privilege check and the magic numbers: `EFAULT`, else the reset |
| `syscall/time.rs` | `SETTOD_SECONDS_MAX`, `settable`, `sys_clock_settime`, `sys_settimeofday`, whole | `clock.rs` | 89 (62) | `clock_settime`, `settimeofday` | `EINVAL`, `EFAULT`, then `EPERM` without privilege |
| `fs/procfs.rs` | `sysrq_trigger`, whole | `sysrq.rs` | 23 (12) | `sysrq_trigger` | the panic on `c` |
| `fs/procfs/render.rs` | `set_hostname`, `set_domainname`, whole | `sysctl.rs` | 28 (17) | `sysctl_hostname`, `sysctl_domainname` | `EINVAL` |
| `syscall/memfd.rs` | `set_noexec`, whole | `memfd_noexec.rs` | 40 (19) | `sysctl_memfd_noexec` | `EPERM` without `CAP_SYS_ADMIN`, `EINVAL` |
| `fs/sysfs.rs` | `write`'s checks and requests: the name, the device, the unbind's driver, `devmgr::request` | `driver_bind.rs` | 30 (17) | `driver_bind` | `ENODEV`, `devmgr`'s answer |
| (new, `docs/CORE-QUARANTINE.md` §4) | a write to `cpuN/online`: the privilege check, `0` or `1`, `smp::service::set_in_service` | `cpu_online.rs` | 50 (32) | `cpu_online` | `EPERM` without `CAP_SYS_ADMIN` in the first user namespace, `EINVAL`, `EBUSY`, `ETIMEDOUT` |

**The bodies.** `uts.rs`, `clock.rs`, `sysrq.rs`, `sysctl.rs` and
`memfd_noexec.rs` hold their functions byte-identical to the removed
lines but for `pub(crate)`/`pub(super)`/private to `pub` on the entry and
`crate::fs::timerfd` to `ferrix_kernel_load::fs::timerfd` (the landing
lists the script diff, 3623 (R21)). Three bodies changed shape, each said
here: `log.rs` is new code around `read_all`, whose `clear` argument went
(it now returns the next line's number, which `READ_CLEAR` clears to, and
the clear mark's start when nothing was read, where a clear changes
nothing); `reboot.rs` is the `RESTART2` arm byte-identical behind the
three lines of commits and audit record `sys_reboot` made before its
`match`, called after the privilege check and the magic numbers, so in
`live` the commits, the record and the printed lines come in today's
order, and a refused `fixed` call records nothing (3623 (R7));
`driver_bind.rs` takes `(bus, driver, unbind, data)` where `write` took
sysfs's private `Dir` and `Attr`, and `sysfs.rs` keeps a five-line `write`
that matches a driver's `bind` or `unbind` and calls the entry (anything
else stays `EACCES`). The load files keep one-line shims where a table
needs a function at compile time (`procfs.rs`'s `sysrq_trigger`,
`render.rs`'s `set_hostname`, `set_domainname`, and `set_memfd_noexec`,
which still finds the writer and answers `EPERM` without one).

**The load's side** (product): `setters.rs` 111 lines (49 code); call
sites in `system.rs` (+25 / -88 with the moves), `time.rs` (+10 / -87),
`procfs.rs` (+4 / -17), `render.rs` (+14 / -22), `sysfs.rs` (+5 / -21),
`memfd.rs` (+1 / -33), `kernel_load.rs` (+2). Raised from `pub(crate)` or
private to `pub` because the crate names them (no body changed): in
`syscall/system.rs` `NAME_MAX`, `SetName`, `UtsNamespace` with its
`owner`, `set_hostname` and `set_domainname`, `CLEARED`, `user_buffer`,
`read_all`; in `syscall/time.rs` `TimeWidth`, `nanos_of`, `read_pair`,
`set_realtime` (now documented); `syscall/process.rs`'s `Process`,
`with_credentials`, `nsproxy`, `numbers`; `syscall/credentials.rs`'s
`Credentials`, `holds`, `require_privilege`; `syscall/nsproxy.rs`'s
`NsProxy`, its `uts`, `acting`; `syscall/pidns.rs`'s `PidNamespace`,
`Numbers`, `namespace`, `memfd_noexec_floor`, `set_memfd_noexec`;
`syscall/userns.rs`'s `UserNamespace`, `CAP_SYS_ADMIN`, `capable_over`;
`syscall/memfd.rs`'s `SCOPE_NOEXEC_ENFORCED`; `fs/procfs.rs`'s `Kernel`;
`fs/sysfs.rs`'s `device_named`; `fs/mod.rs`'s `timerfd` module and its
`clock_was_set`. The item: `main.rs` +52 (the binding, `bind_setters`, the
assert, the lint attribute) and `build_flags.rs` +8 (one `Flag`); the
core: `kernel_core.rs` +2 (the flag word's two values). The crate's root
is 74 lines (45 code).

**What stays in the load, and who still calls it in a `fixed` binary**
(3623 (R6)): `UtsNamespace::set_hostname` and `set_domainname`, and
`timerfd::clock_was_set`, have no caller there and are `absent_symbols`
of the ELF check; the `CLEARED` mark is read by `/dev/kmsg` and written
by nothing; `pidns::set_memfd_noexec` keeps `fs/memfd_check.rs`'s boot
check as its caller; `set_realtime` keeps the boot's reading of the
real-time clock; `devmgr::request` is `devmgr`'s own (item);
`power::request_boot_mode` and the stm32mp1 TAMP write keep
`service_check.rs`'s check of a word no board knows.

**The boot checks that call a moved entry** go through the setters
module's wrappers (`setters::<entry>(..)`): `syscall/check.rs`'s `clock_settime` and
`sethostname` checks and `fs/memfd_check.rs`'s `memfd_noexec` write
(3623 (R8)); `fs/smallns_check.rs`'s programs set a hostname by the
system call, so through the dispatch. **Statement settings-1** (a load
statement in LOGGING.md §21.7's form, 3623 (R16)): *every entry of
`setters::Absent` answers `EPERM` without reading an argument*; boot
sub-case `syscall/check.rs`'s `check_absent_setters_refuse`, which calls
each `<Absent as Setters>` entry directly in every build and requires as
many answers as `seam!`'s `ENTRIES`;
control (j). "A fixed kernel contains no setting entry" is a
configuration statement the ELF check proves, not a requirement, and no
`L` id is reserved: ids come only when a fixed profile claims it in the
Security Target as FMT_MOF/FMT_SMF (3623 (R16)).

**Checked against 3558's list and (8)** (3623 (R9)-(R11)):

* **`adjtimex`, `clock_adjtime`: nothing moved.** They set nothing: every
  adjustment is refused `EPERM` in every profile (`time.rs` `adjustment`).
* **cgroupfs: does not move in this landing, and is a residual.**
  cgroupfs writes set a kernel `Job`'s own `cpu.max`, `memory.max` and
  `pids.max`, so in a `fixed` build whose kernel cgroupfs is reachable
  root could change a job's budget at run time. The kernel cannot tell
  init applying the image's budgets at a unit's start, or a delegated
  subdivision ((8)), from that without a new mechanism (12). A claimed
  fixed profile that reaches the kernel's cgroupfs needs a design review
  of budget sealing first (`docs/backlog/p2.md`).
* **sysfs `bind`/`unbind`: moved.** `devmgr`'s own matching stays ((8)).
* **A child UTS or pid namespace's own name or scope moved too**: a
  sandbox in a `fixed` build keeps its parent's names, and
  `bwrap --hostname` is refused. That loses nothing a fixed image needs
  but is a hardening loss for a sandbox that wanted its own name.
* **How a fixed image's own values reach the kernel** without the entries
  (its hostname, domainname, `memfd_noexec` scope, wall clock): boot words
  from the verified image, `set_realtime` at boot or DeviceClock for the
  time, or linuxd in M5. Owed before a fixed profile is buildable
  (`docs/backlog/p2.md`; max and fast have `linux_abi=on`).
* **`/dev/ferrix-crashes` and the crash hold**: `ferrix-kernel-crash`'s
  (3595 (C2)), not this crate's.
* **powerctl is still in a `fixed` kernel at this landing** (3623 (R13)),
  deactivated by `ferrix.powerctl`, which no profile permits
  (`POWERCTL.md` §5 states the mechanism). Its parts are core
  (`arch/x86_64/powerctl.rs`, 920 lines, and `arch/mod.rs`'s wrappers) and
  item (`discovery/powerctl.rs`, 55; `syscall/native.rs`'s `power_call`,
  30; `main.rs`'s finder). **Step 2**, under its own boundary-move design
  review before code: the item's `power_call` and the finder go through
  native fields of `Setters` whose `ABSENT` answers `NOT_SUPPORTED`.
* **linuxd's matching setters in `max`** are M5's (3558 (5)): noted, not
  built.

A `fixed` build is link-only (`test-removable`) until a `fixed` profile is
buildable; its boot owes the boot checks above their `EPERM`
expectations (`docs/backlog/p2.md`, 3623 (R14)).
