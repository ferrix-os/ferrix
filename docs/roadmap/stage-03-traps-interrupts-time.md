# Stage 3 — Traps, interrupts, time ✅

x86-64: GDT, TSS, IST stacks, IDT, exception handlers, LAPIC, IOAPIC, HPET/TSC
deadline. AArch64: `VBAR_EL1` vector table, synchronous/IRQ/FIQ/SError
handlers, GICv2 and GICv3, the architected generic timer.

Both behind one facade: `irq::register`, `timer::after`, `arch::TrapFrame`.

**Done — the trap half.** Vectors are installed on both architectures as the
first thing after the stage 1 self-check, before anything can deliberately
fault. That ordering is not fastidiousness: until it runs the CPU is still
pointing at firmware's handlers, and those stopped existing at
`exit_boot_services`. A fault in that window is a jump into reclaimed memory,
which on x86-64 is a triple fault and a silent reset with nothing on the wire to
say why.

* x86-64 — GDT, TSS and a 256-entry IDT, every gate naming the kernel code
  selector the GDT installed a moment earlier. The double-fault gate runs on an
  IST stack of its own, because it is the one fault whose cause may be that the
  stack is unusable. So do the NMI, the debug exception and the machine check,
  on three more, because they arrive whether or not the kernel is on a stack of
  its own, and the `SYSCALL` trampoline's first and last instructions run in
  ring 0 on the program's stack: IST 1 the double fault, 2 the NMI, 3 `#DB`, 4
  `#MC`, sixteen kibibytes each, static on the boot processor and from the
  vmap arena, guard pages and all, on every other.
* x86-64 — those four take a *paranoid* entry (`arch/x86_64/paranoid.rs`),
  because the ordinary stub's rule, swap `GS` when the saved `CS` says ring 3,
  is wrong on the trampoline's ring-0 instructions that hold the program's
  `GS`. The entry reads `GS_BASE` instead, as Linux's `paranoid_entry` does:
  a kernel base is an upper-half record and no program can load one, so it
  swaps only for anything else and remembers to swap back. That rule needed a
  fix of its own: `syscall::init` had parked the kernel's record in the shadow
  MSR, so every program ran with the record as its `GS` base, both halves of
  `swapgs` held one address, and a wrong swap was invisible — the first boot
  of an entry that decided by `CS` passed the breakpoint check. A program now
  starts with `GS` base zero, as on Linux, and the check requires it. It clears `DR7` for
  the handler's run, which is what keeps `#DB` from nesting on its own stack
  (Linux's scheme since it retired the IST shift), counts each stack's
  occupants so a nested entry is reported as FX-9006 instead of returned
  from, and moves a ring-3 `#DB` onto the task's stack, where a signal may
  block or end the task. An NMI is counted and returned from; its handler
  takes no exception that could `iretq` early and let a second NMI in. A
  kernel `#DB` from a hardware breakpoint returns with `RF`; `#MC` stops the
  machine as FX-9005, on its own stack and the kernel's `GS`. A boot check
  after the trap flag check shows it (`paranoid::check`): an NMI sent to the
  processor itself with interrupts masked comes back; instruction breakpoints
  on the trampoline's `swapgs` and its `sysretq`, while a pinned program makes
  its calls, each find the kernel's `GS`, and the program's own `GS` base must
  not be a per-CPU record; and a breakpoint on code the `#DB` handler runs
  does not fire inside it. Each part stops the boot with its fix taken out,
  under tcg and kvm alike — tcg implements the debug registers and delivers
  the NMI.
* AArch64 — the `VBAR_EL1` vector table and its handlers.
* One dispatch path above both (`src/kernel/src/trap.rs`), reached only through the
  architecture facade — `TrapFrame`, `classify`, `report_trap` — so generic code
  still never names a CPU.

**Exit criterion, half met, and in the boot test on both architectures.** A
breakpoint returns to the instruction after it, twice, with a canary in a
register the trap frame saves and restores required to come back intact — which
is what proves the entry path *restores* rather than merely arrives. Then a page
fault is resolved rather than reported: three pages in an unmapped window are
touched out of order, the handler maps the faulting address, the instruction
retries, and the pages are required to read back what was written, to be zeroed,
and to have cost exactly the frames they should — one per page plus one per
table level for the first fault into a fresh region, and exactly one for a fault
into a region already tabled. That last bound is what makes the accounting a
measurement rather than a shrug.

Resolving a fault by mapping a page and retrying *is* demand paging, arriving
here rather than at stage 6 because it is the only honest way to show the fault
path recovers. Stage 6 inherits it instead of writing it.

**Done — the interrupt and time halves.** Something now arrives that the
kernel did not ask for at the moment it arrives, which is the difference
between a program and an operating system.

* x86-64 — the local APIC is mapped from the MADT's address, enabled, and its
  task priority dropped to accept everything; the local APIC timer is
  calibrated against the HPET, whose period firmware states exactly in
  femtoseconds so that nothing has to be measured. A machine with no HPET
  table falls back to the TSC calibrated against the PIT, which is the only
  clock a PC is guaranteed to have. An HPET whose main counter is 32 bits
  wide — capability bit 13, and common on AMD chipsets — wraps every five
  minutes, so it is not handed out as the counter: the TSC is calibrated
  against it, subtracting in 32 bits, and used instead. QEMU's HPET is
  64-bit, so the boot test does not reach that path; `src/lib/platform/acpi`'s `hpet`
  module holds its arithmetic and its tests. Since 2026-09-19 an invariant
  TSC (`CPUID.80000007H:EDX[8]`) is the counter whenever the processor has
  one, measured against the HPET, because every HPET read is a device access
  and costs an exit under a hypervisor; QEMU's `qemu64` model has none, so
  the boot test still counts on the HPET. Since 2026-10-10 a guest whose
  processor model hides that bit (libvirt's `host-passthrough`) takes the
  TSC on KVM's word instead, where KVM's paravirtual clock says it is
  stable, at the rate that clock states; and a boot that does end on the
  HPET says so, with the reason, in a warning beside its `clock` line
  (`docs/CLOCK.md`). Every I/O APIC firmware described is
  mapped and every input masked — not configured, because nothing is wired to
  a device line until stage 10, but quiesced, because a line firmware left
  enabled would arrive at a vector chosen by whoever wrote the firmware.
* AArch64 — GICv2's distributor and CPU interface, from the MADT; the
  architected *virtual* timer, whose interrupt number comes from the GTDT.
  Virtual rather than physical because a kernel at EL1 is by definition below
  any hypervisor present, and on bare metal the two are the same counter.
* The other two thirds of the facade: `irq::register` and `timer::after`.
  One-shot is the primitive, because AArch64's timer has no periodic mode at
  all — it compares against an absolute instant — and stage 5's tickless
  scheduler wants one-shot anyway. Periodic is a re-arm inside the handler.

**Exit criterion met, and in the boot test on all three architectures.** The
timer interrupts are counted — 250 of them, a quarter of a second at the
kilohertz asked for; it was a thousand, and a second per boot per
architecture bought nothing the quarter does not — and the time they took is
measured with the *counter* rather than by multiplying the tick count by the
rate they were programmed at, which would be arithmetic that cannot fail
rather than a measurement. Against a requested 1000 Hz, all three report 998
to 999. It is a measurement, so it moves.

It used to report 969 to 992, and the shortfall was written up here as one
interrupt entry and exit per period under an emulator. That was the wrong
diagnosis of a real defect. The handler re-armed the timer for one interval
*from the moment it ran*, so every period was one interval plus however long
that interrupt had taken to arrive — an error that never averaged out, because
it was added afresh each time. On a fast host it hid inside the tolerance. On
QEMU's Windows build, whose timer resolution is about a millisecond, it
doubled the period outright and the self-check failed: 500 Hz against 1000
requested, which is what a kernel that measures its own latency and calls it a
frequency looks like.

A periodic timer is now a *schedule*: tick `n` is due at `start + n * interval`
and the instant tick `n - 1` happened to arrive has no say in it, so a late
tick is absorbed rather than propagated. Unbounded catch-up is its own hazard —
a kernel held off for a second owes a thousand interrupts — so past sixteen
intervals behind, the debt is written off and the schedule restarts. This is
also the shape stage 5 wants: a deadline, not a delay.

Before that, a one-shot is armed and the count required not to move for ten
further intervals. That check is there for one specific bug: AArch64's timer
interrupt is level triggered, so a handler that acknowledges the controller
without disarming the timer is re-entered immediately and forever. It does
not fail by producing a wrong number — it fails by never returning, with
nothing in the log after the line before it. On x86-64 the count cannot move
at all, because the local APIC's one-shot never refires. The mistake that would
make it refire, a timer programmed periodic, was tried there: it hangs the boot
before the check reports, and the boot test's timeout catches it instead.

The boot marker reads `FERRIX-BOOT-OK stages 1-3`, and it is now the whole
truth.

**Deferred, and none of it is reachable from a machine Ferrix boots on
today.** The first two are hardware variants rather than gaps in the stage:
the facade above them does not change, and neither is on any later stage's
path. The third is stage 4's by definition.

* GICv3 and its redistributors. `gic::init` reads the version out of the
  MADT and refuses anything that is not a GICv2, rather than writing GICv2
  layouts into GICv3 registers and producing a machine that takes no
  interrupts for reasons nothing explains. QEMU's `virt` gives a GICv2 unless
  asked otherwise, so this needs a second boot-test configuration as much as
  it needs code.
* TSC-deadline mode, which replaces the local APIC timer's countdown with a
  comparator against the TSC and is how a tickless kernel avoids
  reprogramming a divider on every reschedule. The calibration it needs is
  already here.
* Per-CPU anything. The controller is brought up for the boot CPU because
  there is only one; stage 4 is where a redistributor per core and a local
  APIC per core start to mean something. *Done in stage 4*: every processor
  brings up its own local APIC or GIC CPU interface.

