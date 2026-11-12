# Anonymous mappings joined to their neighbour's object

Design note for branch `rss/anon-merge` (ef-30fps, step 4 after
`docs/RSS-COUNTS.md`'s resident counts and `/proc/meminfo`). Status: design,
for review.

## 1. Why

With Endfield running on the N150, Ferrix's `/proc/<pid>/maps` holds 10,389
to 16,009 lines where Linux holds 2,931 to 2,964 for the same game
(`~/.local/share/ferrix/ef-30fps/NOTES.md`, pairs 1-3). `status` and `stat`
no longer walk the regions (`L.user.236`-`238`), but the regions still cost
wherever they are walked or searched:

* every `mmap` and `munmap` of the game: `give_back`'s `all_still_named` and
  `room_to_count_out` sort what an unmap touches; a fault and every map
  change search the map, O(log regions);
* every reader of `maps` and `smaps`, a line each;
* the exit: the whole-space unmap counts every region out and sorts them
  (the silent-reset agent's stack overflow is a sort of this size);
* one `Vmo` per region, with its `Objects` charge, its mapper record, its
  object-table entry and, since `L.user.236`, its own resident view.

Linux keeps a fifth of the regions because it merges a new anonymous mapping
into the mapping beside it. Ferrix merges only regions that already name one
object at contiguous offsets (`ferrix_vma::mergeable`), and every anonymous
mapping it makes names a new object at offset 0.

## 2. What Linux does

`mm/vma.c`'s `vma_merge_new_range` (from `__mmap_region` and
`do_brk_flags`): a new anonymous mapping joins the mapping before it and/or
after it when the flags are equal, neither has a file, their `anon_vma`s are
compatible (one of them has none, or both the same), and the page offsets are
contiguous. An anonymous mapping's `vm_pgoff` is its address in pages, so two
anonymous mappings side by side are always contiguous unless one was moved
by `mremap`. A merged mapping extends the existing `vm_area_struct`, and the
new range's pages, when touched, join the neighbour's `anon_vma`. `brk`
extends the heap's mapping in place. `MAP_FIXED` over part of an anonymous
mapping unmaps that part first, then merges the new mapping with what is left
on either side, if the flags allow.

## 3. Where Ferrix's regions come from

Wine allocates nothing with a plain `mmap`. It reserves address space with
one `mmap(PROT_NONE, MAP_NORESERVE)` and then maps every view, decommit and
release into it with `mmap(MAP_FIXED | MAP_PRIVATE | MAP_ANONYMOUS)`
(`dlls/ntdll/unix/virtual.c`: `map_fixed_area`, `decommit_pages`,
`unmap_area`). Ferrix's `sys_mmap` answers each with an `unmap` and then
`AddressSpace::map_anonymous`, which makes a new object. So the reserve ends
up as a row of objects, each between two pieces of another, and no two of
them can merge again, whatever their protections become. A commit, which
Wine does with `mprotect`, would have merged them.

`brk` grows the heap by `map_anonymous` of the extension
(`Process::set_break`), so every step of the heap is also an object of its
own. A plain `mmap` goes through `map_anywhere`, which searches the map top
down, so a new mapping usually sits just below the previous one.

`regionbench` (source `~/.local/share/ferrix/ef-30fps/faultbench/regionbench.rs`,
sha256 2ce945a9...; the static binary built from it as its header says,
sha256 a749b0cd...) does all three. Both columns are release builds in a KVM
guest on nazuna with xtask's `-cpu` and `-smp 4 -m 512`: Ferrix on kernel
tree 3ddb22aa0 booted by `cargo xtask test-shell --release --accel kvm
--kernel-option ferrix.checks=skip` with fc88ae4af's xtask, whose log shows
`cmdline ferrix.checks=skip (from /FERRIX/CMDLINE.TXT)`
(`regionbench-release-skip-3ddb22aa0.serial.log`); Linux 7.0
(`vmlinuz-7.0.0-29-generic`) with the program as `/init`, run by
`qemu-system-x86_64 -accel kvm` with the same `-cpu` string
(`regionbench-linux-7.0-kvm-guest.log`, which prints no accelerator line).

| maps lines added by | Linux 7.0 | Ferrix `rss/meminfo` 3ddb22aa0 |
|---|---|---|
| Wine's pattern, 4,000 views of 64 KiB (each mapped, every fourth decommitted in part, every third released) | 3,999 | 5,332 |
| `brk`, 500 steps of 64 KiB | 1 | 501 |
| 2,000 `mmap(NULL, 64 KiB)` | 0 | 2,000 |

And what Wine's pattern costs, in us (the mean of each call; the exit is a
child holding the views, from its last word to its parent's `waitpid`):

| | Linux 7.0 | Ferrix `rss/meminfo` |
|---|---|---|
| a view mapped (`MAP_FIXED`, RW) | 0.8 | 3.4 |
| a part decommitted (`MAP_FIXED`, `PROT_NONE`) | 1.4 | 8.6 |
| a view released (`MAP_FIXED`, `PROT_NONE`, `MAP_NORESERVE`) | 1.5 | 123.4 |
| the exit, 8,015 / 13,178 maps lines | 1,691 | 92,886 |

A release is the costly call because it leaves an object no region names,
and `give_back`'s `drop_unnamed` then reads every region of the map
(`Named::of`) to find which: once per release, so Wine's frees grow with the
regions it holds. With the views in the reserve's object, a release leaves
it named and the walk is not made. (Logs:
`~/.local/share/ferrix/logs/rss/regionbench-*`.)

## 4. The decision

A private anonymous mapping beside a region of this space's own private
anonymous memory takes that region's object, at the offset the object's
addresses continue to, when the object holds no held page there; any other
page left there is taken back first (section 5.4). The map then merges the
two regions whenever their flags are equal, as it already merges an
`mprotect`'s pieces of one object, and if they are not, a later `mprotect`
that makes them equal merges them.

The offset convention does not change: a new object still starts at offset
0. Linux's convention, an anonymous object's offset being its address
(section 9.1), would let a mapping join its neighbour on either side, but
every anonymous object would then be as long as its highest address in
pages, and the code that takes a whole object (`Vmo::seal`,
`hold_whole_on_payer`, the lift build's bridge copy, the native size checks)
would meet objects of up to 2^35 pages. It is left until the figures show
the case it adds (section 9.1). `ferrix_vma`'s comment at `lib.rs` 432-435,
which says private memory's offset is its start address, is corrected with
the code: it is not Ferrix's convention.

## 5. The mechanism

### 5.1 Own private anonymous objects, and one line (I1)

An object id of a space is *own private anonymous* when no handle, file or
shadow of the space names it (it is in none of the space's `native`,
`files` and `shadows` tables) and its regions name it only by
`Backing::Anonymous` with `flags.shared` clear.

Which ids those are. Every handle and kernel-core mapping puts its id in
`native`: `map_by_handle` (and `map_object` through it), `map_code_page`,
`map_shared_code`, `allocate_by_handle`; shm goes through `map_file` and so
`files`; devices and windows are `Backing::Device`. Ids outside those three
tables are made only by `map_anonymous`, `map_anywhere` and the fresh object
of a private `mremap` (`add_fresh`), each with one value of `shared` for the
whole id; a split, a merge, `mprotect` and `mremap` keep a region's
`shared`. So the neighbour's own `flags.shared` and `Backing::Anonymous`
stand for every region of its id. The reuse declines on what it can see
of the neighbour itself (a `shared` neighbour, a backing other than
`Backing::Anonymous`, an id in one of the three tables); that every other
region of the id agrees is asked of the whole map only by the `anm` check,
over its own space after each step (section 7), since a walk of every
region at each `mmap` would cost what this change saves.

**I1.** For each own private anonymous object X of a space there is a base
`b(X)` such that every region of the space naming X names offset `v - b(X)`
at each of its addresses `v`. So no two addresses of the space name one page
of X. Every path that makes or changes a region naming such an id keeps it:

* `map_anonymous` and `map_anywhere` make X with one region at offset 0:
  `b(X)` is its start. The reuse (section 5.3) places a region on the
  neighbour's line by construction.
* A split (`mprotect`, `munmap` or `madvise` of a part, `mlock`, and the
  flag splits of `protect_by_handle` where they reach such a region) names
  each piece by `Backing::advanced_by`, its offset moved with its address.
* A merge keeps the left region's offset and merges only when the right's
  continues it (`try_merge_at`, `contiguous_backing`); `clone_for_fork`'s
  `merge_all` is the same rule.
* `abandon_region` and the remap undo paths (`undo_move`, `drop_fresh`)
  remove a region whole.
* A private `mremap` always moves the region's pages into a new object at
  offset 0 at the new address (`move_private`), in place or not; X keeps
  only regions it had. A shared one keeps its object, but that is not own
  private anonymous.
* `fork` copies the objects by id and the regions as they are, so a child's
  objects keep their parent's lines.

Every other map change copies a region whole or names offsets by
`Backing::advanced_by`.

### 5.2 Who commits or takes pages of such an object

The reuse's window check (section 5.3) is only good while nothing can put a
page into the window. Each `Vmo` method that commits or takes pages by index
of an own private anonymous object, and the lock it runs under:

* commits: `commit` from `resolve`, `write_copy_on_write`, and the fault
  through `with_page`, all under the space lock, through a region: none
  reaches the window while no region names it;
* takes: `take_range` in `give_back`'s third phase and in `advise`, under the
  space lock; their retires and `give_back`'s fallback `decommit_range` come
  after it lets the lock go (section 5.4);
* `adopt_pages`, into a fresh object only; `Vmo::fork`, making a new object;
* `vmo_write`, `vmo_hold` and the other handle calls reach no such id, since
  a handle's id is in `native`.

### 5.3 Placement

The reuse runs in a new entry, `map_anonymous_beside` and
`map_anywhere_beside`, which takes this space's layout guard (section 5.4).
`sys_mmap` (`memory.rs` 196) and `set_break` (`process.rs` 906) pass theirs.
Every other caller keeps `map_anonymous` or `map_anywhere`, which never
reuse: exec's stack and guards (`exec.rs` 457-470), the ELF loader's
anonymous runs (`load.rs` 551), `first_image` (155, 226-228), and the
self-checks.

Under the space's lock, before a new object is made:

1. Unless the space is marked as being emptied (section 5.4) or is a window
   or region space (`is_window`, `is_region`): find the region ending at the
   new range's start (the left neighbour) and the one starting at its end
   (the right neighbour), two lookups in the map, O(log regions).
2. Take the left neighbour if it qualifies, else the right; never the right
   after the left qualified and its window was declined. A neighbour
   qualifies when its object is own private anonymous (5.1), it and the new
   mapping are private, it is not the exec stack's region (the stack's id,
   which exec records in the space and a private `mremap` of it carries to
   the new object; `fork` copies it), and the offsets its line gives the new
   range are not negative: the left's offset plus its length, or the right's
   offset less the new length. Equal flags are not required: a later
   `mprotect` merges what the flags kept apart. The ELF loader's anonymous
   runs may be joined by a later mapping, as Linux merges into an anonymous
   bss.
3. One method of the object, `Vmo::join_window`, in one hold of its pages
   lock: the object must be plain (not sealed, not pager-backed, not
   coherent, not combining, no kernel view, and `mapper_count() == 1`, as
   `move_private` requires), else declined; no page in the window may be
   held, else declined; every other page in the window is taken out (the
   list for them allocated fallibly; no room, declined) to be retired after
   the space lock (section 5.4); then the object grows to cover the window
   (`len` raised; a plain object always grows). The method carries a `SEAL`
   tag (`check-vmo-seal-tags`).
4. Count the region in (`count_in`, as `add_region` does: X's view in this
   space raises its cover over the window, and moves no count, since the
   window holds no page now) and insert the region naming X at that offset.
   The map's insert merges it with each neighbour it continues. The object
   is in the tables already and this space is its mapper: nothing is
   attached and nothing is added to the tables.
5. After the space lock: the taken pages are retired with
   `Own { space: self, shootdown: None }` (section 5.4).

A declined neighbour leaves the mapping to a new object, as today. A refusal
of the count-in or the insert (no memory) undoes the count-in exactly, as
`add_region` does, and answers the refusal; the taken pages are retired all
the same, and the grown length stays (a longer plain object costs nothing).

### 5.4 Why the window is safe to reuse

What an unmap does with the window it leaves, in three phases: under the
space lock it takes the range out of the map and its translations out of
the tables (phase one); it shoots them down on every processor of the set
(phase two); and in `give_back`, under the space lock taken again, it deals
with the object's pages there (phase three), in one of three ways:

* the space holds the only reference to the object and can list the pages:
  `take_range` takes them out under the lock and `retire` gives them back
  after it, skipping this space (`Own { space: self, shootdown: None }`);
* the only reference, but `take_range` is refused: `decommit_range` by
  offset after the lock, which reaches every mapper, this space included;
* another reference exists (a `madvise` holds one through its retire, a
  fault holds one while it waits), or there is no room to note the range:
  the pages are kept, and stay in the object until it is dropped.

**The hazard is phase three.** A reuse inserted between an unmap's phase one
and its phase three, then faulted, would have its new frames taken by that
phase three and retired skipping this space: a freed frame behind a live
translation. (The decommit fallback is memory-safe, and only loses data.)
So the reuse runs only with this space's layout lock held, which every
Linux-process map change holds for its whole call (`mmap`, `munmap`,
`mremap`, `brk`, `shmat`, `shmdt`, and the native map calls in `native.rs`,
the lift core's `space_unmap` among them at 5217): no other change's phase
three can then be pending. `AddressSpace::layout` returns a guard that names
its space; `map_*_beside` compares it with `self` and, for another space's
guard, declines in a release build and stops a check build.

The unmaps that take no layout lock:

* exec's `empty_user_half`, the exit's whole-space unmap and
  `Bare::release`: each empties the whole user half with no thread of the
  process in user mode, and after their phase one no region of an old id is
  left, so there is no neighbour to reuse;
* the OOM emptier (`object/oom.rs`), which unmaps a killed process in chunks
  from the top while its threads may still be inside a call (one waiting on
  the layout lock, say): there the race is reachable. The emptier sets a
  mark on the space (an `AtomicBool`, stored with release ordering before
  its first unmap's phase one takes the space lock); the reuse reads it
  under the space lock, and a marked space never reuses. None of `unmap`,
  `room_to_count_out`, `all_still_named` or `give_back` changes for it.

**The kept pages (finding 8 of ledger 9155).** In the third case above, the
pages stay in the object. Before reuse, the object went with its last
region and its pages with it. With reuse, the object stays named (Wine's
reserve, the heap, jemalloc's reserve with its purges running beside), and
kept pages would stay until the space goes: mapped nowhere, under no cover
(so in no resident figure), charged to the job, and the window refused by
every later reuse. So the reuse takes them back itself (step 3): no
translation of them is left in this space, because the unmap that left them
finished its phase two under a layout lock this reuse now holds and no
region has named the window since; no other space has any, as
`mapper_count() == 1`; no kernel view exists. The retire's skipping this
space is then right. A held page still declines. Wine maps every released
or decommitted view again with `PROT_NONE` at once (`unmap_area`,
`decommit_pages`), so the pages an unmap kept in Wine's reserve go back at
that next call. A window that is never mapped again keeps them until the
object goes, as today's kept ranges do (`RANGES_KEPT`).

So a fresh, untouched mapping reads zeros (`L.user.243`) and shares no page
with any other address (`L.user.244`), as a new object's would.

### 5.5 What it does not change

`unmap`, `give_back`, `Named::of`, `resident_pages`, `all_still_named` and
`room_to_count_out` are not touched: the count-out of a region naming a
reused object is the same `counting` and `lower` as any region's.
`fix/exit-unmap-sort` (ledger 9112, F-112) is making those four sort without
recursion, and the exit path's design, an exit teardown that takes no room
it can be refused, is its design (d): it owns the exit, and this change
leaves it to that branch. What reuse does to today's reservation there,
given to (d) as an input (ledger 9155 A10): k regions that named k objects
asked `reserve_lowers(1)` of each of k covers, 3 runs each; after reuse, the
k' regions left once they merge (k' at most k) name one object, whose one
cover is asked for 2k' + 1 more runs in one list, and `Cover::reserve`
grows its room to at least twice what it had (`cover.rs` 149). At
k' = 60,000 that is one contiguous request of about 120,000 runs of 24
bytes, about 3 MB: fewer bytes in all than before, but one request that can
be refused. It falls at the exit's whole-space unmap, and at any `munmap`
reaching that many regions of one view. A refusal at the exit leaves the
space's pages until the space is dropped at the reap (what (d) removes); a
refused `munmap` answers `ENOMEM` and changes nothing. With I1 the exit
lowers each object's regions in ascending order, so no run splits on the
way.

`mprotect`, `madvise`, `mremap` (but for carrying the stack's mark), `fork`
(but for copying it), faults and `maps` are not touched. `maps` prints an
offset only for a file's region, so a reused object's offsets are not seen.

A job's `Objects` count no longer rises with each anonymous mapping that
reuses its neighbour's object: `L.object.59` and `L.user.9` charge an object
when it is made, and no requirement or limit counts mappings. The docs that
say each anonymous `mmap` makes an object are amended with the code
(`RSS-COUNTS.md`, `MEMORY-AND-TIMING.md`, `IMPLEMENTATION`'s resource table;
the grep cited in the commit).

### 5.6 Costs

Per anonymous mapping through `sys_mmap` or `brk`: two map lookups, and one
hold of the neighbour's pages lock under the space lock: a probe of its held
pages and of its frames over the window. A reuse saves an `Arc<Vmo>`, an
`Objects` charge, a mapper record (`MAPPER_HEAP`), an object-table entry and
a resident view; it allocates only the list of kept pages, when there are
any. `MEMORY-AND-TIMING.md` gains the space-lock-then-pages-lock site and its
hold, and a note that a walk of a whole object under its pages lock
(`Vmo::fork`'s copy, the OOM reaper's take) now covers what were many
objects, bounded by the same pages.

## 6. Requirements

Reserved as `L.user.242`-`244` (8d5925a77, which lands before the ids are
written; the commit that writes them releases it). `L.user.244` first, as it
defines the term the others use:

* `L.user.244`: For each own private anonymous object X of a space (one no
  handle, file or shadow of the space names, named only by private
  anonymous regions) there shall be a base b(X) such that every region of
  the space naming X names offset v - b(X) at each of its addresses v; so no
  two addresses of the space name one page of X. Criterion: by analysis
  (section 5.1's list, each function named), and the `anm` check validating
  it over every own private anonymous object of its space after each step,
  with a negative control.
* `L.user.242`: A private anonymous mapping made by `mmap` (`MAP_FIXED` or
  not) or `brk` shall be made in the object of its left neighbour if that
  neighbour qualifies, else of its right neighbour if that one qualifies,
  else in a new object; a neighbour qualifies when its object is own private
  anonymous (`L.user.244`), the region and the new mapping are private, the
  offset is not negative, the space is not marked as being emptied, the
  object is plain (not sealed, pager-backed, coherent or combining, no
  kernel view, one mapper) and the region is not the exec stack's. It is
  made at the left neighbour's offset plus its length, or the right's offset
  less the new length, when the object holds no held page there; otherwise
  in a new object, with no fallback to the other side. Criterion: the `anm`
  check's cases, and the regions merging through `ferrix_vma`'s merge
  whenever their flags are equal.
* `L.user.243`: Each page of a private anonymous mapping shall read as zero
  until written, whether it was made in a neighbour's object or a new one.
  Criterion: a window that kept a page across its unmap, mapped again; a
  window written, unmapped and mapped again; a region moved out by `mremap`
  and its window mapped again; each page read.

Parents: the `H.MEM.*` the diff reaches, named at the code review. Units:
`map_anonymous_beside`, `map_anywhere_beside`, the reuse's choice and
`Vmo::join_window`; for `L.user.244` also `ferrix_vma`'s split and merge,
with F-76's note as 8701 K8. `L.user.9`, `.45`, `.46` and `.54` keep their
words (true with the reuse).

## 7. Checks

A core self-check, `user/anon_merge_check.rs` (every profile, the `core anm`
line), on a space of its own, validating I1 over every own private
anonymous object of the space and its resident counts against `user::rss`'s
walk after every step:

* Wine's pattern: a reserve, views mapped `MAP_FIXED` into it, some
  decommitted and released the same way: each mapping takes the reserve's
  object, the regions are as many as the protections' changes, and an
  `mprotect` that makes them equal leaves one region;
* `brk`'s pattern, mapping after mapping at the end: one region, one object;
* a right neighbour with room below its offset (made by unmapping its
  front): taken;
* declined, each with a new object: a window holding a held page; a
  neighbour mapped by handle; a shared neighbour; a shared new mapping; a
  private file mapping's region as the neighbour; a right neighbour at
  offset 0; a marked space; another space's guard (a check build stops, so
  this case runs the release path's decline through a test seam);
* a window that kept a page across its unmap (a second reference held
  across it, as a `madvise` holds one): mapped again, the page is taken
  back, and every page of the mapping reads zero;
* a window written, unmapped and mapped again reads zero; a region moved
  out by `mremap` and its vacated window mapped again reads zero;
* `fork`, then a reuse beside a copy-on-write region: the merged region
  keeps copy-on-write, the parent's write to the new part stays its own and
  the child's pages are unchanged;
* a 4,000-view reserve on one object counted out by a whole-space unmap,
  the resident counts equal to the walk after.

The exec stack's case needs an exec'd process: a stage 7 or procfs check,
`MAP_FIXED` read-write at the stack's top, which must make a new object and
leave `[stack]` and `VmStk` as they were.

Negative controls (`gate.sh control --expect`, each FIRED with a panic, on
the tree): the held-page window check left out (built on the kept-page
window, `--expect` the zero-read line); the take-back left out (the same
read); the right neighbour's room check left out; the native test left out;
the offset off by one page (`L.user.244`'s validation); the emptier's mark
not read; the stack's mark not read; the reuse off altogether (the region
count). No control for the plain-object declines: no own private anonymous
object can be sealed (only `memfd`'s are), pager-backed, coherent or
combining (a handle's or a driver's, in `native`), or take a kernel view (a
ring's or a buffer's, by handle); the decline is a guard, argued here.

## 8. Figures owed

`regionbench` and `statusbench` after the change, taken as section 3's,
both logs cited, release builds with `ferrix.checks=skip` beside Linux 7.0
in the same guest. A row with 60,000 regions made in one reused object
(`MAP_FIXED` views with alternating protections inside one reserve):
`fix/exit-unmap-sort`'s `test-exit-regions` in that variant once it lands,
or regionbench's exit part at that size before then, PASSED on x86-64 KVM
release, the job's memory back to its figure before the child after
`waitpid`, the teardown time beside 9112 E8's. Then the game's `maps` and
its `mmap` costs on the box, by the coordinator.

## 9. Left out

### 9.1 A mapping just below its neighbour

`map_anywhere` searches top down, so a plain `mmap` lands just below the
last one, whose object starts at offset 0 and has no room below it. Joining
it needs the neighbour's offsets to leave room, which Linux's convention
(offset = address) gives every anonymous object. With section 4's objection
answered (the whole-object operations taking a length, not reaching every
page), it is one line in the two `_beside` entries. Measured first: the
game's own `maps` on the box shows how many of its regions are of this kind
and how many are Wine's reserve.

### 9.2 Joining two objects

A mapping between two different own private anonymous objects joins the
left one only. Merging the right one in would move its pages and change its
id in every region naming it: Linux does not do that either, unless the two
`anon_vma`s are the same.

## 10. Where it stands

Design reviewed 2026-11-12: OK IF A1-A8, consultant ledger line 9131, and
OK IF A1-A11, line 9155 (A9 (i) chosen: the reuse takes a window's kept
pages back, section 5.4; its text is to come back before code).
