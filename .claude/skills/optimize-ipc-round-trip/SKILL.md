---
name: optimize-ipc-round-trip
description: How to optimize Ferrix's native channel round trip (channel_write_read, 0x1013, to a waiting server and back) end to end, on x86-64 or ported to AArch64 and ARMv7-A. Load it before measuring, profiling or changing the IPC path -- the syscall entry and exit, the channel's write/wait/read, the scheduler's wake and switch, the address-space switch, user register state, the fast path -- or before porting steps 1 to 5 of OPAQUE-KERNEL.md §9 to another architecture. It gives the figure and its exact measuring protocol, what may never be traded away, the budget, every attempt so far with its measured result (dead ends included), what each x86-64 step maps to on Arm, and how to hand a result back.
---

# Optimizing the IPC round trip

This is a hot-path instruction in the format of `docs/HOTPATHS.md`. Its
facts in machine-readable form, and the full list of attempts, are in
`docs/hotpaths/ipc-round-trip.json`; measured results are filed under
`docs/hotpaths/results/ipc-round-trip/<hardware hash>/`. The history it is
written from is `docs/OPAQUE-KERNEL.md` §9 (read §9.1, §9.5 to §9.10),
`docs/handover/2026-10-03-ipc.md`, and the certification consultant's
ledger (`~/.local/share/ferrix/cert-consultant/reviews.md` on nazuna,
2026-10-01 to 2026-10-06).

Every figure below is marked **(measured)**, with where, or **(guessed)**,
or **(argued)** where it follows from a document rather than a run. Do not
quote a guessed figure as a result; measure it.

State on 2026-10-06: `main` ccf72dd94 has step 1, 2a to 2f, 3a/3b and
§9.10's budget. On branches, not landed: step 4 (`po7/step4`), ERAPS
(`po7/step5`, consultant OK, may land), the `VZEROALL` reset
(`po7/step5-vec`), the timing build (`po7/prof`, never lands). Read them
with `git show <branch>:<path>`; do not edit them. Check `git log main`
first: this list ages by the hour.

## 1. The path

- **Entry:** the client's `SYSCALL` (`SVC` on Arm) of `channel_write_read`
  (0x1013) in `ferrix_rt`'s stub, `src/user/system/native/rt/src/arch/<isa>.rs`.
- **Exit:** the client's next instruction after `SYSRET` (`ERET`), the
  server's echo in its return registers.
- **One direction:** one side's entry into 0x1013 to the other side's return
  from its own 0x1013. A round trip is two directions; budgets are per
  direction.
- **In between** (x86-64, general path): the entry stub and its hardening
  (`arch/x86_64/syscall.rs`), the native decode (`syscall/native.rs`
  `channel_write_read`), the handle lookup, `write_small` and the peer's wake
  (`object/channel.rs`, `sched/wait.rs`), the block and the scheduler's
  decision (`sched/mod.rs`, `src/lib/kernel/sched`, the EEVDF queue and
  `account`), the address-space install with the speculation domain's
  barrier decision and the return-stack refill (`user/space.rs`,
  `arch/speculation.rs`, `arch/x86_64/speculation.rs`), user register state
  (`arch/x86_64/switch.rs`: selectors, FS/GS bases, the vector reset),
  `switch_to`, and the way out with the pending-work look.
- **The fast path** (step 4, `po7/step4`) replaces the decode-to-pick span by
  one function reached from the entry stub for 0x1013: a park on the channel
  half, the words written into the waiting peer's frame, and a direct switch
  (`hand_over`). Its design and the eleven conditions it was accepted under
  are §9.7.

## 2. The figure, and how to measure it

**The figure:** `bench-ipc`'s `domain-call` line, p50, in ns: a client's
`channel_write_read` answered by an echo server's own, both programs in one
speculation domain, 20,000 timed trips after 1,000 untimed.

**The gated configuration is the matched one** (customer, 2026-10-02): every
mitigation on, both programs in one domain, compared with seL4 built with the
same protections. `--mitigations off`, the `call` line (outside a domain,
with the switch barrier) and IBPB at every switch are reported, not gated.

**The target:** under 400 ns p50 matched, on nazuna (customer, 2026-10-06).
seL4 matched is 440 ns there, 400 to 410 without its refill **(measured,
§9.6)**. Redox 0.9.0's scheme round trip is 1,965 ns with no speculative
defence at all **(measured, §9.6a)**.

**The protocol** (x86-64 under KVM; the same shape everywhere):

1. Fingerprint the machine: `cargo xtask hw-fingerprint --accel kvm`. Look
   in `docs/hotpaths/results/ipc-round-trip/<first 12 hex>/` for results of
   your exact machine, and match `cpu_hash` across directories for results of
   the same silicon elsewhere.
2. **Quiet host.** Load under 4 and the pinned core's SMT sibling under 20%
   busy. `bench-ipc` prints both for every run (`host load … SMT sibling cpuN
   M% busy`); a busy sibling adds 15 to 20% **(measured, §9.6)**, 30% at 44%
   busy **(measured, §9.6a)**. Before starting, check that no other session
   is benchmarking on the same core: `ps -eo pid,args | grep 'taskset -c 11'`.
   On the shared host, never kill another session's QEMU; wait.
3. **ABAB, never a lone figure.** Absolute figures move hour to hour on a
   shared host; a ratio taken turn about in one run holds. Measure a change
   against its base:

   ```
   FERRIX_HOTPATH_LOG=<log> cargo xtask bench-ipc --release --accel kvm --smp 1 --arch x86_64 \
       --init ~/.local/share/ferrix/busybox/{arch}/bin/busybox.static \
       --alternate <base ref> --rounds 5 --record 2>&1 | tee <log>
   ```

   It builds the base in `.claude/worktrees/bench-alt-<sha>`, runs one
   uncounted warm-up of it, then five rounds this-tree-first, and prints the
   median ratio and its spread. Use your own `CARGO_TARGET_DIR`. Quote the
   ratio, its spread, the rounds and the load, e.g. "2,287 against 2,427 ns,
   0.942 (0.930 to 1.124), 5 rounds, load 2 to 4".
   An A/A run (the same kernel both sides) read 0.996, spread 0.996 to
   1.004, over 3 rounds **(measured, 2026-10-06, the `aa-noise` entry)**:
   a ratio inside about 1% of 1.000 is not a change.
4. **Pinned, one vCPU.** `bench-ipc` defaults to `--smp 1` and pins QEMU to
   host core 11 (where seL4 and Redox were measured) on hosts with more than
   12 processors.
5. **Outliers:** a round far off its neighbours (one read 2,716 against
   about 2,290 in the 3a/3b run) is reported, not dropped; if the median
   depends on it, take five more. The `call` row is noisier (11 to 15 us in
   two rounds against 6 us) and is retaken before anything cites it.
6. **Resolution:** the guest TSC steps in 44 ticks (10 ns) at 4,400 MHz.
   Before 3349682db (`bench-exact`) the p50 was a histogram floored to an
   eighth of a power of two and moved in 233 ns steps; figures from before it
   cannot show savings below that, which is why 2a's and 2c's own savings
   were never measured.
7. **Record it:** `--record` writes the result under
   `docs/hotpaths/results/ipc-round-trip/`, keyed by the hardware hash, the
   commit, and the configuration (`docs/HOTPATHS.md` §6). Commit the record
   on your branch with the change it measures.
8. **Against seL4:** `--against-sel4` alternates with
   `~/.local/share/ferrix/sel4/run.sh matched-nopcid` (seL4 at c6ce4d2a,
   sel4bench-manifest 80add415). seL4's own one-way figures read a `cpuid`
   per sample, which exits the VM; compare its root task's round trip.

**Profiling** (where the time goes, not the figure): the timing build
`po7/prof` stamps the TSC at 40 points of one direction, subtracts the
stamp's own cost (7 to 9 ns), counts only directions whose stamps came in
order and that issued no IBPB (so timer switches, the general trip and the
cross-domain run drop out), and quotes each span as its mean up to p90; the
spans add to 1,209 to 1,300 ns against the 1,130 ns half of the
uninstrumented p50 **(measured, §9.10)**. Ablation switches (one piece
skipped at a time, measured ABAB) give a piece's cost without the stamps'
distortion. Timing builds never land.

## 3. What may never be traded away

- **The matched configuration.** A figure with fewer defences than seL4's
  matched build is not the figure.
- **The return-stack refill at every address-space switch** (§9.3a, A2),
  except where the processor provably empties the predictor itself (ERAPS,
  under its own review: CPUID 0x8000_0021 EAX[24] and `CR4.PCIDE` clear).
- **The switch barrier between programs not in one speculation domain.**
- **FDP_RIP.2 as widened** (customer, 2026-10-02): no register state of one
  program reaches another at any switch -- general, vector, `MXCSR`, x87,
  `XINUSE` as a program can read it (`XGETBV 1`), selectors and their RPL
  bits, FS/GS bases.
- **The general path's observable results.** The fast path is a second
  implementation, accepted as a *tested equivalence*: `ferrix.fastpath` off
  and on must agree on every return word, code, order, a peer's close, a
  signal or kill during the wait. It is off in the certified configuration
  for the first release (customer, 2026-10-02).
- **Process:** every change here is inside the certified item. The design
  goes to the certification consultant before code, and the code before
  `land.sh take`; requirement ids are reserved before rows are written; one
  landing, one review. Each condition needs a check and a negative control
  that fires (`gate.sh control … --expect`); a control no boot can reach is
  replaced by one that can, or by a `loom` model, with the consultant's
  agreement.

**The checks a change here runs:** `cargo xtask gate-rows` names them;
typically `check` (with `loom` in `src/tests/loom`), the boots on x86-64 KVM
and TCG, AArch64, ARMv7-A and ARMv7-A at `--smp 2`, `test-threads --arch
all`, `test-shell --init ferrousli`, and every control. Step 4 adds
`ipc-equiv` (the equivalence cases, fast path off and on) and one control per
fast-path condition (its test replaced by "true").

## 4. Where the time goes (x86-64, nazuna, 2026-10-06)

One direction is about 1,130 ns on `main` **(measured, prof-7/8)**:
software from entry to exit with the scheduler about 650 (the wake's EEVDF
insert 170 to 200, the decision 215 to 240 of which `account` is 120 to 134,
mostly `follow_group_share` at 85), user state 240 to 275 (the `DS`/`ES`/`FS`
loads 105 to 118, `GS` 35 to 43, the `XRSTOR` reset 71 to 79), `CR3` 90 to
113, the user TLB refill after it 50 to 100, the stub and ring 3 60 to 85,
the refill 12 to 26. The 10 to 19 us p90 is `arm_timer` reprogramming the
local APIC (an exit) on 10 to 20% of directions.

What is left after everything known: about 210 to 260 ns a direction
(**guessed**), so under 400 a round trip needs every item at once -- step 4 at
the low end, ERAPS, the `VZEROALL` reset, the `DS`/`ES` skip, and at most two
user pages touched a side. The floor no software moves is about 200 ns a
round trip: two `CR3` writes without PCID and two hardware entries and exits.
On hardware with PCID, 400 is comfortably within reach **(guessed)**. The
table is `budget` in the data file.

## 5. What has been tried

The data file's `tried` has every attempt; the ones that teach:

- **The selector skip, three times.** §9.4 item 5 skipped segment and base
  writes equal to what the save read: no difference measured -- but that
  bench resolved 233 ns. §9.8's answer 12 then ruled "the segment skip does
  not come back", because 3b's per-processor "last written" base record
  leaks across vendors (`USER_DS` then a null `FS` keeps or clears the base
  by vendor; condition 8). The exact bench and the profile then measured the
  selector loads at 140 to 160 ns a direction, the largest item after the
  scheduler, and the consultant reopened it narrowly (S1 to S7): `DS` and
  `ES` only, skipped only exactly 0 to 0 (1 to 3 are null selectors with RPL
  bits a program can read back), compared with the processor's own registers
  read in the same switch, never with a record. `po7/step4`'s a12485d7a also
  skips `FS`/`GS` and does not meet S3 to S5 or S7. *Lesson: a "no
  difference" from a coarse bench is not a dead end; and a skip compared with
  a remembered value is a leak, one compared with the register is not.*
- **`IBPB` was thought to cost 2 us a switch; it costs about 230 ns under KVM
  here** **(measured, §9.6)**. The speculation domain still pays off.
- **PCID:** nazuna has none (CPUID 1 ECX[17] clear; it has `INVPCID`,
  ERAPS, TCE, PKU), so step 3's PCIDs wait for hardware that has them; seL4
  with `KernelSupportPCID` will not boot there.
- **`XSAVEOPT`** for the save made no difference; an `XRSTOR` costs about
  70 ns whatever it restores (whole area 77, split 134, SSE+AVX alone 72),
  while `VZEROALL` + `LDMXCSR` with the x87 by `XINUSE` costs 15
  **(measured, §9.10)**.
- **The charge:** the direct switch must charge by a subtraction and a
  store, not through `CpuQueue::account` (120 ns, `effective_weight` at every
  charge).
- **Not pursued:** PKU instead of separate spaces, segment-limited small
  spaces (no limits in long mode, no LMSLE), SVM ASIDs (they tag VMs).

## 6. Porting to AArch64 and ARMv7-A

§9.5 says the Arm ports gain from step 2 and get ASIDs and a fast path of
their own after x86-64, **measured on hardware**: under TCG the TLB costs say
nothing. What each x86-64 step maps to:

| x86-64 step | AArch64 | ARMv7-A | Notes |
|---|---|---|---|
| 1, 2a, 2c, 2d, 2e, 2f | the same code | the same code | Arch-neutral. Built and booted on Arm in every gate, never *measured* there. |
| 2b, the preemption count without a locked op | same | same, but remote reads of the packed `u64` may tear | §9.8 condition 2: every exception that can take a lock is masked (AArch64 SError and pseudo-NMI; ARMv7-A FIQ with `cpsid if`). The consultant's advisory: measure on Arm before claiming a saving. |
| 3a, vector contract and reset | `q0`-`q31` (512 bytes) plus `FPCR`/`FPSR` saved at every switch (`arch/aarch64/switch.rs`). Port: a task blocked in 0x1013 saves nothing; the reset zeroes `v0`-`v31` and loads the task's own `FPCR`/`FPSR` (the analogue of V1's `LDMXCSR`). No `XINUSE` analogue, so no conditional arm. Only when no SVE/SME state is enabled (the analogue of V2). | VFP `d0`-`d31` (or `d0`-`d15` on D16 parts), `FPSCR`, `FPEXC` | **First:** the Arm runtime stubs (`rt/src/arch/aarch64.rs`, `armv7a.rs`) do not declare the vector registers clobbered as the x86-64 stub does (`clobber_abi("sysv64")`); the contract is not in force on Arm until they do (`clobber_abi("C")`). The check and controls of V5 have Arm forms to write. |
| 3b, FS/GS bases kept in the task | TLS is `TPIDR_EL0`, which EL0 writes itself, so it must be read at save; an `mrs` costs a few cycles, not an `rdmsr`. Little to gain **(argued)**. | `TPIDRURO` is written only by the kernel (`set_tls`), so it can be kept in the task and the read at save (`armv7a/switch.rs`, `read_tpidruro`) dropped: a direct port of 3b. `TPIDRURW` is user-writable and stays saved. | Condition 8's lesson applies: never skip a write by comparing with a per-processor record. |
| Selector skip (`DS`/`ES`) | none | none | No segment state on Arm; the 140 to 160 ns x86 item does not exist there. |
| `CR3` write without PCID; step 3's PCIDs | Today every space is ASID 0 and each switch writes `TTBR0_EL1` and runs `TLBI ASIDE1` (`aarch64/cpu.rs`, `write_ttbr0`, `flush_user_tlb`). Port: ASIDs (8 or 16 bits, `ID_AA64MMFR0_EL1.ASIDBits`) with a generation allocator (Linux's arm64 allocator is the model), the ASID in `TTBR0_EL1`, no flush on switch, a rollover flush. | ASID 0 and `TLBIASID` today (`armv7a/cpu.rs`; with LPAE the ASID is `TTBR0` bits 55 to 48). Port: 8-bit ASIDs, the same allocator. | Unlike nazuna's missing PCID, every ARMv8-A and ARMv7-A core has ASIDs: likely the largest Arm win **(guessed)**. Must be built with the lazy TLB (FX-0009) and every shootdown reaching every ASID a space holds; break-before-make on remaps. |
| `IBPB` between domains | `ARCH_WORKAROUND_1` (SMCCC) on an address-space switch where the core lacks `CSV2` and firmware says it needs it (`aarch64/speculation.rs`). | `BPIALL` (A8, A9, A12, A17) or `ICIALLU` (A15) on switch (`armv7a/speculation.rs`). | The speculation domain skips them exactly as it skips `IBPB`. Cortex-A72 under QEMU has no firmware to call, so the reference machine pays nothing; cost on hardware unmeasured. |
| Return-stack refill (A2), ERAPS | none | none | Ferrix has no return-stack stuffing on Arm. Arm's per-entry cost instead is the **Spectre-BHB loop** on every entry from EL0 (8 branches on Cortex-A72; `ECBHB` cores skip it): budget it where x86 budgets the refill. |
| `SYSCALL`/`SYSRET`, entry hardening | `SVC`/`ERET`, the vector table, `x0`-`x30` saved | `SVC`, banked registers, `r0`-`r14` | §9.7 condition 2's table of entry measures has to be rewritten per ISA. |
| Step 4, direct switch and fast path | Part 1 (`hand_over`) and the park are arch-neutral; the entry insertion point and the frame tail (return words to `x0`-`x3`) are not. | Same, return words to `r0`-`r3`. | §9.7 answer 13: the park is compiled for x86-64 only if that touches just its own sites, otherwise argued coverage on Arm. An Arm fast path is a new design for the consultant. |
| `arm_timer`'s APIC reprogram exit | The generic timer's compare register is written without a trap under KVM **(guessed)**. | Same. | If so, the p90 tail of x86 has no Arm equivalent; measure. |
| Counter for the bench | `CNTVCT_EL0`, readable by EL0; no `isb` before the read yet (the analogue of `bench-exact`'s `lfence`). Resolution is `1/CNTFRQ`: 16 ns at 62.5 MHz **(guessed for the target hardware; read `CNTFRQ_EL0`)**. | `counter()` returns `None`: EL0 may not read the counter yet. | **ARMv7-A cannot run `bench-ipc` until the kernel sets `CNTKCTL.PL0VCTEN`** -- a kernel change inside the item. |

**Where to measure on Arm** (each needs the product owner's OK before the
device is touched, with the address list; never a persistent write):
the Pixel 7 runs Ferrix's AArch64 image in crosvm under KVM (Tensor G2:
Cortex-X1, A78, A55; no SVE) **(argued from the self-hosting work)**; the
STM32MP157D-DK1 is ARMv7-A, two Cortex-A7 at up to 800 MHz; test ARMv7-A at
`--smp 2` as the board has two cores. seL4 needs the same treatment there
as §9.6 gave it on x86-64 before a ratio means anything: built matched for
the same core, run alternately.

**Order for a port:** (1) make the counter readable and exact (ARMv7-A
kernel change; AArch64 `isb`), so a figure exists; (2) take the baseline on
the hardware with `--record`; (3) the runtime stubs' clobbers, then 3a's
reset; (4) ASIDs, with the lazy TLB; (5) 3b on ARMv7-A; (6) the profile on
hardware, then step 4's port as its own design. Each item change goes to the
consultant first.

## 7. Handing a result back

To the product owner (and to the consultant for an item change), in one
message:
- the branch, its tip, and what changed, one line a piece;
- the record's path under `docs/hotpaths/results/ipc-round-trip/`, the
  ABAB line (figure, base figure, median ratio, spread, rounds, load), and
  the log's path;
- the gate logs and every control with its verdict, by `gate.sh` tag;
- the consultant's ledger line, or "not yet reviewed";
- the update to this skill's data file: a new or changed `tried` entry with
  `evidence` and `source`, and the budget rows the change moves. A dead end
  goes in too, with what was measured and why it is dead.

Do not push and do not land without the product owner; publishing results
outside this repository is the customer's decision (`docs/HOTPATHS.md` §7).
