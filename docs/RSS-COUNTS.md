# Kept resident counts

A read of a process's `/proc/<pid>/status` or `/proc/<pid>/stat` reports its
resident set (`VmRSS`, `RssAnon`, `RssFile`, `RssShmem`; `stat`'s field 24).
Until this change every read counted it afresh, and this document says how
the counts are kept instead, as pages come and go, and what that costs and
guarantees. Session ef-30fps, 2026-11-12; the row is `docs/BACKLOG.md`'s "a
large program's `/proc/<pid>/stat` and `status` cost".

## 1. Why

Arknights: Endfield under Wine on the N150 (about 5.5 GB resident, 181
threads, 14,441 regions) reads its own `status` (Wine's
`GetProcessMemoryInfo`) and `stat` (`GetThreadTimes`) dozens of times a
second in bursts of 4 to 10 s every 30 to 190 s. A read took 17.2 ms
(`status`) and 16.6 ms (`stat`); in a burst one game thread spends 4.4 s of
every 10 s in `Vmo::committed_in` and `BTreeMap::range` under
`AddressSpace::resident_split`, all four processors are busy, and the frame
rate falls from 30 to 27-28, worst 22. Leaving out the gaps between an
object's adjacent regions (tried on 2026-11-12) made it no cheaper (19.6 ms):
the cost is the per-read pass over every region, not any one part of it.

`statusbench` (`~/.local/share/ferrix/ef-30fps/faultbench/statusbench.rs`:
15,000 regions in Wine's pattern, one `mmap(PROT_NONE)` reserve with every
second two-page piece `mprotect`ed read-write and touched, and 64 MiB more
touched; 200 reads of each file) measures it in a guest:

| Release build, KVM guest on nazuna (xtask's `-cpu`) | `status`, µs a read (mean) | `stat` |
|---|---|---|
| Linux 7.0 as the guest | 2.5 | 1.6 |
| Ferrix develop 1aea9a223 | 522 | 511 |

(Figures of 1,860-1,933 µs in earlier notes were a dev build.)

The customer's goal (2026-11-12) is the same or better than Linux.

## 2. What Linux does

`include/linux/mm.h`, `mm/memory.c`, `fs/proc/task_mmu.c`, `fs/proc/array.c`
(6.18): each `mm_struct` keeps `rss_stat[NR_MM_COUNTERS]`, per-CPU counters
(`percpu_counter`) for `MM_FILEPAGES`, `MM_ANONPAGES`, `MM_SWAPENTS` and
`MM_SHMEMPAGES`. They move where a page table entry is installed or cleared:
`add_mm_counter`/`inc_mm_counter`/`dec_mm_counter`, with the kind from the
folio (`mm_counter`: anonymous, else swap-backed is shmem, else file).
`task_mem` (`status`) sums the per-CPU parts (`get_mm_counter_sum`, O(CPUs));
`do_task_stat` reads the approximate total (`get_mm_rss`). `total_vm`,
`data_vm`, `stack_vm` and `locked_vm` are kept the same way by
`vm_stat_account` as regions come and go. Nothing a reader does grows with
the regions or the pages.

Linux's figure counts **translations**: a page is in a process's RSS while
that process has a present entry for it. Ferrix's figure (L.user.171,
L.user.172) counts **the committed pages of each region's object inside the
region's range**, whether or not this space has faulted them in: a page of a
shared mapping another process touched, a cached page of a mapped file read
with `read`, all of a forked child's pages. The two differ for a reader.

## 3. The decision: keep Ferrix's definition, keep it counted

The definition does not change. L.user.171's and L.user.172's statements stand
word for word, and every existing check of them passes unchanged; only how the
figure is produced changes. A change to Linux's definition would be a visible
change of meaning (a forked child would show almost nothing resident until it
touched its pages; a shared mapping would show only what this process
touched), and that is the customer's decision, not this change's.

Ferrix's definition makes the count depend on two things that change apart:
which page indexes of an object a space's regions cover (the space's map,
under its lock), and which of those indexes the object holds (the object's
page set, under its pages lock, changed by any space that maps it, by `read`
and `write` on a file, by reclaim, by a pager). So the count is kept where
both meet: **in the object, under its pages lock.**

## 4. The mechanism

**Per space**: three `AtomicU64`s, `anon`, `file`, `shmem` (`user::rss::Rss`),
in an `Arc` of their own, made with the space (`AddressSpace::new` and
`AddressSpace::fork` both make one and hand it to `assemble`, so a native
process's space, made by `new`, has one too), so that an object can reach them
without reaching, keeping alive or dropping the space. Views hold the `Arc`, so
a freed space's address can never name another's counts.

**Per object, per (space, id, kind) that names it**: a *view*
(`user::rss::View`), under the object's pages lock, with

* the space's `Arc<Rss>`, the id and the kind, its key (`ViewKey`); the kind
  is the present `Counted::kind` (private anonymous memory `anon`, shared
  anonymous memory and a memory filesystem's file `shmem`, a disk file's
  `file`), part of the key; an id's `shared` is its mapping's for life, so
  a private file mapping's key always names a masked view, which check
  builds hold;
* its *cover*: how many of the space's regions of that id cover each page
  index, a sorted list of canonical runs `(first, end, count)`
  (`ferrix_vma::Cover`, `src/lib/kernel/vma/src/cover.rs`, host-tested
  against a count per index);
* for the file object of a private file mapping, its *mask*: the indexes the
  mapping's shadow holds a copy of, which the region shows instead of the
  file's page, a `BTreeMap` of index to whether it is masked (insert,
  look-up and unmask O(log n) under the file's pages lock). An index the
  shadow gave back stays, marked unmasked, until the view goes, so that an
  unmask frees no node: it runs on the decommit and drop paths inside the
  shadow's pages lock, where a B-tree's merge and the heap's give-back
  below it deepened `check-stack-depth`'s "a process made".

The views of an object are a list, found by key with a walk of it: a page
event walks every view anyway (one per space and id mapping the object, the
mapper list's length), so a key's lookup costs no more than the event.

**The invariant.** For every space `s` and kind `k`, whenever no change is in
progress (§6):

```
s.rss[k] = Σ over objects O, Σ over views v of O keyed by s with kind k,
           Σ over committed indexes i of O, cover_v(i) · [i ∉ mask_v]
```

and, because each region of id `n` adds one to `cover` over its range in the
view `(s, n, kind)` of the object it names (and of its shadow, as `anon`), and
because the mask is the shadow's page set, the right side is exactly what
`resident_split` counted before: each region's object's committed pages
inside its range, a private file mapping's copies as `anon` and not its
file's. An object mapped twice counts twice; the gap between two regions of
one object counts nothing.

**Where it moves.** Two kinds of event, each applied under the object's pages
lock, so each keeps the invariant on its own:

1. *A page comes or goes* (`user::rss::Frames`, the one type through which an
   object's page set changes, private to `user::vmo`: commit, fill, supply,
   adopt, copy-on-write replacement, decommit, take, reclaim, truncation,
   poison, an object's drop; the compiler refuses any other write of the
   map). For each view, `rss[kind] ± cover(i)` if `i` is not masked. A
   replacement at the same index moves nothing. A shadow's change at `i` also
   masks or unmasks `i` in its file's view (below), and moves `file` or
   `shmem` by `∓ cover(i)` if the file holds `i`.
2. *A region comes or goes* (`user::space`, under the space's lock:
   `count_in` and `count_out` from `add_region`, `abandon_region`, `unmap`'s
   removal, `mremap`'s placement and removals, the kernel's code page,
   `map_file` for a private mapping once its shadow is in the tables,
   `attach_child` for a fork's child). The view's cover moves by ±1 over the
   region's index range, and `rss[kind]` by the committed, unmasked indexes in
   that range, counted under the pages lock at most `COUNT_CHUNK` (256)
   committed pages a hold, the cover changed over the same indexes in the
   same hold (`Frames::cover_step`). A region is counted in before the map
   takes it and out after the map lets it go, so a refusal of either is an
   exact undo.

`mprotect` splits and merges regions but changes no index's cover, so it moves
nothing and costs nothing here: Wine's thousands of `mprotect`s over one
reserve leave one run in one view.

**Room.** A cover's change in place needs room for the runs it adds: a lower
splits at most its two edges, and a raise also fills every gap of its range;
made in chunks, the run under way may split one edge more. So a change's first
hold has room for `3` runs (a lower) or `3 + gaps` (a raise), and a raise
`UNDO_ROOM` (3) more, so that its exact undo, which may split an edge the
raise joined, is always in room. `munmap` and `mremap` have room for a lower of
every region the removal reaches before the map changes
(`AddressSpace::room_to_count_out`), so the count's change after it cannot be
refused. With no room, the `mmap`, `mremap`, `fork` or fault answers `ENOMEM`
and nothing changed. A view's drop needs none. The cover's room is charged to
the job making the change, as the region map's own room is
(`ferrix_kmem::Charge`); a view's own heap is in `MAPPER_HEAP`, one view per
mapper. A view is made by the first raise of its key, before the map change,
with its room had then; check builds stop if a view is left on an object being
dropped, or on an id leaving a space's tables while it still covers pages.

**Shadows.** A private file mapping's writes go to its shadow, an anonymous
object indexed by the file's page index, which the space alone changes, under
its lock (space.rs says so at the fault). Its view counts its pages as `anon`.
The file's view must not count an index the shadow holds; it keeps that set
as its mask, so that a change to the file's pages (from anywhere: another
process's fault, `read`, reclaim) is decided under the file's pages lock
alone. The shadow carries a link to its file object and the view's key
(`Vmo::link_shadow`, made when the region is counted in, with the mask built
from the shadow's pages then); a change to its page set, made under its own
pages lock, then takes the file's pages lock to move the mask and the count
(`Vmo::with_frames_nested`, the one place two pages locks are held). The
mask's entry is had before the shadow's page goes in (`Frames::try_insert`:
the mask first, refused as `ENOMEM` at the private-file fault, as a refused
frame already is); unmasking never allocates. A shadow gains pages only
through `insert_absent` at that fault; a path that cannot refuse
(`Frames::insert_held`) stops a check build if it ever reaches a shadow. The
mask keeps an entry for every index the shadow has ever held, up to the
mapping's pages, and each entry is charged to the job whose fault made it
(32 bytes, a B-tree node's share at its least full) for as long as the
view lives, as the cover's runs are (ledger 8894 T1): a program that
writes and drops copy after copy pays for the heap that leaves behind.

**Lock order: a shadow's pages lock, then its file's pages lock; never a
file's, then a shadow's, and never two pages locks otherwise.** No path held
two pages locks before this change (`committed_in_both`, now check code, went
one at a time). Check builds count the pages locks each processor holds
(`user::vmo::PagesLock`) and stop, with FX-0977, at a take with another held
other than through `with_frames_nested`, or at a nested take with none.

## 5. Costs

* **A read**: three relaxed loads. Nothing grows with regions or pages.
* **A page committed or taken**: per view of the object, a binary search of
  its cover and one atomic add; plus, for a shadow, the file's pages lock and
  a search of its mask. A private object has one view. Measured (stage 8's
  rss check, KVM on nazuna, 2026-11-12): 4,096 new pages written into a
  memfd took 1.36 µs a page with no mapping and 2.67 µs with 64 mappings
  viewing it, about 20 ns a view a page.
* **A region mapped or unmapped**: the cover's run edit plus the committed
  pages in the range, counted 256 a hold of the pages lock, with the space's
  lock held across the chunks: an `mmap` of a memfd with 4,096 pages cached
  took 196 µs (48 ns a page), so a 4 GiB cached file would hold the space's
  lock about 50 ms. Every read used to pay that much.
* **A fault**: faultbench (128 MiB, KVM guest on nazuna, host load about 30):
  a first write 2,890 ns a page on develop, 3,075 with the counts kept;
  cowbench's plain first write 3,129 and 3,032. A loaded host: not told from
  noise; an idle host's figures are a BACKLOG row.
* **`fork`**: the child's covers, one run per region, and their counts: the
  child's committed pages, which `Vmo::fork` already copies one by one.
* **`mprotect`**: nothing.

## 6. What a reader sees

The three counts are separate atomics, read separately, as Linux's per-CPU
sums are ("such snapshots can always be inconsistent", `task_mem` in
`fs/proc/task_mmu.c`). `VmRSS` and `stat`'s `rss` are each made from one read
of the three, so `VmRSS` is their sum by construction. Each event of §4 moves
one count by its whole delta in one atomic add made inside the object's pages
lock, after the change it accounts for, so no count is ever negative (check
builds stop on a decrement below zero rather than wrap) and each is exact
between events. While an operation is in progress a reader can see it partly
done: an `unmap` of many pages as some of its 256-page chunks; a private-file
write fault as its file page uncounted and its copy not yet counted, for the
instant between the two locks. The figure is never off by more than the pages
of the operations in flight at that moment, and is exact again when they end.
That bound is a property of the reader, stated here and in
`MEMORY-AND-TIMING.md`, not a requirement: no check can prove it.

## 7. Checks

* **Kept equals recount** (`L.user.236`): `user::rss_check::recount`, the
  body `resident_split` had, moved unchanged into check code, which reads only
  the map and the pages and which no product path calls, compared with the
  kept counts after each change of `user::rss_kept_check` (stage 8, every
  boot): a new mapping; a first touch; an `mprotect` split and merge; an
  `munmap` of a middle; `madvise(DONTNEED)`; whole regions unmapped; a fork,
  in both spaces; copy-on-write writes in the child and the parent; a child's
  write to shared anonymous memory; the child's exit (and an object it alone
  mapped viewed by nobody after it); a memfd mapped shared twice and private
  once; the private mapping's write; a fork writing the private mapping's
  copies; a truncation under the copies and the file grown back; an `mremap`
  move over a fixed destination; 4,000 regions in Wine's pattern with a fixed
  mapping into the reserve; a file of 4,096 cached pages mapped, and written
  beside 64 mappings; shared anonymous memory grown, moved and shrunk by
  `mremap`, whose new placement covers its pages a second time until the
  old range is counted out; a private file mapping writing and dropping
  each of 1,024 pages, whose mask must hold a charged entry for each; 34
  comparisons. Stage 12's reclaim check does the
  same for a disk file mapped shared and private with a copy, through a
  reclaim that took some of its clean pages and the faults that fill them
  again. The checks use 4,000 regions, enough that a recount costs
  milliseconds a read; 15,000 is `statusbench`'s measurement, not a check.
* **A read is constant** (`L.user.237`): 200 reads of `AddressSpace::resident`,
  which procfs calls, over 4,000 regions within four times 200 over one region
  and a millisecond (KVM: 21 and 18 ns a read; TCG AArch64 135 and 37;
  ARMv7-A 184 and 54). A `status` read on develop took 522 µs at 15,000 regions (release build,
  KVM guest).
* The existing checks of L.user.171 and L.user.172 (procfs, syscall,
  madvise, rehome, reclaim) run unchanged against the kept counts.
* Negative controls through `gate.sh control`, each FIRED (the landing's
  commit lists them with their logs): the funnel's removal uncounted (the
  `core rss` check's `madvise(DONTNEED)` case); `unmap`'s count-out left out
  (FX-0977: an id left its space's tables still covering pages); the
  shadow's mask not moved (`L.user.172`'s stage 6 check, `rehome_check`: a
  disk file's page a private mapping wrote was not counted as anonymous);
  the shadow's unmask left out (the rss check); `fork`'s child left
  uncounted (the `core rss` check); the recount put back in `resident` (the
  `core rss` check's time bound); a second pages lock taken unnested
  (FX-0977); a view left at a space's drop (FX-0977 at the object's drop);
  the region totals not lowered by a removal (the region map's own stage 2
  check, `check_invariants`).

## 8. The other figures of `status` and `stat`

`VmSize`, `VmLck`, `VmData` and `stat`'s `vsize` walked every region too
(`Memory::of`): with the counts kept, a `status` read over 15,000 regions
still took 31.8 µs (release build, KVM guest). The region map keeps them now as running totals
(`ferrix_vma::Totals`: every region, the `mlock`ed ones, the private writable
ones) on every insert, removal and `protect`, as Linux's `vm_stat_account`
does, and `VmStk` is the one region holding the start stack pointer, found by
one lookup, which `VmData` leaves out as it always did (`L.user.238`). The
walk stays as check code (`Memory::walked`), the procfs check holds the two
equal, and the map's host tests hold the totals equal to a sum after every
operation (`check_invariants`).

`statusbench`, release build, KVM guest on nazuna with xtask's `-cpu`,
2026-11-12 (`~/.local/share/ferrix/logs/rss/`; Linux under the same QEMU
with the program as `/init`):

| | `status` µs (mean) | `stat` | `/proc/meminfo` |
|---|---|---|---|
| Linux 7.0 as the guest | 2.5 | 1.6 | 2.1 |
| develop 1aea9a223 | 522 | 511 | 6.6 |
| counts kept (L.user.236) | 31.8 | 25.7 | 8.8 |
| and region totals kept (L.user.238) | 15.5 | 9.3 | 8.8 |

On the N150 with the game (a release image, run rss1): `status` 13.8-21.4
ms to 0.8 ms a read. What is left grows with nothing a program holds: the
cost of opening a procfs file, which Ferrix renders at `open` (an `open`,
`read` and `close` of a small process's `status` about 12 µs on develop's
release build, against 2.0 µs in the Linux guest). It is not this change's.

## 9. `/proc/meminfo`

### 9.1 What a read is made of, and what it costs

`fs::procfs::render::meminfo`: free and managed frames (two loads under the
frame allocator's lock), the heap's pages, the writable btrfs mounts' clean
bytes (`fs::btrfs::clean_bytes`, one load per mount: `L.user.175`), and
`user::cache::totals`, which walks the page cache's list: under the list's
lock every entry's `Weak` is upgraded into a fresh `Vec` (the dead entries
too, until a close-up), then per object its pages lock for `committed`,
again for `dirty_pages`' page states, its filler's `dirty_pages` (a btrfs
writable file's: its dirty set's lock) and `reclaimable`. Per object:

* a disk file's (`filler` some): `disk += pages`; `dirty += min(fs + states,
  pages)`, with `fs` the filesystem's own count of pages written and not
  written back (`Filler::dirty_pages`) and `states` the pages a shared
  mapping wrote (`Pages::states`: Dirty or AwaitingClean); if the object is
  `reclaimable`, `reclaimable += pages - that dirty`;
* a memory filesystem's (tmpfs, the initramfs, `memfd`): `memory += pages`.

`Cached = disk + memory`, `Shmem = memory`, `Dirty = dirty`, `MemAvailable =
free + reclaimable + clean nodes` (`L.object.189`, `L.user.175`).

The read grows with the number of cached files, live and dead, not with what
anyone holds: release build, KVM guest, a boot's few files: 6.6 µs (develop
1aea9a223), Linux 7.0 in the same guest 2.1 µs; on the N150 with the game,
3.05 ms (3 to 4.6 ms).

### 9.2 What Linux does

`fs/proc/meminfo.c` reads `global_node_page_state` (`NR_FILE_PAGES`,
`NR_SHMEM`, `NR_FILE_DIRTY`) and `si_mem_available`: counters kept in
`vmstat` as pages enter and leave the page cache and as a folio's dirty flag
is set and cleared (`__lruvec_stat_mod_folio`, `folio_account_dirtied`,
`folio_account_cleaned`). Each processor adds into its own differential
(`vm_node_stat_diff`) and folds it into the global atomic when it passes a
threshold (`stat_threshold`, up to 125 pages); the reader reads the global
atomics alone, so its figure lags by up to the threshold times the
processors. Nothing a reader does grows with the files.

### 9.3 The decision: the same figures, kept

The figures do not change. `L.object.189`'s and `L.user.175`'s statements
stand word for word, and so does every term of §9.1 -- including the cap
`min(fs + states, pages)` per object, which a machine-wide count of dirty
pages could not keep: the cap is a per-object fact (a page in both the
dirty set and the page states, or a dirty mark for a page reclaim or a
truncation has already taken, is counted once per object at most up to its
pages). So the count is kept per object, as the resident counts are kept per
view, and summed machine-wide as it moves.

### 9.4 The mechanism

**Each cache object's share.** `Pages` gains `share: Option<Share>`, set by
`Vmo::new_filled` before the object is registered and on no other object
(every other constructor of `Pages`, the fork's included, leaves it `None`):
exactly the objects the walk counts. A `Share` holds what the object adds to
the machine's counts now -- `given_pages`, `given_dirty` -- its class (disk
or memory, and for a disk file whether it is `reclaimable`, fixed at
creation: every source answers a constant, and the trait's documentation
says so from now on) and `fs`, the filesystem's dirty count as last read,
first read by `new_filled` itself, so that a source already dirty when its
object is made is counted.

**One settle, at every release of the pages lock.** `PagesGuard`'s `Drop`
(no longer check builds only) settles inside its body, the lock still held:
every change of `frames` and of `states` is made under that guard (the
`Frames` funnel of §4 and the thirteen `states` sites alike), so no change
site can be missed. It compares `(frames.len(), min(fs + states.len(),
frames.len()))` with what the share gave, and if either moved adds the
differences to the machine's counts and records them: O(1), no allocation,
cannot fail. An object with no share pays one test of an `Option`. An
object's drop (which reaches its pages with `get_mut`, no guard) takes back
all its share gave, and in check builds stops if `reclaimable()` no longer
answers the class fixed at its creation.

**The filesystem's dirty count.** `Filler::dirty_pages` (and the vfs
`PageSource::dirty_pages` it forwards to) answers from an atomic load: no
lock, no blocking, no allocation, as the core calls it under the pages spin
lock. A source whose count moves tells its store after the change, holding
none of its own spin locks: `Pages::dirty_changed` (vfs; a no-op by
default), which the kernel's store answers by taking the object's pages
lock, reading `dirty_pages` into the share, and letting the settle move the
counts. Because the store reads the count rather than being handed it, two
tellings that race leave the last read, which comes after the last change:
exact once the changes are done, in any order.

btrfs's writable mount keeps its dirty set in a `DirtySet`, its set private
behind its guard, so every change goes through the guard. The guard notes
the set's length when taken; in its `Drop`, *before* the set's lock is let
go, it stores a changed length in the atomic `dirty_pages` reads (stored
after the unlock, an earlier holder's late store could overwrite a later
one, and every later reading would be stale for good); then, the lock let
go, it tells the store if the length changed. The store is the node's page
cache, which the set holds weakly (the cache's source holds the set, so a
strong hold would be a cycle), and is set under the node's slot lock as the
cache is made, before any write can reach the set. So the tell is
structural: the six places the set changes -- a write's marks, a
truncation's cut, a writeback slice's take and its failed return, a full
writeback's take and its failed return -- tell without a line of their own,
and the places that only read it tell nothing. The read-only mounts', the
served volumes' and tmpfs's count is a constant zero.

**Lock order.** The tell takes a pages lock with none of btrfs's spin locks
held: never under the dirty set's lock or the `still` lock, and no dirty
set's lock is ever taken under a pages lock (the core reads the atomic
instead). It may be taken under btrfs's volume lock, as `write_runs`' reads
of the cache already are.

**The machine's counts, per processor.** Four signed counts -- disk pages,
memory pages, dirty, reclaimable -- live in each processor's `PerCpu` record
(next to `syscalls`, the same idea; no offset read from assembly moves, and
`irq_guards` stays last), plus an early set. A settle finds its record by
`smp::this_cpu()`, and adds to the early set while that answers none: before
`discover` installs the boot processor's record, which it takes from
`topology().cpus()`, so a record a settle adds to is always one the reader
sums. Parked threads run no settle. Today's boot makes its first cache
object in stage 6, after stage 4's `discover`, so the early set covers a
boot order Ferrix does not have today: a settle that ever runs that early is
counted, not lost (its negative control cannot fire, and is recorded as not
fired, for that reason). A settle runs under the pages spin lock with
preemption held off, so the task cannot move between finding its record and
adding; it adds with a relaxed `fetch_add` to that record: one line per
processor, never one line every processor writes on every page of every file
(K10). In the lift build (the core in ring 3, the QM profile) the thread can
move between finding its record and adding, so the add may land on another
processor's record: the sums stay exact, being sums of every record, and
`L.user.241`'s own-record claim is the kernel builds'. `AtomicU64` is
single-copy atomic on all three architectures (ARMv7-A by
`ldrexd`/`strexd`), so a remote reader never sees half a word, which matters
here because a processor's own sum goes below zero whenever pages it did not
add leave on it (the counts are two's complement, summed wrapping).

**The reader.** `totals()` sums the early set and every record of
`topology().cpus()`, each field wrapping as `i64`, and clamps each sum at
zero: no lock, no list, no cache object. A reader racing changes on two
processors can see one and not the other; it is exact whenever the changes
it is to see are done, as the walk's figure was. The walk (`walked_totals`,
today's `totals` body unchanged, with `Vmo::dirty_pages`) moves to a
CHECKS-ONLY file as the checks' oracle; no release build reaches it.

### 9.5 Costs

* A page entering or leaving a cache object: the settle's comparison and up
  to three relaxed adds on this processor's line. A fault of a file page
  already pays a pages lock, a frame and a fill. Every other release of a
  pages lock: one test.
* A btrfs change of a dirty set: one pages lock after it.
* Memory: 32 bytes per processor record; every VMO's `Pages`, anonymous ones
  included, grows by `size_of::<Option<Share>>()`, 32 bytes (two flags and
  three counts; `None` takes a flag's spare values); no allocation
  anywhere.
* The reader: four loads per processor.

### 9.6 Checks

* `cache_kept_check` (core, every profile), with check sources -- a memory
  filesystem's object, a reclaimable disk file's whose dirty count moves
  and tells, a non-reclaimable one, and one dirty before its object is made
  -- pages committed and decommitted, page states set Dirty, AwaitingClean
  and Clean, the cap reached (a page counted by the filesystem and in the
  page states), objects dropped holding pages, and 4,096 objects made and
  dropped:
  * each check object's share equals its own walked figures exactly;
  * the machine-wide equality on a quiet reading only: the kept read
    between two walks that agree, retried a bounded number of times, then a
    failure, never a pass for running out;
  * `L.user.240`: a check-build count of walks unchanged across `totals()`
    with 4,096 live objects; the time against none reported;
  * `L.user.241`: with preemption held off on a processor other than 0,
    committing N pages to a check object moves that record's disk count by
    exactly N (`--smp` 2 or more).
* Stage 12 (`btrfs_write_check::reclaim_check::kept_cache_counts`, the
  `pc-kept` line): the file's share and the machine's sum against the walk
  on a real writable mount after a write of 260 pages, a sync (a slice's
  take of 256, then the full writeback's), a page written again, a shared
  mapping's write, a truncation's cut and the sync after it. The failed
  returns of a writeback, which a healthy disk does not make, are the btrfs
  host test `every_change_of_a_dirty_set_tells_its_store_the_count_after_it`'s:
  a store that notes each telling, whose copy out is made to fail, sees the
  slice's take (44 of 300 left) and its return (300), the full writeback's
  take (0) and its return (300), the write's marks and the truncation's cut,
  and nothing told by a read.
* `L.object.189`'s criterion lines (`core pc`, test-btrfs `reclaim`) and
  `L.user.175`'s keep their criterion text and must stay green; the `core
  pc` check's source tells its store when its dirty count moves (its body
  changes, its criterion does not).
* Negative controls (`gate.sh control --expect`): a settle that skips the
  dirty term; a `DirtySet` change that does not tell; a drop that does not
  take back; a share set on an unregistered object; the per-processor sum
  leaving one record out; the early counts left out; a disk object's pages
  counted as memory; `new_filled` not reading the source's dirty count;
  `totals` calling the walk; the settle adding to record 0; the creation
  class assertion. The store-before-unlock order has no deterministic
  control: it rests on the guard's structure, and code review.

### 9.7 Figures

`meminfobench` (`~/.local/share/ferrix/ef-30fps/faultbench/meminfobench.rs`,
sha256 9ff6013a…; logs in `~/.local/share/ferrix/logs/rss/`): 200 reads of
`/proc/meminfo` (open, read to the end, close) with a boot's few files, then
with 10,000 more files of one written page each. Medians, µs a read:

| Release build, KVM guest on nazuna (xtask's `-cpu`) | boot's files | 10,000 more |
|---|---|---|
| Linux 7.0 as the guest | 1.9 | 2.0 |
| Ferrix, rss/kept-counts 9f2fa4e46 (the walk) | 8.8 | 289.4 |
| Ferrix, rss/meminfo (kept) | 7.7 | 8.4 |

The read no longer grows with the cache; what is left is the cost of a
procfs file's open, which renders it, against Linux's (the coordinator's
`readbench`: a procfs open and close 11.2 µs, a tmpfs one 2.3 µs against
0.30), not of the figures.

### 9.8 Requirements

`L.object.189` and `L.user.175` keep statement and criterion word for word.
`L.object.189`'s unit list names the code that now computes its figures:
the settle, the drop's take-back, the dirty refresh, the per-processor sum
and `cache::totals`, with `Totals::cached` and `Totals::available`.
New (ledger 8919 M1): `L.user.239`, `totals` answers each figure as the sum
over every live cache object of what it gives (its pages; its dirty count,
the filesystem's last-read count and its page states, capped at its pages;
for a reclaimable object its pages less that), kept as the object's pages
lock is let go and as its filesystem tells, never below zero and equal to
those per-object figures whenever no change is under way; `L.user.240`, a
read takes only the early counts and each processor's record, touching no
cache object, list or lock; `L.user.241`, a settle adds to the record of the
processor it runs on, or to the early counts before the records are kept.

## 10. Where it stands

Design reviewed 2026-11-12: OK IF K1-K11, consultant ledger line 8701
(conditions 8705-8715); code reviewed the same day: OK IF R1-R9, ledger 8826.
Built on branch `rss/kept-counts`: the resident counts kept (`L.user.236`,
`L.user.237`) and the region totals kept (`L.user.238`). `/proc/meminfo` (§9): design
reviewed 2026-11-12, OK IF M1-M12, ledger line 8919 (conditions 8920-8931);
built on branch `rss/meminfo` (`L.user.239`-`241`); code reviewed the same
day: OK IF N1-N9, ledger line 8988 (conditions 8991-8999).
