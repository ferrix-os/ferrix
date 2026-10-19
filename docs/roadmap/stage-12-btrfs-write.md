# Stage 12 — btrfs, write ✅  ·  *≈ 60 points, spent*

Copy-on-write allocation through the extent tree, delayed refs, transaction
commit against both superblocks with correct flush/FUA ordering, the free-space
tree, and log-tree replay.

**Exit, and it is a strict one:** Ferrix writes a tree, and host `btrfs check`
finds nothing. Then the power-fail test — kill QEMU at a random point inside a
transaction, remount, replay, `btrfs check` again — over hundreds of seeds.

**Done — the write path, host-side (21 points, 2026-09-21).** `src/lib/fs/btrfs/write`
changes a volume `mkfs.btrfs` made, allocating but forbidding `unsafe`, and is
checked by host `btrfs check`. What it writes:

* **Copy-on-write trees.** An edit copies every node on its path that the
  last commit can reach and edits the copies; a node held as a typed list of
  items and written whole, never patched. One bottom-up fix-up after each edit
  splits what overflowed into pieces of equal size, drops what emptied, gives
  every parent pointer its child's first key (which Linux checks on every read)
  and shrinks a root with one child.
* **Allocation, recorded as btrfs records it.** Block groups with three range
  sets each — free, *pinned* (freed this transaction, still used by the last
  commit, so never handed out before it), and what the free-space tree says.
  Reference changes queue as delayed refs and become extent items at commit:
  skinny metadata items and data items with inline and keyed back-references
  in the order Linux's tree-checker demands. The free-space tree is rewritten
  by difference; a group kept as bitmaps becomes extents on its first change.
  Chunks are allocated when a kind runs out — `CHUNK_ITEM`, `DEV_EXTENT`s, the
  `DEV_ITEM` in both places, the block group and its free space, and the
  system chunk array for a system chunk — and only between edits, never in the
  middle of one.
* **The commit.** Bookkeeping settles in a loop until a pass changes nothing;
  then every new node is written to each copy (DUP metadata to both), the
  device is flushed, and only then is the primary superblock written with FUA,
  then the mirrors, with the next backup-root slot filled as Linux fills it.
  Nothing the last commit reaches is overwritten, so a crash before the
  superblock leaves the last commit, and a test replays exactly that.
* **Files.** Create, link, unlink, rename, symlinks, device nodes; writes as
  new extents with their checksums (inline for a whole file of at most
  2 KiB), overwrites that cut extents into pieces sharing one reference,
  truncation that zeroes the rest of the last sector, holes by omission
  (`NO_HOLES`), orphans for files unlinked while open and their cleanup at the
  next open, and compressed extents cut by overwrites. Every operation either
  completes or aborts the transaction; nothing half-made is committed.
* **A full volume.** The mount takes room for every page a write dirties
  against what the volume can still take -- free space in data block groups,
  and unallocated device space less a headroom kept for the trees -- so a
  write past it answers `ENOSPC` at the `write`, as Linux's btrfs reserves
  data space, and `statfs` answers the same room. The trees are measured
  too: an operation is admitted only if the metadata groups and the device
  have room for its worst case and the commit's, plus a global reserve only
  a deletion may use, as Linux reserves metadata per transaction; so a
  volume whose trees are full refuses a create with `ENOSPC` and still
  deletes. Running out finds no room before an edit changes anything, which
  aborts nothing. A DUP chunk is sized so both copies find a stripe. What
  is not measured yet: the log replay and orphan cleanup a mount runs, which
  on a full volume can still abort and fail the mount, and the reservation's
  sizes are estimates (docs/BACKLOG.md). A transaction that
  aborts all the same (a disk error) is thrown away, the last commit
  reloaded, and the mount goes read-only with a console line saying why, as
  on Linux. Until 2026-10-02 a `dd` into a nearly full /data latched the
  whole mount into `EIO`; the guest check now fills its disk to `ENOSPC` at
  every boot.

What it refuses to write, with a reason naming it: more than one device, a
profile other than `SINGLE` or `DUP`, no free-space tree, no skinny metadata or
`NO_HOLES`, subvolumes and snapshots (their blocks are shared), quotas, and an
unreplayed log.

It is checked three ways. The stage 11 reader opens and reads back what was
written. A consistency check in the tests recomputes, from the trees, every
tree block's extent item and owner, every data extent's references, each block
group's usage and free space, the superblock's total, each inode's links,
directory size and `nbytes`, and checksum coverage both ways; three negative
controls showed it fails where it should. And `tools/common/test/btrfs-check-writer.sh`
runs host `btrfs check --check-data-csum` over seven volumes the tests write —
DUP and SINGLE, a tree of every object kind with a file big enough to allocate
chunks, the same tree edited, an orphan, split compressed extents, and churn —
all clean, and it too failed on a sabotaged free-space tree.

**Done — the mount and the exit's first half (18 points, 2026-09-21).** The
write path is under the VFS and in the boot test, on all three architectures.

* **Writes reach the disk.** `BlockDevice` gained `write` and `flush`, and the
  block ring's kernel side dispatches them: a write's bytes are copied into
  the region it is sent through before the driver is told, and a flush is the
  barrier `src/lib/fs/common/block`'s queue already knew how to keep. The ring protocol and
  the ring-3 driver needed no change — they had both since stage 10.
* **`src/lib/fs/btrfs/vfs`'s writable mount.** The whole volume behind one sleeping
  lock, because every read must see the running transaction; writes into the
  page cache, remembered as dirty pages and turned into extents a mebibyte at
  a time when something commits; `fsync` writing one file back and committing;
  orphan inodes deleted when the last reference to them goes. Its rule is that
  nothing calls into the page cache while holding the volume lock except to
  read a page that is certainly there, which is what keeps a fill from waiting
  on itself.
* **The calls.** `mount -t btrfs /dev/vdc /mnt` without `MS_RDONLY` now gets
  the writer, and `EROFS` names what it will not maintain. `fsync`,
  `fdatasync`, `syncfs` and `sync` commit rather than answering nothing;
  `Inode::fsync` and `FileSystem::sync` are the VFS's new hooks, and every
  filesystem that keeps nothing back inherits the old answer.
* **The exit's first half.** Every boot now carries a third disk, a fresh
  blank volume, and writes a tree on it: files of every size — including one
  larger than the data chunk `mkfs.btrfs` made, so a chunk is allocated —
  a hole, a symlink, a hard link, an overwrite in the middle of a file, a
  truncation, a rename and an unlink. Then it syncs, **unmounts, mounts
  again**, and reads it all back, so nothing compared came from a cache:
  `btrfs-rw vdc written and remounted: 8 files (10997256 bytes) and 2
  directories read back as they were written`. `cargo xtask test-btrfs` adds
  the other half, host `btrfs check --check-data-csum` over that image: clean
  on x86-64, `AArch64` and ARMv7-A, and it fails, as it must, when the write
  path is sabotaged.

**Done — the log tree (13 points, 2026-09-21).** `fsync` no longer commits
everything. `src/lib/fs/btrfs/write/src/log.rs` keeps a tree outside the root tree
whose address the superblock names in `log_root`: a log commit writes the
log's blocks, flushes, and writes a superblock that is the last committed one
*plus* that address, so it still names the old, whole trees. Nothing else
moves, and a log commit writes strictly less than a commit — a test compares
the two.

A log carries one inode's stat data, its file extents and their checksums.
Not names: a log that carried half a rename would have to carry the whole of
it and the directories either side, which is where Linux's tree-log gets its
size. An `fsync` after anything changed the shape of the tree commits
instead, as Linux does with `BTRFS_LOG_FORCE_COMMIT`.

The mount finds `log_root` set and replays before it hands the volume to
anyone, which it must: the data a log names was written by a transaction that
never committed, so the committed free-space tree calls that space free.
Replay allocates each logged extent at exactly its address, as Linux's
`btrfs_alloc_logged_file_extent` does, puts the logged items into the fs
tree, commits, and clears the log. Its tests are crashes: a volume thrown
away after a log commit and opened again must hold what the log promised, one
thrown away *before* the superblock must hold the last commit whole, and both
must pass the consistency check.

Replay took two more fixes, both found by the power-fail test's host half
below before any QEMU was killed. A log holds the whole of an inode, so
replay drops every extent of the committed tree the log does not name — a
file that shrank otherwise kept its old tail, and `nbytes` fell short of it
— and takes an extent at its address only when the extent tree does not
already name it, since the two ends of an overwritten extent name one a
transaction did commit. And a log commit now puts its own blocks out of
reach, as a commit does its trees: the next edit copies them instead of
writing over them, because the superblock on the disk names them. Without
that, a second `fsync` wrote its items into the blocks the first had
promised, and a crash between the two replayed the wrong file.

**Done — the power-fail test (8 points, 2026-09-21). The exit is met.**
`cargo xtask test-powerfail --seeds N` boots with `ferrix.btrfs=churn`: the
guest (`src/kernel/src/fs/btrfs_powerfail.rs`) rewrites four files on a blank
volume and makes each durable — the body with `fsync`, then a 32-byte
trailer naming the body's length and CRC-32C with a second `fsync`, and a
whole `sync` every eighth pass — until xtask kills QEMU at a moment the seed
picks. Host `btrfs check` reads what is left; a second boot with
`ferrix.btrfs=replay` mounts the same disk, which replays any log the kill
left, checks every file that ends in a trailer against it, and unmounts; and
`btrfs check` reads the volume again. A trailer is written only after its
body was promised, so a body that does not match it is a completed promise
rolled back. The replay boot reads `log_root` before it mounts and says
whether there was a log, and a run of four or more seeds in which no cut left
one fails, since it would have tested the commit and never the replay.

The run for the exit: 200 seeds on x86-64 under KVM, 184 of them leaving a
log that the next mount replayed, and `btrfs check` clean before and after
every one. Under TCG, where a boot costs several times as much: 25 seeds on
ARMv7-A at `--smp 2`, 22 of them replaying a log, and 24 on `AArch64`, 20 of
them replaying one — all clean. The 25th `AArch64` seed's cut and first check
were clean too, and its replay boot then stopped in stage 7's self-check,
FX-0701, before stage 12 ran; `test-powerfail` now retries either boot of a
seed once when it dies before stage 12 touches the disk. The gate row runs 4
seeds on each architecture. The negative control: with replay no longer
dropping the committed extents past a log's last one, the third seed fails
with `btrfs check`'s `root 5 inode 257 errors 400, nbytes wrong`.

A QEMU kill is gentler than a power failure, because QEMU has already handed
what the guest wrote to the host's page cache. The adversarial half is
host-side, in `src/lib/fs/btrfs/write/src/tests/powerfail.rs`: the device records
every write and flush, and a crash is rebuilt as everything before the last
flush plus a random subset of what came after; two hundred scenarios cut at
twenty-five points each are opened, replayed, checked for consistency and
for any completed promise rolled back. `tools/common/test/btrfs-check-writer.sh` runs
it at that size, and `cargo test` a small one.

**Since the exit — `umount` writes the volume out (ferrix-e4,
2026-09-27).** `umount2` never called the filesystem's `sync`, so a volume
unmounted without one came back without what was written since its last
commit -- a black-box pass lost files, renames and the tail of a 40 MiB
file on the writable test disk. It syncs first now; a failed write-out
keeps the mount and returns the error, and `MNT_FORCE` unmounts anyway.
Still open from the same pass: no file grows past 32 MiB, and eight
concurrent writers leave the mount answering `EIO` (`docs/BACKLOG.md`).

**Since the exit — the marker.** The boot marker moved to `FERRIX-BOOT-OK
stages 1-12` on 2026-09-23. Stage 12's write check had run before it in
every boot since the exit; the marker was simply left behind when the stage
was called done.

**Since the exit — `/` on btrfs.** `cargo xtask run` and `run-compositor`
attach `build/root.img`, a 1 GiB volume made from the `root` fixture the
first time and kept after that. The kernel (`src/kernel/src/fs/root_disk.rs`)
starts on the initramfs, and once its disk driver is serving the volume it
does what Linux's `switch_root` does: mounts it, installs the initramfs onto
it when the archive differs from the one it last got, mounts `/dev`, `/proc`
and `/tmp` inside it, and starts init and every process after it with the
volume as `/`. It commits every 30 seconds, as Linux's btrfs does by default,
and once more when it powers the machine off itself. `--reset-root` starts
the volume over, and `--tmpfs-root` or `ferrix.root=tmpfs` keeps `/` in
memory. The test boots keep the tmpfs root. What is still an init's work,
which stage 15 owes: `pivot_root` itself, so the kernel's tmpfs can be
unmounted from under the switched root (the kernel allows it since every
`/` stands on a bottom mount, `docs/NAMESPACES.md` §2.1; the init has yet
to call it). The root disk is found by its btrfs
label, `ferrix-root`, as Linux's `root=LABEL=` finds one, and any other btrfs
disk from `vdd` on is mounted at `/data` inside whichever `/` processes have:
`test-rustc` gives the guest its compiler that way. A page of a mapped
btrfs file that nothing had read used to show zeros, which kept dynamically
linked programs from running off a btrfs volume. Stage 16 fixed that: a
fault now fills the page from the disk, as a read does. The static busybox
ran from the root before the fix, too: the boots that tried this ran `cat`,
`ls` and `awk` from `/bin` on the volume.

Beside the stage, and not in its points: writeback of pages written
through `MAP_SHARED` (2026-09-23). The page cache keeps no dirty bit, so the
file's VMO says when a mapping that may write has been made, and the next
writeback writes every page the file holds; the mount keeps the file while
anything else holds the VMO. Stage 20 found it: `lld` writes its output
through a mapping, and a proc-macro it linked read back as invalid metadata
once the file had left the cache.

---

