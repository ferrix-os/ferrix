# The native channel round trip

*Reviewed 2026-10-08, to `main` 4b9f04043.* The customer's target for the
round trip is seL4's matched figure, 440 ns, and since 2026-10-06 under 400.
This page is where its progress is kept: what is measured, the points so far,
and what is in flight. The design, step by step, is
`docs/OPAQUE-KERNEL.md` §9, and §9.11 is the round toward 440 ns; the way an
agent measures and optimizes it is `docs/HOTPATHS.md`. The status table's row
for it is in [Status](status.md).

## The figure

`bench-ipc`'s `domain-call` p50: the time of one `channel_write_read`
(0x1013) call to a server that is waiting and back, between a client and an
echo server in one speculation domain, with every mitigation on and
`ferrix.fastpath=on` (from step 4's point on). Matched to how seL4 was
measured on the same machine: KVM, one processor, QEMU pinned to one host
processor (11), nazuna's Ryzen 9 9900X (Zen 5), which has no PCID. The
samples are every one of a run, sorted, read through a fenced counter
(`3349682db`); before that the figure was a histogram's bucket floor and
moved in steps of about 233 ns.

## The chart

![The native channel round trip: domain-call p50 in ns on a log scale, one point per landing on main joined by a line, from 37,191 ns before step 1 to 873 ns with the FS/GS skip on 2026-10-07 (fast boot mode); dashed lines for seL4 matched at 440 ns, Redox 0.9.0 at 1,965 ns and a Linux pipe ping-pong at 2,105 ns](../img/ipc-round-trip.svg)

Drawn by `python3 tools/common/gen/gen-ipc-chart.py`, which holds the points
below; change them there and rerun. The points are one to a landing, not by
the clock. Redox's figure is its scheme round trip (§9.6a), without any
speculative defence; the Linux figure is a pipe ping-pong, native on the
host, 2,105 ns.

## The points

| Date | p50, ns | What changed | Commit |
|---|---|---|---|
| 2026-10-01 | 37,191 | `origin/main` before step 1: write, wait and read on each side, no speculation domain (§9.1: 37 us) | |
| 2026-10-01 17:32 | 6,508 | the branch `os-ipc/zircon-trip`: `channel_write_read` with the synchronous wake, outside a domain; hollow, a branch (§9.1: 6.5 us) | |
| 2026-10-01 18:22 | 3,021 | the same branch with the speculation domain, the call between two members of one; hollow, a branch | |
| 2026-10-02 22:48 | 3,021 | step 1 on `main`: the speculation domain; this is the old histogram's bucket (§9.9) | `3b7a935af` reads it, `d750fd452` retargets the plan |
| 2026-10-05 19:04 | 2,546 | `bench-ipc` made exact; the figure is `main` without 2f, from §9.9's run on `4a8b8dfee` (the commit's own text names only the old buckets, 2,556 among them) | `3349682db` |
| 2026-10-06 09:04 | 2,427 | 2f: no global or locked writes in the switch (2,526 to 2,546 on `main`, 0.957) | `445d09420` |
| 2026-10-06 10:45 | 2,287 | 3a and 3b: the vector-state contract for blocking calls, the FS base kept in the task (2,287 against 2,427, 0.942) | `a1d456820` |
| 2026-10-06 12:53 | 2,212 | step 5: ERAPS in place of the in-domain return-stack refill, −60 to −100 ns; derived, 2,287 less 75 | `3b9b1e267` |
| 2026-10-06 17:58 | 1,553 | step 4: the direct switch and the fast path (1,548 to 1,558 against 2,576 without it); the high boot mode | `0edd644b2` |
| 2026-10-07 11:37 | 1,248 | the same code read in the low boot mode, the other tree now booting with this run's kernel options (the message says about 1,240) | `4c9c078cc` |
| 2026-10-07 12:08 | 1,123 | the DS and ES skip (1,118 and 1,128 against 1,248, low mode) | `690754979` |
| 2026-10-07 12:50 | 1,048 | link-time hooks for the entry and T2 (its own run: 1,208 to 1,228 against 1,248 on the base before the skip; 1,048 is the low mode on the commit, from the next row's message) | `4066f41dd` |
| 2026-10-07 18:05 | 1,028 | the object side's cut 2, the lookup through the task's own core process; preliminary: low mode, one boot, 1,078 beside it on the base | `7b06cef25` |
| 2026-10-07 20:14 | 873 | FS and GS left unloaded 0 over 0 (3c), measured on its branch in a quiet window: 858 and 888 in the fast mode against 998; the slow mode 1,048 to 1,068 against 1,228 to 1,238 | `27d3e23c8` |

Where a figure here is not the one first written down for the point, it is the
commit's own: 2,287 for 3a and 3b (the 2,397 of §9.9 is the same tree before
its rebase onto 2f, against 2,546), and 1,028 for cut 2 (the first form of the
cut, which was not what landed, read 1,008 to 1,018). The 37,191 and 6,508 are the
logged figures behind §9.1's rounded 37 us and 6.5 us. The fsgs figure is from the session's log, not from a commit message.

## Landed since the chart's last point

Each of these has a figure of its own, taken on its own base and host load, so
none is a point on the chart: joined to the 873, the line would step with the
load, not with the code. The table gives each as its commit measured it.

| Landed | Change | Figure | Commit |
|---|---|---|---|
| 2026-10-07 23:47 | user-side inlining: `write_read` and `decode` inline, `Words::of` without a `memcpy` call, so the echo server's steady loop is one text page | alternated, fast path on, KVM, one processor: fast mode 988 to 1,018 ns (6 boots) against 1,048 to 1,088 (6), about -55 ns; slow mode 1,218 (2) against 1,238 to 1,298 (3) | `7fd6c7a17` |
| 2026-10-08 17:02 | the object side's cut 3: T2's `quiet` predicate and `seccomp::check` read `FILTERED_THREADS`, a live count of filtered threads, in place of a flag set once | low mode 958 to 968 ns (5 boots) against 988 to 1,008 (7), -40 to -50 ns, on a busy host (load 16 to 69), preliminary; to retake on a quiet host | `ee58d3912` |
| 2026-10-08 18:56 | Q4: each processor's GDT and RSP0 noted at the table loads, not read at every switch | 3 boots a side, below the five the protocol asks for, preliminary: 5,026 instructions a round trip against 5,044, cycles 4,543 against 4,624 (-1.8%), 809 against 822 ns | `dc5a3c495` |
| 2026-10-08 18:56 | Q8: `ferrix_switch` a naked function, called with `call rel32`, not through the GOT | 3 boots a side: instructions unchanged (5,044), cycles 4,593 against 4,624 (-0.7%, inside the spread), 816 against 822 ns; no figure claimed | `d3cbb0d55` |
| 2026-10-08 19:44 | ARMv7-A user state: `TPIDRURO` in the task, `TPIDRURW` switched (F-66), and 3a's VFP reset for a task blocked in a native call | none: nothing has been timed on ARMv7-A, and the DK1 measurement is owed | `1b50a9d49`, `df6ca167a` |
| 2026-10-08 18:56 | F-65's fix: the caller's endpoint let go with interrupts open (FX-0535) | costs about +40 to +50 instructions and +20 to +40 cycles a round trip; the median 819 to 858 ns on both sides, no p50 change resolved | `9ac307691` |

The DK1 itself can now be measured: user mode reads the virtual counter
(`b17462efe`) and `bench-ipc --board-log` files a board's own log
(`404d59fbf`). No such record exists yet.

## Two boot modes

`domain-call` p50 on `main` falls in one of two modes a boot, about 1,250 ns
and about 1,550 ns, on every tree since step 4 (§9.11). A ratio is taken within
a mode and quoted with its boots a mode. So the step from 1,553 to 1,248 in
the chart is one tree read in its other mode, not a saving, and the chart
marks the point from which the faster mode is read. seL4 shows the same
spread: 430 to 530 ns over six retakes on the same machine, and 440 ns is its
lower cluster, so the target is the faster mode's too.

## Target

seL4's matched figure is 440 ns; the customer's target since 2026-10-06 is
under 400. On `main` the faster mode read 873 ns in one quiet window after the FS and GS
skip, about twice seL4's, and the landings since (above) are measured on other
bases; there is no retake of the whole of `main` in a quiet window yet. The
budget for what is left is §9.10.

## In flight

Not on `main`; none is written up above as landed.

- The job-load fold, −10 to −25 ns by ablation.

Ids reserved on `main` and not written: L.sched.69-70 (the direct switch's
run-slot take and give-back, and the outgoing task's current reference kept on
the queue) and L.object.180 (a processor's `cpu.stat` charge kept back per run
queue, moved into the job's slots once).
