# The x86-64 clock: which counter, and who vouches for it

Version 1. Written on 2026-10-10, the day a study of a software-GL game
found the kernel running on the HPET under a hypervisor without saying so
(`~/.local/share/ferrix/steam-race/softgl-gap.md` on the gate host). The
code is `src/kernel/src/arch/x86_64/clock.rs`; its requirements are
`L.x86_64.38` to `L.x86_64.41` and `L.x86_64.166` to `L.x86_64.169`
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

What the flag does not promise is the two things the invariant bit rules
out by forbidding migration:

* **Migration to a host of another TSC rate.** QEMU carries the guest's
  TSC rate along and KVM scales the TSC on a processor that has TSC
  scaling; on a host that has none, the TSC runs on
  at the new host's rate and the time page's parameters change to say so.
  A kernel that took the rate once, at boot, then runs fast or slow.
* **The host losing its TSC** (its watchdog marking it unstable). KVM
  clears the flag; a kernel that read it once does not hear of it.

Ferrix reads the rate once, with the invariant bit and without it: nothing
in the kernel or the vDSO data page can change a rate after boot. So the
second and third rows of §3 state that limit rather than hide it, the boot
line names KVM as the source of the trust, and the remedy for both cases is
the full paravirtual clock of §7, which is not built here.

## 3. The order, as built by this change

`clock::init` takes the first that holds:

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
   an emulated HPET costs are not paid. Line: `clock TSC, stable by KVM's
   word, at its paravirtual clock's rate`. This comes before the HPET and
   before the PIT: a virt-manager domain often has `hpet present='no'`, and
   KVM's stated rate is better than one measured through an emulated PIT.
3. **An invariant TSC and no HPET**, or **no HPET at all**: the TSC
   measured against PIT channel 2. Unchanged, including that the second
   case asks nobody whether the TSC is a clock; with no other counter there
   is no alternative to it. Line: `clock TSC`.
4. **A 64-bit HPET**: the HPET. Unchanged as a counter, and now said aloud
   (§5). Line: `clock HPET`, then a second line.
5. **A 32-bit HPET**: the TSC measured against it. Unchanged.

The time page is 32 bytes of the kernel's own static data, aligned to 32
so that it cannot cross a page. It is registered by writing its physical
address with bit 0 set, read, and given back by writing 0 before `init`
returns, on every path: the kernel leaves no memory a hypervisor writes
into on its own schedule. KVM fills the structure before the guest runs
on after the registering write; the read follows KVM's protocol all the
same (the version is read before and after, and an odd or
changed version means read again, at most 1,000 times, then give up and
fall through to the next source). A page whose version is still 0 after
the write was never filled and vouches for nothing.

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

When row 4 is the answer the boot prints a second `clock` line with the
reason and the cost, for example under plain TCG:

```
  clock    HPET at 100.000 MHz
  clock    WARNING: the HPET is the clock, three device reads a reading and no vDSO clock (every clock_gettime is a system call): CPUID reports no invariant TSC and the hypervisor (TCGTCGTCGTCG) does not vouch for it. Under KVM that is about 22 us a reading against 20 ns; give the guest invtsc (QEMU: -cpu ...,migratable=off,+invtsc; libvirt: migratable='off' and <feature policy='require' name='invtsc'/>)
```

The reason is one of: no invariant TSC on bare metal (no hypervisor bit);
a hypervisor, named by the twelve bytes of leaf `4000_0000h`, that does not
vouch; KVM that offers a paravirtual clock without the stable bit or with a
time page that never said stable (the host's own TSC is not its clock
source). `clock::hpet_reason` answers the reason exactly when the HPET is
the counter, so the line cannot be printed for a TSC or left out for an
HPET.

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

| Id | Shall | Check |
|---|---|---|
| `L.x86_64.38` (reworded) | `clock::init` brings up a 64-bit counter and its rate in the order of §3 | baselined, as before |
| `L.x86_64.166` | `choose` answers the source for a machine described: the TSC by measurement with an invariant TSC, the TSC on KVM's word when the bit is absent and KVM vouches in full, the HPET only when neither holds and a 64-bit HPET exists | every boot: a table of described machines, each with the source it must get (`clock::check::sources_decided`) |
| `L.x86_64.167` | `pvclock_hz` answers `10^9 * 2^32 / (mul * 2^shift)` rounded down, and nothing for a zero multiplier, a shift outside -31..=31 or a rate outside 1 MHz..=2^40 Hz | every boot: computed vectors, the refusals among them (`clock::check::pvclock_rates`) |
| `L.x86_64.168` | `kvm_time_page` registers the kernel's time page, reads it whole (version even, non-zero and unchanged across the read), and has given it back when it returns, whatever it read | under KVM: after `init`, a fresh read answers a page and `MSR_KVM_SYSTEM_TIME_NEW` reads 0; elsewhere the check says it did not run (`clock::check::time_page_given_back`) |
| `L.x86_64.169` | `hpet_reason` answers a reason exactly when the counter is the HPET, and on this boot the counter is not the HPET when the processor has an invariant TSC or KVM vouches | every boot: the live machine's facts against the counter in use (`clock::check::counter_matches_machine`) |

Negative controls, each a `gate.sh control` whose panic line is recorded in
the commit: row 2 taken out of `choose` (166, any boot; and 169 under KVM
with `FERRIX_X86_CPU=host`), the shift's sign reversed in `pvclock_hz`
(167), the give-back write removed (168, under KVM), and `hpet_reason`
answering nothing (169, under TCG).

Gates: the image row, x86_64 under KVM and TCG, plus one boot the row does
not have: `FERRIX_X86_CPU=host cargo xtask test-boot --arch x86_64 --accel
kvm`, the card domain's processor line, which must print row 2's line and
pass the vDSO check on it.

## 9. Where it stands

* 2026-10-10: designed; ids `L.x86_64.166-169` reserved on branch
  `softgl-clock-reserve`. The certification consultant's design verdict is
  recorded below when given.
