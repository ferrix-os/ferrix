# The heap gives an emptied slab page back without walking its class (D1)

Design addendum for branch `heap/slab-give-back` (silent-reset,
2026-11-13). This is landing D1 of design (d), `docs/EXIT-TEARDOWN.md`
(consultant ledger 9543, OK IF ET.1-ET.9, and 9574). Condition ET.2 asks
for this addendum to be reviewed before any of D1's code is written.

## 1. What it removes

`ferrix-heap` keeps each size class's free objects on one singly linked
list, threaded through each object's first word (`Heap::free`). When a free
returns the last object of a slab page, `give_back_if_empty` calls
`unlink_page`. That function walks the class's whole list, from its head, to
take the page's objects off it.

The kernel calls this under `HEAP`, the machine's one `IrqSpinLock`. So
every walk runs with interrupts masked on that processor, and every other
processor whose allocation misses its cache waits for it.

`docs/EXIT-TEARDOWN.md` §2 measured what this costs at an exit:

| Exit | Walks | Links read | Heap frees | Mean walk |
|---|---:|---:|---:|---:|
| 60,000 regions | 26,983 | 356.5 million | 3.3 s of a 3.3 s teardown | 0.12 ms |
| 9 GiB | 48,794 | 4.59 billion | 222.2 s of 222.7 s | 4.55 ms |

On the N150, a killed game left the box without ssh for 60 s, and in its
world for more than 145 s. Section 4.5 of that note explains why the whole
machine waits, not only the exiting task.

## 2. Two shapes, and the one chosen

### (a) SLUB's shape: per-page lists, not taken

This is the shape of Linux's `mm/slub.c`:

* each slab page has its own free list;
* the class keeps a list of its partial pages, doubly linked through the
  page's `struct slab`;
* an emptied page leaves that list with `remove_partial` (`list_del`, O(1))
  and goes to `discard_slab`.

Ferrix has nowhere to keep that page state. Each slab page would need a
free-list head (a 9-bit index), its free count (`slab_free`, which exists),
and two links of the partial list (frame numbers, 4 bytes each). That is
about 10 more bytes. `PageEntry` (`ferrix-frame`) is 16 bytes with no
padding left: `next` u32, `previous` u32, `refcount` u32, `order` u8,
`state` u8, `slab_free` u16.

For an allocated order-0 frame, the free bytes would be:

* `next`, which is unused;
* `previous`, which carries `Frames::set_owner`'s charge (`mm.rs` uses it
  for charged frames and page tables, `large.rs` for `KEPT_OWNER`);
* `refcount`, which belongs to `Frames::share`.

Giving those fields a second meaning on slab pages would make the frame
allocator's fields mean different things by page kind. Every reader of
`owner` and of `refcount` (the MCE poison and rmap paths among them) would
then need a slab-page test, under a checks-only assertion. Growing
`PageEntry` to 24 bytes instead costs 8 bytes per 4 KiB frame, which is
0.2 % of RAM (32 MiB on the N150's 16 GiB). Neither is needed for what this
landing must do.

### (b) Chosen: the class's free list doubly linked, an emptied page unlinked by index

The structure:

* Each free object holds two links: word 0 `next` and word 1 `previous`
  (`END` at either end of the list).
* The class's head is unchanged.
* Allocation pops the head, LIFO as now.
* A free pushes at the head, as now.

When a free empties a page, `slab_free` has just reached the page's object
count. So every object of that page is free and on its class's list: none
is handed out, and none is in a processor's cache, because a cached object
counts as allocated to the heap. `unlink_page` therefore visits the page's
objects by index (`page + i * size` for `i` in `0..objects_per_page`). It
unlinks each one in O(1) through its own two links. It never reads another
page's objects and never walks from the head.

The cost:

* **Every object must hold two links,** so `SMALLEST_CLASS` goes from 8 to
  16 bytes. `CLASS_SIZES` becomes `[16, 32, ..., 2048]`, eight classes, and
  a request of 8 bytes or fewer takes 16.
* **Measured,** by a never-landing diag (`diag/heap-classes`, a print at the
  end of a boot; x86-64 KVM, `test-boot`, dev): at `FERRIX-BOOT-OK` the
  classes held `[2, 2, 8, 16, 81, 241, 184, 83, 79]` pages, of 8 B to 2 KiB,
  and 2,841 pages as large blocks. The 8-byte class held 2 pages, 8 KiB. Its
  objects move to the 16-byte class: at most 2 more pages at boot.
  Desktop figures are owed with the code (section 7).
* **Unchanged:** `PageEntry`, `FRAMES`, the frame allocator, `Backing`'s
  contract (8-byte link accesses inside memory the heap owns), `Reserve`
  (its own singly linked lists through word 0 of objects it holds as
  allocated) and `CpuCache`.

## 3. What bounds each hold of the heap's lock

Before and after, per operation, under one hold of `HEAP` (or of a
reservation's heap):

| Operation | Before | After |
|---|---|---|
| allocate, class not empty | pop: 1 link read; `take_from_page`: `slab_free` read and set, 2 `FRAMES` holds | pop: 1 link read, 1 link write (the new head's `previous`); the same 2 `FRAMES` holds |
| allocate, class empty (refill) | 1 page from the buddy (`FRAMES`), `objects_per_page` pushes, 1 `set_slab_free` | the same, each push 2 link writes plus the old head's `previous` |
| free, page not emptied | push: 1 link write; `slab_free` read and set, 2 `FRAMES` holds | push: 2 link writes and the old head's `previous`; the same 2 `FRAMES` holds |
| free, page emptied | the class's whole free list walked (up to 94,000 links a walk at 9 GiB), then the page to the buddy (`FRAMES`) | at most `objects_per_page` unlinks (256 for 16 B, 2 for 2 KiB), each 2 link reads and up to 2 link writes, then the page to the buddy |
| overflow (`CpuCache`, 9 frees) | 9 frees, each up to a whole-list walk | 9 frees, each bounded as above |
| drain (144 frees a processor) | up to 144 whole-list walks a processor, interrupts masked | at most 144 x 256 unlinks a processor |
| `release_kept` (an idle reservation heap) | a walk per kept page | `objects_per_page` unlinks per kept page |

`FRAMES` is taken under `HEAP` exactly as often after as before.
`MEMORY-AND-TIMING.md` §1.8 gets the new bound: a free's hold of the heap
lock is O(1), plus at most `objects_per_page` unlinks and one page given to
the buddy when it empties a page. The overflow's and the drain's bounds
follow from it. The text "a free that empties a page walks its class's free
list" goes.

## 4. What stays as it is

* **L.mm.13:** a class still keeps its last page (`pages_per_class`), and
  pages still go back when their last object returns. Allocation still takes
  the head object, wherever its page is. There is no "first partial page" in
  this shape, and placement and fragmentation are unchanged.
* **HEAPS (L.mm.81):** a reservation's heap is the same `Heap` type over
  `ReservedPages`, so it gets the same lists and the same bound. Its
  give-back at once is unchanged.
* **`drain_caches`, `CpuCache`, `Reserve`:** unchanged. What they hold is
  allocated, as far as the heap knows, and is never on a free list.
* **`free_objects`,** the diagnostic count, walks `next` as now, under its
  budget.

## 5. A corrupted list still ends

`unlink_page` is a loop of exactly `objects_per_page` steps, so it ends
whatever the links say. Pop and push are straight-line code. No heap
operation walks a list any more; only `free_objects`, the diagnostic,
does, under its budget.

In a build with checks on, each unlink first verifies its neighbours: the
`previous` object's `next`, or the class's head, names this object, and the
`next` object's `previous` names it. A mismatch stops the kernel with a new
catalog entry ("a heap free list's links disagree"). This catches a double
free and a use-after-free that wrote over a link. In a build with checks
off, the unlink writes what the links say, as today's walk does.

## 6. `bugfix/heap-batched-give-back` and L.mm.135

Ledger 9543's ruling Q1: D1 supersedes it, and it does not land first.

* The branch (`97e14c9bd`, OK IF HB.1-HB.7 at ledger 7960, its box A/B
  owed since 2026-11-08) is parked with a note pointing here.
* Its reservation of L.mm.135 (`ef-slow`, requirement-reservations.json) is
  released by D1's landing: the entry is removed, and the id is never
  written.
* Its BACKLOG row, if any, is closed as superseded by D1.
* If the product owner lands it before D1 anyway, D1 removes its
  set-aside and withdraws L.mm.135 by name.

## 7. Requirement, checks and controls (ET.3)

**One new id, L.mm.\<n\>,** reserved first in a docs-only landing:

> "A free that empties a slab page shall take that page's objects off its
> class's free list and give the page back at a cost bounded by the objects
> one page of its class holds, whatever the length of the class's list."

Parent: H.MEM.9. Units: `Heap::deallocate`, `give_back_if_empty`,
`unlink_page`, `unlink_one`.

**Host test (`ferrix-heap`).** A class list of at least 100,000 free
objects over many pages. The test's `Backing` counts link reads and writes.
Each page's give-back must read and write at most 4 x `objects_per_page`
links, and the class list must hold exactly the objects of the pages still
held. Control (`gate.sh control --expect-host`): the class-list walk put back
in `unlink_page` -> FIRED (host), the test's line.

**Boot check (stage 3's heap check).** A CHECKS-ONLY counter of the links
`unlink_page` touches. The check allocates and frees enough objects of one
class to empty about 1,000 pages, on the main heap and on one reservation's
heap, and asserts the counter at most 4 x `objects_per_page` x pages
emptied. It asserts a count, never a time. Control: the same walk put back
-> FIRED (panic).

**Ring 0.** A rise of about 30-60 library lines (the second link, the
unlink by index, the checks-only verification and counter). It is argued in
`ring0-budget.json` with the consultant's rise line, citing 9543, with its
block on top of the chain.

**Rows on the final tip (gate.sh, run: PASSED, accelerator named):**

* check, check-docs, test-ring0-budget;
* check-frames in its four builds;
* check-stack-depth `--arch all`, and `--arch x86_64 --kernel-profile
  desktop`;
* test-boot on x86_64 kvm and tcg, aarch64, armv7a, and armv7a `--smp 2`;
* test-threads `--arch all`;
* test-exit-regions on the three architectures;
* test-btrfs-pressure;
* test-stall.

Coverage is carried. The x86_64 `--release` kvm exit row runs on both base
and tip, with both figures in the commit.

**The exit figures, before and after.** In a release KVM guest with
`ferrix.checks=skip`, for the 60,000-region, 4 GiB and 9 GiB shapes:

* the teardown time;
* the longest single hold of `HEAP` during the teardown, which is the worst
  masked stretch. It is measured by the never-landing diag of
  `docs/EXIT-TEARDOWN.md` §2, extended with a maximum per hold.

The N150's figure, the game's kill, is for the product owner's box run. It
is not a landing condition (ET.3).

## 8. What this does not do

* The exit's emptying (D2) and its steps (D3).
* Fork's steps.
* A per-page free list (shape (a)). That one would also cut the heap's
  fragmentation, and is a design of its own if a measurement ever asks for
  it.
