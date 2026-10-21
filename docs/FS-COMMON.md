# What filesystems share

Version 2, 2026-10-21: built on branch fs-traits under the certification
consultant's OK IF of 2026-10-19 (ledger line 2587), with the design
corrected where the review changed it (section 2's mapping) and where it
had the counts wrong (section 1). Version 1, 2026-10-19, was for review
before it was built. The customer asked that day for `src/lib/fs` to be
grouped by filesystem with a `common/` for the rest, "so that we can later
add ntfs ext4 etc as well", and for the common parts to be traits; and
chose to move btrfs onto them now rather than when a second filesystem
exists.

## 1. What is common today

Three things every on-disk filesystem Ferrix will have needs, and that
btrfs has as traits or types of its own:

| What | Before the change | Users |
|---|---|---|
| Reading the device | `ferrix_btrfs::volume::Device` and `ReadKind` | 17 implementations: the kernel's `Disk`, `btrfs-vfs`'s cache (`Cached`), the write path's `Borrowed`, and 14 test devices (the fuzz target's and `mkfs`'s tests' among them) |
| Writing the device | `ferrix_btrfs_write::WriteDevice` | 9 implementations: the kernel's `Disk`, the write path's `Borrowed`, and 7 test devices (`btrfs-vfs`'s, `mkfs`'s and the write path's tests) |
| A new volume as writes | `ferrix_btrfs_mkfs::{Image, Target}` | `ferrix-install` |

(Version 1 said 5 test devices wrote; the consultant's inventory, ledger
line 2587, counts 17 `Device` and 9 `WriteDevice` implementations, 14 and 7
of them test devices. The branch adds three: the installer's `Volume`, both
traits, and a `Refusing` device in each btrfs crate's tests, the
`L.btrfs.117` and `L.btrfs.118` tables.)

Nothing in either trait is btrfs's: byte-addressed reads with a hint of
what they are for, writes, a flush, a durable write. ext4's and NTFS's
readers and resizers (`docs/INSTALLER.md` §4.6) and a FAT writer (§5.4)
need exactly these. The VFS's `FileSystem` and `Inode` are already common
(`src/lib/fs/common/vfs`).

## 2. The design

Two crates in `src/lib/fs/common/`:

**`device` (`ferrix-fs-device`)**, `no_std`, no allocation, no `unsafe`,
no dependency:

```rust
pub enum ReadKind { Metadata, Data }            // moved from ferrix-btrfs, unchanged

// Not #[non_exhaustive] (the review's condition 3): a new variant must
// break every filesystem's mapping, not fall into a wildcard arm.
pub enum DeviceError {
    Read { at: u64 },                           // was BtrfsError::DeviceRead
    Write { at: u64 },                          // was btrfs-write's DeviceWrite
    Flush,                                      // was btrfs-write's Flush
    OutOfMemory,                                // was either crate's OutOfMemory
}

pub trait Device {
    fn read_at(&mut self, at: u64, buf: &mut [u8], kind: ReadKind) -> Result<(), DeviceError>;
}

pub trait WriteDevice: Device {
    fn write_at(&mut self, at: u64, data: &[u8]) -> Result<(), DeviceError>;
    fn flush(&mut self) -> Result<(), DeviceError>;
    fn write_durable(&mut self, at: u64, data: &[u8]) -> Result<(), DeviceError> { /* write, flush */ }
}
```

The names stay `Device` and `WriteDevice`: the kernel's block layer has a
`BlockDevice` of its own, and every implementation keeps its name.
`ferrix-btrfs` re-exports `Device` and `ReadKind` from `volume`, and
`ferrix-btrfs-write` re-exports `WriteDevice`, so no caller's `use`
changes. Each crate maps a `DeviceError` into its own error where it
calls the device, in one named function keyed by the operation it asked
for, with the address it asked for -- not a `From`, which cannot know
either -- and with every arm named (the review's condition 3; version 1
had one `From` each, and a `Flush` read as `DeviceRead { physical: 0 }`,
which the review refused). The `at` a device reports is kept for
diagnostics; the item does not read it.

`ferrix_btrfs::volume::read_error`, through `read_device` at every read
the reader makes (`L.btrfs.117`):

| A read fails with | `BtrfsError` |
|---|---|
| `OutOfMemory` | `OutOfMemory`, passed on as it is: never retried from another copy, never reported as damage |
| `Read`, `Write` or `Flush` | `DeviceRead { physical }`, the address the reader asked for |

`ferrix_btrfs_write::device::device_error`, through `device::read`,
`write`, `write_durable` and `flush` at every call the write path makes
(`L.btrfs.118`); `WriteVolume::read_node` reads through the reader's
`read_device`, so a node read is mapped as the reader maps it:

| Asked | `OutOfMemory` | `Read` or `Write` | `Flush` |
|---|---|---|---|
| a read at `physical` | `OutOfMemory` | `Volume(DeviceRead { physical })` | `Volume(DeviceRead { physical })` |
| a write at `physical`, durable or not | `OutOfMemory` | `DeviceWrite { physical }` | `Flush` (the flush of a durable write failed) |
| a flush | `OutOfMemory` | `Flush` | `Flush` |

Every device in the inventory (section 1) answers only `Read` from a read,
`Write` from a write, `Flush` from a flush, or `OutOfMemory`, so each
answers through the mapping exactly as it did before; the kernel's
`Disk` keeps answering its unaligned write's failed bounce read as
`Write` at the address it was to write.

**`mkfs` (`ferrix-fs-mkfs`)**: `Image`, the writes a new volume is, and
its `write_to<D: WriteDevice>` in the order that keeps a half-written
volume from looking whole (moved from `ferrix-btrfs-mkfs`); and

```rust
pub trait Mkfs {
    type Options<'a>;
    type Error: core::fmt::Display;
    fn make(options: &Self::Options<'_>) -> Result<Image, Self::Error>;
}
```

which `ferrix-btrfs-mkfs` implements for `Btrfs`, and a FAT or ext4 one
later. The installer writes an `Image` through `WriteDevice` and stops
needing `Target`.

## 3. What it changes in the item

`ferrix-btrfs` and `ferrix-btrfs-write` are item crates. Their `Device`
and `WriteDevice` become re-exports of `ferrix-fs-device`'s, so
`ferrix-fs-device` joins the item ring, below both (it depends on
nothing), and `certification-item.json`'s `below` lines for both name it.

**`ferrix-fs-device` is item code.** Its traits and its enum change only
under the certification consultant's review, as any item line does, and a
change to them is a change to every filesystem at once. What ext4, NTFS or
FAT need of a device and btrfs does not -- a size query, discard, a
partition offset -- goes into an extension trait (`trait Discard:
WriteDevice`, say) in a crate of its own, load or unclassified, beside the
filesystem that needs it, never into `ferrix-fs-device`. The item gates
read it with the two btrfs crates: complexity, fallible allocation (0
sites, held at 0 by `cargo xtask check`, `L.btrfs.22`), the unsafe trace,
the panic exemptions, the arithmetic target (`L.btrfs.23`) and
traceability's units.

**`ferrix-fs-mkfs` stays outside the item.** It depends on
`ferrix-fs-device`, and no item crate depends on it, except, if a test
ever needs it, as a dev-dependency, which the boundary gate does not read
as product code. A new volume is made by the installer and by test
fixtures, never by the kernel.

The behaviour must not change: today every device the item is given --
the kernel's `Disk` (`src/kernel/src/fs/btrfs.rs`) and `btrfs-vfs`'s
cache over it -- returns only `DeviceRead`, `OutOfMemory`, `DeviceWrite`
and `Flush`, which map one to one (the tables above). The question for
review was the test devices: one that returns any other `BtrfsError` from
`read_at` (a checksum error, say) cannot under the new trait, and would
have to inject its fault in the bytes it returns instead. The review's
inventory found none: every test device answers only a device error or
`OutOfMemory`, so none had to move its fault into its bytes.

## 4. Order

One landing after the directory move: the two crates, btrfs onto them,
the kernel's `Disk` and every test device, the installer, the item's
classification; gated by `check` and the four boots, with the btrfs boot
checks of stages 11 and 12 and `test-btrfs-pressure`.

## 5. Where it stands

Built on branch fs-traits (2026-10-21), on the SP-2 directory move, which
lands first and on its own. The certification consultant's design review
of version 1, 2026-10-19 (ledger line 2587), is **OK IF** seven
conditions hold:

1. **Manifest and boundary.** `ferrix-fs-device` is in
   `certification-item.json`'s crates, ring `item`, below both btrfs
   crates, with `check-crate-layering.sh`'s rule; ITEM.md section 2, the
   Security Target, the safety manual and the SysML model name it. Done on
   the branch; the boundary control (the manifest entry removed,
   `check-item-boundary.py` refusing an item crate on an unclassified one,
   FIRED through `gate.sh control --expect`) is still to run.
2. **The item gates read it**, and `L.btrfs.22` and `L.btrfs.23` name it
   beside the btrfs crates, ids unchanged; the trait docs' contracts (a
   partly filled buffer is never success; everything written before a
   returned flush is durable) moved with them, naming `DeviceError` where
   they named btrfs's errors. Done on the branch.
3. **The mapping is keyed by the operation, exhaustive, and keeps the
   address asked**: `DeviceError` is not `non_exhaustive`, and section 2's
   two tables are one named function per crate. Done on the branch.
4. **Requirements.** No `H.STORE` statement changes; `L.btrfs.13`, `.21`,
   `.71`, `.110` and `.111` name the mapping among their units;
   `L.btrfs.117` (the reader's table, under `H.STORE.2`) and `L.btrfs.118`
   (the write path's, under `H.STORE.8`), reserved on main before the
   code and released by the commit that writes them, are each verified by
   one host test. Done on the branch; the control swapping one arm of the
   reader's mapping (OutOfMemory to DeviceRead), FIRED on that test's
   assertion, is still to run.
5. **The inventory in the landing**: the commit lists the 17 `Device` and
   9 `WriteDevice` implementations (section 1), each with what it answers
   and what changed in it. At the commit.
6. **The crate's future is said**: section 3. Done.
7. **Evidence for the code review**, still to come: `check` PASSED;
   `test-boot` on x86-64 under KVM, AArch64, ARMv7-A and ARMv7-A
   `--smp 2`, with stage 11's and 12's btrfs lines identical to main's;
   `test-btrfs`, `test-btrfs-pressure` and `test-vfs` PASSED; the controls
   of (1) and (4) FIRED; coverage carried and its justification checked
   after the final rebase; TRACEABILITY regenerated with no status change
   but `L.btrfs.117` and `L.btrfs.118` verified. Then the consultant's
   code review, before `land.sh take`.

Of the review's lesser points, the installer prints each `io::Error` before
it becomes a payload-free `DeviceError`, so a failed install still says
why (C1).
