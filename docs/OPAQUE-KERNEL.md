# An opaque kernel: services as supervised userspace servers

**Shelved (customer, 2026-09-27): off the table for now.** S0 measured the
seam, and the customer read the numbers and set the plan aside. S1 and
everything after it are not started. The 2026-09-16 decision stands:
monolithic core, device drivers in ring 3. What would bring the plan back is
a cheaper trip to ring 3 (*The verdict of S0*, below).

**2026-09-30: the trip is being cut** (§ 8). The customer chose a direction
for Ferrix, Linux software beside safety functions on a kernel that can be
assured. Getting the btrfs parser out of ring 0 is one step of that, and it
needs the trip cheap first. The trace now says where a trip's time goes,
and the fixes are landing one at a time.

Drafted by ferrix-55b on 2026-09-27. The certification consultant reviewed
§2 and §5, and the init owner reviewed §3. Both decisions are recorded in
`docs/BACKLOG.md`, Decisions, 2026-09-27.

## The verdict of S0

Worth it for filesystems on paper, not yet for the network stack, and not
before the trip to ring 3 is made cheap. That trip is what today's Ferrix
already pays for every cold disk read.

**How often a build crosses to ring 3.** A warm `rustc` compile made 4,345
system calls and crossed once. A cold run crossed 2,986 times while the page
cache filled (§ *S0's first result*).

**What one crossing costs.** A 4 KiB read through the block ring and the
ring-3 driver took 300 to 844 us on x86-64 under KVM, over four back-to-back
pairs at host loads of 20 to 28. Stock Linux, on the same QEMU machine with
its driver in ring 0, took 27 to 48 us: 10 to 20 times less. On AArch64 under
TCG the figures were 453 us and 145 us. With 32 reads in flight the 99th
percentile reaches 150 to 230 ms (§ *S0's second result*).

**What that costs a whole workload.** No Ferrix with in-kernel drivers exists
(the 2026-09-13 decision forbids building one), so stock Linux stands in for
one. Each trip costs Ferrix 252 to 817 us more. Multiplied by the trips each
run made (x86-64, KVM; the `test-rustc` gate on 2026-09-27):

| Workload | Time on today's Ferrix | Trips to ring 3 | Lost to the seam vs in-kernel drivers | Option B would add |
| --- | --- | --- | --- | --- |
| Cold: `rustc -vV`, `cargo -V`, rustc and gcc compile and run | 4.35 s | 2,982 | 0.75 to 2.4 s (17 to 56%) | about 0.9 to 2.5 s more (a server trip per fill) |
| Warm: the second `rustc hello.rs` | 0.14 s | 1 | under 1 ms | under 1 ms |

These are estimates. The per-trip cost was measured on 4 KiB reads one at a
time, while the cold run's trips carry about 15 pages each, and some overlap.
A direct measure would sum the time every request waits in the ring during
the run.

**Why the cost looks like the implementation, not the design.** Kernels that
run drivers in user mode, such as seL4 and Fuchsia, pass a request between
processes in about a microsecond; Ferrix's trip costs hundreds. A median of 2
to 4 ms with a 99th percentile of 150 to 230 ms at depth 32 is the shape of
missed wake-ups rescued by timers, not of crossing into ring 3. Known costs
sit on the path:
- the block ring task's 50 ms recheck timer;
- no PCIDs, so every switch between the kernel's work and the driver
  flushes the TLB;
- the data copied between the kernel and the driver.

QEMU makes each doorbell, interrupt and wake-up an exit to the host, which
Linux's in-kernel path takes fewer of. Real hardware would narrow the gap,
but that is not measured.

**What would reopen the plan.** Cut the trip, then measure again:
1. Fix the depth-32 stall. Done 2026-09-27: the elevator starved reads
   behind its head for up to 500 ms; ring disks now use a 25 ms read
   expiry, and the p99 fell to 28–35 ms.
2. Find where the 250 to 800 us go; the driver's own `device_ticks` already
   splits off the device's share. Done 2026-09-30: the `seam-trip` and
   `seam-count` lines, and what they found in § 8.
3. Add PCIDs. Re-costed 2026-09-30 (§ 8): tagging saves microseconds per
   trip under KVM and nothing under TCG. Lazy TLB for kernel threads comes
   first, and wake placement matters more.
4. Remeasure with `cargo xtask bench-seam` and the `seam` boot line, with a
   target such as within twice Linux's per trip.
5. Add the direct measure of a cold `rustc` run.

That work pays off whatever is decided: every cold read on today's Ferrix
pays the trip. If the trip cannot be brought near the target, today's ring-3
disk driver deserves a harder look too. BACKLOG has a row for each: the stall,
and cutting the trip.

## 0. The decision this would change

On 2026-09-16 the customer reaffirmed `docs/ARCHITECTURE.md` §1: *a monolithic
core, capability seams, device drivers in ring 3* (`docs/BACKLOG.md`,
Decisions). The seam sits at devices because `rustc`'s calls are `open`, `stat`,
`read` and `mmap` on files the page cache holds, and those stay function calls.
"What the shape gives up is restarting a kernel subsystem, which the goal does
not need." The decision was to be closed by evidence: the two "seam measured"
rows in P2, which were measured on 2026-09-27 (*The verdict of S0*).

The door was kept open. C8 (2026-09-23) builds every cgroup over a `Job` "so a
future microkernel keeps working". init treats the kernel's subsystems as
`vfs.builtin`, `net.builtin`, `block.builtin`, which become `*.service`
aliases the day they are servers (INIT.md §7). L12, which ferrix-15 is building
now, makes init start devmgr, and is the first step of that on-ramp.

**What has changed since 09-16:**
- The customer now wants live kernel update and restartable parts of the
  system: the hot-patch plan (kept outside the repository, in `~/.local/share/ferrix/live-update/PLAN.md` on the build host), and
  T0, landed 2026-09-27. That is exactly the "restarting a subsystem" the
  decision gave up.
- T0 showed the cost of keeping services in ring 0. Every driver kind needed
  its own in-kernel park and take-up code (net, input, block), because the
  state that must survive a driver's death is kernel state.
- ~93,700 lines of uncertified code (the `load` ring: fs, net, block, the device
  cores, the Linux personality) run in ring 0. A bug there is a kernel
  compromise, and `ITEM.md` has to argue the load ring around the item.

### S0's first result (2026-09-27): the seam measured, 2

A warm `rustc` compile made 4,345 system calls and crossed to ring 3 once,
for a single page, while the page cache served 16,566 pages. A cold run
crossed about once per three calls while the cache filled (stage 11's
roadmap section has the table). For this plan that means the page cache, and
the VFS caches in front of it, carry the compiler. Option B must keep them in
the kernel, as it does. A filesystem server in B sees cold fills and
metadata writes, never the warm path. Row 1, the cost of one crossing, is
next.

### S0's second result (2026-09-27): the seam measured, 1

One crossing, a 4 KiB disk read through the ring and its ring-3 driver, took
300 to 844 us on x86-64 under KVM over four runs. Stock Linux, on the same
QEMU machine with its driver in ring 0, took 27 to 48 us: ten to twenty times
less. On AArch64 under TCG the figures were 453 us and 145 us. At depth 32 a stall reaches 150 to 230 ms at the 99th percentile
(a BACKLOG row). What that means for B:

- For files, B is sound as drawn. The page cache takes the warm path, and a
  server pays the crossing only on cold fills, as the drivers pay it today.
- For the network stack (S3), B's cost is different in kind. A socket's
  send and receive would cross to a server on every call, not once per
  cache fill. At today's hop cost, each is hundreds of microseconds. S3
  would need the hop itself cut first: batching, the stall fixed, and
  PCIDs so that a switch stops flushing the TLB. It should be re-measured
  before it is planned.
- The stall is a bug whatever is decided, and it costs the drivers already
  in ring 3 today.

### Where a trip's time goes: the `seam-trip` and `seam-count` lines

Since 2026-09-30 the boot check traces its 1,024 depth-1 reads
(`sched::trip`) and prints two lines under `seam`. `seam-trip` gives each
hop's p50/p99 in microseconds, in the order a read passes them: `issue`,
`queued` (before the ring task is nudged), `ring` (the ring task running),
`on-ring`, `bell` (the driver rung), `drv-bell` (its `port_wait` took the
bell), `irq` (the device's interrupt queued), `drv-irq`, `posted` (the driver
rang the ring's port), `ring-again`, `answered` (the bytes copied out),
`reader` (the reader running) and `done`. Then the whole trip's p50/p99 and
what the hops' medians add up to: near 100% means the hops account for the
trip. Then, for each of the five wakes, how many trips woke the task on
another processor than the waker's. Last, how often a hop was not taken: a
ring task or driver that was already awake is not woken, and that hop reads
as zero. `seam-count` divides what the run did by 1,024: switches, switch
barriers (IBPB), user roots written and taken off, IPIs sent, device
interrupts, and the sleeps on the reader's queue and the two ports, ended by
a wake or by the recheck timer. A trip spent in recheck sleeps, rather than
in wakes, is a missed wake-up. On AArch64 the driver now reads `CNTVCT_EL0`,
so the `seam` line's device share is measured there too.

## 1. What "opaque" means here

The kernel keeps what only a kernel can do:
- the scheduler, VM, objects and handles, channels, ports, VMOs, jobs;
- interrupts, the IOMMU, SMP;
- the Linux personality's process, signal, futex, time and memory calls;
- a forwarding layer.

Services move out as ordinary processes, supervised and restartable, and the
kernel stops knowing what kind of service a server is.

There are three shapes, from least to most change:

| | Stays in the kernel | Moves to servers | `rustc`'s cached `read` | Restartable |
|---|---|---|---|---|
| **A. Today + a generic rebind contract** | everything | nothing | function call | drivers only |
| **B. Servers behind the page cache** (recommended) | VFS, page cache, fd table, pipes, epoll, eventfd, procfs/sysfs/cgroupfs | the net stack, filesystem implementations (btrfs, later others), the device cores (input, display, render, audio), the terminal | **function call** (a cache hit never leaves the kernel) | net, fs, device services, drivers |
| **C. Fully opaque (starnix-like)** | the fd table and forwarding only | VFS, page cache, every filesystem, sockets, everything above | IPC round trip | everything but the kernel |

**Recommendation: B, staged, gated by measurement.** B keeps the 09-16
decision's reason (the compiler's hot path stays function calls) and drops its
cost (nothing but the page cache and VFS glue stays in ring 0). C inverts §2,
where Linux is the native ABI, into an emulation layer. It is worth doing only
if the seam measurements come out far better than a TLB-flushing switch
suggests, and it can be decided after B.

Why procfs, sysfs and cgroupfs stay in B: they are views of kernel state, and
`docs/SYSFS.md` already argues that a served `/sys` puts an IPC round trip and
a hung server under every `stat`.

## 2. Kernel mechanisms B needs

Each is useful on its own, and each is a landing.

1. **Measure the seam first (P2 "seam measured" rows 1 and 2).** Measure the
   ring-3 hop cost at queue depths 1 and 32, and the ratio of a syscall to a
   ring crossing, on all three arches and under KVM. Today's only figures are
   open+close ≈ 4.1 µs and a fault 848 ns. Without PCIDs/ASIDs (a P2 row), every
   switch flushes user TLB entries, so this row is also a candidate
   prerequisite. **This is the decision gate for everything after it.**
2. **A served inode.** A kernel `Inode`/`FileSystem` whose operations go to a
   server over the one proven pattern: a request ring in a VMO, a doorbell
   port, and the kernel as the client, as the block ring does. A page-cache
   miss fills from the server as it fills from a disk today (`fs/pages.rs`).
   The fd layer needs no change, because every fd is already a `dyn Inode`.
3. **The three seams that bypass `Inode`:**
   - `Inode::ioctl` (already a BACKLOG row) replaces the type ladder in
     `sys_ioctl`;
   - a socket trait replaces `sockets.rs`'s closed `enum Any`;
   - `mmap` of a served object takes a VMO the server hands over, which
     `memory.rs` can already map.
4. **Kernel-attested caller identity (a new item interface).** A forwarded
   request carries the caller's job, pid and credentials written by the kernel,
   never taken from the message body. Validated and unforgeable, with its own
   checks, like the handover record in the live-update plan. The servers' own
   protocols stay outside the item, as devmgr's do. (Certification's one new
   item interface.)
5. **Charging that follows the client (T.EXHAUST, F-35/F-37).** Memory a server
   holds for a client must be charged to the client's job. Otherwise one client
   exhausts a shared server, and F-37 reopens by another route. Two ways:
   client-supplied VMOs for request memory and buffers, and a kernel
   "charge-to-requester" on the attested identity of point 4. The second is
   **bounded**: a server charges only through a token tied to one outstanding
   forwarded request. The kernel issues the token with the attested identity,
   it is valid until the reply, and the charge is undone when the object it
   paid for goes. A server can never name a job to charge, or a compromised
   server could exhaust any victim by the other door. **This is in the design
   from the start, not retrofitted** (certification's main condition).
6. **One generic rebind contract.** T0's park/take-up, generalised: a service
   whose server dies has its kernel-side state parked (the page cache keeps its
   pages; the socket layer keeps sockets without a stack), its requests
   requeued, and the next server instance takes them up. That is T0's pattern
   with one implementation instead of one per kind, and it is what makes a
   server upgrade a restart rather than a hot patch. Parked state stays
   charged to the jobs of the clients it was held for, as F-37 charges kernel
   heap today, so parking never becomes an uncharged pool.
7. **Item reads through the fs become registered hooks.** Today the item reads
   through the fs in three places: `root_disk::process_context`,
   `fs::read_file_beneath` for firmware, and devmgr's image reader. The
   boundary gate refuses an item-to-load call unless it is a registered hook
   (the F-04/F-07/F-08 pattern).

## 3. Supervision and boot (agreed with ferrix-15)

- **L12 lands first and unchanged:** the kernel starts only init, and init
  starts devmgr. This plan adds successor landings to INIT.md §7.3 rather than
  changing L12. L12's parts stay: the opt-in `ferrix.devmgr=init`, init
  starting nothing but `devmgr.service` before ROOT, and pid 1 re-rooted once.
- **Servers are `Type=native` units** in a system slice, and `net.builtin`
  becomes `net.service` with `Alias=net.builtin` (INIT.md's own scheme).
  devmgr keeps the drivers.
- **Authority never passes through init.** A server that needs a device or a
  block ring gets it from the kernel or devmgr, as L12's starter-capability
  rule already requires for devmgr.
- **The fs server starts from initramfs memory** before any mount it depends
  on (DEVMGR.md §5's rule, applied to the fs). Init needs nothing from it to
  start: its binary and units come from the initramfs, `/run` is tmpfs, and
  cgroupfs and `/proc/cmdline` are kernel-made.
- **The root switch** stays the kernel's under L12. Moving btrfs out makes
  "switch" the fs server's job, and the kernel's re-root of pid 1 must be
  re-argued then (ferrix-15).

## 4. Stages

The points are rough, in the backlog's units, and each stage gates the next.

| Stage | What | Points | Gate |
|---|---|---|---|
| S0 | Measure the seam (P2 rows 1 and 2), and PCIDs/ASIDs if the numbers need them | 5 (+8) | **the customer decides B or not on these numbers** |
| S1 | In-kernel refactors, useful whatever S0 says: `Inode::ioctl`, a socket trait, the generic rebind contract, and item fs reads as hooks | 12 | the current row; T0's `test-restart --boot all` on the generic contract |
| S2 | Kernel-attested identity and client-following charging (item interface; certification review) | 10 | new item checks; F-37's charging tests rerun through a server |
| S3 | **The net stack as a server** (TCP/IP, netlink and packet sockets). The kernel keeps socket inodes that forward over a per-socket ring. Restartable: sockets park | 30 | `test-net --arch all`, Chrome's network bench, a kill-the-stack row |
| S4 | **The device cores as servers**: input, display, render, audio, and the terminal/pty. `/dev` nodes become served inodes | 25 | the compositor, audio, jobs and pty gates, kill rows |
| S5 | **btrfs as a server behind the page cache**, and the root switch through it | 35 | `test-btrfs`, `test-powerfail`, the self-host build time, a kill-the-fs row with `btrfs check` |
| (S6) | Option C: VFS and page cache out | not planned | only if S0 and S3 to S5's numbers argue for it |

That is about 117 points for B after S0, about as large as stages 17 to 19 were.

## 5. What it costs

- **Performance.**
  - Unknown until S0. Cached file I/O is unaffected in B.
  - Every socket operation and every page-cache miss gains a server hop. So
    do device-node ioctls, which matter for the compositor (a GPU submit is
    ≈ 0.95 ms today, dominated by the host).
  - Chrome makes ≈ 8,000 syscalls/s after the vDSO; its network path would be
    the one to watch.
- **Boot checks.** The stage 8 to 12 in-kernel checks that exercise fs, net and
  block in ring 0 are how the reference boot proves those paths today. They
  need server-side equivalents in the gates (certification). Several dozen
  checks move or get rewritten.
- **Certification.**
  - The item hardly shrinks, because these are `load` code already. The gain
    is that they leave ring 0, so isolation holds against them by
    construction.
  - New: the identity interface (item); an A./OE. pair naming the servers as
    environment (the OE.AUTH precedent), because file permissions
    (FDP_ACC) are enforced by a server; T.EXHAUST re-argued.
  - **A build switch, `--servers off|on`.** `off`, the in-kernel build, stays
    the certified reference configuration until the servers and forwarding
    have their own gates. The item's code is identical under both, so its
    evidence covers both, and ITEM.md §5 says so.
- **Effort and focus.** ≈ 117 points after S0, competing with the desktop,
  Steam and the certification work.

## 6. What it gives

- **Restart instead of hot patch.** The net stack, filesystems and device
  services restart and upgrade like drivers do after T0. The live-update plan's
  T1 (kexec handover) shrinks to the small kernel, and its state record
  shrinks with it: servers keep their own state across their restart.
- **Ring 0 shrinks** from ≈ 115,000 lines without checks toward the item plus
  the forwarding and page-cache glue. A bug in TCP or btrfs stops being a
  kernel compromise.
- **One mechanism.** One rebind contract, one forwarding path and one charging
  rule, instead of per-kind kernel code.

## 7. Decisions for the customer

**Answered 2026-09-27: the plan is shelved.** None of the four below is
taken up; they stand as they were asked, for whoever reopens the plan.

1. **Reopen the 09-16 decision for B, subject to S0's numbers?** If yes, S0 is
   the next landing, and nothing past S1 starts before its numbers are read
   together.
2. **Order after S0:** net stack first (S3, the most self-contained and easiest
   to restart), then device cores, then btrfs. Or btrfs first, because it has
   the largest ring-0 code and the most to gain from restart?
3. **S1 regardless?** The ioctl and socket seams and the generic rebind
   contract pay off even if B is never built.
4. **Option C** stays unplanned unless measurements argue for it.

## 8. Cutting the trip (2026-09-30, os-35)

**Why now.** The customer chose a direction for Ferrix: unmodified Linux
software running as a non-safety partition beside safety functions, on a
kernel item that can be assured. The certification consultant (os-9f)
proposed the route, with IEC 61508 SIL 2 as the first target; the claim's
exact wording and that standard are still the customer's to confirm
(`docs/certification/CLAIM.md`, marked PROPOSED). Its
freedom-from-interference argument is easier the less of the Linux layer
runs in ring 0, and the cheapest large piece to move out is btrfs's parser
for untrusted disk images (about 25,000 lines). A filesystem server pays
the trip on every cold fill, so the trip has to be cheap first. The
consultant's recommendation for SIL 2 is software compartments in ring 0
first, then this trip, then btrfs as a server. For SIL 3 it is either the
full opaque kernel, which this makes affordable, or a separation kernel
with a real Linux guest; that choice is the customer's.

**What the trace found.** Four read-only studies of the path, then the
`seam-trip` and `seam-count` lines (§ *Where a trip's time goes*). The
baseline on x86-64 under KVM at two processors, host load 31 to 42:

- A depth-1 trip is p50 230 us and p99 417 us. The hops' medians add up to
  98% of it, so the trace accounts for the trip.
- Per read: 13 switches, 2.4 user roots written and 2.4 taken off, 3.2
  IPIs, 0.2 switch barriers (IBPB) and one device interrupt.
- No sleep was ended by the recheck timer. The missed-wake-up theory of
  *The verdict of S0* is wrong at depth 1: the port packets persist and the
  want-bell handshake is correct. The recheck is 5 ms in practice, not
  50 ms, because `wait_until_deadline` sleeps in 5 ms slices.
- 60 to 85% of the wakes put the woken task on another processor than its
  waker's. `sched::wake` never moves a task, so each such wake is an IPI to
  a virtual processor that is probably halted, and on a loaded host each
  costs tens of microseconds.
- The largest single hop is the bell to the device's interrupt (the device
  and QEMU): 56 us at p50.

A trip makes four or five hops (reader, ring task, driver, ring task,
reader). `dispatch()` wakes every reader on every pass, so a reader wakes
about three times per read and twice for nothing. Every interrupt masks
and unmasks its MSI-X entry, two exits to QEMU. The data is copied four
times, the first byte by byte under the disk's lock.

**The plan, in order.** Points are guesses.

| Step | What | Points | State |
|---|---|---|---|
| 1 | Trace a trip: `sched::trip`, `seam-trip`, `seam-count`; AArch64's driver reads `CNTVCT_EL0` | 8 | **landed** 5cc5ed38 |
| 2 | The ring: wake a reader only when its answer is there, no nudge when the ring task is awake, one word-wise copy, the shared indices read whole (F-45), a driver that rewrites a posted completion checked | 3 (6 spent) | **landed** b7cab053 |
| 3 | Lazy TLB: a kernel thread keeps the last program's space loaded, where the processor has SMAP or PAN | 3–5 (7 spent) | gated; WIP until renumbered and re-gated |
| 4 | Take the ring task off the data path: the reader publishes, the driver's `port_queue` completes inline | 5–8 (7 spent) | written, WIP on `os-35/ipc-ring` (7c52861e) |
| 5 | Interrupts: no MSI-X mask per delivery, with a stated storm bound (L.object.41 rewritten) | 2 (2.5 spent) | **landed** 1dcc433f |
| 6 | A sync wake onto the waker's processor, within its affinity and quota | 3–5 (4 spent) | written, WIP |
| 7 | A bounded poll before the idle halt; targeted IPIs on the GIC | 2 (3 spent) | written, WIP |
| 8 | Read into page-cache frames, one kernel copy | 3–5 | not started |
| 9 | A direct hand-off call (`port_queue_wait`) | 8–13 | not started; high risk |
| 10 | PCIDs and ASIDs | ~12 | not started; small gain |
| 11 | DMA into the page cache | 13–21 | blocked: domains are untranslated |

Steps 2 to 7 are the ones expected to bring a trip near twice Linux's. Each
branch measures itself against step 1's lines, back to back on the same
host load, and goes to the certification consultant before it lands.

**What the consultant requires of these branches.**
- Traceability entries for new functions, a changed requirement for changed
  behaviour, carry-coverage after the last rebase, and a negative control
  shown firing for every new check.
- The ring: every value read from memory the driver can write is read once,
  validated, then used; inline completion is bounded per call and charged
  to the driver's job.
- Lazy TLB: tables are freed only once no processor has the space loaded,
  eagerly or lazily (FX-0009), and the switch barrier keys on the last
  program's space, so user A, a kernel thread, then user B still gets it.
  On a processor without SMAP or PAN (ARMv7-A, a Cortex-A72, an x86-64
  without SMAP) a stray kernel pointer would read the last program's
  memory where it used to fault, so those processors stay eager.
- The IBPB policy stays as it is. `--mitigations off` moved p50 by 0 to
  25 us, an upper bound for every defence together; IBPB fires on 0.2 to
  1 read in 1. Linux's conditional mode would take that to zero, but it
  would narrow SPECULATION.md §3's claim, and it is the customer's and the
  consultant's decision, not proposed.

**Where the branches stand (2026-10-01 wind-down).**

- **The measurement** (step 1), landed as 5cc5ed38 without the consultant's
  review. **After-the-fact review (os-ad, 2026-10-01): OK with conditions.**
  It adds no `unsafe`, no `cfg` or feature, no native call and no upward
  reference. Unarmed, a stamp point costs one relaxed load; armed, it takes no
  lock, allocates nothing and fills at most 12 slots, and its numbers reach
  only the console, so V-06 does not move. Owed, and accepted by os-35 for a
  commit of its own on top of ring part B: (C1) `sched/trip.rs`'s 20 functions
  are classed as check code, yet the hop check arms them on every default
  boot, so an L.* requirement that the instrumentation is inert outside the
  hop check, a check that `TRACING` is false after it and at the boot marker,
  and a negative control (no `disarm`) stopping the boot on the check's own
  message; (C2) `sched/trip.rs` was never measured
  (`docs/certification/TODO.md` §0.2 now lists it); (C3) the traceability
  text of `sched::trip::count` says it counts switches, which no `Count`
  does.
- **The ring, part A** (step 2), landed as b7cab053. The consultant's
  review asked that the ring task's 50 ms recheck stay as the liveness
  backstop, and it does. The landing's KVM boot counted 0.17 sleeps per
  read ended on the 5 ms wait slice rather than a wake. No nudge was lost
  (FX-1005 did not fire), so these are reads slower than 5 ms on a loaded
  host; watch the count. Against 5cc5ed38, back to back at nazuna loads of
  41 to 51:
  - KVM at four processors: 12.7 switches per read became 10.1; depth 32
    went from a mean of 10.3 ms to 2.1 ms; the depth-1 p99 went from 3.3 ms
    to 1.0 ms.
  - AArch64 under TCG: the depth-1 p50 went from 954 us to 314 us.
  - A reader now sleeps once per read, not about three times.
- **The ring, part B** (step 4), WIP 7c52861e on part A. The ring belongs
  to the disk: a caller dispatches its own request and rings the driver.
  The completion port has a server (`Port::new_served`, L.object.106), so
  the driver's own `port_queue` takes the completions and wakes the
  readers. A trip is reader, driver, reader.
  - What it holds to: at most 64 completions per call, work charged to
    the driver's thread, nothing allocated under the lock, and corruption
    handed to the task.
  - Both negative controls fired, and it passed the full row on 5cc5ed38.
  - Measured against 5cc5ed38:
    - KVM at four processors: 4.95 switches and 0 IPIs per read.
    - AArch64 under TCG: the p50 went from 954 us to 211 us.
  - Left: re-gate on current main, the consultant's review, land.
- **Lazy TLB** (step 3), reworked to the consultant's three conditions.
  - The kernel is lazy only where the processor refuses ring 0 a user page:
    SMAP on x86-64, PAN on AArch64. It stays eager on ARMv7-A and wherever
    the backstop is missing, and the `lazy` boot line names the mode.
  - FX-0010 stops a drop that cannot wait, in every build.
  - Gated green at 065e3cb3. User roots written per read went from about
    2.5 installs and 2.5 uninstalls to 0.2 and 0 on lazy processors. The
    p50 moved by no more than the host's noise.
  - QEMU's default AArch64 processor has no PAN, so AArch64 ran eager.
  - Left, on `os-35/ipc-lazytlb-on-ef206bb2`: renumbered past F-55; the
    generated evidence, the full row and the consultant's second look.
- **Interrupts** (step 5), landed as 1dcc433f.
  - An edge MSI-X vector is no longer masked per delivery. The bound is 64
    deliveries per acknowledgement, and L.object.41 is rewritten.
  - At the consultant's word, MEMORY-AND-TIMING.md §2.2 now states who pays
    for a storm: at most 64 handler runs per scheduling of the holder, each
    charged to the task the interrupt cut.
  - On x86-64 under KVM at two processors, eight alternating boots each:
    the depth-1 p50 went from 291 us to 206 us, and the driver's
    submit-to-drain from 204 us to 83 us (medians).
- **Sync wake and idle poll** (steps 6 and 7), WIP on `os-35/ipc-wake`.
  - When reader, ring task and driver meet on one processor, cross-processor
    wakes fall from 60–85% to 1–4%, and IPIs to about 0.1 per read. Until
    the ring's spurious `wake_all` is gone, other boots miss that.
  - The work found a lost wake on AArch64 with a GICv3; it has a BACKLOG
    row.
  - What is left is in `docs/BACKLOG.md`, *Branches that still hold
    unlanded work*.

Hazards the ring work found:
- The new `ferrix-driver` ring (`src/user/system/native/driver/src/block.rs`)
  still reads and writes the shared indices a byte at a time. That is
  F-45 on the driver's side, and it has a BACKLOG row.
- Only four commands fit in flight: four 128 KiB regions of a 512 KiB data
  VMO.
- With the copy moved to the reader, `reader>done` is now 11 to 15 us, a
  cache miss on freshly DMA'd data. `answered>reader` is 20 to 26 us, a
  same-processor wake, which step 6 addresses.

**Measure on hardware for Arm.** Under TCG, QEMU flushes its whole TLB on
the register writes this work saves, so AArch64 numbers from TCG say
nothing about steps 3 and 10. Use the Pixel 7 under KVM or the DK1.

**Still owed to the trace.** `bench-seam`'s Linux side prints a mean only,
not p50/p99 per read; the ftrace segments on Linux and the host-side
count of VM exits (`trace-cmd` on nazuna) are not written.

## 9. The channel round trip, and speculation domains (2026-10-01, os-c7)

**The speculation domain is on `main` since bf9efba95 (2026-10-01).** Its
design and its code were reviewed by the certification consultant (os-ad,
§9.3a and §9.3b). It landed ahead of its full gate on the customer's word;
evidence: `fleet/gate.sh` INDEX tags `osc7-sd4-c1` to `-c8` (the controls,
each `FIRED (panic)`), `osc7-sd4-check`, `-build`, `-boot-*`, `-shell`,
`-threads`, `-vfs-*` on 96f0d28c8, the same tree before its rebase, and
`osc7-land-check`, `-boot-kvm`, `-boot-tcg`, `-boot-a64` on bf9efba95 itself.
What of that was still running at the wind-down is a row in
`docs/BACKLOG.md`, *Verification audit*. The customer's decision is in
`docs/BACKLOG.md`, Decisions, 2026-10-01 (2c0e37214). The rest of the round
trip work (§9.1's figures, §9.4) is not on `main`: it is WIP on branch
`os-ipc/zircon-trip`.

### 9.1 Where the round trip stands

`cargo xtask bench-ipc`, on branch `os-ipc/zircon-trip` and not yet on
`main`, boots a native client and a native echo server
(`/sbin/ipc-bench`) and times 20,000 round trips of eight bytes. That is the
figure an IPC design is quoted by, and it has no device in it.

The reference points:
- Zircon's own report gives about 6 us for a cross-process `channel_call`
  on bare metal (`zircon/docs/benchmarks/microbenchmarks.md`, 2018).
- The SkyBridge and UnderBridge papers measured Zircon at 8,000 to 20,000
  cycles.
- seL4's direct-switch fast path is the one design under a microsecond.

Branch `os-ipc/zircon-trip`, x86-64 under KVM on nazuna, one processor, p50:

| | mitigations on | off |
|---|---|---|
| `origin/main`: write, wait and read on each side | 37 us | |
| branch, the same calls | 12 us | 5.1 us |
| branch, `channel_write_read` (0x1013) | 6.5 us | 2.6 us |

§9.4 lists what the branch changes. The gap between the two columns is the
switch barrier: `IBPB` and the return-stack refill at the two switches between
programs that a round trip makes, about 2 us each on this processor. No round
trip under a microsecond is possible while every switch between two programs
pays it. The decision changes that.

### 9.2 The speculation domain

**What a domain is.** A job created marked. The domain's identity is a
non-zero `u64` taken from a counter when the job is made, and it is never
reused; zero means "no domain". An unmarked job, the default, is no domain.
Neither is any job inside a marked one, because a child job is made unmarked.

**Marking.** `job_create` gains an options argument in its second register,
which today's callers already pass as zero.
- `JOB_SPECULATION_DOMAIN` asks for the mark, and any other bit is
  `INVALID_ARGS`.
- The call already needs MANAGE on the parent, so the mark needs nothing more.
- A job is marked only as it is made, when it has no process. Nothing else
  sets or clears the mark.
- The kernel writes an audit record of the marking, with the parent's and the
  child's ids. It is a new event, `DOMAIN`, in `docs/certification/AUDIT.md`
  §1.

**Membership is stricter than "in the job".** A process is in a domain only
if it was made in the marked job and has never left it.
- It was made by `process_create` into the job, or by a fork of such a
  process, whose child starts in its parent's job.
- `Process` keeps the domain it was born with.
- Any move between jobs sets that domain to zero for good, whichever job it
  moves to. A move is `Process::move_to`: a `cgroup.procs` write,
  `CLONE_INTO_CGROUP`, or a delegation.
- A process moved *into* a marked job does not join the domain.

So nothing joins a domain except by being started in it by a holder of the
job's MANAGE right, and nothing that leaves keeps it. The integrator who marks
a job decides what may be started in it. That is what the assumption of use
means by "places in one domain only programs that may read each other's
memory". MANAGE on a marked job is that authority, so handing it to another
program hands that authority over too, and the AoU says so.

**Where the switch reads it.** On the `AddressSpace`, which is what the barrier
is keyed on today (`speculation::entered_space(root)`). The space gets a
`domain: AtomicU64`, set from its process's domain when the space is made for
that process. It is set to zero for good in two cases:
- a process of another domain, or one that left its domain, comes to share
  the space (`CLONE_VM` without `CLONE_THREAD`);
- the owning process leaves its domain.

Threads share their process's space, so they share its domain.

**The decision is one rule on every architecture.** `entered_space` already
runs on all three, called from each architecture's `install_user_root`.
- Each processor keeps `LAST_DOMAIN` beside `LAST_ROOT`.
- The outgoing space's domain is read *as it leaves*, not as it came: by
  `AddressSpace::install` from the space it replaces, and by
  `AddressSpace::uninstall` on the way to a kernel thread. A process that left
  its domain while it ran is no longer in it at the switch that ends its turn.
- The barrier is skipped exactly when the root differs and
  `LAST_DOMAIN == domain != 0`. Otherwise it is issued as now.
- `forget_root` also clears `LAST_DOMAIN` wherever it clears a root, so a
  reused root never inherits a domain.

The compare is two loads and a branch, at the place the barrier is decided.
Arm's predictor invalidation (`entered_space` on AArch64 and ARMv7-A) follows
the same rule.

### 9.3 Requirements, checks and controls

The rows, as numbered (os-ad, against main and the unlanded branches):
- **H.TRAP.17:** a switch between the address spaces of two programs not
  in one speculation domain issues the predictor barrier. A switch between two
  address spaces of one domain does not.
- **L.object.113, with L.x86_64.126, L.aarch64.52 and L.armv7a.3 for each
  architecture's in-domain barrier:** `entered_space` skips the barrier only when
  the root differs, and the outgoing and incoming domains are the same and
  non-zero, the outgoing one read as it leaves.
- **L.object.114:** `job_create` marks a job only with `JOB_SPECULATION_DOMAIN` and
  only under the parent's MANAGE. It refuses any other option bit and writes
  the `DOMAIN` audit record. A child of a marked job is unmarked.
- **L.object.115:** a process is in a domain only if it was made in the marked job
  and never moved. A move sets its domain, and its space's, to zero.
- **L.object.116:** a member leaves for good, its space with it, on a move
  between jobs and on losing dumpability, and every processor whose last
  space was in the domain issues the barrier before the leave returns
  (§9.3a A1, §9.3b F1).

The check runs in stage 9, on every architecture, at two processors or more.
It counts `switch_barriers_on` around pinned switches:
1. Two processes of one marked job handed one processor back and forth: no new
   barrier.
2. One of them switched with a process of an unmarked job, and with a process
   of another marked job: one barrier each way.
3. A process moved out of the marked job, switched with one left in it: one
   barrier each way.
4. `job_create` with the option but without MANAGE on the parent, and with an
   unknown option bit: refused, nothing marked, no audit record.

Each negative control must be shown firing and stopping the boot on the
check's own message:
- the domain compare answering always true, which fails case 2;
- the MANAGE test removed, which fails case 4;
- the move not clearing the domain, which fails case 3.

### 9.3a The consultant's amendments (os-ad, 2026-10-01, design review of 54509856e)

The verdict was **OK to build**, with these three amendments.

**(A1) A rise in privilege leaves the domain.** A member that ran a set-id
program would otherwise keep its domain, and its peers could then attack the
privileged program by branch-target injection. Linux's conditional `IBPB`
keys on exactly this. So a process leaves its domain for good, its own and
its space's, when it stops being dumpable, by any route:
- an `execve` that changes its effective or filesystem ids (`exec_dumpable`);
- a later change of those ids (`credentials_changed`);
- `prctl(PR_SET_DUMPABLE, 0)`.

Every one of these routes passes through `syscall::attributes::update`.
Dumpability and credentials are the personality's, so the core offers a
one-way `Process::leave_speculation_domain`. The personality calls it from
there whenever a process ends up not dumpable. That is a downward call.

Owed with it:
- **Check case 5:** a member that execs a set-id file, and one that clears
  dumpable, each switched with a member, issue one barrier each way.
- **Control 4:** `leave_speculation_domain` made a no-op.

**(A2) What the skip leaves out, and what it keeps.** The skip removes only the
*predictor invalidation*:
- x86-64: `IBPB`;
- AArch64: `SMCCC_ARCH_WORKAROUND_1`;
- ARMv7-A: `BPIALL`, or `ICIALLU` with `ACTLR.IBE`.

x86-64's 32-entry return-stack refill stays at every switch of address space,
in a domain or not. It is a few hundred cycles where `IBPB` costs about
2 us, and keeping it removes any question of whether a defence of the kernel
leans on it.

Why the invalidation can go without weakening the kernel, on every family in
the reference configuration: a program enters the kernel through its own
system calls, faults and interrupts with no switch of address space in
between. Anything it can train into a predictor against the kernel, it can
therefore use against the kernel from its own entries, where no switch
barrier has ever stood. The kernel is defended at entry instead, and that is
unchanged:
- x86-64: enhanced IBRS, AutoIBRS or IBRS-always-on, `STIBP`, and the entry
  hardening of SPECULATION.md §3;
- AArch64: the Spectre-BHB loop on every vector entry from EL0, and the
  `CSV2` that SPECULATION.md §4 relies on;
- ARMv7-A: SPECULATION.md §5.

A barrier at a switch separates the program that ran from the program that
runs next, and nothing else. SPECULATION.md §3's table changes in one row,
*Spectre v2, program → program*, whose `IBPB` becomes "when the processor
switches to another program's address space outside its speculation domain".
`STIBP`, the refill and every program → kernel row are unchanged, and the row
says so.

**(A3) Two more check cases.**
- **Case 6:** two processes in different domains sharing one space
  (`CLONE_VM` without `CLONE_THREAD`). The space's domain is zero, so its
  switch with a member issues the barrier.
- **Case 7:** a job created marked inside a marked job is a domain of its own,
  not its parent's. A switch between the two jobs' processes issues the
  barrier.

Items 3 and 4 of §9.4: the arguments are accepted, and the checks and
controls listed there are owed with those landings. For 4, that includes a
failing 0x1013 leaving registers 2 to 4 exactly as sent.

When this lands, §9 carries this review as its "where it stands" verdict.
The code goes back to the consultant with the check's logs and each of the
four controls' logs, each showing its marker and the check's own
`FERRIX-PANIC`.

### 9.3b The consultant's code review (os-ad, 2026-10-01) and what changed

The verdict on 5658e7c0c and 0ce13778f (rebased as 8527c9d12) was **changes
required**. What each finding changed:

- **F1, blocker: leaving must take effect at once.** A member that rose in
  privilege kept its domain's predictor state until its next switch, because
  an `execve` reuses the space in place and its other threads run on. Now
  `Process::leave_speculation_domain` calls `arch::leaving_domain`, which asks
  every processor whose last space was in the domain for the barrier. This
  processor serves the request at once. Every other one serves it at the IPI
  of the grace period the leave then waits for (`smp::synchronize`, and
  `arch::serve_wanted_barrier` in `on_ipi`). A fork's child, which has never
  run, leaves without the wait. **Case 8** checks both halves: a task parked
  in the leaver's space on another processor, and this processor, each
  decide the barrier before the leave returns.
- **F2: "left" is a state, not zero.** `Process`'s domain becomes `LEFT`,
  which a birth by `move_new_to` respects. A child that `inherit` made not
  dumpable in the root job therefore stays out when `process_create` moves
  it. **Case 9.**
- **F3: the check covers what the rows claim.**
  - **Case 10** checks: a child job of a marked job; a member's fork; a
    process moved into a marked job and its fork; and a forgotten root,
    after which the next member installed decides the barrier.
  - Case 1 now also counts invalidations issued, which must be none, and
    x86-64's in-domain refills, which must be one per switch (`REFILL_IN_DOMAIN`).
  - Case 4 reads the `DOMAIN` record back: the job, its parent and its domain.
  - *Read as it leaves* is case 8's.
  - The set-id `execve` is not driven by the check. The decision point is
    `syscall::attributes::update`, which is how N4 argued it, because every
    route to "not dumpable" goes through it: `exec_dumpable` writes
    `dumpable` there for a set-id `execve`, as do `credentials_changed` for
    a change of ids, `PR_SET_DUMPABLE`, and `inherit`. Case 5 drives two of
    them through `update` itself.
- **F4:** `AddressSpace::new` and `fork` build their spaces through one
  `assemble`, so `fork` is back under the complexity floor.
- **F5:** the Security Target's FDP_IFC.1/FDP_IFF.1 SFP names the in-domain
  exception. FMT_MSA.1 and FMT_MSA.3 cover the mark: set only by MANAGE on
  the parent at creation, membership only ever lost, unmarked by default. Both
  map to O.ISOLATE, and FAU_GEN.1 lists the `DOMAIN` event.
- **F6: ordering with os-35's lazy TLB.** `os-35/ipc-lazytlb-land` replaces
  `install` and `uninstall` with `switch_here`. Whichever of the two lands
  second moves `left_space` and `entering_space` across. "Read as it leaves"
  must then come from the space actually loaded on the processor, since a
  kernel thread will no longer uninstall it. os-35 (os-49) was not running
  when this was written; this paragraph is the note to it.
- **F7: the window of a move.** A move between jobs made by another process
  (a `cgroup.procs` write by the holder of MANAGE) is ordered with the moved
  process's own running only by that holder's action. Since F1, though, the
  move's leave makes every processor that last ran the domain issue the
  barrier before the write returns. The moved process can still run in the
  space between the write's start and its return, and so can the domain's
  other members: the window is the length of one grace period, and it is the
  MANAGE holder who opened it.
- **C1, on F1: a leave waits for a grace period, so it may not run under a
  spin lock or with interrupts masked.** Where each caller is when it
  reaches `attributes::update` or a move:
  - `exec_dumpable` runs in `execve` after the new image is populated,
    holding no spin lock, with interrupts on.
  - `credentials_changed` runs in the setuid family after `with_credentials`
    has let its lock go.
  - `PR_SET_DUMPABLE` calls `update` straight from `prctl`.
  - A move by `cgroup.procs` (`write_to`, `Job::adopt`) runs on a file
    write's path, which holds only sleeping locks.
  - `inherit` (fork, `process_create`) and `setrlimit`'s `update` never
    wait, because the process they reach is not a member. A fork's child
    whose parent is not dumpable has already left. A child of
    `process_create` is in the root job's no-domain. And a member that
    is still dumpable does not leave.

  The rule is checked as well as argued. The waiting branch of
  `leave_speculation_domain` stops the machine with FX-0907 unless
  `sched::may_block()` holds, that is unless the preemption count (FX-0503's)
  is zero and interrupts are on. A control calls a leave from inside a
  `SpinLock` in the check.
- **F8:** `Job::new_child_in` is `bare_child` with a domain argument.
- **F9:** MEMORY-AND-TIMING §2.2c gives the commit measured and the
  benchmark's resolution: its percentiles are an eighth of a power of two,
  so a p50 can read below the minimum.

### 9.3c The leave's local check (F-60, 2026-10-02)

The consultant's review of the step 2 and 3 design found F1's leave open to
the store-buffer pattern. `leave_domain` stores `OUT` with a Release store,
and the scan then loads each processor's `LAST_DOMAIN`, with nothing ordering
the store before the loads. A processor that read the space's domain before
the store could therefore record the domain after the scan had passed it, and
then switch P → M2 → P with no barrier: open on x86-64 and ARMv7-A, and in
the Rust model, and closed on AArch64. The accepted fix is a local check:
- **Publish.** `leaving_domain` puts the domain in a free slot of `LEAVING`,
  a set of eight (condition a), with a SeqCst compare-exchange before the scan
  and before `smp::synchronize`'s SeqCst increment and its interrupt
  (condition b). The leave then waits for the grace period inside
  `leaving_domain` and gives the slot back after it. A ninth leave yields
  until a slot is free.
- **Answer.** `smp::answer_grace_periods`, which both the interrupt handler
  and a waiting processor run with interrupts masked, reads the generation
  first, then serves `BARRIER_WANTED`, then runs `answer_leaving`. That
  compares this processor's `LAST_DOMAIN` with every slot and, on a match,
  issues the barrier and clears it (condition b). Reading the generation
  first also closes a smaller gap the old order had: the handler served
  `BARRIER_WANTED` before it read the generation it answered.
- **Case 11** (condition c). The check parks a kernel task on another
  processor and makes that processor's last domain none. It arms a hook
  (`arch::CheckHook`, named for the check that armed it) that `leaving_domain`
  runs between its scan and its grace period. The hook has the task install
  and leave a member's space, so its processor records the domain the scan
  has just passed. The leave must still see that processor decide the
  barrier before it returns. The check disarms the hook, and a boot check
  before the marker stops the machine with FX-0908 if a hook is still armed.
- **Controls.** `answer_leaving` made a no-op stops the boot on case 11's
  own message. The case's disarm removed stops it on FX-0908.
- **L.object.116** names the scan, the local check and the ordering
  (condition d).

### 9.4 The rest of the branch, for its own review

These are reviewed separately, once rebased and gated, as the consultant asked.

1. **The timer** (`timer.rs`). A one-shot already armed no later than the
   deadline asked for is not rewritten, and `stop` leaves a one-shot to fire.
   The LAPIC's registers are emulated, so each write is an exit, about 7 us on
   nazuna.
2. **Wakes inside a call.** `trap::system_call` marks the task as inside a
   call. A wake the call makes then leaves its decision to the call's way out
   (`sched::call_left`) rather than to a 20 us timer.
3. **The wake and the wait.** os-35's `Wake::Sync` is used for channel writes,
   and defers the decision to the waker's block. The wait queue keeps its
   buffer, and `wait_trusting` is new.
   - Why `wait_trusting` needs no recheck: every condition its one caller
     waits for, a message or the peer's close, wakes the queue in the same
     lock order as `wait_until_deadline`'s wakers do. A signal or a kill wakes
     the task itself (`syscall::process`, `sched::wake` then
     `sched::interrupt`). A task there was no memory to list rechecks, as F-23
     has it.
   - Its check, owed: a waiter woken by each of the three. The control makes
     the channel's close not wake the queue, which must leave the waiter
     blocked past a bound the check states.
4. **The new call.** `channel_write_read` (0x1013), with
   `trap::Outcome::ReturnWords`.
   - The words handed back are the message's bytes, then zeros. They come from
     an inbox slot, or from a queued message copied into a zeroed array
     (`Small::of`). A call that fails answers `Outcome::Return`, which leaves
     the argument registers as the caller set them. So no kernel value reaches
     the registers.
   - Its check, owed: a message shorter than three words comes back with the
     rest zero, and a failing call's registers come back as sent. The control
     fills the slot's tail with a pattern, which the check must catch.
5. **Segment state.** x86-64's switch skips segment, descriptor and base
   writes that equal what the same switch's save read. It made no measurable
   difference, and is a candidate to drop.
6. **The clock.** `now_nanos` uses two exact 64-bit divisions in place of one
   128-bit division.

With the domain, a round trip between two members of one domain is 2.8 to
3.0 us p50 with every mitigation on (`bench-ipc`'s `domain-call`, one
processor under KVM), against 2.6 us with mitigations off. What is left before
it is under a microsecond:
- PCIDs, so a switch does not flush the user half;
- a direct switch from caller to callee;
- the system call's own path, at 464 ns for a native call that does not sleep.

**Where the branch stands (wind-down, 2026-10-01, os-86, which was os-c7).**
`os-ipc/zircon-trip` (a1379b25b, on GitHub) is based on a `main` from before
the domain landed. Its domain commits (54509856e to 09afb2ddf) are an early
version and are superseded by `main`'s. The work still to land is its other
eight commits:
- 937b75972, `bench-ipc`;
- 5b300da82, item 1 and 2;
- 54ff92c2a, the sync wake;
- 169c39374, item 4;
- 82ee2fa44, item 6;
- dfedadeae, item 3;
- 15ae15cc3, item 5, to drop;
- a1379b25b, XSAVEOPT, which made no difference, to drop.

To resume, cherry-pick the six that stay onto `main`, then write the two owed
checks with their controls (items 3 and 4), gate, and send it to the
certification consultant. `os-ipc/prof` and `os-ipc/prof2` are timing builds
(spans printed at the shell's exit). They exist to find costs and must never
land. The per-span figures they gave are the ones in §9.1 and the list above.

**Step 1 (2026-10-02), branch `ipc-step1`.** The six commits are on it, on
`main` at 5e1341d35 and the reservation of L.object.128-132 and L.sched.5-8
(16605f863).
The two owed checks are `object::write_read_check`, stage 9's `wrread` line,
which makes every call through each architecture's own entry from a thread
of a check's process:
- item 4: a 5-byte message held in the slot and a 7-byte one queued come
  back as their bytes then zeros, and six failing calls leave registers 2
  to 4 as sent. The control fills `Small::of`'s tail with `0xA5`.
- item 3: a thread blocked in the call is woken by a message, its peer's
  close and its process's kill, each within 10 s. The control removes the
  close's wake from `Endpoint::drop`, and the waiter stays blocked past the
  bound.
- A signal ends no native wait (`must_leave`), so 0x1013's documentation no
  longer says it ends this one.

The rows are L.object.128-132. Five functions the commits pushed over the
complexity floor are split, and `native::answer`'s one new arm is recorded.
`ipc-bench` carries the `domain-call` run from 5658e7c0c, on `main`'s
`Job::create_speculation_domain`. Measured on that head, KVM, one
processor, back to back at a load of 7 to 13, p50 (mitigations on, then
off): `call` 6.5 us and 2.8 us, and `domain-call` 3.0 us both ways.

The consultant's review (2026-10-02) was OK IF, with four conditions,
each met on the branch:
1. The trusting wait reads the message and the close under the end's
   inbox lock. A kill and an `execve` are ordered against it by a `SeqCst`
   fence pair (`wait_trusting`'s documentation gives each waker's order).
2. The `sync` line: a Sync write from another processor moves a reader
   free to move in at least one of 8 rounds. A pinned reader is woken
   where it is pinned.
3. The timer keeps the clock read after the arm, so a skip is never late
   (the `arm` line). The clock's conversion is ferrix-vdso's
   `counter_nanos`, host-tested against the 128-bit formula. The deferred
   decisions are bounded in MEMORY-AND-TIMING §2.2c. The rows are
   L.sched.5-8.
4. A kernel reader takes the slot's message in the inbox's spare buffer
   without allocating, and `ReadError::NoMemory` is gone.

### 9.5 Within 1.5 times seL4: the plan (2026-10-01, os-86)

The customer asked for a plan to bring §9.1's round trip within 1.5 times
seL4's. Nothing in it is built yet. Points are estimates, one point being 20 to
30 minutes of one session. A figure marked *guess* stands until step 0
measures it.

**Start here** (for a session pointed at this section).
- *Read first:* §9.1 to §9.4, then this section.
- *The code to start from is on GitHub*, branch `os-ipc/zircon-trip` at
  a1379b25b. It forks from a `main` older than the speculation domain, so do
  not merge or rebase it whole.
  - Make a branch from current `main`.
  - Cherry-pick these six commits in this order, the branch's own:
    1. 937b75972, `bench-ipc`;
    2. 5b300da82, the timer and the decision at a call's end;
    3. 54ff92c2a, the sync wake;
    4. 169c39374, `channel_write_read` (0x1013);
    5. 82ee2fa44, the clock;
    6. dfedadeae, the deferred decision and `wait_trusting`.
  - Leave out the rest of the branch:
    - 15ae15cc3, the segment skip;
    - 54509856e to 09afb2ddf, an early speculation domain that `main`'s
      supersedes;
    - a1379b25b, `XSAVEOPT`.
  - That is step 1.
- *The timing builds* are on GitHub too: `os-ipc/prof` (41bdef2ca) and
  `os-ipc/prof2` (4b5be585c), both on `os-ipc/zircon-trip`. They print each
  span's ns at the shell's exit. Step 0 refreshes `os-ipc/prof2` onto `main`.
  Neither ever lands.
- *Branches that meet this work:*
  - os-35's lazy TLB, backed up on GitHub as
    `backup/2026-10-01/os-35/ipc-lazytlb-land` (bdc227dbd). Step 3's PCIDs
    must be built with it (§9.3b, F6; FX-0009).
  - os-35's sync wake and idle poll, `os-35/ipc-wake` (dbd392808, §8).
- *How the work is run:*
  - Builds and gates run on nazuna. The gate pool is
    `~/.local/share/ferrix/fleet/gate.sh`, with verdicts in its INDEX.
  - Landings take the lock with `fleet/land.sh` and follow
    `docs/CONVENTIONS.md`.
  - Requirement ids are reserved before they are written.
  - Every landing here touches the item, so it goes to the certification
    consultant first. The customer names that seat.
- *The measurement*, once step 1 is in: `cargo xtask bench-ipc --release
  --accel kvm --smp 1`. Its `domain-call` line is the figure. Run it with and
  without `--mitigations off`, back to back on the same host load.

**The target, as a number that can be checked.**
- *What is compared.* §9.1's round trip: a client's `channel_write_read`
  answered by the echo server's own. The seL4 equivalent is `seL4_Call` plus
  `seL4_ReplyRecv` between two address spaces on one core at one priority.
  sel4bench reports those two as "IPC call" and "IPC reply", each one way, so
  a round trip is their sum.
- *seL4's published figure*, from sel4.systems/performance.html. On an i7-6700
  (Skylake, 3.4 GHz), in the default configuration without its Meltdown
  defence, a call is 741 cycles and a reply 598. That is 1,339 cycles a round
  trip, or 0.39 us.
  - Both run seL4's fast path: the message fits in registers, no capability
    moves, and the server is waiting.
  - In that setup the server's FPU is off, so no vector state is switched.
- *On nazuna* (Ryzen 9 9900X, Zen 5, up to 5.66 GHz), the same cycle count
  would be about 0.25 us, and 1.5 times that about 0.37 us (*guess*). Today's
  2.8 to 3.0 us inside a domain is eight times that.
- *How it is compared.*
  - Both kernels boot under the same QEMU and KVM, with the same `-cpu` model
    and one virtual processor pinned to one host core.
  - They run alternately: seL4, Ferrix, seL4, Ferrix.
  - The figure is the ratio of their p50s within one run. nazuna's load moves
    absolute figures from hour to hour; a ratio taken in one run holds.
  - Both are kept in TSC ticks, which count at a fixed rate rather than at the
    core's clock.
- *Configuration: the gate figure has the protections matched.*
  - **Ferrix**: every mitigation on, and the two programs in one speculation
    domain.
    - On Zen 5 that means AutoIBRS and `STIBP`, both set once at boot, and the
      return-stack refill at every switch of address space (§9.3a, A2).
    - Ferrix builds no page-table isolation, and Zen 5 needs none.
  - **seL4**, built for the same processor:
    - `KernelSkimWindow` off. This is its Meltdown defence, which Zen 5 does
      not need.
    - `KernelX86RSBOnContextSwitch` on, to match Ferrix's refill.
    - `KernelX86IBPBOnContextSwitch` off, as by default.
    - PCIDs on (`KernelSupportPCID`, also its default).
  - **Reported beside the gate figure**, not gated:
    - Ferrix with `--mitigations off`, against seL4's defaults.
    - Both kernels with `IBPB` at every switch. `IBPB` alone costs about 2 us
      a switch on this processor, so no design reaches the target between
      programs that are not in one domain. The report says so.

**Where the time goes today.** Read from the code on `os-ipc/zircon-trip` and
from the timing builds.
- *Locks.* A round trip takes about 25 spin locks on each side:
  - about eight of the run-queue lock;
  - about 14 `PreemptSpinLock`s: the handle table, the inboxes and
    observers, the task slots, the switch's `LEFT`, and the process's and
    thread's signal state;
  - three wait-queue locks.

  A `PreemptSpinLock`, taken and released, costs about six locked
  read-modify-writes and four interrupt saves and restores. They keep the
  preemption count and FX-0503's bookkeeping.
- *`sched::current()`* takes the run-queue lock and clones an `Arc<Task>`. A
  side calls it about eight times: from `native_call`, from `must_leave` three
  times in one wait, from the wait itself, and from `needs_attention`.
- *The way out of every call.* On every return to ring 3, `needs_attention`
  takes `current()`, two `Arc` clones, the process's `state` lock and the
  thread's `signals` lock. With it, `decode_syscall` matches twice and the call
  is dispatched indirectly through the Linux personality. A native call that
  does not sleep costs 464 ns.
- *The scheduler.*
  - The writer's wake files the reader in the EEVDF queue. That costs
    `effective_weight`'s 128-bit divisions at each job level, `now_nanos` and
    the quota atomics.
  - The writer's block then runs `choose_next`, which picks the same reader
    back out. That costs four `Arc` clones, a SeqCst read-modify-write of the
    global `IDLE` word on every switch, and `arm_timer`.
- *The switch's state.*
  - Two `rdmsr`s read the FS and GS bases at every save, though the kernel
    already knows both: a program has no `FSGSBASE` and can change them only
    by a call.
  - An `XSAVEOPT` and an `XRSTOR` of 832 bytes, because the program's
    `syscall` stub promises it every vector register back.
  - A `mov cr3` without PCID, which drops every user translation. QEMU's CPU
    model does not offer `pcid` at all.
- *What the timing builds measured*, per switch:

  | Span | Time |
  |---|---|
  | the scheduler's choice | 0.8 to 1.2 us |
  | the wake | 0.3 to 0.6 us |
  | user state | 0.37 us (vector 0.12, segment bases 0.14) |
  | the address space | 0.32 us |

  The stamps inflate every span and the spans overlap. So the figures rank the
  costs; they do not add up to the trip.

seL4's fast path has none of this. It is one function. The caller's message
stays in registers, one capability lookup finds the endpoint, and the checks
are a handful of compares. The processor goes straight from the caller to the
waiting server, and neither ever enters a run queue (Heiser and Elphinstone,
*L4 Microkernels: The Lessons from 20 Years of Research and Deployment*,
2016).

**The budget.** At 1.5 times seL4, one direction may take about 185 ns, or
about 1,000 cycles. Here is an allowance for each piece, to be checked against
step 0's profile:

| Piece | Allowance (*guess*) |
|---|---|
| `syscall`, `sysret`, and the frame pushed and popped | 40 ns |
| the fast path's checks and its one handle lookup | 25 ns |
| the words written into the waiting peer's frame | 5 ns |
| the direct switch: stack, current task, and one `rdtsc` of accounting | 25 ns |
| the FS base write and the vector scrub | 25 ns |
| `CR3` with a PCID, no flush | 20 ns |
| the return-stack refill | 40 ns |
| **total** | **180 ns** |

The refill and the hardware's own entry take almost half of it. The software
between them has about 100 ns.

**The steps.**

| Step | What | Points | Round trip after it (*guess*) |
|---|---|---|---|
| 0 | Measure the target | 10–16 | — |
| 1 | Land the written work (§9.4) | 5–8 | 2.8–3.0 us |
| 2 | A cheap common path | 20–33 | 1.3–1.8 us |
| 3 | A cheap switch | 17–24 | 0.9–1.3 us |
| 4 | The direct switch and the fast path | 23–37 | 0.3–0.4 us |
| 5 | Squeeze by the profile, and hold it | 5–11 | within 1.5 times seL4 |
| | **Total** | **80–129** | about 27 to 65 hours of one session |

0. **Measure the target** (10–16; runs beside step 1).
   - *seL4 on nazuna* (3–5). Build sel4bench for x86-64 in the two
     configurations above, and boot it with the gate's QEMU and `-cpu`.
     nazuna has `cmake`, `ninja`, `gcc` and `python3` but not `repo`, so either
     clone the manifest's repositories by hand or install `repo` for the user.
   - *A seL4 root task that times like `ipc-bench`* (2–3). It runs 20,000
     `Call` and `ReplyRecv` round trips, timed and counted exactly as
     `ipc-bench` does, so the two figures differ in nothing but the kernel.
   - *`bench-ipc` made exact* (3–5):
     - an `lfence` around the counter read, or `rdtscp`;
     - the p50 taken from sorted samples, since today's histogram reads up to
       an eighth low;
     - one processor by default, the virtual processor pinned;
     - `+pcid,+invpcid` in the CPU model, for both kernels;
     - `--against-sel4`, which alternates the two images and prints the ratio
       and its spread.
   - *The timing build, refreshed onto `main`* (2–3). The stamp's own cost is
     measured and subtracted. Ablation switches skip one piece at a time and
     measure the change, which gives each piece's cost without the stamps'
     distortion. This build never lands.
   - Done when the target is a number measured on nazuna, written here and in
     the BACKLOG row.
1. **Land the written work** (5–8). Cherry-pick §9.4's six commits onto
   `main`, write the two owed checks with their controls, gate, and send it
   to the certification consultant.
2. **A cheap common path** (20–33). Every program gains from these, Linux
   programs included, and none of them changes what a call does.
   - `current()` read from the processor's own record, with no run-queue lock
     and no `Arc` clone. It returns a borrow that lives while the task runs
     (3–5).
   - A lighter `PreemptSpinLock` (5–8). The preemption count and FX-0503's site
     words become plain fields of the processor's record, which need no locked
     operation and no masked interrupts. A lock and its release then cost one
     locked operation. FX-0503's checks stay.
   - The way out as one word of pending work per task (5–8):
     - its bits are a signal, a stop, a kill, a resched and a regroup;
     - it is read with interrupts masked, just before `sysret`;
     - `needs_attention`'s locks are taken only when a bit is set;
     - whoever posts the work sets the bit under the lock it already holds.
   - A native call decoded once, and dispatched without the Linux
     personality's table (1–2).
   - The channel's `ready()` and `must_leave` read from one atomic state word,
     instead of the inbox lock and `current()` (3–5).
   - No global or locked writes in the switch (3–5):
     - `IDLE` is written only when a processor goes idle or wakes from it;
     - `entered_space`'s three swaps become plain per-processor stores, since
       interrupts are already masked there;
     - `effective_weight` is kept per job and recomputed only when a weight
       changes.
3. **A cheap switch** (17–24).
   - *PCIDs on x86-64* (§8, step 10; 10–13):
     - an allocator per processor, with generations;
     - `CR3` written with the no-flush bit;
     - a PCID that is reused gets flushed;
     - a shootdown reaches every PCID a space holds, including one a processor
       keeps lazily (os-35's lazy TLB, FX-0009).

     Kernel pages are already global.
   - *A vector-state contract for native calls that block* (5–8; a decision
     for the customer, below):
     - `channel_write_read` and the native waits are declared to destroy the
       vector registers, as a function call does in the C ABI, and the
       runtime's stub tells the compiler so.
     - A task blocked in one of these calls has no vector state to save.
     - Before the task runs again, the kernel resets its vector registers to
       their initial state (`XRSTOR` of an empty header), keeping `MXCSR` and
       the x87 control word. Nothing of the other program reaches it.
     - A task preempted anywhere else is saved and restored as today.
   - *The FS and GS bases kept in the task* rather than read back with
     `rdmsr`, since only a call changes them. The switch's `LEFT` lock is
     replaced by per-processor fields (2–3).
4. **The direct switch and the fast path** (23–37). The design goes to the
   certification consultant before any code (2–3).
   - *The direct switch* (8–13).
     - When a call wakes the task it then blocks waiting for, on this
       processor, and nothing runnable here is more urgent, the processor
       goes straight to that task. It is not filed in the run queue and
       picked back out.
     - The run queue's policy is kept by three conditions: the woken task's
       affinity includes this processor, its job is within its quota, and
       the caller's slice has not ended.
     - Every channel call gains from this, including one that carries handles.
   - *The fast path* (8–13). It is one function, reached from the entry stub
     for 0x1013 before the general dispatch. It runs with interrupts masked
     from entry to `sysret` and takes one lock, the channel's.
     - It applies only when all of these hold:
       - the message is at most 24 bytes and carries no handles;
       - the handle names a channel with the write and read rights;
       - the peer is the one task blocked in 0x1013 reading that channel;
       - the peer's inbox is empty, so nothing queued is overtaken;
       - the direct switch's conditions hold;
       - neither task has a pending-work bit set.
     - Then the words go straight into the peer's saved frame as its return
       values, the caller is marked blocked reading, and the processor
       switches.
     - If any test fails, the general path runs, unchanged.
   - *The evidence* (5–8). The fast path is a second implementation of
     0x1013, so the argument is that its results equal the general path's.
     - A boot argument turns the fast path off, and the boot says which path
       it ran. The channel checks and `ipc-bench` run both ways, and every
       observable result must agree: return words, return codes, order, a
       peer's close, and a signal or a kill during the wait.
     - One negative control per condition: each condition's test replaced by
       "true", one at a time, must make a check fire.
     - The check covers every branch of the fast path, and the coverage is
       carried as for the rest of the item.

     seL4 proved its fast path equal to its slow path; this is the tested
     version of that argument. Whether that is enough for DAL C and EAL5+ is
     the consultant's call, before step 4 starts.
5. **Squeeze, and hold it** (5–11).
   - Work through whatever step 0's profile still shows (3–8): the layout of
     the task and channel records, `Arc` traffic on the fast path, the handle
     lookup.
   - Then add a perf row to the gate pool that runs `bench-ipc
     --against-sel4` and fails above 1.65 times, so the figure stays (2–3).

**Order and wall time.**
- Steps 0 and 1 run side by side.
- After step 1, step 2 splits across two sessions, and step 3's PCIDs take a
  third.
- Step 4 starts once steps 2 and 3 are in. Its conditions read the
  pending-work word, and its switch assumes the cheap one.

That critical path is about 50 to 80 points, 17 to 40 hours with three
sessions. Every landing touches the item, so each needs the certification
consultant, and at this writing that seat is empty.

**Where it can fail, and what then.**
- *seL4 on nazuna is far from 0.25 us.* The target moves with it, and step 0
  restates it.
- *After steps 2 and 3, the profile puts the cost somewhere other than the
  scheduler and the wake.* Step 4 is re-planned before it is built.
- *The consultant does not accept a tested second implementation.* Then the
  work stops after the direct switch, at about 0.5 to 0.8 us (*guess*): under
  the customer's microsecond, but two to three times seL4.
- *The refill and the hardware entry leave too little room.* A2 keeps the
  refill, and only the consultant can change that. The matched seL4 pays it
  too.

**Not in this plan.**
- Calls between processors. seL4's fast path is one core only, too.
- Linux programs. They gain from step 2 only.
- Messages that carry handles or are longer than 24 bytes. They gain from
  steps 2 and 3 and from the direct switch, but not from the fast path.
- AArch64 and ARMv7-A. They gain from step 2, and get ASIDs and a fast path of
  their own after x86-64. Those are measured on hardware, because under TCG
  the TLB costs say nothing (§8).

**Decisions for the customer.**
1. Which figure is the promise: protections matched inside a domain
   (recommended), or mitigations off.
2. The vector-state contract for native calls that block. It changes the
   native ABI for native programs only; the Linux ABI is untouched.
3. Whether a fast path, which is a second implementation inside the
   certified item, is acceptable at all, with the consultant's view. Without
   it the plan stops at the direct switch.
4. Who takes the certification consultant's seat.

The customer's answers (2026-10-02):
1. The promise is the matched figure: every mitigation on, both programs in
   one speculation domain, against seL4 with the same protections.
   `--mitigations off` is reported beside it and not gated.
2. Yes: native calls that block may destroy the vector registers, under the
   contract in step 3.
3. Yes to a fast path for 0x1013, if the consultant accepts its design
   before any code is written.
4. The session running this plan spawns its own consultant subagent. Its
   verdicts go in the consultant's ledger, as on 2026-10-02 for NVIDIA and
   btrfs.

### 9.6 Step 0: seL4 measured on nazuna (2026-10-02)

seL4 was built and timed under the gate's QEMU and KVM, with one virtual
processor pinned to host core 11. Each figure is a full round trip: one
`seL4_Call` and one `seL4_ReplyRecv` between two address spaces at one
priority, on the fast path, with one word in a register. A root task times
20,000 trips per sample run with `lfence; rdtsc; lfence`, and takes p50 from
the sorted samples. The guest TSC runs at 4,400 MHz.

| seL4 configuration | p50 round trip |
|---|---|
| matched: no skim window, return-stack refill, no `IBPB` | 1,936 ticks, 440 ns |
| seL4's defaults (skim window on) | 2,860 ticks, 650 ns |
| `IBPB` at every switch | 3,960 ticks, 900 ns |
| matched, fast path off (control) | 2,244 ticks, 510 ns |
| matched without the refill (extra) | 1,760–1,804 ticks, 400–410 ns |

These are quiet runs, with the SMT sibling of core 11 under 20% busy. A busy
sibling adds 15 to 20%, so only ratios taken back to back are compared.

**The target: as good as seL4 or better (the customer, 2026-10-02).** The
matched figure, 1,936 ticks (440 ns), is the target itself, not 1.5 times
it. The customer raised it from §9.5's 1.5 times once the measurement put
1.5 times at about 0.66 us. The *guess* of §9.5 had assumed seL4's
published cycle count, and the virtual machine adds to it. The ratio is
taken as §9.5 says: both kernels in one run, alternated, protections
matched, inside a domain.

What the new target changes:
- Step 5's perf row fails above 1.10 times seL4, not 1.65; the margin is
  for the spread of a busy host (a busy SMT sibling adds 15 to 20%).
- Ferrix's software outside the switch must come to about seL4's, some
  90 ns a direction (its 220 ns one way, less the 130 ns switch below),
  where §9.5's 1.5 times left about 200.
- The items below under *what seL4 does not do* are part of the plan now,
  as step 5's means to the target, not things for later.
- If the consultant refuses the fast path, the plan stops at the direct
  switch, as §9.5 says, and this target is out of reach.

What this changes in §9.5:
- **nazuna has no PCID.** CPUID leaf 1 ECX bit 17 is clear on the host, so
  KVM cannot offer it, and a seL4 built with `KernelSupportPCID` refuses to
  boot. Every seL4 figure above is without PCIDs. Step 3's PCIDs, and
  `+pcid,+invpcid` in step 0's CPU model, cannot be done under KVM here. The
  allowance of 20 ns for `CR3` in the budget becomes a full flush on both
  kernels. The PCIDs still belong to the plan for hardware that has them.
- **`IBPB` costs about 230 ns a switch under KVM here**, (3,960 − 1,936) / 2,
  not the 2 us that §9.1 gives. §9.1's account of the gap between its two
  columns needs a recheck once step 1 is in.
- **The return-stack refill costs about 20 ns a switch**, within the budget's
  40.
- The gate's CPU model clears `RDCL_NO` in `IA32_ARCH_CAPABILITIES`, so the
  seL4 runs add `+rdctl-no` (true for Zen 5) and `+rdtscp`.
- sel4bench's own one-way figures read a `cpuid` beside every sample, which
  exits the virtual machine. Their sum, 2,640 ticks, is larger than the
  round trip measured, so the root task's figure is the one to compare.
- Ferrix's `ipc-bench` still differs in its counter read (no `lfence`) and
  its p50 (a histogram floored to an eighth of a power of two). Those are the
  "made exact" part of step 0, still to do.

**The address-space switch without PCID.** sel4bench's one-way figures,
matched build: `seL4_Call` and `seL4_ReplyRecv` take 748 ticks each in one
address space and 1,320 across two. The difference, about 130 ns a
direction, is the cost of a switch of address space here. Of it, about 20 ns
is the refill. The rest is the `CR3` write and the user TLB refilled after it,
each miss a two-level walk under nested paging. The budget's 20 ns for "`CR3`
with a PCID" is about 110 ns on nazuna, for both kernels.

**What seL4 does not do** (step 5, after steps 1 to 4, which carry the
2 us; each is measured with the timing build's ablations first):
- *ERAPS in place of the refill* (about 40 ns a round trip). Zen 5 clears
  the return-address predictor on every `MOV CR3` (CPUID 0x8000_0021 EAX
  bit 24). nazuna has it, KVM reports it to guests and QEMU 10.2.1 names it
  `eraps`. With `+eraps` in the gate's model, the switch may skip the
  refill where the processor reports it. §9.3a's A2 keeps the refill at every
  switch, so this needs the consultant first.
- *Fewer user pages touched per trip* (part of the 110 ns): the stub and the
  loop on one code page, stack top, message and TLS on one data page; 2 MiB
  pages for native programs' text and data.
- *Global user pages for the runtime's shared read-only text*, mapped
  identically in every native process, so that it survives `CR3`. It needs
  the address reserved in every space and `INVLPG` everywhere to unmap, and
  it gives up ASLR for that text. It is an isolation change and goes to the
  consultant as a design first.
- *FSGSBASE* in the gate's model (10 to 20 ns): cheaper FS base writes, at
  the price of user-mode GS writes the entry path must then handle.
- *Not pursued:* PKU in place of separate spaces (its switch is a user
  instruction), segment-limited small spaces (long mode has no limits, and
  nazuna has no LMSLE), AMD's SVM ASIDs (they tag virtual machines, not
  processes).

To repeat: `~/.local/share/ferrix/sel4/` on nazuna (`fetch.sh`, `build.sh`,
`run.sh`, `series.sh`, `summarize.py`, the patch to sel4bench's `apps/ipc`,
and every run's log). The sources are sel4bench-manifest 80add415, with seL4
at c6ce4d2a.

### 9.6a Redox measured on nazuna (2026-10-03)

The customer asked how Ferrix compares with Redox, the other Rust OS whose
services run as user-space servers. Redox publishes no round-trip figure,
only its base system call (116 cycles), so it was measured here, the way
seL4 was: the gate's QEMU and KVM, the gate's CPU model with `+rdtscp` and
`+rdctl-no`, one virtual processor pinned to host core 11, and
`lfence; rdtsc; lfence` around each sample. The guest TSC runs at 4,400 MHz.
Each figure is the p50 of 5 repeats of 20,000 samples after 2,000 warm-up
iterations, the median over four boots, alternated with runs on the host.

The image is Redox OS 0.9.0, server variant, from static.redox-os.org
(`redox_server_x86_64_2026-10-02_541_harddrive.img.zst`, sha256 `2d57daaf…`),
with kernel 0.5.12 (`16282036`) as shipped. One benchmark program, `rbench`,
built for `x86_64-unknown-redox` against relibc, was added to the image with
the `redoxfs` host tool.

| What one sample is | Redox | Ferrix, `main` a305c35c3 | Linux, the host, native |
|---|---|---|---|
| An unknown system call answering `ENOSYS` | 29 ns (129 ticks, batched) | about 275 ns (`bench-ipc`'s floor) | 37 ns |
| A request and its answer between two processes | 1,965 ns: one `SYS_WRITE` to a user-space scheme daemon (2,025 ns by `SYS_CALL`) | 2,556 ns: `channel_write_read` in one domain | 2,105 ns: a pipe ping-pong |
| The same through the kernel alone | 1,475 ns: a pipe ping-pong | — | — |
| A 4 KiB read through the block driver | 80 us p50, 109 us p90: a raw read of the virtio-blk scheme | 300 to 844 us: the block ring (§0) | 59 us: `O_DIRECT` on the NVMe |
| A 4 KiB read of a file | 19 us cached, 147 us cold (redoxfs) | not measured | 135 ns cached |

**What the comparison is worth.**
- *Redox runs with no speculative-execution defence at all.* Its kernel was
  disassembled: no write to `SPEC_CTRL` or `PRED_CMD`, so no IBRS, IBPB or
  STIBP; no return-stack refill; no `VERW`; no retpolines. Ferrix's figure
  has every defence of §9.6's matched configuration. So Redox's lead in the
  round trip is partly the defences it does not have.
- *Its round trip is the general path.* A scheme request is queued, the
  daemon takes it with one call and answers with another, and the caller is
  woken through the scheduler. Redox has no direct switch and no register
  fast path. Steps 2 to 4 aim at 440 ns, about 4.5 times faster than Redox,
  with every defence on.
- *Block I/O is where Redox is ahead,* 4 to 10 times, and nothing in §9.5 to
  §9.8 changes that: Ferrix's 4 KiB read is dominated by the block ring's
  copies and its four commands in flight, not by the round trip. The BACKLOG
  row "Block I/O faster than Redox" takes Redox's 80 us as the figure to
  beat.
- *The disks are not identical.* Redox does not drive the modern-only
  virtio-blk device the gate uses (`disable-legacy=on`), so it ran on the
  transitional one. Both systems' disk reads reach QEMU and the host's page
  cache (QEMU's default writeback cache), not the NVMe media.
- *The host was shared.* Load was 9 to 26. A boot with core 11's SMT sibling
  44% busy ran about 30% slower; it is the top of each range in the logs.

To repeat: `~/.local/share/ferrix/redox-bench/` on nazuna (`rbench/` and its
`build.sh`, `run.py`, `linux.sh`, `series.sh`, `summarize.py`, and every
run's log with its command, load and sibling-busy figures in `.meta`).

### 9.7 Step 4's design: the direct switch and the fast path (draft for the consultant)

**Reviewed (2026-10-02): OK to build IF, with eleven conditions.** The
conditions, the consultant's answers to part 9, and what each changed are
at the end of this section, under *The consultant's design review*. Parts 1
to 8 below already carry the changes, and a new part 2a lists every effect
of the general call, as condition 1 asked.

Nothing here is built. This is the design §9.5 step 4 sends to the
certification consultant before any code, and the consultant's verdict
decides whether the target of §9.6 can be reached at all. It is written
against branch `ipc-step1` (read again at 5d7186d00, with its five
conditions met) and against steps 2 and 3 as §9.5 describes them; part 5
says what of those must land first. The customer's answers of 2026-10-02 (§9.5's end) stand: the
gated figure is the matched one, blocking native calls may destroy the
vector registers, and a fast path is wanted if the consultant accepts this
design.

**Why a fast path at all.** seL4's own fast path saves it only 35 ns a
direction on nazuna (§9.6: 510 ns a round trip without it, 440 ns with it),
because seL4's general path is already short. Ferrix's is not: after steps 2
and 3 the general 0x1013 is guessed at 0.45 to 0.65 us a direction, against
the 90 ns of software the target leaves (§9.6). The gap is the general
system call machinery itself: the dispatch, the wait queue, the wake's
placement, the run queue's pick, the way out. No amount of squeezing gets a
general path written for every call down to seL4's. So the fast path is the
only route to the target, and the question is how to make a second way
through 0x1013 that can be argued equal to the first.

**The principle: compose, do not copy.** The fast path is not a second
implementation of the call's logic. It calls the general path's own
functions wherever one exists: `channel_in` for the handle and its rights
(with its Spectre clamp), `AddressSpace::install` for the space and the
barrier, `switch_user_state`, `carry_in_call`, `note_running`, `arm_timer`,
the stub's own entry and exit. What it adds is three pieces, each small and
each with its own evidence:
1. `RunQueue::hand_over`, which replaces the pick when nothing else could be
   picked, host-tested equal to the general sequence (part 1);
2. the *park*, a one-task record on a channel half with a reply cell in the
   task, and the commit that fills it (parts 2 and 3);
3. the frame tail, which moves the reply into the four return registers
   (part 2).

Everything else the fast path does is a test, and a failed test runs the
general path from the call's first instruction. The one thing a declined
attempt leaves behind is a count: the decline counter of the test that
failed (part 6). It is a kernel statistic that no program can read.

#### Part 1: the direct switch

**When it applies.** All of these, decided under this processor's run-queue
lock:
- the task to switch to is *asleep at home* here: state 2 of `wake_with`'s
  list on `ipc-step1` (blocked, switched out, detached from the fair class,
  holding both its slots), with its home processor this one;
- the caller is this processor's `current`, and is blocking;
- after `wake_sleepers(now)`, as `choose_next` calls it first, the fair class
  holds nothing but the caller: `waiting() == 0`.

The last condition is stricter than §9.5's "nothing runnable here more
urgent", on purpose. To decide "more urgent" is to run the EEVDF pick, which
is what the direct switch exists to skip. With nothing else queued, the pick
can only choose the woken task, so skipping it changes no decision. The
other two conditions §9.5 named then need no test of their own:
- *Affinity* is implied by "home is this processor": a task's home is
  always in its affinity (`set_cpu` is only ever given an allowed processor,
  and affinity is fixed after construction, as the consultant's (d) on step
  1 records).
- *The caller's slice* does not matter when nothing waits: `arm_timer` arms
  no slice end with nothing waiting, and the general pick would choose the
  woken task whatever was left of the caller's request.
- *The job's quota* for processor time is a weight (H.SCHED.3), not a cap.
  There is no throttled state to test. What must be kept is the accounting,
  below. **This is true of `main` only** (condition 10). cgctl's `cpu.max`
  throttle and `cgroup.freeze` are not on `main` yet. When either lands, the
  direct switch must test the woken task's job for it (throttled, frozen) and
  decline, or this design is reopened. Each document names the other.

Neither task of a direct switch may be the idle task. The direct switch
asserts it (part 2's A4), as the consultant asked with question 4.

**What it skips.** Everything between the wake and the pick that the general
path does because it cannot know the woken task will run next:
- the wake's placement (`wake_with`, `wake_onto` or `wake_at_home`, their
  second lock and their loop), `kick_after_wake`, the resched flag and the
  timer's re-arm for the wake;
- the wait queue's push and pop of the caller, with their `Arc` clones;
- `schedule_from` and `pick_and_switch`'s general branches, and the pick
  itself: the woken task is never filed in the fair tree and picked back out.

**What it keeps, by calling it.** The tail of `choose_next` after the pick is
split out as one function, `switch_chosen`, and both `choose_next` and the
direct switch call it. It does, in this order: the switch count,
`carry_in_call`, `queue.previous` and `queue.current`, `note_running`,
`set_idle`, `exec_start`, `arm_timer(now)`, `note_switch`,
`swap_address_space` and `switch_user_state`, and `note_pick` while a
measuring window is open. Since neither task is the idle task, `set_idle`
writes "not idle", and step 2 makes that write a no-op when nothing changes.
`finish_switch` is the general one.

**The EEVDF accounting.** The general path makes, in this order: the
reader's insert while the writer still runs (`insert`, which charges the
running task first, joins the job's load, sets the effective weight and
enqueues with the reader's saved lag, scaled against the load that includes
the writer); then at the writer's block, `account`, the writer set blocked
(leaving its job's load), `detach_current` (its lag saved against an
average that includes the reader), and `pick_next`. The direct switch's
`hand_over(now)` is defined as exactly that sequence with every clock read
equal to `now`, on a queue whose tree is empty:
- the two tasks' vruntime, deadline and lag, the queue's load, sum and
  average, and its slice come out bit for bit as the composed calls leave
  them;
- the jobs' loads move as they do: the woken task joins before the caller
  leaves, so the transient is the general path's;
- `follow_group_share` runs inside `account` as it does now.

A host test in `src/lib/kernel/sched` checks the equality: random queue
states with one running entity and an empty tree, random weights, lags at
and past the clamp, `now` at wrapping boundaries, each run through
`hand_over` and through the composed calls, and every field compared. Its
control changes one field of `hand_over`'s result.

**H.SCHED.2's bound** is about tasks waiting in a queue. The direct switch
never runs while one waits, so it passes nobody over and the bound is
untouched. A task that arrives while two programs trade the processor this
way is inserted by its waker under the run-queue lock, and the next trip's
test of `waiting()` sees it and declines. The arrival's waker also kicks
this processor as it does today, and the kick is taken the moment either
program is back in user mode. So an arrival waits at most one trip longer
than it would on the general path. H.SCHED.1 and H.SCHED.4 follow the same
way.

**Not in step 4: the direct switch from general channel wakes.** §9.5 said
every channel call would gain, including one that carries handles. That
needs a woken task the waker holds for its own coming block, runnable but in
no queue: a new state every waker, stealer, balancer and `has_work` would
have to know. The fast path needs none of that, because it decides and
switches inside one hold of the run-queue lock. It is left for a step 4b,
reviewed on its own, and the target does not need it.

#### Part 2: the fast path for 0x1013

**Where it is entered.** At the top of `ferrix_syscall_entry`, the Rust
function x86-64's `SYSCALL` stub calls once it has pushed the frame. It
comes before `filter_system_call`, `answer_here` and the interrupt enable.
It returns into the stub's own exit, so entry and exit hardening are the
stub's and nothing is duplicated. AArch64, ARMv7-A and the `int $0x80`
entry have no fast path. Its first act, as `trap::system_call`'s is, is
`call_entered`, which raises `IN_CALL`, because its tail lowers it
(condition 2).

**Entry hardening runs before the insertion point** (condition 2). Every
row of SPECULATION.md §3, as `ferrix_syscall_stub` and the boot plan apply
it on `ipc-step1`:

| §3 row | Where it runs | Before the fast path? |
|---|---|---|
| Spectre v1: clamps | the handle index, in `HandleTable::get` under `channel_in`. The number is compared, not used as an index | shared: the fast path calls `channel_in` |
| Spectre v1: registers zeroed on `SYSCALL` entry | the stub, after the frame is pushed and before `callq ferrix_syscall_entry`, all fourteen | yes |
| Spectre v1: `lfence` after the conditional `swapgs` | interrupt entry only. `SYSCALL`'s `swapgs` is unconditional | not on this path |
| Spectre v2, program → kernel (enhanced IBRS, AutoIBRS, IBRS) | set in `IA32_SPEC_CTRL` or `EFER` at boot, on every processor; nothing per entry | yes, standing |
| Spectre v2, program → program (`IBPB`, refill, `STIBP`) | `IBPB` and the refill in `install` (part 4). `STIBP` is set at boot | the switch is `install` |
| Speculative store bypass (`SSBD`) | set at boot | yes, standing |
| MDS (`VERW` on return to ring 3) | `FERRIX_CLEAR_BUFFERS` in the stub's exit, after the pops, before `swapgs` and `sysretq` | after: the fast path returns into it |
| Zenbleed, GDS | `DE_CFG[9]` or the microcode switch at boot, else no AVX | yes, standing |
| (not a §3 row) `SFMASK`: IF, TF, DF, AC and the rest cleared by `SYSCALL`; `cld` in the stub | the processor, then the stub | yes |

So nothing of §3 runs between `callq ferrix_syscall_entry` and the general
dispatch that the fast path would skip.

**Interrupts are masked from entry to `sysret`**, as `SFMASK` left them: the
caller's entry, the commit, the switch, and the peer's tail up to its own
`sysret`. Nothing in it loops or waits, so the masked span is bounded by
its straight-line length (part 6).

**It waits on no lock.** Every lock it takes is taken with `try_lock`. A
lock that is held is a failed test, and the general path runs. That is the
whole of its lock-order argument: a code path that never waits cannot close
a cycle. It holds at most three at once, taken in a fixed order: the
channel's half of side A, the half of side B, then this processor's run
queue. The halves are locked by side, A before B, whichever end the caller
holds. The handle table's lock is taken and released before them.

*Nor does it close an end (F-65, 2026-10-07; ledger line 520).* The
caller's `Arc<Endpoint>` from the lookup is held across the park, and
another thread of the caller's process may close the handle meanwhile, so
that reference can be the end's last; its close takes locks, wakes the
survivor's reader and frees memory. So it is let go only with interrupts
open: the continuation lets go inside its own unmasked span, and the frame
tail's return and a decline after the lookup through `release_unmasked`,
which opens interrupts, makes the `may_block` check, lets go and masks
again. `Endpoint::drop` stops with FX-0535 on a masked close
(`L.object.179`). It is the only counted reference the masked span lets
go: the peer is a borrowed half, and the parked record's task moves.
What this gives a masked branch (os-76's ledger 524, V2-22): nothing is
allocated or freed and no lock is waited for while masked, since the
release runs with interrupts open; the peer's `PEER_CLOSED` is not
deferred -- the end closes at the release itself, as soon as the caller
runs again after its park, which is when the general path's waiter lets
go of its own reference at its call's end, so the delay is the same as
the general path's and bounded by the caller's resume; and stage 9's case
16 makes the close on both resumes, the frame tail's and the
continuation's.

**Every lock it takes**, directly or through what it calls (condition 5).
Each is either taken with `try_lock`, or is a *leaf*: a lock under which
nothing else is taken and nothing waits, so that its holder lets it go
within a few instructions whatever else is held.

| Lock | Taken by | How |
|---|---|---|
| the process's handle table (`Process::handles`) | the lookup, through `with_handles` | `try_lock`, through a new `try_with_handles`. `with_handles` itself waits. Since po9 (§9.11) `sync::try_lock_masked`, counted for A3, through `try_with_handles_masked` |
| the caller's half's inbox, the peer's half's inbox | T6 to T10, the commit | `try_lock`, by side. Since po9 (§9.11) `sync::try_lock_masked`, counted for A3 |
| the peer's half's observers (`Half::observers`) | T10's read | `try_lock`, under the peer's inbox lock: inbox before observers, the order `write_small`'s `trigger` already takes |
| the peer's half's wait queue (`WaitQueue::waiters`) | T10's read | `try_lock`, under the peer's inbox lock. No path takes an inbox lock under it: `wait_sliced` lets it go before `ready()` takes the inbox |
| this processor's run queue | T11 to T13, `hand_over`, `switch_chosen`, the switch | a new `try_lock_manually` beside `lock_manually`, which `choose_next` uses, handed over at the switch as there. `try_lock` exists on every lock type in `ferrix_sync` |
| the peer's `run_slot` and `sleep_slot` (`asleep_at_home`'s `holds_slots`); the caller's `run_slot` (`detach_current`'s `return_run_slot`); the peer's (`insert`'s `take_run_slot`) | A1, `hand_over` | leaves: a `SpinLock<Option<TaskSlot>>` taken for one `take`, store or `is_some`, with nothing under it |
| `ZOMBIES` | `finish_switch`, for a dead previous task only | not reached: the caller is blocked, not dead |

What takes no lock: `install` (the `CpuMask` join and leave and
`entered_space`'s words are atomics), `note_running`, `carry_in_call`,
`set_idle`, `arm_timer` (`ARMED` is an atomic, and the LAPIC write is
per-processor), `switch_user_state` (registers and this processor's GDT),
`set_state` with `join_group` and `leave_group` (`quota::adjust` is an atomic
add at each job level), `effective_weight`, and `trip::count` (an atomic,
inert unless tracing).

**The park.** Both sides of a trip block in 0x1013's read, and the peer's
words can only go into its frame if the peer will resume somewhere that
returns them. So a 0x1013 reader on the fast path *parks*:
- `Half` gains `parked: Option<Arc<Task>>`, under its inbox lock: the one
  task blocked in 0x1013 reading that end through the fast path.
- `Task` gains a reply cell: a length and three words, and whether it is
  full. Only the commit (below) fills it, and only the parked task empties
  it.
- A task parks only while its end's inbox is empty, with nothing else
  parked there, and sets itself blocked under the same hold of the lock.
- A parked task's continuation, wherever it is woken from, is: if the reply
  cell is full, take it and leave by the frame tail; otherwise take the
  *general continuation*, below.

**The general continuation** (condition 3) re-enters the general path at a
point from which everything it would still have done is done, with the
call's own arguments:
1. It opens interrupts, as `ferrix_syscall_entry` does before
   `trap::system_call`, and checks `sched::may_block()`, the check FX-0907's
   leave makes. A continuation reached with interrupts masked or a lock held
   stops the machine there.
2. It un-parks: it clears the record under its half's lock if the record
   still names this task.
3. It carries on exactly where `receive_words` carries on after
   `wait_trusting` returns: `must_leave`, then the loop.
4. It returns through `dispatch_write_read`'s `record_call`, with the six
   argument registers read back from the task's own frame. The fast path
   never writes them before the frame tail, and the frame tail is not on
   this branch. Then it maps the answer as `native_call` does, then
   `regroup_current` and `call_left` as `trap::system_call` does, still with
   interrupts on.
5. It masks interrupts and makes the stub's own way out: the outcome into
   the frame, then `needs_attention` and, if it says so, `return_to_user`.

The walk (part 2a) found that the general path asks `needs_attention` with
interrupts *masked*: it is the masked look of Linux's
`exit_to_user_mode_loop`, and `return_to_user` opens them itself. So step 5
masks them, as the general path does. Steps 1 to 4 run with them on.

To make this one function, `dispatch_write_read` is split at the wait:
`channel_write_read` up to the send, and `receive_then_answer(endpoint,
caller, args)`, the rest. The general path calls both. The continuation
calls the second.

A park is a field, not a list, so it never allocates (F-23).

**The general path gains one test.** `Endpoint::write`, `write_small` and
`Endpoint::drop` take the half's parked record under the inbox lock they
already hold, and wake that task after they let the lock go, beside the
wait queue's `wake_all_with`. A close stores `closed` first, then takes the
reader's half's lock to take the record, which is condition 1's order on
step 1. With the fast path off, or on AArch64 and ARMv7-A, nothing ever
parks, the record is always empty, and the general path behaves as step 1's.

**The two halves.** 0x1013 is a send and then a receive, so the fast path is
two halves, each of which commits or declines on its own:
- *The send half* delivers to a parked peer and switches to it directly. If
  any of its tests fails, `send_words` runs as on the general path.
- *The receive half* parks the caller. If any of its tests fails,
  `receive_words` runs as on the general path. After a send half that
  committed, the receive half's tests have already passed under the same
  locks, so the two commit together.

The receive half alone serves a receive-only call, and a call whose send
half declined. Without it, one trip that fell back would leave both sides
waiting in the general way, and no later trip could find a parked peer
again. With it, the first trip that parks puts both sides back on the fast
path.

**The tests, in order.** "Test" means the fast path declines when it fails.
The letters are used by parts 3 and 6.

*Before any lock:*
- **T1** The fast path is on (the boot switch, part 6), the entry is the
  native `SYSCALL` and the number is 0x1013.
- **T2** The caller is not *filtered*. The fast path runs before
  `filter_system_call`, so without T2 it would bypass a filter
  (`docs/SECCOMP.md`: a filtered process's native calls are filtered too).
  - *What exists today.* The walk found that on `ipc-step1` and on `main`
    no program can install a filter yet. The only filter is the boot
    check's probe (`seccomp::arm_probe`, `PROBE_TASK`), and there is no
    `ptrace` and no other per-call interception. So T2 is, for now, "no
    probe is armed": `PROBE_TASK` reads zero, loaded `Acquire` against
    `arm_probe`'s `Release`.
  - *What is owed when filters or tracing land* (condition 8). A core flag on
    `Process`, set one way by the personality, inherited by fork, `clone`
    and `process_create` as the filter is, and set on every process a
    `TSYNC` reaches. It is set for an attached filter, for a tracer's
    attach, and for any other per-call interception the personality gains.
    It must be visible before the filter or tracer takes effect on any
    thread: stored under the filter-install lock that the filter's own
    installation takes, or by a `SeqCst` store paired with a `SeqCst` load
    at T2. Seccomp's landing S3 and any `ptrace` landing carry the flag's
    rows and checks, and this design is named in theirs.
  - *As met for seccomp* ("as built" 2, §9.11's cut 3): S3 answers T2 by the
    personality's quiet predicate, whose first read after the probe word is
    a live count of filtered threads (`seccomp::FILTERED_THREADS`), raised
    before a thread's flag is raised and given back after it is lowered or
    the thread is dropped. One location changed by `AcqRel`
    read-modify-writes and read `Acquire`; by its coherence a thread whose
    own filter has taken effect never reads a value without its own count,
    so `SeqCst` is not needed (ledger 457, D1). `TSYNC` (S5) and `ptrace`
    reopen this note.
- **T3** The count is at most 24 bytes, or is `WRITE_READ_NOTHING`.

*Under the handle table's lock:*
- **T4** The handle names a channel endpoint, and **T5** it has READ, and
  WRITE when sending. Both are `channel_in`, the general function, whose
  refusal ends the fast path before anything else happens.

*Under the two halves' locks:*
- **T6** The caller's own inbox is empty. Otherwise the general path answers
  at once with what waits.
- **T7** Nothing is parked on the caller's half.
- **T8** The caller's peer end is not closed, read under the caller's half's
  lock as condition 1 requires.
- **T9** (send half) A task is parked on the peer's half.
- **T10** (send half) The peer's half has no observers and no wait queue
  waiters. Otherwise the general path would trigger `READABLE` on them and
  wake them, and the fast path, whose message never enters the inbox, would
  not. `WRITABLE` and `read_small`'s `was_full` wake are unaffected: the
  peer's inbox is empty (A2), so it was not full, and the general path's
  read makes no such wake either.

*Under this processor's run-queue lock (send half):*
- **T11** The parked peer's home is this processor.
- **T12** After `wake_sleepers(now)`, nothing waits in the fair class.
- **T13** The last look: neither the caller nor the peer has the *end* bit
  of step 2's pending-work word set, the bit posted for every task whose
  `must_leave` would answer true (a kill, another thread's `execve`).

*Asserted, not tested.* Three conditions follow from the others and from
the park's invariants. A test that cannot fail cannot have a control that
fires, so these stop the machine instead, each with a new FX code, and each
with a control that breaks the invariant from elsewhere:
- **A1** At the commit the peer is asleep at home (`asleep_at_home`). A
  parked task that has been woken is runnable, so it is queued here (T12
  fails) or running or queued on another processor (T11 fails).
- **A2** The peer's inbox is empty and its end is open. A record is set
  only while the inbox is empty, and any writer that fills it takes the
  record under the same lock. An end with a parked reader is held open by
  that reader's `Arc`.
- **A3** The preemption count is zero at the switch: FX-0503's own check,
  called from the direct switch as `schedule_from` calls it.
- **A4** Neither the caller nor the peer is this processor's idle task.

**The commit**, when every test has passed and with all three locks still
held, cannot fail:
1. The message's words go into the peer's reply cell, the bytes past the
   count zeroed as `Small::of` zeroes them. The peer's record is cleared.
2. The peer is set runnable (it joins its job's load), then the caller is set
   blocked (it leaves its job's load) and parked on its own half: that is
   `wait_trusting`'s state, with the record in place of the list.
3. The two halves' locks are let go.
4. `hand_over(now)`, then `switch_chosen` and `switch_to`, with the run-queue
   lock handed over to the peer's side as `choose_next` hands it over.

The receive half alone commits the caller's park and blocked state under its
own half's lock, then makes the last look at `must_leave` after a `SeqCst`
fence, which is `wait_trusting`'s ordering as condition 1 has it, and
blocks through the general `schedule()`.

**The frame tail**, on the peer's side, after `finish_switch`:
1. The reply cell is moved into the peer's own frame: the count into RAX and
   the words into RSI, RDX and R10, the registers `Outcome::ReturnWords`
   writes. No other register is touched.
2. With interrupts still masked, it looks at the peer's pending-work word
   (every bit, part 5), this processor's resched flag, the regroup word and
   T2's flag. If none is set, it lowers `IN_CALL` and returns to the stub's
   exit.
3. If any is set, it takes the *general branch*. It opens interrupts and
   runs `regroup_current` and `call_left` with them on, as
   `trap::system_call` runs them. Then it masks them and runs the stub's
   way out: `needs_attention`, masked as on the general path, and
   `return_to_user` if that says so. The branch begins with step 1 of the
   general continuation's `may_block()` check.

Every resume of a task whose vector registers were not saved goes through
the vector reset first (condition 7, part 5). That holds on the frame tail
and on the general continuation alike, because the reset is made in
`switch_user_state` as the task is switched to, not on either branch.

**The reply cell, not the frame, is what the commit writes.** §9.5 said the
words go straight into the peer's saved frame. They go into the peer's own
record instead, and the peer moves them into its frame. The cost is four
stores. In return, no task writes another task's kernel stack. And both
continuations, the frame tail and the general one, read the same cell, so a
peer woken some other way never finds its frame half-written.

**What the fast path does not do.** It does not allocate, queue a message,
touch the wait queue, trigger an observer, write an audit record or call
the filter. Each is either something the general path does not do for a
call that succeeds (an audit record is written for a refused call only,
`record_call`), or the subject of a test that declines (T2, T10). Part 2a
gives the whole list, function by function.

#### Part 2a: every effect of the general 0x1013 (condition 1)

The walk was made by reading `ipc-step1` at 5d7186d00. It goes from
`ferrix_syscall_stub` to `sysretq`, through every function a successful
round trip reaches on both sides: the caller's call, the wake of the peer,
the peer's resumption and its way out. Each is marked:
- **composed**: the fast path calls it, or its tail or the direct switch
  does;
- **declined by Tn**: the fast path runs only when that test shows the
  function would do nothing the fast path does not;
- **not done on success, because …**: what the reason is.

*The way in and the way out*

| # | Function | Mark |
|---|---|---|
| 1 | `ferrix_syscall_stub`, entry: `swapgs`, the stack, the frame, the zeroing, `cld` | composed (the stub's) |
| 2 | the stub's exit: the pops, `FERRIX_CLEAR_BUFFERS`, `swapgs`, `sysretq` | composed (the stub's) |
| 3 | `filter_system_call`, through `trap::ask` and `seccomp::check` | declined by T2 |
| 4 | `answer_here` (`arch_prctl`, `rt_sigreturn`) | not done on success, because it answers only those two numbers and returns false for 0x1013 |
| 5 | `enable_interrupts` / `disable_interrupts` around the dispatch | not done on the fast trip, because the fast path runs masked by design (part 6 bounds the span); done on the general continuation and branch |
| 6 | `trap::system_call`: `call_entered` | composed (the fast entry's first act) |
| 7 | `trap::system_call`: `SYSCALL_ENTRY`, then `dispatch_with`'s native-range test | declined by T1, the same predicate narrowed to one number |
| 8 | `native_call`: `sched::current()`, `thread()`, `process()` | composed (by borrow, after step 2) |
| 9 | `dispatch_write_read`: `record_call` | not done on success, because `record_call` matches only refusals (`ACCESS_DENIED`, a `RIGHTS` record) and other calls' successes. Every refusal comes from the general path, or from its continuation, which still calls it |
| 10 | `native_call`'s mapping, `errno::encode(Ok(count))`, `Outcome::ReturnWords`, and the entry's write of RAX, RSI, RDX and R10 | composed in effect: the frame tail, a new piece, writes the same four registers with the same values (case 1) |
| 11 | `trap::system_call`: `regroup_current` | composed (the frame tail's general branch when the regroup word says so; otherwise it has nothing to do) |
| 12 | `trap::system_call`: `call_left` | composed (the tail lowers `IN_CALL`; a resched flag takes the general branch, which calls it) |
| 13 | the return path: `needs_attention` (`must_leave`, `is_stopped`, a deliverable signal, a saved mask, a restart) | composed (every input has a pending bit, part 5; any bit takes the general branch, which calls it) |
| 14 | the return path: `return_to_user` | composed (the general branch) |

*The call*

| # | Function | Mark |
|---|---|---|
| 15 | `channel_write_read`: the count's conversion and `WRITE_READ_NOTHING` | declined by T3 |
| 16 | `with_handles`, `HandleTable::get` (its clamp), `channel_in`'s type and rights, the `Arc` clone | composed (`try_with_handles`, then `channel_in`); refusals declined by T4 and T5 |
| 17 | `send_words`: the 24-byte test, and the bytes built from the words | the test declined by T3; the bytes composed (the loop factored out of `send_words`, used by both) |
| 18 | `write_small`: `is_closed` | not done on success, because the peer's end is open (A2) |
| 19 | `write_small`: `accepts` and `is_full` | not done on success, because of T3 and an empty inbox (A2) |
| 20 | `write_small`: `put_small` into the peer's slot | not done on success, because on the general path the peer takes the message out of the slot in the same trip (`pop_small`), leaving the inbox as it was. The fast path's reply cell carries the same bytes (case 1) |
| 21 | `write_small`: `reserve`, the heap buffer, `push`, and the kernel-memory charge an allocation makes | not done on success, because with an empty inbox the general path uses the slot and allocates nothing either |
| 22 | `write_small`: `trigger(READABLE)` on the observers, and `deliver` | declined by T10 |
| 23 | `write_small`: `wake_all_with(Sync)`: the queue's `wakes` count, its drain, `wake_with` for each | the drain and wakes declined by T10. The `wakes` count is not done, because it is a kernel statistic that no program reads; it is the second exception beside the decline counters. The wake of the parked peer is composed (row 32) |
| 24 | `receive_words`: `read_small` on the caller's side, which finds nothing | declined by T6 |
| 25 | `receive_words`: `wait_trusting`'s `ready`, which is `readable_or_closed` and `must_leave` | declined by T6 and T8 (under the caller's half's lock), and by T13 (under the run-queue lock) |
| 26 | `wait_sliced`: `try_push` onto the wait queue, `set_state(BLOCKED)`, the fence, the last look, `block`, `unqueue` | the blocked state composed (`set_state`, in the commit); the list replaced by the park, which allocates nothing; the fence replaced by the run-queue lock (part 3) on the full trip, and kept on the receive half alone |
| 27 | `wait_sliced`: the queue's `woken` count, and `trip::slept` | not done, because they are kernel statistics (the second is inert unless tracing) |
| 28 | the peer's side: `read_small`, `pop_small`, the `was_full` wake | not done on success, because the message never enters the inbox, and the inbox was not full (T10's note) |
| 29 | the peer's side: the words built from the slot, zeros after the count | composed (the loop factored out of `receive_words`, used by the commit; the zero tail as `Small::of` makes it) |

*The scheduler*

| # | Function | Mark |
|---|---|---|
| 30 | `schedule_from`: FX-0503's preemption test | composed (A3) |
| 31 | `choose_next`: `account` (`add_runtime`, busy and idle time, `update_curr`, `follow_group_share`, `account_load`) | composed (inside `hand_over`) |
| 32 | the peer's wake: `wake_onto`'s move or `wake_at_home`, `remove_sleeper`, `take_sleep_deadline`, `set_state(RUNNABLE)` with `join_group`, `insert` (`effective_weight`, `enqueue`, `set_queued`, `rescale_slice`) | the move declined by T11 (the peer is already home here); the rest composed (the commit and `hand_over`, host-tested equal) |
| 33 | the wake's `kick_after_wake`, `resched_here`, `arm_timer` | not done on success, because the wake is deferred to the caller's block on the general path too (`Wake::Sync`), and the block's decision is the direct switch itself; the timer is armed once, in `switch_chosen` |
| 34 | `choose_next`: `wake_sleepers` | composed (before T12) |
| 35 | `choose_next`: `detach_current`, and `file_sleeper` for a deadline | `detach_current` composed (`hand_over`); `file_sleeper` not done, because a parked task, like a task in the trusting wait, has no deadline |
| 36 | `choose_next`: `pick_next` | replaced by `hand_over`, host-tested equal on a queue with nothing waiting (T12) |
| 37 | `choose_next`'s tail: the switch count, `note_preemption` (never for a call), `carry_in_call`, `previous` and `current`, `note_running`, `set_idle`, `exec_start`, `arm_timer`, `note_switch`, `note_pick` when measuring | composed (`switch_chosen`) |
| 38 | `swap_address_space`, `install`, `entered_space` | composed (`switch_chosen`) |
| 39 | `switch_user_state` | composed (`switch_chosen`) |
| 40 | `switch_to`, `finish_switch` | composed |

*What a program or a job can read*

| # | Effect | Mark |
|---|---|---|
| 41 | the task's run time (`CLOCK_THREAD_CPUTIME_ID`, procfs, `getrusage`), the processor's busy and idle time and load average (`/proc/stat`, `/proc/loadavg`) | composed (`account`, at `now`) |
| 42 | the task's `switches` and `cpus_run_on` | composed (`note_switch`) |
| 43 | the jobs' processor-share loads, `quota::adjust` at every level | composed (`set_state`, the woken task first, as on the general path) |
| 44 | job charges for kernel memory | not done, because nothing is allocated on either path (row 21) |

**The count.** There are 44 rows: 26 composed, 7 declined by a test, and
11 not done on success. A row that is part one mark and part another is
counted once, by its main mark: rows 16, 17 and 32 as composed, row 23 as
declined. Three of the 26 rest on the new pieces: the frame tail (row 10),
the park (row 26) and `hand_over` (row 36). The other 23 call the general
function itself.

**What the walk changed in the design:**
- *T2 today is "no probe armed".* No program can install a seccomp filter
  on `ipc-step1` or `main`, and there is no `ptrace`. Condition 8's flag is
  owed by whichever landing brings either (T2's note).
- *A refused 0x1013 writes an audit record only for `ACCESS_DENIED`.* So
  condition 3's case "a parked receive-only call ended by a close writes its
  audit record" becomes "writes what the general path writes": no record
  for `PEER_CLOSED` today. The continuation still calls `record_call`, so a
  record added later is written on both paths.
- *`needs_attention` is asked masked* on the general path. The general
  branch matches that, and runs only `regroup_current` and `call_left`
  with interrupts on (the general continuation's note).
- *The wait queue's `wakes` and `woken` counts* move on the general path
  and not on the fast one. Both are kernel statistics. With the decline
  counters, they are the only state a declined or committed fast path
  leaves differently, and no program can read any of them.
- *Job loads cost one locked add per job level per task.* `quota::adjust`
  walks to the root, so part 7's count of locked operations grows with the
  depth of the jobs. It must be measured (part 7).
- *`install` takes no lock, but makes five locked operations* (the
  `CpuMask` join and leave, and `entered_space`'s three swaps) until step 2
  makes the swaps plain stores.

#### Part 3: the task states and who may touch them

The states of a task that makes 0x1013, with the lock or atomic that orders
each transition. States 1 to 3 are step 1's (`wake_with` on `ipc-step1`). P
is the park.

| State | What holds | Who may change it, under what |
|---|---|---|
| R: running | `current` here, runnable | itself; a kill, signal or `execve` posts a bit and wakes (a no-op) and interrupts it |
| P1: parking | record set and blocked under one hold of its half's lock; still `current` | a writer or close takes the record and wakes it (state 1 wake: runnable again, under the run-queue lock); only its home's `choose_next` or the direct switch takes it off the processor |
| P2: parked | state 2 and the record: switched out, detached, holding both slots | a fast commit (both half locks and the home's run-queue lock); a writer or close (half lock, then the wake under the run-queue lock, which may move it by `wake_onto` under both queues' locks, lower first); a kill or `execve` (bit, then the wake under the run-queue lock); a signal (the same wake: the continuation finds no reply and `must_leave` false, and waits again in the general way) |
| H: handed | reply full, runnable, record clear: exists only inside the commit, with the run-queue lock held until the peer runs | nobody: every waker needs that lock |
| W: woken | runnable, queued somewhere, reply empty, record maybe still set | its queue's lock as for any runnable task; a deliverer finds it not asleep at home and declines (T11, T12, or A1) |
| C: continuing | running in its continuation | itself: takes the reply, or un-parks under its half's lock |
| 3: filed elsewhere | sleep slot out | nobody here: a fast park files no deadline, so a parked task is never in it |

**Why no wake is lost**, waker by waker. This is the ordering argument of
step 1's condition 1, applied to the two new states:
- *A message* from a general writer. The record and the blocked state are
  written under the half's inbox lock, and the writer reads the record under
  the same lock. Either the writer came first, and T6 sees its message, or it
  comes after, and finds the record and wakes a task that is blocked.
- *A close.* It stores `closed`, then takes the record under the reader's
  half's lock. Either T8 sees `closed`, or the close finds the record.
- *A kill or an `execve`* during a full trip. The poster sets the end bit,
  then calls `sched::wake`, which takes the target's home run-queue lock
  even when the target runs (`wake_at_home` today). The fast path's last look
  (T13) and the caller's blocked state are both under that lock. If the
  poster's wake came first, its bit happens before T13 and T13 declines. If
  it comes after, it finds the caller in P2 and wakes it. No fence is needed
  because the lock orders both. This needs two of step 2's rows, which part
  5 lists: every poster sets the bit before it wakes; and `sched::wake`
  takes the target's home run-queue lock *before* it reads the target's
  state, with no lock-free early exit (condition 6). `wake_at_home` does so
  on `ipc-step1`: it takes the lock, then re-reads `task.cpu()`, then reads
  the state. An optimisation that read the state first and returned on
  "not blocked" would break T13's argument, and the row's control is that
  early exit, which must make the T13 case hang past its bound.
- *A kill or an `execve`* during the receive half alone. The caller blocks
  through the general `schedule()`, so the order is `wait_trusting`'s: the
  blocked state, a `SeqCst` fence, the last look, paired with the fence
  before the poster's wake, as condition 1 has it.
- *A signal* ends no native wait (step 1). It wakes a parked task, which
  finds no reply, finds `must_leave` false and waits again in the general
  way. Its handler runs at the call's way out, as on the general path.
- *A stop*, and in time a tracer's stop, likewise end no native wait. Each
  posts its bit (part 5). A task handed a reply with a bit set takes the
  frame tail's general branch, and `return_to_user` stops it there, as it
  stops a task leaving the general path.
- *A tracer or a filter attaching while the task is parked* (condition 4),
  once either exists. The frame tail reads T2's flag as it reads the pending
  word, and takes the general branch whenever the process is filtered. So a
  tracer's attach need post no bit of its own to reach a parked task's way
  out.

**Lock order.** New edges: the half of side A, then side B, then the run
queue, and under a peer's inbox its observers and its wait queue (part 2's
table). All of them are taken by `try_lock` and never waited on, so no new
cycle exists, and the slot locks are leaves. The general path keeps its order. Writers and closes let the
half lock go before they wake, as today, so they take no new nesting.

#### Part 4: speculation

The direct switch makes the barrier decision by calling the function that
makes it on the general path. `switch_chosen` calls `swap_address_space`,
which calls `AddressSpace::install(replacing)`. That reads the outgoing
space's domain as it leaves (`left_space`), names the incoming one
(`entering_space`), writes `CR3`, and in `entered_space` compares with
`LAST_DOMAIN`. It then issues `IBPB` and the refill, or, inside one domain,
the refill alone (§9.3a, A2). There is no second copy of the rule to drift.
It follows that:
- no `IBPB` is skipped outside one domain;
- the fast path has no domain condition, and is correct between programs of
  different domains too. It is only slower there, by the 230 ns of §9.6;
- `forget_root`, `leaving_domain` and `serve_wanted_barrier` are untouched.
  A grace period waiting for this processor's IPI waits at most the masked
  span of part 6, which is shorter than the general path's own switch
  spans with a barrier in them.

The handle lookup is `channel_in`, so its index clamp is the general one.
When os-35's lazy TLB replaces `install` with `switch_here` (§9.3b, F6), the
direct switch follows, because it calls whatever `choose_next`'s tail calls.

The equivalence check (part 6) counts the decisions: the same trips run on
both paths must give the same `barrier_decisions_on`, `switch_barriers_on`
and `refills_in_domain_on`, in a domain and across two.

#### Part 5: vector state, FS and GS, and what must land first

**Vector state, under the contract of step 3** (the customer's yes, 2026-10-02).
Both tasks of a direct switch are blocked in 0x1013 by construction, so the
contract covers both. The caller's vector registers are not saved, only its
`MXCSR` and x87 control word. The peer's are reset (`XRSTOR` of an empty
header) before its `sysret`, keeping its own `MXCSR` and control word. None of
the caller's vector state reaches the peer. This is `switch_user_state` as
step 3 changes it, called from `switch_chosen`, not a fast path variant. A
task preempted anywhere else is saved and restored in full, as today.

**Every resume of an unsaved task is reset** (condition 7). A task whose
vector state was not saved, because it blocked under the contract, is
marked so in its record. `switch_user_state` resets every such task as it
is switched to, whoever switches to it. That covers the frame tail and the
general continuation, after a reply, a message, a close, a kill or a
signal, on the direct switch and on any `choose_next`. The mark is cleared
only by the reset. PKRU and AMX tile state stay at their reset values as
long as `CR4.PKE` is off and AMX is not in `XCR0`, as on the reference
configuration. If either is turned on, the reset's component list is
re-reviewed. Part 6's case 15 checks this, with a control.

**FS and GS, under step 3.** The bases are kept in the task, written only by
`arch_prctl` and the switch, and the switch writes each only when the two
tasks' values differ. Native programs have no segment selectors or TLS
descriptors, so the switch skips those when both tasks' are null. FSGSBASE
stays off (§9.6).

**What the fast path assumes, and so what must land first:**
- *Step 1*, with its five conditions met: (1) the trusting wait's
  ordering, (2) the `sync` line, (3) the timer's bound, the clock and the
  rows, (4) the kernel reader's slot, (5) the rebase and the full gate. The
  fast path also relies on step 1's state list, its Sync wake and
  `Small::of`'s zero tail.
- *Step 2's pending-work word* (condition 4), with a bit for everything
  `needs_attention` looks at, so that a clear word means `needs_attention`
  would answer false:
  - *end*: the process is ending (a kill), or another thread's `execve`
    is replacing the program: `must_leave`;
  - *stop*: `is_stopped`;
  - *signal*: a deliverable signal, a saved mask to put back, or a restart
    (`signal::needs_attention`'s three);
  - *trace*: a tracer's exit stop, reserved until `ptrace` exists;
  - and T2's filtered flag, read beside the word by the frame tail.

  Step 2's rows say so, one per bit, each with a control that leaves the bit
  unposted and shows a case going wrong. Two more rows: every poster sets
  its bit before it calls `sched::wake` on the target; and `sched::wake`
  takes the target's home run-queue lock before it reads the state, with no
  lock-free early exit (condition 6), with the control of part 3. Without
  these rows, T13 has no meaning.
- *Step 2's lighter `PreemptSpinLock`*, with a `try_lock` that keeps
  FX-0503's bookkeeping (the count raised, the site recorded) and leaves
  `may_block()` meaning what it means now. The fast path blocks with
  interrupts masked, as every switch does inside `schedule`. It calls no
  wait that requires `may_block()`, nothing that allocates, and nothing that
  waits for a grace period, and FX-0503's check at the switch stays.
- *Step 2's `current()` by borrow*, with its argument against reaping. The
  fast path reads the running task once, from the processor's record.
- *Step 3's vector contract and FS and GS in the task*, for the budget. The
  fast path is correct without them, because it calls `switch_user_state`,
  whatever that does; it is only too slow.
- Not needed: PCIDs (nazuna has none, §9.6), step 2's `IDLE` and
  `effective_weight` changes (budget only), and step 4b.

#### Part 6: the evidence

**What is argued, and what is not.** This is a tested equivalence argument,
not a proof. seL4's fast path is covered by its refinement proof: the
fast path's C is proved to implement the same abstract specification as the
slow path. Ferrix has no such specification and no proof. What it has:
- the composition principle, so most of what the fast path does is the
  general path's own code, verified by that code's own checks;
- `hand_over`'s host-tested equality;
- for every test the fast path adds, a case that makes it fail and a
  negative control that shows the case would catch its removal;
- the same observable results from the same programs on both paths.

**The criterion.** For deterministic cases, the results must be identical:
return codes, return words, message order, and the order of closes and ends.
For cases where a waker races the trip (a kill, a close, a signal during the
wait), the general path itself has more than one correct result, depending
on timing. A message may be read before an end or left unread, for example.
There the criterion is refinement: every result the fast path gives must be
one the general path can give. To that the consultant added liveness
(question 2): no waiter stays blocked past the bounds that T7's, T8's and
T13's cases state. The allowed set for each racing case is derived from
the general path's requirements (L.object.126 to 130 and the channel's
rows), not from the results of observed runs, and the check states each
set with the row it comes from.

**The boot switch.** `ferrix.fastpath=off` turns the fast path off.
`ferrix.fastpath=on` turns it on. Which is the default in the certified
configuration is the customer's call. The consultant recommends off for the
first release, with on in development builds and in the perf rows, and the
Safety Manual describing the option. The option is read as
`ferrix.checks` is, from the loader's command line and then the device
tree's `bootargs`. Stage 9 prints one line either way, `ipc fast path for
channel_write_read: on` or `off`, and with any other value says it was
ignored. The kernel keeps counters: trips taken, parks, and declines per
test, T1 to T13. With the queue's `wakes` and `woken` counts, they are the
only state a fast path leaves differently from the general path (part 2a),
and none of them is readable by a program. The equivalence check and `ipc-bench` print them, so every
figure says which path it measured. A `domain-call` figure counts only if
at least 99.9% of its trips took the fast path.

**The equivalence check.** A native program pair, `ipc-equiv`, is run by
stage 9 on every architecture. On AArch64 and ARMv7-A it shows the general
path unchanged, and every fast path counter zero. It prints one transcript
line per case. The gate boots x86-64 twice, once with the fast path on and
once with it off, and the two transcripts must be identical but for the
counter lines and the racing cases, which must be in their allowed sets.
The cases:
1. Echo of every length from 0 to 24 bytes: the words and the zero tail.
2. A receive-only first call, then 1,000 trips carrying sequence numbers:
   the order.
3. A message already waiting on the caller's end (T6).
4. Two threads of one process reading one end (T7).
5. The peer end closed before the call (T8), during the wait, and before the
   reply, by another thread of the peer's process.
6. A kill of the waiting process, of the peer during a trip, and an
   `execve` by another thread of the waiting process.
7. A signal with a handler during the wait: the wait goes on, and the
   handler runs at the way out.
8. A port observing the peer's end, and an `object_wait_one` waiter on it
   (T10): the packet and the wake.
9. The peer pinned to another processor (T11), at two processors.
10. A spinner of equal weight pinned to the trip's processor (T12): its share
    within H.SCHED.2's bound on both paths.
11. A filtered process, with a filter that refuses 0x1013 and one that
    allows it (T2). Today that is the boot check's probe armed on the
    caller's task (T2's note).
12. A count of 25, a closed handle, a VMO handle, and a handle without WRITE
    or without READ (T3 to T5), with the `RIGHTS` audit records counted.
13. The trips in one domain and across two, with the barrier counters of
    part 4.
14. An end posted in T13's window (below).
15. Vector state across a general resume (condition 7). A parked caller is
    resumed by the general path, by a message from a third task, by a close
    and by a kill of a sibling thread, after another program has run on its
    processor with every vector register set to a pattern. The caller reads
    back every vector register it can name, and must see none of the
    pattern.
16. The general continuation (condition 3). A parked receive-only call
    ended by its peer's close answers `PEER_CLOSED`, and the audit log holds
    the same records after it as after the same call on the general path:
    none today, since `record_call` writes none for `PEER_CLOSED` (part 2a,
    row 9).

**One negative control per test.** Each test the fast path adds is replaced
by `true`, one at a time, and the check must stop the boot on its own
message:
- T2: the refusing filter's case answers success;
- T3: the count of 25 answers success with 24 bytes delivered;
- T6: the waiting message is not the answer;
- T7: one of the two readers is never woken within the stated bound;
- T8: a receive-only call after the close stays blocked past the bound;
- T10: the observer's packet is missing;
- T11: the peer reports a processor its affinity does not allow;
- T12: the spinner's share falls below H.SCHED.2's bound;
- T13: the process ended in the window stays blocked past the bound;
- the reply's zero tail: step 1's control, re-run on the fast path;
- A1 and A2: a record planted by the check, naming a running task or set
  beside a full inbox, stops the machine with its FX code;
- `hand_over`: the host test's own control;
- the vector reset skipped on a general resume: case 15 sees the pattern;
- the general branch of the frame tail left masked: the `may_block()`
  check of FX-0907 stops the machine;
- the T13 hook left set after stage 9: the boot check below stops the
  machine with its FX code;
- `sched::wake` given a lock-free early exit (condition 6): case 14 hangs
  past its bound;
- one control per pending-work bit, as step 2's rows (condition 4).

T1 is the switch itself; its evidence is the two boots. T4 and T9 are
structural: an `Option` or a `Result` the code must match, and "true" does
not compile. T5 is `channel_in`'s, shared with the general path and
controlled by that path's own rights check.

**T13's window** is a few nanoseconds between the caller's entry and its
last look, and no program can aim a kill at it. The check uses a hook in the
fast path (condition 11):
- it is a static function pointer that only stage 9's check sets, and that
  stage 9 clears before init starts;
- the fast path calls it at that point when it is set, and it posts an end
  to the caller;
- a boot check after stage 9 stops the machine with its own FX code if the
  hook is still set, and that check has its own control;
- unset, it costs one load.

A decline count above zero for T13 is not evidence that T13 works. Only
case 14 and its control are.

**An exhaustive model of the park protocol** (condition 9). The racing cases
on the real code are samples. To cover every interleaving, a host model is
built with `loom`, in a test crate beside `src/lib/kernel/sched`, because
it is Rust, it explores the orderings the C11 model allows, and its fences
and atomics are the ones the kernel uses. TLA+ with TLC is the fallback if
`loom`'s state space proves too large. The model has:
- a caller, a peer and a general writer, each a thread;
- the two halves' inbox locks, the run-queue lock, the record, the reply
  cell, the task states, the end bit and `IN_CALL`;
- the operations park, commit, the general writer's write, close, a kill
  posting its bit then waking, the un-park, and T13's last look under the
  run-queue lock, each as the design orders it;
- for the receive half alone, `wait_trusting`'s fence pair.

It checks that every outcome is in the allowed sets part 6 states (one
model test per set), and that no task stays blocked with a message, a close
or an end that should have woken it. Its controls drop the fence, read T13
outside the lock, and let a writer leave the record, each of which must
fail. It is planned at 5 to 8 points. `ipc-equiv`'s racing cases stay as
samples on the real code.

**Coverage.** The check must reach every branch of the fast path, which the
decline counters show: each one is at least 1 with the fast path on. The
fast path's statements and decisions are carried into the coverage evidence
as the rest of the item's are (F-10, F-13), x86-64 only. On AArch64 and
ARMv7-A the record is never set (question 13). If it can be compiled for
x86-64 only by touching just the record's own sites (the `Half` field and
the three writers' takes), it is. Otherwise its branches on Arm are argued
in `coverage-argued-*.json`, which the consultant accepts. Every low-level requirement of 0x1013
is verified on both paths, and a test that ran only one path does not count
for the other.

**`ipc-bench` both ways**, back to back on the same host load: `call` and
`domain-call` with the fast path on and off, with the counters, and against
seL4 as §9.5 has it.

**The residual risk, plainly.**
- *An effect not listed.* The tests are derived from a list of everything
  the general path does for this call: lookup, rights, audit, filter, send,
  observers, wakes, wait, read, accounting, the way out. An effect of the
  general path missing from that list is a difference that no case looks
  for. Review is the only defence; the list is part 2's.
- *An interleaving not exercised.* The racing cases sample races on the
  real code. The `loom` model enumerates them, but on a model, and the
  model can differ from the code. That gap is closed by review only.
- *`hand_over`'s test samples states.* It covers the boundaries the code
  has, but it is not exhaustive.
- *Dependence on rows elsewhere.* Step 1's state list and step 2's poster
  rule are what part 3 stands on. A later change to either must re-open
  this design. Each of the three documents names the others.
- *One architecture's register map.* The frame tail writes x86-64's four
  registers. That is checked by case 1 and by step 1's register check.
- *The hook.* T13's control needs code in the item that only a check uses,
  guarded by the boot check above.

The consultant's answer to question 1 is that this is enough for DAL C,
and for EAL5+ through part 8's documents. Accepting the residual risk is
the customer's and the evaluator's decision, not the consultant's.

#### Part 7: the cycle budget

Per direction, at the guest TSC's 4,400 MHz (1 ns is 4.4 ticks). The
address-space switch is §9.6's measured 130 ns, the refill's 20 ns included,
and both kernels pay it. The rest is an estimate from the code on
`ipc-step1` and the costs §9.5 measured, to be checked by the timing build's
ablations once step 4 is built. Step 4's correctness depends on none of
part 7: not on the figures, and not on ERAPS.

| Piece | ns (estimate) |
|---|---|
| `SYSCALL`, `swapgs`, stack switch and frame push; frame pop, `swapgs`, `SYSRET` | 20–25 |
| T1 to T3, and the running task by borrow | 2–3 |
| the handle lookup: the table's lock, the index, type and rights, the endpoint's `Arc` clone | 8–12 |
| two half locks and the run queue's, taken and let go | 8–12 |
| T6 to T13 under them: about 15 loads and compares | 3–5 |
| the commit: the reply cell, the record, two state swaps with their job-load updates | 8–12 |
| `hand_over` and `switch_chosen`'s bookkeeping: the clock read, the charge, the placement arithmetic, `note_running`, `carry_in_call` | 12–18 |
| `switch_to` and the entry stack | 3–5 |
| user state: the FS base when it differs, the vector reset, `MXCSR` and control word | 12–20 |
| the frame tail | 2–3 |
| **software, total** | **80–115** |
| the address-space switch (measured) | 130 |
| **one direction** | **210–245** |

Against seL4's 220 ns a direction, that is between 5% under and 11% over:
inside step 5's gate of 1.10, but not yet "as good or better" at its upper
end. Where the time goes, and what step 5 can take:
- *The locked operations:* about 14 a direction (four locks, the `Arc`, the
  state swaps, the job loads), and more with deeper jobs, since
  `quota::adjust` makes one at every job level, and five more in `install`
  until step 2. The count is measured, by ablation, before anything assumes
  that ERAPS closes the gap. Two tasks of one job could fold their two
  job-load updates into none, but the transient differs from the general
  path's, so that needs its own argument. A lookup without the table's lock,
  and a park that holds the task without an `Arc`, are step 5 items too.
- *The clock read:* `rdtsc` is about 10 ns. The design reads the clock once
  a direction, in `hand_over`, and every later use takes that `now`.
- *The vector reset:* `XRSTOR` of an empty header is cheap when the
  processor's init tracking already shows the state clean. That is measured,
  not assumed.
- *ERAPS in place of the refill* (§9.6) would take 20 ns a direction off the
  switch. That is what brings the upper end under seL4. It stays in step 5,
  with its own consultant review (question 12).

#### Part 8: requirements and documents

**Requirement rows** (names only; ids are reserved before they are written):
- *H, direct switch:* a switch made without the run queue's pick leaves
  every scheduling quantity as the pick would, and is made only when the
  pick could choose nothing else.
- *H, fast path:* every result of 0x1013 answered by the fast path is a
  result the general path gives in the same circumstances, and the fast
  path bypasses no filter, audit record or barrier.
- *L.sched:* `hand_over` equals the general sequence on a queue with nothing
  waiting (host test).
- *L.sched:* the direct switch runs only with the woken task asleep at home
  on this processor and nothing waiting after `wake_sleepers`.
- *L.sched:* the direct switch's tail is `choose_next`'s, through
  `switch_chosen`.
- *L.object:* a park record is set only with an empty inbox, the task
  blocked, under its half's lock. It is cleared by the commit that delivers
  to it, by a writer or close, or by the task before it leaves the call.
- *L.object:* a general writer or close that finds a record wakes its task
  after letting the lock go.
- *L.object:* a parked task's continuation takes its reply, or carries on as
  `receive_words` does after its wait.
- *L.object:* the reply's bytes past the count are zero.
- *L.x86_64:* the fast path is entered only from the `SYSCALL` entry, for
  0x1013, with the switch on and the process not filtered. It runs with
  interrupts masked and waits on no lock.
- *L.x86_64:* a declined fast path has changed nothing.
- *L.x86_64:* the frame tail writes RAX, RSI, RDX and R10 from the reply
  cell and no other register, and leaves by the general way out when any
  work is pending.
- *L.object or L.syscall:* the filtered flag is set one way and inherited.
- *L.x86_64:* `ferrix.fastpath` turns the fast path off, and the boot says
  which path it runs.
- *L.x86_64:* the fast path raises `IN_CALL` at entry, and runs after every
  entry measure of SPECULATION.md §3 (condition 2).
- *L.object:* the general continuation re-enters with interrupts on,
  through `record_call` with the call's own arguments, and passes the
  `may_block()` check (condition 3).
- *L.sched:* every task resumed with its vector state unsaved is reset
  first, on any path (condition 7).
- *L.x86_64:* the T13 hook is set only during stage 9, and a boot check stops
  the machine if it is set after (condition 11).
- *L.sched:* the direct switch's tests and asserts: A1 to A4.
- *Step 2's rows*, owed by step 2 and named here (conditions 4 and 6): a
  pending-work bit for each of end, stop, signal and trace, each with a
  control; posters set the bit before `sched::wake`; `sched::wake` takes the
  home run-queue lock before it reads the state.
- *Owed by seccomp S3 and any `ptrace` landing* (condition 8): the filtered
  flag's visibility, its inheritance by fork, `clone`, `process_create` and
  `TSYNC`, and its setting at a tracer's attach.
- *Owed by cgctl's `cpu.max` and `cgroup.freeze`* (condition 10): the direct
  switch's test of the woken task's job.
- *The `loom` model* (condition 9) verifies the park protocol's rows above,
  beside the boot checks.
- *Reused, not new:* H.TRAP.17 and L.object.113 with L.x86_64.126, since
  the barrier goes through `install`; H.SCHED.1 to H.SCHED.4; L.object.126
  to 130 and L.sched.5 to 8 (step 1); step 3's vector-contract row.

**Documents.**
- *SPECULATION.md* §3: no row changes. The *program → program* row gains a
  sentence: every switch, the direct one included, decides its barrier in
  `AddressSpace::install`. If ERAPS is accepted later, that row changes then.
- *MEMORY-AND-TIMING:* a §2.2f for the fast path. The span it masks
  interrupts for has no loop and no wait. Measured, it is the one-direction
  time, about 0.25 us in a domain and 0.5 us across two, where `IBPB` adds
  230 ns. It is no WCET claim, as §2.1 says of the rest. §2.2c gains the
  fast path figures and which path each one ran.
- *FINDINGS, F-23:* the fast path and the park allocate nothing (NOALLOC at
  each site, counted by the gate as now). A parked reader is on no list, so
  the recheck `wait_trusting` keeps for a task it could not list does not
  arise there. No new finding is expected; the consultant may open one.
- *The Security Target:* no new SFR, and FDP_IFC.1 and FDP_IFF.1 unchanged.
  ADV_ARC's non-bypassability argument gains the fast path: a second way
  through one call that bypasses no filter (T2), no audit (it answers only
  calls that write no record) and no barrier (part 4). ADV_TDS describes it
  as a module of the channel subsystem. ATE names the equivalence check as
  the test of both paths. VULNERABILITY-ANALYSIS gains an entry, because a
  second path is where a bypass is looked for first.

#### Part 9: questions for the consultant

Answered on 2026-10-02; the answers are in the review below.

1. Is a tested second implementation, built by composing the general path's
   functions with three new pieces, enough for DAL C and EAL5+, with part
   6's residual risk stated? If not, the plan stops at step 3 plus a direct
   switch, and §9.6's target is out of reach.
2. Is refinement (every fast result is a general result) the right criterion
   for racing wakers, with identical transcripts for the rest?
3. The commit writes the peer's reply cell, and the peer writes its own
   frame. Does the consultant want the frame written by the waker instead,
   as seL4 does and §9.5 said?
4. Is the stricter condition, nothing waiting at all, accepted in place of
   "nothing more urgent", with affinity, the slice and the quota argued away
   as part 1 does?
5. Is "every lock taken with `try_lock`, none waited on" accepted as the
   fast path's lock-order argument, with up to three locks held at once?
6. Is a check-only hook inside the item acceptable for T13's control, or
   should that case be statistical, with a decline count above zero as its
   evidence?
7. Are the structural tests (T4, T9) and the shared one (T5) accepted
   without controls of their own, and the asserted invariants A1 and A2
   with planted-state controls, A3 being FX-0503's own check?
8. In the certified configuration, should the fast path be on by default, as
   measured, or off, with on as an option the integrator takes and the
   Safety Manual describes?
9. Can step 4b, the direct switch from general channel wakes, leave step 4,
   to be reviewed on its own or not built?
10. Is the filtered flag acceptable as a one-way downward interface, like
    `leave_speculation_domain`, set by the personality for seccomp and for a
    tracer?
11. Should step 5's candidates that touch this design (folding the job-load
    updates, a lookup without the table's lock, a park without an `Arc`)
    each come back as a design first?
12. The upper end of part 7 meets "as good or better" only with ERAPS. Should
    ERAPS's review come with step 4's code, or stay in step 5?
13. Is the general writers' record on AArch64 and ARMv7-A, never set there,
    acceptable as argued coverage, or should those architectures not compile
    it at all?

**Points**, re-estimated after the review. §9.5 gave step 4 23 to 37
points; the first draft 29 to 45. The review adds the `loom` model, the
general continuation's split and checks, the `try_` variants, the hook's
guard, two cases and four boot controls:

| Piece | Points |
|---|---|
| this design, and its two revisions | 4–6 |
| `hand_over`, its host test and control; `switch_chosen` split out | 5–8 |
| the park: record, reply cell, the general writers' take, the general continuation with `receive_then_answer` split out | 5–8 |
| the fast path: entry, `IN_CALL`, T1 to T13 and A1 to A4, the `try_` variants, the commit, the frame tail and its general branch, the boot switch and line | 7–10 |
| `ipc-equiv`: 16 cases, both boots in the gate, the counters, the T13 hook and its boot check | 7–10 |
| the `loom` model of the park protocol, with its three controls | 5–8 |
| seventeen controls in the boot and host checks, coverage carried, rows and documents | 6–9 |
| **total** | **39–59** |

Not counted here, because they belong to the landings that bring them: step
2's pending-work rows and their controls, the filtered flag (seccomp S3,
`ptrace`), and the throttle and freeze tests (cgctl).

#### The consultant's design review (2026-10-02) and what changed

The verdict on 55dcec7aa was **OK to build IF**, with eleven conditions.
It was made by a consultant subagent of the session running this plan, with
the seat empty, as the customer allowed (§9.5's decision 4), and is recorded
in the consultant's ledger. Each condition, and what it changed:

1. **The effect list, before any code.** A walk of every function the
   general 0x1013 reaches, from `ferrix_syscall_entry` to `SYSRET`, each
   marked composed, declined by a test, or not done on success with the
   reason. Done now, from `ipc-step1` at 5d7186d00: part 2a. It has 44
   rows: 26 composed, 7 declined, 11 not done. It changed four things: T2
   (no program filter or tracer exists yet), condition 3's audit case (no
   record for `PEER_CLOSED`), where `needs_attention` runs (masked), and part
   7's count of locked operations (one per job level, five in `install`).
2. **Entry hardening.** Part 2's table shows every SPECULATION.md §3
   measure in the stub, at boot, in `install`, or in the stub's exit, and
   none between the insertion point and the general dispatch. The fast path
   raises `IN_CALL` at entry.
3. **The continuation.** Part 2's *general continuation* re-enters through
   `record_call` with the call's own arguments, read back from the frame, and
   runs `regroup_current` and `call_left` with interrupts on. The walk found
   that the general path asks `needs_attention` with interrupts masked, so
   the continuation does too, as the stub's way out. Case 16 is the close's
   case. It checks the general path's audit records, which are none for
   `PEER_CLOSED`. The `may_block()` check of FX-0907 begins the branch, with
   a control.
4. **The pending-work word.** Part 5 lists a bit for everything
   `needs_attention` reads: end, stop, signal (with the saved mask and the
   restart), trace, and T2's flag beside them. A tracer's attach to a parked
   task reaches it because the frame tail takes the general branch whenever
   the process is filtered (part 3). Step 2's rows carry one control per
   bit.
5. **Locks.** Part 2's table lists every lock reached: the handle table
   (`try_with_handles`), both inboxes, the peer's observers and wait queue,
   the run queue (`try_lock_manually`), and the slot locks, which are
   leaves. `install`, `note_running`, the timer and the user state take
   none.
6. **T13's row.** `sched::wake` takes the home run-queue lock before it
   reads the state, with no lock-free early exit. That is a step 2 row, with
   the early exit as its control (part 3).
7. **Vector state.** The reset is made in `switch_user_state` for every task
   switched to with its state unsaved, so the frame tail and the general
   continuation are both covered (part 5). Case 15 and its control check a
   general resume.
8. **The filtered flag.** Its visibility, its inheritance by fork, `clone`,
   `process_create` and `TSYNC`, its tracer and its other interceptions are
   owed by the landings that bring filters and tracing (T2's note, part 8).
   Until then T2 reads the boot probe.
9. **An exhaustive model.** A `loom` model of park, commit, writer, close,
   the kill bit, the un-park and T13's look, with TLA+ as the fallback,
   checking the allowed sets and liveness. It has three controls and 5 to 8
   points (part 6).
10. **Throttle and freeze.** Part 1 now says "no throttled state" is true of
    `main` only. cgctl's `cpu.max` and `cgroup.freeze` must bring their test
    into the direct switch, or this design is reopened.
11. **The T13 hook.** A static that only stage 9 sets, cleared before init,
    and a boot check with its own FX code and control (part 6).

**The answers to part 9.**
1. Yes for DAL C, and for EAL5+ through part 8's documents. Accepting the
   residual risk is the customer's and the evaluator's.
2. Refinement, plus liveness through the bounds of T7's, T8's and T13's
   cases. The allowed sets come from the general path's requirements, not
   from observed runs (part 6).
3. Keep the reply cell.
4. Yes, subject to condition 10, and assert that neither task is the idle
   task (A4).
5. Yes, subject to condition 5. Halves are locked by side, A before B.
6. The hook, subject to condition 11. A decline count above zero is not
   evidence that T13 works.
7. Yes.
8. The customer's call. The consultant recommends off in the certified
   configuration for the first release, and on in development builds and the
   perf rows.
9. Step 4b leaves step 4. Whether it is built at all is the customer's call.
10. Yes, subject to condition 8.
11. Yes: each comes back as a design.
12. ERAPS stays in step 5 with its own review, and step 4's correctness
    must not depend on it (part 7).
13. Compile the park for x86-64 only if that touches just its own sites;
    otherwise argued coverage on Arm is accepted (part 6).

**Advisories applied.** The decline counters, and the wait queue's two
counts, are named as the exception to "a declined fast path changed
nothing" (the opening, part 2a, part 6). T10 notes that `WRITABLE` and the
`was_full` wake are unaffected. PKRU and AMX stay at reset while `CR4.PKE`
is off and AMX is not enabled (part 5). Part 7's locked operations are to be
measured before ERAPS is counted on. Step 1 has five conditions (part 5).

**The customer's calls (2026-10-02).**
- *The default of `ferrix.fastpath`:* off in the certified configuration the
  Safety Manual names, for the first certified release, as the consultant
  recommends. It is on in development builds and in the gate's perf rows. The
  certified default becomes on once one coverage run has measured the fast
  path and its vulnerability-analysis entry exists. Until then the switch is a
  configuration item, read once at boot and printed by stage 9.
- *Step 4b:* built, after step 5, under a design of its own that goes to the
  consultant first.
- *The residual risk:* accepted. That the fast path is a tested equivalence
  and not a proof is written into the Safety Manual and the vulnerability
  analysis when its code lands.

**What is left before code.** Steps 1 to 3 landed as part 5 lists, and
the requirement ids reserved. After that, the code comes back to the
consultant with the logs of every case and control in part 6, and of the
`loom` model.

#### Where step 4 stands (2026-10-03, session B)

Groundwork that needs neither 2a nor session A's files, built on branch
`step4-prep`; no fast-path code yet.
- **`ferrix.fastpath`** (part 6's boot switch, `src/kernel/src/fastpath.rs`,
  in the item ring beside `checks.rs`). Read once with `ferrix.checks`, from
  the loader's command line then the device tree's `bootargs`; `on` only for
  `ferrix.fastpath=on`, off otherwise, the certified default (the customer,
  2026-10-02). Stage 9 prints `fastpath ipc fast path for channel_write_read:
  off (by default)` and says the call takes the general path, since no fast
  path exists. L.x86_64.150.
- **The `loom` model of the park protocol** (condition 9,
  `src/tests/loom/tests/park.rs`): a park against a fast commit and a
  general writer, against a close and a commit, and against a kill; and the
  send half's last look (T13) against a kill. Each asserts part 6's
  refinement (nothing lost) and liveness (nothing stranded). Three
  model-only controls, each failing as it must: a general writer that
  leaves the record, the park without its fence, and T13 read outside the
  run-queue lock. Bound 3, under a second; 15 models with 2c's and 2e's.
- **`ipc-equiv`** (part 6, `src/user/system/native/ipc-equiv`), a native
  program pair, and **`cargo xtask test-ipc-equiv`**, which boots it with the
  switch off and, on x86-64, on, and requires the two transcripts to be the
  same. Built and passing on the general path: cases 1, 2, 3, 5 (before and
  during), 6 (a waiter killed, a peer ending during a trip), 8 (the port), 12
  (the refusals) and 16. Owed, and printed as owed in the transcript: 4, 5c
  and 6c (native programs have no threads), 7 (no signals), 9 and 10 (no
  affinity call), 11 and 14 (kernel checks: the probe and the T13 hook), 13
  (the barrier counters are the kernel's) and 15 (needs 3a). Those that are
  the kernel's come as stage-9 cases with the fast path; the rest need a
  native thread, signal or affinity call first.

#### Step 4 as built (2026-10-06, po7-ipc4): the code, and where it differs from parts 1 to 8

Built by po7-ipc4 on branch `po7/step4` with `step4-prep`'s groundwork;
taken over by po8 at po7-ipc4's usage limit and landed from `po8/step4`, on
`main` 80f32db1f, which holds 2f, 3a and 3b, ERAPS and the vector reset.
Behind `ferrix.fastpath=on`, x86-64 only. **Code review: OK IF**
(consultant, 2026-10-06, its ledger's line 404). Its three blocking
conditions are met in the landing: a declined call lowers `IN_CALL` (item 13),
the timer's skip and stop are stated and checked (item 11), and this section
says what lands.

**Where the code is.**
- `src/kernel/src/sched/direct.rs`: the direct switch. `begin` takes this
  processor's run-queue lock by `try_lock_manually` and makes T11 to T13;
  `Direct::hand_over` asserts A1 and A4 and makes the queue step;
  `Direct::switch` makes A3 (FX-0503's check, `require_preemption_on`, split
  out of `schedule_from`), `switch_chosen`, `switch_to` and `finish_switch`.
  The decline counters are here too.
- `src/kernel/src/sched/mod.rs`: `switch_chosen`, split out of `choose_next`
  as part 1 says, and `nothing_due_here`, the frame tail's look.
- `src/kernel/src/sched/queue.rs`: `CpuQueue::hand_over`, `insert_at`,
  `sleeper_due`. `src/lib/kernel/sched`: `RunQueue::hand_over`, with its
  host test against `enqueue`, the rescale, `remove_curr` and `pick_next`.
- `src/kernel/src/sched/task.rs`: the reply cell (`fill_reply`, `take_reply`).
- `src/kernel/src/object/channel.rs`: the park record (`Inbox::parked`,
  x86-64 only), the general writers' take (`write`, `write_small`,
  `unread`, the close), `Endpoint::send_direct` (the send half, T6 to T10,
  the commit), `park` and `unpark` (the receive half).
- `src/kernel/src/syscall/native.rs`: `fast_write_read` (T2 to T5, the
  lookup, the continuation's choice), `continue_general` (condition 3),
  `park_for_reply` (the receive half in `receive_words`).
- `src/kernel/src/arch/x86_64/syscall.rs`: the entry's call of the fast
  path, `frame_tail`, and the entry's way out split into `leave` and
  `write_outcome`, which the general path and both continuations share.
- `src/kernel/src/trap.rs`: the `Fast` answer, the fast path's registration
  slot (T1) and the filter's quiet predicate (T2).
- Checks: `src/kernel/src/object/fast_path_check.rs` (stage 9: cases 4, 9,
  10, 11, 13, 14 and the general continuation by a message, a close and a
  kill), case 15 in `src/kernel/src/arch/x86_64/switch/check.rs` (3a's vector
  check, its sending variant), and `cargo xtask test-ipc-equiv`, which now
  also requires trips on the boot with the fast path on and every counter at
  zero on the one with it off.

**What differs from parts 1 to 8**, each for the consultant to accept or
send back:
1. **The receive half runs on the general path.** The fast entry handles the
   full trip only: a send of at most 24 bytes to a parked reader. Every
   other call declines to the general path, and there `receive_words` makes
   the receive half before its wait (`park_for_reply`): T6 to T8 under the
   caller's half lock, the record and `BLOCKED` in that hold, the `SeqCst`
   fence, the last look at `END`, the general `schedule()` -- part 2's
   receive half and the order condition 1 asks, with interrupts on, as
   `wait_trusting`'s block is made. A task parked there resumes inside the
   general path, so only a task parked by its own send's commit needs the
   general continuation. Part 2's flow -- the send half declining into a
   general send, then the receive half -- is what results; it is reached
   through the general dispatch instead of from the fast entry.
2. **T2 is the filter's own answer.** Seccomp filters exist on `main` now
   (`syscall::seccomp::check`, a thread's `filtered` flag), so T2 is not
   "no probe armed" alone. The personality registers beside its filter a
   predicate, `seccomp::quiet`, which the core reads through
   `trap::filter_quiet`: no probe armed, and either no thread is filtered
   now (a count since 2026-10-07, §9.11's cut 3; until then a flag set once
   that every boot's checks set) or the running one is not -- the reads
   `check` makes, in its order, so the two agree on any call. A filter registered without the
   predicate is never quiet. The frame tail reads it again, and takes the
   general branch when it is not quiet. Condition 8's flag on `Process`,
   with its `TSYNC` and tracer rows, stays owed by the landings that bring
   them. *Since 2026-10-07 (po9-sched, ledger line 415, H4):* the predicate
   is no registered pointer but a link-time hook, `ferrix_filter_quiet`,
   which `trap.rs` declares and `main.rs` defines as one call of
   `seccomp::quiet`; the pointer's indirect call cost about 8 ns of the
   9.4 a look took. The filter and its predicate are registered by one call,
   `trap::set_quiet_syscall_filter`, which raises the core's
   `QUIET_REGISTERED` flag only after it installed the filter, and only for
   the filter it installed: a reader sees no filter (quiet), the filter
   without the flag (never quiet), or both (the predicate's answer), never
   the flag beside a filter registered without it. `filter_quiet` reads
   both at every call.
3. **T1 is a flag.** `fastpath::init` raises the core's fast-path flag
   (`trap::set_fast_write_read`) only for `ferrix.fastpath=on`, before the
   first program, and nothing lowers it; the entry reads it (Acquire) at
   every call and, when it is raised, calls the fast path directly
   through the link-time hook `ferrix_fast_write_read`, which `main.rs`
   defines as one call of `syscall::native::fast_write_read`. Until
   2026-10-07 this was a registered pointer, a `Once`, whose unset slot was
   off. The hooks, the gate that holds them (`check-item-boundary.py`,
   `composition_root.hooks`) and the `LINK` obligation are
   `docs/certification/ITEM.md` §2's; `L.x86_64.150`, `.159` and `.161`
   state them. Measured alone, the change took a fast-mode domain-call
   from 1,248 ns to 1,208 to 1,228 ns (5 fast-mode rounds of 6, alternated
   against `main` 4c9c078cc).
4. **T11 includes the sleep slot.** A parked task files no deadline, but
   one woken early from an earlier sleep and moved may still have its sleep
   slot in a sleeper set elsewhere, state 3 of `wake_with`. Part 1's A1 would
   stop the machine there. So "holds its sleep slot" (`holds_sleep_slot`) is
   tested under the run-queue lock with T11 (and declines as T11). The run
   slot is not tested: it is held exactly while no queue holds the task, and
   A1 asserts that with the rest: blocked, not running here, not queued.
5. **T12 declines on a sleeper due.** Part 1 says the test is made after
   `wake_sleepers(now)`. Waking them and then declining would leave the
   queue changed by a declined attempt. T12 is "nothing waits, and no
   sleeper's time has come by `now`" instead, which is the same condition
   on every queue where the wake would have moved nothing, and declines
   where it would have.
6. **`hand_over`'s pieces.** `CpuQueue::hand_over` is `insert`'s charge at
   `now`, its weight for the peer (`effective_weight`, computed while the
   caller is still counted in its job, the general path's order), then the
   caller blocked and parked (`between`), then `RunQueue::hand_over`, which
   the host test holds bit for bit to `enqueue`, `set_slice_ns` to the
   rescaled slice, `remove_curr` and `pick_next` on a queue with one entity
   running and nothing waiting (200,000 random states, weights and lags at
   and past the clamp, virtual times either side of the wrap). The second
   `account` of `choose_next`, at the same `now`, changes nothing and is
   not made.
7. **The half locks are held through `hand_over`.** Part 2's commit lets
   them go before `hand_over`; here they go after it, before
   `switch_chosen`. No lock is nested that was not already: the run-queue
   lock was taken under them for T11 to T13.
8. **The counters.** Per processor, each a load and a store with
   interrupts masked (no locked operation on the trip). Two counts beyond
   T1 to T13: a half's lock held (`halves`) and this processor's run queue
   held (`queue`); and `parks`. Printed by the kernel as the shell exits
   (`fastpath counts: ...`), which `bench-ipc` and `test-ipc-equiv` print
   with their figures.
9. **The frame tail's quiet exit makes no audit.** With the self-checks on,
   the general way out asks the personality anyway when the pending-work
   word is clear (FX-0520's audit). The tail's quiet exit, taken only with
   every bit clear, does not; the general branch and every other way out
   still do.
10. **A general writer's take is counted as a wake.** It wakes the parked
    task with the same `Wake` its queue's waiters get, and counts the wait
    as one a wake ended on that queue (`waits_ended_by_a_wake`), so that
    statistic no longer differs between the paths (part 2a, row 27).
11. **Outside §9.7, found by measuring.** Each is a commit of its own, so it
    can be taken or dropped apart:
    - *The timer re-armed at every switch.* A sleeper's deadline asked for
      again at the next decision found the armed bound later than it by
      the arm's own write, and wrote the local APIC again: an exit at every
      switch while any task slept on the processor. `timer::after_from`
      keeps the deadline the one-shot was asked for (`REQUESTED`) and skips
      a request no earlier than it, which may then be served late by up to
      the time from the armed one's clock reading to the end of its write
      (for `after_from`, the decision's), as the request that wrote it
      already is;
      `timer::stop` leaves a one-shot alone, armed or fired, and stops only a
      periodic timer. On both paths. L.sched.5 states both, and stage 3's
      skipped-arm check counts the module's timer writes (`timer::WRITES`):
      one 2 ms deadline asked for twice from one clock reading, fired and
      then stopped, must write the timer once. The check now also arms with
      interrupts masked, as `timer::after` asks, which the review found it
      did not (a tick between an arm and its bound left the bound over a
      quiet timer, the likely cause of one armv7a `--smp 2` hang in that
      check).
    - *Null selectors loaded over null ones* are **not** in this landing.
      They reopen 3b's condition 8 and stay on `po7/step4-selectors` with the
      consultant's advisory reading (its ledger's line 398: S3 not met, the
      skip also covers `FS` and `GS`).
    - *The scheduler's 128-bit divisions.* `ferrix_sched` divides in 64 bits
      when the operands fit, and not at all for the unit weight, with a host
      test against the 128-bit formulas at every boundary. On both paths.
12. **The cases.** Cases 4, 9, 10, 11, 13 and 14 are stage-9 cases made by
    check processes' threads driving the entry, as `write_read_check` is,
    rather than `ipc-equiv`'s native programs, which have no threads,
    affinity or signals: each is held to the general path's result on both
    boots, and to having reached its test with the fast path on. Case 7 (a
    signal during the wait) is case 15's `SIGUSR1` variant and 3a's own
    signal case. Cases 5c and 6c (another thread's close and `execve`) are
    not made; the continuation's close and kill cases stand nearest. Case
    10's control fires as FX-0530 (the pick not the peer) rather than as a
    share below the bound, because with T12 removed the queue step falls
    back to the composed calls and the pick can choose the spinner.
13. **Changed after the list above was written**, all accepted in the code
    review: (a) the peer's `Arc` moved from its park record into the run
    queue and the caller's into its own park record, and the caller borrowed
    from the processor record (`sched::with_current`), not cloned; (c) T10
    reads a sticky `Half::observed` flag and `WaitQueue`'s listed count under
    the peer's inbox lock instead of taking the observers' and waiters'
    locks (an argument, not a check: a waiter seen late listed after the
    look); (d) the direct switch's two state changes are stores under the
    run-queue lock (`Task::set_state_from`), sound because every waker takes
    the home run-queue lock first (condition 6); (e) the switch statistics,
    `add_runtime`, `swap_in_call`, `take_sleep_deadline` and the timer's
    `INTERVAL` without read-modify-writes; (f) `RunQueue::normalize`'s
    one-entity case written down; (g) `timer::after_from` takes the
    decision's clock reading; (h) `Direct::hand_over` makes no
    `remove_sleeper`, T11 having proved the sleep slot held; (i)
    `IN_CALL` raised at the fast entry and lowered at the tail's quiet exit,
    **and lowered by every decline**, since a declined call the filter
    answers leaves without `call_left` (the review's B1; case 11 asserts it
    down after both its calls, L.object.169); (j) `sched::work::arm` refuses
    a hook another check still holds (FX-0908); (k) `arch::FAST_WRITE_READ`
    in place of generic cfgs; (l) stage 9's "a queued write takes the park"
    case for A2. Cases 13 and 14 were brought to ERAPS (no in-domain refill
    is wanted where `refill_wanted_in_domain` says so) and to TCG (case 14
    makes its call again until T13 saw it).
14. **The task's slots are atomic cells (2026-10-07, po9-sched; po9-cert's
    S1-S7, ledger line 415).** `Task::run_slot` and `sleep_slot` were each a
    `SpinLock<Option<TaskSlot>>`, a lock pair of about 5 ns per take, give
    back or look, and a fast trip made three a direction: T11's
    `holds_sleep_slot`, the peer's `take_run_slot`, and the caller's
    `return_run_slot`. Each is now `SlotCell`, one `AtomicPtr` to the node.
    A take is a swap with null. A give-back is a compare-exchange from null,
    which stops the machine with FX-0534 if the cell is not empty. A look is
    one load. The task's drop frees a node the cell holds. Each operation
    is a single atomic and so linearisable (`L.sched.63`). The give-back
    was first proposed as a load and a store; `loom` 0.7.2 lost a node
    given back that way against a concurrent swap, so it is the
    compare-exchange, which also makes FX-0534's test exact. `ferrix_sched`
    stays free of `unsafe`: `Slot::into_box` and `from_box` are safe, and
    `Node` is public but opaque. The kernel's `Box::into_raw`/`from_raw`
    are traced `KMEM`. Nothing is allocated (F-23).

    *Every site, and who owns the slot there (S1).* The run slot is taken
    only under the lock of the queue that will hold the task:
    - `CpuQueue::insert_at`: the wake, and `wake_sleepers`'s insert;
    - `CpuQueue::hand_over`: the peer, under this processor's lock;
    - `Zombies::push`: a dead task its queue has let go, under the reaper's
      lock.

    It is given back only by that holder, as it lets the task go:
    - `insert_at`'s refused enqueue, at once;
    - `detach_current`;
    - `release`;
    - `hand_over`, for the leaving caller and for a refused peer;
    - `Zombies::pop`.

    The sleep slot is taken only by `file_sleeper`, for the running task
    blocking on its own processor, under that queue's lock. It is given
    back only by the set that holds it: `wake_sleepers` and
    `remove_sleeper`, under that set's lock.

    So for every give-back the giver took the only copy, and no two
    give-backs of one slot can be in flight.

    *The one compound use* is `holds_slots`, two loads one after the other
    (it was two locks one after the other, no more atomic as a pair). Its
    caller, `asleep_at_home`, reads it under the home queue's lock, which
    every run-slot taker for a task homed there holds, so the run slot
    cannot change under it. The task is blocked, so nothing takes its sleep
    slot. A sleeper set elsewhere may give the sleep slot back meanwhile,
    under its own lock. That can only turn "lent" into "held", so a stale
    read declines a move it could have made, and never makes one it should
    not have. T11's `holds_sleep_slot` is the same look, made under the
    same lock.

    *Checks.* `src/tests/loom/tests/slots.rs` models the cell: two takers,
    a give-back and a look; a give-back against a take; a double give-back
    stopped. Its controls are a take as a load and a store, and a
    give-back as a plain store, and both fail as required. The
    `ferrix-sched` host test takes a slot apart and puts it together
    around an enqueue, a pick and a removal. A1's control (T12 removed, so
    the pick is not the peer) and T11's control were fired again on this
    code.

### 9.8 Steps 2 and 3: the designs (draft for the consultant)

**Reviewed (2026-10-02): OK IF.** 2d is OK to build; 2a, 2b, 2c, 2e, 2f and
3a are OK IF, with eight conditions; 3b is OK IF its write skip is dropped.
Question 5 was real and is filed as F-60, whose fix lands on its own before
2f. The conditions, the answers to the questions and what each changed are
at the end of this section, under *The consultant's review (2026-10-02) and
what changed*. Each piece below carries its change in an *After the
review* paragraph.

Nothing here is built. These are the designs of §9.5's steps 2 and 3, which
go to the certification consultant before any code, as step 4's did. They
are written against branch `ipc-step1` (read at b030f107d, while it is being
rebased) and against §9.7, whose part 5 says what step 4 needs from these
two steps and whose review put four of its conditions (4, 6, 7 and, for the
filtered flag, 8) on rows these steps own. They also answer what the
consultant asked of steps 2 to 5 in its review of step 1 (2026-10-02).

**Why these two steps first.** After step 1 a round trip inside a domain is
about 3.0 us; seL4 matched is 440 ns (§9.6). Step 4's fast path skips the
general path's machinery, but it composes the general path's own functions
(§9.7's principle), so it pays whatever those functions cost: the locks
they take, the way the running task is found, the way out, the switch's
writes and the user state. Steps 2 and 3 make those cheap for every call,
so the fast path inherits cheap pieces instead of carrying its own copies.
None of them changes what a call does.

**How each piece is given.** What changes; the invariant and why it holds,
with lock order and, where an atomic replaces a lock, the ordering per
instruction set; the requirement rows, by name only; the checks and their
negative controls; points; the saving; and what it touches in
SPECULATION.md, MEMORY-AND-TIMING, FINDINGS and the Security Target.
Savings come from §9.5's spans where those measured the piece, and are
marked *guess* otherwise. §9.5's spans were taken with stamps that inflate
every span, so they rank the costs and do not add up to the trip.

**Four corrections the walk made to §9.5.** Reading `ipc-step1` changed
four things §9.5 assumed:
- *The switch's `LEFT` lock is not on `ipc-step1`.* It belonged to §9.4
  item 5, the segment skip, which step 1 dropped. Step 3's FS and GS piece
  brings a skip back without a lock (3b).
- *`effective_weight` cannot be kept per job and recomputed only when a
  weight changes.* A task's effective weight is its base weight times, at
  each job level, that job's weight over its load (`quota::effective`), and
  a job's load changes every time one of its tasks wakes or blocks: twice a
  direction on a round trip. A cache keyed on weights alone would change the
  policy. What can be cut without changing a bit of the answer is the
  128-bit arithmetic (2f).
- *`quota::adjust` makes one locked add per state change, not one per job
  level.* It stops at the first level whose busy or idle state does not flip
  (`object/quota.rs`). On a round trip between two tasks of one job the job
  never goes idle, because the woken task joins before the caller leaves
  (§9.7 part 1), so each state change costs one add. §9.7 part 2a's last
  bullet overstates it; the deeper walk happens only when a job turns busy or
  idle.
- *A program can change its FS and GS bases without a call.* With no
  `FSGSBASE` it cannot write the bases directly, but loading a segment
  selector into `FS` or `GS` loads the base from the descriptor, and on
  processors that clear the base on a null selector, loading zero clears it.
  §9.5's "only a call changes them" is true of `arch_prctl`'s base and not
  of the register. 3b follows Linux's rule for this.

**Not in these steps.**
- *PCIDs* (§9.5 step 3's first item, 10 to 13 points). nazuna has none:
  CPUID leaf 1 ECX bit 17 is clear on the host, KVM cannot offer it, and a
  seL4 built with `KernelSupportPCID` refuses to boot (§9.6). So the item is
  deferred to hardware that has PCIDs, and nothing of it is built or
  measured here. When it comes, two notes stand. First, F6 and FX-0009: with
  os-35's lazy TLB, a processor may keep a space loaded for a kernel thread,
  so a shootdown must reach every PCID a space holds, including one held
  lazily, and tables are freed only once no processor has the space loaded,
  eagerly or lazily. Second, the advisory on step 1: the Sync wake moves
  more tasks between processors, so the allocator must flush or tag a PCID
  for the processor a space arrives on.
- *ERAPS*, global user pages and the rest of §9.6's *what seL4 does not do*:
  step 5, each with its own review.
- *AArch64 and ARMv7-A* gain from step 2, whose pieces are written for all
  three. Step 3 is x86-64 only (3a's last paragraph says why).

#### 2a: the running task by borrow

**What changes.** `sched::current()` takes the run queue's lock with
interrupts masked and clones an `Arc<Task>`: on x86-64 an interrupt save and
restore, the ticket's locked add, the `Arc`'s locked increment, and its
locked decrement when the caller drops it. A side calls it about eight
times a trip (§9.5). The per-processor record (`smp::PerCpu`) gains
`current: AtomicPtr<Task>`, a copy of `Arc::as_ptr` of the run queue's
`current`, written wherever that is written: in `choose_next`'s tail, which
§9.7 makes `switch_chosen`, and where a processor's idle task is installed.
A new `sched::with_current(|task: &Task| ...)` reads it and hands the task
to a closure by reference. `current()` stays, for callers that must keep the
task beyond the closure, and is then `with_current`'s borrow cloned into an
`Arc`, with no lock.

The hot callers move to the borrow: `native_call`, `wait_sliced` (which
still clones an `Arc` to list itself on a wait queue, but no longer takes
the lock to get it), `may_block`, `regroup_current`, `call_left`,
`needs_attention` through the personality's `thread::current`, and the
native `must_leave`, which 2e replaces outright.

**How it is read.** On x86-64, one `mov` from `GS:offset`. One instruction
cannot be split by an interrupt or a migration, so it needs no masking. On
AArch64 and ARMv7-A the record's address comes from `TPIDR_EL1` or
`TPIDRPRW` and the field is a second load, so the two are made with
interrupts masked, as `current_id` already does: a task moved between the
two loads would read another processor's task.

**The invariant.** Whenever interrupts are on, the record's `current` is
the task the run queue's `current` holds an `Arc` to. Both are written only
by this processor, with interrupts masked and its queue lock held, in one
place. Between the write and the stack switch interrupts stay masked, so
no code can read the record while it already names the incoming task and
the outgoing one still runs.

**Why the borrow cannot outlive the task** (the consultant's ask). A borrow
is used only by code running as the task: the closure runs in the task's own
context, and the reference cannot leave it, because the closure is
`for<'a> FnOnce(&'a Task) -> R` and `R` cannot name `'a`. So the question is
whether a task can be freed while its own code is running. It cannot:
1. While a task runs, its processor's run queue holds an `Arc` to it as
   `current`.
2. A task stops running only at a switch, and when it is switched to again
   it is `current` once more. A borrow held across a block is not used while
   the task is not running, and is valid again when it is.
3. A task is freed only when its last `Arc` goes. For a dead task that is
   the reaper's, and the reaper takes a task only from `ZOMBIES`, where
   `finish_switch` files it after the switch away from it for the last time.
   After that switch none of the task's code runs, so no borrow is used.
4. A task that migrates while it holds a borrow holds a reference to the
   task, not to a processor's slot, so the move does not change what it
   names.

An interrupt handler that calls `with_current` runs as the interrupted task,
and that task is `current` by (1). `sched::exit`'s comment that a task's own
`Arc` is dropped before interrupts open, or it is leaked, no longer applies:
the borrow holds nothing.

**Rows.**
- *L.sched, the running task by borrow:* while interrupts are on, the
  processor record's `current` names the task its run queue's `current`
  holds; `with_current` lends it only to a closure.
- *L.sched, `current()`:* answers the running task without the run queue's
  lock. (A changed row if one already states that it takes the lock.)

**Checks and controls.**
- Stage 5, every architecture, at two processors or more: across 10,000
  switches made by a check's tasks, at each task's resumption the borrow's
  pointer equals `Arc::as_ptr` of the queue's `current`, read under the
  lock. Control: the idle task's installation leaves the record unset, and
  the check stops the boot on its own message.
- The same check from an interrupt handler: a timer handler compares the
  two. Control: `switch_chosen` writes the record after `switch_to` instead
  of before, which leaves a window the handler sees.

**After the review (condition 1).** The record is written at every write of
the run queue's `current`, and there are three: the boot processor's
adoption of its boot context as a task in `sched::init`, each processor's
idle-task installation, and `switch_chosen`. A write of `current` anywhere
else is refused in review, and the invariant's row names the three. The
stage-5 check covers the boot write too: its control leaves the boot
adoption's record unset, and the first comparison on the boot processor
fails. The closure form and a borrow held across a block inside it are
accepted (question 1).

**Points:** 4 to 6. **Saving:** *guess* 0.1 to 0.25 us a round trip, from
about eight calls a side, each one lock, one interrupt save and two locked
`Arc` operations.

**Documents.** SPECULATION.md: none. MEMORY-AND-TIMING: none; the borrow
allocates nothing. FINDINGS: none. Security Target: ADV_TDS's scheduler
module text names the borrow and its argument.

#### 2b: the lighter `PreemptSpinLock`

**What changes, and what does not.** A `PreemptSpinLock` taken and let go
costs, on `ipc-step1`, one locked add for the ticket and, in
`preempt_disable_at` and `preempt_enable_from`, an interrupt save and
restore each, a locked add and a locked compare-exchange loop on
`PREEMPT_OFF`, and the same again on `LOCKS_HELD`: about six locked
read-modify-writes and four interrupt saves and restores (§9.5). The
ticket's add stays, because it is what makes the lock a lock. The rest
becomes plain per-processor fields:
- `PREEMPT_OFF` and `LOCKS_HELD` become one `u64` in `PerCpu`: the
  preemption count in the low half, the locks held in the high half. A lock
  adds `(1 << 32) + 1`; a `preempt_disable` by hand adds 1.
- On x86-64 the add is one `xadd` to `GS:offset` *without* a `lock` prefix.
  It returns the old value, from which the 0 → 1 transition of the locks
  held is read, to record `LOCK_SITE`. One instruction is atomic against an
  interrupt and against a migration, so no masking is needed. It is not
  atomic against another processor, and needs not be: no other processor
  writes the field.
- On AArch64 and ARMv7-A the read of the record's address and the
  load-add-store are made with interrupts masked (`msr daifset` and
  `cpsid i`), which costs a few cycles there, not the `pushf`/`popf` of
  x86-64. No exclusive pair is needed, for the same reason.
- `PREEMPT_SITE` and `LOCK_SITE` become `PerCpu` fields stored `Relaxed`:
  a plain store on all three, read by a report as a whole word.

What stays, as the consultant asked:
- **FX-0503.** `schedule_from` still stops the machine when asked to switch
  with the count raised, naming the site. `preempt_enable_from` still stops
  it for an enable that finds the count at zero: the subtraction's old value
  is checked before it is used, and the field is restored before the panic.
- **`may_block()`** still means: the scheduler runs, interrupts are on, and
  this processor's count is zero, read with the processor's number in one
  instruction on x86-64 and with interrupts masked on Arm. FX-0907's leave
  and §9.7's general continuation call it unchanged.
- **The deferred decision.** The enable that takes the count to zero makes
  the decision an interrupt asked for, if interrupts are on, as now.
- **`smp::flush_tlb_everywhere`'s test** reads the locks held from the high
  half, as it read `LOCKS_HELD`.

**Why the plain fields are enough.** Linux's `preempt_count` argument. The
field is written only by code running on its processor. Code that runs there
is either the task or an interrupt handler that interrupted it, and every
handler leaves the count as it found it, because each lock it takes it lets
go before it returns. On x86-64 the update is a single instruction, so an
interrupt comes before or after it, never inside. On Arm, interrupts are
masked across it. Either way no update is lost and none lands on another
processor's field: that is the bug `preempt_disable_at`'s comment describes,
where the processor's number was read, the task was moved, and the increment
landed on the processor it had left. One instruction, or masked interrupts,
closes that window.

**What step 4 needs** (§9.7 parts 2 and 5): `try_lock` on the
`PreemptSpinLock` exists and keeps the bookkeeping: the count is raised
before the attempt and lowered if it fails. A failed attempt with interrupts
masked never switches, because the deferred decision is made only with
interrupts on. Step 2 adds, in `ferrix_sync::SpinLock`, `try_lock_manually`
beside `lock_manually`, for the run queue (§9.7's table), and documents that
the handle table's `try_with_handles` and the wait queue's `try_lock` sit on
`PreemptSpinLock::try_lock`. With the lighter lock the try variants cost the
same as a lock that succeeds.

**New assembly.** Two instruction sequences on x86-64 (`xadd` to and from
`GS:offset`) go into `tools/common/data/asm-allowlist.json` with
`docs/ASSEMBLY.md`'s justification. The alternative, Rust with interrupts
masked around a load and a store, keeps x86-64's `pushf`/`popf` pair, which
is most of what this piece removes (question 2).

**Rows.**
- *L.sched (changed), the preemption count:* a lock raises and lowers its
  processor's count and locks held without a locked operation, atomically
  against interrupts and migration.
- *L.sched (unchanged ids, re-verified):* FX-0503 at the switch and at an
  unmatched enable; `may_block()`.
- *L.sync:* `try_lock` and `try_lock_manually` leave the count, and the
  interrupt state, as they found them when they fail.

**Checks and controls.** The existing FX-0503 controls are re-run on the new
fields: a task that blocks holding a lock stops the boot naming the line,
and an enable on the wrong processor stops it. New:
- Stage 5, two processors or more, every architecture: 100,000 lock pairs
  per task from tasks that a timer preempts and moves between processors,
  after which every processor's count reads zero. Control: on x86-64 the
  `xadd` split into a load and a store, which loses an update under the
  timer and leaves a count above zero; on Arm the masking removed, the same.
- `try_lock` on a held lock from a check task with interrupts on and masked:
  the count is back where it was, and nothing switched. Control: the failure
  path leaves the count raised, which FX-0503 then catches at the check's
  next block.

**After the review (condition 2).** The plain count is sound only if every
context that can touch it addresses the right record and cannot be split
by another context that also touches it. So the code carries three
arguments, each with a check where one can be made:
- *x86-64, every entry vector.* The `xadd` addresses `GS:offset`, so every
  path that can reach a `PreemptSpinLock` must run with the kernel's GS
  base, or take none. The entries that can arrive with the user's GS base
  live are named one by one: the NMI, `#MC` and `#DB` through the paranoid
  entry, which decides `swapgs` by reading `GS_BASE`, and an NMI that lands
  in the `SYSCALL` stub before its `swapgs`. For each, the design states
  either that it has switched to the kernel's base before any Rust runs, or
  that the handler takes no `PreemptSpinLock`. Where it is the second, a
  debug assertion in `preempt_disable_at` checks that `GS_BASE` is the
  record's own address. A boot check sends an NMI to a processor spinning in
  the stub's window and requires the count unchanged.
- *Arm, every exception that can take a lock is masked.* On AArch64 that is
  IRQ, and also SError and the pseudo-NMI where the kernel uses them: the
  sequence masks `DAIF`'s A and I bits, or the code argues that neither
  handler takes a lock. On ARMv7-A it is IRQ and FIQ: `cpsid if`, or an
  argument that the FIQ handler takes no lock.
- *ARMv7-A, a remote read may tear.* The packed `u64` is two words there. A
  read from another processor (a report, `flush_tlb_everywhere`'s test of
  another record) may see one half old and one new, and every such reader
  must give an answer that tolerates it: a report prints both halves as
  read, and no decision is taken from another processor's word.

The `xadd` sequences may join the asm allowlist under these arguments
(question 2). The saving is claimed on x86-64 only until 2b is measured on
Arm (advisory).

**Points:** 7 to 10 after the review (6 to 9 in the draft). **Saving:**
*guess* 0.3 to 0.6 us a round trip: about 14 `PreemptSpinLock`s a side (§9.5),
each losing four locked operations and, on x86-64, four interrupt saves and
restores.

**Documents.** SPECULATION.md: none. MEMORY-AND-TIMING: §2.2's masked spans
shrink: taking and letting go of a lock no longer masks interrupts on
x86-64. FINDINGS: none. Security Target: none; ADV_ARC's self-protection
argument does not rest on the count's representation.

#### 2c: the pending-work word

**What changes.** On every return to ring 3 the core asks the personality's
`needs_attention`, which takes `current()`, two `Arc` clones, the process's
`state` lock and the thread's `signals` lock (§9.5). Most returns find
nothing. `Task` gains `work: AtomicU32`, a word of pending work, and the way
out reads it, with interrupts masked, just before the stub's exit, on
every architecture and on every return to ring 3: a call's, an interrupt's
and a fault's. Only when a bit is set does it call the personality's
`needs_attention` and `return_to_user`, which are unchanged.

**The bits.** One for each thing `needs_attention` looks at, so that a clear
word means it would answer false (§9.7's condition 4):

| Bit | Means | Posted by | Under |
|---|---|---|---|
| `END` | the process is ending, or another thread's `execve` is replacing the program: every case `must_leave` answers true | `Process::end` (`exit_group`, a kill, the last thread's exit, and every other route to `end`) on every task of the process, the caller's included; `end_other_threads` on every task but the caller's; `PreparedStart::launch` and `start_thread`, for a task that starts after either | no lock: `ending`'s swap and `exec_thread`'s compare-exchange order the state, and the bit is posted after them |
| `STOP` | `is_stopped` | `enter_stop`, on every task, the caller's included | no lock: after `stopped`'s store |
| `SIGNAL` | a signal deliverable to the thread, a saved mask to put back, or a call to restart: `signal::needs_attention`'s three | a thread-directed post (`post_signal_to`, `force`) on that thread's task; a process-directed post on every task whose thread does not block the signal; `hand_on` on each thread it hands to; and the thread itself whenever it changes its own mask, saved mask or restart | the signal lock (`Process::state`, then `Thread::signals`) orders the record against the task's clear, below |
| `TRACE` | a tracer's exit stop | reserved: nothing posts it until `ptrace` exists | the landing that brings `ptrace` |
| `FILTERED` | the process is filtered (§9.7's T2 flag) | today `seccomp::arm_probe` on the probe's task, cleared by its disarm; S3's real filters reach T2 instead through the quiet predicate and its live count of filtered threads (§9.7 T2's note, §9.11's cut 3), so no bit is posted for them | the count's `AcqRel` changes against T2's `Acquire` load (ledger 457, D1) |

Two more inputs of the way out are per processor, not per task, and stay
so: the resched flag (`NEED_RESCHED`, read by `call_left`) and the regroup
word (`MOVES` against `RUNNING_SEEN`, read by `regroup_current`). The way
out, and step 4's frame tail, read both beside the word. 2f makes each read
take no locked operation when nothing is set.

**One way to post, so "set before wake" holds by construction.** A new core
function, `sched::notify(task, bits)`, does `work.fetch_or(bits, Release)`,
then `sched::wake(task)`, then `sched::interrupt(task)`. Every
`sched::wake(task); sched::interrupt(task)` pair in the personality (in
`wake_other_tasks`, `wake_thread`, `launch` and `start_thread`) becomes a
`notify` with the bit its caller posts. A task posting to itself (an
`exit_group`, its own mask) needs no wake and uses `sched::post_own(bits)`,
which is the `fetch_or` alone. The word is private to `sched::work`, a new
file, and nothing else writes it.

**The clear.** Only the task clears its own bits, and only before it reads
the state they stand for. The personality's `needs_attention` begins with
`work.fetch_and(!(STOP | SIGNAL), Acquire)`, then reads the state as it does
now. `END` is never cleared: a process that is ending stays ending, and a
thread told to leave by an `execve` leaves. `FILTERED` is cleared only by
the probe's disarm, and for real filters never (one-way, condition 8).

**Why no posted work is missed.** The poster writes the state (C), then
posts the bit (D, a `Release` read-modify-write). The task clears the bit
(A, an `Acquire` read-modify-write), then reads the state (B). A and D are
read-modify-writes of one word, so one of them comes first in its
modification order:
- *D before A.* A reads D's value, so A synchronises with D, C happens
  before B, and B sees the state.
- *A before D.* D's bit survives A, so the word is set, and the task's next
  look at the word (the masked look at the end of `return_to_user`'s loop,
  or its next way out) finds it.

This holds in the language's model, without fences, because both sides are
read-modify-writes on one location. For `SIGNAL` the signal lock gives the
same order a second way: the poster records the signal under it, and the
task reads the deliverable set under it after A. The instructions Rust
emits, per instruction set:
- x86-64: `lock or` and `lock and`, and a plain `mov` for the way out's load,
  which is `Acquire` on x86-64's ordering.
- AArch64: `LDSETL` and `LDCLRA` with the large-system extensions, or
  `LDXR`/`STLXR` and `LDAXR`/`STXR` loops without them; `LDAR` for the load.
- ARMv7-A: `LDREX`/`STREX` loops with `DMB ISH` before (release) and after
  (acquire); a load followed by `DMB ISH`.

**Why the bit reaches a task that is about to leave.** A task whose way out
read its word just before the poster's D runs in user mode with work
pending. That is the case `sched::interrupt` exists for: `notify` kicks the
task's processor after the bit, and the kick's interrupt is taken the moment
the task is back in user mode, where the interrupt's way out reads the word.
The kick's own ordering is §8's and unchanged (`kick`, `KICK_PENDING`).

**The wake row** (condition 6, T13's). `sched::wake` and `wake_with` read a
task's state only under the run-queue lock of the processor that owns it,
after re-reading `task.cpu()` under that lock, with no lock-free exit before
it: `wake_at_home` and `wake_onto` already do this on `ipc-step1`. The row
makes it a requirement, so that step 4's T13, which reads the word under that
lock with no fence, can rely on it. The general path does not need the row:
`wait_trusting` orders a kill against its last look with a fence pair, so an
early exit would not lose a general wake. That is why the row needs a check
of its own (below), not one of the general path's.

**A behaviour this must keep.** Today a process-directed signal is taken by
whichever thread that does not block it next comes back through the kernel,
because every thread's way out reads the process's pending set. Posting
`SIGNAL` only to the taker would make the other threads skip the look
(Linux's `complete_signal` works that way). The design posts the bit to
every task whose thread does not block the signal, from the thread list
`Process::post` already walks, and wakes only the taker as now. So every
thread that would have taken the signal still looks, and the change is
invisible to programs (question 3).

**Where the posters live.** Every poster is in the Linux personality
(`syscall/process.rs`, `signal.rs`, `kill.rs`, `seccomp.rs`), in the `load`
ring. The core owns the word, `notify` and the way out; the personality owns
when a bit is due. This is the shape `leave_speculation_domain` and §9.7's
filtered flag already have: a one-way downward interface. What the core
relies on is the personality posting a bit whenever it makes
`needs_attention` true. A missed post delays a stop, a signal or an end of
the personality's own process; it reaches no other process's memory, so no
isolation property depends on it. Step 4's T13 relies on `END` for the same
reason, and its failure would be a trip that runs one exchange after an end,
which the next way out then ends (question 4).

**Rows.**
- *L.sched, the word:* a task's way out to ring 3, on every architecture and
  from every entry, calls the personality's attention path exactly when its
  word, its processor's resched flag or the regroup word is set, and reads
  them with interrupts masked.
- *L.sched, `notify`:* every bit a waker posts is set before the target is
  woken and its processor kicked; only the task clears its own bits, and only
  before reading their state.
- *L.sched, the wake row* (condition 6): a wake reads its target's state
  only under the target's home run-queue lock, re-reading the home under it.
- *L.syscall, one per bit,* each naming its posters: `END` for every case
  `must_leave` is true; `STOP` for `is_stopped`; `SIGNAL` for a deliverable
  signal, a saved mask or a restart, including the thread's own changes of
  each; `TRACE` reserved; `FILTERED` owed by S3.
- *Reused:* step 1's wait rows (L.object.126 to 130), whose fences stay.

**Checks and controls.** One control per bit and one per rule, each a
stage-9 case that stops the boot on its own message, every architecture, at
two processors:
1. *`END`*: a thread spinning in user mode, and one blocked in a native wait,
   in a process killed from another; each leaves within 100 ms. Control:
   `end` posts no `END`: the spinner runs past the bound.
2. *`END`, `execve`*: a second thread spinning while the first calls
   `execve`; the `execve` completes within the bound. Control:
   `end_other_threads` posts nothing: the `execve` waits past the bound.
3. *`STOP`*: `SIGSTOP` to a process of two spinning threads; both stop
   (FX-0701's own check). Control: `enter_stop` posts nothing to the second
   task.
4. *`SIGNAL`, thread-directed*: `tgkill` to a spinner; its handler runs
   within the bound. Control: `post_signal_to` posts nothing.
5. *`SIGNAL`, process-directed, to a non-taker*: a signal pending for the
   process with the taker blocked in a long wait and a second thread
   spinning; the spinner takes it. Control: post to the taker only.
6. *`SIGNAL`, own mask*: `sigprocmask` unblocking a pending signal; the
   handler runs before `sigprocmask` returns to the program. Control: the
   unblock posts nothing.
7. *`SIGNAL`, saved mask*: `ppoll` with a signal mask that returns on its
   timeout; the program's mask afterwards is its own. Control:
   `suspend_with` posts nothing, and the temporary mask stays.
8. *`SIGNAL`, restart*: ferrix-ea's case, a process-directed signal waking
   one thread's restartable wait and taken by another; the first thread's
   call restarts and never returns `-512`. Control: `mark_restart` posts
   nothing.
9. *The clear's order*: a host model with `loom` of the poster's C then D
   against the task's A then B, checking that the task either reads the state
   or keeps the bit. Control: the clear made after the state's read.
10. *The wake row*: a check-only blocker that does what T13 will do. It
    takes its home run-queue lock, reads its `END` bit, signals a poster on
    another processor and waits up to 1 ms for it, marks itself blocked and
    switches out, all in one hold of the lock. The poster posts `END` and
    calls `sched::wake`. With the row kept, the poster waits for the lock,
    finds the blocker blocked, and wakes it. Control: a lock-free exit in
    `wake_at_home` that returns when the state reads runnable; the blocker
    stays blocked past the bound. The blocker is reached through a hook with
    §9.7 condition 11's rules: a static set only by stage 9, cleared before
    init, and a boot check, with its own control, that it is clear after.
    Step 4's case 14 reuses it.
11. *`FILTERED`*: the probe armed on a task sets the bit, disarmed clears it.
    Control: `arm_probe` posts nothing, and a check that reads the bit
    beside `PROBE_TASK` fails.

**After the review (conditions 3 and 4).**

*Termination's `END` is the core's* (condition 3, question 4). The core
stores `terminated` in `object::process::Exit::record`; it posts `END` there
too, on every task of the process, after the store. The core's `Process` has
no list of its tasks today, so the personality's `tasks` list (`Weak<Task>`
per task, already used for every wake) moves down into the core's
`Process`, and the personality's `wake_other_tasks` reads it from there. The
list's push already reports failure (F-23), and the move changes nothing
there. `launch` and `start_thread` keep their re-check after the push, now
for `END` posted by the core. The personality keeps the rest as the stated
interface: `execve`'s `END` (`end_other_threads`), `STOP` and `SIGNAL`.

*The bit table is checked, not only argued* (condition 4). In check mode
(`ferrix.checks`) the way out calls the personality's `needs_attention`
even when the word is clear, and stops the machine with a new FX code if
that answers true: the word said nothing was pending, and something was.
That check runs across the five boots, `test-shell`, `test-threads`,
`test-vfs` and `test-init`. Its control removes one poster (`enter_stop`'s
post to the other tasks) and must fire the FX code in `test-threads`.

*The walk: every input `needs_attention` reads, every writer of each, and
the bit each posts.* A writer that can only make the answer false (a clear,
a block) posts nothing; one that can make it true posts.

| Input read | Writer | Can make it true? | Posts |
|---|---|---|---|
| `Exit::terminated` (`is_terminated`, in `must_leave`) | `Exit::record`, from the personality's `Process::end` (every route: `exit_group`, a kill, a last thread's `exit`) | yes | `END`, by the core, every task |
| `Process::exec_thread` (in `must_leave`) | `end_other_threads`, its compare-exchange | yes, for every thread but the caller | `END`, every task but the caller's |
| | `end_other_threads`, its clearing store | no: the other threads have left | none |
| `Process::stopped` (`is_stopped`) | `enter_stop` | yes | `STOP`, every task, the caller's by `post_own` |
| | `leave_stop` | no | none |
| `Signals::shared.pending` (deliverable) | `Process::post`, through `post_signal` (kill, `tell_parent`, timers, a broken pipe to the process) | yes | `SIGNAL`, every task whose thread does not block it |
| | `cancel`, `take_next`, `reset_for_exec` | no | none |
| `ThreadSignals::private.pending` (deliverable) | `ThreadSignals::post`, through `post_signal_to` (`tkill`, `tgkill`), and `force` (a fault) | yes | `SIGNAL`, that thread's task (`post_own` for a fault on itself) |
| | `discard`, `cancel`, `take_next` | no | none |
| `ThreadSignals::blocked` (deliverable) | `rt_sigprocmask`, `leave_handler` (`rt_sigreturn`), `suspend_with`, `restore_saved_mask`, `force`'s unblock | yes, by unblocking | `SIGNAL`, own task |
| | a handler's mask added at delivery (`act`) | no | none |
| | `hand_on_newly_blocked`, which hands a process signal to another thread | yes, for the thread it hands to | `SIGNAL`, through `hand_on` |
| `ThreadSignals::saved_mask` | `suspend_with` (`rt_sigsuspend`, `ppoll`, `pselect6`, `epoll_pwait`) | yes | `SIGNAL`, own task |
| | `restore_saved_mask`, a handler frame that takes it | no | none |
| `ThreadSignals::restart` | `mark_restart` (`syscall/linux.rs`) | yes | `SIGNAL`, own task |
| | `take_restart` | no | none |

The walk is made again on the code when 2c is built, and every writer it
finds that this table misses is a defect of the design, not of the code.
The check-mode FX code is the walk's backstop: a writer missed here makes a
case answer true with the word clear. A process-directed signal keeps
today's behaviour, every thread that does not block it (question 3).
`TRACE` stays reserved, and `FILTERED` waits on seccomp S3's install lock
(advisories). The wake row's check and step 4's case 14 share one hook
static, which records which check armed it; the boot check after stage 9
verifies that every check's mark is clear (question 14).

**Points:** 11 to 15 after the review, for the core's `END`, the task list's
move, the check mode and the walk; in the draft 9 to 13: the word and `notify`
(2), the posters (3 to 4), the way out on three architectures (1 to 2), eleven
cases and controls with the hook and the `loom` model (3 to 5).

**Saving:** *guess* 0.1 to 0.2 us a round trip: per direction two `Arc`
clones, two `PreemptSpinLock`s and a `current()` on the way out, now one
load.

**Documents.** SPECULATION.md: none. MEMORY-AND-TIMING: the way out's masked
look is one load and its span shrinks. FINDINGS: F-23, no new allocation:
the process-directed post walks the thread list `post` already allocates.
Security Target: ADV_TDS describes the word as a module of the scheduler,
with the personality's obligation to post as an interface the item states;
the Safety Manual's description of the personality boundary names it.

**As built (session B, branch `step2b`, 2026-10-03).** Built to the
design above with the review's conditions, for its code review. What
differs from the draft, and what the walk on the code found:
- *The clear is the core's.* The way out's look (`sched::work::look`, from
  `trap::attention_due`) clears `STOP` and `SIGNAL` as it finds them, and
  the personality's `needs_attention` reads the state only. Same order
  argument (A then B), one place; the last look of `return_to_user` goes
  through `attention_due` too.
- *A process-directed signal posts `SIGNAL` with its record*, in
  `Process::post`, to every thread that does not block it (question 3), not
  in `notify_signal`. The audit's first boot stopped on FX-0520 for a writer
  the table above did not have: stage 7's hand-off check records a signal
  with `post_signal` and wakes only another thread; with the post beside the
  record, every caller is covered.
- *A new task is told what was posted before it was listed.* Every start
  lists its task before it runs (`Process::list_task`, fallible, F-23), and
  after the launch posts `END` if the process is ending or being replaced,
  and `SIGNAL` if its thread can already take a pending signal. A thread
  started into a pending process-directed signal was the second writer the
  table missed.
- *`STOP` to the caller* only when the caller is one of the process's
  threads; a kernel caller (a check) gets none.
- *Termination's `END` is the core's* (condition 3): `Process::end_record`
  stores the end, fences, and posts `END` through `post_to_tasks`, which
  takes the core's task list a batch of eight at a time onto its stack, so
  nothing is allocated or let go under the list's lock. `Exit::record` is
  private to it.
- *The bit table checked* (condition 4): `sched::work::audit` at every clear
  word with the self-checks on, FX-0520, with every poster's write and post
  bracketed in a `Posting` so that a look between them is not a miss.
- *`TRACE` and `FILTERED` are reserved bits*; nothing posts either, and the
  way out does not read `FILTERED`. Case 11 is not built.
- *The cases.* One per bit, each with its control: `END` by the core, the
  OOM kill of stage 13's cgroup check, which no signal reaches first;
  `execve`'s `END`, a new stage-7 case (two threads of the stop program
  counting while the first replaces the program); `STOP`, the stop check run
  a second time with the stop alone; `SIGNAL`, stage 7's hand-off check.
  The wake row's case 10 (`sched::work::check`, the `wakerow` line) and its
  hook, required disarmed by F-60's boot check (FX-0908), which now asks
  both hooks; each keeps its own static. Cases 4 to 8 are not separate cases: the audit covers them
  on every boot and test. The `loom` model (case 9) is owed: `loom` is not a
  dependency of the tree, and adding one is a change of its own.
- *Without 2a*, the look takes the run-queue lock for its one load, so no
  saving is claimed for 2c before 2a lands.
- *The delivery cap* (the consultant's condition 2 on landing):
  `return_to_user` acts on at most 65 signals a pass, and the look that
  brought the thread there cleared `SIGNAL`, so a pass that ends on the cap
  posts it again. A thread can have 70 due: one per number in the
  process's set and one in its own. The `capped` line takes 70 ignored ones
  in one way back.
- *The walk's restarts are bounded*: a task listed while `post_to_tasks`
  walks is started by a thread of the ending process, which has `END`
  itself and leaves instead of starting more.

#### 2d: the native call decoded once

**What changes.** x86-64's `answer_here` decodes every call against the Linux
table twice, once for `arch_prctl` and once for `rt_sigreturn`, each a clamp
and a jump-table `match`, before a native call reaches `native_call`. The
native range is tested first instead (`ferrix_native_abi::nr::is_native`, a
compare), and a native number skips `answer_here` altogether; a Linux
number is decoded once and matched against both. `dispatch_with` already
dispatches a native call before any Linux table (`syscall/mod.rs`), so
that part of §9.5 stands as built. `native_call` gets the caller through
2a's borrow.

**The invariant.** A native number never indexes the Linux table, and a
Linux number is decoded once behind its clamp. SPECULATION.md §2's two rows
for the decode are unchanged: the Linux number is clamped before the table,
and the native number before `native.rs`'s own `decode`.

**Rows.** *L.x86_64 (changed):* the `SYSCALL` entry answers `arch_prctl` and
`rt_sigreturn` from one decode, and passes a native number to the dispatcher
without decoding it against the Linux table.

**Checks and controls.** The existing boot check that every number up to 600
answers `ENOSYS` or its call, and the `arch_prctl` and `rt_sigreturn` cases,
run unchanged. New: a native number whose low bits equal `arch_prctl`'s
Linux number reaches `native_call`. Control: the native test removed from
`answer_here`, which answers it as `arch_prctl`.

**Points:** 1 to 2. **Saving:** *guess* 10 to 30 ns a round trip.

**Documents.** SPECULATION.md §2: the decode rows say where the native test
now runs. MEMORY-AND-TIMING, FINDINGS, Security Target: none.

#### 2e: the channel's wait from one state word

**What changes.** `receive_words`'s wait asks `readable_or_closed`, which
takes the caller's inbox lock, and `must_leave` twice, each a call through
the registered `Processes` into the personality, which finds the thread
through `sched::current()` (§9.5: three times in one wait). Two words
replace them:
- `Half` gains `state: AtomicU8`, with `NONEMPTY` (its inbox holds a message)
  and `PEER_CLOSED` (the other end has gone). `NONEMPTY` is written under the
  inbox lock by every operation that changes whether the inbox is empty:
  `push`, `put_small`, `pop`, `pop_small`, `unpop`, and the close's drain.
  `PEER_CLOSED` is set once, by the closer, on the reader's half, where
  `closed` is stored now.
- `must_leave` becomes the caller's `END` bit (2c), read by borrow (2a).

`ready()` is then two loads: the half's word and the caller's `END`. The
inbox lock and the personality call leave the wait.

**The invariant.** Under the inbox lock, `NONEMPTY` is set exactly when the
inbox is not empty. `PEER_CLOSED` is set exactly when the peer's `closed` is.

**Why no wake is lost**, which is step 1's condition 1 argued again, because
`ready()` no longer takes the inbox lock:
- *A message.* The writer queues under the inbox lock and sets `NONEMPTY`,
  lets the lock go, then takes the wait queue's lock to wake. The waiter
  takes the wait queue's lock to list itself, then marks itself blocked,
  then reads the word. Either the writer's wake section comes after the
  waiter's listing, and finds it listed, or it comes before, and then the
  word's store happens before the writer's release of the wait queue's lock,
  which happens before the waiter's acquire of it, so the waiter's read sees
  `NONEMPTY`. The order is carried by the wait queue's lock, where it was
  carried by the inbox lock.
- *A close.* The closer stores `PEER_CLOSED` (with `Release`), then wakes the
  reader's wait queue under its lock: the same argument.
- *A kill or an `execve`.* Unchanged from step 1: the `SeqCst` fence after
  `set_state(BLOCKED)` in `wait_sliced`, paired with the fence in
  `wake_other_tasks` before it reads task states. `END` is posted before
  that fence's wake (2c), so a waiter whose last look misses `END` is found
  blocked.

Step 4's park reads neither: its T6 and T8 read the inbox and `closed` under
the halves' locks (§9.7 part 2). Its A2 ("the peer's inbox is empty") may
read `NONEMPTY` under the peer's lock instead of the inbox, which the
invariant makes equal.

**Rows.**
- *L.object (changed: step 1's wait row, among L.object.126 to 130):* the
  wait reads readiness from the half's state word and the caller's `END`
  bit; message and close are ordered by the wait queue's lock, kill and
  `execve` by the fence pair.
- *L.object:* the half's word equals the inbox's emptiness and the peer's
  close under the inbox lock.

**Checks and controls.**
- A host test of `Inbox` and `Half` drives every operation that changes the
  inbox, in random orders, and compares the word with the inbox after each.
  Control: `unpop` leaves the word as it was.
- Step 1's wait case 2 (a waiter woken by its peer's close within 10 s) and
  its message case run unchanged. Control: the close sets `PEER_CLOSED` after
  the wake instead of before; the waiter stays blocked past the bound.
- 2c's case 1 covers the kill.

**After the review (condition 5).** The argument above misses one window:
the waiter is listed on the wait queue but has not yet stored `BLOCKED`.
The writer's wake, under the wait queue's lock, drains it and reads its
state; if the state still reads runnable, the wake does nothing, which is
right only if the waiter's later look sees the word. The waiter stores
`BLOCKED`, then its `SeqCst` fence, then reads the word. The writer stores
the word, then reads the waiter's state in the wake. That is a store then a
load on each side, so the writer and the closer gain a `SeqCst` fence
between the word's store and the wake's read of the state, paired with the
wait's fence: of the two fences one comes first, and the side whose fence
comes second sees the other's store. The fence is the same pairing step 1
already uses for a kill (`wake_other_tasks`). The case is added to the
`loom` model of 2c's case 9 (writer, closer and waiter, with the listing,
the state, the word and both fences), with a control that exists only in the
model and drops the writer's fence, which must find the lost wake.

**Points:** 4 to 6 after the review (3 to 5 in the draft). **Saving:** *guess*
0.05 to 0.15 us a round trip: per direction one inbox lock and two or three
calls through the personality, each with a `current()`.

**Documents.** SPECULATION.md: none. MEMORY-AND-TIMING: none. FINDINGS: none.
Security Target: none; the change is inside the channel's module.

**As built (session B, branch `step2e`, 2026-10-03).** Built to the design
and condition 5, for the consultant's code review:
- `Half::state` holds `NONEMPTY` and `PEER_CLOSED`, written only under the
  half's inbox lock (`Half::note` after every operation that changes the
  inbox's emptiness, `note_peer_closed` in the close). `readable_or_closed`
  is one load of it, and `receive_words`' wait reads the caller's `END`
  (`sched::work::own_end`) in place of the personality's `must_leave`.
- Condition 5: `write`, `write_small`, `unread` and the close each make a
  `SeqCst` fence after the word and before their wake. One more fence the
  walk found: `END` is now read by the wait itself, and its poster
  (`post_to_tasks`, `notify`) stored it after the kill's fence, so
  `sched::work::wake_posted` makes a `SeqCst` fence after the post and
  before the wake reads the state.
- *Without 2a*, `own_end` takes the run-queue lock to find the task, so of
  the guessed saving only the inbox lock and the personality call go.
- The checks: the `chword` boot line drives 4096 operations on one pair and
  compares each end's word with its inbox (a boot check, since `Inbox` is
  the kernel's), each operation first once across the empty boundary, and
  the `wrread` line's cases run unchanged. Controls, each firing its
  check's own message: the put-back's update removed ("disagreed after a
  put-back into an empty inbox"); the close's mark removed (wait case 2,
  not woken by the close); the wait reading no `END` (wait case 3, not
  woken by the kill). The design's close control, the mark moved after
  the wake, did not fire: the woken waiter's next look comes a switch
  later, after the mark, so the boot cannot hit that window; the loom
  model's control is what shows its order.
- The `loom` model, `src/tests/loom` (`cargo xtask loom`, and a step of
  `check`): 2c's case 9 with its control (the clear after the read), and
  condition 5's waiter listed and not yet blocked against a writer, a
  closer, both, and an end, with the model-only control the consultant
  asked for, the writer without its fence, and, at its review, one for the
  mark's place: a closer that marks after its wake, the design's close
  control moved where it can fire. All three controls must fail, and do
  (`should_panic` on their message): eight models, under a preemption
  bound of 3, in about a second. Each names the kernel sites it restates;
  that it matches them is by review (TOOLS.md §3).

#### 2f: no global or locked writes in the switch

**What changes.** The switch on `ipc-step1` makes these locked operations,
beyond the run queue's ticket and the `CpuMask` join and leave, which stay
because a shootdown reads the mask:
- `set_idle`'s `SeqCst` `fetch_and` on the global `IDLE`, at every switch;
- `entered_space`'s three swaps: `ENTERING_DOMAIN`, `LAST_DOMAIN` (both
  `SeqCst`) and `LAST_ROOT`;
- per-processor counters made with `fetch_add`: `REFILLS_IN_DOMAIN` at every
  in-domain switch, `BARRIER_DECISIONS` at every barrier;
- four `Arc` clones in `choose_next`: `previous`, `queue.previous`,
  `queue.current`, and the pick's;
- `take_resched`'s swap and `regroup_current`'s swap on every way out, even
  when nothing is set;
- `effective_weight`'s 128-bit divisions, one per job level, at each wake.

Each becomes a plain per-processor store, or goes:
1. *`IDLE` only on idle entry and exit.* The queue keeps `marked_idle: bool`
   under its lock. `set_idle` writes `IDLE` only when the wanted value
   differs, which happens only at a switch to or from the idle task. The
   global word's value is the same at every instant as now; only its no-op
   writes go.
2. *`entered_space` by loads and stores.* `ENTERING_DOMAIN` is written and
   read only by its own processor with interrupts masked: a load and a
   `Relaxed` store of zero. `LAST_ROOT` and `LAST_DOMAIN` are read or
   written by other processors too (`forget_root` and `leaving_domain`), so
   each becomes a single-copy-atomic load and store, not a read-modify-write.
   The argument is below.
3. *Per-processor counters* become a load and a store, since only their own
   processor writes them, with interrupts masked. A reader on another
   processor reads a whole word, as now.
4. *`Arc` moves instead of clones.* `queue.previous` takes the old
   `queue.current` by move, and `queue.current` takes the pick by move; the
   code between reads both by reference.
5. *The way out's flags.* `take_resched` loads the flag and swaps only when
   it is set; `regroup_current` compares `MOVES` with `RUNNING_SEEN` by two
   loads and swaps only when they differ.
6. *`effective_weight` in 64 bits.* At each level the weight so far times the
   job's weight is at most the product of two entity weights, because the
   weight so far never exceeds the weight of the level below: by induction,
   with `load ≥ below` as `quota::effective` clamps it, `weight_k ≤ own_k`.
   So while every entity weight is below 2^32, which the weight setters
   assert, the product fits in 64 bits, and a 64-bit division gives the
   same quotient as the 128-bit one. The answer is
   unchanged bit for bit. That is the honest form of §9.5's "kept per job"
   (the second correction above).

**Why `LAST_ROOT` and `LAST_DOMAIN` need no read-modify-write.**
- *`forget_root(root)`* runs before `root` is ever installed and must leave
  no processor's `LAST_ROOT` equal to it. This processor's install stores
  only the root it is installing, which cannot be `root` before
  `forget_root` returns. So a store that overwrites `forget_root`'s clear
  writes a value other than `root`, which is all the rule asks. If this
  processor read the old value just before the clear, its decision compares
  against a root that no space is using; the stored value is still its own.
- *`LAST_DOMAIN`* is written locally by `left_space`, `entered_space` and
  `serve_wanted_barrier`, cleared remotely by `forget_root`, and read
  remotely by `leaving_domain`. A swap was never what ordered a local write
  against `leaving_domain`'s read. The read and the write are each a single
  access in both versions, and the walk found that the pattern is a read of
  the space's domain then a write of `LAST_DOMAIN` on one side, and a write
  of the space's domain then a read of `LAST_DOMAIN` on the other. That
  pattern allows, even with every access `SeqCst`, a processor that read
  the space's domain before the leave stored `OUT` and wrote `LAST_DOMAIN`
  after the leaver read it. So the stores here can be `Relaxed` without
  weakening anything F1 has. But F1 itself may have this window (question
  5): a processor installing a member's space as the member leaves may
  record the domain where the leaver's scan does not see it, and the grace
  period's interrupt then finds no barrier wanted. The fix proposed is local:
  `leaving_domain` publishes the domain it leaves in a global word before
  its scan, and `answer_grace` (or `serve_wanted_barrier`), which each
  processor runs with interrupts masked and therefore after any switch in
  progress, compares its own `LAST_DOMAIN` with it and issues the barrier
  itself. The remote scan then only saves an interrupt. This is a change to
  landed code and is not in 2f; it is reported for its own landing.

**Ordering per instruction set.** A `Relaxed` load and store are a plain
`mov`, `LDR`/`STR` and `LDR`/`STR` on the three, with no barrier. A `Release`
store is a `mov` on x86-64, `STLR` on AArch64 and `DMB ISH; STR` on
ARMv7-A. None is locked. The full barrier a `SeqCst` read-modify-write made
at every switch goes. The walk found no argument in the scheduler that names
it: `IDLE`'s protocol needs its fence on the set side (the idle loop's set
before its look) and in `wake_idle_processors`, and both stay; step 1's
wait argument uses its own fences. A `mov cr3` at a switch between spaces
is serialising on x86-64 in any case. The consultant is asked to confirm
(question 6).

**Rows.**
- *L.sched:* `IDLE` is written only when a processor's idle state changes.
- *L.object.113 with L.x86_64.126, L.aarch64.52 and L.armv7a.3 (unchanged
  text, re-verified):* the barrier decision, now made from per-processor
  loads and stores; the rule they state is the same.
- *L.object (quota):* `effective` in 64-bit arithmetic equals the 128-bit
  formula over every weight a job or task can have (host test).

**Checks and controls.**
- The speculation domain's check (§9.3, cases 1 to 10) runs unchanged; it
  counts the decisions, which must not move. Control: `LAST_DOMAIN`'s store
  after `entered_space`'s compare instead of before it is read, which makes
  case 2 skip a barrier.
- `this_cpu_reads_as_idle`'s check (a processor running a task never reads
  as idle) at two processors under 10,000 switches. Control: `set_idle`
  skips the write at a switch away from the idle task, and the check fires.
- A host test of `quota::effective` against the 128-bit formula: random and
  boundary weights and loads, depths 1 to 8. Control: one division left at
  `u32`, which overflows and differs.
- The way-out flags: 2c's cases with a resched pending and a move pending.
  Control: `take_resched`'s load reads the wrong processor's flag.

**After the review (condition 6, F-60, question 6).** F-60's fix lands on
its own before 2f, under its four conditions (FINDINGS.md, F-60). With it,
`LAST_DOMAIN`'s ordering no longer carries the leave's argument, and the
stores here may be plain. The consultant names the two places a full barrier
in the switch was in fact relied on: F-60's installing side, which the fix
replaces, and `regroup_current`'s `MOVES` against `RUNNING_SEEN`. Every
remote reader of a word the switch writes, and the ordering it relies on:

| Word the switch writes | Remote reader | Ordering it relies on |
|---|---|---|
| `IDLE` | `wake_idle_processors`, `machine_is_quiet` | the set side's fence before the idle loop's look, and the waker's fence after its enqueue; the clear needs none, since a stale bit costs one interrupt. Unchanged |
| `LAST_ROOT` | `forget_root` (compare-exchange) | none: this processor stores only the root it installs, never the forgotten one (above) |
| `LAST_DOMAIN` | `forget_root` (store), `leaving_domain` (load) | after F-60's fix, none for the leave: each processor checks itself at the grace-period answer. `forget_root`'s as for `LAST_ROOT` |
| `RUNNING` | `current_id` from another processor (reports, the failure policy) | none: a whole word, a hint |
| `RUNNING_GROUP` | a charge made by the running task; reports | written and read by the processor itself with interrupts masked; a remote read is a report |
| `RUNNING_SEEN` against `MOVES` | `note_moved` increments `MOVES`, then each processor's way out compares | message passing, not a store-buffer pattern (the consultant's correction, 2026-10-05, ledger 382): a move stores the process's new job, then increments `MOVES`, and no mover reads a `RUNNING*` word afterwards; the way out loads `MOVES` (`Acquire`), then reads the job. The `Acquire` load orders the two; the way out also keeps a `SeqCst` fence between seeing a changed `MOVES` and reading the job, paired with the `SeqCst` increment after the job's store, as a second order kept on purpose. The fence runs only when the counts differ, so the common way out stays a load |
| the space's `CpuMask` | a shootdown's sender | the locked join before the root write and leave after it, unchanged |
| per-processor counters (`REFILLS_IN_DOMAIN`, `BARRIER_DECISIONS`, `SWITCH_BARRIERS`) | the domain check, reports | none: a whole word, read after the switches it counts, through the check's own synchronisation |
| ARMv7-A: a processor's active ASID tag (`asid::ACTIVE`, §9.13), swapped by the install's fast path, stored by its slow path | a rollover on another processor, under the allocator's lock, swaps it with 0 | one location's read-modify-writes are totally ordered: a fast path swaps before the rollover's swap (its number is reserved) or after it (it reads 0 and takes the locked slow path). `Relaxed` throughout; on ARMv7-A each access is an `ldrexd`/`strexd` pair or loop with no barrier. The `loom` model, L.armv7a.19 |
| ARMv7-A: a space's ASID tag (`SpaceTag`), written only under the allocator's lock | any processor's fast path (load) | none beyond the lock: a stale tag fails the fast path's generation compare or its swap, and the slow path rereads it under the lock |
| ARMv7-A: the generation (`asid::GENERATION`), stored under the lock at a rollover | every fast path (load) | none: a stale generation sends the fast path to the slow one, or is covered by the active tag's swap (row above) |


**The table re-read against the code (2026-10-05, on `step2f` rebased onto
`main` e41489fd7).** Every access to `LAST_DOMAIN`, `LAST_ROOT`,
`ENTERING_DOMAIN`, `IDLE`, `RUNNING_SEEN` and `MOVES`, and every caller of
the functions that make them, was listed with `grep` and read:
- `LAST_DOMAIN` is written by its own processor in `left_space`,
  `entered_space` (a load and a `Relaxed` store since 2f), `answer_leaving`
  and `serve_wanted_barrier` (each a `SeqCst` store), and by another
  processor only in `forget_root`. It is read remotely by `leaving_domain`'s
  scan (`SeqCst`) and by stage 9's `last_domain_on`. **`answer_leaving` (F-60's
  fix) reads it, but never another processor's:** its one caller,
  `smp::answer_grace_periods`, runs on the answering processor with
  interrupts masked, reached through `as_this_cpu` from `synchronize` and
  its wait, and from the grace-period interrupt. `entered_space` also runs
  with interrupts masked, so on one processor an answer comes wholly before
  or wholly after a switch, and reads what the switch stored by program
  order alone. The table's row stands: no ordering for the leave rests on
  the switch's accesses.
- `LAST_ROOT`: as the table says. `forget_root`'s compare-exchange and this
  processor's load and store are each single-copy atomic; a store that
  follows the clear stores the root being installed, never the forgotten
  one.
- `ENTERING_DOMAIN`, `RUNNING_SEEN` and the three counters: written and read
  only by their own processor with interrupts masked, read elsewhere only
  as reports or by a check after the switches it counts.
- `IDLE`: `set_idle` is called only with the calling processor's own number
  (the idle loop's four calls and `set_idle_at_switch`), so the switch's
  load of its own bit reads what it last wrote.
- `MOVES`: incremented only by `note_moved` (`SeqCst`, after
  `Process::move_charged`'s store of the new job's slot), read by `regroup_current`
  (`Acquire`, then the fence when the counts differ). It is message
  passing: no mover reads a `RUNNING*` word after its increment, so the
  `Acquire` load alone orders the job's read, and the fence is a second
  order, kept. The `loom` model `regroup` holds it; its control drops both
  the `Acquire` and the fence and finds a way out reading the old job
  (dropping either alone does not fail, which is why the control drops
  both).

One thing the re-read found is not 2f's and is older than it: **`forget_root`
can erase the domain a processor has just recorded.** It compares and clears
a processor's `LAST_ROOT`, then stores zero to its `LAST_DOMAIN` as a second
access. A processor whose `LAST_ROOT` names a freed root, in the middle of
`entered_space` for a member's space, can store the member's domain between
the two, so that the switch skips the barrier inside the domain while the
processor's `LAST_DOMAIN` ends at zero. If that member then leaves the domain
on this processor, `leaving_domain`'s scan and `answer_leaving` both find
zero there, and no barrier separates it from the member that ran before it.
The old swaps allowed the same interleaving, so 2f neither opens nor closes
it. The fix proposed is for `forget_root` to clear only `LAST_ROOT`: the
domain a processor records is the one it last ran, whichever root it was
in, and the incoming domain always comes from `ENTERING_DOMAIN`, so a reused
root cannot inherit a domain through it. The consultant confirmed it and
numbered it **F-64** (Moderate; ledger 382), reserved in FINDINGS.md; its
fix is a landing of its own (`docs/BACKLOG.md`).

The 64-bit weight arithmetic is accepted with its host test at depths 1 to
8, and the weight setters' assertion that every entity weight is below
2^32 becomes a requirement row of its own (question 7). Rows added: *L.sched:*
the regroup word's fence pair; *L.object (quota):* every entity weight a job
or task can be given is below 2^32.

**Points:** 5 to 7 after the review (4 to 6 in the draft), F-60's fix not
counted. **Saving:** *guess* 0.05 to 0.15 us a round trip: about six locked
operations and two 128-bit divisions a direction.

**Documents.** SPECULATION.md §3: no row changes; the *program → program*
row's implementation note says the decision is made from per-processor
words. If question 5's fix is taken, §3 and §9.3b's F1 record it.
MEMORY-AND-TIMING: none. FINDINGS: question 5 may become a finding, which
the consultant numbers. Security Target: none; FDP_IFF.1's in-domain
exception is unchanged.

#### 3a: the vector-state contract for native calls that block

The customer approved this contract on 2026-10-02 (§9.5, decision 2). This
is its design, with §9.7's condition 7, which puts the reset in
`switch_user_state` for every resume of a task whose state was not saved.

**The contract.** Three native calls are declared to destroy the vector
registers: `channel_write_read` (0x1013), `object_wait_one` (0x1008) and
`port_wait` (0x101A), the native calls that block. Through any of them the
caller must assume that every register the System V AMD64 ABI makes
caller-saved is lost, as across a function call: `XMM0` to `XMM15` and the
upper halves of `YMM0` to `YMM15`, and the x87 data registers. It keeps the
two that ABI makes callee-saved: `MXCSR`'s control bits (the kernel keeps
`MXCSR` whole) and the x87 control word. The contract is by call number,
on the native `SYSCALL` entry, and applies to any process that makes those
calls. No Linux number is affected, and nor is `int $0x80`.

**The runtime's stub.** `src/user/system/native/rt/src/arch/x86_64.rs` keeps
`trap_words` for every other call and gains `trap_blocking` for the three:
the same `syscall` with the same operands, plus `clobber_abi("sysv64")`. The
declaration names RAX, RCX, RDX, RSI, RDI, R8 to R11, `XMM`/`YMM` 0 to 15,
`k0` to `k7` and `ZMM` 16 to 31 where the target has AVX-512, the x87 and
MMX registers, and AMX tiles where the target has them. The explicit
operands override it for the registers that carry arguments and results. So
the compiler keeps no live value in a vector register across those calls.
The functions in `ferrix_native` for the three calls use `trap_blocking`, and
the native ABI's documentation states the contract per call.

**What the kernel does.**
- At entry, the `SYSCALL` path marks the task `vectors_dead` for the length
  of the call when the number is one of the three. The mark lives in the
  task's `UserState` and is lowered as the call returns.
- At a switch away from a task that is *blocked* with the mark raised,
  `switch_user_state` saves only `MXCSR` (`stmxcsr`) and the x87 control word
  (`fnstcw`), not the `XSAVE` area, and marks the saved state `unsaved`.
- At a switch to a task whose state is `unsaved`, whoever switches to it,
  `switch_user_state` resets the vector registers: `XRSTOR` with an
  `XSTATE_BV` of zero for every enabled component, which loads each
  component's initial state, and with the task's own `MXCSR` in the area,
  which `XRSTOR` loads from memory whenever SSE or AVX is requested. Then
  `fldcw` loads the task's control word if it is not the initial `0x037F`.
  The mark is cleared only by the reset. That is condition 7: the frame tail
  of step 4, the general continuation, a message, a close, a kill or a
  signal, on a direct switch or any `choose_next`, all switch to the task
  through `switch_user_state`.
- A task switched out *runnable* (preempted in user mode, or preempted inside
  one of these calls while interrupts are on) is saved and restored in full,
  as today. So is a task blocked in any other call, and so is a task that was
  reset and then preempted before it left the call: its registers are the
  reset state, saved in full.

**Why nothing of another program reaches the task.** The registers a resumed
task sees are either its own, restored from its own area, or every
component's initial state with its own `MXCSR` and control word. The `XRSTOR`
of an empty header writes every enabled component, so no register keeps what
the program that ran before left in it. The kernel itself never uses these
registers (it is built for a target with no SSE). A signal delivered at the
call's way out saves the post-call state, the reset one, into its frame, as
the contract says.

**AVX-512, AMX and PKRU.** The reference configuration enables x87, SSE and
AVX in `XCR0` only, with `CR4.PKE` off (`cpu::enable_extended_state`). If any
of these is ever enabled, the reset's component list is reviewed again, and
this is the rule it must keep:
- *AVX-512* (opmask, `ZMM_Hi256`, `Hi16_ZMM`): caller-saved under the ABI,
  so the reset may include it, and the save area grows.
- *AMX* (`TILECFG`, `TILEDATA`): caller-saved under the ABI, so the reset may
  include it, but a component armed in `IA32_XFD` must be left out of the
  `XRSTOR`'s requested set, or the `XRSTOR` faults.
- *PKRU* must never be reset. Its initial value grants every key, so a reset
  would widen the program's own protection. It is saved and restored at every
  switch, blocked or not, outside the reset (`rdpkru`/`wrpkru`, as Linux keeps
  it apart).

**A program that breaks the contract** (one that makes 0x1013 through the old
`trap_words` and keeps a value in `XMM0`) loses that value. That harms only
the program itself; no other program's data reaches it.

**Why x86-64 only.** On AArch64, AAPCS64 makes the low 64 bits of `V8` to
`V15` and `FPCR` callee-saved, so "as across a call" would keep part of the
state, and Arm has no fast path in step 4 to pay for it (question 9).

**Rows.**
- *H, vector state of a blocked native call:* a task resumed after blocking
  in 0x1013, 0x1008 or 0x101A holds either its own vector state or the
  initial state with its own `MXCSR` and x87 control word, never a value
  another program left.
- *L.x86_64:* the switch saves only `MXCSR` and the control word of a task
  blocked in one of the three with `vectors_dead` raised, and marks its state
  `unsaved`.
- *L.sched (condition 7):* every task switched to with its state `unsaved` is
  reset first, on every path, and the mark is cleared only by the reset.
- *L.x86_64:* a task switched out runnable, or blocked in another call, is
  saved and restored in full.
- *L.x86_64:* PKRU, when enabled, is never part of the reset.
- *The native ABI's contract*, in `ferrix_native_abi`'s documentation and the
  Safety Manual: the three calls destroy the caller-saved vector registers.

**Checks and controls.** Stage 9, x86-64:
1. *The reset* (§9.7 case 15, built here first on the general path). A task
   sets every vector register to a pattern and blocks in 0x1013. A second
   program on the same processor sets every register to another pattern and
   runs. The first is woken by a message, a close, a kill of a sibling thread
   and a signal, one at a time, and reads back every vector register it can
   name: none holds either pattern, and `MXCSR` and the control word are its
   own. Control: the reset skipped on a general resume; the check sees the
   second pattern.
2. *The full save stays full.* The same task preempted by a timer in user
   mode, and blocked in a Linux `read`, reads its own pattern back. Control:
   the switch treats every blocked task as `unsaved`; the `read` case sees
   zeros.
3. *The mark is the call's.* A task that blocked in 0x1013, was woken and
   then makes a Linux `read` that blocks keeps its registers across the
   `read`. Control: `vectors_dead` not lowered at the call's return.

**After the review (condition 7, question 10).**
- *The format.* The area is the standard (non-compacted) form, written by
  `XSAVE64` and read by `XRSTOR64`, never `XSAVEC` or `XSAVES`. The
  requested-feature bitmap of both the save and the reset is `XCR0`'s
  enabled set, x87, SSE and AVX on the reference configuration (`0x7`).
  The reset area's header has `XSTATE_BV` zero and `XCOMP_BV` zero, and its
  legacy region's `MXCSR` field holds the task's `MXCSR`. Intel's SDM,
  Vol. 1 §13.8.1 (*Standard Form of XRSTOR*), is what makes the reset keep
  it: with `RFBM[1]` or `RFBM[2]` set, `XRSTOR` loads `MXCSR` from memory
  whatever `XSTATE_BV` says, and initialises each component whose
  `XSTATE_BV` bit is clear. The x87 component's initial state has the
  control word `0x037F`, which is why `fldcw` follows when the task's
  differs.
- *Every reader of a saved area.* A task's `UserState` gets one accessor for
  its vector state, and an `unsaved` state answers through it as the
  initial state with the task's own `MXCSR` and control word. On `main` the
  switch is the only reader of a saved area: a signal frame is built from
  the live registers at the way out (`UserState::capture`), after the reset,
  and there is no core dump of vector state and no register report that
  reads another task's area. Any such reader added later goes through the
  accessor; a signal frame built later, a core dump and a register report
  are named in the accessor's documentation, and a host test checks that an
  `unsaved` state reads as the initial one.
- *The Security Target.* The customer chose (2026-10-02) to widen FDP_RIP.2
  to "the register state a program is given at every switch", which covers
  both the full restore and the reset. ADV_ARC describes the mechanism. The
  contract is x86-64 only (question 9).

**Points:** 6 to 9: the stub and the ABI text (1), the mark and the two
switch paths (2 to 3), the PKRU rule written and asserted (1), three cases
and controls (2 to 4).

**Saving:** from §9.5's span, vector state 0.12 us a switch. The save becomes
two stores and the restore one `XRSTOR` of initial state, which the
processor's init tracking makes cheap where it has it (measured, not
assumed, §9.7 part 7): *guess* 0.15 to 0.2 us a round trip.

**Documents.** SPECULATION.md: the Zenbleed and GDS rows are unchanged; the
reset is not a side-channel defence, and a guest that hides AVX still hides
it. MEMORY-AND-TIMING: §2.2c records which state a figure ran with.
FINDINGS: none. Security Target: the reset is residual information
protection for a register file, where FDP_RIP.2 names frames only; either
FDP_RIP.2's refinement gains "and the vector registers of a task resumed
from a blocking native call", or ADV_ARC argues it as domain separation
(question 10). VULNERABILITY-ANALYSIS gains an entry: a resume path that
skips the reset.

**As built (2026-10-05, branch `po6/step3`, ids H.SCHED.12, L.sched.54,
L.x86_64.152-157).** As designed, with five differences for the review:
- *The runtime's clobber is on every call.* `trap_words` gains
  `clobber_abi("sysv64")` instead of a second block `trap_blocking`: the
  assembly budget stood at 1,606 of 1,610 lines with 3a's three kernel
  instructions (`stmxcsr`, `fnstcw`, `fldcw`), and a second trap block is 13.
  Every call through `ferrix_rt` is then compiled as if it lost the vector
  registers, which costs a program only the compiler's choice to keep a value
  there across a call that would have kept it. The kernel's contract is still
  the three numbers (`syscall::vectors_die_in`).
- *The accessor is checked at boot, not on the host.* The kernel crate has no
  host tests; stage 9's `check_unsaved_reads_as_initial` builds a state with
  foreign bytes in its area, keeps only the two words, and reads it through
  `fxsave`, `avx` and `xstate_bv` as `FpuArea::initial` with its own `MXCSR`
  and control word; a writer (`fxsave_mut`, `avx_mut`, `set_xstate_bv`) first
  turns an `unsaved` state into that image (`materialise`).
- *The wakes are a message, a close and a signal.* A native wait is ended by
  a message, its peer's close, its process's end and another thread's
  `execve`, and not by a signal (`NativeCall::ChannelWriteRead`). So the
  signal case sends `SIGUSR1` while the task waits -- which wakes it, resets
  it and lets it block again -- and then the message: the handler's frame
  holds the post-call state, and `rt_sigreturn` puts it back. A kill, of the
  process or of a sibling, ends the task before it reaches user mode, so no
  program can read a register after one; it passes through the same restore.
- *No partial save without `XSAVE`.* The reset is an `XRSTOR` of an empty
  header; on a processor that saves with `FXSAVE` every switch saves in full,
  and the `vectors` line says the cases were not run.
- *PKRU.* `cpu::reset_components` masks `XSTATE_PKRU` out of the reset's
  requested set, and stage 9 checks that it does and that nothing past x87,
  SSE and AVX is enabled.
The mark lives in `UserState`, raised and lowered through
`sched::with_own_user_state` with interrupts masked; the switch passes
`previous.is_blocked()` to `save_user_state` on all three architectures (the
Arm pair ignore it). The checks are `arch/x86_64/switch/check.rs`, with two
fixtures assembled by GNU `as`; the `vectors` boot line.

#### 3b: the FS and GS bases kept in the task

**What changes.** `save_user_state` reads `FS_BASE` and the program's
`GS_BASE` (in `KERNEL_GS_BASE` while the kernel runs) with two `rdmsr`s at
every switch, and `restore_user_state` writes both: about 0.14 us a switch
(§9.5's segment bases span). Instead:
- The task's `UserState` keeps both bases as the truth. `arch_prctl`
  writes the register and the running task's record together, with
  interrupts masked.
- The switch still reads the four data selectors and the three
  thread-local descriptors, which are moves from segment registers and
  loads from this processor's GDT, not MSRs.
- *Following Linux's `save_base_legacy`:* if the outgoing task's `FS`
  selector reads null, its recorded base stands and no `rdmsr` is made. If
  it is not null, the base is the descriptor's, and the restore reloads the
  selector, which loads it. `GS` likewise.
- The restore writes both bases at every switch to a task with user state,
  as now. (The draft kept, in `PerCpu`, the bases each processor last
  wrote, and skipped a write equal to it. The review found that skip leaks
  across programs on both vendors, so it is dropped; see *After the
  review*.)

**The one discrepancy.** On processors where loading a null selector into
`FS` clears the base (Intel), a program can clear its own base with no call,
and no selector read shows it. The program then gets its recorded base back
at its next switch-in, as on Linux; that is its own value, not another
program's. That is Linux's rule, on the read side only (question 11).

FSGSBASE stays off (§9.6).

**Rows.**
- *L.x86_64 (changed):* a task's FS and GS bases are its record's, written by
  `arch_prctl` and loaded by the switch; the switch reads no base MSR.
- *L.x86_64:* the switch writes the incoming task's FS and GS bases at every
  switch to a task with user state.

**Checks and controls.** Stage 9, x86-64:
- Two programs with different `FS` bases trade one processor 10,000 times,
  each checking its own `%fs:0`. Control: the restore skips every write;
  the second program reads the first's.
- A program that loads `FS` with its TLS selector, then a null one, then is
  switched out and back: it reads the base Linux would give it.
- The review's case (condition 8): one program loads `USER_DS` into `FS`,
  then a null selector, and is switched out; a second program, whose
  recorded base equals the one the processor last wrote, is switched in and
  reads its own `%fs:0`. Control: the write skip restored, after which the
  second program runs on whatever base the first left.

**After the review (condition 8, question 12).** The write skip leaks
across programs on both vendors. A program that loads `USER_DS` into `FS`
and then a null selector leaves the base as the descriptor's (zero) or
keeps whatever was there, depending on the vendor, and no selector read
shows which. A per-processor record of "the base last written" then names a
base that is no longer loaded, and the next program whose recorded base
equals it would run on the first program's choice. So the read skip stays
(no `rdmsr`, the base is the task's record) and the write skip goes. With it
go the per-processor fields and the boot probe, and §9.4 item 5's segment
skip does not come back in any form.

**Reopened narrowly: the `DS`/`ES` skip (2026-10-06 and -07).** §9.10's
profile then measured the selector loads as the largest item after the
scheduler, and the consultant reopened the skip for `DS` and `ES` alone
(ledger line 393, conditions S1 to S7), read po7-ipc4's candidate
f71585ad1 against them (line 398: S1 and S2 met; S3, S4, S5 and S7 not, as
it also skipped `FS` and `GS`), and gave po9-sel's design its verdict (line
412, E1 to E6, under the round's G1 to G11 at line 409). Answer 12 is
amended to: *the segment skip does not come back, except `DS` and `ES`
skipped 0 to 0, as Linux's `__switch_to` does.* Why this is not condition
8: `DS` and `ES` have no MSR base and no record, and the comparison is with
the processor's own register. In long mode only an explicit load writes
`DS` or `ES` (`SYSCALL`/`SYSRET`, interrupts, `IRET` and far transfers
leave them), so one reading 0 was last loaded with 0; whatever a vendor's
null load does to the hidden part, it is idempotent, so loading 0 over a
`DS` that reads 0 changes nothing, and the skip leaves exactly the state
the load would.

*As built (branch `po9/sel`, po9-sel).* `load_selectors` reads `DS` and
`ES` (`cpu::read_data_selectors`) with no load between, and leaves each unloaded
only where it and the record's selector are both exactly 0 (S1, S2): 1 to
3 are null selectors whose RPL bits a program reads back, so they are
loaded over. `FS` is loaded and its base written, and `GS` loaded between
its `swapgs` pair and its base written, at every switch as before (S3,
E1); the `FS`/`GS` extension of line 398 is not taken. A per-processor
count of the switches that skipped (`SELECTOR_SKIPS`) is read only by the
check and by the fast path's counts line, never by the switch. Every
switch -- the general path's and the fast path's direct one -- goes
through the one `restore_user_state`. L.x86_64.8 is restated (S5, E4); no
new id. The Security Target's FDP_RIP.2 mechanism text and the
vulnerability analysis's item 7 say the same.

*Checks (S4, E3; stage 9, x86-64, the `selector` line):* (i) a program
that leaves 3 in `DS` and `ES`, beside one whose record is 0, which reads
0 and 0 after each of 1,000 yields; (ii) the same with `USER_DS`; (iii) a
program that loads a based thread-local descriptor and then 0 into `DS`
and `ES`, beside one with `DS` 0 that far-returns into compatibility mode
and reads through `DS`: `SIGSEGV` under KVM or on hardware, with the skip
and without it (under TCG not decided, below); and an i386 program
(`USER_DS`) and a 64-bit one (0) trading the processor, each reading its
own; (iv) two 64-bit programs with `DS` and `ES` 0 trading the processor
count more than 0 switches that left `DS` or `ES` unloaded, and with
`ferrix.fastpath=on` the counts line after the bench names the skips the
direct switch took. Accepted by the consultant OK IF (ledger line 414). Under QEMU's TCG
the compatibility-mode read is not decided: TCG loads a null selector as
an absent segment but never checks a data access against it, so the read
succeeds with the skip and without it (the control that turns the skip
off showed it, `po9-sel-c4-off-tcg`); the check recognises TCG by `CPUID`
leaf `0x4000_0000` and says so on its line, and the case is decided under
KVM on the AMD reference host, where the read is `SIGSEGV`.

*Measured (S6, E5):* `DS` and `ES` alone, fast path on, alternated against
112a12b63 under the bench lock, 6 boots each, host load 0.5 to 3.3. Both
sides boot in one of two modes (po9-user is finding why): in the high
mode 1,388, 1,388 and 1,408 ns against 1,548 (ratios 0.897, 0.897,
0.910), in the low mode 1,118 and 1,128 against 1,248 (0.896, 0.904); one
round crossed modes (1,158 against 1,548). The skip saves 120 to 160 ns a
round trip, 60 to 80 a direction. Records under
`docs/hotpaths/results/ipc-round-trip/6bc2646211ca/`, logs
`~/.local/share/ferrix/logs/po9-sel/dses/` on nazuna.

**Reopened for `FS` and `GS` (2026-10-07, 3c).** po10-sel-cert's design
verdict (ledger line 445, K1 to K10) gave the same skip for `FS` and `GS`,
with the bases still written from the record at every switch and every
processor loading both with 0 at bring-up. Answer 12 is amended again, to:
*the segment skip does not come back, except `DS`, `ES`, `FS` and `GS`
skipped 0 to 0 by the processor's own registers, the `FS` and `GS` bases
written after at every switch.* Condition 8 stands unchanged: no base is
ever compared with anything.

**Points:** 2 to 3. **Saving:** *guess* 0.07 to 0.14 us a round trip, from
the span: the two `rdmsr`s a switch go; the writes stay.

**Documents.** SPECULATION.md: none. MEMORY-AND-TIMING: none. FINDINGS: none.
Security Target: FDP_RIP.2 as widened for 3a covers the bases too: a program
is given its own at every switch.

**As built (2026-10-05, branch `po6/step3`, id L.x86_64.158; L.x86_64.9 and
.61 changed).** `save_user_state` reads no base MSR; `arch_prctl` writes the
MSR and the running task's record with interrupts masked (the entry answers
it before it opens them); `execve`'s `reset_user_state` zeroes the record
with the MSRs, without which the new image would get the old one's thread
pointer back at its next switch-in. `UserState::capture`, which is not the
switch, still reads both MSRs: a fork child and a signal frame take what the
processor holds. The checks (`fsbase` line): two programs with different bases
trade one processor 10,000 times each; one loads `USER_DS` and then a null
selector into `FS` before each yield -- after which its base is zero on both
vendors, while its record holds its own -- beside a second whose recorded base
is the same address, which reads its own word at every turn; and the first
reads its own word again after a `nanosleep`, its recorded base back.

**The controls (2026-10-05 and -06, on 4a8b8dfee, x86-64 under KVM; logs
`~/.local/share/ferrix/logs/queue/po6-ipcB-s3-<name>.log` on nazuna), each
FIRED with the text named:**
- `c1-1`, 3a check 1: the reset skipped on a resume (`state.unsaved =
  false` for `reset_vectors`): "read another program's vector registers".
- `c2-1`, 3a check 2: every blocked task treated as `unsaved` (the
  `vectors_dead` test dropped): "lost its vector registers to a reset".
- `c3-1`, 3a check 3: the mark not lowered at the call's return: "the mark
  outlived its call".
- `c4-1`, 3b's first check: the restore skips every `FS` write: "did not
  each read their own FS base".
- `c6-1`, Linux's rule: the switch reads `FS_BASE` again on the way out:
  "did not get its recorded base back".
- `c5-3`, condition 8, the write skip restored as one "last written" word:
  fires in the trading check, "did not each read their own FS base", which
  runs first.
- `c8-2`, condition 8 alone: the draft's skip, one "last written" base per
  processor kept by every `FS_BASE` write (`set_thread_pointer`), on a side
  ref (`po7-ipcB/c8-base`, 1c5d1f6ca) that differs from 4a8b8dfee only in
  not running the trading check, so the condition-8 case runs first; the
  side ref unchanged PASSED (`c8base-1`), with the skip it FIRED: "a program
  whose recorded FS base equals the one last written ran on the base another
  program left". The same skip on 4a8b8dfee (`c8-1`) fires in the trading
  check instead. Why: nazuna's Ryzen 9 9900X clears the base when a null
  selector is loaded (AMD's `NullSelectorClearsBase`, CPUID `0x8000_0021`
  `EAX` bit 6, reads 1 on the host; Intel clears it too), and the
  switch itself loads the incoming task's null `FS` before it writes the
  base, so on this processor any skipped write leaves the base zero whatever
  the programs do. The condition-8 case is the one that would catch the skip
  alone on a processor that keeps the base.

#### 3c: FS and GS 0 over 0, and FSGSBASE (design for the consultant, po9-sel and po10-sel, 2026-10-07)

**Why now.** po9-sched's profile of `main` puts `load_selectors` -- the
four selectors and the two bases -- at 173 ns a direction, the largest
single item on the trip; the `DS`/`ES` skip took 60 to 80 of it. What is
left is the `FS` load, the `GS` load inside its `swapgs` pair with
interrupts masked, and the two base `WRMSR`s. Line 412 gave `DS` and
`ES` only, and said `FSGSBASE` needs a design of its own (F1 to F5).
This is that design, in two slices that land apart.

**What Linux does at a switch** (Linux 6.18.54,
`arch/x86/kernel/process_64.c`):
- `__switch_to`, lines 656 to 664: `DS` and `ES` as built in 3b's
  *Reopened narrowly*, then `x86_fsgsbase_load`.
- `save_fsgs`, 275 to 291: `savesegment` of `FS` and `GS` from the
  register; with `FSGSBASE`, `rdfsbase()` and `__rdgsbase_inactive()`
  (`swapgs; rdgsbase; swapgs`, 165 to 212), "user code expects us to
  save the current value"; without it `save_base_legacy` (236 to 273).
- `x86_fsgsbase_load`, 392 to 410: with `FSGSBASE`, `FS` is loaded only
  `if (unlikely(prev->fsindex || next->fsindex))`, `GS` likewise, where
  `prev->fsindex` was read from the register by `save_fsgs` in the same
  switch; then `wrfsbase(next->fsbase)` and `__wrgsbase_inactive`
  (`swapgs; wrgsbase; swapgs`, 214 to 228) at every switch.
- Without `FSGSBASE`, `load_seg_legacy`, 319 to 361, skips a null load
  only when `prev_index | next_index | prev_base` is 0, and on
  `X86_BUG_NULL_SEG` parts (AMD) loads `__USER_DS` then the null selector
  to clear the base. That compares a recorded base, which is condition
  8's leak; it is not proposed here.
- Enabling: `arch/x86/kernel/cpu/common.c` 2393 to 2401 sets
  `CR4.FSGSBASE` and `elf_hwcap2 |= HWCAP2_FSGSBASE` (bit 1,
  `arch/x86/include/uapi/asm/hwcap2.h` line 11); `nofsgsbase` on the
  command line clears it (509 to 513).
- The paranoid entries, `arch/x86/entry/entry_64.S`: `paranoid_entry`
  (857 to 910) with `FSGSBASE` does not decide `swapgs` by the `GS`
  base's sign; it saves the base with `RDGSBASE` into `RBX` and writes the
  kernel's (`SAVE_AND_SET_GSBASE`, the per-CPU offset found without `GS`),
  and `paranoid_exit` (987 to 990) and the NMI exit (1422 to 1429) write
  `RBX` back with `WRGSBASE`.

**Slice A: `FS` and `GS` skipped 0 over 0, no `FSGSBASE` (proposed for
this round).** The rule of `DS` and `ES` (S1 to S7, line 412's E1 to E6),
extended as line 393's note and line 398 sketched, with condition 8 and
3b's base rule untouched:

- *The rule (A1).* In `load_selectors`, `FS` is left unloaded only where
  the selector to load (after `gdt::loadable`) and the one the processor
  holds -- read by the same `cpu::read_data_selectors` that the `DS`/`ES`
  skip already makes, in the same switch, with no load between -- are both
  exactly 0. `GS` likewise, and its skip drops the whole `load_user_gs`:
  the `swapgs` pair, the `RFLAGS` read and the interrupt mask around it.
  Exactly 0, never "null": 1 to 3 are null selectors whose RPL bits a
  program reads back with `mov %fs`/`mov %gs`, and 4 to 7 name the LDT.
  Never a per-processor or per-task "last loaded" (S2 as it stands); a
  switch from a kernel thread or idle reads the processor fresh, since the
  read is in `load_selectors` itself.
- *The bases (A2), unchanged.* Where the record's `FS` (`GS`) selector is
  0, its base is written from the task's record at every switch, skipped
  or not: `set_thread_pointer` (`WRMSR` of `FS_BASE`) and
  `set_program_gs_base` (`WRMSR` of `KERNEL_GS_BASE`), as 3b built them.
  Where it is not 0 the selector is loaded, which loads the descriptor's
  base, as now. So no base is ever compared with anything, recorded or
  held; condition 8 is not touched.
- *The other loads (A3), unchanged.* `load_program_selectors` (a 32-bit
  signal frame's entry and return), `enter_compat_segments`, and
  `set_thread_area`'s reload of a selector naming the slot it rewrote keep
  loading unconditionally. Only the switch's `load_selectors` skips, and
  `reset_user_state` (`execve`) goes through it with zeros and zero bases,
  so a new image whose `FS` read 0 keeps it, and one that held 3 or a TLS
  selector is loaded over.
- *The count (A4).* Two more per-processor counters, `FS_SKIPS` and
  `GS_SKIPS`, kept as `SELECTOR_SKIPS` is (a load and a store with
  interrupts masked, no locked write), count the switches that left `FS`,
  and those that left `GS`, unloaded, apart, so a skip taken for one
  register alone cannot pass the check (K5); they are read only by the
  check and the fast path's counts line, never by the switch. Both
  switches -- the general path's and the fast path's direct one -- go
  through the one `restore_user_state` (G5).
- *The bring-up load (A5, the consultant's K1).* Nothing loaded `FS` or
  `GS` at bring-up: `reload_segments` loads `DS`, `ES` and `SS` with the
  kernel's data selector, and the trampoline the same, so an application
  processor ran with `INIT`'s `FS` and `GS` (selector 0 over a usable
  flat data segment, limit `FFFF`) and the boot processor with whatever
  the firmware left, until a switch loaded one. With the skip, a processor
  on which no program ever held a non-zero `FS` or `GS` would keep that
  hidden part where today's load replaces it. So every processor loads the
  null selector into `FS` and `GS` explicitly, in long mode, in
  `set_cpu_local` before it writes its per-CPU `GS_BASE` (a null `GS`
  load clears the base on Intel and on `NullSelectorClearsBase` parts):
  from then on the registers read 0 because they were loaded with 0.

*Why it leaks nothing (the FDP_RIP.2 argument, both vendors).* It is line
393's, and holds for `FS` and `GS` as for `DS` and `ES`, with one part
added for the base:
1. In long mode the `FS` and `GS` selectors are written only by an
   explicit load (`MOV`/`POP`/`LFS`/`LGS`). `SYSCALL`/`SYSRET`, interrupts
   and exceptions, `SWAPGS` (which swaps the base MSRs, not the selector)
   and `WRMSR` of a base leave them. `IRET` and a far `RET` to ring 3 null
   a data selector only when its descriptor's DPL is below 3; every
   selector the kernel ever puts in `FS` or `GS` is a user one checked by
   `gdt::loadable` or 0 (the six load sites above; the kernel never loads
   one of its own), so that rule never applies. `VMRUN`/`#VMEXIT` and
   `RSM` restore the registers whole. And A5 loads both with 0 on every
   processor before any program runs there (`DS`, `ES` and `SS` hold the
   kernel's data selector from `reload_segments`, which the first switch
   loads over). So a `FS` or `GS` that reads 0 was last loaded with 0.
2. A vendor's null load acts on the hidden part by a rule f that depends
   only on the selector (Intel: unusable, base cleared in 64-bit mode;
   AMD: base cleared where `NullSelectorClearsBase`, CPUID `0x8000_0021`
   `EAX[6]`, kept before; limit and attributes kept or marked by vendor).
   f(f(x)) = f(x): loading 0 over a register that reads 0 and was last
   loaded with 0 changes nothing. So the skip leaves exactly the hidden
   state today's load leaves, on every vendor -- including a stale AMD
   limit and attributes, which are then today's question, not the skip's
   (the compatibility-mode case below decides it on the reference host).
3. The one part a 64-bit program can read through a null `FS` or `GS` is
   the base, and A2 writes it from the incoming task's record after,
   whether the load was skipped or not; and in the `GS` case the base the
   program will see is `KERNEL_GS_BASE`, which the skipped `swapgs` pair
   never touched anyway.
4. A 32-bit program (compatibility mode) faults on any use of a null `FS`
   or `GS`, with the skip or without; an i386 program's TLS selector is
   not 0 and is always loaded.

So the selector values are given exactly (exactly 0, A1), the hidden part
is identical to the load's (2), and the bases are the program's own at
every switch (3, A2): FDP_RIP.2 as widened holds on Intel and AMD
semantics alike, with the vendor-specific part (2) argued and the
observable part checked below.

*Checks (stage 9, x86-64, on KVM and TCG, a new `fsgs` line after the
`selector` line; one new fixture, `FSGS_PROGRAM`, the `DS`/`ES` fixture's
shape with `FS` and `GS`, and its bases set by `arch_prctl`
`ARCH_SET_FS`/`ARCH_SET_GS` to a page holding its own word):*
- (i) a program that loads 3 into `FS` and `GS` before each of 1,000
  yields, beside one whose record is 0 and whose bases are its own: the
  second reads `FS` = `GS` = 0 and its own word through `%fs:0` and
  `%gs:0` after every yield. Control: the skip on "null" (`selector & !3
  == 0`), which must fire "read the RPL bits of a null selector another
  program left in FS or GS".
- (ii) the same with `USER_DS`. Control: the skip on the record alone
  (`fs == 0`), which must fire "read the USER_DS another program left in FS
  or GS".
- (iii) a program that installs a based descriptor by `set_thread_area`
  and loads it into `FS` and `GS`, then 0, before each yield (so the
  processor holds 0 over a hidden part that was a TLS segment's), beside
  the reader of (i): it reads its own word through `%fs:0` and `%gs:0` at
  every turn. Control: the base writes skipped when the load is (line
  398's control), which must fire "did not read its own FS or GS base".
  And the compatibility-mode half, as the `DS` case's: a reader with `FS`
  0 far-returns into compatibility mode and reads through `FS`:
  `SIGSEGV` under KVM on the AMD reference host and on hardware; under
  TCG undecided and said so on the line, as for `DS`.
- (iv) an i386 program with a TLS selector in `GS` (`set_thread_area`,
  then `mov %gs`), reading its own `GS` and `%gs:0` word after each
  `sched_yield`, beside a 64-bit reader of (i): each reads its own.
  Neither the record alone nor the processor alone decides a skip.
  Control: the skip on the processor alone (`held_gs == 0`), which must
  fire in this case or (ii).
- (v) two readers with `FS` = `GS` = 0 trade the processor and
  `FSGS_SKIPS` moves above 0; with `ferrix.fastpath=on` the counts line
  after the bench names the `FS`/`GS` skips the direct switch took.
  Control: the skip removed (the load always taken), which must fire "no
  switch between two programs with null FS and GS left them unloaded".
- *The `GS` base (the consultant's K2).* Ferrix has no `ARCH_SET_GS`
  (`arch_prctl` answers `ARCH_SET_FS` alone, and `paranoid.rs` rests on
  that), and none is added here: a 64-bit program's recorded `GS` base is
  always 0. So the `GS` half of (i) to (iii) checks that base with what
  exists: the reader reads `%gs:0x400100` and compares it with its own
  word at `0x400100`, which only a base of 0 gives, never another
  program's descriptor base. The `GS` base-write control (the write
  skipped with the load) can fire only on a processor whose null load
  keeps the base; on `NullSelectorClearsBase` parts and under TCG the
  null load the other program made already cleared it, so the control
  cannot fire there, the `fsgs` line prints the vendor and CPUID
  `0x8000_0021` `EAX[6]`, and a BACKLOG row records the run owed on an
  AMD part without it. The `FS` base-write control must fire everywhere.
- (vi, K1) the compatibility-mode reader of (iii) also runs through `FS`
  and through `GS` on an application processor before any stage 9 case
  runs there (at `--smp 2` or more): `SIGSEGV`. Control: A5's bring-up
  load removed, which must fire under KVM on the AMD reference host.
- 3b's `fsbase` checks stay as they are and now run through the skip (both
  programs hold 0): two programs trading `FS` bases 10,000 times, the
  cleared base coming back (Linux's rule, no base read), and condition 8's
  case. Their controls `c4`, `c6`, `c8` are re-fired on the branch, since
  the path under them changed.

*Rows (split, the consultant's K6).* L.x86_64.8 stays the `DS`/`ES`
row with the `selector` line. L.x86_64.165, reserved on main for
po9-sel's stream (`requirement-reservations.json`) and released in the
commit that writes it: "`FS` and `GS` are each loaded from the record
unless the processor's selector and the record's are both 0; the `FS`
and `GS` bases are written from the record at every switch where the
record's selector is 0, skipped or not; and every processor loads the
null selector into `FS` and `GS` before it writes its per-CPU base",
its criterion naming the `fsgs` line's cases and what the
compatibility-mode case decides per hypervisor. L.x86_64.9, .61 and
.158 unchanged in substance (the bases are still the record's and still
written at every switch); their check list gains (iii). ADV_ARC's and the
Security Target's FDP_RIP.2 mechanism text and the vulnerability
analysis's item 7 say the same. 3b's *After the review* records this
section, its ledger line and the amended answer 12: *"... except `DS`,
`ES`, `FS` and `GS` skipped 0 to 0 by the processor's own registers, the
`FS` and `GS` bases written after at every switch"*.

*Measured* (ablation 1, the load skip alone, on 4066f41dd as 52aba9e0d,
alternated against 4066f41dd under the bench lock, fast path on, 5 boots
a side, every boot ran, load 0.8 to 1.4 at each start; logs
`~/.local/share/ferrix/logs/po10-sel/fsgsA/` on nazuna). Both sides
boot in one of the two modes: main 1,048 and 1,068 ns in the low mode
(2 boots), 1,298, 1,298, 1,298 in the high (3); the skip 888 and 908 in
the low (2), 1,118, 1,118, 1,128 in the high (3). **-160 to -180 ns a
round trip in each mode, -80 to -90 a direction**, the same as po9-sel's
first ablation on 690754979 (-130 to -170 in three rounds, three of the
ablation's boots stopped on FX-0902). With it the round trip is about
880 to 910 ns on a low-mode boot.

**Slice B: `FSGSBASE`.** The bases written by `WRFSBASE` and
`swapgs; WRGSBASE; swapgs` instead of `WRMSR`. It needs `CR4.FSGSBASE`,
which hands ring 3 the same four instructions, so (F1 to F5):
- *F1, the record is no longer the truth.* A program writes its own
  bases, so every save reads them back (`RDFSBASE`, `swapgs; RDGSBASE;
  swapgs`), as Linux's `save_fsgs`; 3b's read skip and Linux's legacy rule
  go, and the `fsbase` check's "cleared base comes back" becomes "a base
  a program cleared stays cleared", as on Linux with `FSGSBASE`.
  L.x86_64.9, .61 and .158 are restated. Condition 8 still holds: nothing
  is compared with a per-processor record; both bases are written at
  every switch.
- *F2, the entries.* The paranoid entry (`trap.rs`,
  `ferrix_paranoid_common`) decides `swapgs` by the sign of `GS_BASE`
  (`rdmsr` of `0xC0000101`, `js`), on the ground that "no program can
  load one" (`paranoid.rs`). With `FSGSBASE` a program can `WRGSBASE` any
  canonical address, a kernel one included, and an NMI, `#DB` or `#MC` in
  ring 0 would then run its handler through a program-chosen per-CPU
  pointer. So the paranoid entry must take Linux's form: save `GS_BASE`
  by `RDGSBASE`, write the kernel's per-CPU base found without `GS`
  (`RDPID` or the CPU number in a GDT limit, as Linux's `GET_PERCPU_BASE`;
  Ferrix has neither yet), and restore by `WRGSBASE` on the way out. The
  ordinary entries decide by `CS` and the `SYSCALL` entry swaps always, so
  they stand; the conditional `swapgs` and its `lfence` (CVE-2019-1125,
  SPECULATION.md's SWAPGS fence row) leave the paranoid path, and that row
  is reviewed again. The preemption count read through `GS` (2b) is the
  kernel's and needs the entries right, nothing more.
- *F3, what programs are told.* `user_hwcaps` (`arch/x86_64/mod.rs`)
  answers `AT_HWCAP2` 0 today; it would carry `HWCAP2_FSGSBASE` (bit 1)
  when `CR4.FSGSBASE` is set, and a `ferrix.fsgsbase=off` option (F-31's
  on/off pattern, Linux's `nofsgsbase`) clears both.
- *F4, capture and signal frames.* `UserState::capture` already reads the
  live registers for a fork child, so it stays right; an x86-64 signal
  frame carries selectors, not bases. `execve` zeroes both, as now.
- *F5, the model.* `+fsgsbase` in `x86_cpu`'s model for KVM and TCG
  (QEMU's TCG implements the four instructions); the ablation booted with
  it under KVM.

**Measured, slice B's parts** (alternated against ablation 1, fast path
on, under the bench lock):
- *The writes* (ablation 2, 300d3f1c0, `WRFSBASE` and `swapgs; WRGSBASE;
  swapgs` with `CR4.FSGSBASE` set, nothing else): 938, 948, 948, 938
  against 998, 978, 978, 978 ns in four rounds with both boots (0.940,
  0.969, 0.969, 0.959); one base boot stopped on FX-0902. **-30 to -60 ns
  a round trip, -15 to -30 a direction.**
- *The writes and the reads together* (ablation 3, F1's `RDFSBASE` and
  `swapgs; RDGSBASE; swapgs` at every save on top of the writes, on
  4066f41dd as 4bfbe4ecf; alternated against ablation 1 on the same base,
  logs `~/.local/share/ferrix/logs/po10-sel/fsgsB/`): **not decided.**
  po9-sel's first run of it (612bf99c4) stopped at every boot on 3b's
  cleared-base check, which answers 139 as well as 3 when the cleared base
  stays cleared, as F1 says it must; with the check taking both, it boots.
  Of eight rounds, the two taken on a quiet host read 1,048 against 1,128
  (crossing modes) and 898 against 908; the others ran at load 5 to 25
  with other sessions' gates on the host and crossed modes, and a third of
  the boots on either side stopped on FX-0902 (the alloc-sweep flake
  po9/fx0902 fixes). What can be said: slice B's net is at most a few tens
  of ns a round trip, against -160 to -180 for slice A.
- The paranoid entry's change (F2) is off the trip.

**Recommendation.** Slice A in this round: it is nearly all the saving
left in `load_selectors`, needs no new processor feature, no entry change
and no ABI change, and its argument is line 393's with the base part
added. Slice B is **not proposed now**: net of its reads it is worth at
most a few tens of ns a round trip (to be measured on a quiet host after
po9/fx0902 lands), and it costs a paranoid-entry rewrite in the item (F2,
with a per-CPU base found without `GS`, which Ferrix does not have), an
ABI change for programs (F3), and the restatement of 3b's rows (F1). It
stays here as the design to come back to, with F1 to F5 answered as far as
they can be without code.

**Questions for the consultant (3c).**
1. Is slice A, A1 to A4 with the checks (i) to (v) and their controls,
   accepted under S1 to S7 and E1 to E6 read for `FS` and `GS`, with
   condition 8 and 3b's base rule untouched?
2. One row (L.x86_64.8 restated for all four selectors) or the `FS`/`GS`
   half split as L.x86_64.165?
3. Is deferring slice B with the figures above acceptable, or is the
   paranoid-entry design (F2) wanted in this round regardless of its
   saving?

**The consultant's design verdict (po10-sel-cert, ledger line 445).**
Slice A may be built under K1 to K10, with S1 to S7 and E1 to E6 read
for `FS` and `GS`, and G1 to G11. Two claims of the draft did not hold on
the code: point 1 (nothing loaded `FS` or `GS` at bring-up; K1, now A5)
and the fixture's `ARCH_SET_GS`, which does not exist (K2, the `GS` base
checked as above, no new interface). The row is split (K6,
L.x86_64.165). Slice B is deferred (K9): `CR4.FSGSBASE` stays clear,
`AT_HWCAP2` 0, `paranoid.rs`'s rule stands, and slice B comes back as a
design of its own, ablation 3 re-measured on a quiet host after
po9/fx0902 lands. The ablation's figure is not counted for the landing
(K8): the built tip is measured against its base.

**As built (branch `po10/fsgs`, po10-sel, on main f8b44e04c).**
- `load_selectors` (`switch.rs`) takes `FS` and `GS` from the same
  `cpu::read_data_selectors` the `DS`/`ES` skip makes, and leaves each
  unloaded only where it and `gdt::loadable`'s output are both exactly 0;
  a skipped `GS` drops the whole `load_user_gs`. The base writes
  (`set_thread_pointer`, `set_program_gs_base`) stand where they were,
  under `fs == 0` and `gs == 0`, skipped or not (A1, A2, K3).
  `load_program_selectors`, `enter_compat_segments` and `set_thread_area`'s
  reload are unchanged (A3).
- `FS_SKIPS` and `GS_SKIPS`, per processor, counted apart (K5);
  `fs_gs_skips` for the check, and `selector_skips_total` now answers
  `[DS or ES, FS, GS]`, which the fast path's counts line prints as a
  second line, "fastpath switches that left FS unloaded, 0 over 0: N; GS:
  M" (A4).
- `set_cpu_local` (`x86_64/mod.rs`) calls `cpu::load_null_fs_gs` first, on
  every processor, before it writes `GS_BASE`: `load_fs(0)`, then
  `load_user_gs(0)`, whose `swapgs` pair points the null load at
  `KERNEL_GS_BASE` and leaves `GS_BASE` as it was for the write after
  (A5, K1). No new assembly.
- The checks are `run_fs_gs` in `switch/check.rs` (stage 9, the `fsgs`
  line), on one new 64-bit fixture (`FS_GS_PROGRAM`, modes `L`, `R`, `T`,
  `C`, `G`) and one i386 fixture (`FS_GS_PROGRAM_I386`), cases (vi), (ii),
  (i), (iii), its compatibility-mode half, (iv) and (v) in that order. The
  line says whether the compatibility-mode reads were decided, the
  application processor the bring-up case ran on, and the vendor with
  CPUID's `NullSelectorClearsBase` (under KVM the model's bit, not the
  silicon's: nazuna's guest reads 0 while the Ryzen 9 9900X clears).
- Row: L.x86_64.165 (new, released from the reservation in the same
  commit); L.x86_64.8's note points at it.

*Controls (K4, on 0481f8183, logs `logs/queue/po10-sel-<name>-<accel>-<n>.log`
on nazuna).* FIRED, with the text named, on KVM and TCG unless said:
the `FS` skip on "null" and the `GS` skip on "null" (`f1-null`,
`g1-null`: "read the RPL bits of a null selector another program left");
each on the record alone (`f2-record`, `g2-record`: "read the USER_DS
another program left"); each on the processor alone (`f3-held` on both,
`g3-held` on KVM: "an i386 program did not read its own thread-local
selector and base in FS and GS"; under TCG `g3-held` stops earlier, in
stage 3's `set_thread_area` check, which loads `GS` itself, so the
expected text is not reached: a failure, not a pass); each skip removed
(`f4-off`: "left FS unloaded", `g4-off`: "left GS unloaded"); the `FS` base
write skipped with the load (`f5-base`: "two programs trading one
processor did not each read their own FS base", 3b's case running first).
Re-fired on KVM since `load_selectors` changed: the `DS`/`ES` controls of
line 414 (`d1-null`, `d2-record`, `d3-held`, `d4-off`, each with its old
text) and 3b's `b4-write` ("did not each read their own FS base"),
`b6-read` ("did not get its recorded base back") and `b8-last`, which now
fires in condition 8's own case ("a program whose recorded FS base equals
the one last written ran on the base another program left"), because the
switch no longer loads a null `FS` over the base. DID NOT FIRE: `g5-base` (the `GS` base write skipped with the load)
on KVM and TCG, because a null load clears the base on the reference host
and under TCG, so the base another program left is 0, the reader's own
(K2 foresaw it); and `k1-bringup` (A5's load removed) on KVM, which K1
required to fire: the compatibility-mode reads
through a null `FS` and `GS` on processor 3 still faulted. Either the
processor faults on a null selector in compatibility mode whatever the
hidden part (as the `DS` case suggests), or processor 3 had already had
`FS` and `GS` loaded with 0 by then -- `load_program_selectors` at a
signal's entry and return, or a switch to a program that held a non-zero
selector, both load unconditionally -- and stage 9 comes too late to be the
first program there. The bring-up load stays as the argument's ground (a
`FS` or `GS` reading 0 was loaded with 0), accepted on argument and code
review alone (po10-sel-cert, ledger line 498, L2), with a BACKLOG row for
a run where the control can fire; its case cannot show it on this host.

*Measured (K8, G8).* The built tip 09da3bbcd against its base main
f8b44e04c, in a quiet window the PO made by pausing the fleet
(2026-10-07 20:11 to 20:14 local), turn about, 6 rounds, fast path on,
KVM, `--smp 1`, load 0.9 to 4.3 (logs `~/.local/share/ferrix/logs/po10/quietbench.out`
and `logs/po9-obj/quiet-fsgs-{mine,base}-N.log` on nazuna): the skip 858,
1,068, 888, 1,068, 1,048, 1,068 ns; main 1,238, 1,228, 998, 1,228, 1,238,
1,238. By boot mode: the high mode 1,048 to 1,068 (4 boots) against 1,228
to 1,238 (5), **-160 to -190 ns a round trip**; the low mode 858 and 888
(2 boots) against 998 (1), -110 to -140, 2 against 1 boots, not claimed.
The consultant's code verdict: OK IF L1 to L5 (ledger line 498).

#### The parallel split and the landing order

Every piece goes to the consultant as its own landing. Two sessions can work
side by side if the files they touch do not meet. What each piece touches:

| Piece | Files |
|---|---|
| 2a | `sched/mod.rs` (`current`, `with_current`, the idle install), `smp.rs` (`PerCpu`), each `arch/*` per-CPU read, the callers |
| 2b | `sched/mod.rs` (`preempt_*`), `smp.rs` (`PerCpu`), `arch/x86_64` (the `xadd`s), `src/lib/kernel/sync` (`try_lock_manually`), the asm allowlist |
| 2c | `sched/task.rs` (`work`), `sched/work.rs` (new: `notify`, `post_own`), `trap.rs` and each `arch/*` return to ring 3, `syscall/process.rs`, `signal.rs`, `deliver.rs`, `kill.rs`, `linux.rs`, `seccomp.rs` |
| 2d | `arch/x86_64/syscall.rs` (`answer_here`), `syscall/mod.rs` (`native_call`) |
| 2e | `object/channel.rs`, `syscall/native.rs` |
| 2f | `sched/mod.rs` (`set_idle`, `choose_next`'s clones, `take_resched`, `regroup_current`), `sched/queue.rs`, `arch/speculation.rs`, `object/quota.rs` |
| 3a | `arch/x86_64/switch.rs`, `sched/mod.rs` (`switch_user_state`), `arch/x86_64/syscall.rs` (the entry mark), the native runtime, `ferrix_native_abi`'s documentation |
| 3b | `arch/x86_64/switch.rs`, `arch/x86_64/syscall.rs` (`arch_prctl`) |

`sched/mod.rs` and `smp.rs` are where the pieces meet, so one session owns
them:
- **Session A, the scheduler and the processor record:** 2b, then 2a, then
  2f, then 3a and 3b. It owns `sched/mod.rs`, `sched/queue.rs`, `smp.rs`,
  `arch/speculation.rs`, `arch/x86_64/switch.rs` and `src/lib/kernel/sync`.
  3a's mark lives in `UserState`, not in `Task`, so A never touches
  `sched/task.rs`.
- **Session B, the way out and the channel:** 2d, then 2c, then 2e. It owns
  `sched/task.rs`, the new `sched/work.rs`, `trap.rs`, the personality's
  files, `object/channel.rs` and `syscall/native.rs`. Its only line in
  `sched/mod.rs` is `mod work;` and its re-export, and the wake row is a
  comment and a check, not a change to `wake_at_home`.

Where they still meet:
- `arch/x86_64/syscall.rs`: B's 2d changes `answer_here`, and A's 3a and 3b
  change the entry's mark and `arch_prctl`. 2d is the first landing, so A
  rebases onto it.
- 2c's way out reads the per-processor resched and regroup flags that 2f
  changes. B reads them through `call_left` and `regroup_current` as they
  are; 2f changes their insides only.
- 2e needs 2c's `END` bit, so it follows 2c in B's order. 2a's borrow helps
  2c and 2e but is not needed by them: they call `current()` until 2a lands.

The order on `main`:
1. 2d (B): small, and the base for A's later x86-64 entry changes.
2. 2b (A): step 4's `try_` variants stand on it.
3. 2c (B): the largest; conditions 4 and 6 and the wake row's hook.
4. 2a (A).
5. 2e (B).
6. F-60's fix, on its own, then 2f (A).
7. 3a (A), then 3b (A). If B is free first, B takes 3b after 3a lands.

Step 4 may start once 2b, 2c and 2a are in (§9.7 part 5). 3a must be in
before step 4's case 15 can run, and 2e, 2f and 3b are for its budget, not
its correctness.

**Points, together.**

| Piece | Points | Saving a round trip | Risk |
|---|---|---|---|
| 2a `current()` by borrow | 4–6 | *guess* 0.1–0.25 us | medium: the lifetime argument, many callers |
| 2b lighter lock | 7–10 | *guess* 0.3–0.6 us | medium-high: the kernel's one lock type, per-ISA code, FX-0503 |
| 2c pending-work word | 11–15 | *guess* 0.1–0.2 us | high: every poster in the personality, signal semantics |
| 2d decode once | 1–2 | *guess* 10–30 ns | low |
| 2e channel state word | 4–6 | *guess* 0.05–0.15 us | medium: the wake order moves to the wait queue's lock |
| F-60's fix, on its own before 2f | 3–5 | none | medium: the leave's ordering, a hook |
| 2f switch writes | 5–7 | *guess* 0.05–0.15 us | medium: fences removed, the speculation words |
| 3a vector contract | 6–9 | 0.15–0.2 us, from the 0.12 us span | medium-high: an ABI change, residual information |
| 3b FS and GS (write skip dropped) | 2–3 | 0.07–0.14 us, from the 0.14 us span | low-medium: vendor behaviour of null selectors, read side only |
| this design and its revision | 4–6 | | |
| **total** | **47–69** | **about 0.8–1.7 us** | |

§9.5 gave steps 2 and 3 37 to 57 points, of which 10 to 13 were PCIDs, now
deferred. The consultant's conditions (one control per bit, the wake row's
hook, the reset's cases) add about as much as the PCIDs took away, and the
review's conditions and F-60's fix add 7 to 9 more (the draft's total was 40
to 60). With 3b's write skip dropped, the savings' sum would take the round
trip from about 3.0 us to between 1.3 and 2.2 us, against §9.5's guess of 0.9
to 1.3 us after step 3. The gap is mostly PCIDs, which nazuna cannot have, and
the address-space switch, which §9.6 measured at 130 ns a direction on both
kernels. The timing build's ablations measure each piece once it is built,
before step 4 counts on it.

#### Questions for the consultant

Answered on 2026-10-02; the answers are in the review below.

1. *2a:* is the closure form (`with_current`, whose reference cannot leave
   it) the right shape for the lifetime argument, or is a `!Send` guard type
   with the same argument preferred? May a borrow be held across a block
   inside the closure, as the argument allows?
2. *2b:* may two new assembly sequences (`xadd` to and from `GS:offset`,
   without `lock`) join the asm allowlist for x86-64's count, or must the
   count stay in Rust with interrupts masked, keeping x86-64's `pushf`/`popf`?
3. *2c:* is posting `SIGNAL` to every thread that does not block a
   process-directed signal (today's behaviour) right, or should only the
   taker get it, as in Linux?
4. *2c:* the posters are the personality's, in the `load` ring, and the
   core's way-out skip and step 4's T13 rely on them. Is an interface the
   item states, with the eleven cases and controls, enough, given that a
   missed post delays only the personality's own process? Or must the
   posters, or the conditions they post, move into the core?
5. *2f, and F1 as landed:* is the window real, where a processor installing
   a member's space as it leaves records the domain after the leaver's scan
   read it, so the grace period's interrupt wants no barrier there? If so,
   is the local check at the grace-period answer the fix, as its own landing
   before 2f? And is the finding the consultant's to number?
6. *2f:* every switch loses a full barrier (`set_idle`'s `SeqCst`
   read-modify-write and `entered_space`'s swaps). The walk found no
   argument that names it. Does the consultant know of one?
7. *2f:* is `effective_weight` in 64-bit arithmetic, with its bound proven
   and host-tested, accepted in place of §9.5's per-job cache, which would
   change the policy?
8. *§9.7 part 2a:* `quota::adjust` makes one locked add per state change,
   and more only when a level turns busy or idle. Should §9.7's last bullet
   and part 7's count be corrected in step 4's next revision, or here?
9. *3a:* is the contract x86-64 only for now, with AArch64 and ARMv7-A saving
   in full, acceptable? On AArch64 a contract would keep the low halves of
   `V8` to `V15` and `FPCR`.
10. *3a:* should the reset be claimed under FDP_RIP.2, with its refinement
    widened from frames to "the vector registers of a task resumed from a
    blocking native call", or argued under ADV_ARC as domain separation?
11. *3b:* is Linux's rule accepted, where a program that cleared its own FS
    base with a null selector on a processor that clears gets its recorded
    base back at its next switch-in? And the skip only where the boot probe
    allows it?
12. *3b:* the segment skip §9.4 item 5 dropped comes back here without its
    lock, as per-processor fields. Is that the review of item 5 the
    consultant wanted?
13. *Order:* may each piece land on its own review, in the order above, or
    does the consultant want step 2 reviewed as one landing?
14. *2c case 10:* the wake row's check needs a check-only hook in the item,
    as T13's control does. Are condition 11's rules (a static set only by
    stage 9, cleared before init, a boot check with its own control) enough
    here too, and may step 4's case 14 reuse the same hook?

#### The consultant's review (2026-10-02) and what changed

The verdict on e809d98f8 was **OK IF**: 2d OK to build; 2a, 2b, 2c, 2e, 2f
and 3a OK IF, with the conditions below; 3b OK IF its write skip is dropped.
It was made by a consultant subagent of the session running this plan, with
the seat empty, as the customer allowed (§9.5's decision 4), and is
recorded in the consultant's ledger.

**F-60, question 5 found real.** `leave_speculation_domain`'s `Release`
store of `OUT` is not ordered before `leaving_domain`'s loads of each
processor's `LAST_DOMAIN`: a store-buffer pattern against the installing
processor's read of the space's domain and its record of it. It is open on
x86-64, on ARMv7-A and in the Rust model, and closed on AArch64, where a
`STLR` is ordered before a later `LDAR`. It is pre-existing since bf9efba95,
filed in FINDINGS.md as F-60, and its fix, the local check at each
processor's grace-period answer, lands on its own before 2f. The fix's
conditions:
- (a) concurrent leaves of different domains are each seen: the published
  domains are a small set, or leaves are serialised under a sleeping lock,
  not one global word;
- (b) the publish is `SeqCst`-ordered before the grace period's generation
  increment and its interrupt, and `answer_grace` reads it after the
  generation;
- (c) a stage-9 case, through a hook under §9.7 condition 11's rules, makes
  another processor record the domain between the scan and the grace period,
  with a control that removes the local check;
- (d) L.object.116's text names the mechanism.

**The conditions, and what each changed.**
1. **2a: every write of `current`.** The processor record is written at
   every write of the queue's `current`: the boot adoption, the idle
   installation and `switch_chosen`. The stage-5 control covers the boot
   write. *2a's After the review.*
2. **2b: every context that touches the count.** On x86-64, for each entry
   vector, every path that can touch the count runs with the kernel's GS
   base or takes no `PreemptSpinLock`: the NMI, `#MC` and `#DB` through the
   paranoid entry, and an NMI in the `SYSCALL` stub before its `swapgs`. On
   Arm every exception that can take a lock is masked (ARMv7-A's FIQ with
   `cpsid if`, or an argument that it takes none; AArch64's SError and
   pseudo-NMI). On ARMv7-A, remote reads of the packed `u64` tolerate
   tearing. *2b's After the review.*
3. **2c: termination's `END` is the core's.** The core posts `END` where it
   stores `terminated` (`object::process::Exit::record`), which moves the
   task list into the core's `Process`. The personality keeps `execve`'s
   `END`, `STOP` and `SIGNAL` as the stated interface. *2c's After the
   review.*
4. **2c: the bit table checked.** In check mode the way out evaluates
   `needs_attention` with the word clear too, and stops the machine with a
   new FX code if it answers true, across the five boots, `test-shell`,
   `test-threads`, `test-vfs` and `test-init`; its control removes one
   poster. The walk of every input, writer and bit is written. *2c's After
   the review, the table.*
5. **2e: the listed-but-not-blocked window.** The writer and the closer gain
   a `SeqCst` fence between the word's store and the wake's read of the
   state, paired with the wait's; the case joins the `loom` model with a
   model-only control that drops the writer's fence. *2e's After the
   review.*
6. **2f: every remote reader of a switch word.** A table of each, with the
   ordering it relies on; regroup's `MOVES`/`RUNNING_SEEN` store-buffer
   pattern keeps a fence pair, taken only when the counts differ. *2f's
   After the review.*
7. **3a: the format and every reader.** The standard `XSAVE` form, the
   requested-feature set `XCR0` (x87, SSE, AVX), SDM Vol. 1 §13.8.1 for
   `MXCSR` loaded with an empty header; every reader of a saved area treats
   `unsaved` as the initial state with the task's own `MXCSR` and control
   word, through one accessor. *3a's After the review.*
8. **3b: the write skip leaks.** `USER_DS` then a null `FS` keeps or clears
   the base by vendor, and no selector read shows it, so a per-processor
   "last written" record can name a base no longer loaded. The read skip
   stays and the write skip goes, with a check and a control that restores
   the skip. *3b's After the review.*

**The answers to the questions.**
1. The closure form is right, and a borrow held across a block inside it is
   accepted.
2. Yes to the `xadd` sequences in the asm allowlist, under condition 2.
3. Keep today's behaviour: every thread that does not block the signal.
4. The interface suffices for `STOP`, `SIGNAL` and `execve`'s `END`;
   termination's `END` is the core's (condition 3).
5. Real: F-60, fixed on its own before 2f.
6. The two places a full barrier was relied on are F-60's installing side
   and regroup's `MOVES`/`RUNNING_SEEN` (condition 6).
7. Yes, with the host test at depths 1 to 8; the weight setters' assertion
   that every entity weight is below 2^32 becomes a requirement row.
8. In step 4's next revision.
9. x86-64 only.
10. The customer chose (2026-10-02) to widen FDP_RIP.2 to "the register
    state a program is given at every switch", covering the restore and the
    reset, with ADV_ARC describing the mechanism.
11. Linux's rule, on the read side only.
12. The segment skip does not come back. *Amended 2026-10-06 (ledger
    line 393):* except `DS` and `ES` skipped 0 to 0, compared with the
    processor's own registers, as 3b's "Reopened narrowly" says.
13. Each piece on its own review, in the proposed order, with F-60's fix
    before 2f.
14. One shared hook static, if it records which check armed it and the boot
    check verifies that every one is clear.

**Advisories.** 2b is measured on Arm before a saving is claimed there.
`TRACE` stays reserved. `FILTERED` waits on seccomp S3's install lock.

**What is left before code.** The requirement ids for each piece's rows,
reserved before the rows are written. Then each piece is built in the order
above and comes back to the consultant with the logs of its cases and
controls, F-60's fix first among 2f's.

### 9.9 Where it stands (2026-10-03, at the wind-down)

**On `main`:** step 1; F-60's fix; and 2a, 2b, 2c, 2d and 2e of step 2, each
with the consultant's OK, its checks and its negative controls. The `loom`
model of 2c's and 2e's protocols runs in `check`. The round trip inside a
domain is 2,556 ns p50 with every mitigation on (37 us before step 1, 3,021
ns after it). The target is seL4's 440 ns (§9.6); Redox's scheme round trip
is 1,965 ns without any speculative defence (§9.6a).

**2f (2026-10-06, with this text):** no global or locked writes in the
switch (§9.8). The exact `bench-ipc` landed as 3349682db; against `main`
de0eb7b32, five ABAB rounds (`bench-ipc --release --accel kvm --smp 1
--alternate main --rounds 5`, host load 2 to 4) put the domain-call p50 at
2,427 ns with 2f against 2,536 ns without it, 0.957 the median ratio
(spread 0.953 to 0.961). The customer's target since 2026-10-06 is under
400 ns.

**Not on `main`:** step 4's groundwork (`step4-prep`: the
boot switch, the park protocol's `loom` model and `ipc-equiv`).
`docs/roadmap/open-branches.md` lists what each owes. 3a, 3b, step 4's fast
path, step 5 and step 4b are not started.

**What changed in the plan on the way:**
- The target is seL4's figure itself, not 1.5 times it, so Ferrix's software
  outside the switch has about 90 ns a direction (§9.6).
- No PCID on nazuna: step 3's PCIDs wait for hardware that has them, and the
  budget's 20 ns for `CR3` is about 110 ns here, for both kernels (§9.6).
- `IBPB` costs about 230 ns a switch under KVM, not 2 us (§9.6).
- ERAPS, fewer TLB misses, global pages for shared text and FSGSBASE are
  step 5's means to the target (§9.6).
- `bench-ipc`'s p50 resolves 233 ns. 2a's, 2c's and 2e's own savings are
  unmeasured until `bench-exact` lands; 2b's 465 ns and 2e's 230 ns showed.

The session's account, with every landing and every decision, is
`docs/handover/2026-10-03-ipc.md`.

**Update 2026-10-06 (po7-ipcB): 3a and 3b built, branch `po6/step3`.**
`bench-exact` is on `main` (3349682db). 3a and 3b are one commit on it, as
§9.8's *As built* paragraphs say, with every row, check and control there.
Gated on that commit, x86-64 under KVM unless named, logs
`~/.local/share/ferrix/logs/queue/po6-ipcB-s3-<name>.log` on nazuna: `check`
(`check-5`), the boots on x86-64 under KVM and TCG (`kvm-4` ran on 6dcb56b6a,
which differs from 4a8b8dfee only in where one `cfg_attr` stands in
`sched/mod.rs` and in coverage data; the KVM boots of `thr-x86-1`,
`shell-fl-1` and `bench-2` ran on 4a8b8dfee itself), AArch64, ARMv7-A and
ARMv7-A at `--smp 2` (`tcg-1`, `a64-1`, `a32-1`, `a32s2-1`),
`test-threads` on all three (`thr-x86-1`, `thr-a64-1`, `thr-a32-1`) and
`test-shell --init ferrousli` (`shell-fl-1`), all PASSED; the seven controls
FIRED. Rebased onto 2f (`main` at 445d09420), the kernel, the native ABI
and the runtime are byte-identical to 4a8b8dfee's but for one `Verifies:`
tag, so the controls stand; `check` PASSED on the rebased tree
(`~/.local/share/ferrix/logs/queue/po7-ipcB-s3r-check-1.log`).
`bench-ipc --release --accel kvm --smp 1 --alternate main --rounds 5` on it
(`po7-ipcB-s3r-bench-1`), against `main` with 2f: `domain-call` p50 2,287 ns
against 2,427 ns, the median of five rounds turn about, ratio 0.942 (0.930
to 1.124; one round read 2,716 ns), 140 ns a round trip, below §9.8's guess
for the two together (0.22 to 0.34 us). The run's `call` row, outside a
domain, read 11,066 and 14,922 ns here in rounds 1 and 3 against about
6,200 ns at `main`, and 6,002 to 6,142 ns in the other three: taken as host
noise, to be retaken before step 4 cites it. Before the rebase, against `main`
without 2f (`bench-2`, on 4a8b8dfee): 2,397 against 2,546 ns, 0.945.

### 9.10 The budget for under 400 ns (draft, 2026-10-06, po7-ipcM)

The customer's target since 2026-10-06 is a native channel round trip
**under 400 ns p50, matched**: every mitigation on, both programs in one
speculation domain, `cargo xtask bench-ipc --release --accel kvm --smp 1`,
`domain-call`, fast path on. That is 200 ns a direction. This section says
where one direction spends its time today, what step 4 removes, what is
left after it, and whether 400 is reachable on nazuna. Nothing in it is a
design. Each step-5 means it names still goes through its own review.

**How it was measured.** The timing build `os-ipc/prof2` was refreshed
onto `main` de0eb7b32 plus `step2f` and `po6/step3` (3a, 3b), as branch
`po7/prof` (d0a378f75, never lands). It stamps the TSC at 40 points of
one direction: from one side's entry into `channel_write_read`, through
the write, the wake, the block, the switch and the other side's return,
to that side's next entry. Only a direction whose stamps came in exactly
that order is counted, and only if no `IBPB` was issued in it. So timer
switches, the general trip and the cross-domain `call` run all drop out,
and so does the direction after each count, whose caches the count
disturbs. About 20,800 directions are counted a run. The stamp's own cost
(7 to 9 ns, measured at reset) is subtracted from each span. The guest
TSC moves in steps of 44 ticks (10 ns), so a span is quoted as the mean of
its samples up to its p90. The spans add up to the direction: 1,209 to
1,300 ns net, against the 1,130 ns half of the uninstrumented `domain-call`
p50 of 2,257 ns on the same tree. Host load was 1 to 6 with the SMT
sibling idle. The logs are `~/.local/share/ferrix/logs/po7-ipcM/prof-*.log`
on nazuna. `prof-7.log` and `prof-8.log` are the figures below.

**One direction today** (ns, net of stamps):

| Span | ns |
|---|---|
| ring 3, `SYSRET`, `SYSCALL`, the stub: the client's side / the server's | 167–180 / 117–129 |
| the same for a native call that does not switch (the floor loop) | 61–86 |
| entry to `channel_write_read`: filter, early decode, vector mark, `sti`, `call_entered`, `current()`, thread and process | 58–62 |
| the handle lookup | 20–22 |
| `write_small` | 28–41 |
| the wake: drain 1–7, `wake_onto` 0–5, home lock and EEVDF insert 170–201, the timer kick 16 (p50) | 190–230 |
| `read_small` (empty), the wait's list and mark | 21–52 |
| the decision: lock 0–3, `now_nanos` 11–12, `account` 120–134, sleepers 0–4, detach and pick 69–75, bookkeeping and `arm_timer` 10–12 (p50) | 215–240 |
| `install`: domain and mask 0–4, **`CR3` 90–113**, refill 12–26, rest 0–4 | 105–145 |
| user state: save 0–2, TLS 0–1, **`DS`/`ES`/`FS` loads 105–118**, `FS` base 12–15, **`GS` load 35–43**, `GS` base 12–13, **vector reset (`XRSTOR`) 71–79**, entry stack 4 | 240–275 |
| `switch_to`, `finish_switch` | 10–15 |
| unblock, `read_small`, `record_call`, `regroup` and `call_left` | 44–72 |
| exit: the frame, the pending-work look | 45–67 |

Two figures come from the difference between the rows. *The user TLB
refill:* the client's side of ring 3 is 167 to 180 ns after a switch,
against 61 to 86 ns for the same loop with no switch, so a `CR3` write costs
about 50 to 100 ns of misses in ring 3 a direction, on top of the 90 to 113
of the write. *The timer:* `arm_timer` is a skip at p50, but on 10 to 20%
of directions it reprograms the local APIC timer, which is an exit. That is
most of why `domain-call`'s p90 is 10 to 19 us and its mean 2.5 to 3.5
times its p50.

**The budget against 400.**

| Piece, a direction | now | after step 4 | after step 5 | 200 ns target | seL4 matched |
|---|---|---|---|---|---|
| `SYSCALL`/`SYSRET`, the stub, ring 3's own loop | 60–85 | same | same | 45 | ~40 |
| user TLB refill after `CR3` | 50–100 | same | 20–40 | 20 | ~20 |
| `CR3` write, no PCID | 90–113 | same | same | 90 | ~90 |
| return-stack refill | 12–26 | same | 0 (ERAPS) | 0 | 20 |
| software: entry to exit, scheduler included | ~650 | 50–110 | 35–60 | 35 | ~50 |
| user state | 240–275 | same | 15–30 | 15 | ~0 |
| `switch_to` and its tail | 10–15 | 5 | 5 | 5 | — |
| **a direction** | **~1,130** | **~560–700** | **~210–260** | **200** | **220** |
| **a round trip** | **2,257** | **1.1–1.4 us** | **420–520** | **400** | **440** |

*Step 4* (§9.7 part 7's 80 to 115 ns of software) removes the dispatch,
the queue, the EEVDF insert, `account`, the pick and the general wake and
wait. Its part 7 also assumed user state at 12 to 20 ns. That holds only
after step 5: step 4 does not change `restore_user_state`, which measures
240 to 275. Two of part 7's figures read low against this profile. The
handle lookup measures 20 ns, not 8 to 12. And `hand_over`'s charge costs
120 ns or more if it goes through `CpuQueue::account`, so the direct switch
needs a charge of its own, a subtraction and a store.

*Step 5's means*, each with its estimated saving a direction:
- **Vector reset by `VZEROALL`** (about −60). The gate's model enables x87,
  SSE and AVX only. `VZEROALL` clears `YMM0` to `YMM15`, and an `XRSTOR`
  of the x87 component runs only when `XINUSE` (`XGETBV` 1) says it is in
  use. 3a's contract and condition 7 stand. This changes only the
  mechanism of the reset, and needs the consultant.
- **ERAPS in place of the refill** (−12 to −26). It needs `+eraps` in the
  gate's model and the consultant (§9.7 question 12).
- **The selector loads skipped null to null** (−140 to −160, the largest).
  3b's review closed this ("§9.4 item 5's segment skip does not come back
  in any form"). §9.4 item 5 had measured no difference, but this profile
  measures it as the largest single item after the scheduler. A narrower
  form may answer condition 8's leak: `DS` and `ES` only, compared with the
  selectors the same switch's save read from the processor, never with a
  per-processor record, with `FS` and `GS` bases written at every switch as
  now. It is for the consultant to reopen or not.
- **Fewer user pages a trip** (−30 to −60): the runtime's stub and the
  loop on one code page, and the stack top, message and TLS on one data
  page; 2 MiB pages for native text. **Global pages for the runtime's
  shared text** are a design of their own (§9.6).
- **FSGSBASE** (about −15 for the two base writes), with the entry's
  handling of a user-written `GS` base. It is a design of its own.
- Step 4's own residue: the lookup without the table lock, a park without
  an `Arc`, and the records' layout.

**Is under 400 reachable on nazuna?** Not with confidence. After steps 4 and
5 the estimate is 420 to 520 ns, with about 380 if every piece lands at
its best. The floor that no software change moves is about 200 ns a round
trip: two `CR3` writes without PCID (180 to 225) and two hardware entries
and exits. Under 400 then needs all of these at once:
- step 4 at the low end of its estimate (60 ns of software a direction or
  less);
- ERAPS;
- the `VZEROALL` reset;
- the selector skip;
- each side touching no more than about two user pages.

Without the selector skip, add about 300 ns a round trip. On hardware with
PCID the `CR3` write and most of the refill go (step 3's PCIDs), and 400 is
comfortably within reach.

**How seL4 gets 440 with the same protections** (§9.6): 130 ns a
direction for the switch (`CR3`, a user TLB of a page or two, its 20 ns
refill), and about 90 ns of entry and software. It switches no segment
state and, with the FPU off in sel4bench, no vector state. Its fast path
never enters a scheduler queue. Without its refill it measures 400 to 410
ns here, which is the figure Ferrix with ERAPS is really compared with.
Being under 400 means doing a direction in less software and fewer user
TLB misses than seL4, with the vector and segment state seL4 does not have.

**Measured since the draft (2026-10-06).**
- *`account`'s 120 to 134 ns*, split by the timing build (`prof-9.log`):
  - load accumulate: 5 ns;
  - the runtime and `update_curr`: 42 ns;
  - `follow_group_share`: 85 ns, mostly `effective_weight` recomputed at
    every charge.

  The direct switch's charge must not go through `follow_group_share`.
  Step 4's own profile (po7-ipc4) puts its charge at 30 ns.
- *The vector reset.* An `XRSTOR` costs about 70 ns here whatever it
  restores:
  - the whole area: 77 ns;
  - the same split into x87 and SSE+AVX: 134 ns;
  - SSE+AVX only, with x87 by `XINUSE`: 72 ns;
  - `VZEROALL` and `LDMXCSR`, with x87 by `XINUSE`: 15 ns.

  Built on `po7/step5-vec` (stacked on `po6/step3`, 340596560): -100 to
  -140 ns a round trip, alternated against `po6/step3`.
- *ERAPS*, built on `po7/step5` (529f32d51, on `main` 445d09420): -50 to
  -100 ns a round trip, alternated against `main`.
- *Step 4's fast path* (po7-ipc4, on `po7/step4`): 1,328 ns p50 against
  1,700 ns with it off. That is 400 to 450 ns of software a direction,
  spread over about 30 locked operations. Its selectors and base writes read
  43 ns, with that branch's own null-to-null selector skip, against this
  profile's 140 to 180 ns without it.
- *The consultant's verdicts* (po7-ipcM's consultant, ledger 2026-10-06):
  - ERAPS: OK IF C1 to C6, met on 529f32d51.
  - The `DS`/`ES` skip: reopened narrowly under S1 to S7. Only `DS` and
    `ES`, both exactly 0, compared with the processor's own registers read
    in the same switch, `FS` and `GS` as 3b has them, with four stage-9
    cases and their controls.
  - The `VZEROALL` reset: may be built under V1 to V7. It is built, and
    its five controls fired.

**The budget, restated with what is built.** A direction is now about:
- 1,000 to 1,100 ns on the general path, with 2f, 3a/3b, ERAPS and
  `VZEROALL`;
- about 600 ns with step 4's fast path;
- about 200 to 250 ns, if step 4's 400 to 450 ns of software comes down to
  seL4's 50 to 90 and the `DS`/`ES` skip lands.

The conclusion stands: under 400 ns needs every item at once.

*The `DS`/`ES` skip, measured (po9-sel, 2026-10-07):* 60 to 80 ns a
direction, 120 to 160 a round trip, with the fast path on (§9.8 3b,
*Reopened narrowly*). The row's -140 to -160 a direction was for all four
loads; `FS` and `GS` stay loaded at every switch, so the rest of it is not
counted.

### 9.11 The round toward seL4's 440 ns (2026-10-07, po9)

The customer's target for the round is `domain-call` p50 matched to seL4's
440 ns (every mitigation on, both programs in one speculation domain,
`ferrix.fastpath=on`). The consultant's design verdicts are its ledger's
lines 409 (G1 to G11, every stream) and 410 to 412 (one per stream). Each
stream records here what it changed and how each condition is met.

**Measuring.** `domain-call` p50 on `main` falls in one of two modes a boot,
about 1,250 ns and about 1,550 ns, on every tree; a ratio is taken within a
mode and quoted with its boots per mode. Before 4c9c078cc,
`bench-ipc --alternate` did not pass `--kernel-option` to the other tree, so
a fast-path tree was compared with the general path.

#### The object side (po9-obj; ledger line 410, O1 to O10)

**Where the time went.** The timing build `po9/obj-prof` (never lands)
stamps 24 points of `fast_write_read` and `send_direct`. The stamp costs
7 ns and the guest TSC moves in 10 ns steps, so a span reads to about 10 ns
a direction. Above that step: T2's `filter_quiet` about 20 ns
(`seccomp::quiet`'s `with_current` and `of_task`'s downcast); the way from
the task to its handle table about 24 (two `dyn` calls, `thread().process()`
and `.core()`); `reply_words` about 19 (a `memcpy` call and a byte loop).
The handle table's `try_lock`, `HandleTable::get` with its clamp, the
`Arc<Endpoint>` clone and its drop, and each half's lock read under one step
each. So the lookup and the park without an `Arc` (O3, O9) were judged
worth about 5 to 10 ns a direction against their protocol notes and checks,
and deferred (the PO, 2026-10-07); cut 3 records them as not built, with
the reason.

**Cut 1: the masked locks and the reply in registers.**
- *The halves' and the handle table's locks are held under the entry's
  interrupt mask, without the guard's full `disable` and `enable`.*
  `ferrix_sync::PreemptSpinLock::try_lock_masked` takes the same ticket
  lock without them, so it excludes every holder as `try_lock` does (host
  test `a_masked_try_lock_excludes_every_holder_and_leaves_the_count_alone`,
  whose control, the count raised, fails it). The kernel's
  `sync::try_lock_masked` wraps it in a `MaskedGuard` that still counts the
  hold as one lock on this processor's preemption word, by a plain per-CPU
  add (`sched::raise_masked`, `lower_masked`): no site record and no
  deferred decision, which a masked holder could not make. So A3
  (`require_preemption_on`, FX-0503) still stops a switch made with a half
  held (the consultant's C1, ledger line 416). Its control moves the caller's
  half's drop after `switch.switch()` and FIRED (`po9-obj-ctl-a3`).
  `send_direct` and `Process::try_with_handles_masked` are `unsafe` with the
  mask as their contract, met by the entry, which holds it until the switch.
  Nothing under them blocks or loops (D9, G4). The lock order and part 3's
  wake argument are unchanged, because the locks are the same (O4). The lock
  table above is restated.
- *`reply_words` computes the words in registers.* Each word is the
  program's `usize`, its bytes from the count on cleared by a mask (O7,
  `Small::of`'s rule). Stage 9's reply-words case holds it to the general
  composition `words_of(&sent_bytes(a), count)` for every count from 0 to
  24, with every byte distinct (L.object.167's criterion). Its control, the
  mask left out, FIRED (`po9-obj-ctl-reply`).
- Rows restated: L.object.166 (how the halves are held), L.object.167 (the
  new case), L.object.169's unit.
- Landed as 69cf2754c (consultant OK IF C1 to C3, ledger 416; OK, line 419).
  A masked hold records no site, so an FX-0503 message after one can name an
  earlier lock's site, already released (the control's message named
  `sched/task.rs:392`, a slot lock). FX-0503's catalogue entry says so
  (line 419's advisory, met in cut 2).

**Cut 2: the lookup through the task's own core process.** The fast path
reached the handle table by `caller.thread()?.process().core()`, two `dyn`
calls, each about 3.5 ns more than a direct call in this guest (po9-sched's
profile), and the loads behind them. `Task` now keeps, beside `thread`, one
reference to the core process its thread runs in, cloned once in
`Task::new` from `thread.process().core_arc()` (`Task::core_process`;
`None` for a kernel thread, as `thread` is); the personality's process
holds its core in an `Arc` for that (`Host::core_arc`). No `unsafe` is
involved. The first form cached a raw pointer and rested on two safe
traits' doc contracts, which the consultant did not accept (ledger 428,
B1). The field is declared before `thread`, so it is let go first and the
core is still freed within its personality process's drop. The lookup itself is
unchanged: `try_with_handles_masked`, then `channel_in` with its clamp, type
and rights (O1). Check: stage 9's echo start requires each spawned task's
cached core to be the one `thread().process().core()` names, which is the
check process's own (L.object.169). Its controls on this form both FIRED
with that check's message: the cache left empty (`po10-obj2-ctl-core`), and
the cache holding another process's core, each fast path echo task given
the previous echo's (`po10-obj2-ctl-other2`). This form, measured by hand
turn about against `main` 4066f41dd, 8 rounds, load 0.4 to 1.5
(`~/.local/share/ferrix/logs/po10/c2a-abab.txt`), is preliminary: in the
high mode 1,268 to 1,278 ns (7 boots) against 1,298 to 1,308 (4 boots),
about -30 ns; in the low mode 1,028 against 1,078, one boot each; base's
other three boots read 1,138 to 1,228. No mode had 5 boots a side, so §9.10
counts no figure from it (ledger 432, C3). The first form (the cached
pointer, ledger 428) measured 1,008 to 1,018 ns (4 boots) against 1,048 to
1,058 (6 boots) in the low mode, load 1 to 4
(`~/.local/share/ferrix/logs/po9-obj/c2-abab.txt`).

#### The user side (po9-user; ledger lines 413 and 456)

nazuna has no PCID, so each `CR3` write empties the user TLB and every user
page a side touches after it is a refill (s9.10). Line 413 allowed fewer
pages in the runtime (a1, U1 to U3); 2 MiB text pages (a2) were built on a
branch and measured no gain, so they were dropped; global user pages (a3)
are not yet, a design of their own.

- *The channel call inlined in every native program.* `ferrix-native`'s
  `Channel::write_read`, `decode` and its kin are `#[inline]`, `decode`'s
  failure out of line and `#[cold]`, and `Words::of` reads whole words as
  words instead of copying a run-time length into a padded buffer (a
  `memcpy` call through the GOT). On `main`, `ipc-bench`'s echo loop
  touched three text pages and the GOT a trip; now it is one text page and
  makes no call. A short last word may still compile to a `memcpy` call;
  a message of whole words makes none. Outside the item: the kernel links
  `ferrix-native-abi`, not `ferrix-native`; `ipc-bench` is unchanged (G8).
- *U2:* the host test `words_of_packs_every_length_as_one_copy_would` holds
  `Words::of` to the old padded copy for every length 0 to 24 and the
  refusal of 25; its three controls (a non-zero pad, big-endian words, the
  bound moved by one) FIRED. `test-ipc-equiv --arch all` agrees off and on.
- *U1:* alternated against 4066f41dd, `domain-call` p50 fast mode 988 to
  1,018 ns (6 boots) against 1,048 to 1,088 (6); slow mode 1,218 (2)
  against 1,238 to 1,298 (3); one boot at 1,048 in neither mode. The `call`
  line moves with it (2,836 to 2,856 in 5 fast boots, 3,116 in one, against
  2,966 to 3,096); the floor, which switches no space, does not (319). No
  second native program was timed.
- *U3:* no page, static or mapping is added or shared.

**Cut 3: T2's predicate from a live count of filtered threads (po10-obj;
design ledger line 457, D1 to D9).** T2's predicate (`seccomp::quiet`) is
asked twice a fast direction, at the entry and at the frame tail, and the
general path's `seccomp::check` asks the same first question at every call.
Its first read was a flag set once that any thread had ever held a filter,
and every boot's own checks set it, so on every boot every call made
`with_current`, `task.thread()`, the `dyn UserThread` to `dyn Any` upcast,
an indirect `type_id` call and the thread's flag load. The flag is now
`FILTERED_THREADS`, a count of the threads whose `filtered` flag is up:
raised before a thread's flag is raised (`Thread::with_seccomp`, under its
leaf lock) and as a thread is made with it raised (`Thread::with`, so a fork
child, a thread and a native child that inherit a chain are counted), given
back after the flag is lowered and in `Thread`'s drop. `quiet` and `check`
both read it through one function, `seccomp::any_filtered`, before anything
else after the probe word, so the two still agree on any call; with no
thread filtered each costs two loads. The ordering is one location changed
by `AcqRel` read-modify-writes and read `Acquire`, argued at the static
(ledger 457's D1 answer: `SeqCst` is not needed; `TSYNC` and `ptrace` reopen
it). The answer is the one it was, so L.object.169 and L.x86_64.161 keep
their words and no new id is used; L.object.171 to 178, reserved for O3 and
O9, are released.
- *The count found a leak in the check, not the product.* FX-1303's check
  now requires the count back at its value from the start once the check's
  threads have gone. On the first build it read 5: the scenario tasks of
  `entry_task` made the call their filter ends them for from frames holding
  their `Arc<Thread>`, `Arc<Process>` and `Env`, and a thread a filter ends
  never returns to the frames below the call, so five threads and their
  processes stayed for every boot's life; no check counts live threads or
  processes (BACKLOG row). The product's kill path already lets its
  references go before it leaves. The scenarios now name their last call
  (`Then::Ending`) and `scenario_task` makes it from a frame holding
  nothing; the member that kills itself drops its `Env` first. Each scenario
  is reaped before the next, and the check waits a bounded second more for
  a processor that frees a task's stack late.
- *A new case* (D2): in the heredity check the thread the chain was
  installed on is dropped while a fork child and a thread made of it live;
  `any_filtered` must still answer yes and both must still be refused
  `getppid`.
- *Controls*, each a one-line `gate.sh control`, each FIRED with the
  check's own message on x86-64 KVM on the landing tree b90da86b2:
  `po10-obj3-ctl-drop-d` (the drop's give-back removed: "the count of
  filtered threads did not come back once they had gone"),
  `po10-obj3-ctl-made-d` (the count at `Thread::with` removed: "the threads
  that kept a chain were not looked for at a call once its installer had
  gone"), `po10-obj3-ctl-raise-d` (the count at `with_seccomp`'s raise
  removed: the count's message, by the underflow at the installer's drop),
  `po10-obj3-ctl-leak-d` (the scenario's last call made holding its thread
  again: the count's message). The same four FIRED as `-c` on a2f04189d,
  whose source tree is the same.
- *Measured* by hand turn about against `main` 7b06cef25 (fast path on both
  sides, under `bench.lock`, 10 rounds,
  `~/.local/share/ferrix/logs/po10-obj/c3-abab.txt`), on a busy host (load
  16 to 69 before a boot, the protocol's quiet host was not to be had that
  evening): in the low mode 958 to 968 ns (5 boots) against 988 to 1,008
  (7 boots), about -40 to -50 ns a round trip, the four looks a trip makes
  at about 10 to 12 ns each; in the high mode 1,188 to 1,218 (5 boots)
  against 1,208 and 1,248 (2 boots). The low mode has at least 5 boots a
  side, but the host was not quiet, so §9.10 counts no figure from it until
  it is retaken on a quiet one.
- *Not built, with the reason (ledger 457, R1 and R2).* O3 and O9, the
  endpoint's `Arc` in the lookup and the park without one: the caller's
  endpoint must outlive the park, because the general continuation
  (`continue_general`: `unpark`, `receive_words`) runs on it after any wake,
  and the general path holds its `Arc` through its wait, so a sibling
  closing the handle never frees it under a waiter. Line 410's read-side
  form, bounded by the masked span, cannot cover a block, so a counted
  reference across the park is needed anyway, and the lookup's own clone is
  that reference. The park's task reference is already moved, not counted
  ("as built" 13 (a)). O8, `IN_CALL`'s raise: one per-processor store, and
  no cheaper form keeps case 11's assertion and the decline's lowering (B1).
  An ablation (`po10/obj3-abl`, never lands: the endpoint borrowed without
  its `Arc`, unsound; `IN_CALL`'s raise left out; and `quiet`'s downcast
  skipped by a cast) gives the upper bound of all three at once, measured
  the same way (8 rounds, `abl3-abab.txt`, load 28 to 53): low mode 948 to
  978 ns (5 boots) against 988 to 1,008 (3 boots), high mode 1,178 to 1,198
  (3) against 1,188 to 1,238 (5) -- no more than cut 3's own saving, which
  the downcast alone accounts for, so the `Arc` and `IN_CALL` together are
  within one 10 ns step. An upper bound, not a saving.


#### Q4, the tables noted, not asked (po10-quick; ledger line 515, Q4-C1 to C7)

Every switch to or from a task with user state asked the processor for its
tables four times: `SGDT` in `gdt::read_tls` (the save), again in
`gdt::write_tls` (the restore), and `STR` with a third `SGDT` in
`set_privilege_stack`, which then decoded the TSS descriptor. Both
instructions are microcoded; the timing-only probe (`po10-quick/probe`
47afa304f, `~/.local/share/ferrix/logs/po10-quick/boot-probe1.log`) read
`SGDT` at 31.4 ticks a loop iteration and `STR` at 22.5 against an empty loop
of 16.9, the probe's figures only.

Each processor's per-CPU record now holds two words, `gdt` and
`privilege_stack`: what asking would answer, written by `gdt::note_tables`,
which asks exactly as before (`live_table`, and `ask_privilege_stack`, the
old body of `set_privilege_stack`). It is called at the only moments the
answer can change while a record is installed: after `gdt::load`, the one
`LGDT` and `LTR` the kernel makes, and as `set_cpu_local` installs the record
(the boot processor's tables were loaded before any record existed; a
secondary's are still the start-up trampoline's, which note as zero, and
`init_secondary`'s load then notes the real ones). The trampoline's own
`lgdtl` runs before any record. `cpu::load_gdt` and `cpu::load_tss` are
narrowed to the x86-64 module, and their `# Safety` sections owe the note
after a load made with a record installed. The readers take the record and
ask past it only where it holds zero: zero means absent, never wrong.

The hazard this accepts, argued: `RSP0`'s address and the thread-local slots'
table now come from a stored pointer rather than from the processor at the
moment of use. That is the class `PerCpu.kernel_stack` already is, which the
`SYSCALL` entry trusts on every call; the two words are written only by
`note_tables`, only by their own processor, with interrupts masked, and read
only by that processor with interrupts masked. A note missing or misplaced is
caught, not merely asked past: at every processor's bring-up, after its last
note, `gdt::check::require_tables_noted` compares the record with a fresh
`SGDT` and `STR` answer, requires both non-zero, and stops the machine with
FX-0409 otherwise, in every build. That check verifies `L.x86_64.6`, which
was baselined; `L.x86_64.59`'s two-programs check and `L.x86_64.7`'s
thread-area check exercise the readers through the record.

**Q4 measured (preliminary: 3 boots a side, below G8's 5, so §9.10 does not count it yet).** Measured with os-76's long bench (`os76/p0-measure` 712cec722 on each tree, measurement only; `~/.local/share/ferrix/logs/po10-quick/perf/v2-*-join.txt`): host perf on the vCPU thread, 60 s of 50,000-trip blocks a boot, turn about with `main` 523fc3d50, 3 boots a side, the fast clock mode's blocks (core 11's SMT sibling 1 to 10% busy in them). `main`: 5,045, 5,045 and 5,041 instructions and 4,608, 4,656 and 4,608 cycles a round trip, 820, 825 and 820 ns. Q4: 5,020, 5,038 and 5,019 instructions (about -18, -0.4%) and 4,560, 4,558 and 4,510 cycles (about -80, -1.8%), 809 ns in all three (about -12 ns a round trip). Branch misses (14.0 against 14.0) do not move; the saving is the four microcoded `SGDT`/`STR` a switch.

**Q6, dead end** (no code; ledger line 507). Skipping the `FS_BASE` or
`KERNEL_GS_BASE` `WRMSR` when an `RDMSR` of the same register already reads
the target would have kept condition 8, but under KVM on the Zen 5 reference
host the read costs more than the write: the timing-only probe
(`po10-quick/probe` 47afa304f, `~/.local/share/ferrix/logs/po10-quick/boot-probe1.log`)
read `RDMSR FS_BASE` at 128.7 TSC ticks a loop iteration against 89.8 for the
`WRMSR`, and `KERNEL_GS_BASE` at 129.6 against 90.7. Bare metal and Intel
were not measured.

**Q1, withdrawn** (ledger lines 507 and 574). `now_nanos` by the counter
rate's exact reciprocal instead of two 64-bit divisions (branch
`po10-quick/q1-v2` dc97f19d1, not landed) measured slower on the reference
host, the same long bench, 3 boots a side: 5,086, 5,086 and 5,090
instructions (about +44) and 4,836, 4,810 and 4,782 cycles (about +4%) a
round trip, 851 to 869 ns, against `main`'s figures above. The probe's
estimate, about 6 ns of division a reading, is refuted by that measurement:
Zen 5's 64-bit `DIV` is cheaper than the reciprocal's multiply chain.
ARMv7-A, where a 64-bit division is a library call, was not measured. The
kernel's clock arithmetic lives in `ferrix-vdso`, which the item manifest
does not classify, with or without Q1 (`docs/BACKLOG.md`).

#### Q8, calls through the GOT (po10-quick; ledger line 533, Q8-C1 to C7)

The release kernel, a static PIE for KASLR, made 4,911 indirect calls through
a `rip`-relative slot on `main` 523fc3d50, 4,475 of them through a GOT slot:
`memcpy` 1,620, `__udivti3` 1,208, `memmove` 922, `memset` 382, `memcmp`
319, `__divti3` 17, and the context switch's 4. The cause is rustc's
`x86_64-unknown-none` target, which has no PLT by default and does not ask
for relaxable relocations, so LLVM emits `R_X86_64_GOTPCREL` for a call to a
symbol it cannot prove local, and lld may not relax that form
(SPECULATION.md §8 had said so; the figures there are restated). Shown on a
five-line crate: `-Z plt=yes` gives `R_X86_64_PLT32` and
`-Z relax-elf-relocations=yes` gives `R_X86_64_GOTPCRELX`; neither exists on
the pinned stable toolchain. A kernel built with `-Z plt=yes` under
`RUSTC_BOOTSTRAP` (an experiment, never landed) made the switch's calls
direct but kept every library call in the GOT: the prebuilt sysroot carries
the target's default through fat LTO.

What Q8 changes: `ferrix_switch` is a naked function (`#[unsafe(naked)]`,
stable since Rust 1.88) of the kernel crate instead of a `global_asm!`
symbol declared `extern "C"`, so its four call sites are `call rel32`, with
no slot and no relocation. Recorded by objdump
(`~/.local/share/ferrix/logs/po10-quick/q8-objdump.txt`): the same fifteen
instructions in the same order, with no prologue, epilogue or `endbr`; four
direct call sites; 4,907 `rip`-relative indirect calls against 4,911, and
`.rela.dyn` all `R_X86_64_RELATIVE`, 6,417 against 6,418, the switch's slot
gone (the same counts on 390d5f278 were 4,908 to 4,904 and 6,416 to 6,415,
`q8-objdump-390d5f278.txt`). The audit counts 833 traced sites in the
item against `main`'s 831. `check-unsafe-audit.py` now counts `#[unsafe(naked)]` and
`unsafe extern "ABI" fn` definitions, so the switch's obligation is traced.
The library calls stay; the certification consultant's view of the ways to
remove them (B1 an unstable flag, B2 a post-link rewriter, B3 the target
changed upstream, preferred) and of mapping the GOT read-only meanwhile is at
the ledger's line 533 and in `docs/BACKLOG.md`. po9-sched measured an
indirect call at about 3.5 ns more than a direct one.

**Q8 measured.** Measured with os-76's long bench (`os76/p0-measure` 712cec722 on each tree, measurement only; `~/.local/share/ferrix/logs/po10-quick/perf/v2-*-join.txt`): host perf on the vCPU thread, 60 s of 50,000-trip blocks a boot, turn about with `main` 523fc3d50, 3 boots a side, the fast clock mode's blocks (core 11's SMT sibling 1 to 10% busy in them). `main`: 5,045, 5,045 and 5,041 instructions and 4,608, 4,656 and 4,608 cycles a round trip, 820, 825 and 820 ns. Q8: 5,043, 5,060 and 5,030 instructions (no change, as expected: a direct call replaces an indirect one) and 4,664, 4,549 and 4,567 cycles (about -30, -0.7%, inside the run-to-run spread), 823, 806 and 820 ns. One indirect call a direction is below what this bench resolves; the change is kept for what it removes -- an indirect branch and a writable code pointer on every switch -- not for a figure.
**F-65: the caller's endpoint let go with interrupts open (po10-obj;
ledger lines 511 and 520).** Not a cut for the figure but a fix: the
endpoint reference the fast path holds across its park was let go masked on
every return after the lookup, and when another thread of the caller's
process had closed the handle meanwhile it was the end's last, so the
end's close ran in the masked span (part 2, "Nor does it close an end").
It is now let go with interrupts open on every return, and a masked close
stops the machine (FX-0535). Stage 9's case 16 makes the last reference
the parked caller's and resumes it by the frame tail and by the
continuation. The frame tail's return now opens and masks interrupts once
more a direction: about +40 to +50 instructions and +20 to +40 cycles a
round trip, measured by os-76's long bench with perf against main
523fc3d50, the median 819 to 858 ns on both sides, no p50 change resolved
(`~/.local/share/ferrix/logs/po10-obj/perf/f65b-*`).

### 9.13 ASIDs on ARMv7-A (B6 step 4; design reviewed twice, OK IF; 2026-10-08, os07-asid)

**Why.** On ARMv7-A every user space runs with ASID 0.
- `install_user_root` writes `TTBR0` and runs `TLBIASID 0`
  (`arch/armv7a/mod.rs`; `cpu::write_ttbr0`, `cpu::flush_user_tlb`).
- `uninstall_user_root` sets `EPD0`, zeroes `TTBR0`, and runs a local
  `TLBIALL` and `TLBIASID 0` (`cpu::disable_ttbr0`).

So both switches of a round trip empty the user TLB, and every page a side
touches after a switch is a walk. A kernel thread run between two programs
empties the whole local TLB, the kernel's global entries included. Skill §6
calls ASIDs likely the largest Arm win **(guessed)**. This step is item B6's
step 4 for the DK1 matrix (`docs/BOARD-BENCH.md` on `os4b/board-matrix`).
The kernel uses LPAE's long descriptors with `TTBCR.A1 = 0` (the loader's
`TTBCR`), so the ASID is `TTBR0[55:48]` (DDI 0406C.d B4.1.154) and is written
with the root in one `mcrr`.

**The reference.** Architecture citations are to the ARM Architecture
Reference Manual, ARMv7-A and ARMv7-R edition, ARM DDI 0406C.d
(Non-Confidential), whose text is kept on the gate host at
`~/.local/share/ferrix/logs/os07-asid/armarm/` (sha256 of the PDF
c7e38cd9…). The Cortex-A7's identification values are QEMU 10.2.1's
`cortex-a7` model (`target/arm/tcg/cpu32.c`: `ID_MMFR1` 0x40000000, `CTR`
0x84448003) **(read; to be confirmed on the board by the `asid` line, which
prints both)**; the Cortex-A7 TRM was not readable from here.

**The first design was wrong, and why.** The first reading (ledger line 590)
reviewed an allocator per processor, where a space could have a different
number on each processor. DDI 0406C.d B3.9.1 forbids that: "For a symmetric
multiprocessor cluster where a single operating system is running on the set
of processing elements, ARMv7 requires all ASID values to be assigned
uniquely within any single Inner Shareable domain. In other words, each ASID
value must have the same meaning to all processing elements in the system."
Neither the author nor the first reading had the manual. This reading
replaces item 1 with one allocator for the machine, Linux's shape
(`arch/arm/mm/context.c`, read, not copied); the rest of the first design
carries over where it says so.

**Not measured under TCG.** QEMU 10.2.1 empties a vCPU's whole TLB:
- whenever a 64-bit `TTBR0` write changes the ASID field
  (`target/arm/helper.c`, `vmsa_ttbr_write`);
- at every `TTBCR` write under LPAE (`vmsa_ttbcr_write`, lines 2728 to 2758:
  the first reading's N1);
- at `TLBIASID`; its `TLBIMVA` and `TLBIMVAA` drop a page for every ASID
  (`target/arm/tcg/tlb-insns.c`).

So under TCG a switch between two spaces costs the same with ASIDs or
without, and an entry that survives a change of ASID can never be seen.
Every cost figure for this step comes from the board. TCG runs show
correctness only, and only for faults that need no entry to survive a change
of ASID; the table of checks says which control fires where.

**FX-0009, the lazy TLB, is not on `main`.** `user/space.rs` says "There is
no lazy TLB", and os-35's branches are WIP. Nothing here depends on it; item
10 says what it must keep when it lands.

#### The design

1. **One allocator for the machine, with generations.** Each number means
   one space on every processor at once (B3.9.1).
   - A space keeps one tag, an `AtomicU64`: `generation << 8 | number`, 0
     until it is first installed. Number 0 is never given: it means "no
     space" (5).
   - The machine keeps a generation (from 1), and, under one spin lock (a
     leaf, taken inside the run queue lock), a map of the numbers given in
     this generation, each processor's *reserved* tag, and each processor's
     *flush pending* bit. Each processor keeps its *active* tag, an
     `AtomicU64` that its own install writes and only a rollover writes from
     elsewhere.
   - *Fast path*, with no lock: the space's tag is of the current generation,
     and swapping it into this processor's active tag returns non-zero. The
     number is the tag's low byte.
   - *Slow path*, under the lock: if the space's tag is not current, give it
     a number (below) and store the new tag; if this processor's flush is
     pending, run the flush of (2) and clear the bit; store the tag as this
     processor's active tag.
   - *Giving a number.* If the space's old number was reserved by a
     processor at the last rollover, the space keeps it in the new
     generation, and every reservation of the old tag is moved to the new
     one. Otherwise, if its old number is free in the new generation, it
     keeps that. Otherwise it takes the next free number, searching on from
     the last one given, and wrapping to 1.
   - *Rollover*, when no number is free: add one to the generation (checked
     arithmetic: an overflow is fatal, never a wrap); clear the map; for
     every processor, swap its active tag with 0 and reserve what it held,
     or, if that was 0, keep the reservation it already had; mark the
     reserved numbers taken; set every processor's flush pending. No
     processor is interrupted or waited for.
   - *Why the swap.* A processor on the fast path that read the old
     generation either swaps its active tag before the rollover's swap of
     the same word, so the rollover reserves its number and it keeps
     meaning the same space in the new generation; or swaps after it, and
     reads 0, and takes the slow path, where the lock orders it after the
     rollover. Read-modify-writes of one location are totally ordered, so
     there is no third case. That is the whole of the concurrency argument,
     and a `loom` model holds it (checks below).
   - *What a number costs.* A space's number is never given back while it
     lives or after it dies. A number comes back only at a rollover, and
     only to a processor that has flushed (2).

   *Why the tag belongs to the space and not to the root.* The tag lives in
   the `AddressSpace`, beside its `CpuMask`, and goes when it goes. A new
   space made on a reused root frame starts with tag 0, so it never inherits
   a number, just as `forget_root` keeps it from inheriting a predictor
   decision.

   *Storage and atomics.* One `u64` per space. Per processor: the active and
   reserved tags and the pending bit, in `[_; MAX_CPUS]` arrays (no eighth-
   processor limit, unlike the first design). On ARMv7-A an `AtomicU64` load
   or store compiles to an `ldrexd`/`strexd` pair, and a swap to an
   `ldrexd`/`strexd` loop, all without a barrier at `Relaxed`. The fast path
   is two loads and one swap of this processor's own word; §9.8 2f's table
   gains these rows (the active tag: written by its processor and by the
   rollover; the space's tag: written under the lock, read by any
   processor).

   *The pure part is host code.* `ferrix_paging::asid` holds the numbers'
   map, the reservations, the pending bits, the search and the rollover as a
   plain type the host tests drive. The kernel keeps that type under its lock
   and supplies the swaps of the active tags, which are its atomics.

2. **A processor's flush after a rollover, in this order.** In its first slow
   path after a rollover, before it writes any number of the new
   generation, a processor:
   1. parks the lower half: sets `TTBCR.EPD0`, `isb`, writes `TTBR0 = 0`
      (root 0, ASID 0), `isb`;
   2. runs a local `TLBIALL` and `BPIALL`, and `ICIALLU` on a core whose
      instruction cache is ASID-tagged VIVT (`CTR.L1Ip` 0b01, B4.1.42 Table
      B4-3), then `dsb nsh`, `isb`;
   3. installs as in (6).

   - Step 1 is belt and braces: the processor's outgoing number was
     reserved by the rollover, so a walk speculated through the outgoing
     space before step 3 tags its entry with a number that still means that
     space, which is Linux's argument. Parked, no walk runs at all.
   - `BPIALL`: B2.2.4 asks for predictor maintenance when the instructions
     at a virtual address change for a given ASID, which is what a reused
     number is. Linux does the same there (`local_flush_bp_all`). On the
     Cortex-A7 the predictor needs none (`ID_MMFR1.BPred` 0b0100, "requires
     no flushing at any time", B4.1.90), so it costs nothing there. It is
     not the switch barrier and is not counted as one.
   - `ICIALLU`: Linux's `flush_context` empties an ASID-tagged VIVT
     instruction cache at a rollover (`icache_is_vivt_asid_tagged`). The
     Cortex-A7's is VIPT (`CTR` 0x84448003, L1Ip 0b10), as is every core
     QEMU's `virt` gives this kernel; the bring-up reads `CTR` so a core
     that differs gets the operation rather than an argument.

3. **Shootdowns stay as they are.** ARMv7-A's `flush_tlb_page` is
   `TLBIMVAAIS` (by address, every ASID, inner shareable), and `flush_tlb` is
   `TLBIALLIS`. Each is followed by `dsb ish`, which completes the
   invalidation on every processor of the inner shareable domain (B3.10.1,
   "TLB maintenance operations and the memory order model") before
   `invalidate_pages` returns (`TLB_FLUSH_IS_BROADCAST`). That reaches every
   processor that may hold the space's entries, whatever the space's set
   says, including one that switched away and keeps them under the space's
   number. So `user/space.rs`'s take-down order holds as written: tables and
   frames go back only after the `dsb ish`, which is the last processor's
   answer.

   Shootdowns stay broadcast (L.armv7a.16): an IPI scheme scoped by the set
   would miss a processor that left the set and still holds entries. They
   stay by address for every ASID, as today.

   *Scoped by ASID, or not, in this step.* With one number per space on
   every processor an invalidation by ASID (`TLBIMVAIS` with the space's
   number, `TLBIASIDIS` for a whole space) is now sound: a processor that
   lags behind a rollover keeps its entries under old numbers unreachable
   until it flushes (second reading, Q7). This step keeps the all-ASID
   operations anyway, because that is the smaller change that is right:
   nothing in the shootdown path changes, the space's number need not be
   read under the space's lock, and no shootdown can read a number made
   stale by a rollover. The price is that a page shot down for one space
   also drops every other space's entry at that address, and a whole flush
   empties every space. Scoping shootdowns by ASID is a later step, measured
   on the board first and reviewed again (Q7). The first reading's ban on
   operations that name an ASID (A9 (ii)) is dropped with its reason, and
   `flush_user_tlb`, which nothing calls any more, is deleted.

4. **The set's meaning.** `CpuMask` is kept, and kept up exactly as today:
   joined before the root write, left after the next one.
   - On x86-64 and AArch64 it still means "may hold this space's
     translations".
   - On ARMv7-A it means "has the space installed, or had it until its last
     switch", which is no longer "may hold". Nothing on this architecture
     reads the set for a shootdown (3).

   Every place that states the old meaning is changed per architecture (A7's
   list). Keeping a processor in the set until its next flush would restore
   the meaning, but nothing would read it.

5. **Uninstall: a kernel thread runs with ASID 0 and `EPD0`, and nothing is
   flushed.** `uninstall_user_root` sets `EPD0`, `isb`, writes `TTBR0 = 0`,
   `isb`, and invalidates nothing; the processor's active tag is left as it
   is, as Linux leaves it. The outgoing space's entries stay in the TLB under
   its number. They cannot be hit while ASID 0 is current, and they are hit
   again if the same space comes back before anything removes them: the
   point of the step for a kernel thread between two programs. The change of
   `EPD0` is accompanied by a change of ASID, which is what B3.10.2 asks of a
   change to `TTBCR.EPDn` made without a TLB invalidation.

   This rests on one fact: no TLB entry tagged ASID 0 exists while a kernel
   thread runs. ASID 0 is never given to a space (1). After a processor drops
   the loader's identity map, no walk ever runs with ASID 0:
   - `disable_ttbr0` flushes with a local `TLBIALL`, at the drop and in
     `secondary_start`;
   - from then on `TTBR0` holds ASID 0 only while `EPD0` is set, and the
     `isb` between the two writes makes `EPD0` take effect first (B3.10.4,
     Example B3-5's order).

   Before the drop, the ASID 0 entries are the identity map's own, and that
   map is still live; the drop flushes them. Today's `disable_ttbr0` keeps
   its `TLBIALL` and is not changed.

   *The DK1 exception (second reading, M1 and B1).* B3.10.4's single write
   of base and ASID is stated for `TTBR0` being the only base whose tables
   hold non-global entries. Two transient mappings in the kernel's own
   (`TTBR1`) tree are non-global for part of the boot:
   - the loader's alias of its own image, on a machine whose RAM is above
     the 2 GiB split (the STM32MP157), until `drop_identity_map` in
     `finish_memory` (the loader's `load.rs`, `transient`, `global: false`);
   - the secondary entry sequence, on the same machine, until
     `CpuStarter::finish` (`armv7a/smp.rs`, `install_identity`).

   Stage 6's installs run inside that window. The hazard B3.10.4 guards
   against is a walk under the new ASID through the old tables, or the
   reverse. These entries have no old or new table: they are reached through
   `TTBR1`, which no install writes, so a walk of them under any ASID reads
   the same tables and caches the same translation. Their removal is
   `unmap_kernel`, whose `flush_tlb_everywhere` is a `TLBIALLIS` that removes
   them under every ASID at once. So an entry of theirs cached under any
   number translates as the tables do until it is removed, and none survives
   the removal **(argued)**. After `finish_memory` the boot walks the
   kernel's tree and counts its non-global leaves, which must be 0
   (L.armv7a.17's last check), so the window cannot quietly stay open.

6. **Install** (`install_user_root(root, tag)`), one `asm!` block for the
   register part:
   1. the number from (1), and the flush of (2) if this processor's bit is
      pending (both before the block);
   2. `TTBR0 = root | number << 48`, one `mcrr`, then `isb`;
   3. only if `EPD0` reads set: clear it, then `isb`. Where it is already
      clear (a switch from program to program) `TTBCR` is not written at all.
   4. no TLB maintenance;
   5. `speculation::entered_space(root)`, as today.

   - With `TTBCR.A1 = 0` and `TTBR0` the only base register for non-global
     entries, "software can update the current translation table base
     address and ASID atomically, by updating the appropriate TTBR, and does
     not require a specific routine" (B3.10.4).
   - The `isb` between the `TTBR0` write and the `EPD0` clear is Example
     B3-5's order (`PD0 = 1`, `isb`, ASID and base, `isb`, `PD0 = 0`): no
     walk can run with `EPD0` clear and the old `TTBR0`, which after an
     uninstall is root 0 with ASID 0 (the first reading's A2).
   - Skipping the `TTBCR` write saves an `mrc`, a `bic` and an `mcr` a switch
     and is the only way a `spaces` control can fire under TCG, since
     QEMU flushes at every `TTBCR` write.
   - `TTBR0` bits [x-1:0] are reserved, SBZP (B4.1.154); `CnP` exists only
     from ARMv8.2, at bit 0. A root is page aligned, so bit 0 is 0 by
     construction, and the `asid` line asserts it after an install.

   The rest of `AddressSpace::install` (the set, `left_space`,
   `entering_space`, the trip count) does not change. x86-64's and AArch64's
   `install_user_root` are not touched: `arch::mod` passes the tag only on
   ARMv7-A, and a zero-sized stand-in elsewhere.

7. **Why a dropped space's number is not reused while a processor may hold
   its entries** (H.MEM.7, restated below).
   - *Base case.* At generation 1, no TLB holds an entry tagged 1 to 255:
     the loader runs `TLBIALL` before it turns the MMU on, its identity map
     and every walk before `drop_identity_map` are ASID 0, and
     `secondary_start` runs `disable_ttbr0` (`TLBIALL`) before its processor
     installs anything. No processor starts with its flush pending.
   - *A walk's entry is tagged* with the ASID in use when it was allocated
     (Table B3-21, "Matches the ASID ... in use when the TLB entry was
     assigned").
   - *Within a generation* a number is given to at most one space (1), and
     is never given back. While its space lives, every processor puts it in
     `TTBR0` only for that space. Once the space is dropped, no live tag
     names it, so no processor writes it again in that generation.
   - *Across a rollover*, a number goes to a new space only if no processor
     reserved it, and a processor writes a number of the new generation only
     after its own flush (2). A processor that has not flushed yet runs only
     its reserved number, which still means the space it meant.
   - *The generation never goes back:* it only grows, under the lock, with
     checked arithmetic; there is no hotplug or resume, and a later one
     starts a new generation.

   So an entry tagged with a dropped space's number is never used, and it is
   gone from each processor before that processor runs the number for
   anything else. *Walk caches:* B3.10.6 lets a TLB cache table entries, and
   a maintenance operation must reach every cached entry that "includes any
   stage 1 information that would be used to translate the address". B3.10.4
   says that with `A1 = 0` a single `TTBR0` write changes base and ASID with
   no further routine, which would be false if a walk under the new ASID
   could use table entries cached under the old one. So a cached table entry
   is used only under the ASID it was cached with **(argued from those two
   sections; the manual has no sentence saying so)**.

   `AddressSpace`'s drop still frees its tables and frames without a
   shootdown. On ARMv7-A its comment, and `mm::unmap_unwalked`'s, change from
   "every processor left through the root write that dropped its entries" to
   this argument.

8. **Break-before-make: F-67.** ASIDs change no table write. Every take-down
   is still: entries out, then the shootdown, then the free. Remaps
   (copy-on-write in `fault`; `install_page` and `copy_shadow_page` with a
   replace) write the old entry invalid, write the new one, then shoot down
   before the fault returns. B3.10.1 ("Using break-before-make when updating
   translation table entries") asks for invalid, `dsb`, broadcast
   invalidation, `dsb`, new entry, when the output address changes and either
   entry is writable. The order is older than this step, and without ASIDs
   the two entries could already be held at once by another processor
   running a thread of the space. The first reading filed it as **F-67**
   (Moderate; on ARMv7-A the manual "strongly recommends", AArch64's is
   stricter). This landing fixes it on the broadcast architectures:
   `arch::break_before_make(page)` runs between the break and the make in
   each of the three paths, under the space lock. On the Arm pair it is the
   page's broadcast invalidation (`flush_tlb_page`: `TLBIMVAAIS` and
   `dsb ish` on ARMv7-A, `TLBI VAAE1IS` on AArch64), which interrupts nobody
   and so may run under the lock. On x86-64 it is nothing: its shootdown is
   by interrupt, which may not run under the lock, and the full shootdown
   after the make stays as it is there and everywhere. L.user.125 states
   the order, and a static check holds each path to it (B5).

9. **Speculation.** `entered_space(root)` runs after the `TTBR0` write, as
   today, keyed on the root and the domain. So the per-core record
   (`LAST_ROOT`, `LAST_DOMAIN`) and the barrier decision do not change, and
   every new space's root still goes through `forget_root`.
   - The Cortex-A7 needs no `BPIALL` against Spectre v2 and gets none.
   - The cores that do need it (A8, A9, A12, A17, and the A15 with
     `ICIALLU`) still get it at every switch between programs not in one
     domain. ASIDs do not tag their branch predictors, so the barrier stays
     keyed as it is.
   - Separately, B2.2.4's correctness rule: a core whose `ID_MMFR1.BPred` is
     0b0001 needs predictor maintenance at every change of ContextID, so on
     such a core every install runs `BPIALL`. Today's kernel already breaks
     B2.2.4 on a BPred 0b0010 core (A8, A9 report it), since every switch
     changes `TTBR0` without a change of ContextID and a switch inside a
     domain issues no `BPIALL`; with ASIDs every switch changes the ASID, so
     only a reused number needs it, which (2) gives.

10. **When the lazy TLB lands.** It is not needed for any of the above. Once
    it lands, a kernel thread keeps the outgoing space in `TTBR0`, with its
    number and `EPD0` clear, instead of (5). Then:
    - (a) its rule that a space's tables go only once no processor has the
      space loaded stays its own (F6), since a loaded space is walked;
    - (b) broadcast shootdowns already reach a processor that holds a space
      lazily (3);
    - (c) a lazily held number stays reserved across a rollover through the
      processor's active tag, so it keeps meaning that space, and the
      processor flushes before it runs any other number of the new
      generation.

    Its `switch_here` must go through this install and keep (2)'s order.

11. **How AArch64 would follow (nothing changes there now).**
    - The same `ferrix_paging::asid` allocator. With
      `ID_AA64MMFR0_EL1.ASIDBits` it may give 16-bit numbers (`TCR_EL1.AS`).
    - The number goes into `TTBR0_EL1[63:48]` with `TCR_EL1.A1 = 0`, and
      `CnP` stays 0.
    - Uninstall writes ASID 0 with `EPD0` set, `isb` between.
    - A pending flush parks the same way, then runs `TLBI VMALLE1`, `DSB NSH`
      and `ISB`.
    - Its shootdowns are already `TLBI VAAE1IS` and `VMALLE1IS`: broadcast,
      for every ASID.
    - The predictor rule (`ARCH_WORKAROUND_1` keyed on the root) is
      unchanged.
    - It is measured on hardware for the same reason as here.

#### Requirements (reserved: L.mm.69-70, L.armv7a.13-20; os07/asid-reserve, 5f7686639)

- **H.MEM.7**, restated for all three architectures: a page-table frame or a
  frame an unmap or a drop releases shall not return to the frame allocator
  until every TLB entry that can reach it has been invalidated, or (ARMv7-A,
  a dropped space) is tagged with a number no processor will use before its
  next full flush.
- **L.mm.69** (`ferrix_paging::asid`): the allocator shall give each number
  from 1 to 255 to at most one space in a generation, never give 0, give a
  tag of the current generation back unchanged, and keep in the new
  generation a number some processor reserved at the rollover, for the space
  that held it.
- **L.mm.70**: the allocator shall roll over exactly when no number is free,
  then reserve each processor's active tag (or its earlier reservation),
  mark every processor's flush pending, count no tag of an earlier
  generation as current, and refuse a generation past its range.
- **L.armv7a.13**: `install_user_root` shall write the root and the space's
  number into `TTBR0` in one write, synchronize it before it clears
  `EPD0`, write `TTBCR` only when `EPD0` is set, and run no TLB maintenance
  but a pending flush.
- **L.armv7a.14**: a processor whose flush is pending shall park `TTBR0`
  (`EPD0` set and synchronized, then ASID 0), and complete a local
  `TLBIALL`, `BPIALL`, and `ICIALLU` where `CTR.L1Ip` is AIVIVT, before it
  writes any number of the new generation.
- **L.armv7a.15**: `uninstall_user_root` shall set `EPD0` and synchronize it
  before it writes `TTBR0 = 0`, and invalidate nothing.
- **L.armv7a.16**: on ARMv7-A every shootdown shall be broadcast to the inner
  shareable domain and complete before what it covers is released.
- **L.armv7a.17**: every leaf the kernel writes under a user root shall be
  non-global, and once `finish_memory` has run the kernel's own tree shall
  hold no non-global leaf.
- **L.armv7a.18**: no processor shall write a dropped space's number into
  `TTBR0` before its own next full flush (under H.MEM.7).
- **L.armv7a.19**: an install shall never run a number in a generation the
  processor has not flushed for, unless it is the processor's own reserved
  number: the fast path's swap and the rollover's swap of the same active
  tag (the `loom` model).
- **L.armv7a.20**: the bring-up shall read `CTR.L1Ip` and `ID_MMFR1.BPred`,
  and add `ICIALLU` to the flush (AIVIVT) and `BPIALL` to every install
  (BPred 0b0001) where they ask.
- **L.user.55**: the statement is unchanged; the criterion: each round
  touches pages the earlier rounds did not, so on hardware a second round
  must walk. L.mm.19's criterion moves with the check.
- **L.user.56**: unchanged; it becomes the TCG-visible check that two spaces
  on one processor never share a number (QEMU does not flush when neither the
  ASID nor `TTBCR` is written).

- **L.user.125** (F-67): a remap that replaces a valid user entry with one
  to another frame -- the copy-on-write in `fault`, `install_page` with a
  replace, `copy_shadow_page` -- shall write the old entry invalid, then
  call `arch::break_before_make` for the page, and only then write the new
  entry, all under the space's lock. Parent H.MEM.1.

Reserved on `os07/asid-reserve` 5a0210b00 (L.mm.69-70, L.armv7a.13-20,
L.user.125), which po11-win lands.

#### Checks and controls

| Condition | Check | Negative control (`gate.sh control`) | Fires on |
|---|---|---|---|
| L.mm.69, 70 | host: `asid_numbers_are_unique_in_a_generation`; `asid_reserved_numbers_survive_a_rollover`; `asid_generation_refuses_overflow` (at the limit) | `next` not advanced; reservations ignored; the generation not advanced | host |
| L.mm.69, 70, L.armv7a.16, 18 | host: `asid_model_tlb_never_hits_another_space`: 2 to 4 modelled processors, a TLB per processor that keeps every entry (table entries too) until told; random new space, drop with no shootdown then frame and table reuse, install, touch, uninstall, broadcast page shootdown, rollovers. Asserts no hit reaches another space or a freed frame | as above, and: no pending flush at a rollover; a shootdown scoped to the set (a processor that left is skipped) | host |
| L.armv7a.19 | `loom` model in `src/tests/loom`: the generation and tag loads and the fast path's swap, against a rollover, the slow path's carry of a reserved tag and the pending flush; asserts no two live spaces share a number, and an unflushed processor runs only its reserved number | the swap replaced by a load and a store; the rollover leaving the active tags; reservations matched by number instead of tag | host (loom) |
| L.armv7a.13 | boot `asid` line: after an install, `TTBR0[55:48]` is the tag's number, not 0; the root is the space's; bit 0 is 0; `EPD0` clear; `spaces` (L.user.56) | install writes ASID 0 (also fails `spaces` under TCG); the kernel's `next` not advanced (`spaces`) | TCG |
| L.armv7a.13, order | static check (`check-armv7a-asid.py`) of the block's instruction order (`mcrr`, `isb`, then the conditional `TTBCR` write and `isb`), refusing any c7 or c8 operation inside it | the `isb` deleted, or the two writes swapped; a `TLBIASID` added to the block | check |
| L.armv7a.14 | `asid` line: a forced rollover adds one to the generation, sets every processor's flush pending, and each processor's flush count (kept where the flush is issued) rises by one at its next install, at one processor and at `--smp 2`; the stale probe (A runs number n and reads its page; a rollover; B is given n and reads its own page at the same address); a static order check of the park and flush block | the flush call removed: the count fires under TCG, the probe on the DK1 only (A11) | TCG, board |
| L.armv7a.15 | `asid` line: after an uninstall, `TTBR0 == 0` and `EPD0` set; the block's static order check | uninstall that only sets `EPD0` | TCG |
| L.armv7a.16 | a `const` assertion that `TLB_FLUSH_IS_BROADCAST` holds where the tags are kept; the host model's scoped control; FX-0602's check | `TLB_FLUSH_IS_BROADCAST` false: the build stops | build, host |
| L.armv7a.17 | `everything_but_execute_is_aarch64s_encoding` and `kernel_mappings_are_global_and_user_mappings_are_not`, tagged; a boot walk of a stage-6 space counting its global leaves, 0; after `finish_memory`, a walk of the kernel's tree counting its non-global leaves, 0 | `user_page` global; a kernel mapping made non-global | TCG |
| L.armv7a.18 | the host model (drop with no shootdown, then reuse; and, B6, no install changes a processor's base while keeping its number, B3.10.2) | as for L.mm.69 | host |
| L.armv7a.20 | host test of the decision from `CTR` and `ID_MMFR1`; the `asid` line prints both | the AIVIVT case decided without `ICIALLU` | host |
| L.user.55 | `check_the_processor_walks_an_installed_space`, rounds on fresh pages | install leaves `EPD0` set | TCG |
| L.user.125 (F-67) | static check (`check-armv7a-asid.py`) that each of the three paths calls `arch::break_before_make` between its `forget_in` and its `map_in`; the CoW checks (`fault_and_write_installed`, the rmap check) green on x86-64, AArch64 and ARMv7-A; the hardware effect a BACKLOG row | one of the three calls removed | check |

The forced rollover and the probe use a check-only `forget_tag` on the
space, which takes no processor and sets the space's tag to 0, so its next
install spends a number: one space stands for 255 made and dropped. It is
safe on a live space, since it only spends a number (the first reading's
A13), and a static check keeps its callers in check code. The probe runs on
the boot processor with interrupts masked, in stage 6.

Rows: `check` (host tests, loom, the static checks), ARMv7-A boots at one
processor and at `--smp 2` under TCG, `test-threads --arch armv7a`, and
whatever `cargo xtask gate-rows` names for `user/space.rs` (the x86-64 and
AArch64 boots, whose lines must not change).

**Estimate:** 25 to 30 points with the conditions; the machine-wide
allocator and its loom model add about 5 to the first reading's 20 to 25.

#### What changed since the first reading (line 590), condition by condition

- A1: ids reserved (5f7686639, for po11-win to land); the section keeps
  §9.13 by os-07's word, os07/ustate's is §9.14.
- A2: (6) and (5), with B3.10.4's Example B3-5 cited; one block each, with
  static order checks.
- A3: (7)'s base case and the generation's monotony.
- A4: (2)'s `ICIALLU` and L.armv7a.20.
- A5: citations to DDI 0406C.d throughout; `CnP` and bit 0 in (6). The
  walk-cache tagging is argued from B3.10.4 and B3.10.6, not cited; the
  Cortex-A7 TRM was not readable, and the A7's values are QEMU's model's
  until the board prints them.
- A6: H.MEM.7 restated, L.armv7a.18.
- A9: (ii) dropped with its reason in (3); the ninth-processor split is gone
  with the per-processor tags; L.armv7a.17 is the non-global row.
- New: item 1 is the machine-wide allocator, with L.armv7a.19 and its
  `loom` model; F-67's fix is in this landing (8).

#### Questions for the consultant (second reading)

- **Q6.** Item 1 replaces the reviewed design's core. Does Q2's acceptance
  carry to the machine-wide allocator, under L.armv7a.19's loom model?
- **Q7.** A9 (ii), the static ban on operations that name an ASID: drop it,
  since a number now means the same on every processor?
- **Q8.** F-67's fix here: is the control "a reading" acceptable, given
  that no emulator shows a TLB conflict?

#### The second reading (ledger line 594): OK IF (B1) to (B8)

- B1: the DK1 exception, item 5; the kernel tree's walk in L.armv7a.17.
- B2: the `loom` model as the table says; MEMORY-AND-TIMING records the
  rollover's work under the lock, O(`MAX_CPUS` + 256) with interrupts
  masked; §9.8 2f's table gets the active tags and the space's tag.
- B3: L.armv7a.18 restated as behaviour.
- B4: L.armv7a.13's block check refuses c7 and c8 operations; L.armv7a.16
  restated without "by address".
- B5: L.user.125 reserved (5a0210b00), its static check and control, F-67's
  BACKLOG row for the hardware effect, the register recounted.
- B6: the host model's base-and-number assertion.
- B7: the DK1 run (A11) prints `CTR` and `ID_MMFR1`.
- B8: below.
- Q6 yes, Q7 yes (A9 (ii) dropped), Q8 no: a static check, not a reading.

#### As built (os07/asid), and where it differs from the text above

- `ferrix_paging::asid` holds `Numbers` (the map, the reservations, the
  pending bits, the search that wraps to 1, the rollover) and `FlushPlan`.
  A rollover that leaves no number free is refused (`AsidError::NoneFree`),
  not ruled out at compile time, so the kernel's `Numbers<MAX_CPUS>` needs no
  limit on processors; the kernel stops with FX-0603 on either refusal.
- `arch/armv7a/asid.rs` keeps it under one spin lock beside the active
  tags, the generation, the flush counts and each processor's plan, decided
  by `init_this_cpu` from `init_speculation` on the boot processor and from
  `secondary_start` on the others. `SpaceTag` is the space's `AtomicU64`;
  other architectures get a zero-sized one through `arch::install_space_root`,
  and their `install_user_root` is untouched.
- `cpu.rs`: `install_ttbr0`, `park_ttbr0`, `flush_for_new_generation`,
  `invalidate_predictor`, `read_ctr`, `read_id_mmfr1`; `write_ttbr0` and
  `flush_user_tlb` are gone. Assembly +23 lines net: recounted after
  os07/ustate landed (a1e6324c2), the cap 1638 to 1661 and `cpu.rs` 107 to
  130.
- F-67's three remaps call `break_page`, which adds the page to the
  shootdown and calls `arch::break_before_make`: one helper rather than two
  lines at each site, so that `resolve` stays within the complexity ratchet.
- L.armv7a.16's check is a boot check (the `asid` line refuses a kernel
  whose shootdowns are not a broadcast) rather than a `const` assertion, so
  that its control stops a boot on its own message instead of a build.
- `check-armv7a-asid.py` runs as an xtask check step, the function that
  verifies L.user.125, since a `Verifies:` tag must sit on a check
  function; the loom model is verified through `cargo xtask loom`
  (L.armv7a.19), as L.sched.51 is.
- The loom oracle takes a processor in its install to run no program until
  its `TTBR0` write. In that window its old number is still in `TTBR0` and
  may mean another space on another processor for a few instructions: the
  install's swap has already replaced its active tag, so a rollover there
  reserves the new tag, not the old. Linux's allocators have the same window.
  What the old number can do there is a speculative walk, whose entries the
  oracle's TLB keeps and which are never hit for another space: the
  processor's flush is pending, and it flushes before it runs any number of
  the new generation **(argued; the model holds the TLB half)**.
- The boot check's other processors look at their registers from a task
  pinned to each, since `run_everywhere`'s work is taken only before the
  scheduler starts.
- The kernel tree's non-global walk prints its own line, `global`, on
  ARMv7-A only, after the `w^x` line, through `arch::check_kernel_tree_global`
  (no `target_arch` conditional in generic code, which the crate-layering
  gate refuses); x86-64's and AArch64's lines do not change. The space's tag
  is a `pub(crate)` field, read by the ARMv7-A check, for the same reason.

#### The DK1 run owed before any figure (A11, B7)

On the board, with the image built from this branch as the matrix builds
it (checks on, which every boot has):

1. Boot it and read two lines. `asid` must end "number n reused by another
   space, which read its own page" and print the board's `CTR` and
   `ID_MMFR1` (QEMU's model: 0x84448003 and 0x40000000). `global` must say
   every kernel leaf is global.
2. The control, one line in `src/kernel/src/arch/armv7a/cpu.rs`,
   `flush_for_new_generation`: the `TLBIALL` taken out of the rollover's
   flush, its count kept, so only the probe can see it. Replace

   ```
               "mcr p15, 0, {zero}, c8, c7, 0",
               "mcr p15, 0, {zero}, c7, c5, 6",
   ```

   (the pair is unique in the file) with

   ```
               "nop",
               "mcr p15, 0, {zero}, c7, c5, 6",
   ```

   and boot again. On the board it must stop with "FERRIX-PANIC stage 6
   self-check failed: asid: a space given a reused number read the page of
   the space before it". Under QEMU the same image boots: QEMU empties its
   TLB at every change of ASID, so the control cannot fire there.
3. If the control boots on the board too, the probe's entry was evicted
   before its number came back: the run is inconclusive, recorded as such,
   and the BACKLOG row stays open.

- Host, loom and static controls are run directly
  (`~/.local/share/ferrix/logs/os07-asid/ctlhost.sh`), since `gate.sh`'s
  verdict needs a `FERRIX-PANIC` or a guest's line: 14 of 14 FIRED on
  c9c7364bd (`ctl-host-c9c7364bd.log`, the two order controls rerun with the
  right expected text in `ctl-host2-c9c7364bd.log`). Boot controls go
  through `gate.sh`, tags `os07a-<commit>-c*`.

#### Where it stands

- 2026-10-08, first reading: OK IF (A1) to (A15) on the allocator per
  processor (os07-asid-cert, ledger line 590), F-67 reserved.
- 2026-10-08: DDI 0406C.d B3.9.1 requires every ASID to mean one space on
  every processor of the inner shareable domain. Line 590's acceptance of
  per-processor numbers (its Q2) is withdrawn, with A3's per-processor
  generation, A9's ninth-processor split and Q4.
- 2026-10-08, second reading of the machine-wide allocator: OK IF (B1) to
  (B8) (ledger line 594).
- 2026-10-08, code on `os07/asid`, rebased onto `main` 9ed9e8428 (the
  reservations 4ea144230 and 9ed9e8428). Gated through `gate.sh`, tags
  `os07a-<commit>-*`; the host, loom and static controls in
  `~/.local/share/ferrix/logs/os07-asid/ctl-host-*.log`. Owed: the
  consultant's code review, and the DK1 run above before any figure.
- 2026-10-08, code review on cfd99de43: OK IF (C1) to (C4) (ledger line
  610): every `os07a-cfd99de43-*` verdict in before the take, the landing
  message citing lines 590, 594 and 610 with the counts and the three first
  control variants that misfired (c2-same caught by the rollover check, not
  `spaces`; c5-epd0 fired as the page fault at 0x50002000 with the wrong
  expected text; c6-userglobal changed `user_page`, which the fault path does
  not use), a rebase with coverage carried and the register recounted if
  `main` moves, and this section's note on the shared assembly cap brought up
  to date. Still owed: the DK1 run, before any DK1 figure.

### 9.14 ARMv7-A user state: the stub's clobbers, 3a's reset and 3b (design for the consultant, os07-ustate, 2026-10-08)

Steps 3 and 5 of the Arm port order (`.claude/skills/optimize-ipc-round-trip`
§6; `docs/BOARD-BENCH.md` B6), for the DK1 matrix: (a) the runtime stub's
clobbers, (b) 3a's vector reset, (c) 3b's thread pointer kept in the task.
ARMv7-A only; AArch64 is the last paragraph and changes nothing. One design
for the three, because they share the switch's two functions, one check
file and one review. Drafted as §9.13 and renumbered: §9.13 is the
ARMv7-A ASIDs (os07-asid). The consultant's verdict changed the contract
of (a) and (b); *After the review* at the end is the design as built, and
where it differs from the draft below, it rules.

**What was read and measured first.**
- *F1. The callers hold no VFP value.* Every native program -- the only
  programs that link `ferrix_rt`, and the only ones that make native calls
  on ARMv7-A (the one other stub in the tree, `nvrm`'s `nv_call`, is an
  x86-64 `syscall`) -- is built for `armv7a-none-eabi`, soft float
  (`tools/common/xtask/src/native.rs`).
  On that target rustc 1.97.1 cannot allocate a VFP register at all:
  `clobber_abi("C")` in `trap_words` fails to build ("couldn't allocate
  output register for constraint '{s0}'"), and so does an explicit
  `out("d8") _` **(measured, scratch build, 2026-10-08)**. Rust exposes no
  Arm floating-point `target_feature` to `cfg` on stable, but `target_abi`
  is `eabi` there and `eabihf` on the hard-float targets.
- *F2. AAPCS* makes `s16`-`s31` (`d8`-`d15`) callee-saved, `d0`-`d7` and
  `d16`-`d31` caller-saved; `FPSCR`'s condition and cumulative flags are not
  preserved by a call, its mode bits (rounding, `FZ`, `DN`) are the
  program's own. rustc's `clobber_abi("C")` on Arm names `r0`-`r3`, `r12`,
  `lr`, `s0`-`s15` and `d16`-`d31`, not `d8`-`d15`.
- *F3. `FPEXC` is not program state.* A PL0 `vmrs`/`vmsr` of any VFP system
  register but `FPSCR` is UNDEFINED (ARM ARM v7-A/R, `VMRS`/`VMSR`). The
  kernel writes `FPEXC` once per core, `EN` set, at bring-up
  (`cpu::enable_user_fpu`), and never again. Cortex-A7 takes no trapped or
  asynchronous VFP exceptions: `FPEXC.EX` and `FPSCR`'s trap-enable bits are
  RAZ/WI (Cortex-A7 MPCore TRM; QEMU's model does the same).
- *F4. Today's switch* (`arch/armv7a/switch.rs`) saves `FPSCR` and
  `d0`-`d31` (256 bytes, `d0`-`d15` on a D16 core) at every switch out and
  loads them at every switch in, reads `TPIDRURO` at save and writes it at
  restore, and moves USR's banked `sp` and `lr`. `_blocked` is ignored.
- *F5. Finding: `TPIDRURW` is not switched.* It is the thread ID register
  USR mode reads **and writes** (`mcr p15, 0, rX, c13, c0, 2`). Nothing in
  the kernel saves, loads or resets it, so a value one program writes there
  is read by the next program on that core, across processes, and survives
  `execve`. That breaks H.SCHED.8 (the user state no trap saves is the
  running task's own) and FDP_RIP.2 as widened on 2026-10-02 **(argued from
  the code; control U8-c below shows it on `main`'s switch)**. Linux
  switches it at every switch out and in, inherits it at `fork` and zeroes
  it at `execve` (`arch/arm/include/asm/tls.h`: `switch_tls`, `set_tpuser`,
  `flush_tls`: "TPIDRURW is fully context-switched (unlike TPIDRURO)"). It
  is fixed here, under (c), and is for the consultant to number.
- *F6.* The kernel is soft float: no kernel code touches a VFP register but
  `switch.rs`'s save, restore and `FPEXC` enable. ARMv7-A has no fast path
  (`FAST_WRITE_READ` is false), so every resume is `choose_next`'s, through
  `sched::switch_user_state` (L.sched.54).

**(a) The contract and the stub.** Through `channel_write_read` (0x1013),
`object_wait_one` (0x1008) and `port_wait` (0x101A), made with `svc`, a
program on ARMv7-A must assume that **every** VFP register is lost --
`d0`-`d31` (`d0`-`d15` on a D16 core), `d8`-`d15` included -- and keeps
`FPSCR` whole. What it finds instead is zero in every one of them and its
own `FPSCR`. Why every register rather than AAPCS's caller-saved set, with
`d8`-`d15` kept by the kernel:
1. *The callers pay nothing.* No caller can hold a value in a VFP register
   (F1), so declaring all 32 dead costs them nothing, while keeping
   `d8`-`d15` would cost the kernel a 64-byte store at every blocked switch
   out and a 64-byte load at every switch in, for registers no caller uses.
2. *One rule for the reset*: every register the core has is zero, as
   `VZEROALL` makes every `YMM` register zero; no mixture of the task's own
   and initial values for a reader to reason about.
3. *A hard-float caller* would pay, per call of the function holding the
   trap, a `vpush {d8-d15}`/`vpop {d8-d15}` (the compiler saves a clobbered
   callee-saved register in that function's prologue) and a spill and
   reload of each value live across the trap. Measured on
   `armv7-unknown-linux-musleabihf` with `clobber_abi("C")` plus
   `d8`-`d15`, a loop keeping one double across the trap: `vpush`/`vpop` of
   eight registers and two more core registers in the prologue and
   epilogue, and per trap a `vstr`/`vldr` of the live value and one
   constant rematerialised, three instructions **(measured, scratch build)**.

The stub cannot declare a VFP clobber on its own target (F1), and needs
none. Instead `rt/src/arch/armv7a.rs` refuses to build hard float:
`#[cfg(target_abi = "eabihf")] compile_error!(..)`, whose text says to
declare `clobber_abi("C")` and `out("d8") _` to `out("d15") _` first. A
soft-float ABI with VFP code enabled by `-C target-feature` (softfp) is not
caught by that `cfg`; xtask builds native programs with no target features,
which the guard's comment records as an assumption. `ferrix_native_abi`'s
contract text gains ARMv7-A; AArch64 stays "keeps the whole state".

**(b) 3a's reset, ported.**
- *The mark.* `trap::system_call` raises `vectors_dead` in the running
  task's `UserState` (`sched::with_own_user_state`, with interrupts still
  masked from the `svc` entry) after the seccomp filter returned nothing and
  before it opens interrupts for the dispatch, for exactly the three numbers
  (`vectors_die_in`, by number, as on x86-64); and lowers it after it closes
  them again, before the outcome is written and before the generic way out
  (`regroup`, `throttle_current`, signal delivery). The early answers
  (`set_tls`, the signal returns) never raise it.
- *The save.* `save_user_state(state, blocked)`: where `blocked` and
  `vectors_dead` and the core has a VFP, only `FPSCR` is read (`vmrs`) into
  the record and the state is marked `unsaved`; `d0`-`d31` are not stored.
  Every other switch out -- runnable (preempted, even inside one of the
  three), or blocked in any other call -- saves in full, as today. So is a
  task reset and then preempted before it left the call: its registers are
  zero, saved in full.
- *The reset.* `restore_user_state(state, ..)`, for a state marked
  `unsaved`: `vmsr fpscr` with the task's own `FPSCR`, then `vldmia` of
  `d0`-`d15`, and of `d16`-`d31` on a D32 core, from a block of zeros in
  read-only data; then the mark is cleared. Nothing else clears it. Every
  resume -- a message, a close, a kill, a signal -- reaches the task through
  this one restore (F6). Loading zeros from read-only data rather than 32
  `vmov dN, rZ, rZ`: the same state, 7 assembly lines instead of 36, and
  the block is the kernel's own, so the reset cannot be pointed at a
  program's memory.
- *`FPSCR` whole* (x86-64's V1): the reset loads the task's own `FPSCR`
  every time, never compared with anything, so no other program's
  cumulative flags, condition flags or rounding mode reach it.
- *`FPEXC`* needs nothing (F3): it is no register a program can read or
  write, it stays `EN` from bring-up, and neither the save nor the reset
  touches it. No lazy switching: clearing `FPEXC.EN` to defer the reset to a
  first use would leave another program's values in the register file
  behind a trap, the LazyFP pattern (CVE-2018-3665).
- *The accessor.* `UserState::fp()`, the one reader of a saved VFP state
  (a signal frame's VFP record), answers an `unsaved` state as its `FPSCR`
  and 32 zero doubles, never the stale bytes; `set_fp()` (`sigreturn`)
  leaves the state whole. Today both run only on `capture()`d states, which
  are saved whole.
- *D16 and no VFP.* A D16 core's reset zeroes `d0`-`d15`, every register it
  has; a core with no VFP saves and resets nothing. QEMU's `cortex-a7` and
  the DK1's Cortex-A7 are VFPv4-D32, so both branches are unreachable in the
  reference configurations: argued coverage, as V6.
- *No `PKRU` analogue*: nothing in `FPSCR` widens a program's rights.

**(c) 3b on ARMv7-A, and `TPIDRURW`.**
- *`TPIDRURO` is the record's.* `set_tls` writes the register and the
  running task's record together, with interrupts masked (they are, from
  the `svc` entry, where it is answered); `execve`'s `reset_user_state`
  zeroes both with interrupts masked; `clone`'s `CLONE_SETTLS` writes the
  child's record, as today. `save_user_state` reads it no more (the `mrc`
  at save goes); `restore_user_state` writes it at every switch to a task
  with user state, never skipped (condition 8). `UserState::capture()` (a
  fork child, a signal frame), which is not the switch, reads the register,
  as x86-64's reads the MSRs.
- *`TPIDRURW` is switched (F5).* `save_user_state` reads it at every switch
  out, blocked or not; `restore_user_state` writes it at every switch in,
  never skipped by a comparison with anything: a program writes it with no
  call, so a per-processor "last written" record would name a value no
  longer held, which is condition 8's leak exactly. `capture()` carries it
  to a fork child and a thread; `execve` zeroes it, as Linux's `flush_tls`.
- *Saving:* one `mrc` less and one `mrc` and one `mcr` more a switch. (c)
  costs about two coprocessor accesses a direction against today
  **(argued)**; it is a fix, not a saving.

**Conditions, each with its check and a negative control that fires**
(stage 9, ARMv7-A, a new `arch/armv7a/switch/check.rs`, its `vectors` and
`tls` lines; programs assembled by GNU `as` for ARMv7-A, pinned with a
second program to the check's core, as x86-64's `switch::check`):
- *U1, the mark is the call's.* Check: the entry's test names exactly the
  three numbers in the native range, Linux's first 1,024 and the Arm private
  range; a program woken from 0x1013 by a message that then blocks in a
  Linux `nanosleep` keeps its pattern. Control U1-c: the lowering removed --
  "the mark outlived its call".
- *U2, only a blocked marked task is saved in part.* Check: the same program
  blocked in a Linux `nanosleep`, and preempted in user mode while it spins,
  beside a program filling every register, reads back its own pattern and
  `FPSCR`. Control U2-c: the `vectors_dead` test dropped from the save --
  "lost its vector registers to a reset".
- *U3, the reset.* Check: a victim with its pattern in `d0`-`d31` and its own
  `FPSCR` (round toward zero, `DN`, `IXC`, `Z`) blocks in 0x1013 (woken by a
  message; by `SIGUSR1` then a message, whose handler must have run) and in
  0x1008 (woken by its peer's close); a second program fills every register
  with another pattern and another `FPSCR` (round down, `FZ`, `DZC`, `N`) and
  runs; the victim reads zero in all 32 and exactly its own `FPSCR`.
  Controls: U3-a the reset replaced by clearing the mark alone -- "read
  another program's vector registers"; U3-b the second `vldmia` (`d16`-`d31`)
  removed -- the same text, from `d16` up; U3-c the `vmsr fpscr` removed --
  "did not get its own FPSCR back".
- *U4, every reader sees the reset state.* Check (boot, no program): a state
  whose doubles hold other bytes, kept with only its `FPSCR`, reads through
  `fp()` as its `FPSCR` and 32 zeros, and is whole after `set_fp`. Control
  U4-c: `fp()` answers the raw doubles -- "did not read as zero with its own
  FPSCR".
- *U5, `FPEXC`* is argued (F3), with no check: no program can name it, and
  the only write is bring-up's. A future write of it outside bring-up
  (lazy switching, trapped exceptions) comes back to review.
- *U6, `TPIDRURO` is the record's.* Checks: two programs with different
  thread pointers (`set_tls`) trade one core 1,000 times each, each reading
  its own with `mrc` after every `sched_yield`; one reads its own after a
  `nanosleep`. Controls: U6-a `set_tls` writes no record -- "did not get its
  own thread pointer back"; U6-b the restore's `TPIDRURO` write removed --
  "did not each read their own TPIDRURO".
- *U7, the write is never skipped (condition 8).* Check: one program writes
  `TPIDRURW` to W, yields, reads W, writes Z, yields, reads Z, in a loop; a
  second whose own value is W reads W after every yield. With a "last
  written" skip, the first's switch-in writes W and records it, the first
  then writes Z itself, and the second's switch-in compares W with W and
  skips: it runs on Z. Control U7-c: that skip, as one line -- "a program
  whose TPIDRURW equals the one last written ran on the value another
  program left".
- *U8, `TPIDRURW` is switched.* Check: the trading programs of U6 also each
  write their own `TPIDRURW` and read it back after every yield. Control
  U8-c: the restore's `TPIDRURW` write removed, which is `main`'s switch for
  this register -- "did not each read their own TPIDRURW". That is the
  finding shown open without the fix and closed with it.

Besides: `test-ipc-equiv --arch armv7a` (the general path's results with
the mark and the reset in it), the armv7a boots at one core and at
`--smp 2`, `test-threads` and `test-shell` on armv7a.

**Rows.** Reserve `L.armv7a.5-12` on `main` first.
- `H.SCHED.12` restated: ARMv7-A joins, with "zero in every VFP register and
  its own `FPSCR`"; AArch64 keeps the whole state.
- `H.SCHED.8` restated to name ARMv7-A's state: `TPIDRURO`, `TPIDRURW`, the
  VFP registers and `FPSCR`, USR's banked `sp` and `lr`.
- `L.armv7a.5` the `svc` entry marks the three (U1); `.6` a blocked marked
  task keeps only `FPSCR` (U3); `.7` every other switch saves and restores
  whole (U2); `.8` an `unsaved` state is reset -- every VFP register the
  core has zero, its own `FPSCR`, the mark cleared only there, `FPEXC` not
  written (U3, U5); `.9` the accessor (U4); `.10` `TPIDRURO` the record's,
  written at every switch (U6, U7); `.11` `TPIDRURW` saved at every switch
  out and written at every switch in, inherited by `fork`, zeroed by
  `execve` (U7, U8); `.12` held for the review.
- `L.sched.54`'s criterion names the ARMv7-A `vectors` line beside x86-64's.

**Assembly budget.** The tree is at its cap, 1,619 of 1,619. (b) needs
`vmrs` at a blocked save and `vmsr` and two `vldmia` at the reset, which the
soft-float kernel has no Rust spelling for: `switch.rs`'s `global_asm!`
gains `ferrix_user_fpscr` (4 lines) and `ferrix_user_fpu_reset` (7), and
`cpu.rs` the `TPIDRURW` read and write (2). The cap goes to 1,632,
`switch.rs` to 78, `cpu.rs` to 107, each entry's reason extended. The
runtime's file gains no line: its guard is Rust.

**What it saves, and how it will be measured.** Per direction, the blocked
switch out drops the 256-byte `vstmia` pair (and the cache traffic of the
task's area); the switch in loads zeros from a block both cores keep hot
instead of the task's 256 bytes. On a Cortex-A7, whose VFP moves 64 bits a
cycle, that is about 30 to 60 cycles a direction, 75 to 150 ns a round trip
at 800 MHz **(guessed)**. Instructions per round trip are a poor proxy here:
a 16-register `vstmia` is one instruction, so (b) removes about five
instructions a direction and (c) adds two; TCG's instruction count can show
that nothing else moved, not the saving. The figure is the board's,
alternated with `main` (os-07, B6).

**AArch64 would follow the same way, and nothing changes there now.** Its
native programs are built for `aarch64-unknown-none-softfloat`, so the stub
again cannot and need not declare a vector clobber, and would get the same
hard-float guard (`clobber_abi("C")` plus `v8`-`v15`, whose low halves
AAPCS64 keeps). The entry would mark the three, the save of a blocked
marked task would keep `FPCR` and `FPSR` only, and the reset would zero
`v0`-`v31` (a load from a zero block, or 32 `movi`) and load the task's own
`FPCR` and `FPSR`, only while no SVE or SME state is enabled (the analogue
of V2), with no `XINUSE` arm. `TPIDR_EL0` is written by EL0, so it stays
read at every save, as AArch64's switch already does; `TPIDRRO_EL0` is
never written by the kernel, so there is no 3b to port and no `TPIDRURW`-like
gap. Its own design goes to its own review when an AArch64 target is
measured.

**Documents.** Security Target FDP_RIP.2: ARMv7-A's reset (zero VFP
registers, own `FPSCR`) and its two thread ID registers.
VULNERABILITY-ANALYSIS: a resume path that skips the reset; a user-writable
register no switch moves (F5's class). FINDINGS: F5, numbered by the
consultant, closed by U8. `ferrix_native_abi::nr`: the contract per
architecture. Coverage: the D16 and no-VFP branches argued (other
configuration).

**Questions for the consultant.**
1. Is "every VFP register destroyed" the right contract, against AAPCS's
   `d8`-`d15` kept by the kernel, given F1 and the guard?
2. Is `FPEXC` acceptable as argued (U5), without a check?
3. F5: its number, and is U8 with its control the closing evidence?
4. The D16 and no-VFP branches as argued coverage?
5. The assembly cap to 1,632 (+13)?
6. `H.SCHED.8` and `H.SCHED.12` restated, rather than new H ids?


**After the review (os07-ustate's consultant, ledger line 582: OK IF R1 to
R9).** F1 was wrong in one place, and that changed the contract.
- *F1 corrected.* ARMv7-A enters every program as `Abi::Native`, and the
  contract is keyed on the number at the one `svc` entry, so it binds every
  program, not only the native ones. On `main` four Linux programs make
  `port_wait` and `object_wait_one` through `libc::syscall` (init,
  dirclient and authd, soft float; sessiond, built hard float for the
  Cortex-A7). Across `libc::syscall`, an ordinary C call, a hard-float
  compiler may keep values in `d8`-`d15`. "Every VFP register destroyed"
  would silently corrupt such a caller.
- *R1, the contract is AAPCS-VFP's, as x86-64's is System V's.* Through the
  three calls `d0`-`d7` and `d16`-`d31` are lost and read zero; `d8`-`d15`
  and `FPSCR`, whole, are the caller's own. The partial save stores `FPSCR`
  and `d8`-`d15` (one `vstmia`, 64 bytes); the reset loads the task's own
  `FPSCR` and `d8`-`d15` from the record and zeros into `d0`-`d7` and
  `d16`-`d31` from the read-only zero block (`d0`-`d7` alone on a D16
  core). The `ferrix_rt` guard stays for `target_abi = "eabihf"` and asks
  only for `clobber_abi("C")`, which is exactly AAPCS's caller-saved set.
  U3 also shows the victim's own `d8`-`d15` back, with a control U3-d
  ("lost its callee-saved VFP registers"). The saving estimate shrinks: a
  blocked direction still stores and loads 64 bytes, about a quarter of
  today's 256.
- *R2.* `vectors_dead` is raised and lowered only by the `svc` entry;
  `unsaved` is set by the partial save and cleared only by the reset
  (condition 7). `with_own_user_state` loses its `dead_code` expectation on
  ARMv7-A.
- *R3, `execve`.* `syscall/exec.rs` resets the user state with interrupts
  open, so `reset_user_state` writes `TPIDRURO` 0 to the register and the
  record, and `TPIDRURW` 0, in one window with interrupts masked. Check: a
  program sets both and `execve`s an image which, switched out and in
  beside a second program, reads 0 and 0. Controls R3-a (record not
  zeroed: "an execve'd image got the old program's TPIDRURO back") and R3-b
  (`TPIDRURW` not zeroed, `main`'s behaviour: "an execve'd image read the
  old program's TPIDRURW").
- *R4, `fork`.* A child, switched out and in, reads its parent's
  `TPIDRURW`. Control R4-c (`capture()` leaves it out: "a fork child did not
  inherit its parent's TPIDRURW").
- *R5, one check per row.* `FPEXC` leaves `L.armv7a.8` and is argued
  coverage. `L.armv7a.11` is `TPIDRURW` switched at every switch out and in
  (U7, U8); `L.armv7a.12` is `TPIDRURW` inherited by `fork` and zeroed by
  `execve` (R4, R3-b); `TPIDRURO`'s zeroing at `execve` joins
  `L.armv7a.10` (R3-a).
- *R6, the accessor.* `fp()` answers an `unsaved` state as its `FPSCR`,
  its own `d8`-`d15` and zero elsewhere; a state written by `set_fp` is
  whole, `unsaved` cleared, with a control of its own.
- *R7.* One build of `ferrix_rt` for an eabihf target fails with the
  guard's text, quoted in the commit; the Safety Manual records the
  assumption that native programs are built with no `-C target-feature`.
- *R8, evidence.* Every control fires with its own text on an armv7a TCG
  boot: U1-c, U2-c, U3-a to U3-d, U4-c and R6's, U6-a, U6-b, U7-c, U8-c,
  R3-a, R3-b, R4-c. The `vectors` and `tls` lines show at one core and at
  `--smp 2`; `test-ipc-equiv`, `test-threads` and `test-shell` pass on
  armv7a. The assembly cap moves by exactly the lines counted.
- *R9, documents.* F5 is **F-66** (Major), filed in FINDINGS and closed by
  U8 and R3-b; VULNERABILITY-ANALYSIS gains a resume path that skips the
  reset and a user-writable register no switch moves; FDP_RIP.2 names
  ARMv7-A's reset and both thread ID registers; `ferrix_native_abi::nr`
  states the contract per architecture.

*Landing order.* F-66 is a security fix and needs nothing of the reset, so
(c) -- `TPIDRURO` kept in the task, `TPIDRURW` switched, R3, R4 -- is its
own commit, gated and landable ahead of (a) and (b).

**Next step, not this item: lazy VFP switching, as seL4 does it.** seL4 on
ARMv7 keeps `FPEXC.EN` clear until a task's first VFP instruction traps
(`KernelHaveFPU`), so a `Call`/`ReplyRecv` round trip between two tasks
that never touch VFP pays no VFP save or restore at all. Ferrix's native
programs never touch VFP (F1), so even with R1 every blocked direction pays
a `vmrs`, a 64-byte store, a `vmsr`, a 64-byte load and 192 bytes of zeros,
and every preempted one the full 256 bytes each way -- on equal hardware,
work seL4 does not do. The design for it:
- a per-task bit "has used VFP", set on its first VFP trap (an undefined
  instruction with `FPEXC.EN` clear, which today ends the program with
  `SIGILL`);
- save and restore only for tasks with the bit; switching to a task
  without it writes `FPEXC.EN` clear, so none of the register file is
  readable to it;
- the register file then still holds the last VFP user's values. Either
  zero it when an owner switches out (a cost on every owner switch, and
  nothing stale ever stays in the registers), or reset it on the first
  trap of the next user (free until someone uses VFP, but another
  program's values sit behind the trap meanwhile). The second is what
  LazyFP (CVE-2018-3665) attacked on x86; whether any Cortex-A core reads
  VFP registers speculatively past a disabled `FPEXC.EN` must be shown
  before it is chosen, and until then the first is the one that keeps
  FDP_RIP.2 by construction.

It is larger than this item: an undefined-instruction path that tells a
VFP trap from a real one, `FPEXC` written outside bring-up (U5 comes back
to review), the signal frame and `capture()` for a task whose state is not
in the registers, `fork` and `execve` of such a task, and the bit's own
checks and controls. It is os-07's to schedule. The eager cost it would
remove is measured on the board, alternated, either way.

**As built (2026-10-08, branch `os07/ustate`).** Two commits, (c) first so
that F-66's fix can land alone: 3f1743600 (`TPIDRURO` kept in the task,
`TPIDRURW` switched, `fork` and `execve`; `L.armv7a.10-12`) and 2105a31c0
(the guard, the mark, the partial save and the reset; `L.armv7a.5-9`,
AoU-27, AoU-26 before main gave that number to drvupdated). As R1 to R9 say, with these to note for the code review:
- *The entry.* `trap::system_call` grew past the complexity floor, so
  `set_tls`'s answer is `answer_set_tls` and the marked dispatch is
  `dispatch_marked`; the mark is raised and lowered there, interrupts
  masked at both.
- *The assembly.* `ferrix_user_fpu_keep` (`vmrs`, `str`, `vstmia` of
  `d8`-`d15`) and `ferrix_user_fpu_reset` (`vmsr`, `vldmia` of `d0`-`d7`
  and, on a D32 core, `d16`-`d31` from `ZERO_DOUBLES`, a 256-byte
  read-only block, and of `d8`-`d15` from the record): 17 lines; with the
  two `TPIDRURW` accesses the cap is 1,638.
- *The check* is `arch/armv7a/switch/check.rs`, two fixtures assembled by
  GNU `as` (sources in their doc comments, logs and objects in
  `~/.local/share/ferrix/logs/os07-ustate/fixtures/` on nazuna). The
  `execve` case copies the fixture's own image to `/tmp/tls-exec` and runs
  it with `argv[0]` "z". The cases run only on a D32 core and say so
  otherwise.
- *Boot lines* (armv7a TCG, one core and `--smp 2`): `vectors  3 wakes
  from a blocking native call zeroed the caller-saved VFP registers and
  kept FPSCR and d8-d15; 3 Linux sleeps, preemptions and calls after one
  kept them all` and `tls  2000 switches between two programs' TPIDRURO
  and TPIDRURW, each its own; ...`.
- *Controls* (each a `gate.sh control` on an armv7a TCG boot, FIRED with
  the check's own text): `os07u-ctl-u1` (the lowering removed: "the mark
  outlived its call"), `-u2` (`vectors_dead` dropped from the save: "lost
  its vector registers to a reset"), `-u3a2` (`d0`-`d7` not zeroed: "read
  another program's vector registers"), `-u3b` (`d16`-`d31` not zeroed:
  the same), `-u3c` (`vmsr fpscr` removed: "did not get its own FPSCR
  back"), `-u3d` (`d8`-`d15` not stored: "lost its callee-saved VFP
  registers"), `-u4` (`fp()` answering the raw doubles: "did not read as
  zero with its own FPSCR and d8-d15"), `-r6` (`set_fp` leaving the mark:
  "was still marked for a reset"), `-u6a2` (`set_tls` writing no record:
  "did not get its own thread pointer back after a switch"), `-u6b` (the
  restore's `TPIDRURO` write removed: "did not each read their own
  TPIDRURO"), `-u7` (a "last written" skip of `TPIDRURW`: "ran on the value
  another program left"), `-u8` (the restore's `TPIDRURW` write removed,
  `main`'s switch: "did not each read their own TPIDRURW"), `-r3a`
  (`execve` leaving the record: "got the old program's TPIDRURO back"),
  `-r3b` (`execve` leaving `TPIDRURW`: "read the old program's TPIDRURW"),
  `-r4` (`capture` leaving `TPIDRURW` out: "did not inherit its parent's
  TPIDRURW"). Two first tries did not fire with their text and are kept:
  `-u3a`, the whole reset skipped, which the check caught first on
  `FPSCR` ("did not get its own FPSCR back"), replaced by `-u3a2`; and
  `-u6a`, whose boot stopped earlier at stage 13's `cpu.max` check (a
  program "held to 1050 thousandths of the wall clock" at load 23; the
  same sabotage then FIRED as `-u6a2`), a timing flake under load to be
  filed (log kept as `logs/os07-ustate/flake-cpumax-u6a.log`).
- *Gates* on 8e9edd34d, whose source differs from the code tip 2105a31c0
  only in two boot lines' text, and whose `check` (`os07u-check`) FAILED
  because AoU-26 had no `safety-requirements.json` entry, added in
  96cf002d8 (`os07u-check2` PASSED there; `-u3a2` and `-u6a2` also ran on
  96cf002d8): armv7a at one core and `--smp 2`, x86-64 KVM and TCG,
  AArch64, `test-threads`, `test-shell` and `test-ipc-equiv` on armv7a,
  and the release build, each PASSED (`os07u-*`); on the code tip 2105a31c0,
  `check` and the armv7a boots at one core and `--smp 2` with the lines
  above PASSED (`os07u-check3`, `os07u-a7-r3`, `os07u-a7-smp2-r3`); the
  commit of (c) alone passed `check` and booted armv7a (`os07u-c1-check3`,
  `os07u-c1-a7-r3`).
- *Rebased* onto `main` 9ac307691 for the consultant's C1 (ledger 598): the
  two code commits are now 2b73f85ca and ecb5896ef, with the same source
  patch-ids as 3f1743600 and 2105a31c0 (287af8c20556, f25c7418f06a); the
  assumption of use is AoU-27; the evidence was carried again and the
  generated documents remade; `check`, `check-docs` and the armv7a boots at
  one core and `--smp 2` are rerun on the landing hash (`os07u-land-*`).
