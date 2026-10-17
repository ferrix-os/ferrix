# btrfs under memory pressure (`test-btrfs-pressure`)

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

| Run | Kernel | Machine | Result | Time |
|---|---|---|---|---|
| negative control | main f16c98ae7 | 4 vCPUs, 768 MiB | all three writers `ENOSPC` ("No space left on device") within 36 s of the floor, 29 GiB free | 59 s |
| the fix | branch `btrfs-pressure` | 4 vCPUs, 768 MiB | 144 files, 7584 MiB written; deleted in 1 s; longest probe stall 7 cs | 2 min 22 s |
| starved, the fix | branch | 2 vCPUs, `--cpu-quota 40 --pin 22,23` | 7584 MiB written in 480 s; deleted in 6 s; longest stall 42 cs | 8 min 37 s |

One processor and a quarter of it (`--cpu-quota 25 --pin 23`, 4 vCPUs)
does not boot: stage 4's self-check needs two processors to overlap at
least once (FX-0407). `--smp 2` on two host processors at 40 % boots.

Transcripts: `~/.local/share/ferrix/n150-linux/pressure-gate/` on nazuna
(`run14-main-control.log`, `run15-fix-final.log`, `run13-A-starved.log` and
the serial logs beside them).

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

The script retries a step that met `ENOMEM` and says how often
(`pressure: note:`); the other two fail the boot.
