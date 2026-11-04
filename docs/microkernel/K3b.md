# K3b: the page cache's dirty state, on K3, still in ring 0

**Design, 2026-11-04 (mk-m2). Reviewed: OK IF (1) to (12), ledger 5981, folded in here.** `docs/microkernel/K3.md` §7
(reviewed, ledger 2858 (8), (9)), stage M2. Step 1 is on the stack: `Source::Kernel` and one fault
path. This is step 2.

Ids: **H.MEM.28**, **L.user.206-212**. Estimate: **8 points**. Behaviour on today's profiles
changes only in *which* pages a shared writable mapping reports for write-back: the row says so.

## 1. Today

`Vmo::take_mapped_writes` answers **every page the object holds** while a mapping may write, because
a mapped write marks nothing (`mapped_written`). btrfs (`vfs/src/rw.rs` 905) then writes all of it
at every `fsync`, and `reclaimable()` answers no while a mapping may write.

## 2. The change

An object with `Source::Kernel` **tracks** K3's per-page state, whether or not a mapping may write it:

* `Vmo::tracks_dirty()`: pager-backed or sourced. It replaces `is_pager_backed` at each fault site
  (5981 (4)): `writable_page` (and so `writable_in_place`), `read_only_paged`, and the look at
  `user/space.rs`'s `paged_at`; `ask`, which returns at once with no pager, marks here. So a Clean page
  of such an object is installed read-only by every mapping and its first write faults, `TRAP_DIRTY`
  off: the fault marks it Dirty and installs it writable. A kernel copy through a mapping
  likewise. A fork's child starts with an empty root, refaults and marks on its first write.
* `write(2)` needs no mark: the btrfs source keeps its own dirty set, unchanged.
* **`begin_mapped_writes`** (behind `Pages::mapped_writes`): the present Dirty pages become
  AwaitingClean, **and the Dirty runs only lose their translations, with one shootdown before it
  returns; with none there is no shootdown** (5981 (6)). So a write after it faults and is Dirty again:
  no write is lost from the dirty set by a write-back that began before it (H.MEM.23's rule). Its
  answer is exactly the pages it turned.
* **`Pages::mapped_writes` answers a `Result`** (5981 (3)): with no memory for its list no state flips
  and `fsync` answers `ENOMEM`. The earlier `unwrap_or_default` meant `fsync` answered OK without the
  mapped pages (pre-existing on `develop`; the finding number is the product owner's to reserve).
* **`Pages::mapped_writes_done(failed_from)`** ends it: AwaitingClean to Clean, from `failed_from` on
  back to Dirty. "Done" means extents in the open transaction, not a commit. A guard in `write_back`
  runs it on every way out, failed from the first page until the runs are written.
* **Locks** (5981 (1), (2)): `mapped_writes()` used to be called inside `write_back`'s `dirty` `SpinLock`.
  Begin now runs before it, under the volume's `SleepLock` only, and asserts it may block; begin and done
  lie in one hold of the volume lock, which serialises an inode's write-backs. `rw.rs`'s rule "nothing
  calls into the page cache holding the volume lock" gains this exception: begin reads no file and waits
  for nothing but a shootdown. `MEMORY-AND-TIMING.md` states that shootdown under the volume lock.
* **Reclaim** takes a page only if it has no state, per page under the pages lock (`take_clean_into`),
  whether or not a mapping may write (`unwritten_by_mappings` goes). `user/cache.rs`'s two readers of
  `reclaimable()` (564 `take_from`, 712 `totals`) now see every sourced object, and `dirty_pages` adds
  the non-Clean pages, so `totals`' reclaimable count holds none. `take_poisoned` looks at the state too,
  and stage 12's `reclaim_hook` case holds as it did: the hook's write makes the page Dirty first.
* **Stale states** (5981 (5)): a take removes its pages' entries, and `insert_absent` clears one, so a
  page refilled after a cut starts Clean and the map does not grow.
* **A device's write** (5981 (10)): `VMO_PIN` refuses a sourced object as it refuses a pager's
  (`unpaged_vmo_in`), so no device writes such a page unseen.
* tmpfs and memfd objects have no source and are not tracked: they keep `mapped_written`,
  `take_mapped_writes` and L.user.43.
* A write to a present file page can now end in the fault's OOM path: a `states` entry is one allocation,
  fallible, `Refusal::NoMemory` to `OutOfMemory` (5981 (12), Q3). `MEMORY-AND-TIMING.md`: `states` holds
  one entry per non-Clean page, on the kernel heap, paid by the job that faulted.

## 3. Rules

**H.MEM.28** (parent O.ISOLATE): *A write through a shared mapping of a disk file shall keep its page in the set
`fsync` writes until a write-back that began after the write has ended; a page in that set shall not
be reclaimed, and a page not written shall not be reported.*

| Id | Rule | Case | Control |
|---|---|---|---|
| L.user.206 | A Clean page of a sourced object is installed read-only by every mapping, and its first write through a mapping marks it Dirty | `user/dirty_check.rs`, through `oom::user_fault`: leaf read-only, then writable, state Dirty, a fork child's too; `btrfs_write_check/mapped_check.rs`: a ring-3 store on all three architectures | K3B-C1: installed writable |
| L.user.207 | `begin_mapped_writes` answers exactly the Dirty pages, turns them AwaitingClean, and takes their writable translations down before it returns | of 8 pages, the 2 written answered; leaf absent, another page's kept; shootdowns advanced; silent with none | K3B-C2: answers every page |
| L.user.208 | A write after `begin_mapped_writes` makes its page Dirty again, and `mapped_writes_done` leaves that page Dirty | the third write's page answered by the next begin | K3B-C3: done clears Dirty too |
| L.user.209 | A failed write-back leaves its pages to be reported again | `done(false)`: the next begin answers them | K3B-C4: not put back |
| L.user.210 | Reclaim takes Clean pages of such an object, mapped-writable or not, and neither Dirty nor AwaitingClean ones; a reclaimed page reads back as the source has it | three pages, one of each | K3B-C5: Dirty taken |
| L.user.211 | A write through a mapping reaches the disk across a power cut once `fsync` returned (5981 (7): `msync` writes nothing; `MS_SYNC`'s row is in BACKLOG) | **`test-powerfail`'s new mmap round** (below) | **K3B-C6: a mapped write left Clean, which the round must catch** (2858 (8)) |

Checks: `user/dirty_check.rs`, a pure core file, not raised, a `dirty` boot line in stage 6;
`fs/btrfs_write_check/mapped_check.rs`, stage 12's btrfs-rw line, for the ring-3 store and the
fail-closed `fsync`. Controls C1-C5 through `gate.sh control --expect`, x86-64 KVM, C2 and C3 on aarch64.

## 4. `test-powerfail`'s mmap round, first, in its own landing

`fs/btrfs_powerfail.rs`'s churn had no mmap. Each fourth pass now writes its body **through a shared
mapping** of the file: a space of the churn's own maps it (`map_file`, `copy_to_user`), then `fsync`,
then the trailer by `write`, judged as every file is: a trailer promises its body's CRC. It passes on
today's `take_mapped_writes` (f736f07f2: 8 power failures, 5 leaving a log). Its control on today's code:
`take_mapped_writes` answering none, FIRED by the round. Then K3B-C6 on K3b's code.

## 5. Gates

As K3.md §12, ledger 2858 (9): the image row; `test-btrfs`, `test-powerfail`, `test-rustc`,
`test-btrfs-pressure`, on the four boots and armv7a `--smp 2`, PASSED with the same check counts;
`test-rustc`'s wall time reported beside `develop`'s. F-37's charging tests. Coverage carried, with
`take_mapped_writes`'s anchors listed.

## 6. Questions

* **Q1** Extra faults: a shared-writable file page takes one more fault per write-back cycle. No row
  times it; `test-rustc` reports. Recommendation: accept; it is Linux's `page_mkwrite` shape.
* **Q2** tmpfs and memfd untracked, keeping `mapped_written` for them. At M5 they are `linuxd`'s.
  Recommendation: yes.
* **Q3** Is the page state's map (`states`, a `BTreeMap` entry per non-Clean page, fallible at a
  write fault: the fault's `OutOfMemory`) acceptable on the file path, which never allocated in a
  write fault to a present page before? Recommendation: yes, as K3's pager VMOs; the fault takes
  the OOM path.

## Where it stands

* 2026-11-04: diff check OK IF (c), (d), (e), ledger 6162. **OK IF (1)-(12)**, ledger 5981; Q1 as (6), Q2 yes, Q3 yes. Code on `feature/mk-m2`: boots on
  x86-64 with `dirty` and the btrfs-rw line; `test-powerfail`'s round passes before it.
