//! The free-running counter on x86-64.
//!
//! Two candidates, and the choice between them is about what a read costs.
//!
//! The **HPET** is a counter in the chipset with a period firmware states
//! exactly, in femtoseconds, in a table. Nothing has to be calibrated: the
//! frequency is read, not measured, which means it carries no error at all.
//! But it lives behind a device window, and [`main_counter`] reads it three
//! times over to see a 32-bit wrap untorn — so one reading of the clock is
//! three loads from memory-mapped I/O.
//!
//! The **TSC** is a counter in the CPU, read with one instruction and no bus
//! cycle at all. Nothing reports its frequency, so it has to be measured
//! against something else — but the HPET is exactly the something else worth
//! measuring against, and ten milliseconds of it is enough.
//!
//! So an **invariant** TSC is preferred, and the HPET is what it is measured
//! against. The frequency still comes from the one number firmware states
//! exactly; what changes is that reading the clock afterwards costs a register
//! read rather than three trips to a device. On real hardware that is twenty
//! nanoseconds against a microsecond. Under a hypervisor it is the difference
//! between a trap into an instruction emulator and no trap whatever, and the
//! kernel reads this clock twice on every timer tick: at a kilohertz, under
//! `WHPX`, the HPET alone was six thousand virtual-machine exits a second, and
//! stage 3 measured the timer running at a quarter of the rate it asked for.
//!
//! *Invariant* is the whole of the condition. Without that bit the TSC slows
//! with the core and stops in the deeper sleep states, which makes it a cycle
//! counter and not a clock; then the HPET, slow as it is to read, is the only
//! honest answer. A machine with no HPET table at all falls back to measuring
//! the TSC against the PIT, a part designed in 1981.
//!
//! Either way the answer is a counter and a frequency, and the rest of the
//! kernel is told neither which one it got nor that there was a choice.
//!
//! # The HPET raises nothing
//!
//! The HPET is only ever read here, as a counter: no comparator is armed and
//! no FSB message programmed, so under interrupt remapping it needs no entry
//! of its own (`docs/NVIDIA.md` §12.3). An FSB message firmware left armed
//! would be in compatibility format, and a unit that remaps blocks it.
//!
//! # A 32-bit HPET
//!
//! The HPET's main counter may be 32 bits wide — capability bit 13 says, and
//! AMD chipsets commonly are — and at 14.3 `MHz` that wraps every five minutes.
//! Handed out as the time base, it would send `CLOCK_MONOTONIC` backwards at
//! each wrap. Extending it in software needs a read at least once per wrap
//! period, and a processor running one task with its timer stopped makes none.
//! So a narrow HPET is not the counter: it is the reference the TSC is
//! calibrated against, over ten milliseconds and subtracted in 32 bits, which
//! keeps its exactly-stated period and costs the PIT nothing. Every counter
//! this module hands out is therefore 64 bits wide.

use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};

use ferrix_acpi::hpet::{self, Capabilities};
use ferrix_acpi::{Acpi, GenericAddress};

use super::cpu;
use crate::discovery::acpi::DirectMap;
use crate::mmio::Mmio;

pub(super) mod check;

/// General configuration.
const HPET_CONFIG: u64 = 0x010;
/// The main counter.
const HPET_MAIN_COUNTER: u64 = 0x0F0;
/// General configuration: start counting.
const HPET_CONFIG_ENABLE: u64 = 1 << 0;
/// Bytes of register window a timer block occupies.
const HPET_WINDOW: u64 = 0x400;

/// The 8254's input frequency: 105/88 `MHz`, fixed since the PC/AT and the one
/// number on a PC that has never changed.
const PIT_HZ: u64 = 1_193_182;
/// Channel 2's counter port — the one channel whose gate software controls.
const PIT_CHANNEL2: u16 = 0x42;
/// The mode/command register.
const PIT_COMMAND: u16 = 0x43;
/// Port 0x61: channel 2's gate in bit 0, its output in bit 5.
const PIT_GATE: u16 = 0x61;
/// Channel 2, lobyte then hibyte, mode 0, binary.
const PIT_ONESHOT: u8 = 0b1011_0000;
/// Port 0x61, bit 5: channel 2's output, which goes high at terminal count.
const PIT_OUTPUT: u8 = 1 << 5;
/// Port 0x61, bit 0: channel 2's gate. Bit 1 is the speaker, left off.
const PIT_GATE_ON: u8 = 1 << 0;
/// Milliseconds to measure the TSC over.
const CALIBRATION_MILLIS: u64 = 10;

/// Virtual address of the HPET's registers, or zero when the TSC is in use.
static HPET: AtomicU64 = AtomicU64::new(0);

/// How fast the chosen counter counts.
static COUNTER_HZ: AtomicU64 = AtomicU64::new(0);

/// Why the HPET is the counter, as [`HpetReason::code`] numbers it, or zero
/// while it is not.
static HPET_REASON: AtomicU8 = AtomicU8::new(0);

/// `CPUID.1:ECX[31]`: running under a hypervisor.
const CPUID_HYPERVISOR: u32 = 1 << 31;
/// The hypervisor's first leaf: its highest leaf in EAX, its name in EBX,
/// ECX and EDX (`KVM_CPUID_SIGNATURE`).
const HYPERVISOR_LEAF: u32 = 0x4000_0000;
/// KVM's feature leaf (`KVM_CPUID_FEATURES`).
const KVM_FEATURE_LEAF: u32 = 0x4000_0001;
/// `KVMKVMKVM\0\0\0`, as leaf `4000_0000h` gives it in EBX, ECX and EDX
/// (`KVM_SIGNATURE`).
const KVM_SIGNATURE: [u32; 3] = [
    u32::from_le_bytes(*b"KVMK"),
    u32::from_le_bytes(*b"VMKV"),
    u32::from_le_bytes(*b"M\0\0\0"),
];
/// `KVM_FEATURE_CLOCKSOURCE2`: the paravirtual clock at its second pair of
/// registers, of which [`MSR_KVM_SYSTEM_TIME_NEW`] is one.
const KVM_FEATURE_CLOCKSOURCE2: u32 = 1 << 3;
/// `KVM_FEATURE_CLOCKSOURCE_STABLE_BIT`: the host keeps the time page's
/// stable flag honest.
const KVM_FEATURE_CLOCKSOURCE_STABLE: u32 = 1 << 24;
/// `MSR_KVM_SYSTEM_TIME_NEW`: where a guest tells KVM the physical address
/// of its time page, with bit 0 set to register it and as 0 to give it back.
const MSR_KVM_SYSTEM_TIME_NEW: u32 = 0x4b56_4d01;
/// `PVCLOCK_TSC_STABLE_BIT`, in the time page's flags.
const PVCLOCK_TSC_STABLE: u8 = 1 << 0;
/// Times [`read_time_page`] looks again before it gives up.
const TIME_PAGE_TRIES: u32 = 1000;
/// The slowest and the fastest TSC [`pvclock_hz`] believes: 1 `MHz`, below
/// which no x86-64 processor counts, and 2^40 Hz, a thousand `GHz`.
const PVCLOCK_HZ_RANGE: core::ops::RangeInclusive<u128> = 1_000_000..=(1 << 40);

/// What row 1 answers.
const BY_HPET: &str = "TSC, calibrated against the HPET";
/// What row 2 answers.
const BY_KVM: &str = "TSC, vouched for and rated by KVM's paravirtual clock";
/// What row 3 answers.
const BY_PIT: &str = "TSC";
/// The TSC at the rate `CPUID` leaves 0x15/0x16 report, for a machine whose
/// PIT did not answer: Alder Lake and later may gate the 8254 off entirely.
const BY_CPUID: &str = "TSC, at the rate CPUID leaf 0x15/0x16 reports";
/// What row 4 answers.
const THE_HPET: &str = "HPET";
/// What row 5 answers.
const BY_NARROW_HPET: &str = "TSC, calibrated against a 32-bit HPET";

/// KVM's time page: `struct pvclock_vcpu_time_info`, 32 bytes, as eight
/// words. The version is word 0, `tsc_to_system_mul` word 6, and word 7's
/// low byte is `tsc_shift` and its next the flags.
///
/// Atomics, because between [`MSR_KVM_SYSTEM_TIME_NEW`]'s two writes the
/// hypervisor stores into these words as another processor would. Aligned to
/// its own size, so that it cannot cross a page: KVM is given one physical
/// address and writes 32 bytes from it.
#[repr(C, align(32))]
struct TimePage([AtomicU32; 8]);

/// The one time page, registered only inside [`kvm_time_page`].
static TIME_PAGE: TimePage = TimePage([const { AtomicU32::new(0) }; 8]);

/// What a time page said, of the fields this kernel uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct TimeInfo {
    /// `tsc_to_system_mul`.
    pub(super) mul: u32,
    /// `tsc_shift`.
    pub(super) shift: i8,
    /// The flags, [`PVCLOCK_TSC_STABLE`] among them.
    pub(super) flags: u8,
}

/// What the processor says of a hypervisor's clock, gathered by
/// [`kvm_facts`] and judged by [`kvm_word`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct KvmFacts {
    /// `CPUID.1:ECX[31]`.
    pub(super) hypervisor: bool,
    /// Leaf `4000_0000h`'s EBX, ECX and EDX, or zeros with no hypervisor.
    pub(super) signature: [u32; 3],
    /// Leaf `4000_0001h`'s EAX where the signature is KVM's and the leaf
    /// exists, or zero.
    pub(super) features: u32,
    /// The time page, where the features offer one and it could be read.
    pub(super) page: Option<TimeInfo>,
}

/// Why the HPET is the counter: what [`hpet_reason`] answers and the boot's
/// second `clock` line says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HpetReason {
    /// No invariant TSC, and no hypervisor to ask.
    NoHypervisor,
    /// A hypervisor that is not KVM, so nothing this kernel can ask.
    OtherHypervisor,
    /// KVM without `KVM_FEATURE_CLOCKSOURCE2`: no paravirtual clock offered.
    KvmNoClock,
    /// KVM without `KVM_FEATURE_CLOCKSOURCE_STABLE_BIT`.
    KvmNoStableBit,
    /// KVM's time page could not be registered or never read whole.
    KvmPageUnread,
    /// KVM's time page does not say stable: the host's own clock is not its
    /// TSC, or the guest's processors' TSCs differ.
    KvmFlagClear,
    /// KVM's time page says stable and states a rate no TSC has.
    KvmRateRefused,
    /// An invariant TSC that could not be measured against the HPET.
    MeasurementFailed,
}

impl HpetReason {
    /// The reason as [`HPET_REASON`] keeps it: never zero.
    const fn code(self) -> u8 {
        self as u8 + 1
    }

    /// The reason [`HpetReason::code`] numbered `code`.
    const fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::NoHypervisor,
            2 => Self::OtherHypervisor,
            3 => Self::KvmNoClock,
            4 => Self::KvmNoStableBit,
            5 => Self::KvmPageUnread,
            6 => Self::KvmFlagClear,
            7 => Self::KvmRateRefused,
            8 => Self::MeasurementFailed,
            _ => return None,
        })
    }
}

/// An HPET that is mapped, started and counting, as [`choose`] is told of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct HpetFacts {
    /// Whether its main counter is 64 bits wide.
    pub(super) wide: bool,
    /// The TSC's rate measured against it, where that was asked for and came
    /// off.
    pub(super) measured: Option<u64>,
}

/// The counter [`choose`] decides on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Source {
    /// Row 1: an invariant TSC at the rate measured against the HPET.
    TscByHpet(u64),
    /// Row 2: the TSC on KVM's word, at the rate its time page states.
    TscByKvm(u64),
    /// Row 3: the TSC, to be measured against the PIT.
    TscByPit,
    /// Row 4: the HPET itself, and why.
    Hpet(HpetReason),
    /// Row 5: the TSC at the rate measured against a 32-bit HPET.
    TscByNarrowHpet(u64),
}

/// Bring up a counter and return its name, for the boot log.
///
/// The order is `docs/CLOCK.md` §3: the HPET is opened first where firmware
/// describes one, since every row but the PIT's keeps its window the
/// kernel's own and a dead one is an error whichever counter is chosen; KVM
/// is asked only when the processor reports no invariant TSC; and the TSC is
/// measured against the HPET only on the rows that use the measurement.
///
/// # Errors
///
/// A string naming what could not be brought up. There is no third fallback:
/// a machine with neither an HPET nor a working PIT cannot tell the time, and
/// everything from the scheduler up would be built on a guess.
pub(crate) fn init(acpi: &Acpi<'_, DirectMap>) -> Result<&'static str, &'static str> {
    let hpet = match hpet_address(acpi) {
        Some(phys) => {
            crate::console::println!("  clock    opening the HPET at {phys:#x}");
            match open_hpet(phys) {
                Ok(opened) => Some(opened),
                // Firmware describing a timer block it left powered down or
                // gated is a real machine's trap, not a reason to stop the
                // boot: carry on as a machine with no HPET would.
                Err(why) => {
                    crate::console::println!(
                        "  clock    WARNING: {why}; going on as if there were no HPET"
                    );
                    None
                }
            }
        }
        None => {
            crate::console::println!("  clock    firmware describes no HPET");
            None
        }
    };
    let invariant = invariant_tsc();
    let kvm = if invariant {
        // Not asked: rows 1 and 3 need nobody's word but the processor's,
        // and `choose` does not look at this with the bit set.
        Err(HpetReason::MeasurementFailed)
    } else {
        kvm_word(&kvm_facts())
    };
    let facts = hpet.map(|(regs, _, caps, hpet_hz)| {
        let wide = caps.counter_is_64_bit();
        // Ten milliseconds, spent only where the answer is used: row 1, and
        // row 5 when KVM did not vouch.
        let measured = if invariant || (kvm.is_err() && !wide) {
            calibrate_tsc_against_hpet(regs, hpet_hz, caps.counter_mask())
        } else {
            None
        };
        HpetFacts { wide, measured }
    });
    let source = choose(invariant, kvm, facts)?;
    adopt(source, hpet.map(|(_, base, _, hpet_hz)| (base, hpet_hz)))
}

/// Make `source` the counter and answer its name. `hpet` is where the
/// opened HPET's registers are mapped and how fast it counts.
fn adopt(source: Source, hpet: Option<(u64, u64)>) -> Result<&'static str, &'static str> {
    let (name, hz) = match source {
        Source::TscByHpet(hz) => (BY_HPET, hz),
        Source::TscByKvm(hz) => (BY_KVM, hz),
        Source::TscByPit => return start_tsc(),
        Source::Hpet(reason) => {
            let (base, hpet_hz) =
                hpet.ok_or("the HPET was chosen for the counter and firmware described none")?;
            HPET.store(base, Ordering::Relaxed);
            HPET_REASON.store(reason.code(), Ordering::Relaxed);
            (THE_HPET, hpet_hz)
        }
        // `HPET` stays zero, so `counter_now` reads the TSC. The window stays
        // mapped all the same: a mapped device window is how the kernel knows
        // which registers are its own and no driver's.
        Source::TscByNarrowHpet(hz) => (BY_NARROW_HPET, hz),
    };
    COUNTER_HZ.store(hz, Ordering::Relaxed);
    Ok(name)
}

/// Which counter a machine gets: `docs/CLOCK.md` §3, as a function of what
/// was found. `kvm` is [`kvm_word`]'s answer and is looked at only without
/// an invariant TSC; `hpet` is the HPET once opened.
///
/// # Errors
///
/// A 32-bit HPET that nothing vouches past and the TSC could not be measured
/// against: its counter wraps every few minutes and is nobody's clock.
pub(super) const fn choose(
    invariant: bool,
    kvm: Result<u64, HpetReason>,
    hpet: Option<HpetFacts>,
) -> Result<Source, &'static str> {
    // Row 1. An invariant TSC is the counter whatever the HPET's width, for
    // the reason this module opens with: the rate still comes from the HPET,
    // and every later reading costs a register read instead of three loads
    // from a device window.
    if invariant
        && let Some(HpetFacts {
            measured: Some(hz), ..
        }) = hpet
    {
        return Ok(Source::TscByHpet(hz));
    }
    // Row 2. Only without the bit: a processor that has it needs no second
    // opinion, and its boots keep the line and the rate they had.
    if !invariant && let Ok(hz) = kvm {
        return Ok(Source::TscByKvm(hz));
    }
    match hpet {
        // Row 3.
        None => Ok(Source::TscByPit),
        // Row 4. A measurement that did not come off leaves the HPET to
        // answer for itself.
        Some(HpetFacts { wide: true, .. }) => Ok(Source::Hpet(if invariant {
            HpetReason::MeasurementFailed
        } else {
            match kvm {
                Err(reason) => reason,
                Ok(_) => HpetReason::KvmRateRefused,
            }
        })),
        // Row 5.
        Some(HpetFacts {
            wide: false,
            measured: Some(hz),
        }) => Ok(Source::TscByNarrowHpet(hz)),
        Some(HpetFacts {
            wide: false,
            measured: None,
        }) => Err("the HPET is 32 bits wide, and the TSC could not be measured against it"),
    }
}

/// Whether KVM vouches for the TSC, and at what rate: every condition of
/// `docs/CLOCK.md` §3's row 2, in order, each with the reason it fails for.
///
/// # Errors
///
/// The first condition that does not hold.
pub(super) const fn kvm_word(facts: &KvmFacts) -> Result<u64, HpetReason> {
    if !facts.hypervisor {
        return Err(HpetReason::NoHypervisor);
    }
    let [b, c, d] = facts.signature;
    if b != KVM_SIGNATURE[0] || c != KVM_SIGNATURE[1] || d != KVM_SIGNATURE[2] {
        return Err(HpetReason::OtherHypervisor);
    }
    if facts.features & KVM_FEATURE_CLOCKSOURCE2 == 0 {
        return Err(HpetReason::KvmNoClock);
    }
    if facts.features & KVM_FEATURE_CLOCKSOURCE_STABLE == 0 {
        return Err(HpetReason::KvmNoStableBit);
    }
    let Some(page) = facts.page else {
        return Err(HpetReason::KvmPageUnread);
    };
    if page.flags & PVCLOCK_TSC_STABLE == 0 {
        return Err(HpetReason::KvmFlagClear);
    }
    match pvclock_hz(page.mul, page.shift) {
        Some(hz) => Ok(hz),
        None => Err(HpetReason::KvmRateRefused),
    }
}

/// The TSC rate a time page states. KVM turns a TSC interval into
/// nanoseconds as `((ticks << shift) * mul) >> 32`, shifting right for a
/// negative shift, so the rate is `10^9 * 2^32 / (mul * 2^shift)`, rounded
/// down.
///
/// `None` for a multiplier of zero, a shift outside -31..=31, and a rate
/// outside [`PVCLOCK_HZ_RANGE`]. The numerator is at most `10^9 * 2^63`,
/// under 2^93, so nothing here overflows 128 bits.
pub(super) const fn pvclock_hz(mul: u32, shift: i8) -> Option<u64> {
    if mul == 0 || shift < -31 || shift > 31 {
        return None;
    }
    let mut numerator: u128 = 1_000_000_000 << 32;
    if shift < 0 {
        numerator <<= shift.unsigned_abs();
    }
    let mut hz = numerator / mul as u128;
    if shift > 0 {
        hz >>= shift.unsigned_abs();
    }
    if hz < *PVCLOCK_HZ_RANGE.start() || hz > *PVCLOCK_HZ_RANGE.end() {
        return None;
    }
    Some(hz as u64)
}

/// Read a time page whole through `word`, which answers the page's `n`th
/// 32-bit word: KVM's protocol, in which the version is odd while the
/// hypervisor is writing and changes whenever it has written.
///
/// The version is read first and last. A version of zero (never filled),
/// an odd one, or one that changed across the read means look again, at most
/// [`TIME_PAGE_TRIES`] times; then `None`.
pub(super) fn read_time_page(mut word: impl FnMut(usize) -> u32) -> Option<TimeInfo> {
    for _ in 0..TIME_PAGE_TRIES {
        let version = word(0);
        if version == 0 || version & 1 != 0 {
            continue;
        }
        let mul = word(6);
        let last = word(7);
        if word(0) != version {
            continue;
        }
        return Some(TimeInfo {
            mul,
            shift: last.to_le_bytes()[0].cast_signed(),
            flags: last.to_le_bytes()[1],
        });
    }
    None
}

/// Register the kernel's time page with KVM on this processor, read it, and
/// give it back: what the page said, or `None` when it could not be
/// registered or never read whole. Whatever it read, no page is registered
/// when it returns.
///
/// Interrupts are masked from the registering write to the give-back. The
/// register is one per virtual processor, so a task moved between the two
/// writes would give back on another processor and leave this one's page
/// registered; with interrupts masked nothing preempts and nothing moves.
///
/// The caller has seen `KVM_FEATURE_CLOCKSOURCE2` under KVM's signature:
/// without it the register does not exist, and KVM refuses the write with
/// `#GP`. A `#GP` here is the kernel's own trap report naming this
/// instruction, a panic, never a fall-through to another counter. The value
/// written is the physical address of [`TIME_PAGE`] with bit 0, the enable,
/// set: `TimePage` is 32 bytes aligned to 32, so the address has its low
/// five bits clear, and the 32 bytes KVM writes from it lie in one page and
/// are exactly that static, an interior-mutable one whose words are atomics
/// because a writer the compiler cannot see stores into them from the
/// registering write to the give-back.
pub(super) fn kvm_time_page() -> Option<TimeInfo> {
    let virt = &raw const TIME_PAGE as u64;
    let phys = crate::mm::translate(virt)?;
    let were_enabled = cpu::read_rflags() & cpu::RFLAGS_INTERRUPT != 0;
    cpu::disable_interrupts();
    for word in &TIME_PAGE.0 {
        word.store(0, Ordering::SeqCst);
    }
    // SAFETY: (SYSREG) the register exists, since the caller saw KVM offer
    // CLOCKSOURCE2, and a #GP on it is a kernel trap, a panic. The value is
    // `TIME_PAGE`'s physical address with the enable bit: 32 bytes aligned
    // to 32, in one page, which are exactly that static, and whose words
    // are atomics because KVM stores into them until the give-back below.
    unsafe { cpu::write_msr(MSR_KVM_SYSTEM_TIME_NEW, phys | 1) };
    // SeqCst, in program order, the version first and last: the other writer
    // is the hypervisor, between two of this processor's instructions, and
    // the protocol is the order of these loads.
    let info = read_time_page(|n| {
        TIME_PAGE
            .0
            .get(n)
            .map_or(0, |word| word.load(Ordering::SeqCst))
    });
    // SAFETY: (SYSREG) the same register, which exists as the first write's
    // comment says; zero has the enable clear, which ends KVM's writes to
    // the page.
    unsafe { cpu::write_msr(MSR_KVM_SYSTEM_TIME_NEW, 0) };
    if were_enabled {
        cpu::enable_interrupts();
    }
    info
}

/// Where KVM believes this processor's time page is: zero when none is
/// registered. Only where [`kvm_time_page`] may be called.
pub(super) fn kvm_time_page_registered() -> u64 {
    // SAFETY: (SYSREG) as `kvm_time_page`: the caller saw KVM offer
    // CLOCKSOURCE2, under which the register reads back what was written.
    unsafe { cpu::read_msr(MSR_KVM_SYSTEM_TIME_NEW) }
}

/// What the processor says of a hypervisor's clock. Each leaf is read only
/// once the one before allows it: the hypervisor's leaves only under the
/// hypervisor bit, KVM's feature leaf only under KVM's signature and where
/// the first leaf's EAX -- the highest leaf, which KVM before 3.x left zero
/// to mean the feature leaf -- does not rule it out, and the time page only
/// where the features offer one.
pub(super) fn kvm_facts() -> KvmFacts {
    use core::arch::x86_64::__cpuid;
    let mut facts = KvmFacts {
        hypervisor: __cpuid(1).ecx & CPUID_HYPERVISOR != 0,
        signature: [0; 3],
        features: 0,
        page: None,
    };
    if !facts.hypervisor {
        return facts;
    }
    let first = __cpuid(HYPERVISOR_LEAF);
    facts.signature = [first.ebx, first.ecx, first.edx];
    if facts.signature != KVM_SIGNATURE || (first.eax != 0 && first.eax < KVM_FEATURE_LEAF) {
        return facts;
    }
    facts.features = __cpuid(KVM_FEATURE_LEAF).eax;
    if facts.features & KVM_FEATURE_CLOCKSOURCE2 != 0 {
        facts.page = kvm_time_page();
    }
    facts
}

/// Why the HPET is the counter: a reason exactly when it is, `None` when the
/// counter is the TSC or there is none yet.
pub(crate) fn hpet_reason() -> Option<HpetReason> {
    HpetReason::from_code(HPET_REASON.load(Ordering::Relaxed))
}

/// Say that the HPET is the clock, what that costs and why: the boot's
/// second `clock` line (`docs/CLOCK.md` §5), printed only what this boot
/// knows. The hypervisor's twelve bytes are its own and are printed as
/// text only when every one is a printable character, in hexadecimal
/// otherwise, so that nothing a hypervisor answers can end the line.
pub(crate) fn warn_of_hpet(reason: HpetReason) {
    use core::arch::x86_64::__cpuid;
    let cost = "the HPET is the clock, three device reads a reading, and no clock in the vDSO \
                (every clock_gettime is a system call)";
    let remedy = "give the guest invtsc (QEMU: -cpu ...,migratable=off,+invtsc; libvirt: \
                  migratable='off' and <feature policy='require' name='invtsc'/>)";
    let why = match reason {
        HpetReason::NoHypervisor => {
            crate::console::println!(
                "  clock    WARNING: {cost}: the processor reports no invariant TSC"
            );
            return;
        }
        HpetReason::MeasurementFailed => {
            crate::console::println!(
                "  clock    WARNING: {cost}: the invariant TSC could not be measured against it"
            );
            return;
        }
        HpetReason::OtherHypervisor => "which offers nothing this kernel can ask",
        HpetReason::KvmNoClock => "which offers no paravirtual clock",
        HpetReason::KvmNoStableBit => "whose paravirtual clock has no stable bit",
        HpetReason::KvmPageUnread => "whose time page could not be read",
        HpetReason::KvmFlagClear => "whose time page does not say the TSC is stable",
        HpetReason::KvmRateRefused => "whose time page states a rate no TSC has",
    };
    let leaf = __cpuid(HYPERVISOR_LEAF);
    let mut name = [0u8; 12];
    for (chunk, word) in name.chunks_exact_mut(4).zip([leaf.ebx, leaf.ecx, leaf.edx]) {
        chunk.copy_from_slice(&word.to_le_bytes());
    }
    let shown = name
        .iter()
        .rposition(|&byte| byte != 0)
        .map_or(0, |last| last + 1);
    match core::str::from_utf8(name.get(..shown).unwrap_or(&[])) {
        Ok(text) if shown != 0 && text.bytes().all(|byte| (0x20..0x7f).contains(&byte)) => {
            crate::console::println!(
                "  clock    WARNING: {cost}: CPUID reports no invariant TSC, under a hypervisor \
                 ({text}) {why}; {remedy}"
            );
        }
        _ => crate::console::println!(
            "  clock    WARNING: {cost}: CPUID reports no invariant TSC, under a hypervisor \
             ({:08x} {:08x} {:08x}) {why}; {remedy}",
            leaf.ebx,
            leaf.ecx,
            leaf.edx
        ),
    }
}

/// Where the HPET's registers are, if firmware describes a memory-mapped one.
fn hpet_address(acpi: &Acpi<'_, DirectMap>) -> Option<u64> {
    let hpet = acpi.hpet().ok()?;
    let address = hpet.base_address()?;
    // An I/O-space timer block is legal in the specification and has never
    // been built. Mapping a window over that physical address would be a
    // window over whatever else is there.
    if address.address_space_id != GenericAddress::SYSTEM_MEMORY || !address.is_present() {
        return None;
    }
    Some(address.address)
}

/// Map the timer block, start it, and prove it counts: its registers, where
/// they are mapped, its capabilities and its rate.
fn open_hpet(phys: u64) -> Result<(Mmio, u64, Capabilities, u64), &'static str> {
    let base = crate::vmap::map_device(phys, HPET_WINDOW).map_err(|_| "could not map the HPET")?;
    let regs = Mmio::at(base);

    // The register, not the table: firmware's copy of these bits is a hint.
    let caps = Capabilities(read64(regs, hpet::CAPABILITIES));
    let hpet_hz = caps
        .counter_hz()
        .ok_or("the HPET reports a period outside the specification")?;

    let enabled = read64(regs, HPET_CONFIG) | HPET_CONFIG_ENABLE;
    write64(regs, HPET_CONFIG, enabled);

    // Firmware may leave the block present but powered down, in which case
    // every read returns the same value and every later measurement divides
    // by zero elapsed time. Better to find out here.
    let first = main_counter(regs);
    let mut spins = 0u32;
    while main_counter(regs) == first {
        spins = spins.saturating_add(1);
        if spins > 1_000_000 {
            return Err("the HPET is mapped but its counter does not advance");
        }
    }
    Ok((regs, base, caps, hpet_hz))
}

/// Whether the TSC counts at a rate nothing disturbs: `CPUID.80000007H:EDX[8]`.
///
/// The extended leaves have to be asked for before they are read — a CPU that
/// has none reports its highest as a number below the one being asked about,
/// and reading past it gives whatever the highest leaf returns rather than a
/// zero.
fn invariant_tsc() -> bool {
    use core::arch::x86_64::__cpuid;
    /// The highest extended leaf, and the leaf the bit lives in.
    const HIGHEST: u32 = 0x8000_0000;
    /// Advanced power management information.
    const POWER: u32 = 0x8000_0007;
    __cpuid(HIGHEST).eax >= POWER && __cpuid(POWER).edx & (1 << 8) != 0
}

/// Count TSC ticks over [`CALIBRATION_MILLIS`] of an HPET counting at
/// `hpet_hz`, whose counter is `mask` wide.
///
/// Ten milliseconds is about 143 thousand ticks of a 14.3 `MHz` counter, far
/// inside one wrap, so a single wrap-aware subtraction is exact however close
/// to the top the counter started.
fn calibrate_tsc_against_hpet(regs: Mmio, hpet_hz: u64, mask: u64) -> Option<u64> {
    let window = hpet_hz * CALIBRATION_MILLIS / 1000;
    let hpet_start = main_counter(regs);
    let tsc_start = cpu::rdtsc();

    let mut spins = 0u32;
    let hpet_elapsed = loop {
        let elapsed = hpet::ticks_between(hpet_start, main_counter(regs), mask);
        if elapsed >= window {
            break elapsed;
        }
        spins = spins.saturating_add(1);
        // As `calibrate_tsc`: a counter that stops is not one to hang on.
        // Three device reads a spin, each up to a microsecond on real
        // hardware: twenty million is a minute at the very worst, where ten
        // milliseconds of a live counter is some thousands of spins.
        if spins > 20_000_000 {
            return None;
        }
    };
    let tsc_elapsed = cpu::rdtsc().checked_sub(tsc_start)?;

    hpet::measured_hz(tsc_elapsed, hpet_elapsed, hpet_hz)
}

/// Measure the TSC against the PIT and use that.
fn start_tsc() -> Result<&'static str, &'static str> {
    crate::console::println!("  clock    measuring the TSC against PIT channel 2 (bounded)");
    if let Some(hz) = calibrate_tsc() {
        COUNTER_HZ.store(hz, Ordering::Relaxed);
        return Ok(BY_PIT);
    }
    crate::console::println!(
        "  clock    WARNING: PIT channel 2 never reached terminal count (gated 8254?); asking CPUID leaf 0x15/0x16 for the TSC's rate"
    );
    let hz =
        cpuid_tsc_hz().ok_or("no HPET, the PIT did not answer, and CPUID reports no TSC rate")?;
    COUNTER_HZ.store(hz, Ordering::Relaxed);
    Ok(BY_CPUID)
}

/// The TSC's rate as the processor itself reports it, the way Linux's
/// `native_calibrate_tsc` reads it: leaf 0x15's crystal times its ratio, or,
/// where the crystal is not enumerated, leaf 0x16's base frequency.
fn cpuid_tsc_hz() -> Option<u64> {
    use core::arch::x86_64::__cpuid;
    let highest = __cpuid(0).eax;
    if highest < 0x15 {
        return None;
    }
    let tsc = __cpuid(0x15);
    let (denominator, numerator, crystal) =
        (u64::from(tsc.eax), u64::from(tsc.ebx), u64::from(tsc.ecx));
    let base_hz = if highest >= 0x16 {
        u64::from(__cpuid(0x16).eax & 0xFFFF) * 1_000_000
    } else {
        0
    };
    if denominator != 0 && numerator != 0 && crystal != 0 {
        return Some(crystal * numerator / denominator);
    }
    // No crystal: leaf 0x16's base frequency is the TSC's on every part
    // that reports it (Linux derives the crystal from it the same way).
    if base_hz != 0 {
        return Some(base_hz);
    }
    None
}

/// Whether channel 2's output is high, meaning it reached terminal count.
fn pit_finished() -> bool {
    // SAFETY: (DEVICE) port 0x61 is the PC/AT system control port. Reading it has no
    // side effect; bit 5 is channel 2's output.
    let status = unsafe { cpu::inb(PIT_GATE) };
    status & PIT_OUTPUT != 0
}

/// Count TSC ticks over a known PIT interval.
///
/// Channel 2 rather than channel 0: it is the only one whose gate software
/// controls and the only one not wired to an interrupt, so measuring with it
/// cannot disturb anything.
fn calibrate_tsc() -> Option<u64> {
    let count = u16::try_from(PIT_HZ * CALIBRATION_MILLIS / 1000).ok()?;

    // SAFETY: (DEVICE) reading port 0x61 to preserve the bits this does not own —
    // the speaker's, principally, which must come back as it was found.
    let saved = unsafe { cpu::inb(PIT_GATE) };
    // SAFETY: (DEVICE) gate on, speaker off. Bits 2..7 are preserved.
    unsafe { cpu::outb(PIT_GATE, (saved & 0xFC) | PIT_GATE_ON) };
    // SAFETY: (DEVICE) the 8254 command register, programming channel 2 one-shot.
    unsafe { cpu::outb(PIT_COMMAND, PIT_ONESHOT) };
    // SAFETY: (DEVICE) channel 2's counter, low byte then high byte as the command
    // just asked for. The second write starts it.
    unsafe { cpu::outb(PIT_CHANNEL2, count as u8) };
    // SAFETY: (DEVICE) as above, the high byte.
    unsafe { cpu::outb(PIT_CHANNEL2, (count >> 8) as u8) };

    let start = cpu::rdtsc();
    let mut spins = 0u32;
    while !pit_finished() {
        spins = spins.saturating_add(1);
        // A read of port 0x61 takes about a microsecond on real hardware,
        // so ten milliseconds is some ten thousand spins and five million
        // is a few seconds. A PIT that never finishes -- gated off, as
        // Alder Lake and later may leave the 8254 -- is a machine that
        // cannot be calibrated this way, not one to hang on.
        if spins > 5_000_000 {
            // SAFETY: (DEVICE) put port 0x61 back exactly as it was found.
            unsafe { cpu::outb(PIT_GATE, saved) };
            return None;
        }
    }
    let elapsed = cpu::rdtsc().checked_sub(start)?;

    // SAFETY: (DEVICE) put port 0x61 back exactly as it was found.
    unsafe { cpu::outb(PIT_GATE, saved) };

    let hz = elapsed * 1000 / CALIBRATION_MILLIS;
    // No x86-64 processor's TSC counts below 200 MHz. A rate under that is a
    // PIT whose output was already high -- QEMU's `pit=off` reads so, and a
    // gated 8254 may too -- and adopting it would make every delay a
    // hundredth of its length and every timeout come at once.
    if hz < 200_000_000 {
        crate::console::println!(
            "  clock    WARNING: PIT channel 2 says the TSC runs at {hz} Hz, which no x86-64 processor does; the PIT is not counting"
        );
        return None;
    }
    Some(hz)
}

/// Read one of the HPET's 64-bit registers.
///
/// The only 64-bit device registers in the tree, which is why these are here
/// rather than on [`Mmio`]: on AArch64 they would be dead code, and a shared
/// module carrying an accessor nobody calls is how a facade starts to rot.
fn read64(regs: Mmio, offset: u64) -> u64 {
    (u64::from(regs.read32(offset + 4)) << 32) | u64::from(regs.read32(offset))
}

/// Write one of them.
fn write64(regs: Mmio, offset: u64, value: u64) {
    regs.write32(offset, value as u32);
    regs.write32(offset + 4, (value >> 32) as u32);
}

/// The main counter, read so that a 32-bit wrap cannot be observed torn.
///
/// The two halves are read separately, so the low half can wrap between them
/// and produce a value nearly four billion ticks wrong. Re-reading the high
/// half and retrying when it changed is the standard answer, and it
/// terminates: the high half advances once every few minutes.
fn main_counter(regs: Mmio) -> u64 {
    loop {
        let high = regs.read32(HPET_MAIN_COUNTER + 4);
        let low = regs.read32(HPET_MAIN_COUNTER);
        if regs.read32(HPET_MAIN_COUNTER + 4) == high {
            return (u64::from(high) << 32) | u64::from(low);
        }
    }
}

/// The counter's current value.
pub(crate) fn counter_now() -> u64 {
    let base = HPET.load(Ordering::Relaxed);
    if base == 0 {
        return cpu::rdtsc();
    }
    main_counter(Mmio::at(base))
}

/// Whether the counter is the TSC, which a program can read for itself with
/// `rdtsc`: what the vDSO asks before it answers without the kernel.
pub(crate) fn counter_is_tsc() -> bool {
    HPET.load(Ordering::Relaxed) == 0 && COUNTER_HZ.load(Ordering::Relaxed) != 0
}

/// How fast it counts, or zero before [`init`].
pub(crate) fn counter_hz() -> u64 {
    COUNTER_HZ.load(Ordering::Relaxed)
}

/// The width of the counter [`counter_now`] reads: all 64 bits, whichever it
/// is, because a 32-bit HPET is only ever a calibration reference.
const COUNTER_MASK: u64 = u64::MAX;

/// Ticks from one [`counter_now`] reading to a later one, subtracted in the
/// counter's own width.
///
/// The one place a caller measuring an interval should subtract, so the width
/// is decided here, beside the choice of counter, and not assumed at each
/// call site.
pub(crate) fn ticks_between(earlier: u64, later: u64) -> u64 {
    hpet::ticks_between(earlier, later, COUNTER_MASK)
}
