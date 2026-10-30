# Memory the kernel keeps after boot and after a game

Status: R1 design revised for B1-B4 (§2); R2 built (§3, "How it was built"). The certification consultant, ledger line 4971 (2026-10-30): R1 NOT YET (B1-B4), R2 OK IF R2.1-R2.6; F-99 (ledger 4988) found while reviewing R2, fixed with R2 under F1-F4.

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

Revised 2026-10-30 after the consultant's NOT YET (ledger 4971, B1-B4) and
the customer's direction for B1 (keep devmgr's restart set in the kernel,
as MINIX 3's `service -c` and Fuchsia's bootfs do).

**Measured on the box image** (`main-image/1bcaf5652`, built 2026-10-30
from the box's tree; `FERRIX/INITRD.IMG`, command line `ferrix.init=/sbin/init
ferrix.devmgr=init`): the archive is 95,615,684 bytes, 23,343 whole pages;
it holds 3,003 entries (2,120 files, 502 directories, 381 links, no
`.ferrix` inputs) whose file bytes, unpacked into the tmpfs a page at a
time, are 24,639 pages (100,921,344 bytes). `lib/firmware` holds nothing.

### The kept set (B1)

On devmgr's first start `read_files` reads `/sbin/devmgr` into a
`vmap::Buffer`, the manifest, and every driver it names into a VMO of its
own (`Files`). From now on the kernel keeps them: a `Kept` set in
`discovery/devmgr.rs`, made by that first read, and every later start --
pid 1's `devmgr_start` after a `svc restart devmgr.service` or after devmgr
died -- is handed the same names, a new `Arc` of the same VMOs and the same
program bytes. No start after the first reads the tmpfs or the disk;
`READ_FILE` is used once.

**Which set.** Every driver the manifest names, not only the root disk's
path. On the box: devmgr 78,368 bytes (20 pages) and 20 drivers 1,498,760
bytes (378 pages), 1,630,208 bytes in all. The root path alone (devmgr,
`blk`, `nvme`, `ahci`, `sdmmc`) would be 91 pages, 1.26 MB less -- but
devmgr already holds every image VMO for its whole life (`Given::images`,
`devmgr/src/main.rs`), so while a devmgr runs the kept VMOs are the pages it
holds anyway, and what the kept set adds is devmgr's own 20 pages (81,920
bytes) and the names. Only in the moment between a devmgr's end and the
next start does the kernel alone hold the 378 driver pages. Keeping all
needs no second source, no rule for which drivers the root needs on which
machine, and no ordering "the block driver first, then the rest from the
volume"; that policy would be ring 3's (devmgr's) and is not worth 1.26 MB
held only between two devmgrs.

**Unchanged by restarts.** The kept VMOs are written once, by `vmo_of`,
before the first DEVICES; the kernel hands them with `READ | TRANSFER`
only, so no process holds a handle that writes or maps them writable. A
driver update (`drvupdated`, `docs/DEVMGR.md` §4.1) gives devmgr new VMOs
of its own and does not touch the kept set: a devmgr restart starts the
boot images, as re-reading the tmpfs did before.

**Follow-up only:** keeping the block driver alive across a devmgr restart,
in a job of its own, so the root disk never stops serving.

### When, and what goes

Once, when the root is final:

* the kernel path (`ferrix.devmgr` not `init`): in `kmain` after
  `finish_memory`, after every boot check and before `say_booted`'s memory
  line and pid 1;
* `ferrix.devmgr=init`: at the end of `root_disk`'s `after_devmgr` task, on
  every way out of it (switched, no root disk, devmgr late), so after the
  install.

What goes:

| Boot | Archive | tmpfs |
|---|---|---|
| a switch to a root disk happened | given back | the archive's entries unlinked |
| no root disk, `ferrix.root=tmpfs`, the live ISO | given back | kept: it is `/` |
| test boots (no root disk) | given back | kept: the checks' fixtures live there |

A boot with checks that switched (`test-restart --boot blk`, `--boot nvme`,
`test-disk-offline`) releases the tmpfs after its checks, which is why the
release on the kernel path comes after `finish_memory`.

### No reader outlives the archive (B2)

`ARCHIVE` becomes `Once<SleepLock<Option<Initrd>>>`, `Initrd` the physical
base and length; nothing `'static` points into it. `initrd()` and its SAFETY
note ("for the life of the system") go. The one way in is
`with_initramfs_archive(|bytes| ...)`, which holds the lock across the
closure and makes the slice inside it (the new `unsafe`, `(KMEM)`: the
frames are the loader's `Initrd`, never the buddy's while the `Option` is
`Some`, and the release takes it out under the same lock before handing a
frame back). The closure's borrow cannot leave it. A sleeping lock, since
the install holds it across a 95 MB unpack onto btrfs.

Every reader, and where it runs:

1. `fs::init`: the unpack, `init_entries` for pid 1's inputs and the AoU-24
   check (`/.ferrix` absent) -- at bring-up, before anything can release.
2. `root_disk::install` (the stamp's CRC and `unpack_over`) -- inside the
   switch, which comes before the release on both paths.
3. `init::Inputs` (program, script, commands) and `InputEntry::data`:
   `set_inputs` copies each input it takes into a `vmap::Buffer` kept in
   `INPUTS` for the boot (a built-in program is megabytes: 1,306 KiB of zinc
   on `test-restart`'s image; the box carries none). `Inputs` keeps
   `&'static [u8]` -- into those buffers, which are never freed, not into the
   archive; `Image::BuiltIn` borrows the same. `InputEntry` keeps the
   archive's lifetime only for the call.
4. The boot checks: `fs::check::run` reads the `Report` (counts), not the
   archive; `init_check` judges entries it makes itself; nothing else names
   `ARCHIVE` (`git grep initramfs_archive` before the change: `root_disk.rs`
   alone).
5. The tmpfs release below reads the archive's names, under the lock, just
   before the archive goes.

The release: take the `Option` under the lock, then
`mm::give_back_initrd(base, len)`, which hands the whole pages inside
`[base, base + len)` to the buddy under `FRAMES` with
`Frames::insert_free` (it takes only `Reserved` frames, so a page the
loader shares with anything else at either end is not given), adds them to
`managed`, and stores `watermark_for(managed)` in the same hold.

### The tmpfs, after a switch (B3)

**The list.** The archive's own entries, read under the lock just before it
goes: `ferrix_vfs::initramfs::remove(ns, ctx, archive, spare)` walks them in
reverse order (a directory's contents before it), unlinks each file, link
and node and removes each directory, skipping pid 1's inputs (never
unpacked) and every name `spare` keeps. A directory something is mounted on
answers `EBUSY` and one that still holds a spared name `ENOTEMPTY`; both
stay, counted. Host-tested in `ferrix_vfs`. What the checks made in the
tmpfs themselves is not in the archive and stays (only check boots make
any).

**The spared set.**

* the mount points and their parents: `/dev` (and `/dev/shm` inside it),
  `/proc`, `/sys`, `/tmp`, `/sysroot` -- by `EBUSY`, not by name;
* `/lib/firmware` and its parents: the display core reads
  `drm.edid_firmware`'s file beneath it through the kernel's own namespace
  (`interfaces/display/edid.rs`), at any card's HELLO, a driver restart's
  included. 0 bytes on the box image. Reading it from the volume instead
  would put a disk read in the HELLO path, which may come while the disk's
  own driver is restarting.

Nothing of devmgr's: its restart set is the kept copies above.

**Opened by path after the switch, from the tmpfs.** The kernel resolves in
its first namespace (`fs::namespace().context()`), whose `/` is the tmpfs,
in these places outside checks: `read_from_root` (devmgr's files: now read
once, before any switch), the EDID read (spared), the console's
`/dev/console` and devfs's `/dev` notifications (a mount), and the
filesystems' own mounts at bring-up. Processes started before the switch:

* the kernel path: devmgr (reads `/proc/cmdline` once, at its start, before
  the switch; no other path call) and its drivers (native processes, no path
  calls); the checks' processes have ended;
* `ferrix.devmgr=init`: pid 1, which the switch moves onto the volume
  (`root_disk`'s module comment): everything it opens after resolves there,
  and what it had open stays open; devmgr and the drivers as above.

So after the release the tmpfs is reached only through files already open
or mapped: pid 1's own program on `ferrix.devmgr=init` (`/sbin/init`,
1,310,720 bytes, 320 pages, mapped while pid 1 runs). An unlinked tmpfs
file lives as long as an `OpenFile` or its VMO is held, and its pages go
with the last.

### Computed once from the managed frames (B4)

| What | Where | After the release |
|---|---|---|
| `WATERMARK` | `mm::init` | recomputed under `FRAMES` with the insert |
| MemTotal, MemFree, `sysinfo` | `procfs/render.rs`, `syscall/system.rs` | read live each call |
| a tmpfs's `statfs` size, half the managed frames | `fs/pages.rs` `capacity` | read live each call |
| the pin ceiling (a quarter) and the Intel GPU's pin budget | `object/pin.rs` `count_ceiling`, once at stage 10 | kept: up to 23,343/4 frames lower than it could be; a lower ceiling only refuses a pin sooner, and the GPU's budget may already be a domain's |
| the `memory` boot line | `say_booted` | printed after the release, so it shows the new total |
| quotas, `memory.max`, kmem defaults | `object/quota.rs`, `ferrix_kmem` | none derive from the managed frames (`git grep managed_frames`) |

### Requirements

The reserved ids, written in `docs/sysml` by the landing and released from
`requirement-reservations.json` in that commit:

* **L.mm.99**: the initramfs archive's whole pages go to the frame
  allocator once the root is final, at most once a boot; MemTotal and the
  watermark are computed from the new managed count.
* **L.mm.100**: no reader of the archive outlives its release: the archive
  is reached only under its lock, and answers nothing after the release.
* **L.init.5**: pid 1's inputs survive the release unchanged, and none of
  them lies in the released pages.
* **L.user.173**: after a switch the tmpfs keeps only the mount points, their
  parents and `/lib/firmware`; a file open or mapped across the release
  keeps its bytes.
* **L.user.174**: every devmgr start after the first starts from the kept
  set, never from a path.

### Evidence

A boot check around the release itself (`fs/release_check.rs`), on every
boot with checks, the four test boots and `test-restart --boot blk`
included; it takes its "before" just before the release and its "after"
just after:

* the managed count up by exactly the archive's whole pages, every one of
  them no longer `Reserved` in the frame array, the watermark equal to
  `watermark_for(managed)`, `with_initramfs_archive` answering `None`
  (L.mm.99-100);
* each input's CRC the same as before, and no input's bytes inside the
  released range (L.init.5);
* on a switched boot: the tmpfs's root walked, holding only the spared set;
  a file opened and its VMO held before the release reading back the same
  bytes after (L.user.173).

Gates:

* `test-restart --boot blk`: the check above on a ferrix-root boot, then its
  two rounds of disk drivers killed and restarted after the release;
* `test-init` (`ferrix.devmgr=init`, a fresh root disk): its
  `svc restart devmgr.service`, after the release, now also requires the
  restart line to say "from the kept set" and a file written on `/`,
  synced and read back afterwards, which needs the restarted `blk` (L.user.174);
* `test-disk-offline`, `test-iso` (the live ISO: root stays tmpfs, archive
  given back), the four `test-boot`s.

Controls, each `gate.sh control ... --expect`:

1. no release on the kernel path: the managed count does not grow;
2. `set_inputs` keeping the archive's slices: an input lies in the
   released range;
3. the release leaving the `Option` set: the archive still answers;
4. the watermark not recomputed (a test boot, where the total is below the
   cap): the watermark is not `watermark_for(managed)`;
5. the tmpfs release unlinking nothing: the tmpfs holds more than the
   spared set;
6. devmgr's restart reading through `READ_FILE` again (the B1 control):
   `test-init`'s second devmgr is not started, the tmpfs's files being gone.

### What it gives back on the box

The archive's 23,343 pages (95.6 MB) and the tmpfs's 24,639 pages
(100.9 MB) less pid 1's mapped program (320 pages): 47,662 pages, about
195 MB, at every boot with a root disk. The kept set costs 20 pages
(81,920 bytes) beyond what devmgr holds already.

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
