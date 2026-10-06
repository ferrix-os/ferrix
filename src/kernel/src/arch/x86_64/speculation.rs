//! x86-64's side-channel defences.
//!
//! `docs/certification/SPECULATION.md` §3 argues the set; this applies it.
//! Decided once, on the boot processor, from `CPUID` and
//! `IA32_ARCH_CAPABILITIES`; applied there and on every secondary as it
//! starts; read back on each.
//!
//! | Hazard | Defence here | When |
//! |---|---|---|
//! | Spectre v1 | indices clamped (`cmp`/`sbb`), a fence after the conditional `swapgs`, a program's registers cleared on entry | always |
//! | Spectre v2, program to kernel | enhanced IBRS, AMD's automatic IBRS, or IBRS left on where the processor says it may be | whichever it offers |
//! | Spectre v2, program to program | `IBPB` and a return stack refill when a processor switches address space; `STIBP` where eIBRS does not already give it | where offered |
//! | Speculative store bypass | `SSBD`, through `IA32_SPEC_CTRL` or AMD's virtual register | unless `SSB_NO` |
//! | MDS | `VERW` on every return to ring 3 | Intel parts with `MD_CLEAR` and without `MDS_NO` |
//! | Meltdown, L1TF | none: KPTI is not built | reported, and excluded by the safety manual's AoU-11 |
//!
//! What a processor that offers none of the IBRS forms gets is a line in the
//! boot log saying so. Retpolines would be the software answer, and the
//! pinned stable compiler has them only through a deprecated target feature
//! that is scheduled to become an error, and not in the precompiled `core`
//! and `alloc` the kernel links -- §3 has the details.

use core::arch::asm;
use core::arch::x86_64::{__cpuid, __cpuid_count};
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use super::cpu;
use crate::arch::speculation::{Defences, HARDENED, record_this_cpu};
use crate::console::println;

mod check;

/// `CPUID` `0x8000_0021` `EAX` bit 24: ERAPS. AMD's APM vol. 3, and Linux's
/// `X86_FEATURE_ERAPS`.
const ERAPS_BIT: u32 = 24;
/// `CR4.PCIDE`: a `CR3` write may then keep translations, and ERAPS empties
/// the predictor only at one that flushes.
const CR4_PCIDE: u64 = 1 << 17;

/// `IA32_SPEC_CTRL`: IBRS, STIBP and SSBD.
const IA32_SPEC_CTRL: u32 = 0x48;
/// `IA32_PRED_CMD`: writing [`PRED_CMD_IBPB`] empties the indirect predictors.
const IA32_PRED_CMD: u32 = 0x49;
/// `IA32_ARCH_CAPABILITIES`: what the processor is not vulnerable to.
const IA32_ARCH_CAPABILITIES: u32 = 0x10A;
/// AMD's `VIRT_SPEC_CTRL`, where a hypervisor offers SSBD without
/// `IA32_SPEC_CTRL`.
const AMD_VIRT_SPEC_CTRL: u32 = 0xC001_011F;
/// `IA32_EFER`.
const IA32_EFER: u32 = 0xC000_0080;

/// `IA32_SPEC_CTRL.IBRS`.
const SPEC_CTRL_IBRS: u64 = 1 << 0;
/// `IA32_SPEC_CTRL.STIBP`.
const SPEC_CTRL_STIBP: u64 = 1 << 1;
/// `IA32_SPEC_CTRL.SSBD`, and the same bit of AMD's virtual register.
const SPEC_CTRL_SSBD: u64 = 1 << 2;
/// `IA32_PRED_CMD.IBPB`.
const PRED_CMD_IBPB: u64 = 1 << 0;
/// `EFER.AIBRSE`: AMD's automatic IBRS.
const EFER_AUTO_IBRS: u64 = 1 << 21;

/// `IA32_ARCH_CAPABILITIES.RDCL_NO`: not vulnerable to Meltdown.
const CAP_RDCL_NO: u64 = 1 << 0;
/// `IA32_ARCH_CAPABILITIES.IBRS_ALL`: enhanced IBRS.
const CAP_IBRS_ALL: u64 = 1 << 1;
/// `IA32_ARCH_CAPABILITIES.SSB_NO`: not vulnerable to store bypass.
const CAP_SSB_NO: u64 = 1 << 4;
/// `IA32_ARCH_CAPABILITIES.MDS_NO`: not vulnerable to MDS.
const CAP_MDS_NO: u64 = 1 << 5;
/// `IA32_ARCH_CAPABILITIES.GDS_CTRL`: microcode that mitigates Gather Data
/// Sampling, controlled through `IA32_MCU_OPT_CTRL`.
const CAP_GDS_CTRL: u64 = 1 << 25;
/// `IA32_ARCH_CAPABILITIES.GDS_NO`: not vulnerable to Gather Data Sampling,
/// or a hypervisor's word that its host is mitigated.
const CAP_GDS_NO: u64 = 1 << 26;

/// `IA32_MCU_OPT_CTRL`: switches for microcode mitigations.
const IA32_MCU_OPT_CTRL: u32 = 0x123;
/// `IA32_MCU_OPT_CTRL.GDS_MITG_DIS`: the GDS mitigation turned off.
const MCU_GDS_MITG_DIS: u64 = 1 << 4;
/// `IA32_MCU_OPT_CTRL.GDS_MITG_LOCKED`: and that setting locked.
const MCU_GDS_MITG_LOCKED: u64 = 1 << 5;

/// AMD's `DE_CFG`, whose bit 9 is Zenbleed's chicken bit.
const AMD_DE_CFG: u32 = 0xC001_1029;
/// `DE_CFG`'s Zen 2 floating-point backup fix: Zenbleed closed without the
/// fixed microcode, at some cost to vector code.
const DE_CFG_ZEN2_FP_BACKUP_FIX: u64 = 1 << 9;
/// AMD's microcode patch level.
const AMD_PATCH_LEVEL: u32 = 0x8B;

/// What the processor offers and says about itself.
#[derive(Debug, Clone, Copy, Default)]
struct Offered {
    /// `GenuineIntel`.
    intel: bool,
    /// `AuthenticAMD` or `HygonGenuine`.
    amd: bool,
    /// `IA32_SPEC_CTRL` exists and takes IBRS.
    ibrs: bool,
    /// `IA32_PRED_CMD` takes IBPB.
    ibpb: bool,
    /// `IA32_SPEC_CTRL` takes STIBP.
    stibp: bool,
    /// AMD: IBRS may simply be left on, and protects when it is.
    ibrs_always_on: bool,
    /// `IA32_SPEC_CTRL` takes SSBD.
    ssbd: bool,
    /// AMD's virtual register takes SSBD.
    virt_ssbd: bool,
    /// AMD: not vulnerable to store bypass.
    amd_ssb_no: bool,
    /// AMD: automatic IBRS.
    auto_ibrs: bool,
    /// AMD: ERAPS, the return address predictor emptied by a `CR3` write
    /// that flushes the TLB.
    eraps: bool,
    /// Intel: `VERW` clears the buffers MDS reads.
    md_clear: bool,
    /// `IA32_ARCH_CAPABILITIES`, or zero where it does not exist.
    capabilities: u64,
}

/// Whether `leaf` is at or below the highest leaf of its range.
fn has_leaf(leaf: u32) -> bool {
    __cpuid(leaf & 0x8000_0000).eax >= leaf
}

/// `CPUID` and `IA32_ARCH_CAPABILITIES`, read.
fn offered() -> Offered {
    let vendor = __cpuid(0);
    let is = |name: &[u8; 12]| {
        let bytes = [vendor.ebx, vendor.edx, vendor.ecx];
        bytes
            .iter()
            .flat_map(|word| word.to_le_bytes())
            .eq(name.iter().copied())
    };
    let intel = is(b"GenuineIntel");
    let amd = is(b"AuthenticAMD") || is(b"HygonGenuine");

    let leaf7 = if has_leaf(7) {
        __cpuid_count(7, 0).edx
    } else {
        0
    };
    let amd8 = if has_leaf(0x8000_0008) {
        __cpuid(0x8000_0008).ebx
    } else {
        0
    };
    let amd21 = if has_leaf(0x8000_0021) {
        __cpuid(0x8000_0021).eax
    } else {
        0
    };
    let bit = |word: u32, n: u32| word & (1 << n) != 0;
    let capabilities = if bit(leaf7, 29) {
        // SAFETY: (PROTECT) CPUID.7.EDX[29] says the register exists.
        unsafe { cpu::read_msr(IA32_ARCH_CAPABILITIES) }
    } else {
        0
    };
    Offered {
        intel,
        amd,
        ibrs: bit(leaf7, 26) || bit(amd8, 14),
        ibpb: bit(leaf7, 26) || bit(amd8, 12),
        stibp: bit(leaf7, 27) || bit(amd8, 15),
        ibrs_always_on: bit(amd8, 16),
        ssbd: bit(leaf7, 31) || bit(amd8, 24),
        virt_ssbd: bit(amd8, 25),
        amd_ssb_no: bit(amd8, 26),
        auto_ibrs: bit(amd21, 8),
        eraps: bit(amd21, ERAPS_BIT),
        md_clear: bit(leaf7, 10),
        capabilities,
    }
}

/// What every processor applies: decided on the boot processor, before any
/// other has started.
#[derive(Debug, Clone, Copy, Default)]
struct Plan {
    /// Written to `IA32_SPEC_CTRL`, if not zero.
    spec_ctrl: u64,
    /// Set `EFER.AIBRSE`.
    auto_ibrs: bool,
    /// Write SSBD to AMD's virtual register.
    virt_ssbd: bool,
    /// Every defence the plan amounts to.
    defences: Defences,
}

impl Plan {
    /// The plan for a processor that offers `offered`.
    fn for_processor(offered: &Offered) -> Plan {
        let mut plan = Plan {
            defences: Defences::CLAMPED_INDICES
                .with(Defences::SWAPGS_FENCE)
                .with(Defences::ENTRY_REGISTERS_CLEARED)
                .with(Defences::RSB_FILL),
            ..Plan::default()
        };
        let enhanced = offered.capabilities & CAP_IBRS_ALL != 0;
        if enhanced && offered.ibrs {
            plan.spec_ctrl |= SPEC_CTRL_IBRS;
            plan.defences = plan.defences.with(Defences::IBRS_ENHANCED);
        } else if offered.auto_ibrs {
            plan.auto_ibrs = true;
            plan.defences = plan.defences.with(Defences::IBRS_AUTOMATIC);
        } else if offered.ibrs_always_on && offered.ibrs {
            plan.spec_ctrl |= SPEC_CTRL_IBRS;
            plan.defences = plan.defences.with(Defences::IBRS_ALWAYS_ON);
        }
        // Enhanced IBRS covers the sibling thread as well; nothing else does.
        if offered.stibp && !enhanced {
            plan.spec_ctrl |= SPEC_CTRL_STIBP;
            plan.defences = plan.defences.with(Defences::STIBP);
        }
        if !store_bypass_immune(offered) {
            if offered.ssbd {
                plan.spec_ctrl |= SPEC_CTRL_SSBD;
                plan.defences = plan.defences.with(Defences::SSBD);
            } else if offered.virt_ssbd {
                plan.virt_ssbd = true;
                plan.defences = plan.defences.with(Defences::SSBD);
            }
        }
        if offered.ibpb {
            plan.defences = plan.defences.with(Defences::SWITCH_BARRIER);
        }
        if eraps_empties_on_switch(offered) {
            plan.defences = plan.defences.with(Defences::ERAPS);
        }
        if mds_exposed(offered) {
            plan.defences = plan.defences.with(Defences::BUFFERS_CLEARED);
        }
        plan
    }
}

/// Whether the processor says it cannot bypass a store.
const fn store_bypass_immune(offered: &Offered) -> bool {
    offered.amd_ssb_no || offered.capabilities & CAP_SSB_NO != 0
}

/// Whether the processor may leak its buffers to MDS and can clear them.
///
/// Only Intel parts are affected, and only those without `MDS_NO`; `VERW`
/// clears the buffers only where microcode says so with `MD_CLEAR`.
const fn mds_exposed(offered: &Offered) -> bool {
    offered.intel && offered.md_clear && offered.capabilities & CAP_MDS_NO == 0
}

/// Whether the processor may be exposed to Meltdown and L1TF: an Intel part
/// that does not say `RDCL_NO`. AMD's are not.
const fn meltdown_exposed(offered: &Offered) -> bool {
    offered.intel && offered.capabilities & CAP_RDCL_NO == 0
}

/// What decides whether programs may keep AVX: who made the processor, which
/// one it is, and what its microcode and a hypervisor say.
#[derive(Debug, Clone, Copy, Default)]
struct VectorFacts {
    /// `GenuineIntel`.
    intel: bool,
    /// `AuthenticAMD` or `HygonGenuine`.
    amd: bool,
    /// The display family, extended family added.
    family: u32,
    /// The display model, extended model added.
    model: u32,
    /// `CPUID.1:ECX[31]`: running under a hypervisor.
    hypervisor: bool,
    /// The processor offers AVX at all.
    avx: bool,
    /// `IA32_ARCH_CAPABILITIES`, or zero.
    capabilities: u64,
    /// AMD's microcode patch level, read only on a Zen 2 part outside a
    /// hypervisor.
    patch_level: u32,
    /// `IA32_MCU_OPT_CTRL`, read only where `GDS_CTRL` says it exists.
    mcu_opt_ctrl: u64,
}

/// What letting programs use AVX exposes on this processor, and what covers
/// it (`docs/certification/SPECULATION.md` §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VectorLeak {
    /// Built `--mitigations off`: nothing is decided.
    SwitchOff,
    /// Neither Zenbleed nor Gather Data Sampling applies.
    NotAffected,
    /// Zenbleed, closed by microcode at or past the fixed revision.
    ZenbleedMicrocode,
    /// Zenbleed, closed by `DE_CFG`'s chicken bit, set on every processor.
    ZenbleedChickenBit,
    /// Gather Data Sampling, closed by microcode whose mitigation is on.
    GdsMicrocode,
    /// Gather Data Sampling, closed once the kernel turns the microcode's
    /// mitigation back on, which it does before any program runs.
    GdsMicrocodeEnabled,
    /// Uncovered: programs do not get AVX. Says why.
    Uncovered(&'static str),
}

impl VectorLeak {
    /// Whether programs may use AVX.
    pub(crate) const fn allows_avx(self) -> bool {
        !matches!(self, VectorLeak::Uncovered(_))
    }

    /// For the boot log.
    pub(crate) const fn describe(self) -> &'static str {
        match self {
            VectorLeak::SwitchOff => "not decided: built with --mitigations off",
            VectorLeak::NotAffected => "Zenbleed and GDS do not apply",
            VectorLeak::ZenbleedMicrocode => "Zenbleed closed by microcode",
            VectorLeak::ZenbleedChickenBit => "Zenbleed closed by DE_CFG[9]",
            VectorLeak::GdsMicrocode => "GDS closed by microcode",
            VectorLeak::GdsMicrocodeEnabled => "GDS closed by microcode, turned back on",
            VectorLeak::Uncovered(why) => why,
        }
    }
}

/// Zen 2, the parts Zenbleed reads another program's vector registers on:
/// family 0x17, these models (Linux's `amd_zenbleed`).
const fn is_zen2(facts: &VectorFacts) -> bool {
    facts.amd
        && facts.family == 0x17
        && matches!(facts.model, 0x30..=0x4f | 0x60..=0x7f | 0x90..=0x91 | 0xa0..=0xaf)
}

/// The first patch level with Zenbleed's fix, by model; none known for the
/// rest (Linux's `cpu_has_zenbleed_microcode`).
const fn zenbleed_fixed_at(model: u32) -> Option<u32> {
    match model {
        0x30..=0x3f => Some(0x0830_107b),
        0x60..=0x67 => Some(0x0860_010c),
        0x68..=0x6f => Some(0x0860_8107),
        0x70..=0x7f => Some(0x0870_1033),
        0xa0..=0xaf => Some(0x08a0_0009),
        _ => None,
    }
}

/// Intel's parts that Gather Data Sampling samples stale vector data on:
/// family 6, these models (Linux's `cpu_vuln_blacklist` entries with `GDS`:
/// Skylake to Rocket Lake, Ice Lake and Tiger Lake).
const fn is_gds_model(facts: &VectorFacts) -> bool {
    facts.intel
        && facts.family == 6
        && matches!(
            facts.model,
            0x4e | 0x5e
                | 0x55
                | 0x8e
                | 0x9e
                | 0xa5
                | 0xa6
                | 0x6a
                | 0x6c
                | 0x7e
                | 0x8c
                | 0x8d
                | 0xa7
        )
}

/// Zenbleed's verdict for a Zen 2 part. A guest can neither read the host's
/// microcode nor set its chicken bit, so it is uncovered, as a strict
/// reading of Linux's "trust the hypervisor" must be.
const fn zenbleed(facts: &VectorFacts) -> VectorLeak {
    if facts.hypervisor {
        return VectorLeak::Uncovered(
            "AVX held back: Zenbleed, on a Zen 2 guest that cannot see the host's fix",
        );
    }
    match zenbleed_fixed_at(facts.model) {
        Some(fixed) if facts.patch_level >= fixed => VectorLeak::ZenbleedMicrocode,
        _ => VectorLeak::ZenbleedChickenBit,
    }
}

/// Gather Data Sampling's verdict for an affected Intel model. `GDS_NO` is
/// the processor's, or a hypervisor's for a mitigated host, as KVM gives it.
const fn gds(facts: &VectorFacts) -> VectorLeak {
    if facts.capabilities & CAP_GDS_NO != 0 {
        return VectorLeak::NotAffected;
    }
    if facts.capabilities & CAP_GDS_CTRL == 0 {
        return VectorLeak::Uncovered("AVX held back: GDS, and no microcode mitigates it");
    }
    if facts.mcu_opt_ctrl & MCU_GDS_MITG_DIS == 0 {
        VectorLeak::GdsMicrocode
    } else if facts.mcu_opt_ctrl & MCU_GDS_MITG_LOCKED != 0 {
        VectorLeak::Uncovered("AVX held back: GDS, its microcode mitigation off and locked")
    } else {
        VectorLeak::GdsMicrocodeEnabled
    }
}

/// Whether every processor sets Zenbleed's chicken bit as it starts.
static PLAN_ZENBLEED_BIT: AtomicBool = AtomicBool::new(false);
/// Whether every processor turns the GDS microcode mitigation back on.
static PLAN_GDS_ENABLE: AtomicBool = AtomicBool::new(false);

/// The display family and model `CPUID.1:EAX` names, the extended parts
/// added where the base family says they count.
const fn family_and_model(eax: u32) -> (u32, u32) {
    let base_family = (eax >> 8) & 0xf;
    let base_model = (eax >> 4) & 0xf;
    let family = if base_family == 0xf {
        base_family + ((eax >> 20) & 0xff)
    } else {
        base_family
    };
    let model = if base_family == 0xf || base_family == 6 {
        base_model | (((eax >> 16) & 0xf) << 4)
    } else {
        base_model
    };
    (family, model)
}

/// What this processor says, for [`decide_vector_leak`].
fn vector_facts(offered: &Offered) -> VectorFacts {
    let leaf1 = __cpuid(1);
    let (family, model) = family_and_model(leaf1.eax);
    let mut facts = VectorFacts {
        intel: offered.intel,
        amd: offered.amd,
        family,
        model,
        hypervisor: leaf1.ecx & (1 << 31) != 0,
        avx: leaf1.ecx & (1 << 28) != 0,
        capabilities: offered.capabilities,
        ..VectorFacts::default()
    };
    if is_zen2(&facts) && !facts.hypervisor {
        // SAFETY: (PROTECT) the patch level register exists on every AMD
        // processor since the K8, and this is a Zen 2 outside a guest.
        facts.patch_level = unsafe { cpu::read_msr(AMD_PATCH_LEVEL) } as u32;
    }
    if is_gds_model(&facts) && facts.capabilities & CAP_GDS_CTRL != 0 {
        // SAFETY: (PROTECT) `GDS_CTRL` says `IA32_MCU_OPT_CTRL` exists.
        facts.mcu_opt_ctrl = unsafe { cpu::read_msr(IA32_MCU_OPT_CTRL) };
    }
    facts
}

/// Decide what letting programs use AVX would expose, cover it on the boot
/// processor where that is possible, and answer the verdict:
/// `cpu::enable_extended_state` leaves AVX out of `XCR0` for an uncovered
/// one. Before `init`, since `XCR0` is written in `init_traps`; `apply` does
/// the same covering on every processor after.
pub(crate) fn vector_leak() -> VectorLeak {
    if !HARDENED {
        return VectorLeak::SwitchOff;
    }
    let verdict = decide_vector_leak(&vector_facts(&offered()));
    match verdict {
        VectorLeak::ZenbleedChickenBit => {
            PLAN_ZENBLEED_BIT.store(true, Ordering::Relaxed);
            if set_zenbleed_bit() {
                verdict
            } else {
                VectorLeak::Uncovered("AVX held back: Zenbleed, and DE_CFG[9] did not hold")
            }
        }
        VectorLeak::GdsMicrocodeEnabled => {
            PLAN_GDS_ENABLE.store(true, Ordering::Relaxed);
            if enable_gds_mitigation() {
                verdict
            } else {
                VectorLeak::Uncovered("AVX held back: GDS, and its mitigation did not turn on")
            }
        }
        other => other,
    }
}

/// Set `DE_CFG[9]` on this processor, and say whether it reads back set.
fn set_zenbleed_bit() -> bool {
    // SAFETY: (PROTECT) `DE_CFG` exists on every Zen part; only a Zen 2
    // outside a guest plans this.
    let config = unsafe { cpu::read_msr(AMD_DE_CFG) };
    // SAFETY: (PROTECT) as above; bit 9 is the documented backup fix.
    unsafe { cpu::write_msr(AMD_DE_CFG, config | DE_CFG_ZEN2_FP_BACKUP_FIX) };
    // SAFETY: (PROTECT) as above.
    let read = unsafe { cpu::read_msr(AMD_DE_CFG) };
    read & DE_CFG_ZEN2_FP_BACKUP_FIX != 0
}

/// Clear `GDS_MITG_DIS` on this processor, and say whether it reads back
/// clear.
fn enable_gds_mitigation() -> bool {
    // SAFETY: (PROTECT) only planned where `GDS_CTRL` says the register exists.
    let control = unsafe { cpu::read_msr(IA32_MCU_OPT_CTRL) };
    // SAFETY: (PROTECT) as above, and the setting is not locked, or the plan
    // would have left AVX out instead.
    unsafe { cpu::write_msr(IA32_MCU_OPT_CTRL, control & !MCU_GDS_MITG_DIS) };
    // SAFETY: (PROTECT) as above.
    let read = unsafe { cpu::read_msr(IA32_MCU_OPT_CTRL) };
    read & MCU_GDS_MITG_DIS == 0
}

/// The verdict for a processor that says `facts`.
const fn decide_vector_leak(facts: &VectorFacts) -> VectorLeak {
    if !facts.avx {
        VectorLeak::NotAffected
    } else if is_zen2(facts) {
        zenbleed(facts)
    } else if is_gds_model(facts) {
        gds(facts)
    } else {
        VectorLeak::NotAffected
    }
}

/// Whether `VERW` runs on every return to ring 3. Read by the exits in
/// `syscall` and `trap`, from assembly, as a byte.
pub(super) static CLEAR_CPU_BUFFERS: AtomicU8 = AtomicU8::new(0);

/// The memory operand `VERW` takes: a writable data segment's selector. The
/// buffer clearing is a side effect of the memory form only.
pub(super) static VERW_SELECTOR: u16 = super::gdt::KERNEL_DATA;

/// Whether the entry paths' extra instructions are assembled in: `1` or `0`,
/// for an assembler `.if`.
pub(super) const ENTRY_HARDENING: u8 = HARDENED as u8;

/// The plan, for the secondaries: `IA32_SPEC_CTRL`'s value.
static PLAN_SPEC_CTRL: AtomicU64 = AtomicU64::new(0);
/// The plan, for the secondaries: its defences.
static PLAN_DEFENCES: AtomicU32 = AtomicU32::new(0);
/// The plan, for the secondaries: `EFER.AIBRSE`.
static PLAN_AUTO_IBRS: AtomicBool = AtomicBool::new(false);
/// The plan, for the secondaries: AMD's virtual SSBD.
static PLAN_VIRT_SSBD: AtomicBool = AtomicBool::new(false);
/// Whether a switch of address space issues `IBPB`.
static SWITCH_IBPB: AtomicBool = AtomicBool::new(false);

/// Decide, apply on the boot processor, and say what was done.
///
/// Before any secondary starts, which each read the plan, and before the
/// first program.
pub(crate) fn init() {
    if !HARDENED {
        println!("  cpu      speculation defences off: built with --mitigations off");
        record_this_cpu(Defences::NONE);
        return;
    }
    let offered = offered();
    let plan = Plan::for_processor(&offered);
    PLAN_SPEC_CTRL.store(plan.spec_ctrl, Ordering::Relaxed);
    PLAN_AUTO_IBRS.store(plan.auto_ibrs, Ordering::Relaxed);
    PLAN_VIRT_SSBD.store(plan.virt_ssbd, Ordering::Relaxed);
    SWITCH_IBPB.store(
        plan.defences.contains(Defences::SWITCH_BARRIER),
        Ordering::Relaxed,
    );
    ERAPS_EMPTIES.store(plan.defences.contains(Defences::ERAPS), Ordering::Relaxed);
    CLEAR_CPU_BUFFERS.store(
        u8::from(plan.defences.contains(Defences::BUFFERS_CLEARED)),
        Ordering::Relaxed,
    );
    PLAN_DEFENCES.store(plan.defences.bits(), Ordering::Release);

    let applied = apply(&plan);
    record_this_cpu(applied);
    println!("  cpu      speculation defences: {}", applied.names());
    report_exposure(&offered, plan.defences);
}

/// Say what the plan leaves uncovered, and why.
fn report_exposure(offered: &Offered, defences: Defences) {
    let v2 = if defences.contains(Defences::IBRS_ENHANCED)
        || defences.contains(Defences::IBRS_AUTOMATIC)
        || defences.contains(Defences::IBRS_ALWAYS_ON)
    {
        "covered"
    } else {
        "NOT covered: no eIBRS, AutoIBRS or always-on IBRS offered (AoU-11)"
    };
    let meltdown = if meltdown_exposed(offered) {
        "EXPOSED: no RDCL_NO, and KPTI is not built (AoU-11)"
    } else if offered.intel {
        "not affected (RDCL_NO)"
    } else if offered.amd {
        "not affected (AMD)"
    } else {
        "not known to be affected"
    };
    let bypass = if store_bypass_immune(offered) {
        "not affected"
    } else if defences.contains(Defences::SSBD) {
        "covered"
    } else {
        "NOT covered: no SSBD offered (AoU-11)"
    };
    let between = if defences.contains(Defences::SWITCH_BARRIER) {
        "covered"
    } else {
        "NOT covered: no IBPB offered (AoU-11)"
    };
    println!(
        "  cpu      speculation exposure: Spectre v2 into ring 0 {v2}; between programs \
         {between}; store bypass {bypass}; Meltdown {meltdown}"
    );
}

/// Apply the plan on this processor, read it back, and say what took.
fn apply(plan: &Plan) -> Defences {
    let mut held = true;
    if plan.spec_ctrl != 0 {
        // SAFETY: (PROTECT) each bit is in the plan only because CPUID said the
        // register exists and takes it.
        unsafe { cpu::write_msr(IA32_SPEC_CTRL, plan.spec_ctrl) };
        // SAFETY: (PROTECT) as above, the register exists.
        let read = unsafe { cpu::read_msr(IA32_SPEC_CTRL) };
        held &= read & plan.spec_ctrl == plan.spec_ctrl;
    }
    if plan.auto_ibrs {
        // SAFETY: (PROTECT) `IA32_EFER` exists on every processor with long mode.
        let efer = unsafe { cpu::read_msr(IA32_EFER) };
        // SAFETY: (PROTECT) CPUID 0x8000_0021.EAX[8] says the bit is implemented.
        unsafe { cpu::write_msr(IA32_EFER, efer | EFER_AUTO_IBRS) };
        // SAFETY: (PROTECT) as above.
        held &= unsafe { cpu::read_msr(IA32_EFER) } & EFER_AUTO_IBRS != 0;
    }
    if plan.virt_ssbd {
        // SAFETY: (PROTECT) CPUID 0x8000_0008.EBX[25] says the register exists.
        unsafe { cpu::write_msr(AMD_VIRT_SPEC_CTRL, SPEC_CTRL_SSBD) };
        // SAFETY: (PROTECT) as above.
        held &= unsafe { cpu::read_msr(AMD_VIRT_SPEC_CTRL) } & SPEC_CTRL_SSBD != 0;
    }
    held &= apply_vector_cover();
    if held {
        plan.defences
    } else {
        plan.defences.with(Defences::READ_BACK_FAILED)
    }
}

/// What [`vector_leak`] planned for every processor: Zenbleed's chicken bit
/// and the GDS mitigation, each only where the boot processor needed it.
/// Answers whether what was planned reads back.
fn apply_vector_cover() -> bool {
    let mut held = true;
    if PLAN_ZENBLEED_BIT.load(Ordering::Relaxed) {
        held &= set_zenbleed_bit();
    }
    if PLAN_GDS_ENABLE.load(Ordering::Relaxed) {
        held &= enable_gds_mitigation();
    }
    held
}

/// Apply the boot processor's plan on a secondary, as it starts.
pub(crate) fn apply_this_cpu() {
    if !HARDENED {
        record_this_cpu(Defences::NONE);
        return;
    }
    let defences = Defences::from_bits(PLAN_DEFENCES.load(Ordering::Acquire));
    let plan = Plan {
        spec_ctrl: PLAN_SPEC_CTRL.load(Ordering::Relaxed),
        auto_ibrs: PLAN_AUTO_IBRS.load(Ordering::Relaxed),
        virt_ssbd: PLAN_VIRT_SSBD.load(Ordering::Relaxed),
        defences,
    };
    record_this_cpu(apply(&plan));
}

/// Issue the switch barrier: `IBPB`, where the processor offers it, and a
/// return stack refill either way. Answers whether anything was issued,
/// which in a hardened build is always.
///
/// Called with interrupts masked, from `install_user_root`, when the
/// processor is about to run a program other than the one it last ran.
pub(crate) fn switch_barrier(_cpu: usize) -> bool {
    if SWITCH_IBPB.load(Ordering::Relaxed) {
        // SAFETY: (PROTECT) `SWITCH_IBPB` is set only when CPUID said `IA32_PRED_CMD`
        // takes IBPB; the write empties predictors and changes nothing else.
        unsafe { cpu::write_msr(IA32_PRED_CMD, PRED_CMD_IBPB) };
    }
    fill_return_stack();
    HARDENED
}

/// The switch barrier between two programs of one speculation domain: the
/// return stack refill alone, without `IBPB` (`docs/OPAQUE-KERNEL.md`
/// §9.3a, A2). The refill stays because it costs a few hundred cycles and
/// nothing then has to be argued about what else it protects.
pub(crate) fn switch_barrier_in_domain(_cpu: usize) -> bool {
    if ERAPS_EMPTIES.load(Ordering::Relaxed) {
        // The `CR3` write just made emptied the return address predictor:
        // ERAPS, where `CR4.PCIDE` is clear so that every write flushes
        // (`eraps_empties_on_switch`). Nothing is refilled, and nothing is
        // counted as a refill.
        return false;
    }
    fill_return_stack();
    HARDENED
}

/// Whether a `CR3` write on this machine empties the return address
/// predictor: decided at boot from the boot processor's `CPUID`, and read
/// by every in-domain switch.
static ERAPS_EMPTIES: AtomicBool = AtomicBool::new(false);

/// Whether ERAPS stands in for the in-domain refill: a hardened build, a
/// processor that offers ERAPS, and `CR4.PCIDE` clear, so that every `CR3`
/// write the switch makes is one that flushes the TLB, which is the write
/// ERAPS empties the predictor at. Every processor runs with the boot
/// processor's `CR4` bits.
fn eraps_empties_on_switch(offered: &Offered) -> bool {
    HARDENED && offered.eraps && cpu::read_cr4() & CR4_PCIDE == 0
}

/// [`crate::arch::speculation::refill_wanted_in_domain`]: in a hardened
/// build, unless `CPUID` itself, read again here, says ERAPS and `CR4.PCIDE`
/// is clear.
pub(crate) fn refill_wanted_in_domain() -> bool {
    REFILL_IN_DOMAIN && !eraps_empties_on_switch(&offered())
}

/// Whether [`switch_barrier_in_domain`] refills the return stack: in a
/// hardened build.
pub(crate) const REFILL_IN_DOMAIN: bool = HARDENED;

/// Overwrite the return stack buffer with thirty-two harmless entries.
///
/// Each `call` pushes its return address -- the `int3` after it, which a
/// mispredicted `ret` would land on and stop -- onto both the stack and the
/// predictor; the `add` takes them back off the stack and leaves them in the
/// predictor, where they displace whatever the last program left. The
/// `lfence` keeps anything after from running before the refill has.
fn fill_return_stack() {
    if !HARDENED {
        return;
    }
    // SAFETY: (PROTECT) thirty-two calls to the next instruction and the stack put
    // back as it was; no register but the stack pointer is touched, and that
    // is restored. The kernel's target has no red zone to overwrite.
    unsafe {
        asm!(
            ".rept 32",
            "call 2f",
            "int3",
            "2:",
            ".endr",
            "add rsp, 256",
            "lfence",
            options(preserves_flags),
        );
    }
}

/// `index` if `index < len`, else zero, without a branch: `cmp` sets the
/// carry exactly when `index < len`, and `sbb` of a register from itself
/// turns the carry into all ones or all zeros.
#[inline(always)]
pub(crate) fn clamp_index(index: usize, len: usize) -> usize {
    if !HARDENED {
        return index;
    }
    let mask: usize;
    // SAFETY: (PROTECT) two register instructions; no memory, no stack.
    unsafe {
        asm!(
            "cmp {index}, {len}",
            "sbb {mask}, {mask}",
            index = in(reg) index,
            len = in(reg) len,
            mask = out(reg) mask,
            options(pure, nomem, nostack),
        );
    }
    index & mask
}

/// [`clamp_index`], for a 64-bit value: the same thing on this machine.
#[inline(always)]
pub(crate) fn clamp_below(value: u64, end: u64) -> u64 {
    clamp_index(value as usize, end as usize) as u64
}

/// Run `VERW` once, as every return to ring 3 does on an MDS-exposed
/// processor: for the boot check, which shows the operand is one the
/// instruction accepts wherever that path is not taken.
pub(crate) fn clear_cpu_buffers() {
    // SAFETY: (PROTECT) `VERW` of a readable selector only sets `ZF` and, on a
    // processor with `MD_CLEAR`, clears buffers; the operand is a static.
    unsafe {
        asm!(
            "verw word ptr [{selector}]",
            selector = in(reg) &raw const VERW_SELECTOR,
            options(nostack),
        );
    }
}

/// Nothing to add once every processor has started: `init` decided for all
/// of them, from the boot processor's `CPUID`, and printed the exposure then.
pub(crate) fn report_once_started() {}

/// What the boot check adds on this architecture: run `VERW` once, so a
/// machine whose exit path never runs it -- every one without MDS -- still
/// shows the instruction takes its operand.
///
/// # Errors
///
/// None: a `VERW` that did not take its operand would fault, not return.
pub(crate) fn check() -> Result<(), &'static str> {
    if HARDENED {
        clear_cpu_buffers();
    }
    check::vector_leak_decisions()
}
