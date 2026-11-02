# Poison in a clean page-cache page

Design v2, 2026-10-31 (v1 eb8bef26c; consultant OK IF (1)-(12), ledger 5294, carried here). The customer's item 3 of the machine-check follow-ups
(2026-10-30: "clean page-cache poison: drop and reread from disk"). It builds
on `docs/MACHINE-CHECK.md` (H.FAIL.8), `docs/MACHINE-CHECK-AMD.md` and
`docs/MACHINE-CHECK-COPY.md` (H.FAIL.10, whose `copy_mc` it reuses), and
narrows the residual `MACHINE-CHECK.md` §6 and SAFETY-MANUAL AoU-36 name: "a
page-cache read ... is a machine check in ring 0, which halts", and "the
frame stays mapped in the consumer until it exits and in every other mapper".

## 1. What is there today

A page of a file on a disk lives in the file's VMO, the page cache
(`fs/pages.rs`, `user/cache.rs`). Three ways poison in such a frame shows:

* **A read** (`read(2)`, `pread`, `execve`'s loader): `VmoPages::read_piece`
  copies the page into the read's kernel buffer with `Vmo::read_present`,
  under the object's pages lock. A poisoned load there is a ring-0 machine
  check, which stops the machine as FX-9005.
* **A program that maps the file** and loads from the page: a ring-3
  consumption. The frame is retired and the program gets `SIGBUS`
  `BUS_MCEERR_AR` (`MACHINE-CHECK.md` §3.5), and the page stays mapped in it
  and in every other mapper.
* **The poll** finding an action-optional or no-action record at the frame
  (a patrol scrub's find): the frame is retired and nothing else happens; the
  page stays in the cache until someone consumes it.

For a **clean** page -- one the file's source can read again and the
filesystem has no newer copy of, what reclaim may take (`user/cache.rs`) --
all three lose nothing but a disk read: the page can be dropped and read
again. That is what this item does.

## 2. What it does

* A read that consumes poison in a clean cached page drops the page, reads it
  again from the disk and succeeds. A page that is not clean (a write not yet
  written back, a tmpfs or `memfd` page, a page a shared mapping may write)
  has no other copy: the read fails `EIO`, the frame is marked retired, and
  the page stays where it is (its data is what is lost; the next read finds
  it again, and fails again -- each a recovered machine check -- until the
  file is truncated or deleted).
* A program that consumes poison in a clean cached page it maps, with the
  instruction restartable (`RIPV`), is not signalled: the page is dropped
  from the cache and from every address space that maps it, and the program
  resumes and faults it in again from the disk. With `RIPV` clear, or a page
  that is not clean, it is as today.
* The poll's action-optional, no-action or deferred record at a clean cached
  page drops it at once, so nobody consumes it.

x86-64 only, under the rules that already let a ring-3 consumption recover;
AArch64 and ARMv7-A as today.

## 3. The mechanism

### 3.1 Dropping a poisoned page (`user/cache.rs`)

`cache::drop_poisoned(frame) -> Dropped` (new):

1. retire the frame first (`mm::retire_frame`; it is allocated, so this
   marks it). Only an answer of `Claimed`, `Marked` or `Already` goes on:
   on `Full` or `NotCovered` nothing is taken and it answers `NotRetired`,
   since a taken page's frame would otherwise go back to the allocator when
   the take releases it (ledger 5294 (1));
2. find its page: a walk of the cache list as reclaim walks it (the walker
   count held, no spin lock across an object), asking each live object
   `Vmo::index_of(frame)`. That is a scan of the object's page map, not a
   lookup: at most `CHUNK` entries a hold of its pages lock, as reclaim's
   pick does, let go between chunks (5294 (6)); the whole walk is bounded by
   the pages the cache holds, and runs only after a machine check;
3. take it as reclaim takes a page, through `Vmo::reclaim_clean`'s test --
   the object reclaimable now, the source saying the page is clean -- with
   one addition in the same hold of the pages lock as the take: the frame at
   the index is checked again to be this frame (5294 (2)); a page replaced
   meanwhile is not taken (`NotCache`), and a held page the take skips is
   `NotClean`, never `Clean`. Taken, it leaves the object and every address
   space that maps it (`decommit_clean_range`: the reverse map per object,
   then a shootdown), and the release keeps the retired frame out;
4. answer `Clean`, `NotClean` (found but kept: dirty, no source, written
   through a shared mapping, held, or a clean page the source holds still at
   that moment for a write, a truncation, a writeback or a reclaim of its
   file -- its read then fails `EIO` and its mapper gets `SIGBUS`; a retry
   would be a design point of its own), `NotRetired` or `NotCache`.

It takes locks and may wait for a shootdown: never in the machine check,
never under a spin lock, never under the lock a filesystem holds to keep a
file still (5294 (5)). Where the caller already knows the page (a read), it
passes `(vmo, index)` and step 2 is skipped (`drop_poisoned_at`).

### 3.2 A read (`fs/pages.rs`, `user/vmo.rs`)

`Vmo::read_present` copies through `arch::copy_mc` (`MACHINE-CHECK-COPY.md`
§3.1) instead of `copy_from_slice`. The ring-0 machine check recovers at the
copy's `rep movsb` under that design's rules unchanged: the record must name
the page behind `rsi`, which here is the cached frame's direct-map alias. A
copy that comes back poisoned makes `read_present` answer
`VmoError::Poisoned(frame)`, after checking the page against the frame it
read (a mismatch stops as FX-9005, as `uaccess` does), with the pages lock
let go on return. It writes no task record (5294 (7)). Every caller of
`read_present` matches `Poisoned` explicitly -- a caller that turned it into
a plain `EIO` would leave the frame unretired, to go back to the allocator
once freed -- and today that caller is `read_piece` alone.

`read_piece` then calls `drop_poisoned_at(vmo, index, frame)` with no lock
held:

* `Clean`: the loop it already runs for a page reclaim took goes round
  (`REFILLS`): the page is filled again from the source and read. The direct
  read from the source after the last round is reached only when that
  round's drop answered `Clean` (5294 (8)).
* `NotClean` or `NotRetired`: `EIO` at once.

**Locks at the call** (5294 (5)): `read_piece` runs under `Pages::read`,
called by tmpfs's and btrfs's file reads and by the loader; the code review
names, for each, the locks held there, and none may be a spin lock or the
hold-still lock `drop_poisoned_at` would take on the same file. A caller that
cannot meet that is listed and keeps the plain copy.

No task record (`uaccess`'s) is involved: the read acts on its own copy, in
the task that made it, wherever it runs -- a system call or a kernel task
(`execve`'s loader) -- since nothing here waits for a way out. The `in_call`
rule of H.FAIL.10 is `uaccess`'s, not this path's (the consultant's answer,
ledger 5294).

**Other readers of possibly cached frames keep `copy_from_slice`** and stop
as FX-9005 on poison (5294 (7)): `syscall/native.rs`'s two page reads (near
lines 1526 and 2079), `devmgr.rs`'s (near 595) and `render/mod.rs`'s (near
1759); §4 lists them. On AArch64 and ARMv7-A `arch::copy_mc` is the plain
copy and `Poisoned` never occurs.

### 3.3 A program's own consumption (`arch/x86_64/mce.rs`)

`mce::signal_for`, which runs with interrupts open and no lock held after
the ring-3 entry marked the frame, drops the page and answers no signal only
when all three hold (5294 (3)):

* the check was restartable (`RIPV` set: the mark without `MARKED_KILL`);
* the running task's address space maps the frame before the drop
  (`AddressSpace::find_frame` answers `Some`);
* `cache::drop_poisoned(page)` answers `Clean`, and the space no longer maps
  the frame after it (`find_frame` answers `None`).

Then `machine_check_signal` answers `None`, `trap::user_fault` returns
without raising anything, and the program re-runs the load, faults (the page
is gone from its tables) and fills the page from the disk. Otherwise
`SIGBUS` as today -- so a record naming some other page, or a frame still
reachable through a second holder, cannot loop the program on the same
poison -- and `signal_for` does not retire the frame again. L.x86_64.228's
`SIGBUS` clause and H.FAIL.8 gain this exception.

### 3.4 The poll (`arch/x86_64/mce.rs`)

The deferred retirement of an action-optional, no-action or AMD deferred
record (the poll's `retire`) calls `drop_poisoned` instead of
`mm::retire_frame`: a clean cached page goes, any other frame is retired as
today. The poll is a task with interrupts open and no lock held. A record
left from before this boot names a frame whatever now holds it: a clean
cached page there is dropped, which costs a read and nothing else.

### 3.5 What else changes and what stays

* A system call's copy from a mapped clean file page (H.FAIL.10's path)
  still fails the call and signals; the record's way out calls
  `drop_poisoned` in place of the bare retirement, so later touches read
  good data. Retrying the call is not attempted. L.syscall.25 is amended,
  with a `test-mce-copy` mode on a clean file mapping (5294 (9)).
* Lines: `mce: frame <phys> retired` as today, then `mce: frame <phys> was a
  clean cached page, dropped` (or `... not clean, kept`) -- addresses only in
  the kernel log, as today's line. Stage 12's poison check holds these lines back
  while it names frames poisoned itself, so that a boot with the checks
  announces no retirement of a frame nothing found bad (the lines first
  confused test-mce-amd-deferred's and -corrected's judges, which look for
  any `mce: frame`). A real machine check landing in that window, stage 12's
  poison cases, would be silenced too, and its retirement forgotten with the
  check's own by count: the same window, before the system runs anything.

## 4. What it does not do

* A dirty page's data is lost (the read fails `EIO`, a mapper is signalled).
  Keeping it in place is stricter than Linux, which removes it and lets later
  reads see the disk's older bytes. **Writeback of a kept dirty page whose
  frame is retired** (5294 (4), option (b); 5311 (4)): btrfs writeback reads
  it through `read_present`'s recoverable copy, so each round recovers the
  machine check, fails `EIO`, and leaves the page dirty for the next round
  (first seen in test-mce-pagecache's first dirty run). A writeback read
  outside that copy would halt and a device's DMA could put the poison on the
  disk; btrfs has none today. AoU-36, `MACHINE-CHECK.md` §6 and
  VULNERABILITY-ANALYSIS say so, and a BACKLOG row asks writeback to skip a
  retired frame (the page staying dirty, `fsync` answering `EIO`).
* The other readers of cached frames §3.2 names stay as they are.
* A page a device DMA reads, anonymous memory, the kernel's own memory: as
  today.
* The copy-on-write copy: `MACHINE-CHECK-COPY.md` §4's follow-up.
* Under QEMU the injected record is believed and no real poison is loaded:
  the gate shows the decision, the drop and the refill; the backlog row for
  a hardware run (`MACHINE-CHECK-COPY.md`) covers this too.

## 5. Requirements

To reserve before code, in one docs-only landing (all free on origin/develop;
L.object.253 stays released):

* **H.FAIL.11**: poison in a clean page-cache frame shall cost a disk read
  and nothing else -- a read refilled and answered, a mapper refaulting
  without a signal, the poll dropping it first -- and a page that is not
  clean shall never be dropped or refilled (its read fails `EIO`, its mapper
  is signalled as H.FAIL.8 says); a frame that cannot be retired shall never
  be taken.
* **L.object.259**: `cache::drop_poisoned` retires the frame before it takes
  the page and takes nothing unless the retirement answered `Claimed`,
  `Marked` or `Already`; finds the page by a walk of the cache list under the
  walker count, at most `CHUNK` entries a hold; takes it only through
  `reclaim_clean`'s test with the frame checked again in the take's hold;
  and answers `Clean`, `NotClean`, `NotRetired` or `NotCache`.
* **L.object.260**: `Vmo::read_present` copies through `arch::copy_mc`,
  answers `Poisoned(frame)` for a stopped copy after checking the page
  against the frame it read, and writes no task record.
* **L.object.261**: `VmoPages::read_piece` on `Poisoned` drops the page with
  no lock held, goes round its fill loop again only after `Clean`, and fails
  `EIO` at once on `NotClean` or `NotRetired`.
* **L.x86_64.247**: a restartable ring-3 consumption of a frame the running
  space maps, dropped `Clean` and mapped no more, is answered with no signal;
  anything else as L.x86_64.228.
* **L.x86_64.248**: the poll's deferred retirement drops a clean cached page.

## 6. How it is checked

* **Stage check** (`user/cache_check.rs` or the reclaim check's file): a
  clean page of a file on the test disk is read, its frame named poisoned
  through `drop_poisoned`: the frame is retired, the page gone from the
  object and from a mapping of it, and a second read returns the disk's
  bytes; a page written and not written back answers `NotClean` and stays;
  a tmpfs page answers `NotClean`; an anonymous frame `NotCache`; with the
  retired set full `NotRetired`, the page kept; a held page `NotClean`; a
  page replaced between the find and the take `NotCache`. A `read_piece`
  whose `read_present` answers `Poisoned` (a check-only hook,
  `inject_read_poison`, compiled into check builds only and unreachable in a
  product build) refills and returns the source's bytes; on a dirty page it
  fails `EIO`. The ring-3 decision's three conditions over frames the check
  maps and unmaps.
* **`test-mce-pagecache`** (x86-64, KVM with AMD's records as
  `test-mce-copy`, each mode naming the record class it injects): a reader reads a file of the btrfs test disk in a loop;
  the gate stops the machine with `rip` at the copy's movs and `rsi` in that
  file's cached page (the page's bytes are the disk fixture's, which xtask
  knows), injects there, and the reader's read must succeed with the disk's
  bytes, the frame retired and the page dropped (`mce: ... dropped`), the
  kernel going on. **Mapped mode**: a program maps the file and loads from a
  page in a loop; the gate injects in ring 3 at that page; the program must
  get no `SIGBUS`, read the disk's bytes after, and the frame be retired and
  dropped. **Dirty mode**: the same with a page that has no other copy,
  a `memfd`'s: the read fails `EIO`, the mapper gets `SIGBUS`. A disk page
  written and not written back is covered by stage 12's check instead: on
  the writable disk such a page is written back within seconds, so no gate
  run could keep it dirty while it injects (the first run caught the disk's
  writeback reading the page through the same copy, and kept it). **Poll mode**: Intel's no-action record (UCNA) at a clean
  cached page, under TCG -- the one machine here whose Intel model reports
  `MCG_SER_P`, without which every uncorrected record is a machine check that
  stops -- drops it (`mce: ... dropped`) before anyone reads it. AMD's
  deferred record cannot show it: it is retired only by `PhysAddrV`, which no
  emulator's banks set (test-mce-amd-deferred pins that it is logged and
  nothing retired). Intel's action-optional record is shown by the stage 3
  tables, and the Intel-host BACKLOG row (ledger 5283 (3)) is extended to it. TCG rows fail-closed only, and say
  so, as `test-mce-copy`'s. **`test-mce-copy` file mode**: a system call
  copying from a mapped clean file page fails, raises `SIGBUS` and drops the
  page.
* **Negative controls**: the clean test removed (a dirty page is dropped:
  the dirty mode reads the disk's older bytes); the retirement before the
  take removed (the frame goes back to the allocator: `/proc/ferrix-mce-
  retired` lacks it); the mapper's drop answered as `NotCache` (the mapped
  mode signals); `read_piece`'s refill on `Clean` removed (the read fails);
  the retirement answer's test removed (5294 (1)); the frame check in the
  take's hold removed (5294 (2)); the after-drop `find_frame` test removed
  (5294 (3)); `read_present`'s `Poisoned` turned into `EIO`. Each through
  `gate.sh control --expect`, FIRED.

## 7. Docs

SAFETY-MANUAL AoU-36 (a clean cached page is dropped and read again; a
dirty one's data is lost, the read fails `EIO`); VULNERABILITY-ANALYSIS
T.EXHAUST path 8 (the walk's bound, run only after a machine check);
MEMORY-AND-TIMING (the walk: at most every cached page's map entry, one
object's pages lock at a time, `CHUNK` entries a hold, interrupts open, run
only after a machine check; `copy_mc` on every file read, with a
before/after read throughput figure in the code review); `MACHINE-CHECK.md`
§6; a BACKLOG row for writeback skipping a retired frame.

## 8. Where it stands

* Design v1 (eb8bef26c) reviewed 2026-10-31, ledger 5294: **OK IF
  (1)-(12)**, with the in_call question answered (the read acts on its own
  copy, sound in kernel tasks too, as long as it writes no record and every
  caller matches `Poisoned`). v2 carries (1) the retirement's answer (§3.1),
  (2) the frame checked in the take's hold (§3.1), (3) the ring-3 path's
  three conditions (§3.3), (4) writeback as a residual, option (b) (§4), (5)
  the locks at each call (§3.1, §3.2; named in the code review), (6) bounded
  holds (§3.1, §7), (7) the callers and the other readers (§3.2, §4), (8) the
  fill loop (§3.2), (9) the split ids and the copy path's amendment (§5,
  §3.5), (10) the checks (§6), (11) the controls (§6), (12) the docs and the
  code review (§7).
* Code reviewed 2026-10-31 at 4cf8dde5d, ledger 5311: **OK IF (1)-(7)**:
  the rows and controls on the landing hash, a clean page held still by its
  file kept as not clean in the texts (§3.1), the writeback residual as the
  build has it (§4), the criteria naming each mode's machine and record, the
  throughput argument, and a clean final rebase with coverage carried.
* Re-reviewed 2026-10-31 at b8832db4b, ledger 5324: **OK IF (A)-(C)**:
  test-mce-copy-environ accepted on mcp9-r59 (4cf8dde5d) and the clean
  rebase, its four pool failures on b8832db4b (a QMP close, QEMU gone with no
  signal line, no stderr, no core, no OOM or kill in the host's journals)
  cited and a BACKLOG flake row; the dirty read's criterion as the judge
  reads it.
