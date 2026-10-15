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
* S3 and S4: not started. D5 (`KERNEL-INTERFACE.md`) and the
  `disallowed-methods` list come with S3, which makes the first `pub`.
