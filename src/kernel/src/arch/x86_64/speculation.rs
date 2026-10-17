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
//! | RFDS | the same `VERW`, which clears the register file too where microcode says `RFDS_CLEAR` | Intel parts Linux lists, any Intel model it does not know and any Intel guest, unless `RFDS_NO` |
//! | BHI | `BHI_DIS_S` in `IA32_SPEC_CTRL` where `BHI_CTRL` is offered, else Linux's branch-history clearing loop on every entry from ring 3 | every processor but AMD's and Hygon's that has enhanced IBRS or runs under a hypervisor, `BHI_NO` notwithstanding |
//! | Meltdown, L1TF | none: KPTI is not built | reported, and excluded by the safety manual's AoU-11 |
//! | ITS, MMIO stale data, SRBDS, TAA, Retbleed, SRSO, TSA | none but eIBRS for Intel's Retbleed and `IBPB` for SRSO between programs where the part has `SRSO_USER_KERNEL_NO` | decided from Linux's tables and reported, `NOT covered` where affected; VMSCAPE does not apply, the item being no hypervisor |
//!
//! The model tables are Linux 6.18.54's (`arch/x86/kernel/cpu/common.c`,
//! `cpu_vuln_whitelist` and `cpu_vuln_blacklist`, sha256 `7eb0cd22...3a72`;
//! the loop is `arch/x86/entry/entry_64.S`'s `clear_bhb_loop`, sha256
//! `8c90ad9f...ebdb`), read from the tree at
//! `~/crp123i-port/clean/linux-6.18.54` on the gate host. Every hazard is
//! named on the exposure lines, so one the item does not cover says so
//! rather than reading as covered by being left out.
//!
//! What a processor that offers none of the IBRS forms gets is a line in the
//! boot log saying so. Retpolines would be the software answer, and the
//! pinned stable compiler has them only through a deprecated target feature
//! that is scheduled to become an error, and not in the precompiled `core`
//! and `alloc` the kernel links -- §3 has the details.

use core::arch::asm;
use core::arch::x86_64::{__cpuid, __cpuid_count, CpuidResult};
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

/// `IA32_SPEC_CTRL.BHI_DIS_S`: a program's branch history does not steer
/// the kernel's indirect branches, where `CPUID.(7,2):EDX[4]` (`BHI_CTRL`)
/// offers it. Linux's `SPEC_CTRL_BHI_DIS_S`.
const SPEC_CTRL_BHI_DIS_S: u64 = 1 << 10;

/// `IA32_ARCH_CAPABILITIES.RDCL_NO`: not vulnerable to Meltdown.
const CAP_RDCL_NO: u64 = 1 << 0;
/// `IA32_ARCH_CAPABILITIES.IBRS_ALL`: enhanced IBRS.
const CAP_IBRS_ALL: u64 = 1 << 1;
/// `IA32_ARCH_CAPABILITIES.RSBA`: a `ret` may be predicted from the
/// indirect predictor (Linux counts it Retbleed-affected).
const CAP_RSBA: u64 = 1 << 2;
/// `IA32_ARCH_CAPABILITIES.SSB_NO`: not vulnerable to store bypass.
const CAP_SSB_NO: u64 = 1 << 4;
/// `IA32_ARCH_CAPABILITIES.MDS_NO`: not vulnerable to MDS.
const CAP_MDS_NO: u64 = 1 << 5;
/// `IA32_ARCH_CAPABILITIES.TSX_CTRL_MSR`: TSX can be turned off.
const CAP_TSX_CTRL: u64 = 1 << 7;
/// `IA32_ARCH_CAPABILITIES.TAA_NO`: not vulnerable to TSX Asynchronous Abort.
const CAP_TAA_NO: u64 = 1 << 8;
/// `IA32_ARCH_CAPABILITIES.SBDR_SSDP_NO`, one of MMIO stale data's three.
const CAP_SBDR_SSDP_NO: u64 = 1 << 13;
/// `IA32_ARCH_CAPABILITIES.FBSDP_NO`, one of MMIO stale data's three.
const CAP_FBSDP_NO: u64 = 1 << 14;
/// `IA32_ARCH_CAPABILITIES.PSDP_NO`, one of MMIO stale data's three.
const CAP_PSDP_NO: u64 = 1 << 15;
/// `IA32_ARCH_CAPABILITIES.BHI_NO`: not vulnerable to Branch History
/// Injection across modes. Decoded, and on purpose not trusted alone: Linux
/// 6.18.54 keeps its BHI defence on such a part, against intra-mode BHI
/// (`cpu_set_bug_bits`).
const CAP_BHI_NO: u64 = 1 << 20;
/// `IA32_ARCH_CAPABILITIES.RFDS_NO`: not vulnerable to Register File Data
/// Sampling.
const CAP_RFDS_NO: u64 = 1 << 27;
/// `IA32_ARCH_CAPABILITIES.RFDS_CLEAR`: `VERW` clears the register file;
/// set by a hypervisor for a guest whose host may be affected.
const CAP_RFDS_CLEAR: u64 = 1 << 28;
/// `IA32_ARCH_CAPABILITIES.ITS_NO`: not vulnerable to Indirect Target
/// Selection.
const CAP_ITS_NO: u64 = 1 << 62;
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
    /// The display family and model, as [`family_and_model`] reads them.
    family: u32,
    /// The display model.
    model: u32,
    /// `CPUID.1:ECX[31]`: running under a hypervisor, whose model and
    /// capabilities need not be the host's.
    hypervisor: bool,
    /// `CPUID.(7,0):EDX[15]`: a hybrid part, cores of more than one type.
    hybrid: bool,
    /// `CPUID.1AH:EAX[31:24]` is `0x20`: this core is an Atom core.
    atom_core: bool,
    /// `CPUID.(7,2):EDX[4]`: `IA32_SPEC_CTRL` takes `BHI_DIS_S`.
    bhi_ctrl: bool,
    /// `CPUID.1:ECX[30]` or `CPUID.(7,0):EBX[18]`: `RDRAND` or `RDSEED`.
    random: bool,
    /// `CPUID.(7,0):EBX[11]`: TSX's restricted transactional memory.
    rtm: bool,
    /// AMD: `CPUID 0x8000_0008.EBX[29]`, not vulnerable to branch type
    /// confusion (Retbleed).
    btc_no: bool,
    /// AMD: `CPUID 0x8000_0021.EAX[28]`, `IBPB` flushes every branch type's
    /// predictions, returns included.
    ibpb_brtype: bool,
    /// AMD: `CPUID 0x8000_0021.EAX[29]`, not vulnerable to SRSO.
    srso_no: bool,
    /// AMD: `CPUID 0x8000_0021.EAX[30]`, not vulnerable to SRSO from ring 3
    /// into ring 0.
    srso_user_kernel_no: bool,
    /// AMD: `CPUID 0x8000_0021.ECX[1]` and `[2]` both, not vulnerable to
    /// either kind of Transient Scheduler Attack.
    tsa_no: bool,
}

/// Whether `leaf` is at or below the highest leaf of its range.
fn has_leaf(leaf: u32) -> bool {
    __cpuid(leaf & 0x8000_0000).eax >= leaf
}

/// `CPUID` `leaf`, sub-leaf `subleaf`, or zero in every register where
/// `leaf` is above the highest of its range.
fn leaf_or_zero(leaf: u32, subleaf: u32) -> CpuidResult {
    if has_leaf(leaf) {
        __cpuid_count(leaf, subleaf)
    } else {
        CpuidResult {
            eax: 0,
            ebx: 0,
            ecx: 0,
            edx: 0,
        }
    }
}

/// Whether `CPUID` names Intel, and whether it names AMD or Hygon.
fn vendor() -> (bool, bool) {
    let vendor = __cpuid(0);
    let is = |name: &[u8; 12]| {
        let bytes = [vendor.ebx, vendor.edx, vendor.ecx];
        bytes
            .iter()
            .flat_map(|word| word.to_le_bytes())
            .eq(name.iter().copied())
    };
    (
        is(b"GenuineIntel"),
        is(b"AuthenticAMD") || is(b"HygonGenuine"),
    )
}

/// `CPUID` and `IA32_ARCH_CAPABILITIES`, read.
fn offered() -> Offered {
    let (intel, amd) = vendor();
    let leaf1 = __cpuid(1);
    let leaf7 = leaf_or_zero(7, 0);
    // Sub-leaf 2 only where sub-leaf 0's EAX says there is one.
    let leaf7_2 = if leaf7.eax >= 2 {
        leaf_or_zero(7, 2).edx
    } else {
        0
    };
    let core_type = leaf_or_zero(0x1a, 0).eax >> 24;
    let amd8 = leaf_or_zero(0x8000_0008, 0).ebx;
    let amd21 = leaf_or_zero(0x8000_0021, 0);
    let (family, model) = family_and_model(leaf1.eax);
    let bit = |word: u32, n: u32| word & (1 << n) != 0;
    let capabilities = if bit(leaf7.edx, 29) {
        // SAFETY: (PROTECT) CPUID.7.EDX[29] says the register exists.
        unsafe { cpu::read_msr(IA32_ARCH_CAPABILITIES) }
    } else {
        0
    };
    Offered {
        intel,
        amd,
        ibrs: bit(leaf7.edx, 26) || bit(amd8, 14),
        ibpb: bit(leaf7.edx, 26) || bit(amd8, 12),
        stibp: bit(leaf7.edx, 27) || bit(amd8, 15),
        ibrs_always_on: bit(amd8, 16),
        ssbd: bit(leaf7.edx, 31) || bit(amd8, 24),
        virt_ssbd: bit(amd8, 25),
        amd_ssb_no: bit(amd8, 26),
        auto_ibrs: bit(amd21.eax, 8),
        eraps: bit(amd21.eax, ERAPS_BIT),
        md_clear: bit(leaf7.edx, 10),
        capabilities,
        family,
        model,
        hypervisor: bit(leaf1.ecx, 31),
        hybrid: bit(leaf7.edx, 15),
        atom_core: core_type == INTEL_CORE_TYPE_ATOM,
        bhi_ctrl: bit(leaf7_2, 4),
        random: bit(leaf1.ecx, 30) || bit(leaf7.ebx, 18),
        rtm: bit(leaf7.ebx, 11),
        btc_no: bit(amd8, 29),
        ibpb_brtype: bit(amd21.eax, 28),
        srso_no: bit(amd21.eax, 29),
        srso_user_kernel_no: bit(amd21.eax, 30),
        tsa_no: bit(amd21.ecx, 1) && bit(amd21.ecx, 2),
    }
}

/// `CPUID.1AH:EAX[31:24]` of an Atom core (Linux's `INTEL_CPU_TYPE_ATOM`).
const INTEL_CORE_TYPE_ATOM: u32 = 0x20;

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
        plan.add_ring3_crossings(offered);
        plan
    }

    /// What the plan adds at the ring 3 boundary: `VERW` on every return,
    /// for MDS and RFDS alike -- it clears MDS's buffers where microcode
    /// says `MD_CLEAR` and the register file where it says `RFDS_CLEAR` --
    /// and BHI's defence for every entry.
    fn add_ring3_crossings(&mut self, offered: &Offered) {
        if mds_exposed(offered) || rfds(offered) == Rfds::Cleared {
            self.defences = self.defences.with(Defences::BUFFERS_CLEARED);
        }
        match bhi(offered) {
            Bhi::HistoryDisabled => {
                self.spec_ctrl |= SPEC_CTRL_BHI_DIS_S;
                self.defences = self.defences.with(Defences::BHI_DIS_S);
            }
            Bhi::Loop => self.defences = self.defences.with(Defences::BHB_LOOP),
            Bhi::NotAffected(_) => {}
        }
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

/// Register File Data Sampling: what the processor is, and what covers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rfds {
    /// Not affected, and why.
    NotAffected(&'static str),
    /// Affected, and `RFDS_CLEAR` says `VERW` clears the register file: it
    /// runs on every return to ring 3.
    Cleared,
    /// Affected, and microcode without `RFDS_CLEAR`: nothing clears it.
    NoMicrocode,
}

/// The family 6 models Linux 6.18.54 lists for RFDS (`cpu_vuln_blacklist`'s
/// `RFDS` entries): Goldmont, Goldmont D and Goldmont Plus; Tremont, Tremont
/// D and Tremont L; Gracemont (Alder Lake-N, the N150); and the Alder and
/// Raptor Lake parts that carry Gracemont E-cores.
const RFDS_MODELS: [u32; 12] = [
    0x5c, 0x5f, 0x7a, 0x96, 0x86, 0x9c, 0xbe, 0x97, 0x9a, 0xb7, 0xba, 0xbf,
];

/// The family 6 models Linux 6.18.54 has any entry for, in
/// `cpu_vuln_whitelist` or `cpu_vuln_blacklist`, other than
/// [`RFDS_MODELS`]: those whose RFDS verdict Linux states by leaving them off
/// the list. Any other model is one the tables do not know, which [`rfds`]
/// takes as affected.
const KNOWN_MODELS: [u32; 50] = [
    0x0e, 0x1c, 0x26, 0x27, 0x2a, 0x2d, 0x35, 0x36, 0x37, 0x3a, 0x3c, 0x3d, 0x3e, 0x3f, 0x45, 0x46,
    0x47, 0x4a, 0x4c, 0x4d, 0x4e, 0x4f, 0x55, 0x56, 0x57, 0x5a, 0x5e, 0x66, 0x6a, 0x6c, 0x75, 0x7e,
    0x85, 0x8a, 0x8c, 0x8d, 0x8e, 0x8f, 0x9e, 0xa5, 0xa6, 0xa7, 0xaa, 0xad, 0xaf, 0xb5, 0xbd, 0xc5,
    0xc6, 0xcf,
];

/// The family 6 models Linux 6.18.54 lists for ITS: Skylake X, Kaby Lake
/// and Kaby Lake L (their later steppings there, every one here), Ice Lake
/// L, D and X, Comet Lake and Comet Lake L, Tiger Lake and Tiger Lake L,
/// Rocket Lake.
const ITS_MODELS: [u32; 11] = [
    0x55, 0x8e, 0x9e, 0x7e, 0x6c, 0x6a, 0xa5, 0xa6, 0x8c, 0x8d, 0xa7,
];

/// The family 6 models Linux 6.18.54 lists for MMIO stale data: Haswell X,
/// Broadwell D and X, Skylake X, L and S, Kaby Lake L and S, Ice Lake L, D
/// and X, Comet Lake and Comet Lake L, Lakefield, Rocket Lake, Tremont,
/// Tremont D and Tremont L.
const MMIO_MODELS: [u32; 18] = [
    0x3f, 0x56, 0x4f, 0x55, 0x4e, 0x5e, 0x8e, 0x9e, 0x7e, 0x6c, 0x6a, 0xa5, 0xa6, 0x8a, 0xa7, 0x96,
    0x86, 0x9c,
];

/// The family 6 models Linux 6.18.54 lists for SRBDS, and for MMIO_SBDS,
/// which it treats as SRBDS: Ivy Bridge, Haswell, L and G, Broadwell and G,
/// Skylake L and S, Kaby Lake L and S; Ice Lake L, Comet Lake and L,
/// Lakefield, Tremont and Tremont L.
const SRBDS_MODELS: [u32; 16] = [
    0x3a, 0x3c, 0x45, 0x46, 0x47, 0x3d, 0x4e, 0x5e, 0x8e, 0x9e, 0x7e, 0xa5, 0xa6, 0x8a, 0x96, 0x9c,
];

/// The family 6 models Linux 6.18.54 lists for Retbleed: Skylake X, L and
/// S, Kaby Lake L and S, Cannon Lake L, Ice Lake L, Comet Lake and L,
/// Lakefield, Rocket Lake.
const RETBLEED_MODELS: [u32; 11] = [
    0x55, 0x4e, 0x5e, 0x8e, 0x9e, 0x66, 0x7e, 0xa5, 0xa6, 0x8a, 0xa7,
];

/// Whether `model` is in `models`.
const fn listed(model: u32, models: &[u32]) -> bool {
    let mut index = 0;
    while index < models.len() {
        if models[index] == model {
            return true;
        }
        index += 1;
    }
    false
}

/// RFDS's verdict for a processor that says `offered`.
///
/// Linux's `vulnerable_to_rfds`, closed where it is open: `RFDS_NO` is
/// believed first and `RFDS_CLEAR` next, as there; an Intel part under a
/// hypervisor, whose model need not be the host's, is affected without
/// `RFDS_NO` (KVM gives `RFDS_NO` for an unaffected host); and so is an
/// Intel model the tables do not know, where Linux would answer not
/// affected. An Alder or Raptor Lake (0x97, 0xb7) with no Atom core is not.
const fn rfds(offered: &Offered) -> Rfds {
    let capabilities = offered.capabilities;
    if capabilities & CAP_RFDS_NO != 0 {
        return Rfds::NotAffected("RFDS_NO");
    }
    let affected = if capabilities & CAP_RFDS_CLEAR != 0 {
        true
    } else if !offered.intel {
        return Rfds::NotAffected(if offered.amd { "AMD" } else { "not Intel" });
    } else if offered.hypervisor || offered.family != 6 {
        true
    } else if listed(offered.model, &RFDS_MODELS) {
        !(matches!(offered.model, 0x97 | 0xb7) && !offered.hybrid && !offered.atom_core)
    } else {
        !listed(offered.model, &KNOWN_MODELS)
    };
    if !affected {
        Rfds::NotAffected("not on Linux's RFDS list")
    } else if capabilities & CAP_RFDS_CLEAR != 0 {
        Rfds::Cleared
    } else {
        Rfds::NoMicrocode
    }
}

/// Branch History Injection: what the processor is, and what covers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bhi {
    /// Not affected, and why.
    NotAffected(&'static str),
    /// `BHI_DIS_S` set in `IA32_SPEC_CTRL`, and read back like its other bits.
    HistoryDisabled,
    /// The branch history overwritten by `ferrix_clear_bhb` on every entry
    /// from ring 3.
    Loop,
}

/// BHI's verdict for a processor that says `offered`: Linux 6.18.54's
/// `cpu_set_bug_bits`, which marks every processor but those its whitelist
/// gives `NO_BHI` -- AMD's and Hygon's, which [`Offered::amd`] covers, and
/// Centaur's and Zhaoxin's, which are taken as affected here -- when it has
/// enhanced IBRS or runs under a hypervisor that may hide it. `BHI_NO` does
/// not lift it, as there; `bhi_apply_mitigation` sets `BHI_DIS_S` where
/// `BHI_CTRL` offers it and runs the loop otherwise.
///
/// `max`'s exit-barrier elision (`docs/BUILD-FLAGS.md`,
/// `partition_speculation`; the consultant's HR2, ledger 941) asks for a
/// processor "not BHI-affected": that is this answering
/// [`Bhi::NotAffected`], never `BHI_NO` alone.
const fn bhi(offered: &Offered) -> Bhi {
    if offered.amd {
        return Bhi::NotAffected("AMD");
    }
    if offered.capabilities & CAP_IBRS_ALL == 0 && !offered.hypervisor {
        return Bhi::NotAffected("no eIBRS, so Spectre v2's own verdict stands");
    }
    if offered.bhi_ctrl && offered.ibrs {
        Bhi::HistoryDisabled
    } else {
        Bhi::Loop
    }
}

/// One hazard's verdict on the exposure line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Exposure {
    /// The processor is not affected, and why.
    NotAffected(&'static str),
    /// Affected, and what covers it.
    Covered(&'static str),
    /// Affected, and nothing does: AoU-11 keeps the item off such a part.
    Uncovered(&'static str),
}

impl core::fmt::Display for Exposure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Exposure::NotAffected(why) => write!(f, "not affected ({why})"),
            Exposure::Covered(how) => write!(f, "covered ({how})"),
            Exposure::Uncovered(why) => write!(f, "NOT covered: {why} (AoU-11)"),
        }
    }
}

/// MDS, for the exposure line: what [`mds_exposed`] decides, and the case it
/// leaves silent, an affected part without `MD_CLEAR`.
const fn mds_exposure(offered: &Offered) -> Exposure {
    if !offered.intel {
        Exposure::NotAffected(if offered.amd { "AMD" } else { "not Intel" })
    } else if offered.capabilities & CAP_MDS_NO != 0 {
        Exposure::NotAffected("MDS_NO")
    } else if offered.md_clear {
        Exposure::Covered("VERW on exit")
    } else {
        Exposure::Uncovered("microcode without MD_CLEAR")
    }
}

/// RFDS, for the exposure line.
const fn rfds_exposure(offered: &Offered) -> Exposure {
    match rfds(offered) {
        Rfds::NotAffected(why) => Exposure::NotAffected(why),
        Rfds::Cleared => Exposure::Covered("VERW on exit, RFDS_CLEAR"),
        Rfds::NoMicrocode => Exposure::Uncovered("microcode without RFDS_CLEAR"),
    }
}

/// BHI, for the exposure line.
const fn bhi_exposure(offered: &Offered) -> Exposure {
    match bhi(offered) {
        Bhi::NotAffected(why) => Exposure::NotAffected(why),
        Bhi::HistoryDisabled => Exposure::Covered("BHI_DIS_S"),
        Bhi::Loop if offered.capabilities & CAP_BHI_NO != 0 => {
            Exposure::Covered("BHB loop on entry; BHI_NO not trusted alone")
        }
        Bhi::Loop => Exposure::Covered("BHB loop on entry"),
    }
}

/// The rest of Linux 6.18.54's x86 hazards, each decided from its tables and
/// named on the second exposure line, so that none reads as covered by
/// being left out. Their order is the line's.
const OTHER_HAZARDS: [&str; 8] = [
    "ITS",
    "MMIO stale data",
    "SRBDS",
    "TAA",
    "Retbleed",
    "SRSO",
    "TSA",
    "VMSCAPE",
];

/// The verdict on each of [`OTHER_HAZARDS`], for a processor that says
/// `offered` and has `defences` planned.
///
/// Linux 6.18.54's `cpu_set_bug_bits` and `vulnerable_to_its`, with every
/// stepping of a listed model taken as affected where Linux names a first
/// affected stepping. What covers one is named only where it is what Linux
/// counts as covering it: enhanced IBRS for an Intel part's Retbleed, and,
/// on an AMD part that says `SRSO_USER_KERNEL_NO`, `IBPB` with
/// `IBPB_BRTYPE` at every switch between programs. VMSCAPE is a guest
/// attacking its host's user space: the item runs no guests.
const fn other_exposures(offered: &Offered, defences: Defences) -> [Exposure; 8] {
    [
        its_exposure(offered),
        mmio_exposure(offered),
        srbds_exposure(offered),
        taa_exposure(offered),
        retbleed_exposure(offered, defences),
        srso_exposure(offered, defences),
        tsa_exposure(offered),
        Exposure::NotAffected("no hypervisor in the item"),
    ]
}

/// Whether `offered` is an Intel family 6 part whose model is in `models`.
const fn intel_listed(offered: &Offered, models: &[u32]) -> bool {
    offered.intel && offered.family == 6 && listed(offered.model, models)
}

/// ITS: Linux's `vulnerable_to_its`, under which a guest without `ITS_NO`
/// is affected and a part with `BHI_CTRL` is not.
const fn its_exposure(offered: &Offered) -> Exposure {
    if !offered.intel || offered.capabilities & CAP_ITS_NO != 0 || offered.bhi_ctrl {
        Exposure::NotAffected("off Linux's list, or ITS_NO")
    } else if offered.hypervisor || intel_listed(offered, &ITS_MODELS) {
        Exposure::Uncovered("no ITS-safe indirect thunks are built")
    } else {
        Exposure::NotAffected("off Linux's list, or ITS_NO")
    }
}

/// MMIO stale data: a listed model without all three of its `_NO` bits.
const fn mmio_exposure(offered: &Offered) -> Exposure {
    let caps = offered.capabilities;
    let immune =
        caps & CAP_FBSDP_NO != 0 && caps & CAP_PSDP_NO != 0 && caps & CAP_SBDR_SSDP_NO != 0;
    if !immune && intel_listed(offered, &MMIO_MODELS) {
        Exposure::Uncovered("no VERW is built for it")
    } else {
        Exposure::NotAffected("off Linux's list, or its three _NO bits")
    }
}

/// SRBDS: a listed model with `RDRAND` or `RDSEED`.
const fn srbds_exposure(offered: &Offered) -> Exposure {
    if offered.random && intel_listed(offered, &SRBDS_MODELS) {
        Exposure::Uncovered("its microcode control is not checked")
    } else {
        Exposure::NotAffected("off Linux's list, or no RDRAND")
    }
}

/// TSX Asynchronous Abort: an Intel part with TSX, or its control, and
/// without `TAA_NO`.
const fn taa_exposure(offered: &Offered) -> Exposure {
    let caps = offered.capabilities;
    if offered.intel && caps & CAP_TAA_NO == 0 && (offered.rtm || caps & CAP_TSX_CTRL != 0) {
        Exposure::Uncovered("TSX is not turned off")
    } else {
        Exposure::NotAffected("no TSX, or TAA_NO")
    }
}

/// Retbleed: a listed Intel model or one saying `RSBA`, covered by enhanced
/// IBRS as Linux counts it; an AMD or Hygon part of family 0x15 to 0x18
/// without `BTC_NO`.
const fn retbleed_exposure(offered: &Offered, defences: Defences) -> Exposure {
    let intel = offered.intel
        && (offered.capabilities & CAP_RSBA != 0 || intel_listed(offered, &RETBLEED_MODELS));
    let amd = offered.amd && !offered.btc_no && matches!(offered.family, 0x15..=0x18);
    if intel && defences.contains(Defences::IBRS_ENHANCED) {
        Exposure::Covered("eIBRS")
    } else if intel || amd {
        Exposure::Uncovered("no return thunk is built")
    } else {
        Exposure::NotAffected("off Linux's list, or BTC_NO")
    }
}

/// SRSO: an AMD or Hygon part of family 0x17 to 0x1a without `SRSO_NO`;
/// covered where it says `SRSO_USER_KERNEL_NO` and `IBPB_BRTYPE` and the
/// switch barrier is planned.
const fn srso_exposure(offered: &Offered, defences: Defences) -> Exposure {
    if !offered.amd || offered.srso_no || !matches!(offered.family, 0x17..=0x1a) {
        Exposure::NotAffected("off Linux's list, or SRSO_NO")
    } else if offered.srso_user_kernel_no
        && offered.ibpb_brtype
        && defences.contains(Defences::SWITCH_BARRIER)
    {
        Exposure::Covered("SRSO_USER_KERNEL_NO into ring 0; IBPB_BRTYPE between programs")
    } else {
        Exposure::Uncovered("no SRSO return thunk is built")
    }
}

/// Transient Scheduler Attacks: a Zen 3 or Zen 4 (family 0x19), or a Zen
/// guest, without both `TSA_SQ_NO` and `TSA_L1_NO`.
const fn tsa_exposure(offered: &Offered) -> Exposure {
    let zen_guest = offered.hypervisor && offered.family >= 0x17;
    if offered.amd && !offered.tsa_no && (offered.family == 0x19 || zen_guest) {
        Exposure::Uncovered("no VERW is built for it")
    } else {
        Exposure::NotAffected("off Linux's list, or TSA_SQ_NO and TSA_L1_NO")
    }
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

/// Whether `VERW` runs on every return to ring 3, for MDS or RFDS. Read by
/// the exits in `syscall` and `trap`, from assembly, as a byte.
pub(super) static CLEAR_CPU_BUFFERS: AtomicU8 = AtomicU8::new(0);

/// Whether every entry from ring 3 overwrites the branch history with
/// `ferrix_clear_bhb` before any indirect branch (BHI). Read by the trap
/// entries, from assembly, as a byte, where a test of it mispredicted the
/// other way meets the entry's `lfence`; and by `syscall::stub_address`,
/// which points `LSTAR` at the system call entry that runs the loop, so the
/// `SYSCALL` path has no branch to mispredict at all.
pub(super) static CLEAR_BRANCH_HISTORY: AtomicU8 = AtomicU8::new(0);

/// Whether this machine's plan runs the branch-history loop on every entry
/// from ring 3: what `syscall::stub_address` chooses the entry by.
pub(super) fn clears_branch_history() -> bool {
    CLEAR_BRANCH_HISTORY.load(Ordering::Relaxed) != 0
}

/// The branch-history loop's outer count: how many calls deep
/// `ferrix_clear_bhb` goes. Linux 6.18.54's `clear_bhb_loop`, for every part
/// it runs the loop on.
pub(super) const BHB_LOOP_OUTER: u32 = 5;
/// The loop's inner count: taken jumps at each depth, as there.
pub(super) const BHB_LOOP_INNER: u32 = 5;

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
    // MEASUREMENT ONLY, never to land: both forced on, as on the N150.
    CLEAR_CPU_BUFFERS.store(1, Ordering::Relaxed);
    CLEAR_BRANCH_HISTORY.store(1, Ordering::Relaxed);
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
         {between}; store bypass {bypass}; Meltdown {meltdown}; MDS {}; RFDS {}; BHI {}",
        mds_exposure(offered),
        rfds_exposure(offered),
        bhi_exposure(offered),
    );
    let [its, mmio, srbds, taa, retbleed, srso, tsa, vmscape] = other_exposures(offered, defences);
    let [
        its_name,
        mmio_name,
        srbds_name,
        taa_name,
        retbleed_name,
        srso_name,
        tsa_name,
        vmscape_name,
    ] = OTHER_HAZARDS;
    println!(
        "  cpu      speculation exposure: {its_name} {its}; {mmio_name} {mmio}; {srbds_name} \
         {srbds}; {taa_name} {taa}; {retbleed_name} {retbleed}; {srso_name} {srso}; {tsa_name} \
         {tsa}; {vmscape_name} {vmscape}"
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
    if clears_branch_history() {
        // The system call entry that runs the loop; `syscall::init` may
        // have pointed `LSTAR` at the other before the plan was made.
        held &= super::syscall::point_entry();
    }
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
/// machine whose exit path never runs it -- every one without MDS or RFDS --
/// still shows the instruction takes its operand; run the branch-history
/// loop once, for the same reason; hold this processor's `LSTAR` to the
/// entry the plan chose; and decide RFDS, BHI and the other hazards for
/// processors described rather than run on (`check`).
///
/// # Errors
///
/// `LSTAR` naming the other entry, or the first described processor given
/// a verdict it must not get. A `VERW` that did not take its operand, or a
/// loop that did not return, would fault or hang, not return an error.
pub(crate) fn check() -> Result<(), &'static str> {
    if HARDENED {
        clear_cpu_buffers();
        clear_branch_history();
        if !super::syscall::entry_is_planned() {
            return Err("LSTAR is not the system call entry the BHI plan chose");
        }
    }
    check::vector_leak_decisions()?;
    check::rfds_decisions()?;
    check::bhi_decisions()?;
    check::exposure_decisions()
}

/// Run the branch-history loop once, as every entry from ring 3 does on a
/// BHI-affected processor without `BHI_CTRL`: for the boot check, which
/// shows it returns, with the stack as it was, wherever no entry runs it.
pub(crate) fn clear_branch_history() {
    // SAFETY: (PROTECT) `ferrix_clear_bhb` takes no argument, touches only
    // `EAX`, `ECX`, the flags and the stack below the stack pointer, which
    // its returns give back, and ends in `lfence`. The kernel's target has
    // no red zone to overwrite.
    unsafe {
        asm!(
            "call ferrix_clear_bhb",
            out("rax") _,
            out("rcx") _,
        );
    }
}
