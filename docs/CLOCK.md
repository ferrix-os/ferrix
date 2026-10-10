# The x86-64 clock: which counter, and who vouches for it

Version 1. Written on 2026-10-10, the day a study of a software-GL game
found the kernel running on the HPET under a hypervisor without saying so
(`~/.local/share/ferrix/steam-race/softgl-gap.md` on the gate host). The
code is `src/kernel/src/arch/x86_64/clock.rs`; its requirements are
`L.x86_64.38` to `L.x86_64.41` and `L.x86_64.166` to `L.x86_64.171`
(`docs/sysml/18-x86-64-requirements.sysml`). AArch64 and ARMv7-A read the
generic timer, whose rate the architecture states, and none of this applies
to them: the Pixel 7's crosvm and the DK1 are not affected.

## 1. What went wrong

The kernel has two counters to choose from. The TSC is read with one
instruction, in ring 3 as well, which is what lets the vDSO answer
`clock_gettime` without a system call. The HPET is a device: one reading is
three loads from its register window (§6). Until this change the rule was
one line: *the TSC if `CPUID.80000007H:EDX[8]` says it is invariant,
otherwise the HPET*.

Under a hypervisor that bit is the virtual processor model's, not the
silicon's. libvirt's `host-passthrough` leaves it out, because a guest
holding it cannot be migrated; so does a virt-manager domain made with the
defaults, and QEMU's `-cpu host` without `migratable=off,+invtsc`. On such
a guest the kernel took the HPET, which QEMU emulates in its own process:
every load is an exit to QEMU, under its one lock. Measured on nazuna under
KVM, 8 processors, host load 18 to 30:

| | boot line | `clock_gettime` | futex round trip, p50 | socket round trip, p50 |
|---|---|---|---|---|
| `-cpu host` | `clock HPET at 100.000 MHz` | 22,021 ns, a system call | 199 us | 206 us |
| `-cpu host,migratable=off,+invtsc` | `clock TSC, calibrated against the HPET` | 20 ns, in the vDSO | 6.7 us | 16.6 us |
| a Linux guest, `-cpu host` | `kvm-clock` | 20 ns | 12.7 us | 13.6 us |

Every futex wait, sleep, timer and scheduling decision read the clock, so
all of them paid. The game ran 2.7 times slower than on Linux in the same
virtual machine, and nothing in the boot log said why: `clock HPET at
100.000 MHz` reads as a statement, not as a warning.

Two faults, then. The kernel asked only the processor model whether the TSC
can be trusted, where the hypervisor itself says so by another route. And
when the HPET was the counter, the boot did not say what that costs.

## 2. What "stable" has to mean

The kernel's clock is `counter * 10^9 / hz` (`src/kernel/src/timer.rs`), and
the vDSO computes the same number from `rdtsc` and the frequency word of its
data page (`src/kernel/src/arch/x86_64/vdso.rs`, `src/lib/kernel/vdso`). The
data page holds a mode, a frequency and the real-time offset, each one word
written once at boot: there is no sequence count, no per-processor base and
no rate that changes. For that arithmetic to be a clock, the TSC must be

* **one timeline on every processor**: a thread that moves between two
  processors, or two threads comparing stamps, must not see time step back;
* **at one rate**, whatever the host's frequency scaling and idle states do;
* **never stopped** while the guest runs.

The invariant bit promises all three from the silicon. Under KVM the same
three are promised by the host instead, and the promise has a name: the
*stable* flag of KVM's paravirtual clock. KVM sets
`PVCLOCK_TSC_STABLE_BIT` in a guest's time page only while its *master
clock* is in use, which it is only when the host's own clock source is its
TSC (constant, non-stop and synchronised across the host's processors, or
the host would not use it) and every virtual processor of the guest has the
same TSC offset. That is the condition under which Linux's vDSO reads
`rdtsc` on any processor against one processor's parameters. The CPUID bit
`KVM_FEATURE_CLOCKSOURCE_STABLE_BIT` says the host will keep the flag
honest; a guest trusts the flag only with it.

What the flag does not promise is three things, two of which the invariant
bit rules out by forbidding migration:

* **Migration to a host of another TSC rate.** QEMU carries the guest's
  TSC rate along and KVM scales the TSC on a processor that has TSC
  scaling; on a host that has none, the TSC runs on
  at the new host's rate and the time page's parameters change to say so.
  A kernel that took the rate once, at boot, then runs fast or slow.
* **The host losing its TSC** (its watchdog marking it unstable). KVM
  clears the flag; a kernel that read it once does not hear of it.
* **A guest started with a TSC rate that is not the host's, on a host
  without TSC scaling** (libvirt's `<timer name='tsc' frequency=...>`,
  QEMU's `tsc-frequency=`). This one needs no migration. Asked for a faster
  TSC than it can scale to, KVM steps the guest's TSC forward to keep up
  while the time page goes on stating the host's rate with the stable flag
  set, so a clock built on that rate runs fast from the first second.

Ferrix reads the rate once, with the invariant bit and without it: nothing
in the kernel or the vDSO data page can change a rate after boot. So row 2
of §3 states that limit rather than hides it: the boot line names KVM as
the source of the trust, `docs/certification/SAFETY-MANUAL.md`'s AoU-28
tells an integrator what a guest with that line must not be asked to do,
and the remedy for all three cases is the full paravirtual clock of §7,
which is not built here.

## 3. The order, as built by this change

`clock::init` first opens the HPET where firmware describes one: maps its
window, starts it and proves that it counts, whichever counter is then
chosen. A dead HPET fails bring-up on every row, as before (`L.x86_64.39`
is unchanged), and the window stays mapped on every row: a mapped device
window is how the kernel knows the registers are its own and no driver's.
Then it takes the first that holds:

1. **An invariant TSC and an HPET**: the TSC, its rate measured over 10 ms
   of the HPET. Unchanged. Line: `clock TSC, calibrated against the HPET`.
2. **No invariant TSC, and KVM vouches for it** (new). All of: the
   hypervisor bit `CPUID.1:ECX[31]`; leaf `4000_0000h` reading
   `KVMKVMKVM\0\0\0` in EBX, ECX, EDX; leaf `4000_0001h` EAX with
   `KVM_FEATURE_CLOCKSOURCE2` (bit 3) and
   `KVM_FEATURE_CLOCKSOURCE_STABLE_BIT` (bit 24); and a time page,
   registered with `MSR_KVM_SYSTEM_TIME_NEW` (`4b564d01h`) and read whole,
   whose flags hold `PVCLOCK_TSC_STABLE_BIT` (bit 0) and whose multiplier
   and shift give a rate between 1 MHz and 2^40 Hz. The counter is the TSC
   at that rate, which is the rate KVM itself converts the TSC with:
   `10^9 * 2^32 / (tsc_to_system_mul * 2^tsc_shift)`. Nothing is measured,
   so an HPET is not needed, and the 10 ms of exits that measuring against
   an emulated HPET costs are not paid. Line: `clock TSC, vouched for and
   rated by KVM's paravirtual clock`. This comes before the HPET and
   before the PIT: a virt-manager domain often has `hpet present='no'`, and
   KVM's stated rate is better than one measured through an emulated PIT.
3. **An invariant TSC and no HPET**, or **no HPET at all**: the TSC
   measured against PIT channel 2. Unchanged, including that the second
   case asks nobody whether the TSC is a clock; with no other counter there
   is no alternative to it. Line: `clock TSC`.
4. **A 64-bit HPET**: the HPET. Unchanged as a counter, and now said aloud
   (§5). Line: a warning, then `clock HPET`. An invariant TSC whose
   measurement against the HPET did not come off ends here too, as before,
   and KVM is not asked on its behalf.
5. **A 32-bit HPET**: the TSC measured against it. Unchanged, but for one
   retry: a processor with an invariant TSC and a 32-bit HPET used to
   measure twice before giving up and now measures once.

Rows 3 and 5 still hand out a TSC nobody vouches for and say nothing of it,
even where KVM's flag was read and found clear: with no HPET, or only one
that wraps every few minutes, there is no other counter to prefer.
`docs/BACKLOG.md` has the row.

Leaf `4000_0001h` is read only under the hypervisor bit and KVM's
signature, and only where leaf `4000_0000h`'s EAX, the hypervisor's highest
leaf, is at least `4000_0001h` or is zero, which KVM before Linux 3.x
answered to mean the same.

The time page is 32 bytes of the kernel's own static data, aligned to 32
so that it cannot cross a page. It is registered by writing its physical
address with bit 0 set, read, and given back by writing 0 before `init`
returns, on every path: the kernel leaves no memory a hypervisor writes
into on its own schedule. The register is one for each virtual processor,
so interrupts are masked from the registering write to the give-back:
nothing preempts the task between them or moves it to a processor whose
give-back would leave the first one's page registered. KVM fills the structure before the guest runs
on after the registering write; the read follows KVM's protocol all the
same (the version is read before and after, and a zero,
odd or changed version means read again, at most 1,000 times, then give up
and fall through to the next source). A page whose version is still 0
after the write was never filled and vouches for nothing. A registering
write that faults is a kernel trap, a panic naming the instruction, never
a fall-through: it is made only under `KVM_FEATURE_CLOCKSOURCE2`, under
which KVM accepts it.

All of row 2 is refused, and the order falls through, when any one of its
conditions fails. It is tried only when the invariant bit is absent, so
every boot that reaches row 1 today reaches it still, with the same line
and the same measured rate: the gate's KVM boot (`qemu64,+invtsc`), WHPX as
xtask starts it (`+invtsc`, `docs/GUIDE.md`), and bare metal.

The vDSO needs no change. `counter_is_tsc` is true on row 2 as on rows 1,
3 and 5, the data page gets `MODE_TSC` and the rate, and
`syscall/vdso_check.rs`'s program (`L.x86_64.41`) holds every clock the
vDSO answers between two system calls on that boot as on any other.

## 4. Other hypervisors

* **TCG** (`TCGTCGTCGTCG`): QEMU refuses `invtsc` under TCG and offers no
  paravirtual clock. The counter stays the HPET, as the coverage suite and
  the TCG gate boot it today, and the boot now says so. A TCG boot's
  HPET reads are function calls inside QEMU, not exits.
* **WHPX** (the customer's Windows machine, `docs/GUIDE.md`): xtask gives
  the guest `+invtsc`, so it is row 1 and unchanged.
* **Hyper-V proper** (`Microsoft Hv`, a Hyper-V or Azure virtual machine
  not started by xtask): Hyper-V hides the invariant bit until the guest
  asks for it by writing `HV_EXPOSE_INVARIANT_TSC` (bit 0) to
  `HV_X64_MSR_TSC_INVARIANT_CONTROL` (`40000118h`), which it may when leaf
  `4000_0003h` EAX has `HV_ACCESS_TSC_INVARIANT` (bit 15), and states the
  rate in `HV_X64_MSR_TSC_FREQUENCY` (`40000022h`) under
  `HV_ACCESS_FREQUENCY_MSRS` (bit 11). **Not built**: no QEMU on the gate
  host (9.2.4, 10.2.1) emulates the invariant control, so no boot here can
  show it working, and an unshown write to a model-specific register does
  not go into the item. `docs/BACKLOG.md` has the row. Until then such a
  guest gets row 3 or 4 and, on row 4, the line of §5 naming `Microsoft Hv`.
* **KVM with Hyper-V enlightenments on** moves KVM's leaves to
  `4000_0100h`. Row 2 looks only at `4000_0000h`, so such a guest is a
  Hyper-V guest to this kernel, as above.
* **Any other** (VMware, Xen, bhyve): row 1 if they pass the bit, else as
  before, with the line of §5 naming them.

## 5. The HPET said aloud

When row 4 is the answer the boot prints a warning beside the `clock`
line, with the cost and the reason, for example under plain TCG:

```
  clock    WARNING: the HPET is the clock, three device reads a reading, and no clock in the vDSO (every clock_gettime is a system call): CPUID reports no invariant TSC, under a hypervisor (TCGTCGTCGTCG) which offers nothing this kernel can ask; give the guest invtsc (QEMU: -cpu ...,migratable=off,+invtsc; libvirt: migratable='off' and <feature policy='require' name='invtsc'/>)
  clock    HPET at 100.000 MHz
```

The line says only what this boot knows. The reason is one of: no
invariant TSC and no hypervisor; an invariant TSC that could not be
measured; a hypervisor, named by the twelve bytes of leaf `4000_0000h`,
that is not KVM; and KVM that offers no paravirtual clock, no stable bit, a
time page that could not be read, one that does not say stable (the host's
own TSC is not its clock source), or one stating a rate no TSC has. The
hypervisor's bytes are its own, so they are printed as text only when
every one is a printable character and in hexadecimal otherwise: nothing a
hypervisor answers can end the line and forge the next. The remedy is
printed only under a hypervisor; on bare metal there is none to give. What
a reading costs under KVM, 22 us against 20 ns, is §1's measurement and
stays in this document. `clock::hpet_reason` answers the reason exactly
when the HPET is the counter (`L.x86_64.170`), so the line cannot be
printed for a TSC or left out for an HPET.

Every x86-64 boot under TCG gains this line, and one more from the checks:
`clock no KVM paravirtual clock here: its time page's registration and
give-back were not run`. A boot under KVM gains `clock KVM's time page read
and given back: multiplier ..., shift ..., stable, ... Hz stated against
the counter's ...` instead. No other boot line changes; the Arm boots have
none of this.

## 6. The HPET's three reads stay three

The study suggested one 64-bit load where `main_counter` makes three 32-bit
ones (high, low, high again). Checked, and not done:

* On hardware a 64-bit load of a 64-bit main counter is one atomic read
  (IA-PC HPET specification 1.0a, §2.4.7).
* QEMU serves a 64-bit load whole only since 9.1 (c2366567378d, "hpet:
  accept 64-bit reads and writes", 2024-07-10). Before it, QEMU's memory
  core splits the load into two 32-bit reads taken at two moments, low
  then high. QEMU's HPET counts at 100 MHz, so its low half carries every
  43 s, and a read split across a carry is 2^32 ticks, 43 s, ahead. Ubuntu
  24.04 ships QEMU 8.2.
* The kernel cannot tell which it runs on.

So the cheaper read is not safe everywhere the three-read one is, and the
gain (22 us to about 7) is no longer the point once the HPET is the last
resort and says so.

## 7. Not built: the full paravirtual clock

Reading KVM's time page on every reading of the clock (kernel and vDSO),
instead of taking its word once at boot, would keep the clock right across
a migration to a host of another rate and across the host losing its TSC
(§2). It needs a time page per processor registered at each one's
bring-up, a data page the vDSO reads with a sequence count, a `MODE_PVCLOCK`
beside `MODE_TSC` in `src/lib/kernel/vdso`, a system-call fallback when the
stable flag drops, and a kernel clock that is no longer `counter * 10^9 /
hz`: a change to `timer.rs`'s contract with all three architectures.
`docs/BACKLOG.md` has the row, with the migration case as its reason. Until
it is built, a Ferrix guest on row 2 must not be live-migrated to a host
without TSC scaling; on rows 1, 3 and 5 it could never be.

## 8. Requirements and checks

`L.x86_64.38` is reworded to the order of §3 and stays baselined, its
criterion unchanged. `L.x86_64.39` to `41` are unchanged. The checks are
`src/kernel/src/arch/x86_64/clock/check.rs`, run with stage 3's.

| Id | Shall | Check |
|---|---|---|
| `L.x86_64.166` | `choose` answers the counter for a machine described, and `kvm_word` vouches only with every condition of row 2, refusing with the first that fails | every boot: nine machines, the unmeasurable 32-bit HPET, and each of row 2's seven conditions failing alone (`sources_decided`) |
| `L.x86_64.167` | `pvclock_hz` answers `10^9 * 2^32 / (mul * 2^shift)` rounded down, and nothing for a zero multiplier, a shift outside -31..=31 or a rate outside 1 MHz..=2^40 Hz | every boot: 15 pairs worked out by hand, nazuna's own page among them (`pvclock_rates`) |
| `L.x86_64.168` | `kvm_time_page` registers the time page with interrupts masked and has given it back when it returns | every boot under KVM, whichever counter was chosen: the register reads 0 after bring-up and 0 again after one more read, whether or not it answered a page (one that could not be read is said in a `clock` line and fails nothing), and an answered page's stated rate is within a third of the counter's (`time_page_given_back`) |
| `L.x86_64.169` | `read_time_page` reads the version first and last, looks again on zero, odd or changed, at most 1,000 times, and decodes word 6, byte 28 and byte 29 | every boot: scripted pages (`time_page_read_whole`) |
| `L.x86_64.170` | `hpet_reason` answers a reason exactly when the counter is the HPET | every boot (`reason_only_for_the_hpet`) |
| `L.x86_64.171` | the HPET is not the counter where KVM vouches in full, nor with an invariant TSC unless its measurement failed | every boot, from the machine's own CPUID and time page read again (`counter_matches_machine`) |

The third of `L.x86_64.168`'s tolerance: a formula wrong by a factor of two
states twice or half the rate and fails; the counter's own rate on the
standing KVM boot is measured over 10 ms of an emulated HPET, and to be a
third out the virtual processor would have to lose 3.3 ms between two
adjacent instructions of that measurement. The boots of 2026-10-10, at a
host load of 29 to 35, read 4,400.035 and 4,399.991 MHz against the
stated 4,400.023.

Negative controls, each a `gate.sh control` whose line is in the commit
that built this: row 2 taken out of `choose` (166, any boot); the shift's
direction reversed in `pvclock_hz` (167, any boot); the give-back write
registering again (168, the standing KVM boot); the changed-version test taken out
of `read_time_page` (169, any boot); `hpet_reason` answering nothing (170,
under TCG); and `init` not asking KVM (171, under KVM with
`FERRIX_X86_CPU=host`).

Gates: the image row, x86_64 under KVM and TCG, plus one boot the row does
not have: `FERRIX_X86_CPU=host cargo xtask test-boot --arch x86_64 --accel
kvm`, the card domain's processor line, which must print row 2's line and
pass stage 3's timer check and the vDSO's on it. Only that boot reaches
row 2's arm of `init`; `docs/certification/COVERAGE-RESIDUAL.md` says so,
and `docs/BACKLOG.md` asks the product owner for a standing boot of it.

## 9. Where it stands

* 2026-10-10: designed. The certification consultant's design verdict
  (softgl-clock-cert, ledger line 665): **OK IF** D1 to D18. In short: the
  third residual and an assumption of use for all three (D1, D2); the
  backlog rows in the landing (D3); 38 spelled out and 39 decided (D4, D5);
  the time page's requirement split into registration and read, the
  HPET's into reason and live machine, six ids (D6 to D8); the tables'
  rows (D9, D10); the live checks keyed on the processor's facts so the
  standing KVM boot runs them (D11); interrupts masked across the
  registration (D12); the one-off boot and every control through `gate.sh`
  (D13); the coverage residual and evidence files (D14, D15); the loads'
  order, the registering write's safety argument and the leaves' order
  (D16); the warning line printing only what the boot knows (D17); the full
  `check` after the new boot lines (D18).
* 2026-10-11: the consultant's code verdict (softgl-clock-cert, ledger
  line 694): **OK IF** E1 to E4. The coverage evidence is exactly what
  `carry-coverage.py` writes, the new code entered in
  `docs/certification/coverage-owed.json` with its arguments (E1); a KVM
  whose time page cannot be read is not failed by the check that gives the
  page back (E2); the boot without `invtsc` re-run on the landing's kernel
  (E3); wording (E4).
* 2026-10-10: built as designed and conditioned. Ids `L.x86_64.166-171`
  written and their reservation released. The checks run from
  `arch::check_machine`, at stage 3 right after the timer's rate: run later
  they came after stage 13's `cpu.max` check, which a kernel on the HPET
  under KVM fails first. Measured on nazuna with the card domain's
  processor line (`-cpu host`, 8 processors, the study's tools), before at
  a host load of 18 to 25 and after at 54 to 64:

  | | boot line | `clock_gettime` | futex round trip, p50 | socket round trip, p50 |
  |---|---|---|---|---|
  | before | `clock HPET at 100.000 MHz` | 21,125 to 27,738 ns | 243 us | 198 to 212 us |
  | after | `clock TSC, vouched for and rated by KVM's paravirtual clock at 4400.023 MHz` | 21 to 70 ns (two of six runs 251 and 284 at load 60) | 2.6 and 9.5 us | 12.6 and 32.2 us |

  The same boot reaches the `clock` line 1.2 to 1.7 s after power-on where
  it took 3.5 s before, the HPET no longer being read.
