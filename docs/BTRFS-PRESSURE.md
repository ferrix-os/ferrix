# btrfs under memory pressure (`test-btrfs-pressure`)

**Where it stands (2026-10-17).** The certification consultant's review of
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
