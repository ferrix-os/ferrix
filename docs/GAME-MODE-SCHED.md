# Game mode's layer B: what the scheduler is told, and keeps

**Design notes, 2026-10-30, branch `docs/game-mode-sched`. Nothing in them
is built.** They are the design reviews that `docs/GAME-MODE.md` §13 owes
before any code of layer B (the consultant's ledger line 5115, condition
(12)): §5.2 to §5.5 there, the levers F3, F6 and F7, and conditions (6) to
(9). The customer chose tiers T2 and T3 on 2026-10-30
(`docs/backlog/DECISIONS.md`) and not T1, the session service that §8 of
GAME-MODE.md gave the frame mark's right to; §3.1 here names its holder
instead.

Five pieces, in the order they are built, each a review of its own:

| § | Piece | Levers | Ids |
|---|---|---|---|
| 2 | Applied affinity and per-task slice requests | F3, F7 (F6 stays the lag stream's) | L.sched.96-101 (taken over), L.sched.107-109, L.sched.123 |
| 3 | The frame mark: its right, its holder, its lifetime, its order | §5.2 points 1 and 2 | H.SCHED.19, L.sched.110-114 |
| 4 | Lends: through a served request, through the wake history, NT sync | §5.3 | L.sched.115-118 |
| 5 | The idle-gap class | §5.5 | L.sched.119-120 |
| 6 | Deadlines from the frame | §5.2 point 3 | L.sched.121-122 |

§7 says how each is measured on the box, §8 holds the questions for the
review, and §9 where each stands.

## 1. What every piece keeps

* **Rules, not figures.** Every requirement states a rule a boot check can
  prove: which task is picked, where it is placed, when a mark or a lend
  ends. None claims a latency, a slice met or a deadline met
  (`docs/certification/MEMORY-AND-TIMING.md` §2.1; ledger 5115 (1)). The
  figures in §7 are product evidence for the customer, not item evidence.
* **Inside eligibility.** Nothing here takes a task out of the fair class
  or makes it ineligible. The virtual time, the weights and the lags are
  EEVDF's as now (`ferrix-sched`). A mark changes *which eligible entity*
  is picked, and an unmarked task's wait is bounded by the kernel, in
  decisions, not by the holder (ledger 5115 (6)).
* **Generic.** The kernel learns no game, no window and no program name.
  It learns that a task is marked, by whom, and what slice it asks for
  (`keep-kernel-slim`, 2026-10-22). Which tasks those are is decided in
  ring 3, by the holder of §3.1.
* **Checked.** Each rule has a boot check in `src/kernel/src/sched/` or a
  test-threads step, and a negative control through
  `gate.sh control --expect` that shows the check fires with the rule
  removed. Arithmetic in `ferrix-sched` has host tests.
* **The core ring.** `sched/**` and `ferrix-sched` are the item's core
  ring (`tools/common/data/certification-item.json`); the system-call
  handlers in `syscall/limits.rs` and the chardev core are the load ring.
  Each piece comes back for its own code review with its ids, rows and
  controls.

## 2. Applied affinity (F3) and per-task slice requests (F7)

### 2.1 F3: the design that exists

`sched_setaffinity` was designed and reviewed on 2026-10-25 and never
built: backlog p1-036, the consultant's **OK IF (D1)-(D12)**, ledger line
4446, with L.sched.96-101 reserved by chardev-sigrace for a branch
`feature/sched-setaffinity` that does not exist on any remote. This design
takes it over unchanged (D1 to D12 are its rules: storage under the home
queue's lock, the lock order in a MEMORY-AND-TIMING section, the mask
intersected with online and the domain, kernel threads refused, every
placement path obeying the new mask, the caller moved before its next user
instruction, the setter waiting for a running target, inheritance across
clone, fork, vfork and exec, Linux's permission, its six ids and its
checks) and changes only the reservation's owner and branch.

What has changed since 2026-10-25, and how F3 uses it:

* **Core quarantine landed** (`docs/CORE-QUARANTINE.md`, L.sched.102-106).
  It added `sched::placeable` to every placer and a re-home of a blocked
  task at its wake (`rehome_blocked`, L.sched.103) and of every queued
  task at a park (L.sched.104). (D5)'s "every placement path re-homes an
  excluded task before it is queued" reaches the same sites, but **beside
  `placeable`, not inside it** (ledger 5156 (A4)): `placeable(cpu)` stays
  the in-service test with L.sched.102's own control, and the mask is a
  second conjunct, `allowed(task, cpu)` over the effective mask below,
  asked at `placeable`'s call sites and at `wake_at_home`, the Sync wake,
  `Onto::Pull`, `rehome_blocked` and the park's evacuation (L.sched.103,
  104). So each control removes one test. L.sched.97's units name every
  site.
* **The mask at set time** is the given mask ∩ the in-service set ∩ the
  domain, `EINVAL` when empty (Linux's `cpu_active_mask`; (A1)).
* **Quarantine's ruling (g)** said that an applied `sched_setaffinity`
  would make a user's pin an unprivileged veto on parking a processor.
  So F3 adds one rule, **L.sched.123: a mask never vetoes a park** (A2).
  It is one function, the task's **effective mask**: the stored mask ∩
  in service, or the domain's in-service set while that is empty. Every
  placer, the pick push (98), the move before user code (99: no user
  instruction runs outside the effective mask), the setter's wait (100)
  and `sched_getaffinity` use it. The stored mask is kept, so that it
  applies again once one of its processors returns (Linux's
  `select_fallback_rq` widens the mask for good and logs it; keeping it is
  the narrower change). At a return to service nothing walks: a task
  outside its effective mask moves at its next placement, pick or return
  to user. In the same landing CORE-QUARANTINE.md §3.2's refusal of "a
  processor running a task pinned to it alone" becomes kernel threads
  only, whose spawn-time pins keep their veto (D4), and L.sched.105's
  statement and criterion say so (A3).
* **What F3 does not do for game mode.** A mask keeps a task *on* its
  processors; it does not keep anything else *off* them. GAME-MODE's
  "three cores for the game, one for the system" needs every other task
  confined as well. Without cpuset cgroups (out of scope, (D12)), the
  holder of §3.1 sets the masks of the system's Linux processes itself at
  a session's start, writes each mask it changes with the value before to
  its record under `/run` before the write, and restores from the record
  at its end and at its own start (as ledger 5115 (7) asks of freezing).
  Threads and processes made during the session by a confined process
  inherit its confinement mask (D8) and are not in the record; at the
  session's end, and at its own restart, the holder restores every Linux
  process whose mask is exactly the confinement mask it set, record or
  not, to all processors (A11). Native tasks and
  kernel threads are not confined: the GPU driver is on the path, and the
  per-processor kernel tasks stay where they are (GAME-MODE.md §5.6).
  A cpuset controller is backlog row dg-015.

### 2.2 F7: a slice a task asks for

Today one slice serves every entity of a queue: `slice_for` gives
`TARGET_LATENCY_NS` (3 ms) divided by the number queued, never under
`MIN_SLICE_NS` (`sched/queue.rs`), and `RunQueue::config.slice_ns` is
what every new request is measured with (`enqueue`, `update_curr`'s
renewal, `yield_curr`, `reweigh`, `level`, `hand_over`). Linux 6.12 let a
fair task ask for its own (`sched_attr::sched_runtime`, 0.1 to 100 ms):
a shorter request gives an earlier virtual deadline, so the task is picked
sooner among the eligible and preempts on waking more often, and gets the
same share as before, because its weight is unchanged.

**The rules.**

* **R7.1 (L.sched.107).** Each entity carries a requested slice `r`,
  zero for none. At each new request -- its enqueue, the renewal when its
  request is spent, a yield, a change of weight, a levelling and the
  direct switch's arrival -- its virtual deadline is its virtual runtime
  plus its **effective request** scaled by its weight, where the
  effective request is the queue's slice when `r` is zero and
  `min(r, the queue's slice)` otherwise. `r` is never below
  `MIN_REQUEST_NS`, Linux's 0.1 ms floor, which the setter enforces.
  It is one function, `effective_request(r, slice)` in `ferrix-sched`,
  used at those six sites; `vslice`, `lag_limit`, `placement_lag` and
  `decision_in_ns`'s cap keep the queue's slice, and a host test holds
  `lag_limit` and `placement_lag` bit-identical for every `r` (A6). `r`
  lives in the task and in `EntityState` (carried across a dequeue and an
  enqueue, never in `Config`), written under the home queue's lock by
  (D1)'s rule; a changed `r` takes effect at the entity's next new
  request (A5).
* **R7.2 (L.sched.108).** **No request is longer than the queue's
  slice**, so the lag limit (two of the queue's slices, `lag_limit`), the
  decision bound (`decision_in_ns`) and H.SCHED.2's bound are unchanged.
  This is narrower than Linux, which grants up to 100 ms and widens the
  lag limit with it; a longer request would change a claim the item
  makes, and nothing in game mode wants one. A task can make its
  processor decide at most once per `MIN_REQUEST_NS` (a constant,
  100 µs) by its request: a bound on decisions, stated in
  MEMORY-AND-TIMING, no figure claimed (A7).
* **R7.3 (L.sched.109).** A task's request is set only by
  `sched_setattr` on that task, under Linux's permission (D9's), or by the
  frame mark's holder through the mark (§3.3, M6). **A clone, fork or
  vfork child inherits the task's own request**, and `execve` keeps it.
  That is Linux 7.0's rule, read in the source on 2026-10-30 (A8):
  `dup_task_struct` copies `se.slice` and `se.custom_slice`, and
  `sched_fork` resets them only under `SCHED_RESET_ON_FORK`
  (`kernel/sched/core.c` 4647-4658), which Ferrix refuses. A request
  that a mark set (M6) is not the task's own and is not inherited, as the
  mark is not (M4).

**The system calls** (load ring, `syscall/limits.rs`; no L id, as ledger
3538 and (D10) put the syscall surface):

* `sched_setattr(pid, attr, 0)` with `SCHED_OTHER` or `SCHED_BATCH`:
  `sched_nice` applied as `setpriority` applies it today, and
  `sched_runtime` zero for none, otherwise clamped to 0.1 to 100 ms and
  stored as `r`, as Linux 7.0's `__setparam_fair` clamps it
  (`kernel/sched/fair.c` 5148-5161; no `EINVAL` for the range). Every other
  policy, `SCHED_FLAG_RESET_ON_FORK`, the utilisation clamps and any
  unknown flag: `EINVAL`, as `sched_setscheduler` refuses all but
  `SCHED_OTHER` today ("a promise the scheduler has no class to keep").
  `attr.size` handled as Linux's `sched_copy_attr` (`E2BIG` with the size
  written back for a larger unknown tail that is not zero).
* `sched_getattr` answers the policy, the nice value and `sched_runtime`
  as the stored `r`, or the queue's current slice when `r` is zero (Linux
  answers `se.slice`, its base slice when none was asked; so the parity
  lines print no slice value, C1).

**What F7 does not do alone.** A task woken with negative lag is not
eligible, and waits for the next decision whatever its request
(`docs/SCHED-WAKE-AFFINE.md` §8: the 214 µs-quantised tail). Linux avoids
that with DELAY_ZERO, which is F6, owned by the EEVDF lag stream (p2-149,
ef-sys, ledger 2742) and reviewed there. F7's request helps the woken
task from the decision it becomes eligible at; F6 is what makes that the
wake. The design does not claim F6's gain for F7, and §7 measures F7 with
and without F6 if F6 lands first.

### 2.3 Checks and controls

* **Host tests (`ferrix-sched`):** for each of the six new-request sites,
  the deadline's offset from the virtual runtime is the effective request
  scaled by the weight, `r` zero or not, `r` longer or shorter than the
  queue's slice; `hand_over`'s bit-for-bit test extended with a request;
  the lag limit unchanged by any `r`; two eligible entities, one asking
  for `MIN_REQUEST_NS`, the other none: the short one is picked first.
* **Boot check (`sched/slice_check.rs`, L.sched.107-108):** two spinners
  pinned to one processor, one asking for the minimum, one for nothing,
  and a third asking for twice the queue's slice. At each switch-in read
  through `sched::inspect`, each one's deadline less its virtual runtime
  is its effective request at its weight. Three attempts.
* **Controls:** the renewal measured with the queue's slice
  (`update_curr` ignoring `r`): "a renewed request ignored the task's
  slice"; the cap removed: "a request longer than the queue's slice was
  granted". Both through `gate.sh control --expect`, FIRED.
* **test-threads steps (L.sched.109 and the load surface):**
  `sched_setattr` and `sched_getattr` round trip; 99 µs read back as
  100 µs and 101 ms as 100 ms; `EINVAL` for `SCHED_FIFO` and
  `RESET_ON_FORK`; a fork child reads its parent's request; an exec keeps
  it; another user's task `EPERM`. Lines compared with Linux 7.0 on the
  same processor count (D12's practice). Control: the child given no
  request.

F3's checks and controls are (D11)'s, as (A9) assigns them to ids: 96
the kernel-thread refusal (a canary in `check.rs`; control: the refusal
removed); 97 every placement obeys the effective mask (a mask changed
while asleep at home, then woken, runs inside; control: `wake_at_home`'s
mask test skipped); 98 the pick push (a mask changed while queued;
control: the push skipped); 99 before user code (test-threads `getcpu`,
and RDTSCP on x86-64, right after the call, on three architectures at
`--smp 2` and 4; control: the work bit not posted); 100 the setter's wait
(the target spinning on another processor; control: the wait removed);
101 inheritance (the child's mask, and after exec). And **L.sched.123**,
the fallback: a task masked to the processor being parked runs elsewhere
while it is out, `sched_getaffinity` reads stored ∩ in service during the
park and the stored mask after, and the task is back inside its mask at
its next placement (control: the fallback removed, FIRED). Every control
through `gate.sh control --expect`.

The code review gets (A12): `check`, `build --arch all`, test-boot x86_64
kvm, tcg and `--smp 2`, aarch64, armv7a and armv7a `--smp 2`, test-threads
on all three architectures at `--smp 2` and 4, and test-quarantine, run:
PASSED with the accelerator named on the landing hash; (D12)'s parity
lines in the commit; coverage carried; `docs/sysml/05-scheduling.sysml`'s
affinity attribute and TRACEABILITY's L.sched.8 row updated; the commits
cite ledger 4446 and 5156. MEMORY-AND-TIMING's new §2.2 section (D2)
covers F7 (no new lock; `r` under the home queue's lock) and the fallback
(no walk at a return to service) (A10).

### 2.4 Ids

L.sched.96-101 as (D10) assigns them, and L.sched.123 for the fallback
(A2); L.sched.107-109, parents H.SCHED.2 (107, 108) and H.SCHED.1 (109).

## 3. The frame mark

### 3.1 Who holds the right: sessiond

GAME-MODE.md §8.2 gave the right to the session service of T1, which was
not chosen. The holder this design names is **`sessiond`**, seat0's owner
(`src/user/system/linux/compositor/sessiond`, `docs/AUTH.md` §6.2):

* it is **root**, a unit of `graphical.target` that init starts and
  restarts, and so is outside the user's reach (ledger 698 G2: hyprix and
  yserver run as the user and are reporters, never holders);
* it is **hyprix's parent and its only trusted channel**: descriptors 3 to
  5 are socket pairs no other program of the account can reach, and it is
  already the relay that checks a request it carries and decides nothing
  on the user's word (`docs/SETTINGS-WRITER.md` §12);
* it **places the session's scopes** (`scope.rs`), so it can find the
  game's scope and the path programs' processes itself, as ledger 5115 (8)
  requires, rather than take a pid as given;
* it **ends with the session**: its exit closes the right, and every mark
  ends with it (§3.3, M4), so no failure of the holder leaves a mark
  behind.

Rejected: hyprix (the user's, 698 G2); the GPU driver (a driver given a say
over every process's order); settingsd (absent where
`settings_writer=absent`, and a writer of settings, not the seat's owner);
init (pid 1 grows a policy); a new service (T1's, not chosen).

What `sessiond` gains is ring 3 and outside the item; it is code-reviewed
as any change to it, against these rules:

1. It opens the right at its start (§3.2) and holds it for its lifetime.
2. It reads the **administrator's list**, `/etc/ferrix/game-mode.conf`
   (root's, `0644`, GAME-MODE.md §8.1). Layer B acts only for an
   application named there; the user's list alone grants nothing of it.
3. hyprix asks, on the seat channel, when a window becomes fullscreen and
   focused, naming its client's pid and app id (an X client's through
   yserver). sessiond finds the pid's scope itself in
   `/proc/<pid>/cgroup`, checks it is an `app-*.scope` of the session's
   own user and that the application is on the administrator's list, and
   only then admits the session. What 698 G2 and SETTINGS.md §7.3 say of
   the grant's reach (every program of that user, while the admitted one
   runs) is said on the Settings page.
4. hyprix, yserver and the GPU driver may then report threads that did
   frame work (the render thread that submits, the thread that presents,
   the path's own loops). sessiond marks a reported thread only if it
   finds it, itself, in the admitted game's scope or in one of the path
   programs: hyprix (its own child), yserver (in the session's scope) and
   the GPU driver (by its unit). Anything else is dropped and counted.
5. At focus loss, fullscreen's end, the scope's `populated 0` or the
   user's key it unmarks everything (§8.2 of GAME-MODE.md).
6. It never marks more threads than leave one processor without a marked
   task to run (the 3-of-4 floor of GAME-MODE.md §5.5): a policy on top of
   the kernel's own bound (§3.3, M3), not in place of it (ledger 5115 (6)).

### 3.2 The right: `/dev/ferrix-frame`

A character device of the chardev core (load ring), `0600 root`, made by
the kernel, as `/dev/ferrix-crashes` is. **The open description is the
right**: every mark is given through one and belongs to it.

* `open` needs euid 0 in the initial user namespace (a namespaced root is
  refused) and is refused (`EBUSY`) while another description is open, so
  the right has one holder at a time and a restarted sessiond gets it
  once its predecessor's is closed.
* **The check is made again at every call** (K3): `FRAME_MARK`,
  `FRAME_UNMARK` and `FRAME_CLOCK` each check that the caller's euid is 0
  in the initial user namespace, `EPERM` otherwise, so a leaked
  descriptor gives a user program nothing. sessiond opens it `O_CLOEXEC`
  and never passes it on (its code review).
* `ioctl(FRAME_MARK, {tid, request_ns})` marks the Linux task the number
  names in the holder's pid namespace, with a slice request for F7 (zero
  for none, otherwise F7's range); `FRAME_MARK_NATIVE` names a native task
  by its own id, never a number tried first as a Linux tid and then as a
  native one (K5); `ioctl(FRAME_UNMARK, ...)` ends one mark; `read` lists
  the marks the description holds. A kernel thread is refused (`EINVAL`).
  Native tasks are markable because M5 changes no placement, so
  `sched_setaffinity`'s `ESRCH` for them is no conflict; sessiond finds
  the GPU driver's tasks itself, through init's unit for the driver and
  the process table, a lookup its code review names (K5).
* **Identity** (K6): the description holds references to its marked tasks
  (weak references or generation-checked ids), never bare tids; a task's
  mark word holds the description's id, from a 64-bit counter never
  reused within a boot; exit and exec take the task off the giver's list,
  so dead tasks never hold `MAX_MARKS` slots. The window between
  sessiond's scope check and `FRAME_MARK`, in which a tid could be
  reused, is closed by sessiond reading the marked task's scope again
  after the mark and unmarking a mismatch.
* A description holds at most `MAX_MARKS` marks (a constant, 64),
  `ENOSPC` past it: the list close walks is bounded.

### 3.3 The rules

* **M1 (L.sched.110), only the right marks.** A task is marked only by a
  `FRAME_MARK` on an open description of `/dev/ferrix-frame`, made by a
  caller with euid 0 in the initial user namespace, which only such a
  caller can open. No task can mark itself, and no other call sets the
  mark.
* **M2 (L.sched.111), the order.** Among a fair queue's **eligible**
  entities, a marked one is picked before an unmarked one; among each,
  the earliest virtual deadline, as now. A woken or arriving marked entity
  that is eligible preempts a running unmarked one, except as M3 (b) and
  (c) say. When nothing in either tree is eligible, the fallback is
  today's, the earliest deadline over both trees, not marked first.
  `hand_over` is taken only with nothing else queued, so M3 counts nothing
  there (K8). Eligibility itself,
  the virtual time and every lag are computed as without marks: a marked
  entity that has had its share is not eligible and is not picked before
  anything (ledger 5115 (6): never strict priority).
* **M3 (L.sched.112), the kernel's bound for the unmarked** (K1). It
  bounds service, not only decisions:
  (a) the queue counts, in `choose`, every decision (a timer's, a wake
  preemption's, a yield's, a block's) at which the marked-first order
  picks a marked entity where plain EEVDF over both trees would have
  picked an unmarked one; a decision where they agree resets the count;
  (b) when the count reaches `MARK_PASSES` (a constant, 2; changing it is
  a requirement change, K11), the decision is plain EEVDF, and the
  unmarked entity it picks is preempted only by plain EEVDF's
  `should_preempt`, never by M2's cross-tree rule, until its request is
  renewed or it blocks;
  (c) M2's cross-tree preemption is off while the count stands at
  `MARK_PASSES`.
  So no more than `MARK_PASSES` consecutive decisions pick a marked entity
  where EEVDF would pick an unmarked one, and the unmarked one then picked
  keeps its request. The bound is in decisions, not in time, and holds
  whatever the holder does. Lent marks (§4) count as marks here.
* **M4 (L.sched.113), the end.** A mark ends, before the call that ends it
  returns:
  (a) when the description that gave it is closed, by `close`, by the
  holder's exit, or by its last reference going; every mark it gave ends
  then, and the close returns only after the last is cleared;
  (b) when the marked task passes `execve`'s point of no return;
  (c) on `FRAME_UNMARK`;
  (d) when the task exits.
  **A clone, fork or vfork child is never born marked**, and its parent's
  mark is not copied (ledger 5115 (7)).
* **M5 (L.sched.114), the mark changes nothing else.** Marking or
  unmarking changes no weight, virtual runtime, lag or placement, and
  moves no task between queues; it re-sorts the entity on its own queue
  under that queue's lock. Placement stays Rule A's (`docs/SCHED-WAKE-
  AFFINE.md` §2) and F3's masks.
* **M6, the request.** A mark given with a request sets F7's `r` for as
  long as it lasts; its end restores the task's own request (R7.3's, or
  none). A `sched_setattr` on a marked task sets its own request, which
  is kept beside the mark's and in force once the mark ends; while the
  mark lasts the mark's request applies (K8). Covered by L.sched.107's
  check and M4's.

**H.SCHED.19** (item level, parent of L.sched.110-114; K12): *a task the
frame mark's holder marks is picked before unmarked eligible tasks of its
queue; no more than `MARK_PASSES` consecutive decisions pick a marked
entity where EEVDF would pick an unmarked one, and the unmarked one then
picked keeps its request; and no mark outlives its holder's right, its
task's exec or the task.*

**H.SCHED.2 is not widened** (K2). Its statement gains a scope: *on a
queue holding no marked or lent-marked entity*, which is every queue in
`max` and `fast` (K4) and every queue the `fair` line measures. On a
marked queue the bound claimed is M3's, under H.SCHED.19. Picking any
eligible entity bounds an entity's positive lag only by the others'
negative lags, which grows with their number, so no lag bound for marked
queues is claimed without a written proof here and a property test over
many entities of mixed weights.

### 3.4 How it is built (for the code review)

* `RunQueue` keeps two trees under one virtual time: marked and unmarked.
  `sum` and `load` cover both, so eligibility is unchanged; `choose`
  picks from the marked tree's eligible first, then the unmarked's, then
  the fallback; M3's count lives on the queue. `should_preempt` compares
  across the trees by M2. Host tests prove the lag sums identical to one
  tree's for any sequence (a property test over random operations).
* The task holds its mark as one word (the giver's description id, zero
  for none) written under its home queue's lock (D1's rule); the
  description holds references to the tasks it marked (K6). Close walks
  the list: each task looked up, its mark cleared under its queue's lock,
  the entity moved between trees. Exec clears it at the point of no return
  on the exec path that already resets signal handlers.
* **Lock order** (K7), in MEMORY-AND-TIMING: `FRAME_MARK` takes the task
  lookup, then the task's home queue lock with (D1)'s re-read and retry;
  close walks at most `MAX_MARKS` (64) entries taking one queue lock at a
  time, never two, and never takes the description's list lock under a
  queue lock; exec clears under the home queue lock at the point of no
  return.
* **Profiles** (GAME-MODE.md §8.4; K4): "held by no program" is no
  mechanism, since any root program could open the device. So **the
  right is compiled out by a build flag** declared in `build-flags.toml`
  through BUILD-FLAGS' review, absent in `max` and `fast`. The scheduler's
  mark code (the two trees, M2, M3, M5) stays in every profile with the
  item's full evidence: its host tests and boot checks run everywhere,
  the boot checks marking through check code. A `max` and `fast` boot
  check reads that the device is absent and no mark was given, and the
  safety manual says so.

### 3.5 Checks and controls

* **Host tests:** M2's order over random queues; M3 (K1) by a property
  test over random operation sequences against a shadow plain-EEVDF pick
  (a marked entity with a 0.1 ms request and a full lag against an
  unmarked eligible one: at most `MARK_PASSES` passes, then the unmarked
  one is picked and not cut by the cross-tree rule); M5's sums against
  one tree's.
* **Boot check (`sched/frame_check.rs`, L.sched.111-112, 114):** on one
  processor, a marked spinner with the minimum request and owed lag
  (it slept), and an unmarked spinner: the unmarked one runs within
  `MARK_PASSES` + 1 decisions of becoming eligible (read from the queue's
  pass count and the switch counts), and the marked one is picked first
  while both are eligible. Marking and unmarking leave both lags as
  `inspect` read them before.
* **test-frame (a root user program in `src/tests/`, L.sched.110, 113):**
  open as uid 1000 `EACCES`; open as root in a user namespace refused; a
  uid-1000 process handed the descriptor by `SCM_RIGHTS` gets `EPERM` on
  `FRAME_MARK`; a second open `EBUSY`; mark a sleeping child and read the
  mark through `read`; close and read it gone through a fresh open; mark,
  then the child execs: gone; a marked task forks: the child unmarked;
  unmark; exit; `MAX_MARKS + 1` gives `ENOSPC`; a kernel thread's number
  `EINVAL`.
* **Controls, each FIRED by `gate.sh control --expect`:** close leaves
  marks ("a mark outlived its giver's close"); exec keeps the mark ("a
  mark survived exec"); clone copies it ("a child was born marked"); M3's
  count removed, and M3 (b) removed ("an unmarked task was passed over
  more than MARK_PASSES times"); the marked-first order removed (111);
  marking moves the virtual runtime (114); the open's uid check removed
  ("a user opened the frame right"); the per-call check removed (K3).

## 4. Lends: the mark carried through a wait

GAME-MODE.md §5.3. A marked thread that waits on another task to answer
lends that task its mark until the answer. Three kinds, by what the kernel
knows about who will answer.

### 4.1 A served request: exact

When the kernel forwards a caller's request to a ring-3 task and the
caller sleeps for the reply, it knows the server: the chardev core's and
the render core's requests to their drivers (a render node's
`EXECBUFFER2` and fence waits reach the Intel driver this way), and a
native channel call (`channel_write_read`) whose message a server task has
taken. The direct switch (`sched/direct.rs`) already hands the processor
over on a call, but only where nothing else is queued; the lend is on the
general wake path too (ledger 5115's lesser point).

* **L1 (L.sched.115).** While a marked caller waits for the reply to a
  request a server task has taken, that server task is lent the caller's
  mark. The lend ends, before the caller runs again, when the reply is
  written, when the caller's wait ends otherwise (signal, timeout,
  cancel), when the caller's own mark ends (M4), or when the server
  exits. A server holds at most one lend per caller, and lends from at
  most `MAX_LENDS` callers (a constant, 8); a lend past that is not made,
  and counted.

### 4.2 The wake history: measured, for futexes and `futex_waitv`

A futex word names no owner, so the kernel cannot know who will wake a
waiter; it can know who **did** (scx_lavd's graph, measured edge by edge
instead of inferred).

* **L2 (L.sched.116).** A futex wake -- `FUTEX_WAKE`, `WAKE_BITSET`,
  `WAKE_OP`, `REQUEUE`, `CMP_REQUEUE` and `futex_waitv`'s wake -- made by a
  user task in its own system call records, in the woken task, the waker
  and a streak: incremented when the waker is the same as last time, reset
  to one when not. Wakes from an interrupt, a timer, a signal, a channel,
  a pipe or any other kernel path record nothing (N1). Written only on
  wakes of marked tasks, so an unmarked machine pays nothing. The waker is
  held as a reference or a generation-checked id, never a bare tid, so an
  exited waker whose number was reused is not lent to (N3). When a marked task blocks on a
  futex or `futex_waitv` and its recorded waker's streak is at least
  `LEND_STREAK` (a constant, 3), that waker is lent the mark until the
  marked task is woken, by anyone, or its wait ends, or its mark ends, or
  the lendee exits; the same `MAX_LENDS` cap holds. **A wake-history lend
  goes only to a waker in the marked task's own process or its own job**
  (cgroup), both of which the kernel knows; any other waker gets none, and
  the refusal is counted. L1 is the only lend across processes (N2).
* **L3 (L.sched.117), no chains.** A task holding only lent marks lends
  nothing: its own blocks make no lend. One hop, from a marked task to the
  task that answers it. A longer chain is a later design with a
  measurement behind it.
* **L4 (L.sched.118).** A lent mark is a mark for M2, M3 and M5, and for
  nothing else: it sets no request (M6), it is not listed by the right's
  `read`, and the holder cannot end it except by ending the lender's mark.
  Lent marks count in M3 as (K1) counts them; nothing in L1 to L4 bypasses
  it (N7).
* **Built without allocation** (N5): a lendee's lends are a fixed array of
  `MAX_LENDS` (8) lender references in the task; the overflow is counted
  and tested. **L1 names its paths** in its requirement's units (the
  chardev core's forward, the render core's forward,
  `channel_write_read`'s taken message); an unlisted path makes no lend
  (N4). **Lock order** in MEMORY-AND-TIMING per path (N6): a lend's start
  and end take only the lendee's home queue lock ((D1)'s retry), never
  nested with the lender's queue lock, and under a futex bucket's or a
  channel's lock only where the existing wake already nests that way.

### 4.3 NT sync objects: not yet

Ferrix does not serve `/dev/ntsync` (`docs/WINE.md`: not done); Proton
uses fsync, which §4.2 covers. If Ferrix serves ntsync, a mutex's owner is
known exactly and a marked waiter lends to it as L1 does; that is a
change to this section with its own id, not reserved now.

### 4.4 Checks and controls

* **Boot checks (`sched/lend_check.rs`):** L1: a marked caller's request
  taken by a server task spinning beside an unmarked spinner on one
  processor: the server is picked as a marked task until it replies, then
  as an unmarked one; a signal to the caller ends the lend. L2: a marked
  task woken three times by one waker then blocking on a futex: the waker
  holds the lend until the marked task's wake; a different waker's wake
  ends it too. L3: a lendee blocking makes no lend. Each of L1's ends
  (reply, the caller's wait ending, the caller's mark ending, the server's
  exit) a case of its own (N4). Each read through `inspect` and the
  queue's mark counts.
* **Controls, each FIRED:** the reply not ending the lend ("a lend
  outlived its reply"); a wake not ending a futex lend ("a lend outlived
  its waiter's wake"); a lendee's block lending ("a lent mark was lent
  on"); the cap removed ("a task held more than MAX_LENDS lends"); an
  interrupt-context wake recorded (N1); a cross-job lend made (N2). All
  by `gate.sh control --expect`. No code of §4 before §3 has landed (N8).

## 5. The idle-gap class

GAME-MODE.md §5.5: work that must run but not now, given a class that
runs where no marked task is runnable. Ferrix already has the weight half
of it: a cgroup with `cpu.idle` 1 runs at the entity weight 3
(`docs/CGROUPS.md`, L.object.185). What it lacks is the order.

The idle class is ordered **below marked entities only**, not below
unmarked ones, so M3's bound covers it whole (ledger 5194 (G1), its second
option). The first draft put it below unmarked work as well, and then
"nothing is starved" was false against unmarked load: M3 counts only
marked-over-unmarked passes.

* **I1 (L.sched.119).** An entity whose job is `cpu.idle` is never marked
  and never lent a mark: `FRAME_MARK` refuses it (`EINVAL`), L1 and L2
  make no lend to it, and a marked task moved into such a job loses its
  mark. So M2 orders every marked entity before it, and M3 bounds that
  order exactly as for any unmarked entity.
* **I2 (L.sched.120).** An idle-class entity's request ends no later than
  the next due time when §6 gives the queue one, and never falls below
  `MIN_REQUEST_NS`: when the due time is nearer than that, it gets
  `MIN_REQUEST_NS` (G4).

What is put in the class is ring 3's (lode's segment compression, a
deferred poll), by sessiond's writes of `cpu.idle` on their cgroups,
recorded and restored as §2.1's masks. The btrfs commit, the audit
reader and the machine-check poll are never put there (ledger 5115 (5)),
a rule sessiond's code review checks (G3).

Checks: a boot check marking a task in a `cpu.idle` job (refused), a
marked task moved into one (unmarked), and a futex lend to one (not
made). Control: I1's refusal removed, FIRED. I2's check is §6's.

**No code of §5** until §3 has landed and §7's measurement, recorded in
GAME-MODE.md §11, shows neighbours in the frame's gaps once the mark is
on; then its own code review (G2). The weight alone may be enough.

## 6. Deadlines from the frame

GAME-MODE.md §5.2 point 3 and GAME-PATH.md's question 3 (may game work
open stage 14 early). This design **does not open stage 14**: it adds no
class, no domain, no admission and no EDF. It gives the mark one more
word, inside the fair class.

(Named FD1 and FD2 so as not to be read as ledger 4446's D1 to D12; E5.)

* **FD1 (L.sched.121).** The holder may give its right a frame period and
  phase (`FRAME_CLOCK`, from hyprix's vblank, which GAME-PATH §4.1 makes
  the real flip time), and the kernel computes the next due time itself,
  so no call is made per frame. The period is 1 ms to 1 s, `EINVAL`
  outside; the arithmetic is overflow-free across the clock's wrap (E2).
  `FRAME_CLOCK` is under (K3)'s per-call check, and clearing it or the
  right's close ends it (E4).
* **FD2 (L.sched.122).** While a due time is set, an idle-class entity's
  request ends at or before the next due time, never below
  `MIN_REQUEST_NS` (I2). A rule only: nothing claims that any work is done
  by that time (E3). A marked entity's request does not change: all marked
  tasks share one due time, so it orders nothing among them.

So the due time is the idle-gap class's bound and nothing more, until a
machine runs two frames at once. GAME-PATH's G1 and G2 (a `SoftRt` domain
and a fixed-priority class with budgets) stay stage 14's, and question 3
stays the customer's. **No code of §6 until §5 is built and measured**
(E1); the ids are reserved so the design is complete.

Checks, when built: the due time computed from period and phase against
the clock at each decision (host test of the arithmetic across the wrap;
boot check that an idle-class request ends at or before the due time and
never below `MIN_REQUEST_NS`, and that the right's close clears the clock;
control: the cap removed).

## 7. Measuring on the box

On the N150 (`docs/N150.md`), turns taken through
`~/.local/share/ferrix/n150-linux/BOX-HOLDER`. Endfield in the world at
The Hub, as GAME-MODE.md §2.3; Wine's per-second fps channel (never
hyprix's counter); the **lowest second and the spread**, not only the
mean; alternating A/B windows, three rounds; at the played settings
(1280x720, render scale 60%) and **at 640x480, where the processors
co-limit** (39.6 fps, ~93% busy: GAME-MODE.md §3.4).

* **F3:** the game's threads masked to processors 1 to 3 and every other
  Linux process to 0 (a root script standing in for the holder), against
  no masks.
* **F7:** hyprix's, yserver's and the game's render thread's request at
  0.5 ms (`sched_setattr`, a small tool), against none; with and without
  F6 if it has landed.
* **The mark (§3):** after F3 and F7, with sessiond marking.

What the box can show at 720p is limited: the GPU is 99.9% busy, and the
lows, not the mean, are where a CPU lever can show. Each result goes into
GAME-MODE.md §11 and §9 below, with the raw files under
`~/.local/share/ferrix/logs/game-mode/`.

## 8. Questions for the review, and their answers

The consultant answered each (ledger 5154 to 5201, 2026-10-30):

* **Q1 (F3).** Does ledger 4446 stand with the owner changed, (D5) built
  by widening quarantine's predicate, and the fallback rule? *Yes, amended:
  (D3) takes the in-service set, the fallback is its own id (L.sched.123),
  and the mask test sits beside `placeable`, not inside it (5156 (A1) to
  (A4)).*
* **Q2 (F7).** The cap narrower than Linux, and `sched_setattr`
  unprivileged under Linux's permission? *Both accepted (A7).*
* **Q3 (the mark).** sessiond as holder and the open description as the
  right: *accepted, with the check made at every call (K3).* Deactivated
  code in `max` and `fast`: *refused; the right is compiled out by a
  build flag (K4).*
* **Q4.** Native tasks markable: *accepted, by a distinct ioctl (K5).*
* **Q5.** H.SCHED.2 widened on marked queues: *refused; its scope is
  narrowed to unmarked queues and the marked-queue bound is M3's, which
  bounds service, not only decisions (K1, K2).*
* **Q6 (lends).** *Accepted, with the history written only for futex wakes
  in a user's own call and lends only inside the process or job (N1,
  N2).*
* **Q7 (idle-gap, deadlines).** *Accepted as designs, no code until the
  mark measures (G2, E1).*

## 9. Where it stands

2026-10-30: written in one session (`great-greider-debf91`) from the tree,
GAME-MODE.md, SCHED-WAKE-AFFINE.md, GAME-PATH.md §5, CORE-QUARANTINE.md,
p1-036 and ledger 4446. Ids reserved with this note
(`tools/common/data/requirement-reservations.json`). The consultant's
design reviews the same day, internal and not independent verification
(F-27), each verdict's conditions carried into its section above:

| § | Piece | Verdict | Ledger | Built |
|---|---|---|---|---|
| 2 | F3 applied affinity, F7 slice requests | OK IF (A1)-(A12); 4446's D1-D12 stand | 5156 | no |
| 3 | The frame mark | OK IF (K1)-(K12) | 5170 | no |
| 4 | Lends | OK IF (N1)-(N8); no code before §3 lands | 5184 | no |
| 5 | Idle-gap class | OK IF (G1)-(G5), design only | 5194 | no |
| 6 | Deadlines | OK IF (E1)-(E5), design only | 5201 | no |

What each piece still owes before its code review: §2 the list of every
reader of the mask and of `r` with the lock it holds (A5), and the
MEMORY-AND-TIMING section (A10); §3 the build flag through BUILD-FLAGS'
review (K4) and sessiond's own code review (K10); §4 to §6 the landing of
the piece before them.
