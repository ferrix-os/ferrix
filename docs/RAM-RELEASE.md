# Memory the kernel keeps after boot and after a game

Status: design. The certification consultant, ledger line 4971 (2026-10-30): R1 NOT YET (B1-B4), R2 OK IF R2.1-R2.6; F-99 (ledger 4988) found while reviewing R2, fixed with R2 under F1-F4.

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

## 4. What it saves

On the box: the archive (about 95 MB) and its tmpfs copy (about the same)
at every boot with a root disk, and 130-200 MB of btrfs nodes given back
under pressure and counted as available meanwhile.
