# Robustness: no program stops the machine

**The customer's order (2026-11-08):** "find out how we can make ferrix
more robust, because a process in ring 3 should never crash the whole
kernel".

This document names the classes of bug that let a program take the
machine down, the evidence for each from the N150 box on 2026-11-07/08,
how big each class is today, and the mechanism that closes it. It is the
systemic answer. The fixes for single sites belong to the sessions named
in §8, and this document does not repeat them.

Each class says what Linux does about it. That part was checked against
Linux v7.0's own source (fetched 2026-11-08 to
`~/.local/share/ferrix/robustness-ref/linux-v7.0/` from git.kernel.org).
It is cited by file and function, and it was read, not copied.

The rule it follows is the customer's of 2026-10-22: keep the kernel slim.
Each mechanism below is a small, typed thing in the kernel. Any choice of
*whom* to kill or restart, and when, is made in ring 3.

**Where it stands (2026-11-08, robustness).** This document, and the first
guard, are on branch `docs/robustness`. The guard is
`tools/common/check/check-ring0-alloc.py`, run by `cargo xtask check` as
`ring-0 allocation`. It ratchets class A's 1,382 sites (§2.3). The
certification consultant's verdicts on both are recorded in §9. R1 is
landed. R2 to R13 are open BACKLOG rows.

## 1. The principle, and what "the whole machine" means

There are three ways for a program to take the whole machine down:

1. **A kernel stop.** A panic (`fatal!`, FX-0008) or a lockup report.
   Nothing above the kernel recovers from one, because `panic = "abort"`
   leaves nothing to unwind to.
2. **A system-wide stall.** The kernel keeps running, but nothing in user
   space gets a turn. Init stops feeding `/dev/watchdog`, and FX-9008
   resets the box. The stall itself is the failure, and today the reset is
   its only answer.
3. **A global service failure.** One thing every program needs fails for
   everyone: the root filesystem answers `EIO`, the compositor dies, or
   the GPU driver wedges. Programs then die one after another, init among
   them.

Each of the three has to be closed separately. A kernel that never panics
but stalls for 15 s under one program's teardown is not robust.

## 2. Class A: an allocation that cannot fail, on a path a program drives

### 2.1 Evidence

* FX-0008 "memory allocation of 131072 bytes failed", twice: catch3 #2,
  and catch4 `eventlog-fault-relanding-1121.txt` #7 (11:17, under load).
  The first was ferrix-btrfs-vfs `Scratch::new`, found by reading the
  code. The second record names no caller, because the trace goes to the
  port only (BACKLOG, ledger 8043 MK.5).
* FX-0008 for a 204-byte path clone in `openat`, in
  `test-btrfs-pressure` (owned by btrfs-pressure).
* `docs/BTRFS-PRESSURE.md`: develop stopped with "16384 bytes failed" in
  `Shared::write_back_in_slices`, and with "192 bytes failed" in
  `Node::get`'s insert under the `nodes` spin lock.

### 2.2 Why it is a class

The kernel's `GlobalAlloc` reports an empty heap as null. A `Box`, `Vec`,
`String`, map or `format!` turns the null into the allocation error
handler, and that handler panics. The certified item has been held to
fallible allocation since F-23 (`check-fallible-alloc.py`, 43 sites
left). The rest of ring 0 was never held to it: the load ring's Linux
layer, the filesystems, sockets and interfaces, and the libraries the
kernel links. Every program reaches those paths through `openat`,
`read`, `ioctl`, `sendmsg`, a page fault on a file, or a driver's IPC.

Two things make each such site worse than a count suggests:

* **The rescue does not run everywhere.** `mm::heap_or_reclaim` and
  `heap_or_kill` (L.mm.136-138) reclaim clean cache and then kill, but
  only where the caller may block. An allocation under a
  `sync::SpinLock` can only take what the headroom refill left
  (`refill_headroom`), and so can one in an interrupt or inside a
  reclaim. The inserts under spin locks are the "192 bytes" case.
* **A large block is a contiguous run.** A 128 KiB request is an order-5
  run of frames. With memory fragmented there may be no such run even
  while frames are free above the watermark. In that case `heap_or_kill`
  correctly kills nobody, and the infallible caller panics anyway. The
  box fails this way after hours at the floor.

btrfs-pressure's findings (44cb219e1, 3c4498db2, `docs/BTRFS-PRESSURE.md`)
make the second and third points exact:

* **Under a spin lock there is neither reclaim nor kill.** With the
  preemption count at 1, `heap_may_reclaim()` is false. So any `Box`
  made under a `sync::SpinLock` guard panics with FX-0008 once the heap
  is empty. The `openat` stop was `Dentry::insert_looked_up` allocating
  under the children's lock. The rule for this sub-class is to allocate
  before the lock, or to allocate fallibly under it. tmpfs's maps and
  `move_child`'s map insert under a lock are still unaudited, and they
  are first in R7's queue.
* **The kill allocates from the heap it is meant to refill.**
  `process::live()` builds two `Vec`s, so at an empty heap the kill
  answers Refused. btrfs-pressure is finishing the fix, which serves the
  killer from the per-processor reserve (`park/heap-kill-reserve`,
  a4bcc40e0). Its general form is a rule: **a recovery path allocates
  nothing, or only from a reserve kept for it.** §6.2 (3) applies the
  same rule to filesystems.
* **Large blocks and fragmentation.** A request above 2048 bytes takes a
  buddy block of its order (128 KiB is order 5). L.mm.136 reclaims
  scattered clean pages and retries at most 8 times. There is no
  compaction, and `heap_or_kill` kills nobody while free frames are above
  the watermark. So a 128 KiB request fails with gigabytes of cache
  free, as #7 at 11:17 did.

### 2.2a What Linux does (v7.0)

* **Failure is the default, and "cannot fail" is a flag.** A Linux
  allocation returns NULL unless the caller passes `__GFP_NOFAIL`. That
  flag's contract (`include/linux/gfp_types.h`) is that the allocation
  must be able to block (`__GFP_DIRECT_RECLAIM`), must never be used where
  sleeping is forbidden, and is not supported for buddy orders above 1.
  Larger requests are told to use `kvmalloc()`. So Linux has no
  equivalent of Ferrix's implicit infallible `Box` under a spin lock.
  The kernel's coding rule is that a `GFP_ATOMIC` caller handles NULL.
* **Atomic callers get a reserve, by flag.**
  `mm/page_alloc.c:__zone_watermark_ok` lets `ALLOC_MIN_RESERVE`
  (`__GFP_HIGH`) use half of the `min` watermark. `ALLOC_NON_BLOCK`
  (`GFP_ATOMIC`) gets a quarter more, and `ALLOC_OOM` (an OOM victim on
  its way out) gets half again. `gfp_to_alloc_flags` turns the caller's
  context into these bits. Ferrix has one watermark, and it infers "may
  not block" from the preemption count (`heap_may_reclaim`) instead of
  being told. That inference is why every allocation under a
  `SpinLock` is a no-reclaim, no-kill one.
* **Scoped contexts, not inference.** `include/linux/sched/mm.h`:
  `memalloc_nofs_save`, `memalloc_noio_save` and `memalloc_noreclaim_save`
  mark a region of code, and `current_gfp_context` strips `__GFP_FS`/`IO`
  inside it. A filesystem holding its own locks is then never re-entered
  by reclaim. Ferrix's "inside a reclaim" mark (FX catalog
  `RECLAIM_IN_RECLAIM`, the kill "marked inside a reclaim") is the same
  idea for one case. R6 makes it a general scope.
* **Mempools for the paths that must make progress.**
  `mm/mempool.c:mempool_alloc_noprof` first tries the backing allocator
  without direct reclaim, then takes a preallocated element, then, if it
  may block, waits for one to be freed and retries. The writeback and
  block paths use this, so I/O that frees memory never needs fresh
  memory to start. Ferrix's `mm/reserve.rs` reserved sections are the
  same shape for the item. §6.2 (3) and the kill that allocates (§2.2)
  are the cases that need one.
* **The OOM killer does not help a large block.**
  `mm/page_alloc.c:__alloc_pages_may_oom` skips the kill for
  `order > PAGE_ALLOC_COSTLY_ORDER` (3). The reason given is that
  killing a task will not produce a contiguous run. Instead
  `__alloc_pages_slowpath` tries direct compaction first for costly
  orders, and a caller that can take virtual contiguity uses `kvmalloc`
  (`mm/slub.c:__kvmalloc_node_noprof`, falling back to vmalloc).
  Ferrix's `heap_or_kill` declining to kill above the watermark is
  therefore Linux's behaviour. What Ferrix lacks is the other half:
  neither compaction nor a `kvmalloc` fallback. R6 adds the fallback.

### 2.3 The audit (2026-11-08, develop 3c4498db2)

`python3 tools/common/check/check-ring0-alloc.py --report` gives these
counts. They use `check-fallible-alloc.py`'s token patterns and markers.
Test modules and test files are left out, and a marked site is not
counted.

| Scope | Unmarked allocating calls | Files with any | `.clone()` (not gated) |
|---|---:|---:|---:|
| The certified item, and the 6 load files it reaches (F-23's gate) | 43 | 6 | 40 |
| **Ring 0 outside the item: the new gate's scope** | **1,382** | **176 of 400** | **127** |
| of which the kernel's load ring: `fs/` | 376 | 30 | 44 |
| `interfaces/` (display, render, input, block ring, audio) | 233 | 14 | 9 |
| `syscall/` | 121 | 33 | 29 |
| `net/` | 70 | 10 | 2 |
| kernel extension crates (boards, logctl, net ring) | 24 | 7 | 1 |
| of which the linked libraries: `ferrix-vfs` | 144 | 12 | 22 |
| `ferrix-net` | 87 | 13 | 0 |
| `ferrix-procfs` | 74 | 11 | 0 |
| `ferrix-btrfs-vfs` | 46 | 3 | 8 |
| `ferrix-block`, `ferrix-sysfs` | 39 each | 5, 7 | 2, 0 |
| 24 other crates | 1 to 15 each | | |

The files with the most are `ferrix-vfs` `namespace.rs` (64),
`fs/procfs/render.rs` (56), `fs/sysfs.rs` (50),
`interfaces/display/mod.rs` (45), `interfaces/display/drm.rs` (41),
`interfaces/render/mod.rs` (35), `ferrix-vfs` `tmpfs.rs` (32),
`fs/devfs.rs` (31), `fs/socket.rs` (29) and `ferrix-btrfs-vfs` `rw.rs`
(28).

**How much a program can reach.** Nearly all of it. The load ring *is*
the system-call personality, and an interface's code runs on a driver's
or a client's message. A call graph from the entry points (the syscall
table, the fault handler, the interface dispatchers) would say exactly
which sites are reachable. That work is R7's first step, not this
document's. The honest figure today is "up to 1,382, nearly all of them
reachable". The token scan also misses `.clone()` of an owning type,
`.into()` conversions, `?` into a boxed error and callees. That is why
R12, a fault-injection row, is in the plan.

### 2.4 What happens on failure

There is one rule, with one exception at each end:

* **A system call answers `ENOMEM`.** An allocation made for a request
  fails that request, and nothing else. This matches Linux's `GFP_KERNEL`
  allocations under `__GFP_ACCOUNT`, and the item's rule.
* **A path with no error to return kills the program it serves.** A page
  fault, a signal delivery or an exit's own bookkeeping cannot answer
  `ENOMEM`. Such a path ends the requesting process the way the OOM kill
  does (`SIGKILL`, `oom_kill` counted), so its memory comes back.
  `object::oom` already does this for a fault's frame. Class A extends it
  to the kernel heap a fault needs.
* **Never a panic.** `FATAL-ALLOC:` stays what it is in the item: boot
  only, before the first program, where there is nothing to return to.

For the large-block case there are three candidate mechanisms:
compaction, a fallible-only rule above a page, or non-contiguous large
buffers. R6 takes the third: kernel requests above one page are served
virtually contiguous, from order-0 frames (`vmap.rs` already maps the
stacks this way). The heap's order-n runs then stop being something a
program's request depends on, and nothing has to move pages. Compaction
is rejected because it is a large mechanism in the core that only
lowers the odds. The fallible-only rule is kept as well: a site that
asks for more than a page through `vmap` must still handle the refusal,
because the R1 ratchet sees the call either way.

**Allocation under a live guard, found at run time (R13).** A token
scan cannot tell whether a guard is alive at a call. The kernel can. In
debug and box images, the allocator counts every allocation made with
the preemption count raised, and keeps a small ring of the callers'
return addresses, which `fatal!`'s trace code can already walk. An
xtask row boots a workload and prints the top sites. A static form, the
script tracking a `let g = x.lock()` binding to the end of its block,
follows only if the run-time list proves too slow to converge.

### 2.5 The mechanical guard (R1, this landing)

This was the decision: **a ratchet script**, the same kind of gate as
F-23's, over a per-file baseline. Three other options were rejected:

| Option | Why not now |
|---|---|
| clippy `disallowed-methods` on `Box::new`, `Vec::push` and so on | It is typed and has no false positives, which is better than tokens. But it is workspace-wide or per-crate in `.clippy.toml`, and on today's code it needs 1,382 `#[expect]`s at once. **Adopted per crate later:** once a crate's count reaches 0, its `.clippy.toml` gains the list, and the ratchet entry goes (R7). |
| `try_` APIs only, in "syscall crates" | The kernel's load is one crate, `ferrix-kernel-load`, so there is no boundary to put the rule on. |
| An allocator that never fails (a reserve big enough for everything) | It moves the stop instead of removing it. A reserve is right for atomic sections, which is R6, and wrong as a policy. |

`check-ring0-alloc.py` covers every load-ring product file of the
kernel's source that F-23's gate does not scan, and the product files of
every workspace crate the kernel links (the shipping dependencies of
`ferrix-kernel`, `-core`, `-item` and `-load`, followed through the
workspace), apart from the item's crates and `ferrix-fallible`. The
baseline is `tools/common/data/ring0-alloc-baseline.json`. The gate
fails in two cases: a file's unmarked count rises, or a new file appears
with any unmarked call. A newly linked crate counts as new files, because
linking a library into ring 0 is when its allocations become the
kernel's. A fall in a count passes, and is reported. This is deliberately
looser than the item's ratchet. The load changes in most landings, and
requiring every conversion to re-record a shared file would put a
conflict in most rebases. The next `--record`, which any landing may run,
closes the allowance a fall leaves. An R7 landing records its own
files' falls in the same commit. It runs in about a second.

`--record` only lowers (the consultant's R1.2). It writes each file's
smaller count and drops a file at 0. While any count is above the
baseline, it writes nothing and fails. A raise or a new file is a hand
edit of the JSON, and that edit is reviewed. A commit that only lowers the
baseline says "Certification: ring-0 baseline lowered only (ledger
8202)".

**The residual, stated (R1.3).** Between two records, a removal and an
addition in the same file pass with nothing printed. The consultant
showed this by swapping a `format!` for a `Box::new` in `pipe.rs`. A
fall is an allowance until the next `--record`. A change to
`check-fallible-alloc.py`'s patterns or markers re-records this baseline
in the same commit, under review (R1.6).

It is not `check-kernel-alloc-audit.py` (K16's map of *who pays* for
each kernel file's allocations, marked or not). That gate fails on any
count that moves, and it is answered by changing the count. This one asks
only whether a failure can be reported.

**Negative controls.** These hand runs are diagnosis, not evidence: a
planted `Box::new` in `src/kernel/src/fs/pipe.rs` failed the gate with
"pipe.rs: 10 -> 11", exit 1, and a `vec!` planted in `ferrix-btrfs-vfs`
`rw.rs` failed it too. The `gate.sh control` runs that count as evidence
are cited in the landing's commit. The self-test covers the dependency
walk (renames, target tables, build dependencies excluded), the count,
and `--record`'s refusal of a rise and of a new file.

The evidence, on 1ceeac1d2 through `gate.sh control ... --expect-host
... -- check`, with each edit undone afterwards:

* (a) `const _: fn() -> Box<u8> = || Box::new(1);` in `fs/pipe.rs` gave
  `control: FIRED (host): src/kernel/src/fs/pipe.rs: 10 -> 11`
  (`~/.local/share/ferrix/logs/queue/rob-r1-ctl-pipe.log`).
* (b) the same with `vec![1]` in `ferrix-btrfs-vfs`'s `rw.rs` gave
  `control: FIRED (host): src/lib/fs/btrfs/vfs/src/rw.rs: 28 -> 29`
  (`rob-r1-ctl-rw.log`).
* The `check` row passed: `run: PASSED`, with `ring0-alloc: 1382 ...
  baseline 1382` (`rob-r1-check.log`).

## 3. Class B: kernel memory a program makes the kernel spend

### 3.1 Evidence

* At the floor the box holds `Slab` 362 MiB. Separately, 3,096 MiB is
  in memory objects that no process maps, most likely the GPU's buffers
  that DXVK uploads through the Intel driver. `/proc/meminfo` counts it
  nowhere (`docs/BTRFS-PRESSURE.md`, "What the box holds during play").
* The machine-wide kill picks "the most resident pages" (`object::oom`).
  The game's kernel heap and its GPU buffers are not in that figure. A
  program that holds 3 GB through the driver is invisible to the
  selection, which can then kill an innocent large process. The AHCI
  driver was killed 0.8 s after the hog in `mp2-btp`.

### 3.2 What exists

* Job quotas (`object/quota.rs`, F-35) charge frames, native objects and
  tasks hierarchically, under `memory.max`.
* `ferrix-kmem` (F-37) charges kernel heap to the job that caused it. Its
  `Charge` token lives inside the object and is used in 38 kernel files:
  pipes, sockets, epoll, inotify, timerfd, procfs buffers, namespaces,
  semaphores and so on.

### 3.2a What Linux does (v7.0)

* `mm/oom_kill.c:oom_badness` scores a task by its resident pages, its
  swap entries and its page tables (`mm_pgtables_bytes`), then adds
  `oom_score_adj` scaled to total memory. `OOM_SCORE_ADJ_MIN` exempts
  the task. Kernel heap charged to the task's memory cgroup is not in
  the per-task score. It counts only inside a memcg OOM, where the
  cgroup's whole charge (`mm/memcontrol.c`, `__GFP_ACCOUNT` /
  `obj_cgroup`) is what is over its limit.
  `select_bad_process` walks the candidates. `__oom_kill_process` kills
  the victim and every process that shares its mm.
* `mark_oom_victim` gives the victim `ALLOC_OOM` access to reserves so
  that its exit can finish (`__gfp_pfmemalloc_flags`). `oom_reaper`
  (`oom_reap_task`, at most `MAX_OOM_REAP_RETRIES`, 10, tries) unmaps
  the victim's anonymous memory without waiting for its exit, so a
  victim stuck on a lock still gives memory back. Ferrix empties a
  victim's space in `object::oom` the same way, but its victim gets no
  reserve. That is the hole `docs/BTRFS-PRESSURE.md` names ("Linux lets
  an OOM victim use reserves to finish").
* So R8's "count charged kernel memory" goes beyond Linux's global
  killer. It is closer to the memcg killer, which kills inside the job
  that is over. The consultant's RD.7 states the constraint: score only
  what the victim's death releases.

### 3.3 The gaps, and their mechanisms

1. **Uncharged heap.** Charges are opt-in per site. The ring-0 audit
   (§2.3) names the sites. R9 makes "charged or argued" a second column
   of the same ratchet. A site that allocates per request either carries
   a `Charge` or says why it is fixed-size.
2. **GPU and display buffers.** An object a driver allocated for a
   client is charged to the driver's job, or to nobody. The render
   interface already knows the client for each buffer. R9 charges the
   client's job through `ferrix-kmem`, the way Linux charges a DRM GEM
   object to the cgroup of the process that made it.
3. **OOM selection.** The victim's score becomes its resident pages plus
   the kernel bytes and buffers its own end would release, divided by the
   page size (R8; RD.7). A charge that outlives the process, such as a
   tmpfs file, is not counted. It is one saturating sum in `object::oom`.
   No new policy enters: it is what "largest" should have meant. The
   victim also gets Linux's `ALLOC_OOM` equivalent: a reserve it may use
   to finish leaving.
4. **Protection is ring 3's.** A typed per-process
   `oom_protect: {none, last, never}` that init sets, and only init may
   set (R8). The service files say `OOMPolicy=` for hyprix, the GPU
   driver, btrfsd and lode. Today the kernel knows only "never pid 1".
   Which services are protected is init's decision.

## 4. Class C: unbounded kernel work done for one program

### 4.1 Evidence

* FX-9008 when Endfield's large Wine process is torn down by a kill or
  an early exit: catch4 #3, #5 and #6, and `eventlog-td-1-develop.txt`.
  It reproduces 3 of 3 on develop. The suspect is GPU teardown. The root
  cause is fault-relanding's.
* The records show no lockup report. A processor spinning with
  interrupts masked would have one, given `ferrix.lockup=on`, which is
  not landed. So the stall is one in which interrupts keep coming and
  user space gets no turn.

### 4.2 The mechanism that turns one long hold into a stall

`crate::sync::SpinLock` is a `PreemptSpinLock`. It raises the processor's
preemption count before it takes its ticket, so the holder stays on its
processor (`ferrix-sync`, `lock`). **A waiter also spins with preemption
off.** One task that holds a widely used lock across a long teardown
(an address space's frames, a VMO's pages, a pin table, a render node's
buffers) therefore makes every other processor that wants that lock
spin. Interrupts still arrive, timers still fire and S4a's heartbeat
still ticks, but no task runs. Init's loop never turns, and FX-9008
fires. This is a hypothesis for 2026-11-08's teardown hang, not a
finding: the hang dump fault-relanding is building will say which lock
it is. The class exists whatever the dump shows.

### 4.2a What Linux does (v7.0)

* **Waiters spin with preemption off there too.** A non-RT
  `spin_lock` disables preemption and then spins on the lock
  (`include/linux/spinlock_api_smp.h`, `__raw_spin_lock`). Ferrix's
  `PreemptSpinLock` behaves the same way. So Linux does not avoid this
  stall through its lock. It avoids it through two rules: nothing long
  is done under a spin lock, and a detector reports any CPU that has not
  scheduled for too long.
* **The soft-lockup detector** (`kernel/watchdog.c`). A per-CPU hrtimer
  (`watchdog_timer_fn`) queues `softlockup_fn` on that CPU's stopper
  thread, the highest-priority task, which touches a timestamp.
  `is_softlockup` reports when the timestamp has not moved for
  `2 * watchdog_thresh` (20 s by default): that CPU has not scheduled.
  The report names the task and prints its stack. `softlockup_panic`
  optionally panics. This is exactly the long-hold class. It is R3's
  model, and R3 needs a scheduling-progress mark rather than a clock read
  on the lock path (RD.2).
* **The hard-lockup detector.** `watchdog_hardlockup_check` sees a
  CPU's hrtimer interrupt count unchanged. The buddy form
  (`kernel/watchdog_buddy.c:watchdog_buddy_check_hardlockup`, the next
  CPU by `watchdog_next_cpu`) is S4a's ring watch, almost one for one.
* **The hung-task detector** (`kernel/hung_task.c`). `khungtaskd` wakes
  every `hung_task_timeout_secs` (120 s by default).
  `check_hung_uninterruptible_tasks` walks every thread, breaking the
  RCU lock every `HUNG_TASK_LOCK_BREAK`. `check_hung_task` reports a
  `TASK_UNINTERRUPTIBLE` task whose switch count has not moved, and
  `hung_task_panic` can turn a count of them into a panic. This is R4's
  stage-1 dump, except that Linux runs it on its own clock rather than
  at a watchdog's pre-timeout.
* **Teardown yields at every level.** `mm/mmap.c:exit_mmap` unmaps
  under `mmap_read_lock` through `mm/memory.c:unmap_vmas`, then frees the
  page tables, then removes the VMAs, with `cond_resched()` per VMA.
  `zap_pmd_range` and `zap_pud_range` `cond_resched()` per entry, and
  `zap_pte_range` drops its PTE spin lock at each PMD. Pages are freed
  in batches through `mm/mmu_gather.c` (`tlb_next_batch`,
  `MAX_GATHER_BATCH_COUNT`), with a `cond_resched()` per batch in the
  free loop. `fs/file.c:close_files` `cond_resched()`s while it closes
  an exiting task's descriptors. Ferrix's `Drop for AddressSpace` has
  none of these points. R5 adds them.

### 4.3 Where today's teardown is unbounded

* `Drop for AddressSpace` (`user/space.rs`) unmaps every region and
  detaches every object in one pass, with no rescheduling point. A 5 GB
  space is about 1.3 M pages.
* A VMO's last drop gives back all its frames. A GPU buffer's goes
  through the render interface's pins.
* An exit closes every descriptor in one pass.
* `munmap` of a huge range is one call.

### 4.4 The mechanisms

1. **Teardown in task context, in bounded steps (R5).** An address
   space, a VMO or a descriptor table is torn down in steps of at most N
   pages or handles (start at 512). Between steps no lock is held and
   the task may be preempted (`sched::yield_now` when a reschedule is
   pending). It runs in the exiting task or the reaper, never under a
   `SpinLock` guard. Linux does the same with `cond_resched()` in
   `unmap_page_range`'s callers and `zap_page_range`'s batching. A
   bound written into `MEMORY-AND-TIMING.md` replaces "the whole space".
2. **A long-hold detector (R3), Linux's soft lockup in Ferrix's
   terms.** Each processor bumps a counter only when its preemption count
   goes from 0 to held, and keeps the `track_caller` line that raised it,
   which FX-0503 already does. There is no clock read on the lock path
   (RD.2). The tick that S4a guarantees reports a raised count whose
   counter has not moved across consecutive ticks. Past about 1 s it
   prints a `held` line with the line, the task and the duration, rate
   limited, from fixed storage. On x86-64 it adds the holder's stack
   through S4a's NMI path. On Arm the line says "no stack". It observes
   only: it never stops or kills. This is the instrument that finds the
   class, and it gives R5 its list.
3. **Spinning waiters are not changed.** A ticket waiter holds its
   ticket from the `fetch_add` on, so it cannot be switched out without
   recreating the convoy that c7413ba fixed (ferrix-sync's own comment;
   the consultant's A1). Linux's non-RT waiters spin with preemption off
   too. The fix is no long holds (R5) and a detector that finds them
   (R3), not a waiter change. A lock redesign would be its own reviewed
   item.

### 4.5 S4a, the parked lockup detector: land it

`s4-lockup` (55ee08384, then c57deecca on the crate split, 397 commits
behind develop) carries S4a: per-processor heartbeats, a ring watch, a 10
s bound and the NMI report. Its S4c part, `/dev/watchdog` and init's
feeder, has landed as POSTMORTEM §6.11. **Recommendation: land S4a**,
rebased and off unless `ferrix.lockup=on`, with the box's command line
asking for it. There are two reasons:

* It catches the stop S4c cannot: every processor masked, so the
  watchdog's own task never runs either. On the box that stop is
  silence until someone cycles the plug.
* Its 1 s tick on every processor is the clock that R3's long-hold
  detector needs. Without it, a tickless idle processor is watched by
  nobody.

It is a core change. It owes more than the design review's D1-D14
(ledger line 2889): the code reviews at lines 3002 and 3018 (R1-R3), 5607
W11-W13, and 8093 SW.12(b), which asks for one watch tick reconciled with
the deadline task that has landed. This document does not land it. R3
rides on top of it.

### 4.6 R5's design: fork and teardown of a big process

**What stopped the box.** Endfield (5 GB resident) forks for its embedded
browser: Wine's `CreateProcess` forks twice, then `execve`s.
`AddressSpace::fork` held the space's `SpinLock`, preemption off, across
work in proportion to the resident pages. That work was `Vmo::fork`, then
`unmap_copied_on_write`, then a shootdown. The game's 40 threads faulting
the same space spun on that lock with preemption off on every other
processor, so all of user space stopped for about 10 s (`ef-load/runs/kb-5`:
the sampler's gap from 179.87 to 189.91 s, and card0 "longest 11589 ms").
The game's own hang watchdog then ended its main thread. Teardowns of big
spaces stalled the same way.

**What it cost, measured.** `cargo xtask stall-probe` (below) on x86-64
under KVM with 4 processors forks a 320 MiB, 40-thread process four times,
then kills it:

| | develop 8ccdab764 | step 1 |
|---|---:|---:|
| another process's worst lateness, forks | 826.9 ms | 79.6 ms |
| the same, teardown phase | 400.5 ms (most likely the post-fork storm's tail) | 0.06-29.9 ms (five runs, below) |
| longest fork | 869 ms | 107-400 ms (the fork thread waits for a processor) |
| `Vmo::fork` under the lock | - | 31-34 ms |
| write-protect under the lock | - | 3.2-4.6 ms |
| single-page shootdowns over 5 ms after the forks | 2,853 | 415 |

Linux 7.0 on the gate host itself (24 processors, the same program run as
`threads-test --stall-probe-only`): forks 3.0 ms, teardown 0.09 ms, the
longest fork 23 ms.

Two causes were not where fault-relanding's notes put them:

* **The unmap was quadratic.** `Mapper::unmap_range_named` pruned after
  each leaf, and `is_empty` read the leaf's table from entry 0 to find it
  not yet empty: about 131,000 reads per full table of 512 pages. That was
  the "full 4-level walk per page, ~5 µs/page". It hit `fork`, `munmap` and
  `Drop` (`unmap_unwalked`) alike. (Develop's 400 ms in the probe's
  teardown phase was most likely not the teardown but the tail of the
  post-fork fault and shootdown storm: with the per-leaf prune put back on
  the branch, the phase measured 0.16 ms. A long hold on one processor
  does not move the probe's ticker on another, so the teardown budget has
  no control that fires (ledger 8421 C1). R5-step2 owes a teardown check
  with the ticker on the teardown's own processor.)
* **The shootdowns after the fork were reuses.** The child exits at once,
  so nearly every page of the parent is held alone again by the time the
  parent writes it. Each write still took the page down, mapped it again and
  ran a single-page shootdown. Forty threads queued on the one shootdown turn.

**What Linux does (v7.0, read, not copied).** `mm/mmap.c:dup_mmap` holds
`mmap_write_lock(oldmm)`, a sleeping lock, for the whole copy. Faulting
threads sleep on it; they do not spin. `mm/memory.c:copy_pte_range` takes
the page-table spin lock per PMD and breaks it every 32 units of progress
when `need_resched()` or `spin_needbreak()`, with `cond_resched()` between
PMDs. `copy_present_ptes` write-protects the source entry in place
(`wrprotect_ptes`), and one `flush_tlb_mm(oldmm)` at the end of `dup_mmap`
covers it all. A write by the only holder reuses the page in place
(`wp_page_reuse` through `ptep_set_access_flags`) with no remote flush;
another processor's stale read-only entry costs it one spurious fault
(`fix_spurious_fault`). On teardown, `mm/mmap.c:exit_mmap` runs
`unmap_vmas` with an `mmu_gather` (`tlb_gather_mmu_fullmm`), and
`zap_pmd_range`/`zap_pud_range` `cond_resched()` per entry. `mm/mmu_gather.c`
frees pages in batches (`tlb_next_batch`, `MAX_GATHER_BATCH_COUNT`) with a
`cond_resched()` per batch.

**Step 1 (this landing): cheap work under the same locks.** It changes no
lock and no order:

1. *Linear prune* (`ferrix-paging`): `unmap_range_named` prunes a leaf's
   tables once the walk leaves that table, or at the range's end, or before
   an error returns. It no longer prunes after every leaf. The tables freed
   and the leaf each is named with are the same as before.
2. *Write-protect in place* (`Mapper::write_protect_range`,
   `mm::write_protect_in`, `AddressSpace::protect_copied_on_write`). Fork
   clears the write permission of every present leaf in a copy-on-write
   region, holes stepped whole, a run in one table read along it. Every
   other bit is kept: the per-architecture `Encoding::write_protected`
   clears x86-64's `RW`, or sets `AP[2]` on AArch64 and ARMv7-A LPAE.
   Then one whole-space shootdown, as before (`TlbPages::everything`).
   Taking a permission away needs no break-before-make on Arm, and Arm's
   invalidation is broadcast. A read after a fork now hits. A write takes
   the copy-on-write branch that a read after the old unmapping led to.
3. *Batched references* (`mm::share_frames`). `Vmo::fork` takes the shared
   pages' references 128 at a time, in one hold of `FRAMES` each, instead
   of one interrupt-masked hold per page. The unwind gives back exactly
   what the new map holds, as before.
4. *Reuse in place* (`Mapper::allow_write`, `mm::allow_write_in`). A
   copy-on-write write whose writer holds the page alone, already mapped
   read-only to that frame, sets the write bit where it is and invalidates
   the faulting processor's own entry under the lock. There is no
   shootdown, because no frame is given back and no translation changes its
   frame. A processor still holding the read-only entry faults once more and
   finds the page writable (`allow_write_in` answers yes for a page already
   writable), as Linux's spurious-fault path does. On x86-64 a page fault
   also drops the faulting entry. On Arm the page invalidation is broadcast.
   The same applies to a private file mapping's shadow page. It is taken
   only when the present leaf decodes to what the old path would install,
   less the write permission: a user page with the region's execute
   permission, on normal memory, not global (`reusable_in_place`). Anything
   else takes the old path. It is L.user.229.

**Step 1's TLB argument (ledger 8299, A4).** In the reuse the frame does not
change and nothing is given back. This space is the page's only holder, so
the frame goes back to the allocator only through this space's own unmap,
forget or teardown. Each of those takes the translation down and shoots it
down first (`give_back`, `forget_in`, and `Drop`'s rule that no processor is
left in the set). A processor that cached the read-only entry can reach
only that frame, read-only, and a write through it faults. The fault comes
back to the copy-on-write branch, because a region's `cow` is only ever
set, never cleared (`ferrix_vma`), and `allow_write_in` answers yes for a
page that is already writable.

The faulting processor's own entry is invalidated under the lock. On
x86-64, `invlpg` reaches the current PCID only, and a page fault also drops
the entry that faulted (Intel SDM vol. 3A, 4.10.4). A space that is not
current on this processor keeps a stale read-only entry under its PCID, and
that entry faults once more when the space runs, which ends the same way.
On AArch64 and ARMv7-A, `flush_tlb_page` is `dsb ishst` and then a
broadcast invalidation of the page for every ASID (`TLBI VAAE1IS`,
`TLBIMVAAIS`). Changing only a permission needs no break-before-make.

The write-protect and the reuse rewrite a live leaf with a plain read and
write. That is safe because the hardware never writes a leaf this kernel
made. On x86-64 every leaf is written with its accessed bit set, and a
writable one with its dirty bit too. On Arm the access flag is set in every
leaf, and `TCR_EL1.HA` and `HD` are clear.

**Who writes a user frame after a software translation (A5).** A present
read-only leaf is now the usual state after a fork, so each of these paths
has to be checked:

* `AddressSpace::with_page` and `with_present_page`, which carry
  `syscall/uaccess.rs`'s copies and the futex word's. They let a write
  through only when `writable_in_place` holds: a copy-on-write page with
  more than one holder faults first. They read the holders, not the leaf's
  write bit, so a read-only leaf held alone is written through the direct
  map, which is right for a sole holder.
* `lift/core/fill.rs`, the lift personality's fill. It copies the leaf's
  own write bit into the shadow tables, so a read-only leaf stays read-only
  there, and a write faults back to the space.
* Pinned driver buffers (`Vmo::hold`). A held page is copied at fork, never
  shared (L.user.24), so no write-protect reaches it.
* Ring 0 writing through a user address directly. On x86-64 `CR0.WP` is
  set (`arch::kernel_write_protected`), so a read-only leaf faults. On Arm
  the unprivileged accesses are checked against EL0's permissions.

**Where it stands (2026-11-08).** The restricted-window build
(`personality=user`, `restricted_fastpath=on`, `restricted_window=on`,
`--no-claim`) compiled and booted on x86-64 KVM, with xtask exiting 0. That
is build evidence only, not a passing row: a user-personality boot prints
no FERRIX-BOOT-OK, so gate.sh marks it FAILED (ledger 8421 C4). Step 1 was
reviewed OK IF (ledger 8299:
design A1-A6, code B1-B10, budget C1-C3, id D). Step 2's design was
reviewed OK IF E1-E10, and its code review is still owed. The BACKLOG row is
R5-step2. The stall-probe's budgets are interim: 100 ms forking and 50 ms
tearing down (ledger 8299 C, 8324 CT.1), both lowered to 10 ms by step 2.
Five consecutive runs of step 1 (x86-64, KVM, 4 processors) measured the
forks at 76.1, 81.6, 78.4, 73.5 and 68.9 ms and the teardown at 16.0, 29.9,
0.06, 28.4 and 28.2 ms. The teardown's ~28-30 ms has no cause named yet.
Instrumentation that is not on the branch suggested that every
`AddressSpace` and `Vmo` drop took under 3 ms at preemption count 0, and
that the rest is run-queue lateness while the 40 threads exit. That is a
hypothesis, not evidence (ledger 8324 CT.3). Finding the cause is R5-step2's.

**Step 2 (designed, for its own review): fork in bounded steps.** After
step 1, `Vmo::fork` of 320 MiB still holds the space lock for about 33 ms
(400 ns a page, most of it in `fallible::insert`'s reserve per entry).
Faulting threads spin on that lock on every processor, which is the 79.6 ms
left. To bound it:

* *A fork gate.* Under the space lock, fork sets `forking` on the space.
  Every path that takes the space lock to change the space's pages first
  checks the gate under the lock: the faults, `advise`, `with_page`'s retry
  and the native map calls. If the gate is set, the path drops the lock and
  sleeps on the space's wait queue until fork clears it. A path that may not
  sleep (one holding a spin lock) is listed and argued, or refused. Those
  paths are the analogue of Linux's sleeping `mmap_lock`: waiters sleep, so
  the rest of the machine runs.
* *Chunks.* With the gate set and `layout` held (fork already holds it),
  each private object is forked in chunks of at most 512 pages
  (`Vmo::fork_chunk(from) -> next`). Each chunk holds only that object's
  `pages` lock; the space lock is not held. Then the map is cloned and the
  regions write-protected in chunks of 512 pages, each under the space lock.
  The whole-space shootdown follows, then the gate is cleared and the
  waiters are woken. Between chunks no lock is held and the preemption
  count is 0 (RD.5), so the fork thread can be preempted.
* *Consistency.* No fault, `advise` or map call changes the parent while
  the gate is set. The objects' page lists then change only by what does
  not go through the space: `forget_pages`, driven by a shared object,
  which fork does not fork but shares. A user write through a translation
  still writable reaches a frame the child may share. That is Linux's
  window too, until `flush_tlb_mm`. Every write that completes is in the
  child or ordered after the fork, because a write to a page already
  write-protected waits at the gate.
* *Teardown.* `Drop for AddressSpace` already runs with no lock of its own,
  in the reaper or at a retirement. Its unmap is now linear. A step at N
  regions or 512 pages with `sched::yield_now` when a reschedule is pending
  and the preemption count is 0 (RD.5's count-0-only steps) bounds it. Where
  the count is raised (the reaper's preemption window) the drop goes to the
  reaper's own list instead, so it never steps with the count raised.
* *Expected.* The lock holds per chunk are about 0.2 ms, against the 10 ms
  budget step 2 sets for the row.

**Step 3 (later):** an object-level copy-on-write fork (a shared page list
copied on first write), which makes `Vmo::fork` O(1). Not needed for the
budget once step 2 lands.

**The row.** (Budgets: see where it stands, above.) `cargo xtask stall-probe` boots `threads-test` built with
its `stall-probe` feature as init, with 1536 MiB of guest memory. A ticker
process sleeps 1 ms at a time and keeps its longest gap in each phase. A
game process maps and writes 320 MiB, starts 40 writing threads, forks four
times (each child `_exit`s), and is then killed and reaped. The row fails
when the ticker's lateness passes `FORK_BUDGET_US` in the fork phase or
`TEARDOWN_BUDGET_US` in the teardown phase. **Its control is today's code**: on develop 8ccdab764 it
FIRED (`threads: FAILED stall-probe: another process waited past the
budget while a big process forked`, 826,855 µs;
`~/.local/share/ferrix/logs/r5-stall-probe-develop-8ccdab764.log`). After
step 1 a `gate.sh control` that puts back the per-leaf prune and the
unmapping fork must fire again. The same program runs on Linux as
`threads-test --stall-probe-only`, for the comparison.

## 5. Class D: a wedged ring-3 service that everything depends on

### 5.1 Evidence

* catch1 `hang3`: lode, auth and hyprix died of `SIGBUS` and `EIO` after
  the btrfs abort. Init's loop then stalled and FX-9008 reset the box.
* catch4 #4: FX-9008 armed by `fxdiag`, after a `SIGQUIT` to the game's
  wine64-preloader.

### 5.2 What FX-9008 does today

Init feeds `/dev/watchdog` from its event loop. When the deadline
passes, the kernel's watchdog task reports FX-9008 (who armed it, the
timeout, the time since the last feed, and the log's tail) and then
halts, or resets under `ferrix.panic=reset` (`src/kernel/src/watchdog.rs`).
There is one stage, and it is the last one. The report names no blocked
task, so each of today's hangs needed its own diagnostic image.

### 5.2a What Linux does (v7.0)

* `drivers/watchdog/watchdog_dev.c` takes `WDIOC_SETPRETIMEOUT`, and
  `watchdog_pretimeout.c` hands a pre-timeout to a governor.
  `pretimeout_panic.c`'s governor panics, which gives a report before
  the hardware resets; the other governor does nothing. Neither kills a
  culprit.
* The kernel's own detectors (hung task, soft lockup, above) report, and
  panic only when asked to.
* Killing a culprit is ring 3's job on Linux: systemd's per-service
  `WatchdogSec=` with `sd_notify("WATCHDOG=1")` restarts a wedged
  service, and systemd-oomd or earlyoom kill on memory pressure. Stage 2
  below has no Linux counterpart in the kernel. It is a typed mechanism
  that carries out a policy ring 3 registered ahead of time, because a
  stalled user space cannot run its own killer. Its constraints are the
  consultant's RD.4.

### 5.3 A staged response

| Stage | When | Who decides | What happens |
|---|---|---|---|
| 0. Supervision | always | init (ring 3) | Each service's own liveness: a service with `WatchdogSec=` notifies init, as systemd's `sd_notify("WATCHDOG=1")` does. A service that misses is killed and restarted under its `Restart=`. hyprix, the GPU driver and lode get one. A wedged service is then a restart, not a stalled init. |
| 1. Name the stuck | pre-timeout (half the timeout, Linux's `WDIOC_SETPRETIMEOUT`) | the kernel, a fixed report | The **hang dump** (R4): every task blocked for more than 5 s, with its state, its wait channel, the last lock line it took and its kernel stack, plus every processor's preemption-off holder (R3). It goes to the console, pstore and the event log. This is fault-relanding's dump, made a standing mechanism. |
| 2. Kill the culprit | pre-timeout + grace | **policy registered by ring 3 in advance**, carried out by the kernel | At arm time, init names a *sacrifice set*: jobs whose processes may be killed when user space has stopped (the game's session job, say), through a typed ioctl on `/dev/watchdog` that takes a job handle. At pre-timeout the kernel `SIGKILL`s that set. The policy (which jobs) is init's. The kernel has one typed list and one kill. If feeds resume, the record says "recovered by killing job N". |
| 3. Reset | timeout | the kernel | As today: FX-9008, then `ferrix.panic`'s reset. |

Stage 2 helps only when the stall is user space waiting on the culprit.
A kernel lock held by the culprit's own teardown (class C) will not let
the kill finish. That is why R4's dump comes first and R5 is the real
fix. Stage 0 is cheap and is init's alone.

### 5.4 Services must not die of a filesystem's failure

lode, auth and hyprix died of `SIGBUS` because their next text page was
on a root that answered `EIO`. R10: init, lode, hyprix and the GPU
driver lock their text once they have started (`mlockall(MCL_CURRENT)`).
A failing filesystem then cannot fault their code out. `ferrix-vma` keeps
an `mlock`ed state on a region, but whether it keeps file-backed pages
out of reclaim has not been checked, so R10 checks that first. lode
also needs a write path that does not depend on root: the ESP or pstore,
which `ferrix-eventlog` already reads.

## 6. Class E: a filesystem error that becomes a global one

### 6.1 Evidence

catch1 `hang3` and catch2 #1: btrfs aborted a transaction "(out of
memory)", the reload ran out too, and the mount made it final: "every
answer is EIO". Fixed by 197835013, 414134529 and f5cc0e856
(`docs/BTRFS-PRESSURE.md`).

### 6.1a What Linux does

`fs/btrfs/transaction.c:__btrfs_abort_transaction` and
`fs/btrfs/messages.c:__btrfs_handle_fs_error` set
`BTRFS_FS_STATE_ERROR` and make the superblock read-only
(`btrfs_set_sb_rdonly`). New writes then fail with `EROFS`, and reads
keep working. btrfs takes `memalloc_nofs_save` scopes, among them in
`transaction.c`'s commit path, so reclaim never re-enters the
filesystem. Writeback's bios come from mempools
(`mm/mempool.c`), so the path that frees memory does not need fresh
memory. Rules (1) to (3) below are those three.

### 6.2 The rules that close it

1. **An abort costs writes, never reads.** After an aborted transaction a
   mount goes read-only at its last commit, and reading committed data
   keeps working. L.btrfs.122-123 have landed this, and it is Linux's
   behaviour.
2. **"Out of memory" is never final.** A failure caused by memory is
   retried at the next use. Only damage on the disk is final. This has
   landed for the reload. R11 audits the other "unreadable" and
   "poisoned" states in `ferrix-btrfs-vfs`, `ferrix-vfs` and the block
   layer for the same mistake.
3. **Recovery needs memory reserved for it.** A reload that needs memory
   at the moment memory ran out is the trap. R11 gives the reload a
   reservation taken at mount (`mm/reserve.rs`'s reserved sections
   already exist for the item).
4. **One mount's failure stays its own.** A per-mount error state with a
   counter, readable in `/proc/self/mountinfo` and the event log. btrfsd's
   `restarts = 0` is the same rule for `fs=server` (existing BACKLOG row,
   ledger 6933).

## 7. Work items, in order

The points are story points. The first five are the ones to start.

| # | Item | Class | Points | Owner |
|---|---|---|---:|---|
| R1 | **The ring-0 allocation ratchet** (`check-ring0-alloc.py`, §2.5). Done by this landing. | A | 2 | robustness (landed) |
| R2 | **FX-0008 names its caller in the record.** The trace is image-relative in the event log and pstore (ledger 8043 MK.5's row). Without it every FX-0008 costs a code-reading hunt, as #7 did. A core panic-path design goes to the consultant first. | A | 3 | open (MK.5's row) |
| R3 | **The long-hold detector**, riding on S4a's tick, which lands first (§4.4 (2), §4.5). Debug and box images on, the reference configuration off. Also the spin-waiter measurement (§4.4 (3)). | C | 5 + S4a's rebase 3 | open |
| R4 | **The staged watchdog**: the pre-timeout hang dump (stage 1), then the sacrifice set (stage 2), §5.3. The dump first, in its own landing. | D | 3 + 5 | open, dump with fault-relanding |
| R5 | **Bounded teardown**: address space, VMO, descriptor table and `munmap` in steps of N with no lock held between them (§4.4 (1)). Its order comes from R3's list and fault-relanding's root cause. | C | 8 | step 1 r5-teardown (§4.6); step 2 open |
| R6 | **No contiguous runs for requests above a page**: served through `vmap` from order-0 frames, fallibly. Plus a sized, accounted reserve for allocations under a spin lock, in place of today's implicit headroom (§2.2). | A | 5 | open |
| R7 | **Burn down class A by area**, highest exposure first: the allocations under a lock (tmpfs's maps and `move_child`, then R13's list), `syscall/`, `fs/socket.rs`, `fs/pipe.rs`, `ferrix-vfs` (`namespace`, `tmpfs`, `dentry`), `interfaces/render` and `display` (the GPU ioctls), `ferrix-net`. A crate at 0 gets clippy `disallowed-methods` and leaves the ratchet. Step 1 is a reachability map from the entry points. About 1 point per 40 sites. | A | ~35 | per area owner |
| R8 | **OOM selection counts charged kernel memory and buffers**, plus init's typed `oom_protect` (§3.3 (3), (4)). | B | 5 | open |
| R9 | **Charge the GPU's and the display's buffers to the client's job**, and add a "charged or argued" column to the ratchet (§3.3 (1), (2)). | B | 5 | open, with the intelgpu owner |
| R10 | **Critical services survive a filesystem failure**: `mlockall` in init, lode, hyprix and the GPU driver; lode's root-independent write path; init's `WatchdogSec=` (stage 0) (§5.3, §5.4). | D, E | 5 | init, lode |
| R11 | **Filesystem failure states audited**: no "final" state caused by memory; a reservation for the reload; per-mount error counters (§6.2). | E | 3 | open, btrfs |
| R12 | **A fault-injection row**: `fallible::inject` armed from a debug-only interface on a workload's process tree (busybox, then a game-shaped load). It passes when every failure becomes `ENOMEM` or a kill and none becomes FX-0008. This finds what the token scan cannot see. | A | 5 | open |
| R13 | **Allocations under a live spin-lock guard, counted at run time** in debug and box images, with their callers, and a row that prints the top sites (§2.4). It feeds R7's order. | A | 3 | open |

## 8. What this document does not own

* **btrfs-pressure**: the specific FX-0008 sites (`openat`'s path clone
  and the 128 KiB one), and the kill that allocates
  (`park/heap-kill-reserve`).
* **fault-relanding**: the teardown hang, the hang dump and the process
  bisect.
* **input-click** and **game-fps**: their own topics.

## 9. Reviews

The certification consultant reviewed `docs/robustness` d0a572964 on
2026-11-08. The review is internal, not independent verification (F-27).
The entry is at ledger line 8202.

* **R1, the ring-0 allocation ratchet: OK IF R1.1-R1.7** (line 8203).
  R1.1 asks for a TOOLS.md row and adds `ring-0 allocation` to the step
  cache's item-keyed list. R1.2 makes `--record` lower only. R1.3 states
  the residual. R1.4 asks for two negative controls through `gate.sh
  control`. R1.5 asks for a `check` row by `gate.sh`, PASSED on the
  landing hash. R1.6 is the patterns-change rule in both docstrings. R1.7
  asks for one author, `ledger 8202` named, and this section.
* **This document: OK IF RD.0-RD.8** (line 8214). That approves the
  direction only. Each of R2, R3, R4 (stage 1 and stage 2 separately),
  R5, R6 and R8 changes core files and gets its own design review before
  code (RD.0). RD.1 to RD.7 give each one's conditions: R2's
  allocation-free bounded walk with image-relative addresses; R3's
  counter with no clock read, observing only; R4's reset not depending on
  the dump, and the sacrifice set's job-handle authority, which refuses
  the root job and the caller's own; R5's count-0-only steps and the
  restated safety argument for `Drop`; R6's opt-in for consumers that
  need physical contiguity; R8's score of only what a death releases.
  RD.8 says no review is owed for R7 in the load ring and in non-item
  crates, for R9, for R10, for R11 on `ferrix-btrfs-vfs`, or for R12 and
  R13 as xtask rows. Review is owed for R11 on the item's btrfs crates
  and for R13's counter in `mm.rs`.
* The consultant checked the document's claims about the code and found
  them true. It corrected four: the waiter change (A1, now §4.4 (3)), S4a's
  distance behind develop and its owed reviews (A2, §4.5), the place of
  MK.5 (A3), and the R2 to R13 range (A4).
