# btrfsd: the filesystem server, and the block ring in user space

`docs/MICROKERNEL.md` §6.3 and §3's *Retired calls* (0x1048): under `fs=server`, btrfs runs as a process of its own, the two
crates byte-identical, and a block driver shares its ring with it, a protocol in user space. Ids: L.object.329-333, L.device.81-82. Native calls 0x10C3, 0x10C4 (§2). Panic code none: no kernel code but the start and those two. The customer's Q4 (a certified `btrfsd`) is decided;
the document set for that item is M7's. BOOT-AUTHORITY starts it (`.ferrix/partitions` or a default row, under K14).

**Where it stands:** ledger 6027 **OK IF BD.1-BD.12** and CO.1-CO.6, 6052 **OK IF H.1-H.7**, 6803 BL.1-BL.4. In this landing, dark: `fs=server` requires `personality=user`, which no profile builds. Rows on x86_64 alone, under KVM and through `gate.sh`: `test-btrfsd`, `test-btrfsd-restart`, `test-btrfs`, `test-powerfail`, `test-rustc`, each `--set fs=server`. **`built = "yes"` for `fs=server` is true on x86_64 alone**: aarch64 is a BACKLOG row. BD.8 is answered by the customer, 2026-11-04: `btrfsd`'s heap in `max` is its justified deviation, no heap only in `max_plus`. The manifest's entries for `ferrix-fsproto` and `ferrix-blkclient` say no kernel links them; they are `btrfsd`'s item at M7, and F-23's gate holds from here; no claim stands on them before M7 (BD.7).

**Departures from what was reviewed, for the diff check:**
* `CORE_OPEN` carries no device handle, since mk-lift's change: the server is told a location alone. So CO.1 falls away, and CO.2's check is the word against what
  the driver's HELLO says.
* READY's port has `WRITE|TRANSFER`, not `WRITE`: a handle crosses a channel from user space only with `TRANSFER`.
* `linuxd` makes each volume's channel and gives the server its end. This began as a way round a `Fault` whose cause was then found and fixed: K11's
  `copy_to_user_through` lacked `route` on its `Present` arm. It stays, since the server has no need to make it.
* One thread, one partition per instance (BD.4's second choice). So requests are served in turn, a pool is not used, and the protocol has no `id`.
* `linuxd` writes through to the server at each `write`, and keeps the page cache too; the server keeps a writable file's pages on its heap until the file is forgotten. Nothing
  evicts them but that, so a large write's memory is bounded only by the heap's reservation (512 MiB), past which writes answer `ENOMEM`. That is below what the kernel's store does.
* No decommit call exists on a native VMO (BD.2, H.7 name one), so "the driver decommits under a live request" has no test: what a driver can do is die or stop,
  which `test-btrfsd-restart` holds. Its control is therefore death with the hold in place.
* `test-rustc` and `test-powerfail` (8 cuts, 6 leaving a log the next mount replayed; `btrfs check` clean before and after each) pass under `fs=server`, on x86_64 under KVM. So does the fuzz target `fsproto_wire` for a minute's run.
* H.2 is withdrawn for `personality=user`, whose ring 0 builds no `make_combining` (ledger 6426, 6803 (vi); its load keeps it, so hb5 and hb5d are not owed): `Vmo::hold` refuses a sealed VMO itself and no call under this shape makes a kernel VMO write-combining, so `vmo_hold` takes no kernel view, and hb5 and hb5d are why. `make_combining` is the lift's copy's there.
* While BD.3 is missing a description with a files role and any safety device is refused, by parser and by checker (ledger 6316 P3.3).
* L.object.329-333 stay here and reserved, not in the model: `check-traceability.py` resolves a unit only in the item's crates, and `ferrix-fsproto` and `ferrix-blkclient` are `btrfsd`'s item at M7 (BD.7, BD.12, Q4). * Not built: a mount's description row (BD.3: every disk `devmgr` opens is this instance's, which holds only while there is one instance and no safety disk behind it); CO.2 compares two QM words, so it proves consistency only; BD.3 is what binds a disk.

**Controls that did not fire, and their successors (ledger 6933 BF.5):** hb3's first run, mbl-bc-cg6, mbl-bd-cg6 and mbl-bc-hb7. hb3 and hb2 planted a call a higher crate cannot name; mbl-bf-hb2, -hb3 plant `served` instead. cg6 expected an older sentence; mbl-g3-cg6 expects today's. hb7 measured a job's figure, which a dead job's charge leaves; mbl-bg-hb7 reads the kernel's own count. Ledger 6933 gives this line its verdict.

## 1. What moves, and what does not

| Thing | Today | Under `fs=server` |
|---|---|---|
| `ferrix-btrfs`, `ferrix-btrfs-write` (item ring) | in ring 0 | linked into `btrfsd`, **byte-identical**: same source, same tests, L.btrfs evidence carried (§6.2 *Moves*). Their manifest entry says so; `fast`/`max` no longer link them in ring 0, which the shape gate adds to its list |
| `ferrix-btrfs-vfs` (load ring) | in ring 0 | in `btrfsd`, as the server's model of a volume, unchanged: the `FileSystem` and `Inode` it already is. Its VMO pages are `btrfsd`'s (§4) |
| `fs/btrfs.rs` glue: `Disk`, `Device`/`WriteDevice` over a `BlockDevice` | load | `btrfsd`'s `Disk` over `ferrix-blkclient` |
| `interfaces/block_ring` core | load | **not built**, a client of the ring instead: `ferrix-blkclient`, which is `ferrix_blkring::kernel`'s side of the ring, already a library, on channels, VMOs and ports |
| The VFS's view of a volume | `fs/btrfs.rs` mounts in the VFS | `fs/served.rs` in the load (`linuxd`): a `FileSystem` and `Inode` over the protocol of §3 |

`fs=lift` (mk-lift's stopgap, btrfs inside `linuxd`) uses the same client library from `linuxd`, so the disk works in both; only `fs=server` is the
claim's shape.

## 2. The block ring in user space

BLOCK-RING.md is unchanged on the driver's side: HELLO, READY, the ring, the doorbells. Driver source is unchanged. `devmgr` asks for a control channel by
`CORE_OPEN` (`ferrix-devmgr-proto`) on the channel the kernel gave it; the kernel handed the other end to the server (`btrfsd` for `BLOCK_RING_CREATE`,
`linuxd` for the rest; BOOT-AUTHORITY §3). The server:

1. makes a channel pair and answers `CORE_OPENED` with the driver's end, `CONTROL_RIGHTS`;
2. reads HELLO, validated by `ferrix_blkring::control`, exactly as the core does, refusing with the same `Refusal`s;
3. maps the ring and data VMOs read-write. The ring's indices are the other side's to write: `KernelSide::attach` already treats them as hostile (BLOCK-RING §4), and
   its memory trait is implemented over a mapping as it was over frames;
4. makes a port and answers READY with its write handle;
5. runs the ring from the file server's threads: a read or write takes a region, posts, rings, waits on the port, under a bound. A driver that dies closes the control channel; the
   parked-disk rule (`take_up`) is the same state machine, kept: requests wait for its next driver, up to the bound, then `EIO`.

**PUBLISHED** to devmgr goes on the `CORE` channel's answer: a `CORE_PUBLISHED {location}` from the server, so devmgr's start waits as it does today.

**Bus mastering, pins (BD.1).** The driver pins its data VMO as today, in its device's domain. A server holds the device only by a handle with `TRANSFER` alone, to read `device_info`, which it closes after reading
(CO.1): no MANAGE call answers it, shown by a check. `hello_accepted`'s two kernel steps (a dead driver's pins released from quarantine, `verify_config`) cannot wait for a quiesce or a pin, neither of which proves a reset. So
**`DEVICE_SERVED` (0x10C3): `(device)`, needing `MANAGE`, `personality=user` only**, is made by devmgr on the server's `CORE_PUBLISHED`, for every ring kind. A driver's reset comes before its HELLO, and the server says PUBLISHED only after that HELLO. An early call by a lying
server or devmgr widens nothing: devmgr holds the device anyway and a released quarantine frees pins of a driver it has already killed; V-20 states it. L.device.81 with its check, and controls for release at quiesce and at pin, each FIRED.

**The ring's memory (BD.2): a kernel hold a server can take.** Today `attach` takes `take_kernel_view` and `hold`. A mapping alone is not enough: a driver's decommit, or its job at its limit, would fault a certified server, and a pager-backed VMO would park its thread forever. **`VMO_HOLD` (0x10C4): `(vmo)` → handle**, needing `READ | MAP`, `personality=user` only: refuses pager-backed, file, physical and write-combining kinds (`BAD_STATE`), commits every page, holds them for as long as the handle lives (`Vmo::hold`, so `decommit` skips them and no truncation exists), and answers `Object::Held`, with `TRANSFER` alone, an object with no calls. Chosen over the server making the VMOs, since no driver changes, and over copies through `VMO_READ`, since the data path stays a mapping. Charged to the driver who committed; a `NO_MEMORY` is a refused HELLO. A mapped page then never faults for want of a frame; a table page's fault is the server's own limit. **Control: the driver decommits under a live request: the server lives, and the request answers `EIO` or data, never a fault.** `linuxd`'s shared VMO (§3) is `btrfsd`'s own memory, made by it, held the same way against `linuxd`'s map of it.

**Who pays.** The server's ring memory is the driver's VMO, charged to the driver. A client's I/O is K7's: a ticket on its channel.

## 3. The file protocol (`ferrix-fsproto`)

A control channel from the kernel: `HELLO`, then per disk `OPEN`, `LABEL` and the raw block ops for its `/dev` node. One channel per mount between `linuxd` and `btrfsd`, opened by `OPEN` with a shared VMO, closed by `CLOSE` at unmount, and with a disk's last channel the volume goes. Requests are channel messages, answered in turn; no ticket yet, so a client's I/O is charged to the server (K7's row owed). Fixed little-endian structures, host-tested, fuzzed on the server's side: **`linuxd` is a QM client, so every request is hostile input to a
certified server** (ASR-9, T.MEDIA keep their meaning for the volume; a new T.CLIENT for the channel).

| Op | Arguments | Answer |
|---|---|---|
| `LOOKUP` | dir handle, name | handle, metadata |
| `FORGET` | handle | -- |
| `GETATTR`, `SETATTR` | handle, `SetAttributes` | metadata |
| `READ`, `WRITE` | handle, offset, length, in the request's slot of the shared VMO | bytes, new length |
| `READDIR` | dir, cursor, slot | entries, in the slot |
| `CREATE` `MKDIR` `SYMLINK` `MKNOD` | dir, name, kind, mode | handle, metadata |
| `LINK` `UNLINK` `RMDIR` `RENAME` | as the VFS trait | -- |
| `READLINK`, `SETLEN`, `FSYNC`, `SYNC`, `STATFS` | as the trait | as the trait |

Handles are small numbers in a per-channel table in `btrfsd`, refused past a bound, charged to the client's ticket. Credentials are `linuxd`'s to judge: the VFS checks permissions above `Inode`, as today, and `btrfsd`
sees no uid but what `SETATTR` and `CREATE` carry. A hostile `linuxd` can do to a mount what a root program can: the partition's own data. **It cannot touch another mount: a channel is one mount, one volume.**

**Mapped files** go through K3 at a later step: first the served `Inode` has `mapping()` over a VMO `linuxd` fills by `READ` (as a tmpfs-like private cache in `linuxd`), so `mmap` and `exec` work; writes through a shared map go back by `WRITE` on `msync` and
close; then a K3 pager in `btrfsd`, with mk-m2's page cache on K3. `fs/pages.rs` stays `linuxd`'s page cache policy above it.

## 4. The program

`src/user/system/native/btrfsd`: `no_std` on `ferrix-rt`, a heap (`lode`'s pattern), threads by `THREAD_CREATE`. `ferrix-btrfs-vfs`'s `Storage` is a heap-backed `Pages`. Its `Parker` is K5's address wait.
Started measured under K14, by the manager when a description names it, else by the kernel's table in `boot_authority`. It may be restarted only with its volumes unmounted cleanly: a crash ends the partition's mounts with `EIO`, the journal-free CoW format's own recovery on the next mount (`test-powerfail`).

## 5. Evidence and gates

* `test-btrfs`, `test-powerfail`, `test-rustc` under `--personality user`, unchanged, are M5's gate.
* L.object.329: the client refuses a HELLO `KernelSide` refuses (the checks in `interfaces/block_ring/check.rs` that are about the ring's rules run against the client in a host test, a re-homing with its ids).
* L.object.330: a request past a bound, a bad handle, a name with `/` or NUL, an offset overflow: refused, by a host test and a fuzz target, `fsproto_wire`.
* L.object.331: **one volume per channel**: a handle of one channel names nothing in another, with a control.
* L.object.332: the driver's death answers `EIO` within the bound, never a hang.
* L.object.333: `btrfsd`'s image is on the certified list under `max` (K14 §5).
* `check-item-boundary.py` carries `ferrix-btrfs*`: "ring 0 under `fs=kernel`; `btrfsd` under `fs=server`"; the shape gate refuses them in ring 0 under `fs=server`.

**Memory (customer, 2026-11-04).** Under `max`, `btrfsd` allocates from its own heap on its own reservation, the stated deviation K16 has for every job. No allocator after boot is `max_plus`'s, with PR #38.

### 5.1 What moves with the crates (BD.9)

"Byte-identical" moves only what the crates are. An id whose check is a kernel boot check or kernel reclaim has a successor here:

| Id | Today's check | Under `fs=server` |
|---|---|---|
| L.btrfs.120 (only the machine's reclaim takes clean nodes) | kernel reclaim | no reclaim reaches `btrfsd`'s heap: a server's memory is its job's and answers `ENOMEM` past its reservation. Carried for `fs=kernel` only, scoped |
| L.btrfs.121 (clean bytes counted exactly) | kernel reclaim's count | as L.btrfs.120, scoped |
| L.user.175 (clean btrfs nodes available) | kernel memory reporting | as L.btrfs.120, scoped; `btrfsd` reports nothing to `/proc/meminfo` |
| in-kernel disk checks of stage 12 (`btrfs_write_check`) | a boot check | `linuxd` runs the same check over the protocol; `test-btrfsd`, `test-powerfail` |
| the crates' host tests | `cargo test` | unchanged, carried |

### 5.2 What a client can do (T.CLIENT)

A Linux partition's `linuxd` is QM and its requests are hostile: T.CLIENT in SECURITY-TARGET. `btrfsd` knows no uid, only what `SETATTR` and `CREATE` carry, so that a QM client's root is its own partition's data and no more. Its handles name nothing on another channel (host test, `one_channels_node_means_nothing_on_another`), its requests are bounded before use (`fsproto_wire` fuzz, host tests). The ids are L.object.329-333, written with the crates' item classification at M7 (Q4).

## 6. Questions

* **Q1.** §2's `hello_accepted`.
* **Q2.** BACKLOG row 398 (b), the commit reserve: unchanged in `ferrix-btrfs-write`, so it gates as it does.
* **Q3.** Order: client and read-only serve with a boot row; then write; then K3. `fs=lift` uses the client meanwhile. Agreed?
* **Q4.** `ferrix-btrfs-vfs` was load, and a certified `btrfsd` links it. It moves to the item's crates or becomes `btrfsd`'s own argued code: which? My view: item, 2,730 product lines, with host tests; requirements written at M7.
