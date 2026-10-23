# Power control: the VR current limit from a ring-3 driver

Design version 2, 2026-10-22, session `powerctl`. Replaces
`ferrix.vrcurrent` (`L.x86_64.190`, `191`, `193`, now retired), which put
the N150's VR-current policy in the kernel. The customer's brief: move the
policy out "the Fuchsia route" -- typed, narrow kernel calls behind a
capability, the policy in a ring-3 driver -- "with some further
improvements to make our kernel rock solid"; and then (2026-10-22, through
the coordinator) "keep the kernel slim, we need that for certifications":
the kernel keeps only the typed calls, the per-model bounds and the
restores, and everything else goes to the driver.

Version 1 of this document (commit 0d031151f) had the certification
consultant's design review, OK IF (1)-(16), ledger line 3130. Version 2
is that design cut down to the customer's slim kernel: §13 lists what
changed from version 1 and why, for the consultant's review of the code.

## 1. Why

`ferrix.vrcurrent` works: on the N150 box, 40 A in `MSR_VR_CURRENT_CONFIG`
(0x601) -- written by the unlanded box-test branch `vr36`, whose ceiling
the customer raised for that run, since `main`'s `ferrix.vrcurrent` stops
at 32 A -- lifted the CPU ratio from 22 to 26 under full GPU load at 80-82 C,
where 36 A and below left it held by "amps/pl4". But every decision -- the
steps, their timing, the clamp, the ceiling -- was kernel code in the core
ring, so 40 A to 44 A was a kernel change and a certification review. The
kernel should hold only what makes the write safe whoever asks; how high,
when and why is the driver's.

## 2. Prior art

| System | Interface | Gate | Revert on holder death |
|---|---|---|---|
| Fuchsia / Zircon | `zx_system_powerctl(resource, ZX_SYSTEM_POWERCTL_X86_SET_PKG_PL1, arg)`: units, window and range in the kernel, then `MSR_PKG_POWER_LIMIT` | a `ZX_RSRC_KIND_SYSTEM` resource; policy in a userspace power manager | none |
| Genode / NOVA | guarded MSR access through the PD session's `system_control` | `managing_system`, off by default | none |
| Redox | raw read/write of any MSR through `kernel.acpi`'s `msr` handle | root | none |
| Fiasco, QNX | none from userspace | -- | -- |

Ferrix takes Fuchsia's shape -- one typed system call per operation, the
bounds in the kernel, a capability only the power driver holds -- and adds
what none of them has: firmware's value comes back when the holder dies,
hangs, or the kernel's own backstop trips.

## 3. Authority

This design gives the driver no register `ferrix.vrcurrent` did not
already write, on no processor it did not write it on, and no higher
value -- before this branch's last commit: 0x601 bits 12:0 on Intel family
6 model 0xBE, at most 32 A and 7 A above firmware's, the customer's
authorisation for `ferrix.vrcurrent`. The last commit raises the ceiling
to 48 A and 23 A above firmware's, which (a) below needs.
Two things the consultant found it does add (ledger 3130, condition 1),
which need the customer's own words, with their date, here and in the take
message before it lands:

* **(c) a standing interface** rather than "one N150 run": the word
  `ferrix.powerctl` and its driver stay in the tree;
* **(a) a higher ceiling**, for the 44 A step of §12: 48 A absolute and
  23 A above firmware's is one constant in the model table
  (`logic::TABLE`), the branch's last commit alone.

**The customer's words, 2026-10-23**, asked "Power limits (consultant OK
IF): accept about +500 kernel lines? also retire msrlog (−321 lines)? your
dated words for a standing interface and for 48 A.", answered verbatim:
"power limits are ok". They are the authority for both (c) the standing
interface and (a) the 48 A / 23 A row, so the last commit lands
(re-review, ledger 3180, condition 1). They say nothing about `ferrix.msrlog`,
which stays (§11).

Version 1's package power limits (0x610, condition 1 (b)) are not in
version 2: the box is held by amps, not by PL1/PL2, and the slim kernel
writes one register.

## 4. The shape

```
kernel                                            ring 3
  discovery/powerctl.rs (item): stage 10's        devmgr: TREE_X86_POWER -> `power`
    finder; one node under ferrix.powerctl          (Kind::Power, never restarted)
    on a processor the model table names              |
  syscall/native.rs (item): power_control_create,     v
    power_set, power_read                          power driver (drivers/power/x86,
  arch/x86_64/powerctl.rs (core): the bounds,        policy in lib ferrix-x86-power):
    the write, the restores, one task a second       plan from ferrix.power.* words,
    for the death watch, the lease and the           a power_read a second (its lease),
    backstop                                         back-off, every log line
```

## 5. The node and the claim

**The node** (`L.device.51`). Stage 10's fifth finder,
`discovery/powerctl.rs` (item ring), asks `arch::power_probe`. Without
`ferrix.powerctl` or `ferrix.powerctl.fake` nothing is read, published or
printed. With `ferrix.powerctl` the probe publishes one node,
`TREE_X86_POWER` (binding 7), only when all hold, and otherwise prints
`power: refused: <why> (Intel .., signature .., 0x601 ..); no node` and
publishes nothing -- never a failed finder, so the boot goes on:

1. the model table (`logic::TABLE`) has a row for `GenuineIntel` and
   `CPUID.1:EAX`'s family and model: family 6 model 0xBE alone;
2. the processors span exactly one package (their x2APIC ids shifted by
   CPUID leaf 0x1F's, else 0xB's, package shift);
3. 0x601 reads without `#GP`, its lock bit 31 clear and its bits 12:0 not
   0 (QEMU's TCG reads it as 0, so a TCG processor answering as the N150 is
   refused here);
4. `TjMax` (0x1A2 bits 23:16) reads 80 to 110 C.

Then the probe stores firmware's 0x601 for the boot -- once, never again
(condition 6) -- and prints `power: node: signature .., 0x601 .., ceiling
..`. `ferrix.powerctl.fake` publishes the same node on any x86-64 processor
over `powerctl/fake_check.rs`'s register file (§10).

The node is `Location::Tree(0)` (no other x86-64 node is at a tree address
but the firmware framebuffer's, at its framebuffer). The places that decide
on a tree node, and what each does for this one (condition 4):

| Site | For the power node |
|---|---|
| `DeviceNode::reaches_memory` | false, by name, as for the firmware framebuffer |
| `DeviceNode::domain` | refused (it reaches no memory) |
| `vmo_pin` | `WRONG_TYPE` (it reaches no memory) |
| `io_mapping_create` | `ACCESS_DENIED` (no aperture) |
| `interrupt_create` | `INVALID_ARGS` (no vector) |
| `device_quiesce` | `BAD_STATE` while a live holder has the claim, else as any tree node (no bus mastering to turn off) |
| `iommu.rs` stream lookup, `DeviceNode::interrupts_isolated` | as any tree node: none, false |
| `describe` / `tree_binding` | `DEVICE_TREE_BLOCKS`, device id 7, no windows |
| the load ring's cores (`block_ring`, `display`, `input`, sysfs) | not reached: no core accepts binding 7, and sysfs names it `tree@0x0` |

`cargo xtask test-power` has the driver try the pin, mapping, interrupt and
quiesce on its node and say what each answered (`L.device.53`).

**The claim** (`L.device.52`). `power_control_create(device)` (native call
0x1066) answers in `native.rs` (`power_call`), which checks the handle and
its `MANAGE` with `device_in`, the check `native::control_channel` makes
for every control channel:
`ACCESS_DENIED` for any other node, `ALREADY_BOUND` while a claim stands,
`BAD_STATE` once latched, `NO_MEMORY` when the channel or the task cannot be
made (both fallible, nothing kept). On AArch64 and ARMv7-A no node has
binding 7, so the claim answers `ACCESS_DENIED`, and `power_set` and
`power_read` answer `WRONG_TYPE`.
The claim is one end of a channel whose other end the kernel keeps; the
holder's end comes back with `WAIT` alone, so it cannot be sent, copied,
read or written. Its one use is as the first argument of the two calls, and
to see `PEER_CLOSED` when the kernel ends the claim.

## 6. The calls

| Call | Arguments | The kernel |
|---|---|---|
| `power_set` (0x1067) | claim, eighths of an ampere | refuses `INVALID_ARGS` outside firmware's own value up to the row's ceiling (`L.x86_64.195`); `SHOULD_WAIT` for a raise within a second of the last, whichever claim made it (`203`); `LIMIT_REACHED` for a raise while the backstop's verdict is bad (`200`); `BAD_STATE` latched, the register left to firmware, or not the claim. A value already in force writes and prints nothing. Otherwise writes 0x601 bits 12:0 over the register as it reads now (`196`, `202`), reads it back (`205`), prints `power: set 0x601 <new> (was <old>)`, answers the read-back. A lowering, firmware's value included, is how the driver takes its raise back. |
| `power_read` (0x1068) | claim, `*[u64; 16]` | writes `POWER_READ_*` (`ferrix_native_abi::types`): firmware's 0x601, the ceiling, the signature, the latch flags, 0x601 now, and the fixed read-only list 0x1A2, 0x19C, 0x1B1, 0x198, 0x64F, 0x6B0, 0x611, 0x606, 0x610, each marked present only if its read did not fault (`197`); renews the lease. Decoding is the driver's. |

Both renew the lease. There is no other call: no register number crosses
the interface, and nothing else in it writes.

## 7. What makes it rock solid, and nothing more

**Restores** (`L.x86_64.198`-`206`). Firmware's value comes back:

| When | Seen by | Line | Latch |
|---|---|---|---|
| the holder's end closes: it exited, was killed, or closed the handle | the task, within a second | `power: restored ..: Gone` | no |
| no call for 5 s: a hung driver (`199`) | the task, within a second | `..: Lease` | no |
| a raise in force and a thermal status read faulting, the core's readout invalid, any of bits 0..5 set in 0x19C or 0x1B1, or 90 C on either (`200`) | the task, within a second | `..: Backstop(..)` / `Blind` | yes |
| a write that faults, reads back otherwise, or meets a stop (`205`): the value read back is taken as the last written, so the restore replaces it with firmware's field rather than leave it to firmware | the call | `..: Verify` | yes |
| a restore not confirmed by its read-back, or the eighth claim ended in a boot (`201`) | -- | `power: latched off until reboot: ..` | yes |
| an orderly power-off or reset, and both ends of a panic (`206`) | `restore(what)`, any processor, no lock, no allocation | `power: restored ..: power-off` | -- |

A latched core refuses every later claim until reboot (`201`). The
backstop is `ferrix.vrcurrent`'s guard, unchanged, and minimal: one limit,
no hysteresis; backing off before it is the driver's.

**Firmware's value against the register** (`202`, condition 6). Firmware's
0x601 is read once, at the probe. A claim, a write and a restore each read
the register first and compare it with the last value the core wrote
(firmware's, before any write); one that differs -- firmware changed it --
is written no more until reboot, and a restore leaves it as firmware has
it. A restore writes `(now & !0x1FFF) | firmware's bits 12:0`, never the
whole stored word.

**The write and a panic's restore** (`204`, condition 8). The write runs
with interrupts masked on its processor. A power-off, reset or panic
restore first sets a stop flag, then takes the register out of force, then
compares and writes. A write checks the stop flag before it reads and
again after its read-back, and when it finds it set writes firmware's field
back itself, whichever restore took the register; nothing prints, locks or
allocates inside it. A task restore (holder gone, lease, backstop, a write
that did not read back) runs under the core's lock, as every write does, so
the two never interleave; and it runs masked too (`L.x86_64.208`), so a
panic's stop cannot halt it between taking the register out of force and
writing firmware's field. An exit restore, from another processor, can
meet either: stage 3 runs every interleaving of a write's 13 accesses
with an exit restore's 6 steps, from firmware's value and from a raise in
force, and every interleaving of a task restore's 6 steps with an exit
restore's 6, on a fake whose stop halts a processor that is not masked:
none leaves a raised value in force.

**The claim's own end** (`L.x86_64.198`). The kernel's end of a claim is
kept in the core's state with the claim's generation; ending the claim
hands it to the caller, which drops it after letting the lock go, and each
watch task looks only at the claim of its generation, so a watch whose
claim ended never touches the next one. (Version 1 asked every restore to set the
stop flag; a task restore setting it would stop every later write for the
boot, so only exit restores do, and the lock carries the rest.) An NMI or
an SMI inside the masked write or task restore is not covered, and since
a panic's stop request is not acknowledged (`smp::stop_others`), whether
a masked write or task restore on another processor finishes before a
reset or power-off rests on that exit path's drain lasting longer than a
few MSR accesses (V-16; ledger 3180, condition 6).

**Which processor.** The task runs pinned to the boot processor, and reads
the backstop's registers there; `power_set` and `power_read` run on
whichever processor the driver calls from, and a raise's backstop
verdict reads 0x19C there, the task's on the boot processor. 0x601, 0x1B1, 0x606, 0x610,
0x611, 0x64F and 0x6B0 are package-scope on Intel's client parts, and the
probe admits one package, so any processor's write is the machine's;
0x198 and 0x19C are the reading processor's own core.

**What is not covered** (§6.8 of version 1, kept): a scheduler that stops
running the task stops the death watch, the lease and the backstop, and
firmware's PROCHOT and THERMTRIP are then the hardware's protection;
suspend does not exist (`reboot(2)`'s suspend commands answer `EINVAL`),
and whatever adds it must end the claim with firmware's value back first
and read firmware's value again on resume (BACKLOG); an unorderly reset (a
watchdog, a triple fault, the reset button) writes nothing back, and if
0x601 survives a warm reset the next probe takes the raised value as
firmware's, so the N150 run must try one (condition 14, BACKLOG); the
read-back proves the register, not the limit PCODE enforces (C3); a PCODE
rewrite of 0x601 on a deep C-state exit is taken as firmware's change (C4).

## 8. Logging

The kernel prints only what it does, in raw hex, and the driver decodes:

```
power: node: signature 0xb06e0, 0x601 0xc8, ceiling 0x100
power: refused: not an authorised processor (Intel false, signature 0x60fb1, 0x601 None); no node
power: set 0x601 0xd8 (was 0xc8)
power: restored Done { value: c8, taken: true, back: Some(c8) }: Lease
power: latched off until reboot: Backstop(Hot(90))
power: restored Done { value: c8, taken: true, back: Some(c8) }: power-off
```

Under `ferrix.powerctl.fake` every kernel line says `power (fake):`. The
driver prints `ferrix.vrcurrent`'s shapes, so the box's scripts keep
working. It writes them to `/dev/kmsg`, a record a line at info, which the
kernel writes without ever waiting for the console's room
(`console::write_record`); standard error only if `/dev/kmsg` will not
open:

```
power: claimed: firmware 25.000 A, ceiling 32.000 A, back off at 85 C
msr: vrcurrent set 27.000 A (was 25.000 A)
msr: vrcurrent 27.000 A, core 78 C, package 79 C, ratio 26, package 13512 mW, limited by amps/pl4
power: backing off to 37 A: 86 C
```

## 9. The driver

`src/lib/drivers/power/x86-power` (crate `ferrix-x86-power`, host-tested:
15 tests) and `src/user/system/native/drivers/power/x86` (image `power`),
outside the item. It reads its words from `/proc/cmdline`:

* `ferrix.power.vr=A1,...,An@S`: the plan, one to eight ascending steps of
  whole amperes; steps above the kernel's ceiling are dropped and said.
  Without it the driver writes nothing and only reports.
* `ferrix.power.hot=C`: the soft limit, 50 to 99, 85 without it. At or
  above it on either sensor the driver steps down one step, at most once in
  5 s, and holds until the hottest reads 5 C below it for 30 s; a third
  back-off without such a spell, or one asked of the first step, puts
  firmware's value back for good.
* `ferrix.power.test=hang@S` / `exit@S`: test only (§10).

Its loop: a `power_read` once a second -- the only thing that renews its
lease, so a policy loop that stops is a lapsed lease (C2) -- the policy's
action, and a report line every 5 s. 40 A to 44 A is a boot word, inside
the kernel's ceiling.

**A lapse it lives through** (2026-10-23, after the N150's lapse of that
day: the driver went 13 s without running under an Endfield relaunch, the
kernel put 25 A back, and the driver stopped at the claim's end, so the
GT sat at 450 MHz until reboot). All in ring 3; the kernel is unchanged:

* It asks for nice -20 at the start (`power: running at nice -20`), the
  fair class's heaviest weight; there is no real-time class to ask for.
* It says a second that came more than 1.5 s late: `power: the loop ran
  N ms late (the kernel's lease is 5000 ms)`.
* When `power_read` finds the claim ended, it says so (`power: the kernel
  ended the claim (..)`, `power: lost the claim: firmware's limit is back;
  claiming again (n of 4)`), claims again, and says `power: regained the
  claim: firmware .. A, now .. A; the plan again from its first step`. The
  plan climbs again a step a period from its first step; a back-off's cap,
  its count and a give-up are kept. At most four times a boot, so at most
  five claims end, below the eight that latch the core (`201`). A latch --
  the backstop, a write that did not read back -- refuses the claim, and
  the driver stops as before (`power: the kernel refused the claim again`).

The kernel's guarantees do not move: a lapse is still firmware's value
back within a second of the lease, every later raise is bounded, rate-limited
and backstopped as the first was, and a hung driver -- one that never runs
again -- keeps firmware's value.

## 10. Verification

**Stage 3, every x86-64 boot** (`powerctl/check.rs`, over
`powerctl/fake_check.rs`, nothing reaching an MSR): the gate, the package
count, the bounds and the composition (`194`-`196`); `power_read`'s words
(`197`); a claim's life and the holder gone (`198`); the lease (`199`); the
backstop's verdict on `ferrix.vrcurrent`'s vectors and its restores
(`200`); the latches (`201`); the firmware compares (`202`); the raise
floor across claims (`203`); every interleaving of a write with an exit
restore (`204`); a write that does not read back, restored (`205`); the
exit restore (`206`); every interleaving of a task restore with an exit
restore (`208`); a stale watch and the next claim (`198`).

**`cargo xtask test-power`** (x86_64; `--init` with the static busybox,
whose shell keeps the machine up past `FERRIX-BOOT-OK`): (1) no word: no
power line, no driver; (2) `ferrix.powerctl` on the gate host's AMD: one
refusal, no node; (3) the fake with `ferrix.power.vr=27,29@1` and
`hang@3`: the driver's refusal checks, 27 then 29 A on the fake, the lease
restore, every kernel line `(fake)`; (4) the same with `exit@3`: the
holder-gone restore, devmgr's `ended with status 4`, one start only; (5)
TCG answering as Intel 6/0xBE: the probe refuses 0x601's 0; (6) the fake
with `ferrix.power.vr=27,29@1`, the driver stopped from the shell with
`kill -STOP` for 8 s after its second write -- starved of every processor
-- and let go: the lease restore, the driver's late loop, lost and regained
claim, 27 then 29 A again, no latch, one start.

Its judge has host tests. It verifies `H.DEV.5`, `H.DEV.6`,
`L.device.51`-`53` and `L.x86_64.207`.

**`check-device-access.py`**: outside `cpu.rs`, `write_msr_checked` only
in `powerctl.rs` with `VR_CURRENT_CONFIG` (defined once, 0x601) and in
`cpu/check.rs`'s boot check with `ABSENT`; in `cpu.rs` nothing but its
definition (closes 3090's C2); `powerctl.rs` writes an MSR no other way;
nothing under `powerctl/` reaches an MSR at all.

## 11. The kernel's size

Lines of code, comments and blank lines left out, against `main` at
48d0efdc5:

| | Product | Checks |
|---|---|---|
| removed: `vrcurrent.rs`, `vrcurrent/logic.rs` / `vrcurrent/check.rs` | 439 | 183 |
| added: `powerctl.rs`, `powerctl/logic.rs`, `discovery/powerctl.rs` | 842 | -- |
| added: `powerctl/check.rs`, `powerctl/fake_check.rs` | -- | 610 |
| hooks in `native.rs`, `arch/`, `device.rs`, `main.rs`, `panic.rs` (net) | about 100 | -- |
| the checked write's boot check, moved to `cpu/check.rs` | -- | moved, not added |

So the kernel's product code grows by about 500 lines (after the code review's B1-B3, which added the claim's own end and generation and the masked task restore). What moved out to
ring 3 is all of `ferrix.vrcurrent`'s policy -- the plan's grammar, the
schedule, its packing into a task argument, the guard's logging -- about
150 lines; what the kernel gains is the capability's obligations, which
`ferrix.vrcurrent` never had: a claim, a lease, a death watch, two system
calls, a node, a write and a restore safe against each other on any
processor, and the fake that lets QEMU prove the death and the lease end
to end. Retiring `ferrix.msrlog` too -- its decoded log is the driver's
job now, from `power_read` -- would take 321 product and 144 check lines
out, and is offered, not done: the customer asked to keep it.

## 12. The box procedure (the customer's)

1. Build from this branch. Boot with `ferrix.powerctl ferrix.msrlog
   ferrix.power.vr=30,34,37,40,44@60` (the 48 A row, the customer's words
   of 2026-10-23 in §3). Expect `power: node:`, the
   driver's `power: claimed`, then `msr: vrcurrent set 30.000 A (was
   25.000 A)` with the kernel's `power: set 0x601 0xf0 (was 0xc8)`, and one
   step a minute.
2. Load the GPU. The driver backs off at 85 C (`ferrix.power.hot` to move
   it); the kernel restores at 90 C and latches. Read the driver's 5 s lines
   and msrlog's.
3. The warm-reset check (condition 14): with a raise in force, reset the
   box with its button (or `ferrix.panic=reset` and a forced panic), and
   record the next boot's `power: node: ... 0x601 0x..`: 0xc8 is firmware's
   value back; anything else is a finding for review.
4. Before using the box for anything else: power-cycle, or see a `power:
   restored` line.

## 13. From version 1 to version 2

| Version 1 (ledger 3130) | Version 2 | Why |
|---|---|---|
| a channel protocol: 32-byte requests, 128-byte replies, a kernel codec, breach rules, 64 requests a second | two system calls on a claim handle | slim kernel; no codec, no protocol state, nothing to breach; the request rate is the caller's own CPU |
| `SET_PACKAGE_LIMITS` (0x610) | not written; 0x610 readable in `power_read` | condition 1 (b) and the slim kernel |
| `RESTORE` | a `power_set` of firmware's value | one call fewer |
| `HELLO` | `power_read`'s words | one call fewer |
| a periodic firmware-change watch | the compare before every write and restore | the compares are what keep firmware's change; an earlier sighting changed nothing |
| a quiesce that waits for an ending claim | `BAD_STATE` under a live holder, else at once | the restore never depends on the quiesce |
| a power task serving the channel | a task that looks once a second | the death watch, the lease and the backstop in one look |
| decoded kernel lines in `ferrix.vrcurrent`'s shapes | raw hex from the kernel, the shapes from the driver | logging formats are ring 3's |
| every restore sets the stop flag | exit restores only; task restores serialized by the lock | §7 |

Conditions (2), (3), (4), (5), (6), (7), (9), (10), (11), (12), (13) and
(15) are met as written or as §7 argues; (1) and (14) wait on the customer
and the box; (16) is this review.

## Where it stands

2026-10-22: version 1's design review, OK IF (1)-(16), ledger line 3130.
2026-10-23: the code review of version 2, NOT YET (B1-B3), ledger line
3174; its rulings accept every departure of §13. B1 (a write read back
otherwise left raised), B2 (a task restore not masked) and B3 (a stale
watch could drop the next claim's end) are fixed as §7 says, each with a
stage-3 check and a control. The re-review is OK IF (1)-(7), ledger line
3180: the customer's dated words for the standing interface (and, to keep
the last commit, for 48 A / 23 A); the reservation landing first and the
rebase, keeping `Kind::Engine` and `Kind::Power` both in devmgr's
`launch_again` list and `Power` out of `restarted()`; the 19 controls and
the rows through `fleet/gate.sh` on the landing hash; coverage carried
after the final rebase, vrcurrent's dropped anchors listed; this
section's residual in V-16 (done). The ids were reserved on
branch `powerctl-reserve`, landed on `main` as bb243e114 first.
2026-10-23, meeting ledger 3180's conditions: (1) the customer's words
"power limits are ok" (2026-10-23) are in §3 and
`docs/backlog/DECISIONS.md`, for the standing interface and the 48 A /
23 A row, which stays; `ferrix.msrlog` is not retired. (2) The branch is
rebased onto bb243e114; devmgr's `launch_again` list holds `Kind::Engine`
and `Kind::Power`, and `restarted()` has no `Power`. (3)/(4) the 19
controls and the rows ran through `fleet/gate.sh` on the landing hash
(the tags and verdicts are in the take message,
`fleet/landing-messages/powerctl.txt`). (5) carry-coverage.py: every anchor
carried, 0 dropped (vrcurrent's files held none); the justification is
current. (6) V-16 and §7 name the unacknowledged stop. Still open: the
N150 run (with the warm reset, 3130 (14)) and suspend, as the flakes.md
rows they are.
2026-10-23, the N150's lapse (image main-bca894271, an Endfield relaunch
at load about 12): the driver's last call was at about 1442.3 s, the
kernel's lease restore at 1447.9 s, and the driver ran again at 1456.2 s,
found the claim ended and stopped, so 25 A stayed until reboot. It was not
waiting on its own console line (none was due in the gap, and none was
written late) nor on a sensor read (`power_read` reads the claim's MSRs
and returns). Nor is the fair class the cause: in QEMU, twelve spinning
loops on four processors, with memory churn, `/proc` walkers and a
console flood, made no second late, at nice 0 or -20. On the box, from
the game's exit (1420 s) the driver's seconds came off their grid (1.3 s
late at 1422 s), yt6801 found its interrupts missing at 1431 s and fell
back to polling, and then the 13 s gap came while the kernel's own task and
yt6801 ran on time: the signs of one processor held without interrupts
or preemption, with the driver queued on it. That is a kernel lead, filed
in flakes.md. The driver now lives through it (§9): nice -20, lines to
`/dev/kmsg`, the late second said, and the claim taken again; `test-power`
row 6 shows it. This change does not fix the kernel lead. The
consultant's review: OK IF (1)-(3), ledger line 3451 -- `test-power` and
`check` PASSED through `fleet/gate.sh`, the lead kept a lead (a finding
number if it is confirmed), and the verdict recorded here.
