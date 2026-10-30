# Core quarantine: taking an unstable processor out of service

Design version 2, 2026-10-30, branch `feature/core-quarantine`. Version 1
(5e2c4cdc7) had the consultant's OK IF Q1-Q14 (ledger 4856, §9); this
version carries Q2-Q13 (Q1). Item 6 of
the customer's eight hardware-error follow-ups (2026-10-30: "go for all
items on the list then start with 1, 6, 7"). It builds on the machine-check
log (`docs/MACHINE-CHECK.md`, landed d787815b1), whose poll counts each
processor's corrected errors in `/proc/ferrix-mce`.

## 1. Why

An overclock that is too far usually fails on one core first: that core's
L1/L2 starts correcting errors, then one day fails to correct one and the
machine stops. The corrected errors are the warning, and `MACHINE-CHECK.md`
now counts them per processor. What nobody can do yet is act on them: a
Ferrix processor, once started, runs tasks until the machine stops
(`smp.rs`: "Never cleared: nothing takes a CPU offline"). Linux can take a
processor offline by hand (`echo 0 > /sys/devices/system/cpu/cpu3/online`),
and rasdaemon can do it on a threshold on some AMD parts. This gives Ferrix
both, with the decision in ring 3 and only the mechanism in the kernel.

## 2. What is there (survey of `develop`, 2026-10-30)

* **Placement** asks `task.may_run_on(cpu) && domain_cpus().contains(cpu)`
  (`sched/mod.rs` `may_place`), and `choose_cpu`, `place_on_idle`,
  `pull_onto_waker`, `steal_work` and `balance` all filter by the one
  `DOMAIN`, fixed at `sched::init`. `IDLE` (a bit per idle processor)
  attracts wakes.
* **Affinity** is fixed at a task's creation; `sched_setaffinity` is
  accepted and not applied (`syscall/limits.rs`).
* **Pinned work**: each processor's idle task, its machine-check poll task
  (`arch/x86_64/mce.rs`), `msrlog` and `powerctl` on the boot processor,
  and user tasks made by `prepare_user(Some(cpu))`.
* **Interrupts**: an MSI is aimed at the local APIC of the processor that
  allocated it (`arch/x86_64/msi.rs`), and with interrupt remapping (every
  driver's device, F-57) the destination is held only in its IRTE
  (`iommu/remapping.rs` `remap_message`). Nothing records which processor a
  vector targets, and nothing retargets one. The console's I/O APIC line
  goes to the boot processor.
* **Linux's surface**: `/sys/devices/system/cpu/cpuN/online` (N > 0) reads
  `PerCpu::is_online()`, a write is `EACCES`; `/proc/cpuinfo`,
  `sched_getaffinity` and `sysconf(_SC_NPROCESSORS_ONLN)` read the same
  flag, as do TLB shootdowns and `run_everywhere`, which wait for every
  online processor.
* **lode** reads `/dev/kmsg` as uid 91, holds no capability, and parses
  `mce:` lines into `MCE_CPU`/`MCE_CLASS` (`src/lib/log/lode/src/mce.rs`).

## 3. The mechanism (kernel)

One function, `smp::set_in_service(cpu, bool)`, under one mutex, and two
named per-processor predicates (Q5):

* **`is_online`** (unchanged): started and answering IPIs. It keeps every
  caller that needs an answer to arrive -- TLB shootdowns
  (`invalidate_pages`), grace periods and F-60's barriers
  (`answer_grace_periods`, `synchronize_within`), `run_everywhere`,
  `stop_others`, `interrupt_one`, `preempt.rs`'s check -- so a parked
  processor keeps answering them (ruling (b)).
* **`in_service`** (new): eligible for tasks and interrupts. The Linux
  surface takes this one: sysfs `online`/`offline`/`cpuN/online`,
  `/proc/cpuinfo`, `sched_getaffinity` (`limits.rs`) and through it `nproc`.

Every product caller of `is_online` named in the consultant's survey
(`limits.rs` 295, procfs render 118 and 506, sysfs 1329, `smp.rs` 611,
1489, 1530, 1690, 1837, `preempt.rs` 187) is listed in the code review with
the predicate it takes; the first four move to `in_service`.

### 3.1 One placement helper, at every enqueue (Q2)

`ACTIVE` is a set of words (one bit per processor, atomics). As built,
there is no single `eligible` helper: `sched::in_service(cpu)` reads the
bit and each placer adds it to its own `DOMAIN ∩ affinity` test
(`may_place`, `choose_cpu`, `place_on_idle`, `idle_allowed`), and every
placer and every enqueue goes through one of those: `may_place`, `choose_cpu`,
`place_on_idle`, `steal_work`/`pull`, `balance`'s `allowed`, and each path
that queues a task on its `task.cpu()` -- `wake_at_home`'s fallback insert
(`sched/mod.rs` ~2225), `wake_onto`'s home, the preempt requeue, `yield`,
`set_weight`'s requeue, a direct switch's return, and fired sleepers. Under
the target queue's lock: if the home is out of service the task is
re-homed onto `choose_cpu`'s in-service answer. A task blocked on a
channel, futex or port is on no list; this is what finds it when it wakes.

### 3.2 Taking a processor out (Q2, Q3, Q4)

1. **Refusals**, each before any state changes (Q6): the boot processor;
   the last processor in service; a processor running a task pinned to it
   alone other than its idle and machine-check poll tasks (ruling (g):
   today only checks make such tasks; if `sched_setaffinity` is ever
   applied this becomes an unprivileged veto and comes back for review); a
   domain whose mode does not steal work (only Throughput exists; an RT
   domain's admission must not be broken by evacuation); a target processor
   for the retarget above APIC ID 255; and any interrupt source aimed at the
   processor that cannot be retargeted (§3.3). On AArch64 and ARMv7-A
   `set_in_service` returns `EOPNOTSUPP` and `cpuN/online` stays `EACCES`
   (retargeting there is `GICD_IROUTER`/`ITARGETSR`, a later step).
2. **Retarget** every interrupt aimed at the processor (§3.3), with each
   invalidation's completion observed.
3. **Clear its `ACTIVE` bit**, then `synchronize()`, so no placer that read
   the old bit is still placing.
4. **Request the park**: an IPI sets the processor's request and
   need-resched only. The running task is preempted with its saved state
   and moved like any other; the park runs **in the processor's idle
   task**, never inside a handler, with interrupts on, and the idle task
   never steals and never sets `IDLE` while parked.
5. **Evacuate** the run queue and **every sleeper-timeline entry** -- tasks,
   K10 timer objects and K5 timeouts -- onto in-service processors, under
   the two queues' locks (`move_onto`).
6. **The handshake**: one per-processor atomic, `Active -> Parking ->
   Parked`; at the deadline the waiter compare-exchanges `Parking ->
   Cancelled`, so exactly one of park or undo wins. A processor that finds
   `Cancelled` does not park; the undo sets `ACTIVE` again (`ETIMEDOUT`).
7. **Parked**: timer disarmed, the poll task's timer stopped (its
   corrected records wait in its banks until return, C3). The idle task, if
   it ever finds a task on its queue, drains it to an in-service processor
   and counts it (`/proc/ferrix-service` says `drained`; `test-quarantine`
   expects 0).

### 3.3 Interrupts (Q4)

As built (ledger 4932 accepted the order):

* **No shared lock with `msi_allocate`.** Instead, `msi_allocate` records
  each vector's destination (`AIMED`) with preemption off from reading
  the local APIC's identifier until the record is made, so the asking task
  cannot leave the processor between the two; its route -- unit and
  requester -- is recorded (`ROUTES`) once the entry is written. The
  retarget runs **after the park**: a parked processor runs only its idle
  task, so nothing can make a message aimed at it any more, and one made
  while it was parking is seen. A message with no route recorded -- in
  compatibility format, or its entry still being written -- refuses before
  the park and, if found after it, puts the processor back.
* msi.rs's "written once" and remapping's module doc are restated: an
  entry's destination field can be rewritten, nothing else.
* **The rewrite** is one aligned store of the word that holds the
  destination: on VT-d the IRTE's low quadword with Present kept set
  (VT-d 9.10), on AMD-Vi the four-byte entry (AMD IOMMU specification
  2.2.5). Vector, SID, SQ and SVT are unchanged; F-57's requester-
  verification argument is unaffected, because no verification field is
  written.
* **Invalidation** with its wait: VT-d's interrupt entry cache
  invalidation by index (6.5.2.7) and a wait descriptor (`Wait::Status`);
  AMD-Vi's `INVALIDATE_INTERRUPT_TABLE` (2.4.5) and `COMPLETION_WAIT`.
  A failed one puts the processor back in service.
* **Between the park request and the retarget**, a message still aimed at
  the processor is taken by it in its park (interrupts on) and its handler
  runs there: nothing is lost, since vectors are machine-wide, but the
  processor is not yet quiet; H.SCHED.18 says so.
* **Other sources** (Q4(g)), enumerated: device messages come only through
  `msi_allocate` (device.rs's two calls and virtio's one); the IOMMU
  units' fault and event interrupts are not used -- their interrupts stay
  masked and faults are polled; the one I/O APIC line routed is the
  console's, on the boot processor, which never goes out of service; the
  HPET is not used for interrupts. So every interrupt aimed at a secondary
  processor is a message `msi_allocate` recorded.

### 3.4 Back in service (Q8)

Leave the park, flush the whole TLB including global entries and every
PCID (INVPCID all-context, or toggling `CR4.PGE` where INVPCID is absent),
re-arm the timer, restart the poll (it reads what the banks kept), and set
`ACTIVE` **last**. Interrupts stay where they were moved (C1).

## 4. Who may ask (two doors, one function; Q7)

Both end in `smp::set_in_service`, with the same checks.

* **Root**: a write of `0` or `1` to `cpuN/online` through the `setters`
  seam with `bind`'s privilege check (`EPERM` otherwise). Without the
  setters crate the file is not writable, with no upward reference
  (`check-item-boundary.py`).
* **lode**: the kernel makes one `Object::CpuService` and gives init one
  handle, once, as it gives `Object::Audit` to pid 1. init gives lode a
  copy with `MANAGE` alone -- no `DUPLICATE`, no `TRANSFER` -- pinned by
  the native boot check as `AUDIT_RIGHTS` is. **When lode restarts**, init
  keeps its own copy and gives it again (init holds it with `DUPLICATE`
  and never sends that right on). `CPU_SERVICE(token, cpu, flag)` checks
  `cpu < count` and the flag strictly 0 or 1, else `EINVAL`.
* **Every change** through either door writes an audit record (subject,
  processor, direction, outcome) and a kernel line `cpu: N out of service
  (by root|lode)` / `back in service`.
* `RUNTIME-SETTINGS.md` lists `cpuN/online` and `CPU_SERVICE` as live
  controls. The customer asked (2026-10-30) that root can always bring a
  quarantined core back at run time, so root's `cpuN/online` write is part
  of the feature: where a build has no run-time settings (the customer's
  open max-build question), lode's door goes with that switch, and root's
  return goes too only if lode's does -- a build that can quarantine can
  always un-quarantine.

## 5. The policy (lode, ring 3; Q9, Q10)

lode turns every `mce:` line into a record with `MCE_CPU`, `MCE_CLASS` and,
on AMD SMCA, `MCE_BANK_TYPE` (`src/lib/log/lode/src/mce.rs`, landed with
`docs/MACHINE-CHECK-AMD.md`; this branch rebases on that landing, Q1). The
rule, in `/etc/ferrix/lode.d/quarantine.conf`:

```
action = quarantine      # quarantine, warn, or off
count = 10               # judgement, not a cited rate
window = 600             # seconds
cap = half               # at most this many processors per boot
keep = 1                 # never leave fewer than this many in service
```

* **What counts**: only kernel-facility `/dev/kmsg` records (a line root
  writes to `/dev/kmsg` with the same text does not), and only core-scoped
  errors -- on Intel a cache/TLB MCA code at levels L0-L2 from a core's
  banks; on AMD SMCA the IPID types LS, IF, L2, DE, EX and FP.
  Memory-controller, last-level cache and interconnect records only warn:
  a DIMM's corrected errors are polled by whichever thread got there first.
* **Per physical core**: counts are summed over a core's threads and all
  its threads are taken out together, from a read-only topology surface
  (`/sys/devices/system/cpu/cpuN/topology/core_id` and
  `thread_siblings_list`, added if absent). A partial outcome (one sibling
  refused) is recorded.
* **Bounds**: at most `cap` processors per boot, `keep >= 1` above the
  kernel's last-processor rule; never an automatic return; a refusal
  recorded once per window.
* **Back from quarantine at run time, by root** (the customer, 2026-10-30:
  "add an option to move it out of quarantine during runtime when you have
  super user permissions"): `echo 1 > /sys/devices/system/cpu/cpuN/online`
  (or `chcpu -e N`), root only, takes it back into service at once, no
  reboot. lode sees the kernel's `back in service (by root)` line, starts
  that core's window empty and counts only records after it, so a core
  the person brought back is not taken out again on its old errors; the
  return counts against no cap. A person can thus try a core again after
  lowering an overclock.
* **A configuration lode cannot read or with any value out of range means
  `warn`**, said once: lode never quarantines on a file it could not read.
* **Default: `action = quarantine`**, the customer's choice on 2026-10-30:
  "Quarantine by default". `count = 10`, `window = 600` stay a judgement,
  not a cited rate; `warn` is one line in the file, and a file lode cannot
  read still means `warn`. Both actions are tested.

## 6. What it does not do

* No ACPI hot-remove and no power-down; the processor parks in `hlt` with
  interrupts on and stays in every IPI wait. Linux's `play_dead` (`cli;hlt`,
  out of the masks, INIT to return) is deliberately not followed (C2);
  dropping the parked processor from shootdowns would need its own review
  (ruling (b)).
* No moving of a task pinned to that processor alone (refused).
* No retargeting on AArch64 or ARMv7-A yet (refused).
* Silent corruption with no MCA record is outside this design.
* Quarantine is an availability aid, not a safety mechanism (AoU).

## 7. Requirements and checks (Q11-Q13)

**Hazard**: a new `H.SCHED` id (reserved) for the mechanism's failure modes
-- a stranded task, a lost or misrouted interrupt, a stale translation, a
park never acknowledged -- each mapped to its requirement. SAFETY-MANUAL:
an AoU that quarantine is an availability aid, not a safety mechanism;
uncorrected errors still stop or recover per `H.FAIL.8`.
VULNERABILITY-ANALYSIS: two new privileged control inputs; T.DOS -- a
compromised init or lode can take all but one processor out, bounded by the
last-processor rule and lode's cap, and audited; T.IDENTIFY -- nothing new.

**Requirements** (reserved in a docs-only landing, one check each): the
shared placement helper; re-home at enqueue; evacuation including timer
objects; the park handshake and its undo; back in service; the refusals;
the IRTE retarget (single store, unchanged fields, invalidation with wait);
serialized allocation; other sources refused; the Linux surface; the sysfs
write through setters; `CpuService` and `CPU_SERVICE`'s rights and
arguments; the audit records. **MEMORY-AND-TIMING**: the evacuation walk
under two queue locks (bounded by the queue and the timeline) and the
retarget walk (at most 64 vectors) under remapping's lock.

**Checks**:

* **Stage checks**: the helper over a table; each enqueue path re-homing;
  the handshake's undo; the IRTE fields byte-compared before and after a
  retarget; the refusals.
* **`test-quarantine`** (x86-64, KVM and TCG, `-smp 4`): CPU-bound
  programs, a disk and a network reader running; eleven core-scoped
  corrected records into processor 2; lode (with `action = quarantine` in
  the image) takes it out within a window plus a poll. Then `cpu2/online`
  reads 0; the kernel's PLACED counters show nothing new on cpu 2 and no
  task's `/proc/<pid>/stat` processor field is 2; `/proc/interrupts`'
  per-processor, per-vector counts show no new device interrupt on cpu 2;
  `drained` is 0; `echo 1` brings it back and a task lands there again.
* **Modes**: a task blocked on a channel, homed on cpu 2, woken after the
  quarantine runs elsewhere; `-smp 4,threads=2` -- both siblings go; eleven
  memory-controller records only warn; `action = warn` only warns; out and
  back fifty times under load with no FX-0001 and no lost task; `--smp 2` --
  cpu1 goes, then the last-processor refusal; the refusals (cpu0, a pinned
  check task, a message without remapping) each `EBUSY`; both remapping
  units (q35 `intel-iommu,intremap=on`, and `amd-iommu` with `intremap`
  under `kernel-irqchip=split`, or AMD-Vi by argument with a BACKLOG row);
  AArch64 and ARMv7-A refuse, boot lines unchanged on all four boots.
* **Negative controls** (`gate.sh control --expect`): the helper's
  `ACTIVE` test removed (PLACED on cpu 2); the re-home removed (the blocked
  task never runs); the retarget skipped (a device interrupt counted on
  cpu 2); `cli` in the park loop (FX-0001 on a shootdown); lode's
  threshold off by one; a root-written fake `mce:` line counted.
* **Rows on the landing hash**: check, build-all, test-boot x86_64
  kvm/tcg/`--smp 2` and aarch64/armv7a/armv7a `--smp 2`, test-threads,
  test-shell `--init ferrousli`, ipc-equiv, the test-mce modes,
  test-quarantine kvm and tcg.

## 8. Answered by the review

§8 of version 1's four questions were ruled in ledger 4856: two doors,
one function (a); `is_online` kept for shootdowns (b); fold by physical
core, core-scoped errors only (c); the default is the customer's (d).

## 9. Where it stands

Design v1 reviewed by the certification consultant on 2026-10-30 (ledger
line 4856; internal review, not independent verification, F-27):
**OK IF (Q1)-(Q14)**. Rulings on §8: two doors accepted, with one function
and one set of checks behind both; keeping `is_online` for shootdowns
accepted for v1; folding by physical core required, with only core-scoped
errors counted; the default is the customer's, and either way a config lode
cannot read means warn. Also accepted: the IRTE retarget (Q4), parking in
`hlt` on the suspect core, and refusing pinned user tasks.

Found in v1, to be fixed in v2 before code (Q1):

* Blocked tasks homed on the processor are on neither list evacuation
  walks, and `wake_at_home` would queue them there. Every enqueue must
  re-home through one shared ACTIVE helper, after a grace period (Q2).
* The sleeper timeline also holds K10 timer objects and K5 timeouts (Q2).
* The park runs in the idle task, never inside the IPI handler. The
  deadline undo needs a compare-exchange handshake (Q3).
* `msi_allocate` and the retarget must be serialized. The IRTE rewrite is
  one aligned store with the vector and verification fields unchanged, the
  invalidation is waited for, and every other source aimed at the processor
  is enumerated (Q4).
* lode counts only kernel-facility, core-scoped records (no DIMM, LLC or
  interconnect records), per physical core, with a per-boot cap (Q9).
* §7's retarget control cannot fire, because the parked processor services
  a misdirected message itself. The row reads per-processor interrupt
  counts instead (Q13).

The full conditions are in the ledger entry. The lode half depends on
mce-amd's `lode/src/mce.rs` landing.

Design v2 written (Q1), carrying Q2-Q13, for the consultant's check at the
code review. The kernel half lands first or with mce-amd's lode module on
`develop`, whichever comes first (Q1).

**Built on `feature/core-quarantine` (2026-10-30)**, x86-64:

* The scheduler's half (§3.1, §3.2, §3.4): `ACTIVE`, the gated placers,
  the re-home at wake, the park in the idle task with its handshake and
  undo, the drain backstop, back in service with the whole TLB flushed.
* Interrupts (§3.3): every message's destination recorded before the
  asking task can leave the processor, and every remapped message aimed at
  a parked processor retargeted to the boot processor -- VT-d's low
  quadword or AMD-Vi's entry, one store, invalidated and waited for.
  **Changed from §3.2's order**: the retarget runs after the park, not
  before, so a message made on the processor while it was parking is moved
  too; a compatibility-format message refuses before the park, and one
  found after it puts the processor back.
* Both doors (§4) into `smp::service::set_in_service`, its refusals, the
  `CPU_SERVICE` audit record and the kernel line. **Changed from §4**: a
  handle travels with its rights and a send needs `TRANSFER`, so init
  grants lode `MANAGE | TRANSFER` (a new directory message, GRANT, written
  into lode's bootstrap channel before it starts) and lode drops
  `TRANSFER` on receipt.
* The Linux surface on `in_service`, and `cpuN/topology/core_id` and
  `thread_siblings_list` from CPUID 0Bh.
* lode's policy (§5) in `ferrix_lode_core::quarantine`, host-tested, read
  from `quarantine.conf`, acting through the token.
* `test-quarantine`: eleven core-scoped corrected records into processor 2
  under init and lode; lode takes it out, the surface says so, nothing ran
  or was drained there, and root's `echo 1` brings it back.

**Code review** (ledger 4932, 2026-10-30): NOT YET (B1)-(B4); both
changes above accepted. Answered on the branch:

* **(B1)** The gate grew the rows the review asked for, and they found two
  defects, fixed: a parked processor that took an interrupt with a
  reschedule pending ran a task queued there (and so never saw its return
  -- `pick_or_park` now picks the idle task whenever the processor is not
  fully in service), and a balance's push chose a parked processor as the
  least loaded (`steal_from` refuses a receiver out of service, and
  `balance` leaves parked processors out). Messages are now aimed at the
  processors in service in turn, so a secondary processor always holds
  some for the retarget row. `/proc/interrupts` counts each interrupt
  number per processor.
  - `test-quarantine` (four processors) and `test-quarantine-smp2`: readers
    blocked on empty pipes (`wait_trusting`, on no list) re-homed at their
    wake; drained 0; at least one message moved and, under raw disk reads,
    none delivered to the target while a moved one counts on the boot
    processor; a user that is not root refused at open (EACCES), root of a
    user namespace of its own at the write (EPERM); fifty times out and
    back under load.
  - The native boot check `a_processor_leaves_service_through_its_token_alone`:
    init's rights pinned, `CPU_SERVICE`'s refusals and answers, the
    deadline's undo, the pinned and boot-processor refusals, and the three
    audit records read back.
  - Negative controls (`gate.sh control`): the ACTIVE bit never cleared
    (the native check stops the boot), the re-home removed (a woken reader
    drained), the entry's store skipped (an interrupt lands on the target),
    the park loop with interrupts masked (a shootdown never answered), the
    capability asked in the writer's own namespace (the namespace's root
    takes a processor out) -- each fires; logs under
    `~/.local/share/ferrix/logs/queue/cq*-ctl-*.log`. Removing only
    `may_place`'s test does not fire: `choose_cpu`, `place_on_idle`,
    `steal_from` and `balance` each refuse a processor out of service too.
  - The last-processor refusal is not reachable while the boot processor
    never goes; `--smp 2` shows processor 1 going and the boot processor
    running alone.
* **(B2)** H.SCHED.18 restated for the window between the park and the
  move; every criterion names its check and control; L.iommu.104 alone is
  argued, in the baseline.
* **(B3)** The token is given only in a build with the Linux personality
  and run-time settings, where root's way back is too.
* **(B4)** MEMORY-AND-TIMING §2.2e2 rewritten with the walk each move does
  and the measured hold, 3.0 to 31.2 µs (`move-hold-max-ns`).
* C1-C9 answered in the code and in §3.1 and §3.3.

**Re-review** (ledger 4992, 2026-10-30): **OK IF (1)-(4)**, answered on
the landing commit:

* **(1)** The six controls run again on the landing hash, the new one
  among them: every placer's test removed at once (`sched::placeable`
  answering true). L.sched.102's criterion names both placer controls.
* **(2)** The compatibility-format half of L.iommu.104 is shown, not
  argued: `test-quarantine-unremapped` boots VT-d with interrupt remapping
  off and devmgr starting drivers (`ferrix.unisolated`), and root's
  take-out of each secondary holding a message is refused, each left in
  service; the native boot check's arm on that machine (and on test-nvrm's
  unremapped boot) requires lode's take-out of processor 1 refused with
  status 3. That boot found the refusals' order wrong -- the boot
  processor was refused for its messages rather than as the boot processor
  -- so the scheduler's refusals now come first. Only the race with an
  entry still being written stays argued.
* **(3)** The rows, test-nvrm and test-quarantine-unremapped among them,
  run on the landing hash with the accelerator named. No gate's expected
  transcript holds boot lines, so the native check's new kernel lines
  change none.
* **(4)** Coverage carried after the final rebase (one anchor in msi.rs
  dropped as unmeasured: `aim`'s rewritten body). The branches no run
  reaches, argued here until the next `cargo xtask coverage`:
  - **the undo's retry** (`the_undo_and_the_pinned_refusal`, `Ok(())`): only
    if something woke the processor in the microseconds between the two
    compare-exchanges; its body is `set_in_service(.., true)`, which the
    gate runs fifty times;
  - **an invalidation failure** (`vtd::Unit::retarget`'s and
    `amdvi::Unit::retarget`'s error returns, `take_out`'s put-back): QEMU's
    units never fail an invalidation; the error path is the same
    `require_published` and `submit` every entry write takes, and the
    put-back is `put_back_in_service`, which every return runs;
  - **the drain** (`park`'s count when `evacuate` moves something while
    parked): the gate requires it to stay 0, and the re-home control
    (cq-ctl-rehome) shows it counting when a task does land there;
  - **`put_back_in_service`'s timeout**: a processor that does not leave its
    park within two seconds; since C2 it sets its own bit when it gets there,
    so the timeout only reports. The stuck return the gate found (before
    `pick_or_park`) exercised it.

**Still to do** (BACKLOG): the modes the review lets follow -- threads=2's
siblings, memory-controller records and `action = warn` only warning,
lode's off-by-one and a root-written fake `mce:` line controls -- AMD-Vi's
retarget run, and the AArch64 and ARMv7-A retarget (`GICD_IROUTER`,
`ITARGETSR`).
