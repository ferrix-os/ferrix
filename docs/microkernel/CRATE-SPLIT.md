# The crate split: optional kernel parts out of `src/kernel/`

**Design, 2026-11-05 (mk-crates, branch `feature/mk-crates`). Docs only: no
file moves yet. Reviewed: OK IF (1)-(12), ledger 7346, folded in** (the
conditions are cited inline as 7346 (n)). The customer: "now that our kernel is a lot leaner can we
move out the parts that are not part of the core kernel out of src/kernel/?
Maybe as a subfolder of lib/kernel?" and "ideally these parts that are
optional shall be standalone crates". Three agents (mk-flip3, mk-boot-time2,
mk-issues2) are editing these files, so every move here is done later in one
short freeze per landing (§4), after mk-flip3's `fast` has landed. S0 to S5
move what moves without a new seam and empty `src/kernel/` of optional code;
P1 to P3 then cut the Linux personality into parts, each behind a seam of its
own with its own design review; §2.6 names what blocks each cut and what
unblocks it.

It builds on: `docs/KERNEL-CRATES.md` (the four crates, §6's removable-crate
pattern and §6.3's static seams), `docs/MICROKERNEL.md` §2.1 (what stays in
ring 0), §4 (the lift) and §12 (one source, several kernels),
`docs/microkernel/M5-LIFT.md` §1 (the load built twice, by position),
`docs/BUILD-FLAGS.md` and `tools/common/data/certification-item.json`. Its
rules are the customer's: **one source, compile-time flags only, `desktop`
stays hybrid, no forks**; and the 2026-10-24 rule that a seam where a build
chooses an implementation is a trait with static dispatch, bound at the
composition root.

## 1. Today's map

Measured on develop b6a737ea4 (lines with `wc -l`, rings with the item's
globs; "check" is a `*_check.rs` file or a `check/` module). Crates:
**C** `ferrix-kernel-core`, **I** `ferrix-kernel-item`, **L**
`ferrix-kernel-load`, **S** `ferrix-kernel-settings`, **B** the binary
`ferrix-kernel`, **lift** the `ferrix-lift-*` twins that build linuxd.
Profiles: **pr** `present`, **de** `desktop` (both `personality=kernel`, load
in ring 0), **fa** `fast`, **mx** `max` (both `personality=user`: core and
item in ring 0, the load in `linuxd` once M5 flips them).

| Module | Ring | Crate | Product lines | Check lines | Linked in ring 0 by |
|---|---|---|---:|---:|---|
| `arch/` | core | C (7 check files raised to L, `framebuffer/check.rs` to I) | 38,673 | 14,643 | all |
| `audit.rs`, `audit/` | core | C | 922 | 748 | all |
| `backtrace.rs`, `claim.rs`, `early.rs`, `fallible.rs`, `hooks.rs`, `irq.rs`, `mmio.rs`, `signal_frame.rs`, `sync.rs`, `timer.rs`, `vmap.rs`, `trap.rs`, `trap/` | core | C | 3,746 | 163 | all |
| `console.rs`, `console/{log,output,pstore}.rs` | core | C | 2,174 | 775 | all |
| `console/screen.rs` | core | C | 330 | 0 | all (leaves ring 0 under fa/mx by §2.1, not yet) |
| `console/input.rs` | core | C | 359 | 0 | all (same) |
| `device.rs`, `device/` | core | C (one check file raised to L) | 3,613 | 3,214 | all |
| `discovery/` | core 1,435 / item 4,372 | C, I | 5,807 | 525 | all |
| `iommu.rs`, `iommu/` | core | C (2 check files raised to L) | 7,365 | 2,066 | all |
| `mm.rs`, `mm/` | core | C | 4,696 | 2,516 | all |
| `object/` | core | C (21 check files raised to L) | 13,099 | 21,295 | all |
| `panic.rs`, `panic/` | core | C | 5,307 | 0 | all |
| `sched/` | core | C | 12,108 | 4,710 | all |
| `smp.rs`, `smp/` | core | C | 2,233 | 1,088 | all |
| `user/` | core | C (4 check files raised to L) | 11,665 | 7,633 | all |
| `platform/google/gs201/watchdog.rs` | core | C | 151 | 0 | all (Pixel 7) |
| `syscall/uaccess.rs` | core | C | 572 | 0 | all |
| `kernel_core.rs` | core | C root | 173 | | all |
| `main.rs` | item | B root | 2,242 | | all |
| `stages_check.rs` | item | B (raised) | | 2,806 | all |
| `syscall/{mod,native,native/*,program}.rs` and their checks | item | I (four checks raised to L) | 7,190 | 4,816 | all |
| `init.rs`, `power.rs`, `random.rs`, `fastpath.rs`, `build_flags.rs`, `boot_authority.rs`, `first_rows.rs`, `linuxd_start.rs`, `checks.rs`, `init_check.rs`, `service_check.rs` | item | I (`service_check` raised to L) | 3,168 | 839 | all |
| `kernel_item.rs` | item | I root | 73 | | all |
| `syscall/` (the Linux personality: 60 files) | load | L | 33,396 | 20,260 | pr, de |
| `fs/` VFS core, pipes, sockets, epoll, pty, namespaces | load | L | 15,531 | 20,554 | pr, de |
| `fs/procfs*` | load | L | 4,509 | 1,765 | pr, de |
| `fs/sysfs*` | load | L | 2,207 | 753 | pr, de |
| `fs/devfs*` | load | L | 1,764 | 989 | pr, de |
| `fs/cgroupfs*` | load | L | 1,961 | 4,798 | pr, de |
| `fs/btrfs*` | load | L | 835 | 2,362 | pr, de |
| `net/` | load | L | 5,122 | 1,232 | pr, de |
| `interfaces/block_ring/` | load | L | 2,351 | 2,539 | pr, de |
| `interfaces/chardev/` | load | L | 2,927 | 3,356 | pr, de |
| `interfaces/display/` | load | L | 4,073 | 263 | pr, de |
| `interfaces/render/` | load | L | 3,187 | 0 | pr, de |
| `interfaces/audio/` | load | L | 1,288 | 0 | pr, de |
| `interfaces/input/` | load | L | 1,310 | 0 | pr, de |
| `interfaces/net_ring/` | load | L | 927 | 693 | pr, de |
| `interfaces/logctl/` | load | L | 281 | 190 | pr, de |
| `platform/st/stm32mp1*` | load | L (not in the lift: `cfg(not(ferrix_load_built = "lift"))`) | 1,620 | 0 | pr, de (DK1) |
| `platform/google/gs201/usb.rs` | load | L (same) | 142 | 0 | pr, de (Pixel 7) |
| `seam.rs`, `setters.rs`, `kernel_load.rs` | load | L | 589 | | pr, de |
| `settings/`, `kernel_settings.rs` | load | S (removable, `runtime_settings=live`) | 532 | 0 | pr, de |
| `lift/`, `lift_core.rs`, `lift_item.rs` | (shim) | lift | 8,380 | 220 | none: linuxd only |

Totals: core 104k, item 14k, load 122k lines with their checks, as
KERNEL-CRATES §1 has them. **Raised checks**, from `raised_checks` itself
(42 entries; the record's comment and KERNEL-CRATES §2 still say 26, fixed in
S5, 7346 (1)): 40 built by the load (object 21, arch 7, user 4, syscall 4,
iommu 2, device 1, `service_check.rs`), `arch/x86_64/framebuffer/check.rs` by
the item, `stages_check.rs` by the binary. Under `fast` and `max`, ring 0 is the core and
item rows only; everything from `syscall/` (load) down is either in `linuxd`
(through `ferrix-lift-load`, which compiles `kernel_load.rs` a second time)
or, for the board glue, built nowhere yet.

**The load's internal edges** (non-comment `crate::` paths between groups,
counted per site): `syscall` names every other group (`fs` 116, `net` 15,
`setters` 16, `chardev` 13, `procfs` 7, ...); `fs` names `net` (9), `devfs`
(4) and `block_ring` (4); `net` names `fs` (6); `devfs` names `audio` (12),
`display` (10), `chardev` (8), `input` (8), `render` (7); each of those names
`fs` or `devfs` back; `display`, `render` and `chardev` name one another;
`procfs` names `setters` (8) and `setters.rs` re-exports `procfs::Kernel`.
So `syscall`, the VFS core, `devfs`, `procfs`, `sysfs`, `cgroupfs`, `net` and
the device cores but `logctl` are **one strongly connected component**
today. The leaves, named only by their module declaration and by `main.rs`,
are: `settings` (already a crate), the board glue and `logctl` (core and
item only). `net_ring` is nearly one: besides `main.rs` and
`stages_check.rs`, only `fs/sysfs.rs` names it, five calls of
`net_ring::node_of` (lines 894, 964, 1267, 1450, 1480), which reads
`net_ring`'s static `PLACED` (7346 (2)). `btrfs` is one with a seam.

## 2. The target

### 2.1 What stays in `src/kernel/`

`src/kernel/` holds what ring 0 keeps under `fast` and `max`: the core and
the item, their crates, the binary and its composition root, the build
script and the linker scripts, the lift's shim of the core and the item
(the core's and item's interface seen from user space, which belongs with
what it mirrors), and one crate of raised checks (§2.6 item 5). The load
leaves whole in S5, keeping its package name `ferrix-kernel-load`, and is
then cut into parts inside `src/kernel/extensions/` (P1 to P3), the
customer's name for the parts' place (2026-11-05, §2.2).

```text
src/kernel/                           ring 0 under fast/max, and the composition root
  Cargo.toml build.rs linker/         the binary ferrix-kernel (main.rs, stages_check.rs)
  core/ item/                         ferrix-kernel-core, ferrix-kernel-item
  checks/                             ferrix-kernel-raised-checks (S5): root in src/, files stay put
  lift/{core,item}/                   the shim's manifests (ferrix-lift-core, -item)
  src/                                core and item files, the shim, the crate roots
  extensions/                         optional parts; each a crate, built into ring 0 or linuxd
    settings/                 ferrix-kernel-settings         S1  (twin: none, `fixed` in fast/max)
    board/stm32mp1/           ferrix-kernel-board-stm32mp1   S2
    board/gs201-usb/          ferrix-kernel-board-gs201-usb  S2
    device/logctl/            ferrix-kernel-logctl           S3  + lift/ twin
    device/net-ring/          ferrix-kernel-net-ring         S4  + lift/ twin
    personality/linux/        ferrix-kernel-load             S5  + lift/ twin (ferrix-lift-load)
    fs/btrfs/                 ferrix-kernel-btrfs            P1
    device/{input,audio}/     ferrix-kernel-input, -audio    P2
    device/block-ring/        ferrix-kernel-block-ring       P2
    device/gpu/               ferrix-kernel-gpu (display, render, chardev)  P3
    fs/{procfs,sysfs,devfs,cgroupfs}/                        P3
    net/stack/                ferrix-kernel-net              P3
    fixture/kernel-roots/     check-kernel-roots.py's planted part (S0): no package, never built
```

Each part's directory is a package: `Cargo.toml`, `src/lib.rs`, and the
moved files under `src/` **at the module path they have today**, so
`fs/btrfs.rs` becomes `src/kernel/extensions/fs/btrfs/src/fs/btrfs.rs` and
`crate::fs::btrfs::x` still names it.

**A part's `lib.rs`** (7346 (3)) re-exports the crate below with a glob and
declares its own modules inside the same parent module names. A parent it
declares hides the glob's module of that name, so each such parent
re-exports the lower crate's module first:

```rust
pub use ferrix_kernel_load::*;
pub mod interfaces {
    pub use ferrix_kernel_load::interfaces::*; // keeps crate::interfaces::block_ring
    pub mod net_ring;
}
```

S0's mover proves the shape with a dry build of `logctl` and `net_ring`
before any move lands. Items of the load (or the core or item) a part uses
that are `pub(crate)` today become `pub`: a visibility-only edit, each one
listed in the landing message (`net_ring` needs 5, `logctl` 4, plus the
load items they name); every core or item file edited this way is named
and `kernel-interface --check` runs. A part that linuxd builds has its lift
twin **in its own directory**, `lift/Cargo.toml`, naming the same
`src/lib.rs` (M5-LIFT §1's precedent: `ferrix-lift-load` names
`kernel_load.rs`), so one directory holds everything of a part. Every part's
manifest names the one build script, `src/kernel/build.rs` (ledger 3282
(K1)), and is a workspace member.

### 2.2 Why `src/kernel/extensions/`, not `src/lib/kernel/`

**The customer's decision, 2026-11-05:** "ok i like src/kernel/extensions"
(Q1, §5; `docs/backlog/DECISIONS.md`). The customer first suggested a subfolder
of `src/lib/kernel/`; this design recommended a sibling, `src/kernel/extensions/`;
the customer then proposed a directory under `src/kernel/` (`parts`,
`plugins`, `extensions`) and chose `extensions`. The consultant agreed to a
place under `src/kernel/` (ledger 7429 Q.1-Q.4, 7445). The reasons:

1. **`src/lib/` is a promise.** `check-crate-layering.sh` rule 2: `src/lib/*`
   is host-testable, architecture-neutral logic that uses neither the kernel
   nor the loader. The crates in `src/lib/kernel/` today (`frame`, `heap`,
   `sched`, `objects`, ...) keep that: they are the kernel's pure cores and
   the kernel depends on them. A part depends on the kernel's crates
   (`ferrix-kernel-core`, `-item`, often `-load`) and runs in ring 0. Putting
   it under `src/lib/` either breaks rule 2 or adds an exception to it, and
   the one tree where "no kernel" is checked stops meaning that. Under
   `src/kernel/`, rule 2 keeps no exception.
2. **Kernel code stays in the kernel's tree.** `desktop` links these parts into
   ring 0, and the kernel's other packages (`core/`, `item/`, `load/`, `lift/`)
   already nest in `src/kernel/`. Every tool that takes the `src/kernel/` prefix
   for kernel code (the step cache, `gate-rows`, the signing anchors, the
   asm, panic and unsafe audits) covers a part with no edit, which fails safe:
   more invalidation, more scanning (7429 (1)). It also removes the trap of
   7346 (4), where `src/kernel` as a string prefix matched `src/kernel-parts`.
3. **The item's evidence keys on rings, not paths.** Rings, traceability
   anchors, coverage and the panic catalog are keyed by file path (§3), and a
   part's file is keyed by its repo path under its root. Every kernel source
   root is in one record, `kernel_source_roots`, which also holds the place
   (`parts_place`): the directory's name is written there and in xtask's one
   constant, held equal by a unit test, so a rename is a data change (7445
   B.1-B.3). A figure labelled item, core or ring 0 comes from the rings,
   never from a path prefix (7429 (1)): `git ls-files src/kernel` now counts
   part files, which are load-ring code `desktop` builds into ring 0.

What nesting costs, and S0 covers: `kernel_roots.outside("src/kernel")` holds no
part, so the layering check and rule 4 take every root (7429 Q.1); a root may
not nest in another nor sit under `src/kernel/src` (Q.2); a `.rs` under
`src/kernel/` that no root holds fails `check-item-boundary.py` (Q.3).

### 2.3 The rules a part keeps

* **One direction.** A part depends on `ferrix-kernel-core`, `-item` and, if
  it needs the VFS or the personality, `-load`; never the reverse, and no
  part on another part unless the record (§2.5) lists that edge.
  `check-crate-layering.sh` gets `kernel_rule` lines for each, and
  `check-item-boundary.py`'s `removable_problems` already refuses a kernel
  crate other than the binary listing a removable crate.
* **Selected by the composition root, statically.** Where a lower crate calls
  a part, the lower crate defines a trait (generated by `seam!` with its
  `Absent` beside it, KERNEL-CRATES §6.1 point 2) and `main.rs` binds the
  part's type once per flag value (§6.3). No `dyn`, no table of pointers, no
  run-time registration, no boot option. Where only `main.rs` calls a part
  (`install()`), the call is the composition, under the profile's `cfg`, and
  no seam is needed.
* **Each part is a removable crate** in the sense of KERNEL-CRATES §6.1: a
  `composition_root.removable` entry with its flag, `present_when`, binding,
  `symbol` and `absent_symbols`, checked by `removable_problems`,
  `seam_scan_problems` and `check-removable-symbols.py` on the linked ELF.
  For a part whose flag is the profile's personality (the device cores, the
  filesystems), the flag is the derived `ferrix_load_built`: `ring0` links it
  into the kernel, `lift` into linuxd, `none` nowhere.
* **Same source in ring 0 and in linuxd.** A part that linuxd needs has a
  twin manifest, `<part>/lift/Cargo.toml` (lib name the same, its
  `ferrix-kernel-*` dependencies renamed onto `ferrix-lift-*` with Cargo's
  `package =`), naming the same root; `lift_packages` and the second key of
  `lift_reuse` (the root) list it. No `#[path]`, no `include!` (KERNEL-CRATES
  §3 (1)), no file of a part reached by any crate but the part and its twin.
* **Ring unchanged by the move.** A moved file keeps its ring. Every part is
  `load` ring: nothing in §2.1's "what stays" moves.
* **No `#[cfg(target_arch)]`** in a part (rule 4 not extended, as ruled for
  d930bd4f's relayout). The board glue has none today.

### 2.4 The parts

| Crate | Files today | Product / check lines | Depends on | Who names it today | Seam needed | Ring 0 in | linuxd in | Split |
|---|---|---:|---|---|---|---|---|---|
| `ferrix-kernel-settings` | `settings/`, `kernel_settings.rs` | 532 / 0 | core, load | `main.rs` (binding) | `setters::Setters`, built | pr, de (`live`) | no (`fixed`) | **S1: path move only** |
| `ferrix-kernel-board-stm32mp1` | `platform/st/stm32mp1.rs`, `stm32mp1/` | 1,620 / 0 | core, item | `kernel_load.rs` (mod), `main.rs` (`install`) | none (main's call) | pr, de | not built (Q3) | **S2** |
| `ferrix-kernel-board-gs201-usb` | `platform/google/gs201/usb.rs` | 142 / 0 | core, item | `kernel_load.rs` (mod), `main.rs` | none | pr, de | not built (Q3) | **S2** |
| `ferrix-kernel-logctl` | `interfaces/logctl/` | 281 / 190 | core, item | `interfaces/mod.rs` (mod), `main.rs` | none | pr, de | yes (twin) | **S3** |
| `ferrix-kernel-net-ring` | `interfaces/net_ring/` | 927 / 693 | core, item, load (`net`, `block_ring::location_of`) | `interfaces/mod.rs` (mod), `main.rs`, `stages_check.rs`, `fs/sysfs.rs` (`node_of`, five calls) | none, once S4a moves `PLACED` and `node_of` into the load's `net::device` | pr, de | yes (twin) | **S4a, S4** |
| `ferrix-kernel-btrfs` | `fs/btrfs*` | 835 / 2,362 | core, item, load | `fs/mod.rs` (`start`, `settle`, `drop_clean`), `fs/{root,data,home}_disk.rs` and `syscall/fsctl.rs` (`label`, `mount`, `mount_rw`), `procfs/render.rs` (`clean_bytes`), `main.rs` (powerfail, write check) | `fs::DiskFilesystem`: those seven entries, `Absent` answering `ENODEV`/0 | pr, de | yes, until btrfsd (`fs=lift`) | P1 |
| `ferrix-kernel-input`, `-audio` | `interfaces/{input,audio}/` | 2,598 / 0 | load | `devfs` (20 sites: `device`, `device_indices`, `evdev`, `card`, `card_indices`, `pcm`), `syscall/fd.rs` (the `ioctl` chain), `syscall/memory.rs` | `fd::DeviceFiles` (`of` + `ioctl` + `mmap`) and `devfs::DeviceNodes` (`indices`, `open`), bound as a tuple in the chain's order (§2.6 item 1) | pr, de | yes | P2 |
| `ferrix-kernel-block-ring` | `interfaces/block_ring/` | 2,351 / 2,539 | load | `fs` (VFS block devices), `claim.rs`, device cores' shared helpers | `fs::BlockDevice`; its shared helpers move to the core or item first | pr, de | yes | P2 |
| `ferrix-kernel-gpu` (display, render, chardev) | `interfaces/{display,render,chardev}/` | 10,187 / 3,619 | load | `devfs`, `syscall` (18), `procfs`, `sysfs` | P2's two seams, with their entries; one crate, since the three name one another (Q2) | pr, de | yes | P3 |
| `ferrix-kernel-procfs`, `-sysfs`, `-devfs`, `-cgroupfs` | `fs/{procfs,sysfs,devfs,cgroupfs}*` | 10,441 / 8,305 | load | `syscall`, `fs` core, `setters.rs` | `fs::Mountable`; `setters.rs`'s `pub use procfs::Kernel` moves to the load | pr, de | yes | P3 |
| `ferrix-kernel-net` | `net/` | 5,122 / 1,232 | load | `fs` (9), `syscall` (15), `procfs` (7) | `fs::SocketFamily`; `net`'s 6 sites into `fs` stay (one way) | pr, de | yes, until netd (M8) | P3 |
| `ferrix-kernel-load`, the Linux personality | `syscall/` (load files), `fs/` core, `seam.rs`, `setters.rs`, `kernel_load.rs`, and until P1 to P3 everything above | 49,516 / 40,814 (122k with the not yet split parts) | core, item | `main.rs`, the raised checks | `Personality` (W-5), built | pr, de | yes (`ferrix-lift-load`) | **S5: whole, to `personality/linux/`, name kept** |
| console screen and input | `console/{screen,input}.rs` | 689 / 0 | core | `console.rs`, `device/check.rs`, `discovery/firmware.rs`, the three `arch/*/mod.rs`, `iommu` checks | a core seam `console::Screen` bound by `main.rs` | all | no | P5 (core ring: §2.6) |

### 2.5 The record

`certification-item.json` gains one key, `kernel_source_roots`, the list of
directories whose files are kernel files (`src/kernel/src` and each part's
`src/`), and each part's entry in `kernel_crates` carries its `path` and
`root` as the four crates' do. Ring `members` stay relative to
`src/kernel/src` for files there; a part's files are classed by a new
`members` form `<part path>/**`, repo-relative, so a moved file's ring is read
from one line per part. Every tool that walks kernel files reads
`kernel_source_roots` instead of its own `src/kernel/src` literal (§3.4).

### 2.6 What can't be split clean today, and what would unblock it

1. **The load's strongly connected component** (§1). `syscall` names every
   group; `fs`, `devfs`, the device cores and `net` name one another. A part
   may name the load, never the reverse, so each edge from the load into a
   part is inverted by a seam the load defines (`seam!`, KERNEL-CRATES §6.1
   point 2), bound by `main.rs` under `ferrix_load_built = "ring0"` and by
   linuxd's root under `"lift"`, each seam its own landing with its own
   design review, before its part moves:
   * **btrfs** (P1): `fs::DiskFilesystem`, seven entries (`start`, `settle`,
     `drop_clean`, `label`, `mount`, `mount_rw`, `clean_bytes`), every call
     site listed in §2.4. `Absent` answers as a kernel without btrfs would
     (`ENODEV`, `None`, 0); no profile binds it today, so it is the
     removable pattern's absent value with no profile choosing it until
     btrfsd (`fs=server`) does.
   * **input, audio, and the GPU group** (P2, P3): today `syscall/fd.rs`
     tries each device core's file type in a fixed order (`display::drm::of`,
     `render::node::of`, `input::evdev::of`, `audio::pcm::pcm_of`,
     `control_of`) and calls its `ioctl`; `devfs` names each core's
     `*_indices` and node constructors. The seams are `fd::DeviceFiles`
     (`ioctl`, `mmap`: `Option` for "not mine") and `devfs::DeviceNodes`
     (`indices`, `open`), with the load's `impl<A, B> DeviceFiles for (A, B)`
     trying `A` then `B`. `main.rs` binds the tuple **in today's order**, so
     the answers are the same call for call; static dispatch, no table. The
     order is a recorded field of the removable entry, checked.
   * **pseudo-filesystems** (P3): `fs::Mountable` (by filesystem name), after
     item 3.
   * **net** (P3): `fs::SocketFamily` (socket, netlink, `netns_file`) and
     procfs's `net::acting` view. `net`'s own two names of `fs` (`socket`,
     `wake`) are one-way and stay.
2. **Globals.** `fs::namespace()`, the mount table and the inode caches are
   the load's statics, read by parts: one-way, allowed. A part's static the
   load reads (the device cores keep 53 statics in their `mod.rs` files)
   would not be. *Unblock:* each part's landing lists, from the resolver's
   surface and `nm` on the ELF, every part item the load names; each goes
   behind the seam or down into the load first.
3. **`setters.rs` re-exports `procfs::Kernel`.** A load file naming a part.
   *Unblock:* move `Kernel` into the load (`setters.rs` or `fs`), and procfs
   uses it from there; one small landing before P3.
4. **The device cores' shared ring helpers in `block_ring`**, named by
   `audio`, `display`, `input`, `net_ring`, `render`. *Unblock:* before P2,
   move what they share into the load's `interfaces/mod.rs`; until then
   S4's `net_ring` names `block_ring::location_of` in the load, one-way.
4a. **`net_ring`'s `PLACED` map, read by `sysfs`** (7346 (2)). *Unblock
   (S4a):* the device-key-to-node map and `node_of` move into the load's
   `net::device`, which already keeps the net devices; `net_ring` writes it
   there. A reviewed step of its own, with a diff check, before S4.
5. **Raised checks.** 40 core and item check files are built by the load
   crate because they need the load (`raised_checks`, §1). They are core and
   item files by path (`arch/**`, `object/**`, ...) and must not move with
   the load, or they would change ring by path; but the load's root, once in
   `src/kernel/extensions/`, cannot reach them without `#[path]`. *Unblock (S5):*
   a crate of their own, `ferrix-kernel-raised-checks`, root
   `src/kernel/src/kernel_raised.rs` (classed `load` as a crate root, as
   `kernel_load.rs` is), its package in `src/kernel/checks/`, depending on
   core, item and load; the load's root stops declaring them and the
   binary's root names the new crate where it names them today. Each file
   stays where it is and in its ring; `raised_checks` records the new
   crate. It is the one crate in `src/kernel/` that names a part, it is
   verification code, and only the binary names it; the layering rule says
   exactly that. It takes the 40 entries with their `file_ring` unchanged and
   `crate` rewritten; a product file reached by it is refused, a control that
   must read FIRED (7346 (9)). Under `"lift"` it is not built, as the raised modules are
   not today. If a load *product* file calls a raised check, S5's resolver
   run lists it and that call moves to the binary first.
6. **The console's screen and input are core ring files.** Under
   `fast`/`max` they leave ring 0 (MICROKERNEL §2.1), but `console.rs`,
   `device/check.rs`, `discovery/firmware.rs`, the three `arch/*/mod.rs` and
   the IOMMU checks call them. *Unblock:* a core seam `console::Screen` with
   a core `Absent`, the screen a part bound by `main.rs` under
   `desktop`/`present`. Taking a file out of the core changes what the core
   is and needs its own design review (P5); not in this split's first round.
7. **Board glue under `personality=user`.** Not built into linuxd today
   (`cfg(not(ferrix_load_built = "lift"))`) and MICROKERNEL §2.1 says it
   leaves ring 0; nothing serves it in `fast`/`max` yet. S2 changes nothing
   there; its `fast`/`max` home is M8's board server (Q3).
8. **`gs201/usb.rs` names `stm32mp1`** once, in a doc comment (line 12),
   not in code: the two board parts are independent crates.


## 3. Evidence

Each move is a **line-preserving rename**: the moved files' bytes are
identical, so every body, every line number and every anchor's `match` text
is too. What changes is the files that declare modules (`kernel_load.rs`,
`interfaces/mod.rs`, a part's new `lib.rs`), the manifests, `main.rs`'s
`use` lines, and the records that key on paths. The landing message shows
each item below with its output.

### 3.1 Bodies byte-identical

* `git diff -M100% --stat <base> <tip>` lists every moved file as `R100`; a
  file under 100% similarity fails the landing. The script that moves
  (§4.1) asserts it before it commits.
* The new `lib.rs` of a part carries the module declarations cut from the
  old root, unchanged, plus `pub use ferrix_kernel_load::*` (or `_item`)
  so `crate::` paths resolve as before (KERNEL-CRATES §1's rule).
* `cargo xtask kernel-interface --check` (docs/generated/KERNEL-INTERFACE.md)
  shows the core's and item's `pub` interface unchanged.

### 3.2 Coverage carried

* The coverage evidence (`coverage-*.json`) and its residual are keyed by
  file path. `carry-coverage.py` gains `--rename OLD=NEW` (from the mover's
  map): since the lines are the same, it rewrites keys and checks that each
  renamed file's line set is the same before and after. A residual entry
  whose path does not exist fails.
* `gen-coverage-justification.py --check` passes on the tip: argued anchors
  keep their `match` text, only their path moves.
* **Re-measurement** (7346 (7)): none for S1 and S5, which keep their
  package names, so every function stays in the crate it was compiled in.
  The coverage profile (crate-local ThinLTO) and dev (no LTO) inline only
  within a crate, so a new crate boundary can change what is inlined and so
  which lines are instrumented: S2 to S4, the raised-checks crate and every
  P landing each run one coverage-profile boot on the arches that run the
  moved files and compare the line sets per file with the base's.
  `carry-coverage --rename` and `gen-coverage-justification --check` run
  last, on the compared result.

### 3.3 Traceability, item boundary, boot, symbols

* **Traceability**: anchors are `path::function`. The mover rewrites them in
  `safety-requirements.json`, `traceability-units.json` and
  `traceability-baseline.json` from its map; `check-traceability.py` passes
  with the same count of anchored requirements and units before and after
  (both printed).
* **Item boundary**: `check-item-boundary.py --report` before and after: the
  same core and item files, the same ring counts per ring, and the moved
  files in `load`. "Each file is reached by exactly one crate" holds with
  the new crate; `known_violations` stays empty.
* **Boot lines identical on four boots**: `present` and `desktop` on x86-64
  and AArch64 (and ARMv7-A `--smp 2` for the DK1's glue in S2), boot lines
  compared with `check-boot-line.py` against the base's, byte for byte.
* **Symbols on `present` and `desktop`** (7346 (6)): function **bodies**,
  not section sizes. The release kernels of base and tip, `present` and
  `desktop`, x86-64 and AArch64, compared with the compare tool landed under
  ledger 7319 L.1 (`<.*>` masked to line end), after the recorded crate-name
  rewrite (`ferrix_kernel_load::interfaces::logctl::` to
  `ferrix_kernel_logctl::interfaces::logctl::`) and with mangling hashes
  stripped. No body may differ unless it is named and explained (inlining
  across a new crate boundary is the expected cause); otherwise the landing
  stops. S1 and S5 keep their package names, so their bodies compare with
  no rewrite. Panic locations embed source paths in `.rodata`, which
  differs by exactly the renamed path strings; image size per profile is
  shown before and after for information.
* **Strings that name moved paths** (7346 (8)): `panic/catalog.rs`, a core
  file, holds `see:` paths into moved files (lines 874, 2184 and others).
  That string-only core edit is named in the landing and the catalog is
  regenerated. Gate expectations, goldens and transcripts are grepped for
  every moved path, and each hit is rewritten in the same landing.
* **Rings over parts** (7346 (10), the consultant's ruling on how parts are
  recorded): one `members` line per part, `src/kernel/extensions/<group>/<name>/**`,
  in the `load` ring only. `check-item-boundary.py` fails a file under a part
  directory that no line covers, a part line in a ring other than `load`, and
  a `core` or `item` glob that matches a part path.
* **Removable symbols**: `check-removable-symbols.py` on `fast`/`max`
  (absent: no symbol of the part in the kernel) and on `desktop` (present).

### 3.4 Tools to change (one landing, S0, before any move)

Every tool that walks kernel sources by a path literal would, after a move,
silently stop seeing the moved files and still pass. S0 makes them read
`kernel_source_roots` and adds a **file-set parity guard**: each scanner
prints the files it walked, and the guard compares them with its own count
(`git ls-files` under the roots), never through the scanner's own walk
(7346 (5)).

**The list is built by grep, not by hand** (7346 (4)): every form of the
literal (`src/kernel`, `src/kernel/`, `src/kernel/src`, `kernel/src`) across
`tools/`, xtask, `.github/` and `scripts/`, each hit classed in S0's message,
and each tool stating whether it means the `src/kernel` *prefix* (which also
matched `src/kernel-parts`, by accident, before the customer put the parts
under `src/kernel/`) or the directory `src/kernel/`. The
hand list below is the review's starting point, not the list of record:
`check-item-boundary.py`, `check-crate-layering.sh` (its `find src/kernel
-maxdepth 2` for kernel crates; rule 4's `cfg(target_arch)` grep at line
218, which looks under `src/kernel/src` only and so would miss parts),
`check-lift.py`, `check-build-flags.py` (rule 6e), `check-load-ring.py`,
`check-traceability.py`, `check-fallible-alloc.py`, `check-unsafe-audit.py`,
`check-kernel-alloc-audit.py`, `check-panic-audit.py` and
`check-asm-budget.py` (both walk `ROOTS "src/kernel"`),
`check-signing-anchors.py`, `check-complexity.py`, `check-stack-depth.py`,
`check-device-access.py` (and its allowlist), `check-boot-workers.py`,
`check-first-rows.py`, `check-frames.py`, `check-asid-tlbi.py`,
`check-xsave-sites.py`, `check-vmo-seal-tags.py`, `check-level-doors.py`,
`check-evidence-homes.py`, `check-ferrocene-rust.py`, `check-boot-line.py`;
`carry-coverage.py`, `coverage-report.py`, `decision-coverage.py`,
`debt-report.py`, `gen-panic-catalog.py`; `asm-allowlist.json`,
`device-access-allowlist.json`, `build-flags.toml`; xtask's sites
(`REMOVABLE`, symbolize, postmortem, compositor, restart, `gate_rows.rs`,
`test_cache`, and above all `step_cache.rs`, whose item prefix
`src/kernel/` would not invalidate the kernel's cached steps on a part
file's change, so a stale cache would pass a row).

**S0's negative controls**, each through `gate.sh control --expect` and each
to read FIRED (7346 (5), 7429 S.6): (a) a part's planted unchecked `alloc`
call goes unreported when `check-kernel-alloc-audit.py` walks `src/kernel/src`
alone (the consultant accepted this audit for (a): `check-fallible-alloc.py`
reads item-ring files, and a part is never item, 7429 S.6); (b) one scanner
walking `src/kernel/src` alone, caught by the parity guard; (c) the step
cache's parts entry dropped, caught by its unit test; (d) the parts' place
dropped from `check-item-boundary.py`'s kernel places, caught by its
self-test; (e) rule 4 grepping `src/kernel/src` alone, caught by the guard's
fixture run; (f) a `.rs` under `src/kernel/` no root holds, refused by
`check-item-boundary.py` (7429 Q.3). S0 changes no kernel file; its fixture
part, `src/kernel/extensions/fixture/kernel-roots/src/planted.rs`, is in no
package and never built.

## 4. The plan

### 4.1 How a move is done

A script, `tools/common/move/move-kernel-part.py`, landed in S0, takes a
map (old path to new path, part crate name) and: does the `git mv`s;
cuts the module declarations into the part's `lib.rs` and the old root;
writes the manifest and, if needed, the lift twin; rewrites the path-keyed
records (rings, `lift_reuse`, `lift_cfg_sites`, `composition_root`
`load_modules` and `removable`, traceability, coverage, the panic catalog,
allowlists); and asserts §3.1. It is rerun on a fresh base at landing time,
so the freeze is the run, the build and §3's checks, not hand edits
(d930bd4f's relayout precedent).

**Rebasing open branches across a move.** The script also writes
`rewrite-paths.sed` for the map. A branch with an edit in a moved file
rebases with `git rebase -X find-renames=100%` (git follows a 100% rename);
a branch that adds a new file in a moved directory or names a moved path in
a record runs the sed on its tip, then rebases. Neither changes code. The
landing message says which open branches touch the moved files (`git log
--name-only origin/develop..<branch>` per known branch) and the PO tells
their owners.

### 4.2 The landings

| # | Landing | Points | Freeze (files the PO holds) | Depends on |
|---|---|---:|---|---|
| S0 | **Landed** (see *Where it stands*). Tools: `kernel_source_roots` with `parts_place`, every scanner reads it, file-set parity guard, `carry-coverage.py --rename`, `move-kernel-part.py`, `check-crate-layering.sh`, `check-item-boundary.py` and `check-kernel-shape.py` read every root, the planted part file and its controls. Owed to later landings: the mover's `lib.rs` and manifest writing, the rodata/symbol comparison run, the `lib.rs`-shape dry build (S2 or S3, 7429 S.7) | 5 | none: no kernel file | this design |
| S1 | `ferrix-kernel-settings` to `src/kernel-parts/settings/`: a path move, it is already a removable crate | 1 | `settings/`, `kernel_settings.rs`, `src/kernel/settings/`; about 20 min | S0 |
| S2 | Board glue to `board/{stm32mp1,gs201-usb}/`, two crates | 2 | `platform/st/`, `gs201/usb.rs`, `kernel_load.rs`'s `platform` block, `main.rs`'s `install` lines; about 30 min, DK1 boot under ARMv7-A `--smp 2` included | S0 |
| S3 | `logctl` to `device/logctl/`, with its lift twin | 2 | `interfaces/logctl/`, `interfaces/mod.rs`, `main.rs`; about 30 min | S0 |
| S4a | `PLACED` and `node_of` from `net_ring` into the load's `net::device` (§2.6 item 4a); a code change, reviewed, no move | 2 | none | S0 |
| S4 | `net_ring` to `device/net-ring/`, with its lift twin | 2 | as S3, plus `stages_check.rs`'s one call and `sysfs.rs`'s five | S3, S4a |
| S5 | The raised-checks crate (§2.6 item 5; the record's "26" comment fixed), then the whole load to `personality/linux/`, name kept, its lift twin moved with it. After S5, `src/kernel/src` holds only core, item, the shim and crate roots | 5 | every load file: the longest, about 60 min; announced a day ahead | S0, S4 |
| P1 | `fs::DiskFilesystem` seam (no freeze), then btrfs as a part | 5 | `fs/btrfs*` and their callers' lines | S5; seam design review |
| P2 | `block_ring`'s shared helpers down (no freeze); `fd::DeviceFiles` and `devfs::DeviceNodes` seams (no freeze); then input, audio, block ring as parts | 8 | per part | P1; seam design review |
| P3 | `procfs::Kernel` down; then the GPU group, the pseudo-filesystems, the net stack, one seam and one part per landing | 20 (a guess, like M5's) | per part | P2; seam design reviews |
| P5 | The console's screen and input: a core seam and a part | 5 | per landing | own design review (core ring) |

S0 to S5 total 19 points and need no new seam (S4a is a small code change
with its own diff check): their review is a diff check of §3's evidence on
each landing hash (7346 (12)), and after S5 the customer's request holds for the
tree (nothing optional sits in `src/kernel/`). P1 to P3 (33 points, the
uncertain part) then cut the personality into parts inside
`src/kernel/extensions/` without touching `src/kernel/`, which is why the whole
load moves first: each later cut is a move between two part directories and
its freeze touches no core or item file. P5 is optional for this request
(the screen is core ring today) and goes after `fast` and `max` have a
console server to hand it to.

**Each move landing also updates** KERNEL-CRATES §1 and the SysML model's
structure (`docs/sysml/`) wherever they name a moved path (7346 (11)), and
LAYOUT.md's `src/kernel/` and a new `src/kernel/extensions/` section.

**Ordering against other work.** None of S1 to S5 starts before mk-flip3's
`fast` lands (the coordinator, 2026-11-05): S5's freeze would hold every
file of the flip. S0 can land at any time.

**The freeze.** Before each of S1 to S5 the PO stops landings that touch the
row's files (not the whole tree); agents with branches in them commit and
wait for the sed. Each freeze is the mover's run on the current develop, the
builds of `present`, `desktop` and `fast` on three architectures, §3's
checks, the four boots, then the push and the sed handed out. S1 to S4 can
share one freeze if they land as one stack.

## 5. Questions for the customer

Each has a default the work proceeds on, and a line in
`~/.local/share/ferrix/fleet/morning-questions.md` (7346 (11)).

1. **Where the parts live. DECIDED, 2026-11-05, by the customer:**
   `src/kernel/extensions/<group>/<name>/` ("ok i like src/kernel/extensions"),
   over this design's `src/kernel/extensions/` and the first suggestion,
   `src/lib/kernel/parts/`. §2.2 gives the reasons; the consultant's
   conditions for a place under `src/kernel/` are ledger 7429 Q.1-Q.4 and
   7445, met in S0. Recorded in `docs/backlog/DECISIONS.md`.
2. **How fine the crates are.** One crate per device core and per
   filesystem (as §2.4), or one per group (`ferrix-kernel-devices`,
   `ferrix-kernel-fs`). No certification angle. *Default:* one per part,
   with display, render and chardev as one crate (`ferrix-kernel-gpu`).
3. **The board glue under `fast` and `max`.** It is built into neither ring 0
   nor linuxd under `personality=user` today, and changes no claim.
   *Default:* S2 moves it as it is; its `fast`/`max` home is M8's board
   server, and until then `fast`/`max` on the DK1 and the Pixel 7 run
   without it, which a BACKLOG row records.

Not the customer's: the order of the landings is the PO's (§4.2's order is
the consultant's default: S1 to S3, then S4a and S4, then S5 after `fast`
has landed); how the rings record parts is the consultant's ruling, §3.3's
"Rings over parts".

## Where it stands

Design reviewed **OK IF (1)-(12), ledger 7346**, the conditions folded in
above. The place is the customer's, `src/kernel/extensions/` (2026-11-05,
§2.2, §5 Q1), agreed by the consultant (ledger 7429 Q.1-Q.4, 7445).

**S0 landed** (tools only, no kernel file; diff check ledger 7429 S.1-S.8 and
7445 B.1-B.3). `certification-item.json`'s `kernel_source_roots` lists the
roots (`src/kernel/src` today) and the place; `tools/common/check/kernel_roots.py`
reads both and refuses a nested root, one under `src/kernel/src`, one outside
the place, and a place outside `src/kernel/` without a reason; xtask's one
constant `KERNEL_PARTS_PLACE` is held to the record by a unit test.
`check-kernel-roots.py`, in `cargo xtask check`, holds the census (every bare
or segmented kernel path literal under `tools/`, `scripts/`, `.github/`
classed in `kernel-path-literals.json`, no tool naming the place literally)
and the parity guard (nine walkers, each run with and without the fixture
part, against `git ls-files`; the walkers it does not run each say in
`parity_by` how parity holds). `check-item-boundary.py` fails a `.rs` under
`src/kernel/` no root holds; `check-kernel-shape.py` takes a part's file read
for ring 0 as a breach; `check-crate-layering.sh` reads every root.
`move-kernel-part.py` does the `git mv`s, the path rewrite and the R100 check
and lists the records keyed relative to `src/kernel/src`.

Not in S0, owed: the mover writes no `lib.rs` or manifest yet; the
`lib.rs`-shape dry build on `logctl` and `net_ring` (7346 (3)) is evidence of
the first landing that makes a part crate out of load files, S2 or S3, before
its freeze (7429 S.7); rule A (dangling file literals) is not built. Each of
S1 to S5 comes back for a diff check with §3's evidence on its landing hash;
P1 to P3 and P5 each need a design review before code. Next: S1, which waits
for mk-flip3's `fast`.
