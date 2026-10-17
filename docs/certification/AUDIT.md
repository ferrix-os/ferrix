# Audit: a minimal record of security-relevant events

The design for finding F-21b: *"there is no FAU family at all ... an
evaluator would press on whether a TOE that cannot record a
security-relevant event can claim EAL5."* It says what the TOE records,
where, who reads it, what it costs, and how the build proves it. It is
built (§8, three slices, 2026-09-27) and claimed in the Security Target
(§5, under O.AUDIT), and F-21b is closed.

The TOE is an isolation kernel, and identity is the personality's
(SECURITY-TARGET §9.1). This keeps that split. The item records what *it*
decides: a capability refused, authority handed over, a process ended from
outside, the configuration it booted in. Who a subject is, in uid terms, is
added by the personality through a hook, as the item already takes a process
loader and a file reader from it.

## 1. What is recorded (FAU_GEN.1)

Each record is one event the TSF decided. The events are the decisions
already made at one choke point each, so recording one adds a call and
changes nothing about the decision.

| Class | Event | Where it is decided |
|---|---|---|
| `REFUSED` | A handle's rights did not cover a call: `ACCESS_DENIED` from the rights check | `syscall/native.rs`, where every native call asks for rights (`job_in`, `device_in`, the table's `get_with`) |
| `REFUSED` | A rights request that would widen a handle | `object` table, `Requested::resolve` returning `None` |
| `REFUSED` | A limit refused a charge: tasks, memory, objects at a job's limit | `object/quota.rs`, the one refusal path per resource |
| `REFUSED` | A device's pin budget refused (`DEVICE_LIMIT`): `ACCESS_DENIED` without `SET_LIMIT`, `BAD_STATE` under live pins, `NO_MEMORY` past the ceiling; the device, the limit, the old value and the one asked for. Recorded by the call in place of the generic rights record | `syscall/native.rs` `device_set_limit` |
| `GRANTED` | A job handle with `MANAGE` or `SET_LIMIT` given for a cgroup | `fs/cgroupfs.rs` `job_for_cgroup` (a load-ring caller; recorded through the item's call) |
| `GRANTED` | A native process made, with its creator's ids (P0) | `syscall/native.rs` `process_create` |
| `GRANTED` | `devmgr` started through the starter; the starter given to pid 1 | `devmgr.rs` `devmgr_start`, `init.rs` `next_bootstrap` |
| `GRANTED` | A device's control channel made for a driver (block, net, display, render, input, sound, log) | each core's `*_control_create`, through `native::serve` |
| `GRANTED` | A job made one speculation domain (`DOMAIN`): the new job, its parent and the domain's number (`docs/OPAQUE-KERNEL.md` §9) | `syscall/native.rs` `job_create` |
| `ENDED` | A process or job ended from outside: a job's kill, the scoped OOM kill, `cgroup.kill` | `object/job.rs` `kill`, `kill_members`; `object/oom.rs` |
| `DEVICE` | A device quiesced; a DMA fault the IOMMU reported for a device | `native::quiesce`; `iommu` fault handlers |
| `CHANGED` | TSF data changed by a call that succeeded: a job's limit set (`job_set_limit`), a cgroup's limit file written, a device's pin budget set (`DEVICE_LIMIT_SET`, with the old and new values) | `syscall/native.rs` `job_set_limit` and `device_set_limit`; cgroupfs limit writes |
| `SYSTEM` | The audit function's own start-up, with the ring sizes; and its shutdown, the last record before a power action | `audit::start` at bring-up; `power` |
| `SYSTEM` | The boot's configuration: `--mitigations`, the KASLR state (randomised or not, and why), `ferrix.devmgr=kernel\|init`, whether `ferrix.efifb` asked for the firmware framebuffer to be lent, **`ferrix.checks=skip`** -- a boot that skipped its self-tests leaves a record of it -- the **build flags** the kernel was compiled with (`Config::Build`, the same set as its `build` boot line, from the same `cfg`s: in the second detail word each flag's place in its list of values, two bits a flag in `build-flags.toml`'s order; in the third the profile's place among present, desktop, fast, max, none in bits 0 to 3 and the level's among none, present, sc2, sc4 in bits 4 to 7; `docs/BUILD-FLAGS.md` rule 6 d). In `desktop` the record names `ipc_fastpath=on`, `program_barrier=session`, `ssbd=on_request` and `runtime_trades=game_mode` while the image runs their stand-ins, today's stricter `off`, `switch`, `always` and `none` (`build-flags.toml`'s `stands_in`), until each value's own landing; a stand-in is allowed only at level `none` (`BUILD-FLAGS.md` §3.1), and each self-check stage's verdict | bring-up in `main.rs`, `build_flags.rs` |
| `SYSTEM` | The root switch and pid 1's re-root; a power action | `fs::root_disk`; `power` |

Not recorded: successful calls in general (a mapping, a message), because
their volume would make the record useless and they are no decision against
anyone. What is recorded of successes is authority handed over (`GRANTED`)
and TSF data changed (`CHANGED`), which FAU_GEN.1 expects whatever else is
left out.

**The safe state.** A panic halts the TSF (O.FAILSAFE) and cannot write a
record anyone reads. Its FX code and explanation survive in the console's
kernel log ring and on the serial line, and on the Pixel 7 in the `ramoops`
record read back after the watchdog's reset. That is the audit trail for
that one event, and the design says so rather than claiming a record. Linux-personality decisions -- `setuid`, `EPERM` from credentials,
`execve` of a set-id file -- are outside the TOE; the personality may submit
them through the same interface (§4), marked as its own.

## 2. What a record holds (FAU_GEN.1, FAU_GEN.2, FPT_STM.1)

A fixed 64-byte record, so the store never allocates (`audit::Record`):

| Field | Bytes | |
|---|---|---|
| sequence number | 8 | per boot and per ring, gapless: a missing number is a lost record |
| time | 8 | monotonic nanoseconds since boot (`timer::now_nanos`), the TSF's own clock; wall time is the reader's to add |
| class and event | 4 | §1 |
| outcome and status | 4 | the native status or errno answered |
| subject | 16 | pid and job id, which the TSF attests -- job ids are 64 bits -- and, in a separate field flagged *personality-supplied*, the uid the personality's hook gives (`u32::MAX` when it gives none) |
| object | 12 | the object's kind and identity: a handle's object kind and job id, a device's PCI location, a resource |
| detail | 12 | the rights asked and held, or the limit and the use |

The start-up record carries the whole audit id, its low half as the object
and its high half in the first two detail words, and the two rings' lengths
in the third.

FAU_GEN.2 (associating an event with a user) is claimed, refined, for the
TOE's own subjects: the process and its job, which the TSF attests. The uid
is personality-supplied data carried beside them and marked as such, under
the OE.AUTH and A.AUTH pair `docs/AUTH.md` added; no record presents it as
the TSF's identity of the subject.

## 3. Where it is kept (FAU_STG)

In the TSF, two rings, allocated at bring-up before the boot marker as the
kernel log's buffer is, so nothing on a recording path allocates:

* **The high-value ring**, 512 records (32 KiB): `GRANTED`, `CHANGED`,
  `ENDED`, `DEVICE` and `SYSTEM`. They are rare, and `REFUSED` traffic can
  never evict one.
* **The refusal ring**, 4096 records (256 KiB): `REFUSED`, with fairness
  per budget. Past 64 records charged to one budget in a second, its further
  refusals fold into one *suppressed n* record for it, written when its
  second ends. A program that provokes refusals by the thousand -- varying
  the object, which defeats a fold by identical event -- flushes at most its
  own unit's older refusals, never another unit's, and never a grant.
* **The budget** a refusal is charged to is its job's, fixed when the job
  is made (`Job::audit_budget`): its own id, or its parent's for a job the
  program's own authority could have made -- an anonymous one, which anyone
  holding a job handle that allows it makes, and a named one in a directory
  someone other than root may write, which is a delegatee's `mkdir`. The
  maker decides (`job::Budget`), since whether a directory is writable by
  others is cgroupfs's to know and not the core's. So a program that makes
  sub-jobs to refuse from shares its unit's one budget rather than gaining
  64 a second per job (the certification review, 2026-09-27), and a job
  made by root in a then root-only directory stays its own budget if the
  directory is `chown`ed later. Fixed at creation, a budget is read with no
  lock, as a limit's refusal must be from inside the heap.
  A stated residual: root in the personality may make root-owned cgroups in
  a directory only root may write, and each is a budget of its own, so root
  can widen its own refusal throughput up to `cgroup.max.descendants`
  budgets. That is root's own reach, which the personality already grants,
  not an escalation.

Each ring's lock is an `IrqSpinLock` (`crate::sync`): interrupts masked while
it is held, for a 64-byte copy and two counters. An OOM kill can be decided
with the allocator's locks held, and a limit's refusal is recorded from
inside the heap, so a lock that left interrupts on could be taken by a
handler on the processor already holding it. **They are leaf locks**:
nothing is taken under them and nothing allocates under them, so the F-23
and FX-0503 rules hold on every path that records, and everything a record
needs, its subject among it, is worked out before a ring's lock is taken --
on the heap's path without a lock of its own: a refused charge names the job
its quota slot keeps the id and budget of, and no process.

**The boot's own records.** The first ten system records -- the start-up
record, the boot's configuration and the boot brought up -- are also pinned
where nothing else reaches them (`Which::Boot`), numbered as the high-value
ring numbered them. That array never wraps and holds at most ten (eight
until the build flags' record, `Config::Build`, made the configuration
seven items, nine until `ferrix.vtd`'s, `Config::Vtd`, `L.iommu.71`, made
it eight; `audit::BOOT_RECORDS` is held at two more than
`audit::CONFIG_ITEMS` by a compile-time assertion); a system
record after them is kept in the high-value ring alone. The boot's own
checks make processes, kill jobs and set limits by the dozen, and a longer
run could wrap the high-value ring past the records that say what the boot
was before any reader exists.

When a ring is full its oldest record is overwritten and its lost counter
counts it; the reader sees the gap in sequence numbers and the count. This
is FAU_STG.1 (protected storage: nothing but the TSF writes it) and
FAU_STG.4 with its selection *overwrite the oldest stored audit records*,
the overwritten counted and the gap reported to the reader. FAU_STG.4's
other selections are declined deliberately: ignoring or preventing audited
events when full lets an attacker fill the store first and then act
unrecorded, and stopping the TSF when full would make audit a
denial-of-service lever (T.EXHAUST). FAU_STG.3 is not claimed: no alarm is
raised, and a reader learns of a loss from the gap. Overwrite keeps the newest, and the two rings and the
fairness rule keep a refusal flood from reaching what matters.

Persistence is outside the TOE: a reader writes records to disk.

## 4. Who reads it (FAU_SAR.1, FAU_SAR.2)

A capability, as the log core's reader and L12's starter are:

* The kernel gives pid 1 one `Object::Audit` handle on its K2 channel (`FXAU`,
  after the hello), with `READ` and nothing else -- not duplicable, not sent.
  Only the holder can read, which is FAU_SAR.2.
* `audit_read(audit, which, buffer, count, answer)` copies whole records of
  one ring -- the high-value ring, the refusals, or the boot's own pinned
  records -- numbered from a cursor on, at most 64 a call, and answers the
  next number, the records between the cursor and the first copied that the
  ring no longer held, and the boot's audit id: a random 128-bit number
  drawn at start-up and recorded in the start-up record, so that records of
  different boots can never be spliced into one sequence on disk. The cursor
  travels in the answer's memory, so it is 64 bits on every architecture. A
  count of zero copies nothing and leaves the cursor where it was.
* **It never blocks, and nothing signals it.** A record is made where no
  port may be woken -- inside the heap, under the ring's leaf lock, in an
  IOMMU's fault path -- so the reader polls, **once a second**. What a ring
  overwrote between two polls is never lost silently: it shows in the file
  as a gap in the numbers and in the reader's lost count.
* **The reader is init, for now** (the certification review, 2026-09-27).
  Pid 1 keeps the handle and, once `/` is settled -- the root volume after
  L12's switch, or at once when the kernel started `devmgr` -- copies every
  record into `/var/log/audit/<id>.bin`, the id in hex, 64 bytes a record as
  `src/lib/proto/audit` lays them out: the boot records first, then the
  high-value ring passing over the numbers they already wrote, then the
  refusals. It reads a last time as it goes down -- after the first `sync`
  and the unmounts, before `/` is made read-only -- so that as little as
  possible is made after it. The power action's own record, which no
  reader can read once the machine is off, the kernel says on the console
  by the number its ring gave it, with how far it saw the reader read the
  high-value ring -- counted only once a read's records are all copied out,
  so a read that faulted counts nothing -- and it prints every record past
  that, one line each (`audit    unread #N ...`), and a run the ring
  overwrote before anyone read it as one line (`audit    lost #a..#b`): a
  driver still starting after a `devmgr` restart can make some after the
  last read, and a boot with no reader keeps only the ring's last, and
  either way they reach the console's log rather than going without a
  trace. `test-init` requires init's last read to be where the kernel saw
  it stop, and every record from there to the power action to be on the
  console. With authentication's services uids (AUTH phase 2) the reader
  becomes `audit.service`, a user of its own, and init hands it the handle;
  the handle stays `READ` alone and untransferable, so the interim widens
  nothing later.
* `svc audit` prints the newest boot's file, one decoded record a line.
* A boot without init has no reader, and the ring keeps the last 4096 events,
  readable from a crash dump.

The personality's hook to submit its own events (`audit::submit`, load-ring
callable, marked `PERSONALITY`) is the one write path from outside the TSF,
and a record it submits can never claim a TSF class.

## 5. What it costs

* Memory: 288 KiB fixed (the two rings), all three architectures.
* Time: on the recorded paths only, which are refusals and grants, not the
  hot path of a call that succeeds. One uncontended spinlock and a 64-byte
  store, estimated at 50 to 150 ns under KVM and measured at 21 ns (22 for
  a refusal counted past its budget) on every boot's own check
  (MEMORY-AND-TIMING.md §1.7).
* Code: an `audit` module in the item, about 400 lines; one `record` call at
  each of about 25 sites; the `Audit` object kind and `audit_read` in the
  native ABI; init's reader, about 200 lines.
* Points: about 15 -- the two rings, the fairness rule and the records (4),
  the ~30 call sites with their negative controls (6), the capability and
  `audit_read` (2), init's reader and the disk file (2), the ST and the
  documents (1).

## 6. How the build would prove it

*As built so far (slices 1 and 2):* `audit::check` proves the store on
stores of its own and, at the end of every boot with checks, that each
decision the boot's own checks make at a recording site is in the kernel's
record with its outcome and subject (§8). `DEVMGR_STARTED`, `STARTER_GIVEN`,
`ROOT_SWITCHED` and `POWER` are made only in `test-init`'s boots or at
shutdown, which no test boot reaches: slice 3 proves them, reading them
back through `audit_read` in `test-init`.

A boot check, `audit_check`, provokes one event of every class from a check
process -- an `ACCESS_DENIED`, a widened rights request, a limit, a grant, a
kill, a quiesce -- and reads each back through an `Audit` handle with the
right subject, object, outcome and a gapless sequence. It fills the ring and
requires the lost counter and the gap, and reads with a handle lacking `READ`
to be refused. It floods the refusal ring from one check job and requires
another job's refusal and every grant made before the flood to be still
readable, and the flooding job's *suppressed n* record. It requires the
start-up record, the configuration records, and the same audit id in every
answer.

The record a `ferrix.checks=skip` boot writes cannot be checked from inside
that boot, which runs no in-kernel check. It is verified from outside: an
xtask gate boots with `ferrix.checks=skip` and reads the record through
`audit_read` -- from init's `audit.service` on the console, or a small native
program the image carries -- and asserts it is there, naming the option. Negative controls: drop the `record` call at one site, and the
check names the missing event; widen `Object::Audit` to `TRANSFER`, and the
check that it cannot be written into a channel fails. `test-init` requires
`audit.service` to have written the boot's refusals to the volume.

## 7. What the ST claims, and does not

Claimed: FAU_GEN.1 (the events of §1, at the TSF's decisions, with audit
start-up and shutdown), FAU_GEN.2 refined to the TSF's own subjects (process
and job; the uid carried beside them as personality data), FAU_SAR.1 and
FAU_SAR.2 (the capability), FAU_STG.1 (only the TSF writes the rings),
FAU_STG.4 (overwrite the oldest, counted), and FPT_STM.1 for the
timestamps. FAU_GEN.2's dependency FIA_UID.1 is unmet by design: its
subjects are the TSF's own, and people are identified under OE.AUTH and
A.AUTH. Not claimed: FAU_STG.3 (no alarm; the gap is the report), FAU_SEL (no selection: every
§1 event is always recorded), FAU_SAA (no analysis in the TOE), and the
integrity of records once written to disk, which is the environment's
(OE.AUDIT_STORE, a new objective for the environment).

F-21b would then read: audit claimed for the TSF's own decisions; FIA still
the personality's, which is the isolation kernel's position and the reason no
OS protection profile is claimed.

## 8. How it is built

Three slices, each reviewed before it lands:

1. **The store** (built): `src/kernel/src/audit.rs`, in the core ring -- the
   two rings of static storage under `IrqSpinLock`s, the gapless numbering
   and the lost count, the fairness per budget with its *suppressed n*
   record,
   the audit id, which bring-up draws from the random generator and hands
   in, since the generator is the item's and the core may not name it --
   and bring-up's records: the start-up record and the boot's configuration
   (`ferrix.checks`, `ferrix.devmgr`, the mitigations, KASLR, whether the
   log is kept across a reset, `ferrix.pstore`, `ferrix.efifb`, and the
   build flags, `Config::Build`) right after
   the generator's check, and one when the boot is brought up. Its boot
   check, `audit::check`, is §6's first half on stores of its own and on a
   job tree of its own, and the kernel's store read back (FX-0309). The
   record's layout is `src/lib/proto/audit`, whose host tests round-trip the
   start-up record's id and ring lengths through the bytes a reader gets.
2. **The call sites** (built): a record at each §1 decision the item
   makes during a boot --
   * every native call a handle's rights refused, and a widening refused,
     at `syscall::native::dispatch`, where each answer passes;
   * a limit's refusal, in `object::quota::charge`, against the job its
     slot names;
   * a native process made, a job given for a cgroup and a device's control
     channel given;
   * a job killed, a cgroup killed and an OOM kill;
   * a device quiesced, and a DMA fault an IOMMU reported, with the kernel
     as subject;
   * a limit set through a job's handle (`job_set_limit`), and a cgroup's
     limit file written, as two events;
   * a device's pin budget set or refused (`device_set_limit`, NVIDIA N0f),
     as `DEVICE_LIMIT_SET` and `DEVICE_LIMIT`, which the pin budget's own
     stage-10 check (P5) reads back with the device and both values, on
     the architectures that translate DMA.
   At the end of boot `audit::check::booted` requires a record of each that
   the boot's own checks provoke, with its outcome and subject, and the
   boot's own records pinned; a negative control per site names the event.
   `devmgr_start`, the starter, the root switch and a power action happen
   only in `test-init`'s boots or at shutdown, and are read back there in
   slice 3.
3. **The reader** (built): `Object::Audit` and `audit_read`, init keeping
   `/var/log/audit/<id>.bin` (§4), `svc audit`, `test-init` reading the
   record back and a `ferrix.checks=skip` boot's from outside, and nothing
   before a power action lost without a console line. Then the measured
   cost (MEMORY-AND-TIMING.md §1.7) and the Security Target's claims (§7)
   under O.AUDIT and P.ACCOUNTABILITY, with thirteen `H.AUD` requirements
   in `docs/sysml/13-item-requirements.sysml`, each verified by the check
   that proves it whole.
