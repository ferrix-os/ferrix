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

`regionbench` (`~/.local/share/ferrix/ef-30fps/faultbench/regionbench.rs`,
sha256 a749b0cd...; release builds, KVM guest on nazuna with xtask's `-cpu`,
Ferrix with `ferrix.checks=skip`) does all three:

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
addresses continue to, when the object holds no page there. The map then
merges the two regions whenever their flags are equal, as it already merges
an `mprotect`'s pieces of one object, and if they are not, a later
`mprotect` that makes them equal merges them.

The offset convention does not change: a new object still starts at offset
0. Linux's convention, an anonymous object's offset being its address
(section 7.1), would let a mapping join its neighbour on either side, but
every anonymous object would then be as long as its highest address in
pages, and the code that takes a whole object (`Vmo::seal`,
`hold_whole_on_payer`, the lift build's bridge copy, the native size checks)
would meet objects of up to 2^35 pages. It is left until the figures show
the case it adds (section 7.1).

## 5. The mechanism

### 5.1 Own anonymous objects

An object id of a space is *own anonymous* when the space's regions name it
only by `Backing::Anonymous` with `flags.shared` clear, and it is in none of
the space's `native`, `files` and `shadows` tables. Those are the objects
`map_anonymous`, `map_anywhere` and a private `mremap`'s move make; every
handle and kernel mapping puts its id in `native` (`map_object`, the code
and stack mappings, `vmo_map`), a file mapping in `files`, a private file
mapping's copies in `shadows`. A `user`-personality window is never one.

**One line (I1).** For each own anonymous object X there is an address
`base(X)` such that every region naming X names, for each of its addresses
`v`, the offset `v - base(X)`. So no two addresses of the space name one
offset of X. It holds today:

* `map_anonymous` and `map_anywhere` make X with one region at offset 0:
  `base(X)` is its start.
* A split (`mprotect`, `munmap` of a part, `madvise`) keeps each piece's
  offset at its address's.
* A private `mremap` moves the region's pages into a new object at offset 0
  at the new address (`move_private`), and X keeps only what it had, so X
  gains no region off its line. A shared one keeps its object, but a shared
  object is not own anonymous.
* `fork` copies the objects by id and the regions as they are, so a child's
  objects keep their parent's lines.

The reuse below places a region on the neighbour's line by construction, so
I1 still holds after it.

### 5.2 Placement

In `map_anonymous` (and so `MAP_FIXED`, Wine's every call, and `brk`) and in
`map_anywhere`, under the space's lock, before a new object is made:

1. Find the region ending at the new range's start (the left neighbour) and
   the one starting at its end (the right neighbour): two lookups in the
   map, O(log regions).
2. Take the first of them, left before right as Linux tries `prev` first,
   that is own anonymous and private, with the new mapping private, and
   whose line gives the new range offsets that are not negative: the left
   neighbour's offset plus its length, or the right neighbour's offset less
   the new length. (Equal flags are not required: a later `mprotect` merges
   what the flags kept apart.)
3. Under that object's pages lock, require that no page is committed and no
   page held in those offsets. By I1 no region names them, since a region
   naming them would sit on the new range's addresses, which are free. So
   the window is empty unless an unmap's `give_back` left pages there: it
   takes an unmapped window's pages back only when the space holds the sole
   reference to the object, and keeps them when it cannot list them. A
   window with a page is declined, and the mapping gets a new object as
   today. Nothing can commit a page into the window between this check and
   the region's insertion: a fault commits only through a region, and no
   region reaches the window until the insertion, which comes next under
   the same space lock.

   An unmap gives its window's pages back after it lets the space lock go
   (its third phase), and when it cannot list them it decommits the range
   then, by offset: a reuse made in between would lose its new pages to
   that decommit. So the reuse runs only with the space's layout lock held,
   which every map change of a Linux process holds for its whole call
   (`mmap`, `munmap`, `mremap`, `brk`, `shmat`, `shmdt`, the native map
   calls): no other change's third phase can be pending then. The signature
   takes the layout guard, so a caller without it cannot ask. Two unmaps
   take no layout lock: the OOM emptier (`object/oom.rs`), which empties a
   process already killed, so a page one of its last threads maps and loses
   to that race can read as zero only to that thread before it ends, and is
   lost with the process; and the lift
   build's core (`lift/core/sys.rs`), whose map calls the code review is to
   check against this rule.
4. Grow the object to cover the window (`Vmo::grow_to`, which only raises a
   number), count the region in (`count_in`, as `add_region` does: X's view
   in this space raises its cover over the window, and moves no count,
   since the window holds no page), and insert the region naming X at that
   offset. The map's insert merges it with each neighbour it continues.
   The object is in the tables already and this space is its mapper, so
   nothing is attached and nothing is added to the tables.

A refusal of the count-in or the insert (no memory) undoes the count-in
exactly, as `add_region` does, and answers the refusal; the grown length
stays, as `grow_to` never shrinks and a longer object costs nothing.

So a fresh, untouched mapping reads zeros (L.user.243) and shares no page
with any other address (L.user.244), as a new object's would.

### 5.3 What it does not change

`unmap`, `room_to_count_out`, `all_still_named` and `give_back` are not
touched: the count-out of a region naming a reused object is the same
`counting` and `lower` as any region's. `Named::of`, `resident_pages`,
`all_still_named` and `room_to_count_out` are being made to sort without
recursion, and the exit path's design, an exit teardown that takes no room
it can be refused, is `fix/exit-unmap-sort`'s (ledger 9112, F-112, its
design (d)): it owns the exit, and this change leaves it to that branch.
What reuse does to today's reservation there, for that branch: k regions
that named k objects reserved 3 runs in each of k lists; after reuse, the k'
regions left once they merge (k' at most k) name one object, whose one view
takes 2k' + 1 runs in one list. Fewer runs in all, but in one list, and so
one larger request that can be refused. Nothing in this design takes that
room or depends on it, and once (d) takes no room at exit, the difference
goes with it.

`mprotect`, `madvise`, `mremap`, `fork`, faults and `maps` are not touched.
`maps` prints an offset only for a file's region, so a reused object's
offsets are not seen.

A job's `Objects` count no longer rises with each anonymous mapping that
reuses its neighbour's object: Linux has no such count, and the count was
never a limit a program could see per mapping.

### 5.4 Costs

Per anonymous mapping: two map lookups, one probe of a `BTreeMap` of frames
and one of held pages under an object's lock. A reuse saves an `Arc<Vmo>`,
an `Objects` charge, a mapper record (`MAPPER_HEAP`), an object-table entry
and a resident view. No allocation is added.

## 6. Requirements

New, reserved as `L.user.242`-`244`:

* `L.user.242`: a private anonymous mapping made beside a region of the same
  space whose object is that space's own private anonymous memory is made in
  that object, at the offsets that object's addresses continue to, when the
  object holds no page there and those offsets are not negative; so the two
  regions are one whenever their flags are equal.
* `L.user.243`: a private anonymous mapping, made in a neighbour's object or
  a new one, reads as zero on every page until written.
* `L.user.244`: every region of a space naming one of that space's own
  private anonymous objects names, for each address, that address less the
  object's base: no two addresses of a space name one page of such an
  object.

## 7. Left out

### 7.1 A mapping just below its neighbour

`map_anywhere` searches top down, so a plain `mmap` lands just below the
last one, whose object starts at offset 0 and has no room below it. Joining
it needs the neighbour's offsets to leave room, which Linux's convention
(offset = address) gives every anonymous object. With section 4's objection
answered (the whole-object operations taking a length, not reaching every
page), it is one line in `map_anywhere` and `map_anonymous`. Measured first:
the game's own `maps` on the box shows how many of its regions are of this
kind and how many are Wine's reserve.

### 7.2 Joining two objects

A mapping between two different own anonymous objects joins the left one
only. Merging the right one in would move its pages and change its id in
every region naming it: Linux does not do that either, unless the two
`anon_vma`s are the same.

## 8. Checks

A core self-check, `user/anon_merge_check.rs` (every profile, the `core
anm` line), on a space of its own:

* Wine's pattern: a reserve, views mapped `MAP_FIXED` into it, some
  decommitted and released the same way: each mapping takes the reserve's
  object, the regions are as many as the protections' changes, and an
  `mprotect` that makes them equal leaves one region (`L.user.242`);
* `brk`'s pattern, mapping after mapping at the end: one region, one object;
* a right neighbour with room below its offset (made by unmapping its front):
  taken;
* declined, each with a new object: a window holding a page (an unmap whose
  `give_back` kept it, made with a second reference held), a neighbour mapped
  by handle, a shared neighbour, a shared new mapping, a right neighbour at
  offset 0;
* a window written, unmapped and mapped again reads zero, through a fresh
  fault (`L.user.243`);
* each reused region's offset is its address less the object's base, and the
  space's resident counts equal `user::rss`'s walk after every step
  (`L.user.244`, `L.user.236`);
* `fork` after reuse: the child reads the parent's bytes and its own writes
  stay its own.

Negative controls (`gate.sh control --expect`): the window check left out
(the zero read fails); the right neighbour's offset taken without its room
check (a negative offset refused, or a page shared); the native-table test
left out; the reuse off altogether (the region count fails).

## 9. Figures owed

`regionbench` and `statusbench` before and after, release builds with
`ferrix.checks=skip`, Linux 7.0 in the same KVM guest; then the game's
`maps` line count and its `mmap` costs on the box, by the coordinator.
