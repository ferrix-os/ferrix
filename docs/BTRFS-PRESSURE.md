# btrfs under memory pressure (`test-btrfs-pressure`)

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
  once; only one that could not read the disk stays final.
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
line saying a transaction was aborted. `FERRIX_PRESSURE_BOOTS` is a comma
list of `floor`, `evict`, `commit` and `cgroup`; the first three are the
gate.

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
