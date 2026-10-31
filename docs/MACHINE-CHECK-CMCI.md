# Corrected machine check interrupts (CMCI)

Design v2, 2026-10-31 (v1 18b1e578d; consultant OK IF (1)-(10), ledger
5348, carried here). The customer's item 4 of the machine-check follow-ups
(2026-10-30). It builds on `docs/MACHINE-CHECK.md` (H.FAIL.8, the poll of
§3.2) and replaces the sentence there that "CMCI is not in this version". The
customer chose how it is shown (2026-10-31): a patched QEMU under TCG that
emulates `MCG_CMCI_P`, `MCi_CTL2` and the local APIC's CMCI entry, since no
emulator or host here offers CMCI (QEMU 10.2.1's `MCE_CAP_DEF` is `MCG_CTL_P
| MCG_SER_P`; nazuna's KVM answers `KVM_X86_GET_MCE_CAP_SUPPORTED` with
0x1000100, no `CMCI_P`).

## 1. What is there today

One kernel task per processor, pinned there, polls every bank every
`ferrix.mce.poll` seconds (1 to 3600, 5 by default) and prints, counts and
clears each corrected and no-action record (`arch/x86_64/mce.rs` `run`,
`poll`; L.x86_64.223). A corrected error is therefore seen up to a period
late, and a UCNA's frame (Intel's no-action record with an address) is
retired up to a period late: that long, a program may load the poisoned
line and take a machine check that a retirement would have spared it.

## 2. What it does

On an Intel processor that reports `MCG_CMCI_P`, each processor claims the
banks it can be interrupted for and points its local APIC's CMCI entry at a
kernel vector. A corrected or no-action record in a claimed bank interrupts
that processor; the interrupt wakes the processor's own poll task, which
runs its round at once -- the same round, reading every bank, printing,
counting, clearing and retiring as today. The period stays, unchanged, for
the banks nobody could claim and as the backstop for an interrupt that never
comes. A storm of interrupts turns the entry off for a while and the poll
goes on alone.

Intel only: AMD has no CMCI (its threshold interrupt is another mechanism,
not in this item). AArch64 and ARMv7-A as today. CMCI changes when the poll
runs, not which machine checks the kernel survives or what it retires, and
claims no latency bound (ledger 5348, question 4).

## 3. The mechanism (`arch/x86_64/mce/cmci.rs`, `arch/x86_64/apic.rs`)

### 3.1 Claiming banks

Done by the poll task itself, before its first round: it runs on its own
processor (pinned), in task context, after that processor's local APIC is
up and after the vector's handler is registered (§3.2). Only where the
processor takes Intel's rules (L.x86_64.233), `MCG_CAP` has `CMCI_P` (bit
10), the local APIC's version register names at least seven LVT entries
(max LVT >= 6, as Linux's `intel_init_cmci` requires), `ferrix.mce.cmci` is
not `off` and the handler is registered.

Under one global lock (`CLAIM`, a spin lock holding preemption off with
interrupts open, held for at most `banks` triples of MSR accesses, taken by
no interrupt handler), for each bank:

1. read `MCi_CTL2` (0x280 + i) through the checked read; refused -> the bank
   is not claimed and the refusal is counted;
2. `CMCI_EN` (bit 30) already set -> another processor sharing the bank
   claimed it first, or firmware did: not claimed (Linux's `cmci_discover`);
3. write it back with `CMCI_EN` set and the threshold (bits 14:0) set to 1,
   the other bits as read, through the checked write; refused -> as 1;
4. read it again: `CMCI_EN` set -> claimed, its bit set in this processor's
   `OWNED` mask; clear -> the bank cannot interrupt, not claimed; refused ->
   as 1.

The lock makes steps 1-4 one step for processors sharing a bank (Intel
shares some banks between a core's threads or a package), so exactly one of
them claims it. A threshold of 1 interrupts on every record, Linux's
default; §3.4 bounds the rate. Nothing is ever unclaimed: a bank stays its
claimer's until reset. A processor whose bring-up record was refused
(`REFUSED`) has no record and claims nothing.

### 3.2 The vector and the entry

`CMCI_VECTOR` = 0xFB: below interrupt remapping's check vector (0xFC,
`iommu::CHECK_VECTOR`, which v1 had wrongly taken as free -- the first
local run found the interrupt counted as remapping's), the IPI's 0xFD, the
timer's and the spurious; far above the message range `msi.rs` hands out
(0x40 to 0x7F), whose comment now names 0xFB to 0xFF as the kernel's own.
`start` registers its handler with `irq::register` once, on the boot
processor, before it spawns any poll task; a refusal says a line and no
processor claims a bank.

After its claim, a processor with a non-empty `OWNED` writes its LVT CMCI
entry (offset 0x2F0, x2APIC MSR 0x82F): the vector, fixed delivery,
unmasked (`apic::unmask_cmci`). One with nothing claimed leaves the entry
masked, as reset left it.

### 3.3 The interrupt

`on_cmci` runs on the processor whose bank logged, with interrupts masked,
before the arch path's end-of-interrupt. It reads no bank and takes no lock
but the wait queue's:

1. mask this processor's LVT CMCI entry (`apic::mask_cmci`), so no second
   interrupt comes before the round has read the banks (one that arrives
   between the delivery and the mask adds one count, bounded; ledger 5348
   C3);
2. count it (`interrupts` in that processor's CMCI line, §3.5);
3. set this processor's `HELD` flag, then read its park state
   (`sched::park_state`, `docs/CORE-QUARANTINE.md` §3.2): `ACTIVE_STATE`
   and the flag still set (taken back by a swap) -> set `KICK[index]` and
   `WAKE[index].wake_all()`. Any other state -- Parking, Parked, Cancelled,
   Returning -- leaves the flag set and wakes nobody: a parked processor's
   poll task is pinned there and stays parked, its records waiting in its
   banks (CORE-QUARANTINE §3.2 step 7, C3), and a wake there would leave it
   for the parked processor's drain (`rehome_blocked`).

**The return.** `sched` calls the architecture's `returned_to_service`
each time a processor is back in service: at the end of `park` after it
stores `ACTIVE_STATE`, and in the Cancelled undo after the waiter stores
it. On x86-64 that is `cmci::returned`; AArch64 and ARMv7-A hold nothing
and theirs is empty (a registered hook, the first build, was dead code
there, and an architecture's difference belongs behind the facade). It
swaps the processor's `HELD`
flag and, if it was set, kicks and wakes the poll task, whose round reads
the banks and unmasks the entry. **Why no wake is lost**: the handler sets
`HELD` before it reads the state, and both the handler and the return take it
back by a swap (`SeqCst`), so whichever comes second of the handler's read
and the return's store sees the other's: the handler reads `ACTIVE_STATE`
and wakes, or the return finds `HELD` and wakes; never both, since only one
swap finds it set. **Why a wake as the processor begins to park is
harmless**: the handler read `ACTIVE_STATE` just before the waiter's
`ACTIVE -> PARKING`; the wake then is the period timer firing then, as
today -- the task is woken at home, and evacuation or the re-home at every
wake handles it as it handles that timer.

### 3.4 The poll task

`run` becomes: claim and arm (§3.1, §3.2), one round at once (as today's
`run` polls first, so a record logged before the entry was armed waits no
period), then loop

1. wait on `WAKE[index]` until `KICK[index]` is set or the wait passes
   (`WaitQueue::wait_trusting_until`, `KICK` taken with a swap): the poll's
   period, or one second in a storm;
2. `poll(index)`, unchanged but for answering whether it found a record:
   every bank, claimed or not;
3. the storm rule (below); then, where `OWNED` is not empty and no storm,
   unmask the entry and look once more (the masked window, below) -- on a
   processor in service as the handler decides it: the round holds the
   unmask (`HELD`) and takes the hold back only while the park state is
   active (`take_back_hold`), so a round that ends as its processor parks
   leaves it to the return, whose kick runs a round that makes it (ledger
   5418 (1); the first build tested the active bit instead, and such an
   unmask was lost until the period).

**Storm rule**: every round `KICK` drove -- from the handler or from the
look after the unmask -- counts. More than 15 such rounds within one second
(Linux's `CMCI_STORM_THRESHOLD`) leave the entry masked and print once
`mce: cpu <n> cmci storm: polling every 1 s`; the task then polls each
second, and the first one-second round that finds no record ends the storm,
unmasks the entry and prints `mce: cpu <n> cmci storm over`. So a storm costs
at most 16 interrupts and 16 rounds in its first second and one round a
second after. A bank holds one record: records logged before the round reads
it overwrite it with `OVER` set, as without CMCI, so a storm prints fewer
lines than it had records.

**The masked window.** A record logged while the entry is masked raises no
interrupt: a masked LVT entry drops it (SDM vol. 3A 11.5.1), and the
patched QEMU does the same. So after the unmask the task reads `MCi_STATUS`
of each claimed bank once more, and kicks itself (`KICK`) only on a status
the round would take -- corrected, no-action or deferred by `proc.class`
(`poll_takes`); a record the round leaves, an uncorrected one whose machine
check has not come, is the machine check's, and kicking on it would spin the
task (ledger 5348 (ii)). So a record that landed between the round's read
of its bank and the unmask waits one more round, not a period.

**The held window** (check builds only, `cfg(ferrix_checks = "on")`):
`ferrix.mce.cmci-window=<1..60>` makes the first round a CMCI drove sleep
that many seconds before its unmask, saying `mce: cpu <n> cmci window held
<s> s`. test-mce-cmci's window mode injects into it, so that only the look
after the unmask can find the record before the hour's period. A product
build has no option and no sleep (`read_window`, `hold_window` are empty
`const fn`s there).

### 3.5 What is said and counted

* Per processor, at its claim, where anything was claimed: `mce: cpu <n>
  cmci on banks <mask hex>`, and `mce: cpu <n> refused <k> MCi_CTL2
  accesses` where any was. Once, on the boot processor, after the poll
  tasks are started: `mce: cmci offered` or `mce: cmci not offered` (its own
  `CMCI_P`, max LVT, option and registration), and `mce:
  ferrix.mce.cmci=off: ...` where the option said so.
* `/proc/ferrix-mce` gains, after each processor's `cpu<n> ...` line, a line
  `cmci<n> interrupts <n> held <n> storms <n> owned <mask hex>` (`held`:
  interrupts whose wake was held for the processor's return): a line of its own,
  so the existing judges that read the last `cpu<n>` line or its end
  (`test-mce-broadcast`, `test-mce-copy`) are unchanged. lode reads `mce:`
  lines from `/dev/kmsg`, not this file.

### 3.6 `ferrix.mce.cmci`

`off` claims nothing and leaves every entry masked: the poll alone, as
today. For an operator whose firmware takes CMCI itself in a way the claim
does not see, and for the gate's off mode (§5). Any other value is not
understood, said, and **CMCI stays on**: the safe side, since with CMCI on
the period's poll still runs unchanged and an unknown value cannot turn the
log off (ledger 5348 C4).

## 4. What it does not do

* **No firmware-first coordination.** A bank whose `CMCI_EN` firmware set is
  left to firmware (step 2) and still polled; nothing reads ACPI's HEST or
  `_OSC` for corrected-error ownership. A firmware that takes CMCI by SMI
  and clears the bank before the poll sees it hides those records from the
  kernel, as today.
* **No threshold above 1** and no `MCG_TES_P`/yellow handling.
* **No unclaim** at shutdown, kexec or a processor's park. A bank's records
  while its claimer is parked wait for its return (C3), as the poll's do
  today; the other processors sharing the bank do not take it over.
* **AMD's threshold interrupt** (`MCi_MISC` thresholding, `THRESHOLD_APIC`
  vector) is not in this item.
* **Not shown on hardware here.** The patched QEMU is the only evidence, and
  it is reviewed, not qualified (F-17 stays open): its banks are per
  processor, so the claim of a bank two processors share is shown by the
  stage check over a fake register and argued by `CLAIM` (§3.1), not by a
  gate. A BACKLOG row asks for the N150 run (Intel Alder Lake-N, reports
  `CMCI_P`): banks claimed, a shared bank claimed by one processor of the
  ones sharing it, a corrected record interrupting, the storm rule.

## 5. How it is checked

**The patched QEMU** (`tools/common/data/qemu/0008-target-i386-CMCI-under-TCG.patch`,
in `series-10.2.1`): a `cmci` property on the x86 CPU, off by default, so
every existing boot is unchanged. With `cmci=on` under TCG: `MCG_CAP` gains
`CMCI_P`; `MCi_CTL2` is kept per bank (`CMCI_EN` and the threshold bits
writable, the rest read 0), cleared at every CPU reset (it lives past
`end_reset_fields`, so the reset hook clears it; ledger 5348 (iv)); the local
APIC answers offset 0x2F0 (and MSR 0x82F) and its version register names
seven LVT entries; a corrected record injected into a bank whose `CMCI_EN` is
set raises the CMCI entry's vector (fixed or NMI delivery, masked ->
nothing); and an uncorrected record with `PCC`, `S` and `AR` clear -- Intel's
UCNA by SDM vol. 3B 16.6.2 -- injected with `MCIP` clear is logged and
signalled by CMCI instead of `#MC` (`MCIP` clear is only the injection's way
of asking for it; ledger 5348 (v)). Without `cmci=on` the CPU, its MSRs and
its APIC are QEMU's: the 0x2F0 arms answer as the default arms do. Its
`lvt_cmci` and `mce_ctl2` are not in vmstate (no gate snapshots; C1).

**The shared xtask QEMU** (`~/.local/share/ferrix/qemu`) is rebuilt with the
patch by `fetch-qemu-linux.sh` only with the product owner's go-ahead, with
no gate running or by installing to a new prefix and swapping it in whole
(ledger 5348 (6)). Before any test-mce-cmci row counts, these run PASSED on
the rebuilt QEMU, their logs named in the code landing: the existing
test-mce-corrected, -srar, -broadcast, -srao, -left, -recover-off and -kill
rows; test-mce under KVM and under TCG; test-amd-vi. `test-mce-cmci` refuses,
naming the rebuild, on a QEMU that does not take `cmci=on`. Until then the
rows run locally with `FERRIX_QEMU_DIR` naming a build of the series with
0008 (`~/.local/share/ferrix/qemu-cmci`, `--version` "ferrix-cfi-cmci").

**`test-mce-cmci`** (x86-64, TCG, Intel's model with `cmci=on`, `--smp 2`,
`ferrix.mce.poll=3600` so the period cannot be what finds a record; the
guest is test-mce's victim, which reads a page for 20 s):

* every mode with CMCI on requires `mce: cmci offered` and **each**
  processor's `cmci on banks` line naming its own bank 1: QEMU's banks are
  per processor, so both claim (ledger 5348 (4), first route).
* **Corrected**: after the victim's line, a corrected record into bank 1 of
  processor 1. Required within 10 s: `mce: cpu 1 bank 1 corrected` exactly
  once, and `cmci1 interrupts` at least 1 at the guest's last look.
* **Ucna**: Intel's UCNA (`VAL UC EN MISCV ADDRV`, page-granular misc) at
  the victim's page, `MCIP` clear: within 10 s `mce: frame <page> retired`,
  and the `uncorrected-no-action` line.
* **Storm**: 40 corrected records into bank 1 of processor 1, 20 ms apart:
  the storm line, at most 16 interrupts counted, and `storm over` within
  10 s of the last record.
* **Window** (L.x86_64.253): `ferrix.mce.cmci-window=3`; a first record;
  on `mce: cpu 1 cmci window held` a second record, which raises nothing
  (the entry is masked); within 10 s the second corrected line, from the
  look after the unmask.
* **Off**: `Corrected` with `ferrix.mce.cmci=off`: the option's line, no
  `cmci on banks` line, `interrupts 0`, and no corrected line within 10 s.
* **Unoffered**: `Corrected` without `cmci=on`: `mce: cmci not offered`,
  nothing claimed, no line within 10 s. The existing `test-mce-*` rows are
  this case on the stock model and stay green unchanged.

**test-quarantine's CMCI row** (ledger 5348 (3)): `cmci=on`, TCG, `--smp
2`: cpu1 taken out of service; a corrected record into its bank 1; while
parked no corrected line and `drained 0`; after its return the line within
10 s, with `cmci1 interrupts 1 held 1`.

**Stage check** (`arch/x86_64/mce/check.rs`): `cmci_claims`, the claim's
decision table over a fake `MCi_CTL2` (refused read, already set, refused
write, write that does not stick, refused read-back, read-back set; the
value written), and two claimers in turn on one shared fake register, the
second answered `Taken`; `cmci_storms`, the storm rule over a sequence of
round times; `cmci_kicks`, the look after the unmask over records the round
takes and records it leaves (a fatal one, SRAO, SRAR: no kick).

**Negative controls**, each through `gate.sh control --expect`, FIRED: the
handler's wake removed (Corrected fails); the look after the unmask removed
(Window fails); the look's filter widened to a bare `VAL` (the stage
check's `cmci_kicks` fires); the read-back in step 4 skipped (`cmci_claims`
fires); the already-set test removed (`cmci_claims`' two claimers fire);
the storm threshold raised tenfold (the stage check's `cmci_storms` fires
first: its rounds a hundredth of a second apart no longer begin a storm);
the park-state test removed from the handler (the quarantine row fails:
`held 0`; the wake itself is invisible there, since a pinned task woken on
a parked processor stays queued on it unrun until the return, so the row
reads the count); the return's call removed (the quarantine row fails: no
line after return).

## 6. Requirements

To reserve before code, in one docs-only landing (free on origin/develop):

* **L.x86_64.249**: a processor taking Intel's rules whose `MCG_CAP` has
  `CMCI_P` and whose local APIC names at least seven LVT entries shall, in
  its poll task before its first round and with `ferrix.mce.cmci` not `off`,
  claim each bank whose `MCi_CTL2` it can read with `CMCI_EN` clear by
  setting `CMCI_EN` and threshold 1 and reading it back, under one lock
  across processors, and count as claimed only a bank whose read-back has
  `CMCI_EN`; a refused access claims nothing.
* **L.x86_64.250**: a processor with a claimed bank shall point its LVT CMCI
  entry at `CMCI_VECTOR`, fixed, unmasked; its handler shall mask the entry,
  count the interrupt and read no bank, and wake that processor's poll task
  only when its park state is active, leaving the wake held for the return
  to service otherwise, which wakes it.
* **L.x86_64.251**: more than 15 rounds a CMCI drove within one second shall
  leave the entry masked and the round once a second, said once, until a
  one-second round finds no record, which unmasks it, said.
* **L.x86_64.252** (`ferrix.mce.cmci=off`): no bank claimed, every entry
  left masked, said.
* **L.x86_64.253**: after a round that leaves the entry to be unmasked, the
  poll task shall unmask it and read each claimed bank's status once more,
  going round again at once only on a status the round takes.

L.x86_64.223's statement gains "after its claim, one round at once, and at
once when the processor's CMCI wakes it (L.x86_64.250)"; its criterion names
test-mce-cmci's Corrected mode. H.FAIL.8's criterion names L.x86_64.249-253
and test-mce-cmci. No new H id.

## 7. Docs

`MACHINE-CHECK.md` §3.2 (the CMCI paragraph replaced by a pointer here) and
§6 ("CMCI (step 2, real hardware)" becomes the hardware run row);
CORE-QUARANTINE §3.2 step 7 (a CMCI on a parked processor is held for its
return); SAFETY-MANUAL AoU-36's "CMCI is not used" bullet (CMCI where the
processor offers it, the poll as the backstop; the storm rule; firmware-owned
banks polled only; a parked processor's CMCI held for its return);
MEMORY-AND-TIMING (the handler's bound: two APIC writes, two atomics, a
wake; the claim's `CLAIM` hold: at most 3 x banks MSR accesses, once per
processor at boot; a storm's cost: at most 16 interrupts and rounds in its
first second, one round a second after); VULNERABILITY-ANALYSIS (an
interrupt storm from a failing DIMM bounded by §3.4; the vector not
reachable from ring 3 or a device -- `msi.rs` never hands out 0xFB and
interrupt remapping refuses an unlisted vector); a tool note that the
evidence rests on a patched emulator (F-17 open); BACKLOG (the N150 run,
§4); coverage carried after the final rebase.

## 8. Where it stands

* Code reviewed 2026-10-31 at 7cab7bd19, ledger 5418: **OK IF (1)-(4)**:
  the round's unmask held across a park as the handler's wake is (§3.4),
  L.x86_64.251's criterion naming the stage check its control fires, the
  QEMU the rows ran on named in TOOLS.md and the commit, and the landing's
  squash, rebase and coverage. 5418 also corrects 5348: 0xFC, accepted there
  as free, is `iommu::CHECK_VECTOR`; 0xFB is free.
* Design v1 (18b1e578d) reviewed 2026-10-31, ledger 5348: **OK IF
  (1)-(10)**. v2 carries (1) claim, arm, a round at once, then the wait
  (§3.4); (2) the look after the unmask kicks only on what the round takes,
  every kicked round counted by the storm rule, with a stage check (§3.4,
  §5); (3) the handler reads the park state, holds the wake otherwise, and
  the return wakes it, with the ordering argument and a quarantine row
  (§3.3, §5); (4) per-processor banks: each claims its own, the shared rule
  in the stage check and the N150 row (§4, §5); (5) the patch clears
  `MCi_CTL2` at reset and keys UCNA on `PCC`, `S`, `AR` (§5); (6) the shared
  QEMU rebuilt only with the product owner's go-ahead, the existing rows
  green on it (§5); (7) L.x86_64.250 split, L.x86_64.253 for the look with
  the held window as its deterministic control (§6, §3.4); (8) H.FAIL.8's
  criterion (§6); (9) the controls (§5); (10) the docs (§7). And: the vector
  moved from 0xFC, which interrupt remapping's check holds, to 0xFB (§3.2).
