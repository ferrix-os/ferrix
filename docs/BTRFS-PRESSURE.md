# btrfs under memory pressure (`test-btrfs-pressure`)

**2026-11-08, against Linux.** Ferrix's btrfs failure paths read against
Linux v7.0's `fs/btrfs`: see "Against Linux's btrfs", and its work items
W1-W7 in `docs/BACKLOG.md`.

**2026-11-08.** The N150's hangs during play: a btrfs transaction aborted
out of memory at the floor and its reload ran out too; see "The
transaction aborted out of memory" and the gate's `commit` boot.

**Where it stands (2026-10-18).** Branch `pagecache-evict`, on `main` since 2026-10-18
(edf7a8722, 0bb74a0c3): the page cache dropped whole under a burst of new files, the dentry cache's
doing (F-90), and the gate's `evict` boot that shows it ("The page cache
dropped whole"); the certification consultant's review: OK IF (1)-(9),
ledger line 2336. The box run and a reclaim-driven release of kept dentries
are BACKLOG rows.

**2026-10-17.** The certification consultant's review of
branch `btrfs-pressure`: OK IF (1)-(8), ledger line 2314. The floor's
`ENOSPC` is fixed on the branch (not landed); the gate is an owed row for
the page cache, reclaim, the frame allocator, btrfs and the memory charge
(`docs/BACKLOG.md`, *What a landing runs*). Open: F-86 to F-88 at
`memory.max`, and F-89, the N150 stopping during the launcher's delete,
which waits on a box run.

Every other btrfs gate writes a little into a lot of memory, so the page
cache never fills and reclaim is never reached. This gate makes a small
machine write gigabytes through a writable btrfs the way Endfield's launcher
unpacks the game, and then delete all of it.

## What went wrong (N150 box, 2026-10-17)

GRYPHLINK's 7-Zip (`7zg.exe` in GE-Proton's Wine) unpacking the 59-part
Endfield 1.6.4 archive was told `ERROR_DISK_FULL` ("Disk full",
`0x80070070`) with 400 GB free, four times, after 30 s to 10 min; the
launcher then deleted what it had unpacked and showed "Download Error". Twice
the box then stopped answering while the launcher deleted.

The cause is in `fs/pages.rs`. A write whose page has no frame reclaimed one
fill run (32 pages) of clean cache and tried again, at most eight times, and
then answered `ENOSPC`, which Wine maps to `STATUS_DISK_FULL`. At the
machine's floor every one of those reclaims gave pages back -- there were
hundreds of megabytes of clean cache -- and every retry still found no
frame: a program's frame is refused while fewer than the watermark are free
(`mm::watermark`, a 128th of memory, 32 MiB on the box), the kernel's own
allocations go on below it, and the 32 pages one reclaim handed back went to
the kernel or to another processor first. A fill had the same bound and
answered `ENOMEM` (the 2026-10-15 `GameAssembly.dll` failure,
`docs/WINE.md` §10).

The fix: a machine-wide reclaim for a frame asks for what the free count
lacks of the watermark as well as for a run (`fs/pages.rs`,
`reclaim_want`), so the frames it gives back lift the free count over the
floor and the retry has one. The bound of eight reclaims stays, as
`docs/certification/MEMORY-AND-TIMING.md` states it. Going on without a
bound while a reclaim gives back, the rule a fault follows, was tried first
and passes the floor too; it was dropped because it is the larger change to
argue, and because in the `memory.max` boot a writer stopped moving with it
-- as it does with the bounded fix, so that stall is the memory.max path's
own (see "At memory.max"), but an unbounded loop there would be one more
way for it to never end.

The bound, as `docs/certification/MEMORY-AND-TIMING.md` states it: the
watermark is at most `WATERMARK_MOST`, 8192 frames, so one machine-wide
reclaim steals at most 8192 + 32 = 8224 pages, and a frame takes at most 8
reclaims; the scan stays bounded by reclaim's two passes round the live
list, not by what is asked for, and no lock is held across a reclaim. A
job's reclaim at `memory.max` still asks for one run. Evidence: `gate.sh`
logs `btp-pressure` and `btp-ctl` (below), ledger line 2314.

## The page cache dropped whole (N150 box, 2026-10-18)

With 16 GB of game files cached (MemFree at the floor), a full-speed `tar`
unpack of GE-Proton onto `/data` took `Cached` from 15.97 GB to 0.13 GB in
about 13 s, and the box answered slowly for 16-27 s: ssh logins failed, ping
took 20-80 ms (`~/.local/share/ferrix/n150-linux/FINDINGS.md`, "Page cache
evicted whole under a write at the floor").

Reclaim was not the cause: a machine-wide reclaim asks for a run and the
watermark's shortfall, at most 8224 pages, and on the box it gave back about
60 MB at a time for a minute before the drop. The dentry cache was. It keeps
at most 4096 dentries nobody holds (`ferrix_vfs::namespace::DEFAULT_CACHE`),
oldest out first, and a btrfs file's page cache lives in its inode, which
only its dentries and its open files hold (`ferrix_btrfs_vfs`, `Node`; the
mount's map of inodes is weak). Every name `tar` made pushed one old dentry
out, and once the game files' came to the front of the queue each one's
whole cache went with its inode: a gigabyte at a time. The count of names
looked up decided how much of the cache stayed, not memory.

The fix keeps the dentry of a file that holds cached pages when the queue
lets go of it, in a set beside the queue (`Paged`, by dentry id), until
reclaim has taken the pages (`Inode::holds_cached_pages`, which btrfs's
read-only and writable inodes answer; a writable file with no name left is
never kept). Each lookup that misses looks at two of the kept dentries again
and lets go of those whose pages are gone, a clock at a constant cost per
lookup; an unlink, an `rmdir`, a rename over a name and an unmount take
theirs out at once, as they do from the queue, so a deleted file's pages
and blocks still go when its last user lets go. Linux keeps an inode with
pages in its cache the same way: pruning dentries does not drop page cache.

Since the dentry cache no longer bounds how many files are cached, reclaim
stopped copying the list of every cached file and upgrading each entry on
every call; it takes the list's lock for one entry at a time from where the
last reclaim stopped (`user::cache::reclaim`).

The stall did not show in QEMU (the probe at most 6 cs on main, at 768 MiB
and at 4 GiB). On the box it followed the drop: with the root's and
everything else's cache gone, each ssh login and each of the watcher's
commands read its programs again from the disk, behind `tar`'s writeback.
That it goes with the drop is the box run's to show (below).

The `evict` boot of the gate (below) shows it, KVM, x86-64, debug kernel,
logs in `~/.local/share/ferrix/n150-linux/pagecache-evict/`:

| Kernel | Memory | Cached before | Cached at its least | Probe | Log |
|---|---|---|---|---|---|
| main a7605883a | 768 MiB | 695864 kB | 55524 kB (8 %) | 1 cs | `run1-main.log` |
| the fix | 768 MiB | 695880 kB | 665292 kB (96 %) | 1 cs | `run2-fix.log` |
| main a7605883a | 4 GiB | 4010940 kB | 55508 kB (1 %) | 6 cs | `run3-main-4g.log` |
| the fix | 4 GiB | 4010920 kB | 3980204 kB (99 %) | 1 cs | `run4-fix-4g.log` |

What the fix leaves (F-90's residual, ledger line 2336): a kept dentry stays
charged to the job that looked it up until a lookup's clock finds its pages
gone, so a quiet machine whose cache reclaim emptied, or one whose lookups
only hit, keeps their heap; at `memory.max` that is memory reclaim cannot
give back. A file kept this way costs its dentry, inode and object's heap
besides its pages, as Linux's inode cache does. The set grows fallibly with
no lock held and drops what it has no room for, as the queue did; unmount
takes the mount's kept dentries out in one hold of the set's lock, after the
queue's.

## The transaction aborted out of memory (N150 box, 2026-11-08)

Three hangs during Endfield play on 2026-11-08. The third left its kernel
log in the loader's kept panic log
(`~/.local/share/ferrix/bw-repro/catch1/LASTKERNEL-hang3.TXT`, lines about
12087-12271, uptime 2126-2140 s; the lode store's copy is `hang3.txt`
beside it):

```
btrfs    a transaction was aborted (out of memory), and the last commit cannot be read again (out of memory); every answer is EIO
init     lode.service: reading /sbin/lode: I/O error (os error 5)
signal   pid 31 is hyprix exe /bin/hyprix ... ended by signal 7 ... PageFault { execute: true, present: false }
init     auth.service: execve failed: I/O error (os error 5)
FERRIX-PANIC user space stopped feeding /dev/watchdog ... FX-9008
```

Every program whose next text page was not in the cache died of `SIGBUS`
on the fault, init's loop stalled, and the watchdog reset the box. The two
hangs before it were on images without the watchdog and went silent; their
lode stores end without the notice, since lode wrote to the same root.

**Three things went wrong, one after the other.**

1. *The kernel let an allocation fail with gigabytes of clean cache.* In
   every load run (`~/.local/share/ferrix/ef-load/runs/*/samples-world.txt.gz`)
   the box plays at its floor: `MemFree` 33-460 MB, `Cached` about 5.9 GB,
   `MemAvailable` about 5.8 GB, the processes' resident sets about 11.5 GB
   (Endfield about 5 GB of it). A program's frame is refused at the
   watermark (32 MiB on the box) and reclaims; the kernel's own go on below
   it, and nothing gave those back: no kernel-heap allocation reclaimed,
   and no reclaim ran to keep room under the watermark for the kernel. So
   at the floor the kernel lived on 32 MiB, and a burst of kernel heap -- a
   btrfs transaction holds up to `COMMIT_THRESHOLD` (32 MiB) of changed
   tree nodes, about twice that in heap, and a commit writes each through a
   16 KiB buffer -- ran out.
2. *The writer aborted.* Every allocation of `ferrix-btrfs-write` is
   fallible, and one failing inside an edit or a commit aborts the
   transaction (`fallible.rs`, "What a failure does to a transaction"):
   the edit cannot be undone half-way. The notice names the cause, out of
   memory, not the site; which allocation it was is not recorded.
3. *The abort poisoned the reads.* A writable mount reads everything,
   committed data included, through the volume; after an abort the next
   use reloads the last commit read-only (`WriteVolume::reload_read_only`),
   which reads the chunk tree, the roots and the free-space tree into
   memory again. It ran out at the same moment, while still holding the
   whole aborted transaction, and the mount made that final (`unreadable`
   in `rw.rs`): every later read answered `EIO`, for good.

A fourth hang, about 4 min into play on main-cfde84d79 (develop
589fcb096 and a diagnostic), went the same way: the notice, `SIGBUS` on
text page-ins, `EIO` to lode and auth, FX-9008
(`~/.local/share/ferrix/bw-repro/catch2/records.txt`, record 1; the notice
at line 550, and no memory or allocation line before it: the kernel logs
none).

**The fix (branch `bugfix/btrfs-abort-oom`; the certification
consultant's review: OK IF BO.1-BO.12, ledger line 7988).**

* `mm.rs`, L.mm.136 and L.mm.137: a heap allocation made where it may
  block (no spin lock, interrupts on, not inside a reclaim, not the
  failure injection's) reclaims clean cache, anywhere, when the heap
  refused it after the drain, and tries again while a reclaim gives some
  back, at most eight times (`heap_or_reclaim`); and when a kernel frame
  has left fewer than half the watermark free, each such allocation makes
  one reclaim of a run and the shortfall, one task at a time
  (`refill_headroom`), so
  that what cannot wait -- an insert under a spin lock -- still finds
  frames. That is Linux's direct reclaim and its reclaim at the low
  watermark, in two calls of the reclaim the page cache already has; the
  policy of what to take stays in `user::cache`. A reclaim that found
  nothing is not tried again for 10 ms.
* `ferrix-btrfs-write`, L.btrfs.122: `reload_read_only` lets go of the
  aborted transaction's nodes, delayed references and clean nodes before
  it reads the last commit, and can be called again after it failed.
* `ferrix-btrfs-vfs`, L.btrfs.123: a reload that ran out of memory is
  tried again at the next use, answering `EIO` until then and saying so
  once; only one that could not read the disk stays final. (Since W3,
  below: `ENOMEM` rather than `EIO` until then.)
A transaction can still abort when memory is truly gone -- nothing left to
reclaim -- and the mount then goes read-only at its last commit, as Linux's
does; reading what is committed keeps working once memory is back.

**The `commit` boot** of the gate (below) shows it: the cache filled to the
floor, then 24576 names of 200 bytes made by the shell alone, so that only
the kernel's heap takes memory and no program's allocation reclaims for it.

Local runs (nazuna, KVM, x86-64, debug kernel, 768 MiB; transcripts under
the btrfs-oom session's scratch, the gate's own runs named in the landing's
commit):

| Kernel | Result |
|---|---|
| develop (the fix's three files as on develop) | FX-0008 "memory allocation of 16384 bytes failed" 0.25 s into the burst, in `Shared::write_back_in_slices`'s collect on the periodic sync; the guest never says done |
| the refused-path reclaim alone (no refill) | FX-0008 "memory allocation of 192 bytes failed" in `Node::get`'s insert under the `nodes` spin lock |
| the fix | 24576 of 24576 names, 384 of 384 found again, the committed file whole, no abort; the boot 42 s |
| the fix, refill taken out | FX-0008 "memory allocation of 16384 bytes failed" |
| the fix, refused-path reclaim taken out | passes: the refill alone keeps the headroom here, so L.mm.136's check is the page cache's self-check (`user::rehome_cache_check`, `check_a_refused_heap_allocation_reclaims`) instead |

On develop the burst stopped the machine on an allocation that cannot fail
before any edit could abort; on the box, with its 32 MiB watermark, the
abort came first. Both are the kernel's heap empty at the floor with the
cache full.

Left over, as rows in `docs/BACKLOG.md`: a writable btrfs `getdents`
copies the whole directory from its cursor; `ferrix-btrfs-vfs`'s
infallible allocations under spin locks; the writeback's run buffer
halved instead of `ENOMEM`; and the box run of the fix.

## No clean cache left: the kill for the kernel's heap (2026-11-08)

`mem-pressure`'s half of the N150's out-of-memory hangs. L.mm.136-137
reclaim clean cache for the kernel's heap; with none left they give
nothing back, and the heap's refusal still aborts a btrfs transaction.

**What the box holds during play.** `mp-sample.sh` (the whole
`/proc/meminfo` every second, each process's `RssAnon`/`RssFile`/`RssShmem`
every ten; `~/.local/share/ferrix/ef-load/runs/mp-play/`) on
main-cfde84d79, Endfield in the world, uptime 1727 s, in MiB of 16036:

| | MiB |
|---|---:|
| `MemFree` (at the floor: the watermark is 32 MiB) | 253 |
| `Cached`, of it `Shmem` 252 | 5568 |
| `Slab`, the kernel's heap, of it `SReclaimable` (btrfs clean nodes) 35 | 362 |
| `RssAnon` of the 74 processes (Endfield 4141, the CEF browser processes about 1300, the Steam web helpers about 900, wineserver 28) | 6757 |
| Not in any of these: memory objects no process maps -- most likely the GPU's buffers (textures uploaded by DXVK through the Intel driver), which Linux would count in `Shmem` | 3096 |

`Dirty` stayed at 0, and `MemAvailable` at about 5.6 GB: the box plays at
its floor with gigabytes of clean cache, the case L.mm.136-137 answer. The
kernel's heap grows in bursts (a btrfs transaction holds up to 32 MiB of
changed nodes, about twice that in heap) against a 32 MiB headroom. The
early exits ~4 s after the embedded browser's fork (`es-dev-2`,
`fp-fix-1`) are not memory's: `MemFree` was never below 7.6 GB in those
runs, while every run that reached the world sat at the floor.

**What the kernel does at the floor, against Linux.** Ferrix has one mark,
the watermark (a 128th of memory, 1 to 32 MiB): a program's frame is
refused below it and the program's fault reclaims clean cache, then, after
16 reclaims with no room, kills the largest process (`object::oom`); the
kernel's own frames go on to the last one. Linux has three marks per zone:
kswapd wakes at `low` and reclaims in the background to `high`; an
allocation below `min` reclaims directly; one that still finds nothing
calls the OOM killer, except a `GFP_ATOMIC` one, which may dip under
`min`. Ferrix has no background reclaim and no active/inactive lists (its
reclaim is a clock with no accessed bits), no swap and no overcommit
heuristic (pages are taken at first touch, so a program is refused at
its fault, not its `mmap`). Dirty pages are written by the filesystem's
periodic commit, not by reclaim.

**The gap, shown.** The gate's `hog` boot: a `dd` holds all of
`MemAvailable` but 16 MiB as anonymous memory (its 644 MiB buffer, blocked
on a pipe nobody reads), then the shell makes 24576 names as the `commit`
boot does. KVM, x86-64, debug kernel, 768 MiB; logs in
`~/.local/share/ferrix/mem-pressure/`:

| Kernel | Result | Log |
|---|---|---|
| bugfix/btrfs-abort-oom's tip (L.mm.136-137) | name 4151 `Out of memory`, then `a transaction was aborted (out of memory)`; no kill | `hog-before-1.log` |
| with L.mm.138 (first form) | `oom pid 815 killed, 165185 pages resident`; 24576 of 24576 names, 384 of 384 found again, the committed file whole, no abort; 58 s | `hog-after-1.log` |
| L.mm.138's kill alone off (gate.sh `mp-ctl-nokill`, ef4880252) | FIRED: `a transaction was aborted (out of memory)` | `~/.local/share/ferrix/logs/queue/mp-ctl-nokill.log` |
| the first form, grace alone between kills (gate.sh `mp2-btp`, d45670ed5) | the hog killed, then the AHCI driver 0.8 s later (a false kill); the names all made | `~/.local/share/ferrix/logs/queue/mp2-btp.log` |
| the kill waits while its last victim's space is held | one kill (`6 frames free, the watermark 1487`), 24576 of 24576 names, no abort | `hog-dbg-4.log` |

**The fix (L.mm.138).** A heap allocation that may block, refused after
the drain and L.mm.136's reclaims, asks the machine-wide OOM kill
(`object::oom::for_kernel_heap`: the fault path's choice -- the most
resident pages, never pid 1 or a kernel task -- its 250 ms grace and its
wait for a victim still ending) and tries the heap again while it answers
retry and the free count stays under the watermark, at most 1024
answers, about a second (`mm::heap_or_kill`). A refusal with frames free
above the watermark is the heap's own (a block larger than it serves, a
run of an order fragmentation left none of) and kills nobody. Nor is a
second process killed for the heap while the last one's space is still
held: its memory comes back a while after the kill, and the first form,
with only the 250 ms grace, killed the AHCI driver 0.8 s after the hog
with 331 frames free of a 1487-frame watermark (gate.sh `mp2-btp`). The
kill's console line says the free count and the watermark. That wait has
no time bound: it fails closed (no kill, the refusal as before), the fault
path's kill is not affected, and a bound is a BACKLOG row (ledger 8043). The kill runs marked inside a reclaim: what it
allocates is never answered by a reclaim or a kill. A refusal of it is
served from its processor's allocation reserve (L.mm.139, outside any
section, while the reserve holds a block of its class), since the kill is
asked exactly when the heap is empty; only with that spent too does it
fail, and the kill then answers refused.
A running process the kill chose fails its allocation and leaves. A kill
from L.mm.137's refill, when its reclaim found nothing, was tried and
taken out (ledger 8015 B2): with it removed the hog boot still passed
(gate.sh `mp-ctl-norefill`, DID NOT FIRE), and it would kill after an
allocation that succeeded. This is Linux's `__alloc_pages_may_oom` for an
allocation that may not fail, with the victim choice Ferrix already had;
no new policy enters the kernel.

**What it leaves** (BACKLOG rows, ledger 8015 MP.8). The process making
the allocation may be the one chosen: its allocation fails so that it
leaves, and a btrfs edit it was in still aborts the volume; Linux lets an
OOM victim use reserves to finish (`ALLOC_OOM`). A victim blocked on a sleep lock the allocating task
holds (a btrfs volume's) cannot end; the answers run out after about a
second and the refusal stands as before, so the transaction aborts.
`/proc/meminfo` counts the GPU's buffers nowhere (the 3 GB above). No
background reclaim: every reclaim runs in the task that needs the frame,
which is where the frame rate dips at the floor are likeliest to come
from (not yet measured). The victim choice is the kernel's; a ring-3
policy (`oom_score_adj`, a userspace killer on pressure) is not built.
The FX-0008 report's trace goes to the port and screen only (KASLR), so
an event-log record does not name the infallible caller. Each of these is
a BACKLOG row (ledger 8015 MP.8, 8043 MK.2 and MK.5). The certification
consultant's review: OK IF MK.1-MK.6, ledger line 8043, after NOT YET
B1-B3 / OK IF MP.1-MP.10 at line 8015.

**Two refusals the kill could not answer (2026-11-08, btrfs-pressure).**
The hog boot stopped on `bugfix/fault-relanding` (90713ad5b, gate.sh
`fr-pressure`) with FX-0008, `memory allocation of 204 bytes failed`, in
`openat`, and no kill before it. With a diagnostic in `mm::heap_or_kill`
and `object::oom::for_kernel_heap` (logs
`~/.local/share/ferrix/mem-pressure/bp-diag-fr*.log`), the branch failed 3 of 8 hog boots, in two ways;
develop passed 6 of 6 the same afternoon (gate.sh `bp-dev-pressure` and
five local hog boots), and the difference is timing, not code:

* **An allocation under a spin lock.** `Dentry::insert_looked_up` copied
  the name (200 bytes and the number: 204) and made the dentry while
  holding the directory's children lock. A spin lock keeps its holder from
  blocking, so the refusal is answered by neither the reclaims nor the
  kill (`heap_may_reclaim`: preemption count 1), and the `Box` stops the
  machine. Which allocation first meets the empty heap decides it: one
  that may block kills the hog, one under the lock stops.
* **The kill could not list the living.** The kill is asked because the
  heap is empty, and the list of the live processes it chooses from
  (`process::live`, two small vectors) found it as empty: `live() failed,
  free 0`. The kill answered refused, and the allocation it was asked for
  failed; that one was fallible, so the shell's create said `Out of
  memory` and the volume aborted its transaction.

The fix of the first (bugfix/vfs-no-alloc-under-lock):

* the dentry cache makes what it keeps before its locks, fallibly
  (`ferrix_fallible::try_boxed_slice`), and the map's node under the lock
  inside the host's reserved section (`try_map_insert`):
  `insert_looked_up` and `fill` (whose failure unhashes the name, so the
  next walk asks the filesystem) answer `ENOMEM`; `move_child`, past the
  filesystem's rename and so unable to fail, makes its names before the
  locks;
* a walk reports an allocation it cannot make as `ENOMEM` (`walk.rs`: the
  path's copy, a link's fragment, the last component's name; a step
  borrows its component rather than copying it).

The second, by L.mm.139 (bugfix/heap-kill-reserve; consultant ledger
8172): the task choosing and killing for the heap
(`object::oom::HEAP_KILLER`, set under `CHOOSING`) has its refused
allocations, outside interrupt context, served from its processor's
allocation reserve outside any section (`killing_for_the_heap`,
`mm::reserve::draw`), 16 objects of every size class; L.mm.37 says so.
The list fits a class up to 85 processes (24-byte entries, the 2048-byte
class); with more it is a block of pages the reserve does not hold, and
the kill fails as before (a BACKLOG row). A draw leaves the reserve to be
refilled by the next section on that processor, which is refused
`ENOMEM` while the victim's memory has not come back (`reserve::draw`'s
documentation). The boot check is the allocation check's
`check_the_heap_killer_draws_outside_a_section`, with the heap made to
refuse the checking task (`mm::refuse_heap_for`); before it, 8 of 8 hog
boots on 90713ad5b with the change passed, one killing at `0 frames
free`.

The gate ends a boot at a stop's report (its `code` line) rather than wait
out its 1800 s for `pressure: done`.

## Against Linux's btrfs (2026-11-08, btrfs-reference)

The customer asked whether Ferrix's btrfs was checked against Linux's, so
as not to repeat mistakes Linux already fixed. It was not: the write path's
error and memory handling (`fallible.rs`, `WriteVolume::guarded`,
L.btrfs.122-123) was designed without reading `fs/btrfs`. This section
reads it now, on the failure paths that failed on 2026-11-08.

**The reference.** Linux v7.0, commit
`028ef9c96e96197026887c0f092424679298aae8` ("Linux 7.0", 2026-04-12; the
host runs 7.0.0), tag object `3131ff5a1174`: a sparse, blobless clone of
`fs/btrfs`, `include/linux/gfp*.h`, `include/linux/sched/mm.h`,
`include/linux/mempool.h` and `mm/{page_alloc,oom_kill,mempool,internal}`,
with history since 2019, in `~/.local/share/ferrix/btrfs-ref/linux`
(`README.txt` beside it). It is GPL-2.0: read for design and behaviour,
and no line of it is copied into Ferrix. Paths below are relative to it;
Ferrix's are to this tree at develop 29e3b47c2.

### The one difference behind most of the others

Linux's btrfs is written as if a small allocation does not fail, and in
Linux it nearly never does. Ferrix's is written so that every allocation
may fail, and treats each failure inside an edit like a disk error.

* `mm/page_alloc.c`, `__alloc_pages_slowpath`: an allocation of order 3 or
  less (`PAGE_ALLOC_COSTLY_ORDER`) that may block reclaims, retries
  (`should_reclaim_retry`, up to `MAX_RECLAIM_RETRIES`, 16, rounds without
  progress), then calls `__alloc_pages_may_oom`. For a `GFP_NOFS`
  allocation -- everything btrfs does inside a transaction --
  `mm/oom_kill.c`, `out_of_memory`, kills nobody ("The OOM killer does not
  compensate for IO-less reclaim") and answers true, so the allocator
  counts that as progress and goes round again. It waits for kswapd,
  writeback and the OOM kills other tasks' `GFP_KERNEL` allocations make,
  for as long as that takes. It fails only for a task that is itself an
  OOM victim and has used its reserves (`tsk_is_oom_victim`, `ALLOC_OOM`),
  for `__GFP_NORETRY`/`__GFP_RETRY_MAYFAIL`, and for orders above 3.
* `__GFP_NOFAIL` where btrfs has no way back: every extent buffer, the
  in-memory tree node (`fs/btrfs/extent_io.c`, `alloc_extent_buffer`: the
  `kmem_cache_zalloc` of the buffer, `alloc_eb_folio_array(eb, true)` for
  its pages, and `filemap_add_folio(..., GFP_NOFS | __GFP_NOFAIL)` into the
  btree inode). Reading or COWing a tree node therefore never fails for
  memory in Linux. History shows NOFAIL kept to that: 9f0eac070d23 (2022,
  `btrfs_io_context` without NOFAIL, "could loop indefinitely" and the
  callers handle errors) and 7dde7a8ab324 (2023, `set_extent_bit` drops
  NOFAIL for "unlock, sleep and retry").
* So `btrfs_abort_transaction(trans, -ENOMEM)` exists (most call sites
  abort on any error) but is reached by fault injection, memcg limits and OOM victims.
  8bb808c6ad91 (2022) stopped printing a stack trace for it, because the
  syzbot reports of it were "an artificial condition".

In Ferrix every allocation of `ferrix-btrfs-write` is fallible
(`src/lib/fs/btrfs/write/src/fallible.rs`), and `guarded_with`
(`write/src/volume.rs:259`) aborts the transaction on every error but
`Exists` and `NotFound`, the commit included (`write/src/commit.rs:91`). A
node an edit needs and cannot read into the clean cache fails the edit
(`volume.rs:433`, `load`). Since L.mm.136-138 the kernel's heap reclaims
and then kills before it refuses an allocation that may block
(`src/kernel/src/mm.rs:1555`, `heap_or_reclaim`; `mm.rs:1603`,
`heap_or_kill`), but both are bounded: eight reclaims, then at most 1024
answers of the kill, about a second, after which the refusal stands and
the volume aborts. Where Linux waits, Ferrix aborts after a second.

### Item by item

| # | Linux | Ferrix | Gap |
|---|---|---|---|
| L1 | **Abort.** `transaction.h`, `btrfs_abort_transaction`: the first abort sets `BTRFS_FS_STATE_TRANS_ABORTED` and logs the error with function and line; `transaction.c`, `__btrfs_abort_transaction`, stores the error in the handle and the transaction and calls `messages.c`, `__btrfs_handle_fs_error`, which sets `fs_info->fs_error` and forces `SB_RDONLY`. `transaction.c`, `cleanup_transaction` -> `disk-io.c`, `btrfs_cleanup_one_transaction`: delayed refs destroyed, dirty extent buffers cleared and marked stale (`btrfs_destroy_marked_extents`), pinned extents returned. Nothing is read from disk. | `volume.rs:224`, `abort_with` (first cause kept); `vfs/src/rw.rs:399`, `Shared::lock`, reloads the last commit read-only at the next use (`write/src/open.rs:185`, `reload_read_only`) and tells the notice. | Same outcome (read-only at the last commit); different cost. The notice says the cause, never the site: Linux logs `function:line` for every abort. |
| L2 | **Which errors abort.** Call sites abort after the tree was changed and return before; 828ec765f796 (2025) stopped aborting on an `alloc_bitmap` ENOMEM in the free-space tree's conversion ("just an optimization"); 8793ed87b376 (2023) stopped failing a tree-mod-log allocation that turns out not to be needed. Writes after an abort answer `EROFS` (`file.c`, `btrfs_do_write_iter`'s `BTRFS_FS_ERROR` check; `inode.c`, `btrfs_start_delalloc_roots`); metadata writeback is ended at once (`extent_io.c`, `BTRFS_FS_ERROR` before `btrfs_submit_bbio`). | `guarded_with` aborts on any error but two; `operation_with` (`volume.rs`) aborts once an edit succeeded. `check_writable` (`volume.rs:213`) refuses edits; `errno` (`rw.rs:1773`) maps `ReadOnly` to `EROFS`. | Ferrix cannot tell work an edit needs from work that only helps (a clean-cache insert, a buffer for a heuristic): both abort. Linux has been taking such sites out one by one since 2023. |
| L3 | **Reading after an abort.** No read path checks `BTRFS_FS_ERROR`. The roots in memory and the extent buffers they hold stay; file data is in the page cache, read through extent maps. Going read-only allocates nothing, and reads go on as before. 597441b3436a (2023): the abort's cleanup runs under NOFS, so reclaim does not re-enter the filesystem from it. | Reads of a writable mount go through the volume (`rw.rs:399`); after an abort they wait for `reload_read_only`, which reads the chunk tree, the roots and the free-space tree again. A reload short of memory is retried (L.btrfs.123), and until it works every answer is `EIO`: `Aborted` maps to `EIO` (`rw.rs:1773`), which a page-in turns into `SIGBUS` (`src/kernel/src/user/space.rs:2290`, only `ENOMEM` becomes `OutOfMemory`). | **The largest structural gap.** Ferrix needs memory to go read-only, at the moment memory is gone, and answers `EIO` (a fatal signal on a page-in) for want of memory. Linux needs none. |
| L4 | **Space before the transaction.** `block-rsv.c`, `space-info.c`, `btrfs_reserve_metadata_bytes` -> `reserve_bytes`: a handle reserves its worst case at `btrfs_start_transaction(root, num_items)`; a shortfall queues a ticket and flushes (`enum btrfs_flush_state`: delayed items, delayed refs, delalloc, chunk allocation, commit), `FLUSH_ALL_STEAL` may take the global reserve; the delayed-refs reserve is its own. | `volume.rs:326`, `admit`: an operation's nodes plus `commit_reserve` (`volume.rs:345`) plus `GLOBAL_RESERVE` (`volume.rs:67`), refused with `ENOSPC` before any change; data room taken at `write` (`rw.rs`, "Room is taken at the write"), one commit and a retry. | Close in kind, coarser (one commit, no flush ladder). Disk space only, in both. Low risk on the box (hundreds of GB free). |
| L5 | **Memory contexts.** `GFP_NOFS` on everything under a transaction, `memalloc_nofs_save` around code that calls generic allocators (`free-space-tree.c`, `file-item.c`, `acl.c`; 351cbf6e4410, d1825752e307): reclaim from inside a transaction must not re-enter the filesystem. Preallocate before the spin lock, fall back to `GFP_ATOMIC` under it, and on failure drop the lock, `cond_resched` and retry (`extent-io-tree.c`, `set_extent_bit`, `alloc_extent_state_atomic`, `search_again`). Mempools for I/O (`bio.c`, `btrfs_failed_bio_pool`; bios from the block layer's `bio_set`s). | Re-entry is answered by `drop_clean`'s `try_lock` (`rw.rs:344`) and the task's reclaiming mark (`mm.rs`, `heap_may_reclaim`): the equivalent of NOFS, and sound. Spin-locked infallible allocations remain (`rw.rs:793`, `Node::get`'s insert; `rw.rs:520`, the live-node collect; BACKLOG, "infallible allocations under spin locks"). No mempool: every disk read and write allocates a bounce buffer of its size (`src/kernel/src/fs/btrfs.rs:126` and `:157`), a writeback run up to 1 MiB contiguous (`rw.rs:1036`), a commit its node buffer and address list (`commit.rs:349`). | NOFS: no gap. Preallocation: known rows. **I/O with no reserve:** at an empty heap Ferrix can neither read a page in nor write a commit out. Linux's I/O path always makes progress. |
| L6 | **ENOMEM in write and commit.** Small allocations wait (above); the commit writes extent buffers already in the btree inode's page cache (`transaction.c`, `btrfs_write_and_wait_transaction`), so writing allocates only bios, from mempools. | Any ENOMEM in `commit` aborts (`commit.rs:91`); after the heap's bounded reclaims and kills. | Bounded retry then abort, against an unbounded wait. See W1 for why the answer is a reserve, not an unbounded wait. |
| L7 | **Dirty metadata under pressure.** Dirty extent buffers are pages of the btree inode, counted in `dirty_metadata_bytes`; past `BTRFS_DIRTY_METADATA_THRESH` (32 MiB, `fs.h`) `disk-io.c`, `__btrfs_btree_balance_dirty`, throttles the writer into writeback, and `extent_io.c`, `btree_writepages`, can write them before the commit; `ctree.c`, `should_cow_block`, COWs again any block with `BTRFS_HEADER_FLAG_WRITTEN`. A written buffer is clean page cache that `disk-io.c`, `btree_release_folio`, gives to reclaim once nothing references it. Clean buffers not referenced by a path are reclaimable at any time. 956a17d9d050 / ae1e766f623f (2024): the extent-map shrinker, made kswapd-only after direct-reclaim contention. | Dirty nodes are kernel heap (`WriteVolume::dirty`) until the commit, up to `COMMIT_THRESHOLD` 32 MiB of nodes (`rw.rs:195`), about twice that in heap, checked before a change of shape (`rw.rs:656`) and at a write (`rw.rs:1102`). Clean nodes are dropped only between operations and only by a reclaim that finds the lock free (`rw.rs:344`; F-99), so a task inside an operation cannot have its own volume's clean nodes back. | Ferrix pins up to ~64 MiB of unreclaimable heap per volume per transaction against a 32 MiB headroom, the mechanism of the 11-08 aborts. The threshold is Linux's number, but Linux's 32 MiB is a throttle on reclaimable page cache, not a cap on pinned heap. |
| L8 | **Readers keep working.** Readers do not take the transaction; tree locks are per node, commit roots let some searches skip them; a reader never waits for a commit to finish its writeback. | Every read of a page the cache does not hold takes the volume's sleep lock, which a commit holds for its last slice and the commit itself (`rw.rs`, "A writeback lets the lock go between slices"). A writer that allocates while holding it, and the kill it asks for, wait on a victim that may be waiting for the same lock (BACKLOG, MP.3 and ledger 8172). | Under pressure the reader's progress depends on the writer's. Linux's OOM reaper (`mm/oom_kill.c`, `oom_reap_task_mm`) empties a victim's anonymous memory even while it sleeps in the kernel; Ferrix's empties it only once the victim let go of everything else (`src/kernel/src/object/oom.rs`, module text). |

### Bug fixes in Linux's history that match what Ferrix hit

From `git log -- fs/btrfs` since 2019 (174 subjects mention ENOMEM, abort,
memory, NOFS, NOFAIL, OOM or read-only):

* **ENOMEM aborting the filesystem for optional work**: 828ec765f796,
  8793ed87b376, 162d053e15fe and a580fb2c3479 (no `BUG_ON` on ENOMEM).
  Ferrix's equivalent is L2: no classification yet.
* **The abort's own cleanup allocating into reclaim**: 597441b3436a. Ferrix's
  reload allocating at the floor is the same class, worse, since it reads
  the disk (L3).
* **Allocations under spin locks**: 7dde7a8ab324 and the extent-state
  preallocation; 905a95f3dd25, 8d54518b5e52 and 4addc1ffd67a (2023-2024)
  took `GFP_ATOMIC` out by preallocating. Ferrix's `Node::get` and
  `Dentry::insert_looked_up` (fixed on bugfix/vfs-no-alloc-under-lock) are
  this class.
* **Shrinkers in direct reclaim**: ae1e766f623f (spin-lock contention when
  every allocating task ran the extent-map shrinker; kswapd only since).
  Ferrix runs `drop_clean` and the page-cache clock in the allocating task
  (no background reclaim, BACKLOG mem-pressure row).
* **NOFAIL that loops for ever**: 9f0eac070d23. Linux keeps NOFAIL where no
  error path exists and takes it out elsewhere: the reason W1 below is a
  reserve, not a NOFAIL.

### What to do, in order of risk on the N150

The load: 16 GB, Endfield in the world at the floor (`MemFree` 33-460 MB,
32 MiB watermark), the kernel heap growing in bursts of a transaction's
dirty nodes. Risk is how likely the gap is to stop play there.

1. **W1, a memory reservation for an edit (high).** What Linux gets from
   NOFAIL tree nodes, Ferrix should get as `block_rsv` gets disk space:
   reserve before the first change, never fail inside. `admit` already
   computes an operation's worst case in nodes; take that many node
   buffers (and the delayed-refs and path entries they bring) from the
   heap into a per-volume pool before the edit, where ENOMEM aborts
   nothing (returned as `ENOMEM`, the transaction intact), and have
   `load`, COW and the delayed-reference inserts draw from the pool, so an
   edit's allocations cannot fail. The commit's own worst case is
   `commit_reserve`, reserved the same way before `settle`. This keeps
   the kernel slim (no new allocator policy, one pool in the filesystem)
   and keeps every wait bounded, which an unbounded NOFAIL would not.
   13 points (core item: requirement, host tests failing each reservation
   and each draw, control).
2. **W2, going read-only without memory (high).** Keep what reading the
   last commit needs beside the running transaction, as Linux keeps
   `root->commit_root`: each tree's committed root (bytenr, level,
   generation) and the committed chunk map, updated at every commit with
   allocations made before the superblock is written (as `prepare_unpin`
   is). An abort then drops the dirty nodes and switches to it with no
   allocation and no disk read; `reload_read_only` becomes the fallback
   for a log replay. 8 points.
3. **W3, ENOMEM is not EIO (high, small). Done (2026-11-08, branch
   `bugfix/btrfs-reload-enomem`; the certification consultant's design
   review: OK IF W3.1-W3.7, ledger line 8248).** While the reload is short
   of memory an operation's answer about itself (a lookup, a read, a
   page-in, a listing, a new edit) is `ENOMEM`; an answer about writes
   made before the abort (`fsync`, `sync`, a writeback, and so an edit
   that writes the file's dirty pages back first) stays `EIO`, since those
   writes are lost (`rw.rs`, `Shared::errno` and `durable_errno`). The
   reload is tried at most once per 250 ms per mount, no oftener than the
   machine-wide kill's grace, and `ENOMEM` comes at once in between
   (`docs/certification/MEMORY-AND-TIMING.md` §1.8 states the bound). That
   a page-in then gets no `SIGBUS` rests on the kernel's existing fault
   path as read, not on a check: `user/space.rs` turns a fill's `ENOMEM`
   into `OutOfMemory`, which `object::oom` answers with reclaim and the
   kill, and whose refusal after 1024 answers is `SIGSEGV`. The design as
   first written: While a reload is short of
   memory (`reload_short`) answer `ENOMEM`, not `EIO`: a page-in then goes
   to the OOM path (`space.rs:2290`) and is retried or kills, instead of
   `SIGBUS` killing whatever touched an uncached text page (lode, hyprix,
   auth on 2026-11-08). It amends L.btrfs.123's statement and criterion,
   which say the mount answers `EIO` until a later use reloads it. Worth
   landing before W2, which removes the case. 2 points.
4. **W4, a reserve for I/O buffers (medium-high).** A per-mount mempool:
   one bounce buffer for the largest device read and write
   (`btrfs.rs:126`, `:157`) and one node buffer for the commit, made at
   mount, used when the heap refuses, waited for when in use as
   `mempool_alloc` waits -- but with a stated bound: Linux's wait ends
   because the holder's disk I/O finishes, and Ferrix's disk is a ring-3
   driver that can stall or restart, so the wait ends in `ENOMEM` at a
   deadline (the I/O's timeout, about a second at most) and at once for a
   killed task; the bound is W4's design review's to fix (ledger 8229
   WD.4). Then a page-in and a commit can always reach the
   disk. Fold `write_runs`'s 1 MiB run into it (BACKLOG BO.5's row: halve
   to the reserve's size). The N150's FX-0008 of 131072 bytes at 11:17
   (BACKLOG, fault-relanding row) has no site yet; a 128 KiB bounce buffer
   is one candidate. 5 points.
5. **W5, dirty metadata sized by headroom (medium).** Until dirty nodes
   are reclaimable, commit when the dirty nodes' heap passes a share of
   the headroom left, not only at 32 MiB: at the floor that commits
   sooner and in smaller pieces, which is what Linux's dirty throttling
   achieves for it. A read of the heap's headroom from the kernel through
   the existing notice or a callback; no policy in the kernel. 3 points.
   Writing dirty nodes before the commit with Linux's WRITTEN-flag re-COW
   is the full answer and a new subsystem; not proposed now.
6. **W6, classify allocations as needed or helpful (medium).** Audit
   `ferrix-btrfs-write` for allocations whose failure need not abort (the
   clean cache's insert in `load` when the node can be used uncached, the
   commit's address list, lookups that only speed things up) and answer
   them without the abort, as 828ec765f796 does; record each abort's site
   (`#[track_caller]` location in `abort_with`) and put it in the notice,
   as Linux's `function:line`. 5 points.
7. **W7, reclaim clean nodes from inside an operation (medium-low).** Hold
   the nodes a walk's path uses (a count, as an extent buffer's reference
   does) so the clean nodes no path holds can be dropped by a reclaim made
   from inside the same volume's operation; today the allocating writer
   cannot have its own volume's clean nodes back (35 MiB `SReclaimable`
   during play). 5 points.
8. **Already rows, the same lesson:** the infallible inserts under spin
   locks (`rw.rs`), the victim's reserves (`ALLOC_OOM`, MP.3), the kill
   that cannot list the living (ledger 8172, `park/heap-kill-reserve`),
   background reclaim (kswapd), and reaping a victim that sleeps in the
   kernel, which belongs to the mem-pressure rows.

Left as they are: the space reservation (L4) is close enough; Ferrix's
answer to reclaim re-entering the filesystem (try-lock and the reclaiming
mark) is as good as NOFS for it.

## The gate

```
cargo xtask test-btrfs-pressure --accel kvm --init '<busybox>/{arch}/bin/busybox.static'
    [--memory MiB] [--cpu-quota PCT --pin CPUS]
```

A boot of the image with a static busybox as init and a fresh sparse
32 GiB `mkfs.btrfs` volume at `/data`, 768 MiB of memory unless `--memory`
says otherwise, at the machine's own floor (no cgroup).
`FERRIX_PRESSURE_BOOTS=cgroup` (or `all`) runs the same with the writers and
readers in a cgroup whose `memory.max` is half the machine; see "At
memory.max" for why that is not part of the gate yet.

Two readers hold a 1 GiB source open and stream it, as 7-Zip holds
the archive's volumes, and three writers write 6 GiB between them, each
member as Wine writes a file 7-Zip extracts: one byte at the member's size
(Wine's `SetEndOfFile` extend), `ftruncate` to the size, the bytes from the
start in 32 KiB writes, `ftruncate` again; Endfield's large members (200 MiB
to 1.2 GiB) between runs of small ones. Then `rm -rf` of it all and `sync`.
A probe sleeps 100 ms at a time through both phases and records how late it
woke. The gate fails on any failed step (with the step, its status, what it
said, `df` and `/proc/meminfo` at the time), and on a probe more than 5 s
late.

A second boot, `evict` (since 2026-10-18), fills the cache to the floor with
one 64 MiB file per 64 MiB of memory, written, synced and read back, then
makes 8192 one-page files in directories of 256 as fast as the shell can --
twice the dentry cache -- with the probe running and `Cached` sampled every
200 ms. It fails if `Cached` falls below half of what it was, or on a probe
more than 5 s late.

A third boot, `commit` (since 2026-11-08), fills the cache the same way,
then makes 24576 empty files with 200-byte names in one directory with
the shell's own `: >`, no process started, then syncs, reads back a file
committed before the fill and looks every 64th name up again (not a
listing: a btrfs `getdents` copies the directory from its cursor, see
`docs/BACKLOG.md`). It fails on a failed step, a name short, or a kernel
line saying a transaction was aborted. A fourth boot, `hog` (since 2026-11-08), holds all of `MemAvailable` but
16 MiB in a program's anonymous memory and then makes the same names, so
that the heap grows with no clean cache to reclaim; it fails as `commit`
does. `FERRIX_PRESSURE_BOOTS` is a comma list of `floor`, `evict`,
`commit`, `hog` and `cgroup`; the first four are the gate.

`--cpu-quota PCT` runs QEMU in a transient `systemd-run --user --scope`
limited to that share of a processor (no root), and `--pin` adds `taskset`:
a slow, starved machine. AllowedCPUs needs the cpuset controller delegated
to the user's manager, which this host's is not, so pinning is `taskset`'s.

One writer was not enough: it failed on main in one run of three. Three
writers and two readers fail it every time.

## Results (2026-10-17, nazuna, KVM, x86-64, debug kernel)

The cited evidence, through `gate.sh` (logs under
`~/.local/share/ferrix/logs/queue/`):

| Tag | Commit | Run | Result |
|---|---|---|---|
| `btp-pressure` | ce79aee91 | `test-btrfs-pressure --arch x86_64 --accel kvm --init <musl busybox.static>` | `run: PASSED`, KVM: 144 files, 7584 MiB written in 117 s, deleted in 1 s, longest probe stall 7 cs; the boot 138 s |
| `btp-ctl` | ce79aee91 | control: `run.saturating_add(short)` -> `{ let _ = short; run }` in `fs/pages.rs` | `control: FIRED (line, no panic)`: all three writers `No space left on device`, the first at 26 s |

The control is `--expect-line`, since the failure is a guest's line and
not a kernel panic, which `gate.sh`'s `--expect` requires.

History, not cited (bare xtask transcripts with no `gate.sh` header,
`~/.local/share/ferrix/n150-linux/pressure-gate/`): main f16c98ae7 failed
the same way in 59 s (`run14-main-control.log`); the starved form with the
fix (`--smp 2 --cpu-quota 40 --pin 22,23`) wrote 7584 MiB in 480 s and
deleted it in 6 s with the probe at most 42 cs late (`run13-A-starved.log`).
One host processor at a quarter (`--cpu-quota 25 --pin 23`, 4 vCPUs) does
not boot: stage 4's contention self-check needs two processors to run at
once (FX-0407), and `xtask` now refuses such a machine before QEMU starts.

## At memory.max

With the writers and readers in a cgroup at `memory.max` (half the
machine) main and the fix both fail, on defects of the memory.max path
that are not this fix's and not the box's (it has no `memory.max`):

* a busybox `truncate` or `dd` dies of `SIGSEGV` (status 139) at the limit,
  with `memory.events` `oom_kill 0`: a fault the limit refused ends the
  program with a signal rather than a reclaim or an OOM kill;
* `dd: out of memory`: its own buffer refused, while the job holds
  hundreds of megabytes of clean cache -- a kernel charge at the limit
  (an anonymous mapping's object) is refused without the reclaim that a
  fault (`oom::fault`) and a page-cache write or fill make first
  (H.QUOTA.12);
* with the fix, a writer that never moves again (5365 MiB on `/data` for
  seven minutes of heartbeats, the rest of the machine answering). On
  main the writers die of the floor's `ENOSPC` before they get there.

These are F-86, F-87 and F-88 (`docs/certification/FINDINGS.md`). The
script retries a step that met `ENOMEM` and says how often
(`pressure: note:`); the other two fail the boot. The cgroup form joins the
rows when they close.
