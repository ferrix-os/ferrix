# memkeep: free memory kept above a mark

A small native service, `/sbin/memkeep` (`memkeep.service`), that keeps
`MemFree` above a low mark by asking a slice to give back clean file cache
through `memory.reclaim` (`docs/CGROUPS.md` §10), one ask a second at most.
The kernel has the mechanism; the pace is ring 3's (customer, 2026-10-22:
keep the kernel slim). The consultant's verdict on where background reclaim
lives is ledger 8613: OK IF, a kernel correction of the kill plus this
ring-3 pacer (its conditions RP.1-RP.5); a kernel reclaim task NOT YET.

## 1. Why

On the N150 box (16 GB) the page cache fills memory while a game runs. The
kernel keeps only its watermark free (32 MiB there) and reclaims clean cache
on demand, in the task that needs the frame. The kernel heap's path then
makes a machine-wide OOM kill of the largest program while gigabytes of clean
cache remain: Endfield was killed about 15 s after it entered the world with
about 5.7 GB of clean cache left (event log, 2026-11-11 01:31), and the
watchdog reset the machine after it. Without a loop keeping memory free the
game died in about 5 of 13 launches that night; with a shell stand-in of
this service (`ef-keep-free.sh`) in none of 12
(`~/.local/share/ferrix/ef-30fps/REPORT-2026-11-11.md`).

## 2. What it is not

It is a pacer, not a fix. The kill made while clean cache remains is the
kernel's defect (F-109 reserved: L.mm.138 does not require its reclaims to
have found nothing; BACKLOG) and is fixed there. With memkeep the floor is
reached less often; when a burst outruns it -- a second's worth of
allocation larger than the gap between `MemFree` and the watermark -- the
kernel's own path runs exactly as without it. Nothing in the kernel rests on
it: no requirement, assumption of use or claim names it, and the kernel's
checks pass with it stopped (RP.1).

## 3. The policy

Read once at start from `/etc/ferrix/memkeep.conf` (`key = value`, `#`
comments). A key it does not take, or a value out of range, means the
defaults, said once.

```
low = 2050000      # KiB of MemFree below which it asks
chunk = 268435456  # bytes asked per ask, rounded down to whole pages
```

The defaults, for a key that is absent: `low` = `MemTotal` / 8, never under
32 MiB; `chunk` = `low` / 8 in whole pages, never under 1 MiB. On the box
(`MemTotal` about 16.4 GB) that is about 2 GB and 256 MiB, close to the
stand-in's validated 2.5 GB and 256 MiB; in a 4 GiB guest 512 MiB and
64 MiB.

Once a second it reads `MemFree` from `/proc/meminfo`. If it is under `low`
it takes, among the top-level slices init makes -- `system.slice`,
`user.slice`, `drivers.slice`, `init.scope` (`ferrix_svc::kind`; fixed, not
configurable) -- the one whose `memory.stat` `file` figure is largest *and*
at least twice `chunk`, and writes `chunk` to its `memory.reclaim`. One ask a
second at most; with no slice that qualifies it does nothing.

**No-progress back-off.** If an ask lifted `MemFree` by less than a quarter
of `chunk`, or the write was refused, that slice is not asked again for
10 s. The first box version of the stand-in asked every slice in turn: asks
of slices with no file cache took 20 to 50 s each, emptied the dentry and
inode caches through the shrinker (§10 of CGROUPS.md: a reclaim that finds
too few pages asks the filesystem for the rest), and stalled the game to
19.5 fps with a minimum of 0.2. The "twice `chunk`" rule keeps those asks
from being made; the back-off keeps a slice whose cache is hot, or being
refilled as fast as it is taken, from being asked every second.

**Logging**, to standard output, which init hands to the log: one line at
start with the marks; then at most one line every 10 s summing the asks
made since the last (per slice, how many made no progress or were refused,
`MemFree` before the first and after the last). No line while it does
nothing.

It allocates nothing after start: no heap at all, fixed buffers of 4 KiB for
`/proc/meminfo` (twelve lines) and `memory.stat` (nine), a few hundred bytes
each.

## 4. Authority

It runs as `memscrub` does: a root native service (`Type=native`, no
`User=`) in its own job under `drivers.slice`, which the kernel makes sealed
(CGROUPS.md §3.3), restarted 10 s after any end. That gives it root's file
access, so it could write any cgroup's `memory.reclaim` or limits, not just
the four slices'; no grant narrows it. This is broader than RP.2 asks. The
narrower grant -- init delegating the top-level slices' `memory.reclaim` to
a group and the service running under it -- is a BACKLOG row, not built.
Each ask is bounded by the kernel (`memory.reclaim`'s rounds of at most
32 pages, `EAGAIN` after sixteen that give nothing, `memory.min` spared);
every bound is the kernel's.

## 5. What it cannot do

* **It keeps no promise at the floor.** It is a program like any other: its
  stack and statics are anonymous memory, but where `/sbin` is on a disk its
  program's pages are clean file cache that reclaim can take at the floor,
  and nothing locks them in. While it waits on a page-in it asks nothing.
  RP.3's "locked, or measured" is not met by this landing.
* **It is a 1 s poll.** A kernel low-mark event instead would be a new
  interface into the item and its own design review (RP.4).
* **It cannot tell hot cache from cold.** The kernel's reclaim is a clock
  without accessed bits (BACKLOG, "No background reclaim"): each ask takes
  whatever pages the hand reaches next, a running game's included, and a
  refault costs a disk read.
* **Its marks were measured on one machine with one game** (the N150,
  Endfield; BACKLOG row). Frame time and refaults under the defaults are
  not measured (RP.4).
* An ask blocks the service for as long as the kernel reclaims; init's close
  of its channel is seen at the next second after that.

## 6. The gate: `cargo xtask test-memkeep`

`tools/common/xtask/src/memkeep.rs`. x86-64 under KVM, 768 MiB, init with
`memkeep.service` enabled and `low = 262144` (256 MiB), `chunk` 32 MiB; a
fresh home volume (`btrfs_disk::home_copy`) mounted writable at `/home`,
since a disk's clean cache is what `memory.reclaim` gives back and a file of
the initramfs or a tmpfs is not. The guest's unit, in `system.slice`, writes
a 1 GiB file there, syncs it and reads it back, then waits up to 40 s for
`MemFree` to reach `low`, two seconds more, and says what it is. The host
requires the start line with the gate's marks, at least one ask of a slice,
and `MemFree` at `low` within one chunk: the service stops asking at `low`
and asks again a second later, so the reading may be short by what the
guest took in between, which the next ask of one chunk makes good.

The negative control is the second boot of the same command: the same
image without the unit's link. No `memkeep:` line may appear and `MemFree`
must stay under `low`; a control over `low` fails the gate, since then the
first boot proves nothing. On 2026-11-11 (x86-64, KVM): with the service
`MemFree` 271484 kB after 11 s against a low mark of 262144 kB (from
6304 kB right after the read); without it 6640 kB after 42 s.

`gate_rows.rs`: a change under `src/user/system/native/memkeep/`, to
`memkeep.service` or to the gate owes `check` and `test-memkeep --arch
x86_64 --accel kvm`.

## 7. Where it stands (2026-11-11)

* Built on `feature/memkeep`: the service, its unit, enabled with
  `basic.target` wherever `memscrub` is (the desktop image's
  `init::desktop_files`), the gate and its control.
* Outside the certified item; no consultant review owed for the landing
  (ring-3 program and xtask, CONVENTIONS.md). Of ledger 8613's conditions:
  RP.1 met (nothing rests on it); RP.2 in part (started by init, one
  bounded ask at a time, every bound the kernel's; not narrowed: root, as
  memscrub); RP.3 in part (allocates nothing after start; its pages are not
  locked or measured at the floor); RP.4 in part (marks from the unit's
  configuration; the defaults come from the box's stand-in, without frame
  time or refaults measured); RP.5 in part (the VM gate shows free memory
  lifted from the floor to the low mark; the clean exit on init's close is
  not shown by a boot; this document stands for the section in
  BTRFS-PRESSURE.md).
* Owed: AArch64 and ARMv7-A runs of the gate (not tried); the box run with
  the stand-in `ef-keep-free.service` removed and the defaults measured
  (frame time, refaults, kills over a dozen launches); the narrower grant.
