# Power control: processor power limits from a ring-3 driver

Design version 1, 2026-10-22, session `powerctl`. Replaces `ferrix.vrcurrent`
(`src/kernel/src/arch/x86_64/vrcurrent.rs`, `L.x86_64.190`-`193`), which
put the N150's VR-current policy in the kernel. The customer's brief: move
the policy out "the Fuchsia route" -- typed, narrow kernel calls behind a
capability, the policy in a ring-3 driver -- "with some further improvements
to make our kernel rock solid". The certification consultant reviews this
document before any code (AGENTS.md, *Reviewing before the lock*).

## 1. Why

`ferrix.vrcurrent` works: on the N150 box, 40 A in `MSR_VR_CURRENT_CONFIG`
(0x601) lifted the CPU ratio from 22 to 26 under full GPU load at 80-82 C,
where 36 A and below left it held by "amps/pl4". But every policy decision
-- the steps, their timing, the 25..32 A clamp, which ceiling -- is kernel
code in the core ring, so going from 40 A to 44 A is a kernel change, a
certification review and a full gate. The kernel should hold only what makes
a power-limit write safe whoever asks for it; how high, when and why is
policy, and belongs to a driver that can be changed without touching the
item.

## 2. Prior art

| System | Interface | Gate | Revert on holder death |
|---|---|---|---|
| Fuchsia / Zircon | `zx_system_powerctl(resource, ZX_SYSTEM_POWERCTL_X86_SET_PKG_PL1, arg)`; the kernel converts units (`RAPL_POWER_UNIT`), encodes the time window, range-checks and writes `MSR_PKG_POWER_LIMIT` | a `ZX_RSRC_KIND_SYSTEM` resource at `ZX_RSRC_SYSTEM_POWER_BASE`; policy in a userspace power manager | none |
| Genode / NOVA | guarded MSR access through the PD session's `system_control` | `managing_system`, off by default; policy in an `msr` component | none |
| Redox | raw read/write of any MSR through `kernel.acpi`'s `msr` handle, root only | root | none (its own TODO asks for finer control) |
| Fiasco, QNX | none from userspace | -- | -- |

Ferrix takes Fuchsia's shape -- a typed operation per register, units and
ranges in the kernel, a capability that only the power driver holds -- and
adds what none of them has: the kernel puts firmware's values back when the
holder dies, hangs or loses its claim, and on its own thermal backstop (§6).

## 3. The shape

```
kernel (core ring, x86-64 only)                ring 3
  discovery/power.rs (item): a node,             devmgr: TREE_X86_POWER -> `power`
    TREE_X86_POWER, only under ferrix.powerctl     |
    and only on a processor in the model table     v
  native.rs (item): power_control_create(dev) -> power driver (src/user/system/native/
    one claim, a channel end with no TRANSFER      drivers/power/x86): its plan from
    and no DUPLICATE                               ferrix.power.* boot words, renews
  arch/x86_64/powerctl.rs (core): the power        the lease each second, reads
    task, pinned to the boot processor, the        telemetry, steps the limit, backs
    only code that writes 0x601 or 0x610           off on temperature, logs
```

Nothing in this document is new hardware authority. The register writes are
those `ferrix.vrcurrent` makes today, on the processor it makes them on; what
moves is who decides the value.

## 4. The capability

### 4.1 The node

Stage 10 gains a finder, `discovery/power.rs` (item ring), after the four it
has. It adds one `DeviceNode` when, and only when, both hold:

1. **The word.** `ferrix.powerctl` is on the loader's command line. Without
   it nothing in this design reads, writes, publishes or prints anything
   (the property `L.x86_64.190` gives `ferrix.vrcurrent` today); a certified
   profile does not permit the word (`build-flags.toml`, §11).
2. **The model table** (§5) has a row for the boot processor:
   `GenuineIntel`, family 6, model 0xBE (Alder Lake-N, the N150) is the one
   row. Elsewhere the finder prints one refusal line naming vendor and
   signature, adds nothing, and the boot goes on. On AArch64 and ARMv7-A the
   word is ignored without a line (no MSRs; `arch::power_allowed` is a
   constant `false` there).

The node has binding `TREE_X86_POWER` (a new `DEVICE_TREE_BLOCKS` binding
number in `ferrix_native_abi::types`, after `TREE_FIRMWARE_FB`), no
aperture, no vector, no configuration space and `DmaShape` that reaches no
memory, so `vmo_pin`, `io_mapping_create` and `interrupt_create` on it all
refuse as they do for any node without those resources. Its location is
`Location::Tree(0)`: on x86-64 the only other tree-located node is the
firmware framebuffer's, at its framebuffer's address, so nothing collides;
`reaches_memory` is false for it as for that one, so it has no pin and no
domain. `device_quiesce` on it has no bus mastering to turn off and
succeeds.

Before adding the node the finder asks the power core to **probe**: checked
reads of 0x601, 0x606, 0x610 and 0x1A2 must all succeed, 0x601's and
0x610's lock bits must be clear, and `TjMax` must be believed (80..110 C).
Any failure is one refusal line naming it, and no node.

### 4.2 The claim

`power_control_create(device)` (native call `0x1066`, after
`CHARDEV_DMABUF_FENCE`) answers in `native.rs` (item), through
`native::control_channel`, so the handle and its `MANAGE` right are checked
by the item as for every control channel. Then:

* a node whose binding is not `TREE_X86_POWER`: `ACCESS_DENIED`;
* a claim already held: `ALREADY_BOUND` (one holder at a time);
* the power core latched off (§6.3): `BAD_STATE`;
* otherwise `arch::power::claim()` makes a channel pair, starts or wakes the
  power task with the kernel's end, and the caller gets the other end with
  `READ | WRITE | WAIT` -- **no `TRANSFER`, no `DUPLICATE`**. The claim
  cannot be handed on or copied: it is the process that asked, until its
  end closes. (The device handle itself keeps `DEVICE_RIGHTS` as every
  driver's does; a second process given it could only ask for a claim,
  which the first holds.)

On AArch64 and ARMv7-A the call answers `NOT_SUPPORTED` before looking at
the handle. `devmgr` remains the only grantor of device nodes, as for every
driver (DEVMGR.md §1); a `devmgr` that gave the node to another program
gives it a claim to ask for, not a register.

### 4.3 devmgr

`TREE_DRIVERS` gains `(TREE_X86_POWER, b"power", Kind::Power)`. `Kind::Power`
is started like `Kind::Gadget` -- its device and START over the bootstrap
channel, no PUBLISHED, taken at its word -- and is **not** started again
when it dies: a dead power driver has left firmware's values in force
(§6.1), which is the safe state, and a driver that dies on every start
should not be retried into the registers eight times. `devmgr` writes the
usual DIED line.

## 5. The typed operations and the model table

### 5.1 Operations

The protocol is request and reply on the claim's channel, one request
outstanding, each request 32 bytes and each reply 128, little-endian, laid
out in `ferrix_native_abi::power` (host-tested encode and decode, as
`ferrix_logctl::message` is). No operation takes a register number: each
names one field of one register.

| Op | Request | What the kernel does | Reply |
|---|---|---|---|
| `HELLO` | -- | records nothing; answers what this machine allows | model, the table row's limits, firmware's values of 0x601 and 0x610 (read at claim time), the lease period, the backstop limit |
| `RENEW` | -- | renews the lease (every well-formed request does) | lease left |
| `SET_VR_CURRENT` | amperes in 1/8 A | validates (§5.3), writes 0x601 bits 12:0, reads the whole register back | value written, value read |
| `SET_PACKAGE_LIMITS` | PL1 and PL2 in mW, 0 = leave as is | converts with `RAPL_POWER_UNIT` (0x606) bits 3:0, rounding down, validates, writes 0x610 bits 14:0 and 46:32, reads back | fields written, register read |
| `RESTORE` | which (VR, package, both) | writes firmware's values back now (§6.1); the claim stays | values read back |
| `TELEMETRY` | -- | reads a fixed list, read-only, all on the boot processor | `TjMax` (0x1A2 bits 23:16), `IA32_THERM_STATUS` (0x19C, the boot processor's core), `IA32_PACKAGE_THERM_STATUS` (0x1B1), `PERF_STATUS` (0x198), `CORE_PERF_LIMIT_REASONS` (0x64F), `GRAPHICS_PERF_LIMIT_REASONS` (0x6B0), `PKG_ENERGY_STATUS` (0x611) with `RAPL_POWER_UNIT`, 0x601 and 0x610 as they read now; a read that faults is reported absent, not as 0 |

A reply carries a status: `OK`, `REFUSED` with a reason code (out of range,
locked, rate, latched, absent), or `ENDED` with the reason the claim ended
(§6). An undecodable request, a request carrying handles, or a second
request while one is unanswered is a protocol breach: the kernel restores
(§6.1), sends `ENDED(protocol)` and closes its end.

The kernel decodes the raw readings only as far as its own backstop needs
(temperatures and trip bits, §6.2). Ratios, reasons and energy are the
driver's to decode, from the raw words, in its host-tested library.

### 5.2 Every other bit preserved

Each write is `(current & !FIELD) | value`, where `current` is the register
as last read by the kernel (not firmware's boot value: §6.4 covers firmware
changing it), and `FIELD` is 0x1FFF for 0x601 and `0x7FFF | 0x7FFF << 32`
for 0x610. Enable, clamp, time-window and lock bits are never written to a
value other than the one read. A pure `compose` per register, stage-3
checked with every other bit set.

### 5.3 The model table

A `const` table in `arch/x86_64/powerctl/logic.rs`, one row per
(vendor, family, model). A processor with no row gets no node (§4.1). The
row for 0xBE:

| Field | Value | Why |
|---|---|---|
| VR current, lowest | firmware's own value | thermal back-off returns to firmware, never below it; lowering IccMax is not a use anyone asked for |
| VR current, absolute ceiling | **48 A** (384 eighths) | the customer approved going higher than 40 A (2026-10-22), 48 A being blocked only by tooling; **to be confirmed by the customer in this review** |
| VR current, most above firmware | **23 A** | 25 A firmware + 23 = 48; a firmware already at 48 gets no raise |
| PL1 / PL2, lowest | 4 W / 4 W | below that the package cannot run its own idle |
| PL1 / PL2, highest | **firmware's own value** (25 W / 30 W on the box, msrlog of 0x610 = `0x004280f0002380c8`) | the box is limited by amps, not by PL1/PL2: there is no case for raising them, so the first table allows lowering only; a raise is a later table change with its own review |
| lock bits | 0x601 bit 31, 0x610 bit 63 | a locked register refuses every write to it |
| backstop | 90 C, `TjMax` believed 80..110 C, trip bits 0..5 of 0x19C and 0x1B1 | `ferrix.vrcurrent`'s guard, proven on the box at 40 A |
| packages | 1 | Alder Lake-N is a single-die SoC: the package registers the boot processor writes are the machine's only ones |

Every request is validated against the row **on every call** (not once at
claim): the value against lowest and highest, the register's lock bit as
read now, and the latch (§6.3). Ceilings are kernel constants: no boot
word, driver or message moves them. Changing one is a kernel change.

### 5.4 Rate limit

A **raise** (a value further from firmware's than the value in force) is
refused `REFUSED(rate)` if the previous raise of that register was less
than 1 s ago. A **lowering** toward firmware's value, `RESTORE` and every
kernel restore are never rate-limited: backing off is always immediate.
Requests of any kind past 64 in one second are refused `REFUSED(rate)`
without touching a register, so a driver spinning on `TELEMETRY` costs the
boot processor bounded work.

## 6. What makes it rock solid

### 6.1 Restore: firmware's values come back

At claim time, before any write, the power task reads 0x601 and 0x610 and
stores them as **firmware's values** in lock-free statics (one `AtomicU64`
value and one `AtomicBool` "in force" per register, as `vrcurrent`'s
`FIRMWARE` and `IN_FORCE`). A register is "in force" from just before its
first write until the one restore that writes firmware's value back. The
restore is single-shot per register (`IN_FORCE.swap(false)`), so of every
caller on every processor only the first writes. It writes, reads back,
and prints one line with the reason; a read-back that differs prints a
second line, `NOT confirmed`.

Restore runs on each of:

| Reason | Line ends with | Claim afterwards |
|---|---|---|
| (a) the holder's end closed: it exited, was killed, or closed the handle | `holder gone` | ended; a new claim is allowed |
| (b) the lease lapsed: no well-formed request for 5 s (§6.5) | `lease lapsed` | ended (`ENDED(lease)` sent, then the end closed); a new claim is allowed |
| (c) a protocol breach | `protocol` | ended; a new claim is allowed |
| (d) the holder's `RESTORE` | `asked` | stays |
| (e) the backstop (§6.2) | the reading, as `vrcurrent` says it (`package 90 C`, `THERM_STATUS bit 2 set`, ...) | ended; **latched** (§6.3) |
| (f) a read-back after a write that differs in any bit, or a write refused with `#GP` | `read back 0x...` / `the write took #GP` | ended; **latched** |
| (g) an orderly power-off or reset, and both ends of a panic | `power-off` / `reset` / `panic` | -- |

(g) is `vrcurrent::restore` generalised to two registers: called from
`shutdown()`, `reset()`, and `panic.rs`'s `conclude` and `abridged` on both
the reset and the halt ends, before the last console drain, from any
processor, interrupts off, no lock, no allocation.

(a) needs no polling: the power task waits on its channel end for
`READABLE | PEER_CLOSED` with a deadline (logctl's `wait`), and a process's
handles close when it ends, whatever ended it.

### 6.2 The kernel's thermal backstop

Independent of the driver and of whether it renews: while either register
is in force, the power task reads 0x19C and 0x1B1 every second (the
deadline of its channel wait) and restores at the row's limit -- `vrcurrent`'s
pure verdict, unchanged: core readout invalid, any of bits 0..5 in either
register, or `TjMax` less either readout at or above 90 C. Also before the
first write of a claim: a bad verdict then refuses the write (`REFUSED`,
nothing written), as `vrcurrent` refuses its run.

It stays minimal on purpose: one limit, no hysteresis, no stepping. Backing
off before the limit is the driver's policy (§8); the backstop is what
holds when that policy is wrong, absent or hung.

### 6.3 Latch

A backstop trip (§6.1 e) or a failed write or read-back (f) is evidence
about the hardware, not about the driver. After one, the power core
**latches off until the next boot**: the claim ends, and every later
`power_control_create` answers `BAD_STATE`. The latch is one `AtomicBool`,
printed once: `msr: power latched off until reboot: <reason>`.

### 6.4 Firmware changing a register under us

Firmware may rewrite 0x601 or 0x610 (SMM, the EC through ACPI). Each
second, with the backstop reading, the task reads each register in force and
compares it with the value it last wrote. A difference means firmware has
decided something, and firmware wins: the kernel prints `msr: power
<register> changed under us (wrote X, reads Y); left to firmware`, clears
that register's in-force flag **without writing** (writing the boot value
back could undo a lower value firmware chose), and refuses further writes
of that register until reboot (`REFUSED(latched)` for that register only).

### 6.5 Lease

The claim carries a lease of **5 s**, a kernel constant reported in
`HELLO`. Every well-formed request renews it. The driver renews once a
second (its telemetry poll does). A driver that is alive but hung -- a
deadlock, a loop that never reaches its poll -- lets the lease lapse and
the kernel restores (§6.1 b), which a holder-death revert alone (Fuchsia,
Genode) would never do.

### 6.6 Which processor executes it

The power task is pinned to the boot processor (`sched::spawn_on` with a
one-processor `CpuSet`, as `vrcurrent`). Every read and write of this
design executes there. 0x601, 0x606, 0x610, 0x611, 0x1B1, 0x64F and 0x6B0
are package-scope on Intel's client parts, so one processor writing them
writes the package's; 0x198 and 0x19C are the boot processor's own core,
and the reply says so. The model table admits single-package models only
(§5.3). The panic and power-off restores run on whichever processor gets
there, which is correct for package-scope registers.

### 6.7 The write's critical section

A write is, with interrupts masked on the boot processor (`ferrix_sync`'s
`Irq` guard): check the stop flag, set "in force", `write_msr_checked`, read
back, re-check the stop flag (and restore if a restore ran meanwhile).
Masking closes the window the consultant left open on `vrcurrent`
(ledger 3090, C1): the panic's stop request is a fixed-delivery IPI
(`apic::send_ipi_to_others`), which cannot land between the write and the
re-check, so a raise written while a panic's restore ran is always seen and
undone. An NMI can still land there; nothing in the kernel stops a
processor by NMI today (said in V-16).

### 6.8 What is not covered

* **A stalled scheduler.** If the boot processor stops running the power
  task, nothing restores: no backstop, no lease. The firmware's PROCHOT and
  THERMTRIP remain the hardware's own protection under any value written,
  as for `vrcurrent` (SAFETY-MANUAL hazard sentence kept).
* **Suspend and resume.** Ferrix has none (`reboot(2)`'s suspend commands
  answer `EINVAL`, `syscall/system.rs`). When it gains one, its entry must
  restore and end the claim first, and firmware's values must be read again
  on resume; this document's requirement on that path is owed then.
* **The MMIO copy of the package limits** (MCHBAR 0x59A0 on client parts).
  The effective limit is the lower of the MSR's and MCHBAR's. Writing the
  MSR lower is always effective; this design never raises PL1/PL2 (§5.3),
  so the MCHBAR copy cannot undo a write of ours, only make it moot. Not
  read or written.
* **Other processors' cores' thermal status.** The backstop reads the boot
  processor's core register and the package register; the package's
  readout is the hottest of its cores on Intel client parts.

## 7. Logging

Every write and every restore is one kernel line, whoever asked. The
lines keep `ferrix.vrcurrent`'s shapes for 0x601, so the box's scripts keep
working:

```
msr: power: claimed by power (pid 41); firmware 25.000 A, PL1 25.000 W, PL2 30.000 W, TjMax 105 C
msr: vrcurrent set 30.000 A (was 25.000 A)
msr: vrcurrent reverted to 25.000 A: lease lapsed
msr: pkgpower set PL1 15.000 W PL2 20.000 W (was PL1 25.000 W PL2 30.000 W)
msr: pkgpower reverted to PL1 25.000 W PL2 30.000 W: holder gone
msr: power latched off until reboot: package 90 C
msr: power refused: not an authorised processor (Intel false, signature 0x60fb1)
```

The driver prints its own decisions on standard error (as `devmgr` and
`gc400` do), every 5 s the temperatures in `vrcurrent`'s shape, and each
step or back-off with its reason:

```
power: plan 30,34,37,40,44 A every 60 s, back off at 85 C
msr: vrcurrent 34 A, core 78 C, package 79 C
power: backing off 40 -> 37 A: package 86 C
```

A refused request is a driver line, not a kernel line: a driver spinning
on refusals must not flood the kernel log.

## 8. The driver

`src/lib/drivers/power/x86-power` (host-tested library) and
`src/user/system/native/drivers/power/x86` (the process, image `power`),
on `ferrix-rt`, outside the item.

* **Config** from `/proc/cmdline`, as the Intel driver reads its words:
  * `ferrix.power.vr=A1,...,An@S`: the step plan, one to eight steps, the
    grammar `ferrix.vrcurrent` has. The driver refuses a step above the
    ceiling `HELLO` reports before asking; the kernel refuses it anyway.
  * `ferrix.power.hot=C`: the soft limit, default 85. At or above it the
    driver steps down one step and holds there until the package reads
    5 C below it for 30 s; three back-offs in a row and it `RESTORE`s and
    stays at firmware's value.
  * `ferrix.power.pl=W1,W2`: optional lower PL1 and PL2.
  * `ferrix.power.test=hang@S` / `exit@S` / `spin@S`: test only (§10).
  No `ferrix.power.vr` word: the driver claims, says `HELLO`'s answer and
  telemetry every 5 s, and writes nothing.
* **Loop**: `TELEMETRY` once a second (which renews), the plan's next step
  when due, the soft limit's back-off. Telemetry decoded by the library:
  temperatures, ratio, limit reasons by name (msrlog's words), package
  power from the energy counter.
* Going from 40 A to 44 A is a boot word: `ferrix.power.vr=30,34,37,40,44@60`.

## 9. Retiring `ferrix.vrcurrent`

The branch's last commit removes `vrcurrent.rs`, `vrcurrent/`, its boot
word, its hooks in `panic.rs`, `main.rs` and `arch/mod.rs`, and its
requirements. `L.x86_64.190`, `191` and `193` are deleted (never reused);
`L.x86_64.192`, the checked write and its fixup, stays with its statement
naming the power core as the one caller outside `cpu.rs`. The checked write
is unchanged. `check-device-access.py`'s rule moves to the power core's
files: `write_msr_checked` only in `powerctl.rs` and its boot check, with
the named consts `VR_CURRENT_CONFIG` (0x601) and `PKG_POWER_LIMIT` (0x610),
nothing else writing an MSR there.

`ferrix.msrlog` (read-only) and `ferrix.epp` (HWP EPP, set once at bring-up
per processor, no runtime policy) stay as they are.

## 10. Verification

**Stage 3, every x86-64 boot, constant vectors** (no register the processor
implements is written): the model gate (0xBE Intel passes; 0xBE AMD, 0xBF,
family 15 and the gate host's 0x60fb1 refuse); per op validation (ceiling,
above-firmware, floor, lock bit, latch); `compose` of both registers with
every other bit set; mW to RAPL units at unit exponents 0, 3 and 15,
rounding down; the backstop verdict (`vrcurrent`'s vectors moved); the rate
limiter; the lease arithmetic.

**Stage 3, the engine on a fake register file.** The power task's decisions
are an engine over a `Registers` trait: the real one reads and writes MSRs,
a fake one is an array in the check. Stage 3 drives the engine with a fake
clock through: claim, raise, read back; the lease lapsing (restored, claim
ended, a new claim allowed); holder gone (restored); a backstop trip
(restored, latched, new claim `BAD_STATE`); a read-back differing (restored,
latched); firmware changing the register (not written, refused after);
raise rate refusal and immediate lowering; a panic restore racing a write.

**Negative controls** (`gate.sh control --expect`, x86_64 kvm), each FIRED
(panic) with its own stage-3 sentence: model gate removed; ceiling removed;
compose not masking; lease never lapsing; holder death not restoring;
backstop limit removed; latch not set; firmware change written over; rate
limit removed. Host controls for `check-device-access.py`: a planted
`write_msr_checked` in `msrlog.rs`, and one of 0x1A0 in `powerctl.rs`.

**Boots, `cargo xtask test-power`** (new, x86_64 kvm; host-tested judge):

1. without the word: no `power` line, no node; the transcript identical to
   `main`'s;
2. `ferrix.powerctl` on the gate host's AMD processor: one refusal line, no
   node, `devmgr` starts no `power`;
3. `ferrix.powerctl=fake` (test only), `ferrix.power.vr=27,29@1`,
   `ferrix.power.test=hang@3`: the driver claims, writes 27 then 29 to the
   **fake** register file, stops renewing, and the kernel prints
   `reverted ... lease lapsed`;
4. as 3 with `exit@3`: `reverted ... holder gone`;
5. as 3 with `spin@3` (requests as fast as it can): `REFUSED(rate)` counted
   by the driver, the kernel's line count bounded;
6. TCG with `FERRIX_X86_CPU=qemu64,vendor=GenuineIntel,family=6,model=190`:
   the model gate passes and the probe's read of 0x601 faults, so the
   finder refuses with `VR_CURRENT_CONFIG absent (#GP)` and adds no node.

`ferrix.powerctl=fake` publishes the node on any x86-64 processor with the
fake register file behind it, and nothing in its path reaches `wrmsr`:
the backend is chosen once, when the node is made, and the fake backend has
no MSR access compiled into it. It is `test_only` in `build-flags.toml`, so
no certified profile can carry it.

**Hardware**: the box runs of §12, under the BOX-HOLDER protocol, the
customer's; a BACKLOG row until they are run.

**Other rows**: `check`; test-boot x86_64 kvm (boot lines = `main`'s plus
the stage-3 lines); aarch64 tcg; armv7a tcg `--smp 2`; devmgr's and the
driver library's host tests.

## 11. Requirements and evidence

Reserved before writing (CONVENTIONS.md): `H.DEV.5-6`, `L.x86_64.194-209`,
`L.device.51-53`. Planned, each no broader than one check:

* `H.DEV.5`: a driver changes the processor's power limits only through the
  kernel's typed operations, within the kernel's limits for the model.
* `H.DEV.6`: the kernel puts firmware's limits back when the holder goes,
  hangs, breaks the protocol, the backstop trips, or the machine stops.
* `L.device.51` the finder's node (word, table, probe, nothing otherwise);
  `52` the call's refusals and the claim's rights; `53` spare, released
  unused if not needed (devmgr's table is ring 3 and gets no id).
* `L.x86_64.194` model gate; `195` validation per op; `196` compose of
  0x601; `197` compose and unit conversion of 0x610; `198` telemetry
  read-only list; `199` restore single-shot and its seven reasons; `200`
  lease; `201` backstop; `202` latch; `203` firmware change; `204` rate
  limit; `205` the write's critical section; `206` the protocol breach;
  `207` the fake backend has no MSR path and needs the test word; `208`
  restore from power-off/reset/panic; `209` spare.

`build-flags.toml`: `ferrix.powerctl` (x86-64 Alder Lake-N only, never in a
configuration a claim is made of), `ferrix.powerctl=fake` test only, and the
driver's `ferrix.power.*` words. SAFETY-MANUAL: the hazard sentence moves
from `vrcurrent` to the power core, with the latch and lease. 
VULNERABILITY-ANALYSIS: a new entry (a capability only `devmgr` grants,
non-transferable claim, constants as ceilings, the restore paths, residuals
of §6.8). Coverage carried after the final rebase.

## 12. The box procedure (the customer's)

1. Boot with `ferrix.powerctl ferrix.msrlog ferrix.power.vr=30,34,37,40,44@60`
   (plus the usual words). Expect `msr: power: claimed by power`, then
   `msr: vrcurrent set 30.000 A (was 25.000 A)` and one step a minute.
2. Load the GPU. The driver backs off at 85 C; the kernel restores at 90 C
   and latches. Read `msr: vrcurrent 44 A, core .. C, package .. C` and
   msrlog's ratio and reasons.
3. Before using the box for anything else: power-cycle, or see a
   `reverted` line.

## Where it stands

2026-10-22: design written, awaiting the consultant's design review. No
code.
