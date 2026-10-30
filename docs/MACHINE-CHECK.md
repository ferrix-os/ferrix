# Machine check: a log of hardware errors, and surviving a bad user page

Design version 1, 2026-10-30, branch `feature/mce-log`. The customer's
brief (2026-10-30): Ferrix stops on every machine check and reads no
error bank, so it is worse than Linux for finding an unstable overclock
or failing memory. Build what Linux's MCE code gives: (1) a machine-check
log -- read the banks, report corrected errors through lode, count them
per core; (2) recovery from an error confined to one user page -- kill
the process that consumed it, retire the frame, keep the kernel running.

## 1. What is there today

`CR4.MCE` is set on every processor (F-73, `L.x86_64.175`), and the
machine check's paranoid entry stops the kernel as FX-9005 whatever the
error was (`arch/x86_64/paranoid.rs`). Nothing reads `IA32_MCG_*` or a
bank. So:

* A **corrected** error (the processor fixed it; ECC, a cache retry) is
  never seen. These are what an overclocker watches -- WHEA on Windows,
  rasdaemon or mcelog on Linux -- because they come before the crash.
* An **uncorrected** error stops the machine with no record of which bank,
  which address, which kind.
* An error **left in the banks by the boot before** -- the crash that
  reset the machine -- is never read. Warm resets keep `MCi_STATUS` on
  most Intel and AMD parts; it is the one record of why the last boot died.
* A poisoned **user** page consumed by a program (SRAR) stops the kernel,
  where Linux kills that program and goes on.

## 2. Architecture background (Intel SDM vol. 3B ch. 16, AMD APM vol. 2 §9)

| Register | MSR | Used for |
|---|---|---|
| `IA32_MCG_CAP` | 0x179 | bank count (7:0), `MCG_CTL_P` (8), `MCG_CMCI_P` (10), `MCG_TES_P` (11), `MCG_SER_P` (24), `MCG_LMCE_P` (27) |
| `IA32_MCG_STATUS` | 0x17A | `RIPV` (0), `EIPV` (1), `MCIP` (2) |
| `IA32_MCG_CTL` | 0x17B | all ones to enable every bank, where `MCG_CTL_P` |
| `IA32_MCi_CTL/STATUS/ADDR/MISC` | 0x400 + 4i + 0..3 | per bank |

`MCi_STATUS`: `VAL` 63, `OVER` 62, `UC` 61, `EN` 60, `MISCV` 59, `ADDRV`
58, `PCC` 57, and with `MCG_SER_P` `S` 56 and `AR` 55; the corrected
count 52:38 where `MCG_TES_P`; the MCA error code 15:0.

The classes the kernel tells apart (SDM table 16-6):

| Class | Bits | Signalled by | Kernel action |
|---|---|---|---|
| Corrected (CE) | `VAL`, `UC`=0 | nothing (polled), or CMCI | log, count |
| UCNA | `UC`, `PCC`=0, `S`=0 | nothing (polled), or CMCI | log, count, retire the frame if `ADDRV` |
| SRAO | `UC`, `PCC`=0, `S`, `AR`=0 | `#MC`, `RIPV` set | log, count, retire the frame after the `#MC` |
| SRAR | `UC`, `PCC`=0, `S`, `AR` | `#MC` | from ring 3 with `ADDRV`: retire, `SIGBUS` the consumer; else stop |
| Fatal | `UC` and `PCC`, or `RIPV`=0 and not from ring 3, or anything not above | `#MC` | stop as FX-9005, the banks printed first |

## 3. What the kernel does, and what it leaves to ring 3

"Keep the kernel slim" (customer, 2026-10-22): the kernel holds what only
ring 0 can do -- reading and clearing MSRs, taking `#MC`, unmapping and
retiring a frame, signalling the consumer -- and prints raw, typed records.
Decoding the error codes into words, thresholds ("N corrected errors in an
hour on core 3"), alerts and history are lode's (`docs/LOGGING.md`), from
the `mce:` lines.

### 3.1 Bring-up, on every processor (`arch/x86_64/mce.rs`)

After `enable_machine_check` (unchanged), each processor once it has its
processor record -- a secondary at its bring-up (`smp::secondary_start`),
the boot processor from `mce::start`, since it sets `CR4.MCE` before its
record exists -- and before any program runs there. Every access here is
the checked read or write (`L.x86_64.232`): a register the processor
refuses leaves it without a record, said by `start`, and its machine checks
stop as before. Each processor:

1. reads `MCG_CAP` through the checked read (`L.x86_64.188`); none, or
   no `CPUID` MCA bit (`CPUID.1:EDX` bit 14), leaves the machine as today;
2. **keeps what the boot before left**: every bank with `VAL` set is
   handed to the processor's poll through a pending slot and cleared; the
   poll prints it as `mce: cpu <n> bank <b> left from before this boot
   status=.. addr=.. misc=..` (bring-up itself prints nothing). It is read
   before any bank is enabled, so nothing new is in it yet;
3. writes `MCG_CTL` all ones where `MCG_CTL_P`, and each bank's `MCi_CTL`
   all ones, as Linux's `__mcheck_cpu_init_generic` does (bank 0 on Intel
   family 6 before model 0x1A excepted, which Linux leaves to firmware);
4. records the bank count in its per-CPU record.

### 3.2 The poll (corrected errors and UCNA)

The kernel has no periodic tick (`sched/vtime.rs`), so the poll is one
kernel task per processor, pinned there (`sched::spawn_on`, as `msrlog`
does), started after the scheduler. It wakes every `ferrix.mce.poll`
seconds (1 to 3600 of one to four ASCII digits; 5 by default and for any
value not understood, which a line says -- `msrlog`'s chooser, shared).
Linux polls every 300 s and halves on a find; a fixed short period is
simpler to reason about and costs one `rdmsr` per bank per period. Each
round, and also whenever the processor's pending flag (§3.3) is set, for
each bank with `VAL`:

* corrected: prints `mce: cpu <n> bank <b> corrected status=<hex>
  addr=<hex|-> misc=<hex|-> total=<this cpu's corrected count>`;
* UCNA: the same with `uncorrected-no-action`, and the frame retired
  (§3.4) when `ADDRV`;
* any other class found by the poll (an `UC` error whose `#MC` this
  processor has not taken yet) is left in the bank for the `#MC`;
* clears `MCi_STATUS` (writes 0) after printing.

Banks are read only by the task on their own processor. On Intel some
banks are shared by a core's threads or a package, so two processors may
print the same record; Linux has the same duplication. lode may fold
records with the same bank, status and address within a period.

The lines go to the kernel log at `KERNEL` priority, which lode reads from
`/dev/kmsg` (`src/lib/log/lode/src/kmsg.rs`); a corrected error is
priority 4 (warning), the rest 3 (error), through `console::write_record`.

**CMCI is not in this version.** It needs a local APIC vector and
`MCi_CTL2` ownership per bank, and neither QEMU under TCG nor the gates'
KVM model offers `MCG_CMCI_P` (QEMU's `MCE_CAP_DEF` is `MCG_CTL_P |
MCG_SER_P`), so no gate could show it working. It is a later step,
measured on the N150.

### 3.3 The `#MC` itself

The paranoid entry keeps its rules (`paranoid.rs`): from ring 0 it never
blocks, takes a lock, allocates or prints except on the way to stopping.

**Each bank's action** (`mce/decode.rs`, pure; L.x86_64.224, 225):

| Class | `MCG_STATUS` | From | Action |
|---|---|---|---|
| corrected, UCNA | any | any | leave for the poll |
| SRAO | `RIPV` | any | defer: pending slot, bank cleared |
| SRAR | `RIPV`, `EIPV` clear | any | **unaffected**: another processor's consumption, seen by the broadcast or in a shared bank; not cleared, not retired |
| SRAR | `EIPV` | ring 3, usable address | signal, a kill where `RIPV` is clear |
| anything else | | | stop |

The banks fold to the gravest action; two signals in one machine check
stop. A machine check left with only "leave" or "unaffected" -- a
broadcast this processor did not cause, or one with no valid bank at all --
returns, counted as spurious, where `RIPV` is set, and stops where it is
clear (the consultant's M1).

**From ring 0** (on the IST stack, as today): `ferrix_paranoid_entry`
calls `mce::on_kernel_stack`, which reads `MCG_STATUS` and every bank
(`read_msr`: bring-up proved them with the checked read), folds the
actions, and either stops -- the report prints each valid bank (`mce:
bank <b> status=.. addr=.. misc=.. <class>`) and stops as FX-9005 -- or
defers the action-optional banks to its processor's pending slots, clears
them and `MCG_STATUS`, and returns. It takes no lock, allocates nothing
and prints nothing unless it stops. A machine check on a processor
without a record, with `ferrix.mce.recover=off`, or with no free slot,
stops.

**From ring 3**: the stub's `4:` branch, which moves a ring-3 `#DB` to
the task's kernel stack, takes vector 18 too, and only 1 and 18, as
Linux's `exc_machine_check_user` runs on the task stack since 5.9.
`ferrix_trap_from_user` does the entry hardening the branch skips. On the
task stack `ferrix_trap_entry` calls `mce::from_user` first, before the
trap path's accounting and with interrupts masked; `MCIP` is still set
until it has read the banks. It decides as above and:

* stop: as from ring 0;
* signal: writes the record's page and the kill bit into the frame's
  error code (a machine check pushes none of its own), clears that bank,
  defers any action-optional ones, clears `MCG_STATUS`, and lets the trap
  path go on, where `trap::user_fault` finds the mark: §3.5;
* otherwise it defers, clears `MCG_STATUS` and returns to the program
  without the trap path.

`MCG_STATUS.MCIP` is cleared as the last act before any return: a second
`#MC` while it is set shuts the processor down (QEMU makes it a triple
fault, `helper.c` `do_inject_x86_mce`). A ring-0 one that comes after the
clear and before the `iretq` finds the stack occupied and is reported as
FX-9006, which is the report failing, as before.

**Shared banks and the poll.** A bank shared by a core's threads or a
package is read by every processor that shares it. The races, each
argued:

* *Two processors' polls see one corrected record.* Both print it and
  clear it; it is counted twice, as Linux counts it. lode may fold them.
* *A poll reads a corrected record; an uncorrected one overwrites it
  before the clear.* The uncorrected record's machine check is delivered
  at once on the processor that raised it, which reads it before the
  poll's `wrmsr` of 0 can run on any other (the check is synchronous to
  the consuming instruction; an action-optional one is delivered at the
  next instruction boundary). What the clear can lose is a record whose
  check has been taken, or a second corrected record, which `OVER` would
  have marked anyway.
* *A broadcast: the consumer and a bystander read one shared SRAR bank.*
  The bystander sees `EIPV` clear and leaves the bank; the consumer clears
  it after it has read it. Should the bystander read after the clear, it
  finds no valid bank and `RIPV` set: spurious, it returns.
* *A poll and a machine check on one processor.* The poll leaves every
  uncorrected record to the check; the check leaves corrected ones to the
  poll; the pending slots' protocol keeps a slot from being written while
  it is read.

### 3.4 Retiring a frame

The kernel keeps no map from a frame to its mappers (the reverse map is per
VMO, `user/vmo.rs`). So retirement is Linux's default "late kill"
(`vm.memory_failure_early_kill = 0`): a process that maps the frame and
has not touched it is not signalled; the frame is never handed out again;
whoever consumes the poison takes its own SRAR (§3.5).

* A set of retired frames (256 entries, atomics; full: said once, and
  later frames are logged but not retired -- a machine with 256 bad pages
  has failing memory the log already names). A count beside it is 0 on a
  healthy machine, so the allocator's added test is one relaxed load.
* A retired frame that is **free** is taken out of the free lists with
  `claim_frame` and held for good.
* A retired frame that is **allocated** stays where it is; `release_frame`
  and `deallocate_frames` drop it instead of freeing it on its last
  reference, disowning its charge (`disown_frame`) so its job's counters
  come back to zero. A multi-frame block holding one is split
  (`split_frames`) and the rest freed one by one.
* `/proc/ferrix-mce` lists the retired frames (§3.6).

### 3.5 SRAR from ring 3

The current task consumed the poisoned data. `mce::from_user` has marked
the frame and cleared the bank and `MCG_STATUS` (§3.3). Then, with
interrupts open, `trap::user_fault` asks `mce::signal_for`, which:

1. retires the frame of `MCi_ADDR` (§3.4);
2. finds the lowest address in the program half of the current address
   space that maps the frame, walking its leaves under the space's own lock
   (`AddressSpace::find_frame`), stopping at the first match, at the half's
   end or after 2^18 leaves, else uses 0;
3. answers `SIGBUS`, `si_code` `BUS_MCEERR_AR` (4), `si_addr` that
   address; `Origin::Fault`'s encoding writes `si_addr_lsb` 12 for a
   `SIGBUS` with `BUS_MCEERR_AR` or `_AO` alone, at Linux's offset (and
   the 32-bit layout's, `sigframe32`).

The trap path raises it through the personality's `fault_signal`; with
`RIPV` clear the instruction cannot be restarted, and it goes through the
new `fault_end` instead, which ends the process with the signal as its
status and runs no handler (Linux's `force_fatal_sig`).

The frame stays mapped in that process until it exits. A handler that
returns and touches the page again consumes the poison again on real
memory (another SRAR, another `SIGBUS`); in QEMU, where the injected
poison is not in memory, it reads the old data. Unmapping the page from
every mapper needs a frame-to-mapper map the kernel does not have; that
is §6's.

### 3.6 Counts for ring 3

`/proc/ferrix-mce` (beside `/proc/ferrix-seam`, `fs/procfs.rs`'s table),
mode 0444, computed when read, with no address in it (the consultant's
M9):

```
recover on
cpu0 banks 10 checks 1 recovered 1 spurious 0 corrected 0 ucna 0 left 0 polls 39
cpu1 ...
```

`checks` counts machine checks taken, `recovered` the ones that returned,
`spurious` those with nothing for this processor, `left` the records the
boot before left. `/proc/ferrix-mce-retired`, mode 0400, lists the retired
frames' physical addresses, one a line. lode reads the `mce:` lines for
history and alerts; the files are for tools and the Settings app.

## 4. Requirements

Reserved on `develop` in 971dd7f2a and written in the model
(`docs/sysml/13-item-requirements.sysml`, `17-memory-`, `18-x86-64-`):

* **H.FAIL.8**: which machine checks do not stop (parent `O.FAILSAFE`);
* L.x86_64.221 bank bring-up; .222 the record the boot before left; .223
  the poll's record and clear; .224 the classifier; .225 the bystander and
  spurious rule; .226 an action-optional check handed to the poll; .227
  the fatal report prints the banks; .228 a program's poison is its
  `SIGBUS` (the consumer's own mapping stays, late kill: §3.4); .229 the
  kernel's poison stops; .230 `/proc/ferrix-mce`; .231
  `ferrix.mce.recover=off`; .232 the bank cap and the checked accesses;
* L.mm.91 a retired frame is never allocated again; L.mm.92 the full set;
* L.x86_64.175's criterion now requires the report's `mce: bank` line.

## 5. How it is checked

* **Stage 4**: the classifier over a table (every class, every bit that
  flips it, `MCG_STATUS.RIPV` clear); the retired set's full and refusal
  paths; `MCG_CAP` read on every processor and the bank count said.
* **`test-mce`** keeps its row (a `PCC` error on every processor still
  stops as FX-9005; it now also requires the report's `mce: bank 0` line),
  and gains three modes, each its own boot, under KVM and under TCG:
  * `--corrected`: a corrected error injected into bank 1 of every
    processor; each must print one `mce: cpu <n> bank 1 corrected` line
    within two poll periods (`ferrix.mce.poll=1`), and no more for the
    same injection; `/proc/ferrix-mce` reads `corrected 1` on each.
  * `--srar`: on one processor (`--smp 1`), a ring-3 test program
    (`mce-victim`) maps a page, writes it, prints the page's virtual
    address and spins reading it. The gate finds the physical address
    from the monitor (`gva2gpa`, retried until the program is the one
    on the processor), injects SRAR at it, and requires: the program's
    parent prints its child ended by `SIGBUS` with `si_code` 4 and the
    printed address; the kernel goes on (a second command runs);
    `/proc/ferrix-mce` lists the frame retired and `recovered 1`.
  * `--srao`: SRAO with a physical address the kernel uses nowhere
    special (one the program's page again); the kernel goes on, prints
    the `uncorrected` record from the poll task, and lists the frame
    retired.
  * `--left`: a corrected error injected into bank 1, then the monitor's
    `system_reset`; the next boot must print `mce: cpu 0 bank 1 left from
    before this boot`. QEMU keeps the banks across a reset under TCG
    (`mce_banks` lies after `end_reset_fields` in `CPUX86State`); under
    KVM it is **to measure**, and if KVM clears them the row is TCG only.
* **Negative controls**: the classifier made to call SRAR fatal, `--srar`
  stops as FX-9005; the poll's clear removed, the corrected line repeats
  every round; the allocator's retired test removed, stage 4's
  retired-frame check (retire a frame it allocated, free it, allocate
  until it would have come back) fails.

## 6. What it does not do

* CMCI (step 2, real hardware).
* AMD's deferred errors and SMCA's per-bank `MCA_IPID` decoding: the
  raw values are printed; decoding is lode's.
* Recovery from kernel-mode SRAR, but for a system call's read of a
  program's memory, which `docs/MACHINE-CHECK-COPY.md` recovers (H.FAIL.10).
* Arm RAS (`ERR<n>STATUS`, SEI/SError): AArch64 and ARMv7-A keep their
  current behaviour.
* Early kill and unmapping a poisoned page from every mapper: needs a
  frame-to-mapper map (§3.4). Page-cache poison in a clean page is dropped
  from every mapper and read again from the disk
  (`MACHINE-CHECK-PAGECACHE.md`, H.FAIL.11), the cache list walked to find it;
  a dirty page's is not, and btrfs writeback of it recovers and fails `EIO`
  each round until the page is truncated or deleted.
* **Residuals of late kill** (the consultant's M5; SAFETY-MANUAL AoU-36,
  VULNERABILITY-ANALYSIS T.EXHAUST path 8): the frame stays mapped in the
  consumer until it exits and in every other mapper. The kernel consuming
  it -- a system call's copy, a copy-on-write copy at `fork`, a page-cache
  read, a device's DMA -- is a ring-0 machine check and stops as FX-9005,
  as any poison does today. A handler that returns and touches the page
  again consumes it again on real memory.
  (A system call's copy now fails the call instead: `MACHINE-CHECK-COPY.md`;
  a page-cache read of a clean page is refilled: `MACHINE-CHECK-PAGECACHE.md`.)

## 7. Where it stands

Design v1 reviewed by the certification consultant on 2026-10-30
(`~/.local/share/ferrix/cert-consultant/reviews.md` line 4772): **OK IF
M1-M14**, before any code. Internal review, not independent verification
(F-27).

Rulings on the questions asked: (a) a ring-3 `#MC` on the task stack by the
`#DB` route is accepted (M2); (b) late kill without unmapping other mappers is
accepted as a stated residual, no worse than today (M5); (c) the 5 s poll is
accepted (M8); (d) ids `L.x86_64.221-232`, `L.mm.91-92` and `H.FAIL.8`, to be
reserved before they are written (M11); (e) no new ring-3 control input, but
physical addresses must not be world-readable (M9) and continuing after an
uncorrected error changes the fail-safe argument, so a new `H.FAIL.8`, a
SAFETY-MANUAL statement and a `ferrix.mce.recover=off` switch (M10).

The conditions in short: M1 broadcast `#MC` and shared banks (an unaffected
processor, `EIPV` clear and `RIPV` set, neither stops nor clears; a `#MC`
with no valid local bank and `RIPV` set returns), checked by an `--srar -b`
mode on two processors; M2 the route's limits and `MCIP` held until the
frame is off the IST stack; M3 the classifier fails closed (`OVER` with
`UC`, `AR` without `S`, unusable addresses, frames the allocator does not
cover, at most 32 banks); M4 the retired test and the retirement's claim
under the allocator's lock; M5 section 4's "mappings gone" removed or made
true, residuals written; M6 a `RIPV`-clear kill never returns, the VA walk's
lock and bound; M7 a single-writer protocol for the pending slots; M8 the
poll's limits; M9 `/proc/ferrix-mce`'s addresses root-only and the
vulnerability analysis; M10 `H.FAIL.8` and the switch; M11 the id mapping;
M12 MSR accesses traced and the bring-up writes checked; M13 checks and
negative controls, `--srar`'s flake rate measured; M14 code review before
`land.sh take`, coverage carried.

**Code review, 2026-10-30** (`reviews.md` line 4795, on e2f4d7479 and
8091d0865): **OK IF V1-V8**. Internal review, not independent verification
(F-27). Accepted: the decision carried in `TrapFrame::error_code`; the
widening of `L.x86_64.192` to `mce.rs`'s `MCG_CTL` and `bank_msr`; KVM's
recovery rows ending in the fail-closed FX-9005 on a host without
`MCG_SER_P`; `test-mce-left` injecting while the loader runs, which shows
bring-up's read, not survival across a warm reset. The five controls that
fired on b74ee2423 carry. The `srar-fatal` control that did not fire is
superseded by `srar-fatal-2`, which stage 3's classifier table catches.

Owed before `land.sh take`:

* V1: the mark in `error_code` currently reaches the `SIGBUS` handler as
  `uc_mcontext.err`, which gives a physical address to ring 3. It is
  cleared before the signal is raised, `test-mce` requires `err=0`, and a
  new control shows that check firing.
* V2: a gated `RIPV`-clear kill mode (SRAR with `MCG_STATUS` 0x6): the
  victim is killed by signal 7, its handler does not run, and the kernel
  goes on.
* V3: the BACKLOG row for `find_frame`'s worst case, and BACKLOG's
  machine-check gate row extended to `mce.rs` and the new modes.
* V4: the rows run by `gate.sh` on the landing hash. The 20/20 flake runs
  are an engineering measurement, not row evidence.
* V5: the controls re-run where V1 or V2 touch what they control.
* V6: coverage carried, with the unmeasured lines listed as owed.
* V7: the `wip:` commits squashed or reworded, citing both ledger lines.
* V8: no processor available offers `MCG_SER_P`, so recovery has run under
  TCG only.

**Hardware (V8).** No processor at hand offers `MCG_SER_P`. On nazuna's
AMD Ryzen 9 9900X under KVM, and on the N150 bare metal, the kernel reads
recovery as not reported, so every uncorrected error stops there as
FX-9005, as designed. Recovery has run under TCG only, which emulates
`MCG_SER_P`. An engineering note, not row evidence: the N150 booted
4f7aba894 (this branch before the bring-up split) on 2026-10-30 and read
20 banks on each of its 4 processors, polled them, and found nothing left
by an earlier boot. The flake measurement was also engineering only:
`test-mce-srar` and `test-mce-broadcast` passed 20 of 20 each under TCG
once the gate stopped the VM around the injection
(`~/.local/share/ferrix/logs/mce/mce-flake.out`).

**V1, V2 and V3 done** (after 8091d0865): `mce::signal_for` clears the
frame's error code before the signal is raised, the victim prints its
context's `err` and `test-mce-srar` requires `err=0x0`; `test-mce-kill`
injects SRAR with `MCG_STATUS` 0x6 and requires the victim killed by
signal 7 with its handler not run; P2 rows for `find_frame`'s timing,
CMCI, AMD recovery and the coverage run, and BACKLOG's machine-check
gate row extended.

**Landing record (V4, V5, V7).** Rows by `gate.sh`, logs in
`~/.local/share/ferrix/logs/queue/<tag>.log`, each with its header:

* on b60911804 (the change and its coverage carry), every one `run:
  PASSED`: `mce-land-build-all`; `mce-land-boot-x86-kvm`, `-x86-tcg`,
  `-x86-smp2` (kvm), `-aarch64`, `-armv7a`, `-armv7a-smp2`;
  `mce-land-threads` (all arches); `mce-land-shell-ferrousli` (x86_64
  kvm); `mce-land-ipc-equiv` (kvm); and under kvm and under tcg
  `mce-land-mce-<accel>` and `mce-land-mce-<mode>-<accel>` for corrected,
  srar, broadcast, srao, left, recover-off and kill;
* `check` on adf5329b8, the landing tip: `mce-land-check-7`, `run:
  PASSED`, run with `SENTRY-TRACE` unset (the desktop app's variable
  breaks an xtask environment test, a P1 flake row since adf5329b8;
  `mce-land-check-5` shows that test alone failing). Between b60911804 and
  the tip: the generated architecture document, `expect(dead_code)` off
  x86-64 on three items by `cfg_attr` with `rustfmt`'s wrapping (no code
  on x86-64, none generated on Arm), the coverage carry past those lines,
  and the flake row;
* controls, each `control: FIRED`: `mce-ctl-mark-kept` (b60911804, the
  mark left in the frame: `err=0x0` missing) and, on b74ee2423 with their
  targets unchanged since, `mce-ctl-srar-fatal-2`, `mce-ctl-poll-clear`,
  `mce-ctl-retired-test`, `mce-ctl-unaffected`, `mce-ctl-vector-18`.

**Final check, 2026-10-30** (`reviews.md` line 4810, on 27053f5ce):
**OK**. V1-V8 are met. The rows on b60911804 count for the tip: the only
source difference is three `cfg_attr(not(target_arch = "x86_64"),
expect(dead_code))` attributes. On x86-64 they produce nothing. On Arm,
check-7's three-target build fulfilled the `expect(dead_code)`, which is
the compiler's proof that nothing reaches those items. `mce-land-check-7`,
run with `SENTRY-TRACE` unset, is accepted: the one test that variable
breaks is xtask's, outside the item, and has a BACKLOG row. Internal
review, not independent verification (F-27).
