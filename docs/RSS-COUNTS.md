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

`VmSize`, `VmLck`, `VmData` and `stat`'s `vsize` walk every region too
(`Memory::of`): with the counts kept, a `status` read over 15,000 regions
still takes 31.8 µs (release build, KVM guest on nazuna), and a `stat` read
25.7 µs, where develop took 522 and 511 (Linux 7.0 in the same guest: 2.5
and 1.6). The next
commit keeps them in the region map (`L.user.238`).

## 9. `/proc/meminfo`

What a read is made of today (`fs::procfs::render::meminfo`): free and managed
frames (two loads under the frame allocator's lock), the heap's pages, the
writable btrfs mounts' clean bytes (one per mount), and `user::cache::totals`,
which walks the page cache's list: the list's lock while every entry's `Weak`
is upgraded into a fresh `Vec` (the dead ones too, until a close-up), then per
object its pages lock for `committed`, again for `dirty_pages`' page states,
and its filler's `dirty_pages` and `reclaimable`. It grows with the number of
cached files, live and dead, not with what one program holds: 33 µs in the
guest with a boot's few files, 3.1 ms on the N150 with the game's.

It is a different mechanism from the resident counts: machine-wide, not per
space; moved by every page-cache event of every file, so one shared counter
would be a contended line on every fault and read of the cache (Linux keeps
`vmstat` per CPU and folds it); and two of its parts are not page-set
changes at all: `Dirty` adds a filesystem's own count of pages written and not
written back (`Filler::dirty_pages`) to the object's page states, and
`MemAvailable` counts a reclaimable object's clean pages. So it is a second
branch, `rss/meminfo`, on top of this one, with its own design review (K10):
per-CPU counts of disk and memory-filesystem pages moved by the same `Frames`
funnel for an object the cache registered (its class set at `register`, its
pages counted in then), per-CPU counts of page states moved where `states`
changes, the filesystems' dirty count kept by them, and `L.user.175`'s and
`L.object.189`'s statements kept word for word.

## 10. Where it stands

Design reviewed 2026-11-12: OK IF K1-K11, consultant ledger line 8701
(conditions 8705-8715); code reviewed the same day: OK IF R1-R9, ledger 8826.
Built on branch `rss/kept-counts`: the resident counts kept (`L.user.236`,
`L.user.237`). `/proc/meminfo` (§9) is not built:
its design review and its branch are owed.
