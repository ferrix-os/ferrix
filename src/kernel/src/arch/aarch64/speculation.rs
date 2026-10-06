//! `AArch64`'s side-channel defences.
//!
//! `docs/certification/SPECULATION.md` §4 argues the set; this applies it.
//! Each processor decides for itself, as it starts, from its own ID registers
//! and what firmware says about it through SMCCC: the cores of one machine need
//! not be alike. Only how firmware is reached, and whether it answers SMCCC at
//! all, is found once, on the boot processor. Once every processor has
//! started, [`report_once_started`] says what the machine is exposed to, a
//! line per kind of core.
//!
//! | Hazard | Defence here | When |
//! |---|---|---|
//! | Spectre v1 | indices clamped with `csel` and `csdb` | always |
//! | Spectre v2 | firmware's `ARCH_WORKAROUND_1` when the processor switches address space | the core lacks `CSV2`, is not on Arm's list of unaffected cores, and firmware says this core needs it |
//! | Spectre-BHB | the branch history overwritten by a loop on every entry from EL0 | the core is on Arm's list, and lacks `ECBHB` |
//! | Speculative store bypass | `PSTATE.SSBS` clear in EL1 (`SCTLR_EL1.DSSBS`) and for a program's first instruction; firmware's `ARCH_WORKAROUND_2` without `SSBS` | the core has `SSBS`, or firmware says this core needs the workaround |
//! | Meltdown | none: KPTI is not built | reported, and excluded by AoU-11 |
//!
//! The reference machine's Cortex-A72 needs the BHB loop (eight branches),
//! has neither `SSBS` nor `CSV2`, and under QEMU has no firmware to offer
//! either workaround: its boot log says so. Arm lists it as unaffected by
//! Meltdown.

use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use ferrix_bootinfo::BootView;
use ferrix_sched::MAX_CPUS;

use super::cpu;
use crate::arch::speculation::{Defences, HARDENED, applied_by, record_this_cpu, this_cpu};
use crate::console::println;

mod check;
pub(crate) use check::check;

/// `SCTLR_EL1.DSSBS`: the value `PSTATE.SSBS` takes on an exception to EL1.
const SCTLR_DSSBS: u64 = 1 << 44;

/// SMCCC and PSCI function numbers.
mod smccc {
    /// `PSCI_VERSION`.
    pub(super) const PSCI_VERSION: u64 = 0x8400_0000;
    /// `PSCI_FEATURES`.
    pub(super) const PSCI_FEATURES: u64 = 0x8400_000A;
    /// `SMCCC_VERSION`.
    pub(super) const VERSION: u64 = 0x8000_0000;
    /// `SMCCC_ARCH_FEATURES`.
    pub(super) const ARCH_FEATURES: u64 = 0x8000_0001;
    /// `SMCCC_ARCH_WORKAROUND_1`: invalidate the branch predictor.
    pub(super) const WORKAROUND_1: u64 = 0x8000_8000;
    /// `SMCCC_ARCH_WORKAROUND_2`: turn the store bypass mitigation on or off.
    pub(super) const WORKAROUND_2: u64 = 0x8000_7FFF;
}

/// How firmware is reached, for the workarounds: none, `hvc` or `smc`.
static CONDUIT: AtomicU8 = AtomicU8::new(CONDUIT_NONE);
/// No firmware workaround is to be called.
const CONDUIT_NONE: u8 = 0;
/// Through `hvc`.
const CONDUIT_HVC: u8 = 1;
/// Through `smc`.
const CONDUIT_SMC: u8 = 2;

/// Whether firmware answers `SMCCC_ARCH_FEATURES`, which the boot processor
/// finds out once: whether a core may ask it about the workarounds.
static ARCH_FEATURES_ANSWERED: AtomicBool = AtomicBool::new(false);

/// Whether a switch of address space calls `ARCH_WORKAROUND_1`, by logical
/// processor: each core's own decision, read by the core that switches.
static SWITCH_WORKAROUND: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

/// What each processor's ID registers and firmware said, by logical number,
/// as [`Seen::pack`] packs it: zero for a processor that recorded nothing.
static SEEN: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// How many branches the entry loop takes: the largest any processor needs,
/// zero for none. Read from assembly by every vector entry from EL0.
pub(super) static BHB_LOOPS: AtomicU64 = AtomicU64::new(0);

/// Whether the entry paths' extra instructions are assembled in: `1` or `0`,
/// for an assembler `.if`.
pub(super) const ENTRY_HARDENING: u8 = HARDENED as u8;

/// Arm's parts by the part number in `MIDR_EL1`, for the boot log. The
/// numbers are Linux's `ARM_CPU_PART_*` (`arch/arm64/include/asm/cputype.h`).
const ARM_PARTS: [(u64, &str); 27] = [
    (0xD03, "Cortex-A53"),
    (0xD04, "Cortex-A35"),
    (0xD05, "Cortex-A55"),
    (0xD07, "Cortex-A57"),
    (0xD08, "Cortex-A72"),
    (0xD09, "Cortex-A73"),
    (0xD0A, "Cortex-A75"),
    (0xD0B, "Cortex-A76"),
    (0xD0C, "Neoverse N1"),
    (0xD0D, "Cortex-A77"),
    (0xD0E, "Cortex-A76AE"),
    (0xD40, "Neoverse V1"),
    (0xD41, "Cortex-A78"),
    (0xD42, "Cortex-A78AE"),
    (0xD44, "Cortex-X1"),
    (0xD46, "Cortex-A510"),
    (0xD47, "Cortex-A710"),
    (0xD48, "Cortex-X2"),
    (0xD49, "Neoverse N2"),
    (0xD4B, "Cortex-A78C"),
    (0xD4C, "Cortex-X1C"),
    (0xD4D, "Cortex-A715"),
    (0xD4E, "Cortex-X3"),
    (0xD4F, "Neoverse V2"),
    (0xD80, "Cortex-A520"),
    (0xD81, "Cortex-A720"),
    (0xD82, "Cortex-X4"),
];

/// What the ID registers say about this core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Core {
    /// `MIDR_EL1`.
    midr: u64,
    /// `ID_AA64PFR0_EL1.CSV2` is set: branch targets cannot be trained
    /// across contexts.
    csv2: bool,
    /// `ID_AA64PFR0_EL1.CSV3` is set: not vulnerable to Meltdown.
    csv3: bool,
    /// `ID_AA64PFR1_EL1.SSBS`: 0, 1 (the bit exists), or 2 (and `MSR SSBS`).
    ssbs: u64,
    /// `ID_AA64MMFR1_EL1.ECBHB`: the branch history is not shared.
    ecbhb: bool,
}

impl Core {
    /// This core's registers.
    fn read() -> Core {
        let pfr0 = cpu::read_id_aa64pfr0();
        Core {
            midr: read_midr(),
            csv2: (pfr0 >> 56) & 0xF != 0,
            csv3: (pfr0 >> 60) & 0xF != 0,
            ssbs: (read_id_aa64pfr1() >> 4) & 0xF,
            ecbhb: (read_id_aa64mmfr1() >> 60) & 0xF != 0,
        }
    }

    /// Arm's part number, if Arm designed the core.
    const fn arm_part(self) -> Option<u64> {
        if (self.midr >> 24) & 0xFF == 0x41 {
            Some((self.midr >> 4) & 0xFFF)
        } else {
            None
        }
    }

    /// Whether the core is one Arm lists as not affected by Spectre v2:
    /// Linux's `spectre_v2_safe_list` (`arch/arm64/kernel/proton-pack.c`), for
    /// Arm's own parts -- the Cortex-A53 (`0xD03`), A35 (`0xD04`) and A55
    /// (`0xD05`), in-order designs. The list's other entries are other
    /// implementers' (Broadcom's Brahma-B53, `HiSilicon`'s TSV110, Qualcomm's
    /// Kryo silver cores), not built here.
    fn v2_listed_safe(self) -> bool {
        matches!(self.arm_part(), Some(0xD03..=0xD05))
    }

    /// Whether the core's own hardware keeps it from Spectre v2: `CSV2`, or
    /// Arm's list. Only a core without either needs firmware's workaround.
    fn v2_unaffected(self) -> bool {
        self.csv2 || self.v2_listed_safe()
    }

    /// The branches the BHB loop must take on this core: Arm's figures for
    /// each part it lists, zero where the history is not shared or the part is
    /// not listed. Linux's `spectre_bhb_loop_affected`.
    fn bhb_loops(self) -> u64 {
        if self.ecbhb {
            return 0;
        }
        match self.arm_part() {
            // Cortex-X3, Neoverse V2.
            Some(0xD4E | 0xD4F) => 132,
            // Cortex-A715, Cortex-A720.
            Some(0xD4D | 0xD81) => 38,
            // Cortex-A78, A78AE, A78C, X1, X1C, A710, X2, Neoverse N2, V1.
            Some(0xD41 | 0xD42 | 0xD4B | 0xD44 | 0xD4C | 0xD47 | 0xD48 | 0xD49 | 0xD40) => 32,
            // Cortex-A76, A77, A76AE, Neoverse N1.
            Some(0xD0B..=0xD0E) => 24,
            // Cortex-A57, A72.
            Some(0xD07 | 0xD08) => 8,
            _ => 0,
        }
    }

    /// Whether the core is one Arm lists as not vulnerable to Meltdown, or
    /// says so itself. Linux's `kpti_safe_list`, for Arm's own parts: every one
    /// but the Cortex-A75.
    fn meltdown_safe(self) -> bool {
        self.csv3 || matches!(self.arm_part(), Some(part) if part != 0xD0A)
    }

    /// Whether the core is one of Arm's in-order designs, which speculate too
    /// little to bypass a store: A35, A53, A55, A510, A520.
    fn in_order(self) -> bool {
        matches!(self.arm_part(), Some(0xD04 | 0xD03 | 0xD05 | 0xD46 | 0xD80))
    }
}

/// What firmware answered `SMCCC_ARCH_FEATURES` about one workaround, asked
/// on one core.
///
/// The SMC Calling Convention (ARM DEN 0028D, §7.5.2 and §7.6.2) makes the
/// answer per processor: "not supported" is the same on every core, but
/// where the workaround is implemented, 0 says *this* core needs it and 1
/// that it does not, and the call is then safe, if wasted, on every core.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Answer {
    /// Not implemented, or not asked because the core needs nothing.
    #[default]
    NotSupported,
    /// Implemented, and this core needs it: 0.
    Required,
    /// Implemented, and this core does not need it: 1.
    NotRequired,
    /// `NOT_REQUIRED`, -2: about workaround 2, that the mitigation is always
    /// on for this core or the core needs none, so there is nothing to call.
    /// KVM answers it both for a host that keeps the mitigation on for its
    /// guest and for one that is unaffected. Linux's
    /// `spectre_v4_get_cpu_fw_mitigation_state` takes it, as it takes 1, for
    /// not vulnerable. About workaround 1 Linux gives it no meaning and
    /// counts the core vulnerable, and so does this.
    AlwaysOn,
}

impl Answer {
    /// The answer to a status firmware returned.
    const fn from_status(status: i32) -> Answer {
        match status {
            0 => Answer::Required,
            1 => Answer::NotRequired,
            -2 => Answer::AlwaysOn,
            _ => Answer::NotSupported,
        }
    }

    /// Two bits, for [`Seen::pack`].
    const fn bits(self) -> u64 {
        match self {
            Answer::NotSupported => 0,
            Answer::Required => 1,
            Answer::NotRequired => 2,
            Answer::AlwaysOn => 3,
        }
    }

    /// The answer [`Answer::bits`] made.
    const fn from_bits(bits: u64) -> Answer {
        match bits & 0b11 {
            1 => Answer::Required,
            2 => Answer::NotRequired,
            3 => Answer::AlwaysOn,
            _ => Answer::NotSupported,
        }
    }
}

/// What firmware says about this core, through SMCCC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Firmware {
    /// About `ARCH_WORKAROUND_1`.
    workaround_1: Answer,
    /// About `ARCH_WORKAROUND_2`.
    workaround_2: Answer,
}

/// Whether firmware, through `conduit`, answers `SMCCC_ARCH_FEATURES`.
///
/// Only as far as PSCI says it is safe to ask: an SMCCC call that firmware
/// does not implement is an undefined instruction on a machine with no EL2
/// behind `hvc`, so SMCCC is asked for only once `PSCI_FEATURES` has said it
/// exists.
fn answers_arch_features(conduit: u8) -> bool {
    let call = |function, argument| firmware_call(conduit, function, argument) as u32 as i32;
    let psci = call(smccc::PSCI_VERSION, 0);
    if psci < 0x1_0000 || call(smccc::PSCI_FEATURES, smccc::VERSION) < 0 {
        return false;
    }
    call(smccc::VERSION, 0) >= 0x1_0001
}

/// Ask firmware what it says about the running core, for what the core's own
/// hardware does not already settle: workaround 1 only on a core Spectre v2
/// affects, workaround 2 only on one without `SSBS`.
fn ask_about_this_core(core: Core) -> Firmware {
    if !ARCH_FEATURES_ANSWERED.load(Ordering::Acquire) {
        return Firmware::default();
    }
    let conduit = CONDUIT.load(Ordering::Relaxed);
    let ask = |workaround| {
        Answer::from_status(firmware_call(conduit, smccc::ARCH_FEATURES, workaround) as u32 as i32)
    };
    Firmware {
        workaround_1: if core.v2_unaffected() {
            Answer::NotSupported
        } else {
            ask(smccc::WORKAROUND_1)
        },
        workaround_2: if core.ssbs > 0 {
            Answer::NotSupported
        } else {
            ask(smccc::WORKAROUND_2)
        },
    }
}

/// One SMCCC call through `conduit`; `u64::MAX` (an error) with none.
fn firmware_call(conduit: u8, function: u64, argument: u64) -> u64 {
    match conduit {
        // SAFETY: (FIRMWARE) every function this module calls is a query or a
        // workaround SMCCC defines to change no state a caller relies on,
        // asked for only after PSCI said SMCCC is there to answer.
        CONDUIT_HVC => unsafe { cpu::hvc_call(function, argument, 0, 0) },
        // SAFETY: (FIRMWARE) as above.
        CONDUIT_SMC => unsafe { cpu::smc_call(function, argument, 0, 0) },
        _ => u64::MAX,
    }
}

/// Find how firmware is reached, then decide and apply on the boot
/// processor, and say what it applied.
pub(crate) fn init(view: &BootView<'_>) {
    if !HARDENED {
        println!("  cpu      speculation defences off: built with --mitigations off");
        record_this_cpu(Defences::NONE);
        return;
    }
    let conduit = match super::smp::psci_conduit(view) {
        Ok(super::smp::Conduit::Hvc) => CONDUIT_HVC,
        Ok(super::smp::Conduit::Smc) => CONDUIT_SMC,
        Err(_) => CONDUIT_NONE,
    };
    CONDUIT.store(conduit, Ordering::Relaxed);
    ARCH_FEATURES_ANSWERED.store(answers_arch_features(conduit), Ordering::Release);
    let applied = decide_and_apply();
    println!("  cpu      speculation defences: {}", applied.names());
}

/// Decide and apply on a secondary, as it starts.
///
/// The cores of one machine need not be alike: a Pixel 7 boots on a
/// Cortex-A55, which Arm lists as unaffected by Spectre v2 and which needs no
/// branch history loop, and starts two A78s and two X1s, which have `CSV2`
/// and need 32 branches of the loop. So each core decides everything for
/// itself, asking firmware about itself too, as the SMC Calling Convention
/// has it asked: only how firmware is reached is the boot processor's.
pub(crate) fn apply_this_cpu() {
    if !HARDENED {
        record_this_cpu(Defences::NONE);
        return;
    }
    let _ = decide_and_apply();
}

/// Decide what the running core needs, apply it, record what it saw and
/// applied, and answer what it applied.
fn decide_and_apply() -> Defences {
    let core = Core::read();
    let firmware = ask_about_this_core(core);
    let plan = plan_for(core, firmware);
    let cpu = this_cpu();
    if let Some(slot) = SEEN.get(cpu) {
        slot.store(Seen { core, firmware }.pack(), Ordering::Release);
    }
    let applied = apply(plan, core, cpu);
    record_this_cpu(applied);
    applied
}

/// What `core` needs, given what firmware said about it.
fn plan_for(core: Core, firmware: Firmware) -> Defences {
    let mut plan = Defences::CLAMPED_INDICES;
    if !core.v2_unaffected() && firmware.workaround_1 == Answer::Required {
        plan = plan.with(Defences::SWITCH_BARRIER);
    }
    if core.bhb_loops() > 0 {
        plan = plan.with(Defences::BHB_LOOP);
    }
    if core.ssbs > 0 || firmware.workaround_2 == Answer::Required {
        plan = plan.with(Defences::SSBD);
    }
    plan
}

/// Apply `plan` on this core, logical processor `cpu`, read back what can
/// be, and say what took.
fn apply(plan: Defences, core: Core, cpu: usize) -> Defences {
    let mut held = true;
    let _ = BHB_LOOPS.fetch_max(core.bhb_loops(), Ordering::Relaxed);
    if plan.contains(Defences::SSBD) && core.ssbs > 0 {
        let sctlr = cpu::read_sctlr();
        // SAFETY: (PROTECT) clearing DSSBS changes only the value `PSTATE.SSBS` takes
        // on an exception to EL1, on a core whose ID register says it exists.
        unsafe { write_sctlr(sctlr & !SCTLR_DSSBS) };
        held &= cpu::read_sctlr() & SCTLR_DSSBS == 0;
        if core.ssbs >= 2 {
            clear_ssbs();
        }
    }
    if plan.contains(Defences::SSBD) && core.ssbs == 0 {
        let _ = firmware_call(CONDUIT.load(Ordering::Relaxed), smccc::WORKAROUND_2, 1);
    }
    if let Some(switch) = SWITCH_WORKAROUND.get(cpu) {
        switch.store(plan.contains(Defences::SWITCH_BARRIER), Ordering::Relaxed);
    }
    if held {
        plan
    } else {
        plan.with(Defences::READ_BACK_FAILED)
    }
}

/// What one processor's ID registers and firmware said, kept for the
/// machine's exposure report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Seen {
    /// The core.
    core: Core,
    /// What firmware said about it.
    firmware: Firmware,
}

/// Set in a packed [`Seen`]: a word of zero is a processor that never got
/// here.
const SEEN_RECORDED: u64 = 1 << 63;

impl Seen {
    /// One word: `MIDR_EL1`'s 32 bits, then the flags, the `SSBS` field and
    /// the two answers.
    fn pack(self) -> u64 {
        let core = self.core;
        (core.midr & 0xFFFF_FFFF)
            | (u64::from(core.csv2) << 32)
            | (u64::from(core.csv3) << 33)
            | (u64::from(core.ecbhb) << 34)
            | ((core.ssbs & 0xF) << 35)
            | (self.firmware.workaround_1.bits() << 39)
            | (self.firmware.workaround_2.bits() << 41)
            | SEEN_RECORDED
    }

    /// The record [`Seen::pack`] made, if one was.
    fn unpack(bits: u64) -> Option<Seen> {
        (bits & SEEN_RECORDED != 0).then_some(Seen {
            core: Core {
                midr: bits & 0xFFFF_FFFF,
                csv2: (bits >> 32) & 1 != 0,
                csv3: (bits >> 33) & 1 != 0,
                ecbhb: (bits >> 34) & 1 != 0,
                ssbs: (bits >> 35) & 0xF,
            },
            firmware: Firmware {
                workaround_1: Answer::from_bits(bits >> 39),
                workaround_2: Answer::from_bits(bits >> 41),
            },
        })
    }
}

/// What processor `logical` saw and applied, packed, if it recorded both.
fn kind_of(logical: usize) -> Option<(u64, Defences)> {
    let seen = SEEN.get(logical)?.load(Ordering::Acquire);
    if seen & SEEN_RECORDED == 0 {
        return None;
    }
    Some((seen, applied_by(logical)?))
}

/// Say what the machine is exposed to, once every processor has started and
/// recorded: a line for each kind of core -- the same part, told the same by
/// firmware, having applied the same -- with how many there are.
///
/// Not at [`init`], where only the boot processor has decided: a Pixel 7's
/// Cortex-A55 is not on Arm's Spectre-BHB list, and its A78s and X1s are.
pub(crate) fn report_once_started() {
    if !HARDENED {
        return;
    }
    let count = crate::smp::count();
    for logical in 0..count {
        let Some(kind) = kind_of(logical) else {
            continue;
        };
        if (0..logical).any(|earlier| kind_of(earlier) == Some(kind)) {
            continue;
        }
        let Some(seen) = Seen::unpack(kind.0) else {
            continue;
        };
        let alike = (logical..count)
            .filter(|&other| kind_of(other) == Some(kind))
            .count();
        println!(
            "  cpu      speculation exposure: {alike} x {}: {}",
            CoreName(seen.core.midr),
            Exposure(seen, kind.1),
        );
    }
}

/// A core's name for the boot log: Arm's, for its own parts; the `MIDR_EL1`
/// fields otherwise.
struct CoreName(u64);

impl core::fmt::Display for CoreName {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let implementer = (self.0 >> 24) & 0xFF;
        let part = (self.0 >> 4) & 0xFFF;
        let named = ARM_PARTS
            .iter()
            .find(|&&(number, _)| implementer == 0x41 && number == part);
        match named {
            Some(&(_, name)) => f.write_str(name),
            None => write!(f, "implementer {implementer:#04x} part {part:#05x}"),
        }
    }
}

/// What one kind of core is exposed to, for the boot log: what its plan
/// leaves uncovered, and why.
#[derive(Debug, Clone, Copy)]
struct Exposure(Seen, Defences);

impl Exposure {
    /// Spectre v2: whether the core is affected, and if so whether its plan
    /// covers it.
    fn v2(self) -> &'static str {
        let Exposure(Seen { core, firmware }, plan) = self;
        if core.csv2 {
            "not affected (CSV2)"
        } else if core.v2_listed_safe() {
            "not affected (Arm lists this core as unaffected)"
        } else if plan.contains(Defences::SWITCH_BARRIER) {
            "covered between programs"
        } else if firmware.workaround_1 == Answer::NotRequired {
            "not affected (firmware says this core needs no ARCH_WORKAROUND_1)"
        } else {
            "NOT covered: no CSV2, and firmware offers no ARCH_WORKAROUND_1 (AoU-11)"
        }
    }

    /// Speculative store bypass, likewise.
    fn bypass(self) -> &'static str {
        let Exposure(Seen { core, firmware }, plan) = self;
        if plan.contains(Defences::SSBD) {
            "covered"
        } else if firmware.workaround_2 == Answer::NotRequired || core.in_order() {
            "not affected"
        } else if firmware.workaround_2 == Answer::AlwaysOn {
            "covered by firmware: ARCH_WORKAROUND_2 answers NOT_REQUIRED, always on for this \
             core or not needed"
        } else {
            "NOT covered: no SSBS, and firmware offers no ARCH_WORKAROUND_2 (AoU-11)"
        }
    }
}

impl core::fmt::Display for Exposure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let Exposure(Seen { core, .. }, plan) = *self;
        write!(f, "Spectre v2 {}; Spectre-BHB ", self.v2())?;
        if plan.contains(Defences::BHB_LOOP) {
            write!(f, "covered ({} branches)", core.bhb_loops())?;
        } else if core.ecbhb {
            f.write_str("not affected (ECBHB)")?;
        } else {
            f.write_str("not affected (not on Arm's list)")?;
        }
        let meltdown = if core.meltdown_safe() {
            "not affected"
        } else {
            "EXPOSED: no CSV3 on a core Arm does not list as safe, and KPTI is not built (AoU-11)"
        };
        write!(f, "; store bypass {}; Meltdown {meltdown}", self.bypass())
    }
}

/// Issue the switch barrier on logical processor `cpu`, the one switching:
/// firmware's `ARCH_WORKAROUND_1`, which invalidates the branch predictor,
/// where that core's plan has it. Answers whether it was issued.
///
/// Per core, because the need is: a core with `CSV2` or on Arm's list never
/// pays for a firmware call it does not need, and one that needs it always
/// makes it.
pub(crate) fn switch_barrier(cpu: usize) -> bool {
    if !SWITCH_WORKAROUND
        .get(cpu)
        .is_some_and(|wanted| wanted.load(Ordering::Relaxed))
    {
        return false;
    }
    let _ = firmware_call(CONDUIT.load(Ordering::Relaxed), smccc::WORKAROUND_1, 0);
    true
}

/// `index` if `index < len`, else zero: `csel` on the flags `cmp` set, and
/// `csdb` so that no later instruction may use a *predicted* result of the
/// select.
#[inline(always)]
pub(crate) fn clamp_index(index: usize, len: usize) -> usize {
    if !HARDENED {
        return index;
    }
    let clamped: usize;
    // SAFETY: (PROTECT) three register instructions; no memory, no stack.
    unsafe {
        asm!(
            "cmp {index}, {len}",
            "csel {clamped}, {index}, xzr, lo",
            "csdb",
            index = in(reg) index,
            len = in(reg) len,
            clamped = lateout(reg) clamped,
            options(pure, nomem, nostack),
        );
    }
    clamped
}

/// [`clamp_index`], for a 64-bit value: the same thing on this machine.
#[inline(always)]
pub(crate) fn clamp_below(value: u64, end: u64) -> u64 {
    clamp_index(value as usize, end as usize) as u64
}

/// `MIDR_EL1`: who designed the core, and which part it is.
fn read_midr() -> u64 {
    let value: u64;
    // SAFETY: (SYSREG) an ID register read, legal at EL1 on every core.
    unsafe { asm!("mrs {}, midr_el1", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value
}

/// `ID_AA64PFR1_EL1`, whose `SSBS` field says whether the store bypass bit
/// exists.
fn read_id_aa64pfr1() -> u64 {
    let value: u64;
    // SAFETY: (SYSREG) as `read_midr`.
    unsafe {
        asm!("mrs {}, id_aa64pfr1_el1", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// `ID_AA64MMFR1_EL1`, whose `ECBHB` field says whether the branch history
/// is shared across contexts.
fn read_id_aa64mmfr1() -> u64 {
    let value: u64;
    // SAFETY: (SYSREG) as `read_midr`.
    unsafe {
        asm!("mrs {}, id_aa64mmfr1_el1", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// Write `SCTLR_EL1`, and synchronise.
///
/// # Safety
///
/// (PROTECT) `value` must be the register's current value with only bits the caller
/// has argued changed.
unsafe fn write_sctlr(value: u64) {
    // SAFETY: (PROTECT) the caller guarantees the value.
    unsafe { asm!("msr sctlr_el1, {}", "isb", in(reg) value, options(nostack, preserves_flags)) };
}

/// Clear `PSTATE.SSBS` now, on a core with `MSR SSBS`: the running context
/// took its value before `DSSBS` was cleared. Spelt as the register's
/// encoding, `S3_3_C4_C2_6`, which the assembler takes without a feature
/// flag.
fn clear_ssbs() {
    // SAFETY: (PROTECT) writes one `PSTATE` bit that governs only speculation, on a
    // core whose ID register says the instruction exists.
    unsafe {
        asm!(
            "msr s3_3_c4_c2_6, xzr",
            options(nomem, nostack, preserves_flags)
        );
    };
}

/// The switch barrier between two programs of one speculation domain:
/// nothing. This architecture's barrier is the predictor invalidation alone,
/// which a domain leaves out (`docs/OPAQUE-KERNEL.md` §9.3a, A2).
pub(crate) fn switch_barrier_in_domain(_cpu: usize) -> bool {
    false
}

/// Whether [`switch_barrier_in_domain`] refills a return stack: there is
/// none to refill here.
pub(crate) const REFILL_IN_DOMAIN: bool = false;

/// No refill to want: see [`REFILL_IN_DOMAIN`].
pub(crate) const fn refill_wanted_in_domain() -> bool {
    REFILL_IN_DOMAIN
}
