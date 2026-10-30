# Memory the kernel keeps after boot and after a game

Status: R1 design; R2 built (§3, "How it was built"). The certification consultant, ledger line 4971 (2026-10-30): R1 NOT YET (B1-B4), R2 OK IF R2.1-R2.6; F-99 (ledger 4988) found while reviewing R2, fixed with R2 under F1-F4.

## 1. Why

Measured on the N150 (16 GiB) on main-94d0c9df0: 265 MiB in use with no
desktop, 364 MiB with the desktop. Two items are larger than they need to
be, and neither is a program's:

* **The system, three times over.** The loader reads the initramfs into
  memory the map marks `Initrd`, which the buddy is never given
  (`MemKind::is_free_at_boot`). `fs::init` unpacks it into the tmpfs root,
  copying every file. When a root disk exists, `root_disk::switch_to`
  mounts it, installs from the same archive (the CRC stamp lets a later
  boot skip the copy), and from then on every process's `/` is the volume.
  Neither the archive (about 95 MB on the box image) nor the tmpfs copy of
  it (the same again) is read after that by anything but the processes
  started before the switch: devmgr and the disk driver, whose programs
  and libraries are mapped.
* **btrfs clean nodes.** Each writable volume keeps up to `CLEAN_NODES`
  (4096) decoded tree nodes in `WriteVolume.clean`, cleared only when the
  map is full; a 16 KiB node decodes to about that much again. The box has
  three writable volumes (`/`, `/data`, `/home`), and after Endfield's
  launch they keep 130-200 MB that nothing reclaims and `MemAvailable`
  does not count. The read-only mount cache (`NodeCache`, 1024 entries,
  4-16 MiB a mount) is small and stays as it is.

## 2. Releasing the system's copies (R1)

**When.** Once, after the switch has installed and published the volume
(`ROOT` set), or when the root stays in memory for good. "For good" means:
`ferrix.root=tmpfs`, or no root disk once `devmgr` has reported (the same
point at which `switch_after_devmgr` gives up today). A test boot, which has
no root disk, releases the archive but keeps the tmpfs: the kernel's checks
mount their fixtures there.

**The archive.** `ARCHIVE` becomes a lock around `Option<&'static [u8]>`;
`initramfs_archive()` becomes `with_initramfs_archive(|bytes| ...)`, held
for the install's read. The release takes it out under the lock, so no
reader holds a slice across it. Pid 1's inputs (`init::InputEntry::data`,
`&'static` slices into the archive today) are copied into the heap by
`set_inputs` first; they are a script and a command list, bytes to a few
KiB, and the program is read by path. Then the archive's pages, rounded
inward to whole pages, go to the buddy (`Frames::insert_free`), which
already describes them: the page array spans the lowest to the highest RAM
frame. `MemTotal` grows by what was given, as Linux's `Freeing initrd
memory` does, and the watermark is computed again from the new total.

**The tmpfs copy, after a switch only.** Unlink every entry the unpack made
(`Unpacked` lists them) in reverse order, except the directories something
is mounted on (`/sysroot`, `/dev`, `/proc`, `/sys`, `/tmp`, `/dev/shm`) and
their parents. An unlinked file that a process still has open or mapped
lives on as any unlinked file does (`mm/unlinked.rs`), and its pages go
when the last mapping does; the rest go at once. Nothing resolves a path
in the tmpfs after the switch except a process started before it, and those
are devmgr and the driver, which open nothing new by path from `/` (to be
confirmed by the check below, not assumed).

**Bounds.** One release, at most once per boot; the unlink walks the
unpack's list, a few thousand entries, on the switch's own thread, with no
lock held across entries.

**Requirements.** New, reserved as L.fs.N1-N3:

* N1: the initramfs archive's pages are given to the frame allocator once
  the root is final, and no reader of the archive outlives the release.
* N2: after a switch to a root disk, the tmpfs keeps only the directories
  mounts stand on; a file still open or mapped keeps working until closed.
* N3: pid 1's inputs survive the release unchanged.

**Evidence planned.** A boot check on a test boot with a root disk
(`test-btrfs` attaches one): `MemTotal` and free frames up by the archive's
pages within one page; reading every input after the release; the tmpfs
holding only the mount directories; a file held open across the release
still reading its bytes; the disk driver still serving after it. Controls:
no release (the counts stay), a release that skips copying the inputs (the
input check reads freed memory: caught by poisoning freed frames under the
check), unlinking a mount directory (the mount check fails).

## 3. btrfs clean nodes, reclaimable (R2)

**What.** The fs shrinker (`fs::shrink_dentries`, registered with
`user::cache::install_shrinker`) also asks every mounted writable volume to
drop clean nodes, oldest first, until `want` is met. Clean nodes are a
cache of what is on disk ("dropping them costs reads, never correctness",
`volume.rs`); dirty nodes are never touched. A volume is asked only under
its own lock with `try_lock`: a volume in the middle of an edit or commit
is skipped, never waited for, so a reclaim cannot deadlock against a
writer that is itself allocating.

**Order.** `clean` becomes an insertion-ordered map (a `BTreeMap` keyed by
address plus a `VecDeque` of addresses, as `NodeCache` keeps), so "oldest"
is cheap, and the clear-when-full becomes evict-the-oldest.

**Counted.** `/proc/meminfo`'s `MemAvailable` adds the volumes' clean bytes,
as Linux counts reclaimable slab (`SReclaimable`), and `SReclaimable`
reports them. The counts are the volumes' own, kept as nodes go in and out.

**Charge.** The nodes are kernel heap, charged to no job, so only a reclaim
whose scope is the whole machine takes them (`takes(owner, scope)` with the
kernel's owner); a job's `memory.max` reclaim does not.

**Bounds.** One shrinker call drops at most `want` bytes' worth, each node
an O(log n) remove; `CLEAN_NODES` is unchanged as the ceiling.

**Requirements.** New, reserved as L.btrfs.N1-N2:

* N1: a machine-wide reclaim short of pages drops clean btrfs nodes of
  every writable volume not busy at the time, never a dirty one.
* N2: `MemAvailable` and `SReclaimable` include the clean nodes' bytes.

**Evidence planned.** Host tests in `ferrix-btrfs-write` (oldest-first,
dirty kept, the counts); a boot check under `test-btrfs`: read a tree wide
enough to fill the cache, reclaim, the bytes come back to the heap and the
next lookup reads from the disk with the same result; a busy volume
skipped. Controls: a shrinker that drops a dirty node (the write check
loses an edit), `MemAvailable` without the nodes.

### How it was built

**F-99 first.** Reviewing this design, the consultant found that `load`
cleared the whole cache when it held `CLEAN_NODES` nodes, between the levels
of a walk whose `Path` still named the nodes it cleared; `next_item` then
took the missing node for the end of the tree, and a range scan (`readdir`,
an inode's deletion, the orphan list) ended early or skipped items. Now
`load` drops nothing (`volume.rs`), `next_item`'s climb answers an error for
anything but a level's last pointer (`tree.rs`, `next_leaf`), and the cache
is trimmed only between operations: by `Shared::lock` in the writable
mount's glue (`ferrix-btrfs-vfs` `rw.rs`), the one place the kernel takes a
volume for anything, each time it is taken (F1). The overshoot is argued per
operation rather than evicting what the operation has not touched (F2,
`docs/certification/MEMORY-AND-TIMING.md` §1.8).

**The cache.** `clean.rs`'s `CleanNodes`: a map by address and a
`VecDeque` of loads, each place stamped so that a node copied or dropped
from the middle leaves a stale place that eviction passes over; the queue is
closed up without allocating once stale places outnumber live ones. It keeps
the heap its nodes hold (`TreeNode::heap_bytes`, the lists' capacities) as a
running sum, stored per node so a remove subtracts exactly what the insert
added. `WriteVolume::trim_clean` brings it to `CLEAN_NODES`;
`WriteVolume::drop_clean` gives up to 64 nodes (`DROP_BATCH`), oldest first;
the commit's written nodes join it without evicting anything.

**The mount.** `Shared::lock` hands out a guard (`Held`) that trims after
taking the lock and publishes the volume's clean bytes into an atomic as it
lets go, so `RwBtrfs::clean_bytes` reads without the lock. `RwBtrfs::
drop_clean` takes the lock with `try_lock` and gives nothing when an
operation holds it.

**The kernel.** `fs::btrfs` lists every writable mount weakly as
`mount_rw` makes it (refusing the mount with `ENOMEM` when the list cannot
grow) and asks each in turn (`drop_clean`, `clean_bytes`). The fs shrinker
(`fs::shrink_dentries`) asks for the nodes after the dentry cache, and only
when `takes(quota::NONE)`: the nodes are charged to no job (neither btrfs
crate makes a `ferrix_kmem` charge), so only a reclaim of the whole machine
admits their owner. `/proc/meminfo` gains `SReclaimable`, and `MemAvailable`
adds the same bytes.

**Requirements.** `L.btrfs.119` (eviction between operations only, oldest
first, never a dirty node), `L.btrfs.120` (the machine's reclaim only, busy
volumes skipped, 64 a hold), `L.btrfs.121` (the exact count), `L.user.175`
(`SReclaimable` and `MemAvailable`).

**Evidence.** Host tests, `ferrix-btrfs-write` `tests/clean.rs`: with the
cache at `CLEAN_NODES`, the fs tree's root held and a leaf not, `next_item`
from a gap between two leaves, `prev_item` from slot 0 of a leaf (whose
pointer key the test lowers: only such a tree reaches that climb) and
`read_dir` from the gap answer what they should -- all three fail with the
old `clear()` put back; a trim drops the oldest and no dirty node; batches
until empty leave every dirty node and the edit commits; the count equals
the sum over the nodes after loads, edits and commits. `ferrix-btrfs-vfs`
`tests/rw.rs`: a mount asked for nodes from inside its own read gives none
and returns, and gives them all between operations in batches. The boot
check `fs/btrfs_write_check/clean_check.rs` (test-btrfs's `reclaim` line):
a wide directory made, synced and read on a fresh mount; `SReclaimable` and
`MemAvailable` hold its nodes; a job's reclaim takes none; the machine's
takes some; giving all back raises the free frames by at least a quarter of
the bytes (x86_64: 474,912 bytes, 124 frames); the listing read again comes
off the disk the same. The heap's per-processor caches are drained
(`mm::drain_caches`) before the frames are counted and after the nodes go
back, as R2.3 asks.

**A reclaim that frees heap but no frame** does not loop without end: the
page cache's fill and write retry at most 8 times, `memory.reclaim` counts
the bytes, and the fault path's retry ends with the cache, as the dentry
cache's does (MEMORY-AND-TIMING §2.2d).

### Where it stands

R2 and F-99 land as d0b034524 (the code, tests, boot check and
requirements), 3e1967b5f (doc comments), 60559327d (F-99 closed) and
2f6f02486 (the heap caches drained around the boot check's frame count),
with the heap caches' one-processor check made to retry a disturbed attempt
and say so (16d8d2526, a89cf40f6) and a softsign test that searched its own
log path fixed (94a24df29). Code review: OK IF K1-K3, ledger line 5004
(verdict 5009); the retry, OK IF H5-H6, line 5015.

* Controls, each `control: FIRED`: the old clear in `load`
  (rr-974fb47-f99-ctl, 3 host tests fail), a drop that also takes a dirty
  node (rr-4bcea58-dirty-ctl), `MemAvailable` without the nodes and a job's
  reclaim that takes them (rr-4bcea58-avail-ctl, rr-4bcea58-job-ctl, stage
  12 stops) -- counted for the landing hashes by the review, the R2 code
  being byte-identical since -- and `heap_allocated` not subtracting on
  a89cf40f6 (rr-a89cf40f6-ctl-not-subtracting, 5 of 5 attempts failing).
* Rows on a89cf40f6, each `run: PASSED`: build --release --arch all,
  test-btrfs, test-boot x86_64 kvm, kvm --smp 2, aarch64 tcg and armv7a
  tcg, test-restart --boot blk; every boot reads 0 of 5 single-processor
  attempts disturbed. The check row on the commit that records this.
* R1 waits on B1-B4.

**R2.5.** Nothing in the kernel reads `MemAvailable` but `/proc/meminfo`'s
render: `sysinfo` reports free frames, and no reclaim or OOM decision looks
at it. Outside the kernel, host measurement tools read the file
(`tools/common/game/rig.c`, xtask's compositor bench).

## 4. What it saves

On the box: the archive (about 95 MB) and its tmpfs copy (about the same)
at every boot with a root disk, and 130-200 MB of btrfs nodes given back
under pressure and counted as available meanwhile.
