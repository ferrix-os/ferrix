# The OOM kill's emptying, handed to a task of its own (F-107, B)

Status: design (ledger 8665/8686 design opinion, 8738 Q5). Reviewed
2026-11-12, ledger 8784: NOT YET B1-B2, then OK IF C1-C11. Amended the same
day: section 1 per C9; section 2 for B1 (a dedicated emptier), B2 (patience
by progress, chunks, the mark on the space) and C1-C8. Session ef-30fps,
branch `f107/oom-reaper`.

## 1. Why: the stack rows, and what each remedy buys

`cargo xtask check-stack-depth` (since `f107/stack-walk`: one figure a kernel,
all three architectures judged) adds a system call's way in, the deepest of
what it calls, an interrupt's frame and a timer's interrupt with what a task's
last drop leaves. Every line wants 4,096 bytes of the task's stack left. The
figures below are the walk's on develop 61b566f46 plus `f107/stack-walk`
(dev kernels, the same under any `PYTHONHASHSEED`). Column (a) is the row's
own reading; columns (b) and (c) are edge cuts on that walk's graph, not
builds, and (c) is an estimate: an interrupt stack, or six pages, is the
customer's choice and gets its own design review (ledger 8784 C9).

* (a) as built;
* (b) B: `oom::kill_in`'s calls of `ended` and `AddressSpace::unmap` gone
  (the emptying is the reaper's, section 2);
* (c) B and a per-processor interrupt stack: the timer's walk, the last drop's
  rest and the part of the interrupt's frame saved after the switch go to the
  interrupt stack. What stays on the task's stack is what the entry saves
  before it can switch, taken as Linux does it (registers saved on the task's
  stack, then `call_on_irq_stack`): x86-64 the 176-byte `TrapFrame` (the
  processor's own 40 bytes, SS to RIP, are in it; an IST gate would leave
  nothing, but nests badly); AArch64 the 304-byte `TrapFrame` (the exception
  itself stores nothing; the other 592 of the 896 are the vector state and
  dispatch's frame); ARMv7-A the 80-byte `TrapFrame` (`srsdb`'s 8 bytes among
  them; IRQ mode's banked stack pointer could take even that).

Bytes, and in brackets the pages that line needs with 4,096 left. The stack
is x86-64 4 pages, AArch64 6, ARMv7-A 4.

| Line | (a) as built | (b) B | (c) B + interrupt stack |
|---|---|---|---|
| x86-64, a process made | 21,720 (7) | 17,168 (6) | 13,360 (5) |
| x86-64, clone to `Process::new` | 15,704 (5) | 11,152 (4) | 7,344 (3) |
| x86-64, clone by every direct call | 24,824 (8) | 20,272 (6) | 16,464 (6) |
| AArch64, a process made | 23,424 (7) | 18,656 (6) | 14,256 (5) |
| AArch64, clone to `Process::new` | 16,992 (6) | 12,224 (4) | 7,824 (3) |
| AArch64, clone by every direct call | 26,320 (8) | 21,648 (7) | 17,248 (6) |
| ARMv7-A, a process made | 18,040 (6) | 13,520 (5) | 10,400 (4) |
| ARMv7-A, clone to `Process::new` | 12,584 (5) | 8,064 (3) | 4,944 (3) |
| ARMv7-A, clone by every direct call | 19,672 (6) | 15,552 (5) | 12,432 (5) |
| x86-64, the OOM emptier | -- | 12,952 (5) | 9,144 (4) |
| AArch64, the OOM emptier | -- | 13,488 (5) | 9,088 (4) |
| ARMv7-A, the OOM emptier | -- | 10,744 (4) | 7,624 (3) |

The emptier's rows are the landing tree's (`f107b4-stack.log`; 8881 G4), and B's own (ledger 8856 A4): a kernel task's stack under
its chunk's unmap and the allocation's purge below it, with a timer's
interrupt on top in (b) and the entry's frame alone in (c). Before B the same
unmap ran deeper, on the stack of the task that asked.

The timer's term in (a) and (b) is 3,808, 3,808 and 3,120 (with the last
drop's 752, 800 and 704). An interrupt stack needs its own row: the timer's
walk and the rest of the frame on it, 3,808, 4,400 and 3,120 bytes: with
4,096 left, two pages on x86-64 and ARMv7-A and three on AArch64 (8,496
bytes), and its own guard.

So no remedy alone holds every line. B holds AArch64's process made on its
six pages and all three clone-to-`Process::new` lines on four; B with an interrupt stack holds
everything on AArch64's six pages and ARMv7-A's process made, and leaves
on four pages x86-64's process made 1,072 bytes over, its clone row 4,176
over and ARMv7-A's clone row 144 over.

The three largest frames on each line still failing after B, and what the
line reads with that frame at nothing (the deepest other way then counts):

* x86-64, a process made (17,168): `Process::shared_with_context` 2,664
  (to 14,504), `Process::with_context` 2,568 (14,600),
  `syscall::native::answer` 1,856 (15,312).
* x86-64, clone by every direct call (20,272): `ferrix_net::Stack::poll_transmit`
  2,872 (18,520), `family::sys_clone` 2,352 (17,920), `net::namespace::create`
  1,128 (19,144).
* AArch64, clone by every direct call (21,648): `Stack::poll_transmit` 2,864
  (18,784), `sys_clone` 2,000 (19,648), `namespace::create` 1,136 (20,512).
* ARMv7-A, a process made (13,520): `Process::with_context` 2,368 (11,152),
  `shared_with_context` 1,744 (11,776), `native::answer` 1,536 (11,984).
* ARMv7-A, clone by every direct call (15,552): `Stack::poll_transmit` 2,712
  (13,240), `sys_clone` 1,096 (14,456), `Stack::open_from_request` 936 (14,616).

Trimming `poll_transmit` alone answers AArch64's clone row after B; on four
pages no single trim does. The choice between an interrupt stack and more
pages on x86-64 and ARMv7-A is the customer's (ledger 8738 Q5); B is wanted
whichever is chosen, since it takes about 4.6 KiB from under every
kernel-heap allocation on every architecture.

### 1a. The decision (2026-11-12) and what it came to

The customer played on the box, called it playable and said "just fix the
remaining issues", which left the stack row's choice with ef-30fps's
session; the product owner's message of 2026-11-12 relays it: "the row goes
green by trimming and stack pages now; the per-processor interrupt stack is
a later improvement, not started", "trim first ... then stack pages, only
as far as the trimmed figures need with 4,096 bytes left on every judged
line", "AArch64 should stay at 6". The consultant's design line is ledger
8953 (the earlier 8665 and 8738 had refused nine pages and left the count
to the customer).

Trimmed, each a large local taken off the deep path, not a cosmetic one:
`ferrix_net`'s segment payload on the heap (`poll_transmit` 2,872 to 840
bytes on x86-64); a fork's last step (`start_child`), its child's thread
(`child_thread`) and the parent's captured registers (`captured_state`)
out of the fork's frame (`sys_clone` 1,368 to 232); the user state boxed
from its capture to its task (`sched::BoxedUserState`), never a by-value
copy in every frame between; a network namespace built out of
`namespace::create`'s frame (`made`); a process's core made before the
`Process` is (`core_of`), and `with_context` inlined so that one copy of a
`Process`, not two, stands under its move to the heap; and x86-64's 64-bit
signal frame built out of `setup_signal_frame`'s frame (`setup_native`),
since the row gained a line for the way out of a system call (a signal's
frame, a thread's end, 15,304 / 16,640 / 12,904). The deepest
sub-trees, develop to trimmed (x86-64 / AArch64 / ARMv7-A): a process made
9,808 / 9,632 / 7,392 to 7,856 / 7,680 / 5,720; clone by every direct call
15,384 / 15,136 / 11,432 to 10,216 / 10,288 / 8,240.

Pages, the fewest the trimmed lines need: x86-64 five (at four, a process
made, a fork and the emptier were over), AArch64 six, ARMv7-A five (at four,
a fork was 56 bytes over; its deepest tails all end in `user/vmo.rs`'s
`decommit_in_chunks` and `retire_runs`, 2,768 bytes together, which the
resident-counts work owns: trimmed, ARMv7-A could go back to four, on a
ledger line of its own). The per-processor interrupt stack (column (c)) is
a BACKLOG row, a later improvement, not started.

**One release body on a kernel stack (2026-11-13, F-113; ledger 9414, 9427).** A process's release sends signals (its parent's exit signal, a namespace's init's `SIGKILL` to every member), and a fatal one ends its target, which is released at once when no thread of it is live: a target's own exit racing its killer's look at the live count let the killer run that release inside its own, at every link of a chain, with no bound. Now a release reached while another runs on the same task is owed to the outermost release there and run after it, before it returns (`Process::release`): a Linux thread keeps the list on its `Thread`, a kernel task in one of 16 slots found by its address. The residual: a 17th kernel task inside a release at once, or a process no longer listed, releases at once as before, counted in every build and said once ("release: no slot for a kernel task's owed list; released at once"). `check-stack-depth`'s listing names the entry bounded; `test-release-chain` holds nesting to one.

## 2. B: the emptying is an emptier task's

### 2.1 What happens now

A process ended by an OOM kill keeps its address space until its parent's
`wait4`. So `oom::survey`, at each look over the live processes, empties the
space of one that has ended and closed everything and whose space no living
process shares (`oom::ended`: `AddressSpace::unmap` over the whole user
range), as Linux's OOM reaper does, and answers retry at once
(`L.object.103`). It does so where it stands: in a fault's task for the
scoped and machine-wide kills, and for the kernel's heap below the heap
allocation that asked (`KernelAllocator::alloc`, `mm::heap_or_kill`,
`mm::while_killing`, `oom::for_kernel_heap`, `kill_in`, `survey`, `ended`,
`unmap`, `give_back`, `Vmo::decommit_range`, `retire_runs`, ...: 4,552 bytes
on x86-64, 4,768 on AArch64, 4,520 on ARMv7-A), under every allocation's
stack, a system call's included.

### 2.2 Linux, read first (`mm/oom_kill.c`, v7.0)

* `__oom_kill_process` marks the victim (`mark_oom_victim`) and
  `queue_oom_reaper` arms a timer of `OOM_REAPER_DELAY` (2 s) on the task, at
  most once an mm (`MMF_OOM_REAP_QUEUED`); nothing is allocated, the list
  link is in `task_struct` (`oom_reaper_list`).
* `wake_oom_reaper`, the timer, links the task on `oom_reaper_list` under
  `oom_reaper_lock` (a leaf, irq-safe) and wakes `oom_reaper_wait`, unless
  the victim has ended by itself (`MMF_OOM_SKIP`, set by `exit_mmap`).
* `oom_reaper`, a kernel thread, takes one task at a time and
  `oom_reap_task` tries `oom_reap_task_mm` up to `MAX_OOM_REAP_RETRIES` (10)
  times, 100 ms apart; each try takes `mmap_read_trylock` (it never waits
  for the lock) and `__oom_reap_task_mm` unmaps the anonymous and private
  ranges from the top (`unmap_page_range` under a non-blocking MMU
  notifier), skipping hugetlb, PFN maps and shared file mappings.
* Reaped or not, the mm then gets `MMF_OOM_SKIP`: "Hide this mm from OOM
  killer because it has been either reaped or somebody can't call
  mmap_write_unlock". `oom_evaluate_task` waits (`abort`) while a victim
  without `MMF_OOM_SKIP` exists, and passes over one with it, so the kill
  moves on to the next victim after at most about a second of tries.

### 2.3 The design (amended for ledger 8784 B1, B2, C1-C9)

1. **An emptier of its own** (B1). A kernel task, `oom-emptier`, started at
   boot beside the reaper (`sched::start_reaper`'s place), as Linux's
   `oom_reaper` is a kthread of its own. It is marked inside a reclaim for its
   whole life (`Task::swap_reclaiming(true)` at its start, never put back), so
   that no allocation it makes is answered by a reclaim or a kill
   (`mm::heap_may_reclaim` says no): it can never wait on a kill, its own
   hand-off included. Its refused allocations are served from its
   processor's allocation reserve, as the heap's killer's are
   (`oom::killing_for_the_heap` answers yes for it too; L.mm.139). The
   leftovers reaper is not touched: its pass order stays a wanted purge, then
   the leftovers.
2. **The hand-off, in `oom::ended`** (C1). Where it now calls `unmap`, it
   stores a `Weak` of the space in one slot, `EMPTYING` (a leaf
   `SpinLock<Option<Weak<AddressSpace>>>` in `object/oom.rs`), stamps the
   progress clock (`EMPTY_PROGRESS`, an `AtomicU64` of `timer::now_nanos`)
   when the slot takes a new space, raises an atomic `EMPTY_ASKED`, wakes the
   emptier's wait queue (no lock held: `kill_in` runs under the `CHOOSING`
   flag, never a spin lock), and answers a new `Ended::Handed`; `survey`
   answers `(Retry, pause)` for it. A `Weak` and three stores: **nothing is
   allocated**. The slot keeps the `Weak` until the space is gone, emptied
   (none of it resident) or given up; the emptier does not take it out.
   While it holds another such space, a second ended space answers `Ending`
   (wait). The emptier's wait reads `EMPTY_ASKED`, an atomic, never the
   slot's lock; the slot's lock is never taken in interrupt context.
3. **The emptying, in chunks, from the top** (B2, C3, C7). The emptier
   upgrades the `Weak` after the slot's lock and empties the space a chunk at
   a time: under `AddressSpace::with_regions` (the space's own lock, no
   allocation) it reads the highest region, then unmaps that region's top
   `EMPTY_CHUNK_PAGES` (128 pages, 512 KiB), so a chunk never splits a
   region. It stamps `EMPTY_PROGRESS` only when a chunk lowered the highest
   region's end (the region shrank or went); a chunk whose unmap answered an
   error, or that left the map as it was, stamps nothing and ends this
   space's emptying, which the patience then gives up (8814 D1). It goes on
   until no region is left, then lets the slot go and tells the heap's hold
   it is done. A space given up, or no longer the slot's, is left at the next
   chunk boundary, and the emptier takes the slot's next space, whose
   patience counts from its own hand-off (D2). The re-check after the
   upgrade (C2) is argued, not made: `ended` handed the space only once its
   process had closed everything and no living process shared it, and a
   space gains a sharer only by a clone or `vfork` of a thread running in
   it, of which none is left. The native ways to a space (D3) -- a
   launcher's mapping into a child it made (`launch::load_native`), any
   handle that reaches a space -- can still map into it after the hand-off;
   that is harmless: the emptier empties whatever regions are there when it
   looks, nothing runs in the space, and a mapping made after its last
   chunk is given back at the space's drop like any other. A reader of it
   (`process_vm_readv`, `/proc`) maps nothing and finds it emptied. If the
   upgraded `Arc` is the last, the space is dropped in the emptier, a task
   that may sleep. Nothing on the kill's own way upgrades a `Weak`: the slot
   holds `Weak`s alone, and a space given up while another took the slot is
   kept as a `Weak` and marked at its own next look, which holds it.
   *What one chunk allocates*, all through `AddressSpace::unmap`: the
   removed region's piece (`inner.map.remove`, one `Unmapping`; a top trim
   splits nothing), `Vec<Freeing>` of one entry, the touched ids (one),
   `give_back`'s `retiring` (one), and `Vmo::take_range`'s list of the
   chunk's committed frames, 16 bytes a page, at most 2,048 bytes for 128
   pages: every one within the heap's largest size class (2,048), so each
   can come from the reserve's 16 blocks of its class. A refusal the reserve
   cannot serve fails safe: `give_back` keeps the pages with their object
   (`RANGES_KEPT`) or decommits them, as an unmap does today, and the next
   chunk goes on. Two named tasks (the killer and the emptier) drawing on
   one processor's reserve: a draw masks that processor's interrupts, so
   draws never interleave; one may find a class empty that the other drew,
   and then its allocation is refused as it would be with no reserve, never
   wrong (the reserve's soundness rests on whole-class blocks, not depth:
   `mm/reserve.rs`).
   *The space's lock*: a chunk's take-down holds the space's spin lock for
   at most 128 pages' leaves, where one whole-range unmap held it for the
   whole space (C7).
4. **The wait, bounded, and the way on when the emptying does not come**
   (B2). The asker's wait is the bound it has now: at most
   `KERNEL_HEAP_ANSWERS` (1024) answers for the heap, `MOST_ANSWERS` (1024)
   for a fault, each a look and a 1 ms sleep, about a second, an upper bound;
   then the refusal stands as before (no hang, no reset). A hand-off is
   **given up** when `EMPTY_PATIENCE_NANOS`, 500 ms, pass with no progress:
   none since the hand-off (the emptier never began) or since its last
   chunk. An emptying giving pages back is never given up, however long the
   space. Given up is a mark on the space itself (an `AtomicBool` on
   `AddressSpace`, set by `ended` and read by `ended` and
   `kernel_victim_held`: the only change to `user/space.rs`), said once on
   the console ("oom pid N: its space was not emptied, no progress in
   500 ms; the kill moves on"); `ended` passes such a space over (`Gone`),
   the slot lets it go, and it holds back no kill for the heap. So the next
   look chooses the next victim, within the grace and L.mm.138's other
   rules, as Linux's `MMF_OOM_SKIP` lets `oom_evaluate_task` pass a victim.
5. **The hold for the heap** (C4, MK.2 in part). `kernel_victim_held`
   answers held while the last victim's space is held by anyone and the kill
   still waits for it: `KERNEL_VICTIM` keeps a done flag beside its `Weak`,
   set when the emptier empties that space or its emptying is given up (a
   strong count and a flag read; nothing upgraded, so no last reference is
   dropped under the heap's allocation). Before, it waited for the last strong
   reference, the zombie's, which only the parent's `wait4` lets go. A
   victim that never closes is never handed, so for it the hold stays as
   unbounded as before: BACKLOG 324 (MK.2) is narrowed to that half.
6. **Lock order.** `EMPTYING`'s lock is a leaf: taken with no lock held (in
   `ended` under the `CHOOSING` flag, in the emptier, in
   `kernel_victim_held` after `KERNEL_VICTIM`'s lock is let go), never in
   interrupt context; nothing is taken under it, and the old `Weak` and the
   upgraded `Arc` are dropped after it. The emptier takes the space's locks
   as an unmap does, holds none across a chunk boundary, and the emptier's
   wait queue lock is taken by the hand-off with no other lock held.
7. **All three scopes** hand off on one path. One slot for the machine: a
   job's fault may wait behind another scope's emptying (M&T says so).

### 2.4 Requirements (ids to reserve: L.object.374-376)

* **L.object.103, restated:** an ended victim, closed, unshared, holding
  pages, shall be handed by the next look to the emptier, one space at a
  time, and emptied by it before its parent's wait; a look that finds it
  handed and not emptied shall wait and retry, killing no second process,
  until it is emptied or given up.
* **L.object.374 (new):** no OOM kill shall empty a space where it was
  asked: the look hands it over, allocating nothing. Proved by one static
  rule (2.5, 1). "Empties nothing where it stands" moves here from L.mm.138.
* **L.object.375 (new):** a hand-off shall be given up once
  `EMPTY_PATIENCE_NANOS` pass with no chunk emptied, marked on the space,
  said once, and then hold back no later kill; an emptying that makes
  progress is never given up.
* **L.object.376 (new):** the emptier is a task of its own, inside a reclaim
  for its life, and empties a handed space from the top in chunks of
  `EMPTY_CHUNK_PAGES`, none splitting a region.
* **L.mm.138, restated in its hold clause:** "none while the space of the
  last process killed for the heap is held and still holds pages, unless
  given up". KF.1-KF.3 (F-109) restate when the kill is made; this changes
  neither its preconditions nor its bound, and lowers the clone path. F-109's
  design rebases over this text (C8).
* **L.mm.139, restated:** the reserve also serves the emptier's refused
  allocations; its case names the emptier beside the killer.
* L.object.102 and H.QUOTA.22 unchanged in what they say; units gain
  `object::oom::emptier` and `object::oom::empty_one`.

### 2.5 Checks and controls (C6; every control through `gate.sh --expect`)

1. **Static, check-stack-depth (host):** a named rule beside F-104's, no way
   by the walk's calls from `object::oom::kill_in` to
   `AddressSpace::unmap`, and none from the hand-off (`object::oom::hand_over`,
   its wake included) to the kernel allocator (8814 D4), on any
   architecture, one named rule with self-test cases; the walk leaves out
   failure paths, a switch, and a task's last reference let go in a wake
   (the emptier never ends), each named.
   *Control:* `ended`'s `unmap` put back (`--expect-host` the rule's line).
2. **Boot, the emptier empties** (L.object.103's case and the machine-wide
   kill's case in `object/quota_check.rs`): after the victim lets go, the look
   answers retry, and the case waits on the job's use falling by the
   victim's pages (1 ms sleeps, a deadline of twice the patience that fails
   by name), before any parent's wait. *Control:* the emptier returns at
   once: "an ended victim's memory was not given back by the emptier".
3. **Boot, given up** (L.object.375): a check-only step gate on the emptier
   (`oom::step_emptier_for_check`: `CHECKS-ONLY`, written by the check
   alone, read by the emptier in one place before each chunk, cleared on
   every exit of the check, error paths included, and asserted clear after).
   Held shut: the look waits, then after the patience passes the victim over
   and kills the next process (two kills). *Control:* the patience test
   removed: "a victim whose emptying did not come still held the kill past
   its patience".
4. **Boot, progress is not given up:** a victim of several chunks, the gate
   opened one chunk at a time, each gap shorter than the patience, the gaps
   together longer: the look waits throughout, one kill. *Control:* patience
   counted from the hand-off on the wall clock: "an emptying giving pages
   back was given up".
5. **Boot, the emptier's mark:** the emptier records, at each chunk, whether
   it runs inside a reclaim; the case reads it. *Control:* the mark not set
   at the emptier's start: "the emptier ran outside a reclaim".
6. **check-stack-depth** gains a row for the emptier (its chunk under a
   timer's interrupt), and records the walk's figures, FAILED (section 1's
   column (b)).
7. L.mm.139's case names the emptier as well as the killer (served), with
   its existing controls.

### 2.6 What it costs

* Ring 0: about 70 lines in `object/oom.rs` (the slot, the hand-off, the
  emptier and its chunk, the patience, the check's gate), 2 in
  `user/space.rs` (the mark), 3 in `sched/mod.rs` (the start), less `ended`'s
  old lines; the rise argued with the budget file's figures when measured.
* Latency: the emptying starts after the emptier's wake and a fair task's
  turn, where it ran in the asker; the asker sleeps 1 ms between looks.
  A whole-range unmap becomes as many chunks as the space has 512 KiB
  pieces, each with its own (usually empty) shootdown: a 6 GB victim is
  about 12,000 chunks.
* No change to what is killed or when (F-109's design), to stack pages, or
  to the interrupt path.

### 2.6a What B left as it was (ledger 8856 A6, A7)

* **A last reference dropped on the asker's stack.** `kill_in`'s callers take
  `process::live()`'s list and drop it after the look, on the stack of the
  task that asked (an allocation's, for the kernel's heap). A `wait4` racing
  the look can leave that list holding a process's last reference, and the
  process's drop then tears its space down there: `AddressSpace::drop`, not
  `unmap`, which L.object.374's walk does not take as its target. Pre-existing,
  not B's; so L.object.374 says "unmap", what its walk proves, and the drop
  is a BACKLOG row under F-107 (owner ef-30fps): hand the list's last
  references to the emptier, or drop them where the asker may sleep.
* **A displaced space, marked late.** A stalled space given up while another
  takes the slot is kept as a `Weak` and marked at its own next look, or at
  the emptier's next wake, whichever is first; until it is marked or gone
  no second space takes the slot (that look waits, within the asker's
  bound). Not run by any case (the displacement needs two victims stalled in
  turn): argued, not run; its coverage argument is filed once the item is
  next measured (the residual takes arguments only for measured lines).

### 2.7 Not in this design

R5 step 2a (`r5-teardown`, L.user.230-234 and L.object.373 reserved) moves
other teardown work to the reaper; a dedicated emptier keeps out of its way
(C8). The resident-counts work in `user/space.rs` and `user/vmo.rs`:
this design adds one atomic and its accessors to `AddressSpace` and nothing
else; whichever lands second rebases.
