//! The boot's checks of `speculation`'s decisions that need no processor of
//! their kind: every verdict on Zenbleed and Gather Data Sampling, on RFDS,
//! on BHI and on the rest of Linux's hazards, each from a processor
//! described rather than run on.

use super::{
    Bhi, CAP_BHI_NO, CAP_GDS_CTRL, CAP_GDS_NO, CAP_IBRS_ALL, CAP_ITS_NO, CAP_MDS_NO,
    CAP_RFDS_CLEAR, CAP_RFDS_NO, Defences, Exposure, MCU_GDS_MITG_DIS, MCU_GDS_MITG_LOCKED,
    Offered, Plan, Rfds, SPEC_CTRL_BHI_DIS_S, VectorFacts, VectorLeak, bhi, decide_vector_leak,
    family_and_model, other_exposures, rfds, rfds_exposure,
};

/// A processor for [`vector_leak_decisions`]: `vendor` `b'i'` for
/// Intel, `b'a'` for AMD.
const fn facts(vendor: u8, family: u32, model: u32, hypervisor: bool) -> VectorFacts {
    VectorFacts {
        intel: vendor == b'i',
        amd: vendor == b'a',
        family,
        model,
        hypervisor,
        avx: true,
        capabilities: 0,
        patch_level: 0,
        mcu_opt_ctrl: 0,
    }
}

/// Every verdict [`decide_vector_leak`] can give, each from a processor
/// that must get it, and the family and model read out of two real
/// `CPUID.1:EAX` values -- the decision that keeps AVX from a program where
/// Zenbleed or GDS would leak another's vector registers. With the Zen 2
/// guest's rule taken out of [`super::zenbleed`] (scratch, 2026-09-30) this fails
/// at the guest's row.
///
/// # Errors
///
/// The first processor given a verdict it must not get.
/// Verifies: `L.x86_64.123`
pub(super) fn vector_leak_decisions() -> Result<(), &'static str> {
    let zen2 = facts(b'a', 0x17, 0x31, false);
    let kaby = facts(b'i', 6, 0x8e, false);
    let with = |mut base: VectorFacts, change: fn(&mut VectorFacts)| {
        change(&mut base);
        base
    };
    let cases: [(VectorFacts, VectorLeak); 12] = [
        (zen2, VectorLeak::ZenbleedChickenBit),
        (
            with(zen2, |f| f.patch_level = 0x0830_107b),
            VectorLeak::ZenbleedMicrocode,
        ),
        (
            with(zen2, |f| {
                f.model = 0x90;
                f.patch_level = u32::MAX;
            }),
            VectorLeak::ZenbleedChickenBit,
        ),
        (
            facts(b'a', 0x17, 0x71, true),
            VectorLeak::Uncovered(
                "AVX held back: Zenbleed, on a Zen 2 guest that cannot see the host's fix",
            ),
        ),
        (with(zen2, |f| f.avx = false), VectorLeak::NotAffected),
        (facts(b'a', 0x1a, 0x44, true), VectorLeak::NotAffected),
        (
            kaby,
            VectorLeak::Uncovered("AVX held back: GDS, and no microcode mitigates it"),
        ),
        (
            with(kaby, |f| f.capabilities = CAP_GDS_NO),
            VectorLeak::NotAffected,
        ),
        (
            with(kaby, |f| f.capabilities = CAP_GDS_CTRL),
            VectorLeak::GdsMicrocode,
        ),
        (
            with(kaby, |f| {
                f.capabilities = CAP_GDS_CTRL;
                f.mcu_opt_ctrl = MCU_GDS_MITG_DIS;
            }),
            VectorLeak::GdsMicrocodeEnabled,
        ),
        (
            with(kaby, |f| {
                f.capabilities = CAP_GDS_CTRL;
                f.mcu_opt_ctrl = MCU_GDS_MITG_DIS | MCU_GDS_MITG_LOCKED;
            }),
            VectorLeak::Uncovered("AVX held back: GDS, its microcode mitigation off and locked"),
        ),
        (facts(b'i', 6, 0x97, false), VectorLeak::NotAffected),
    ];
    for (processor, verdict) in cases {
        if decide_vector_leak(&processor) != verdict {
            return Err("a processor was given the wrong verdict on Zenbleed or GDS");
        }
    }
    // A Zen 2 Matisse and a Kaby Lake, as CPUID reports them.
    if family_and_model(0x0087_0f10) != (0x17, 0x71) || family_and_model(0x0008_06ea) != (6, 0x8e) {
        return Err("a processor's family and model were read wrongly for Zenbleed or GDS");
    }
    Ok(())
}

/// An Intel processor for the checks below: family 6, `model`, enhanced IBRS
/// and `IA32_SPEC_CTRL`, on bare metal, with `capabilities` besides.
const fn intel(model: u32, capabilities: u64) -> Offered {
    Offered {
        intel: true,
        amd: false,
        ibrs: true,
        ibpb: true,
        stibp: true,
        ibrs_always_on: false,
        ssbd: true,
        virt_ssbd: false,
        amd_ssb_no: false,
        auto_ibrs: false,
        eraps: false,
        md_clear: true,
        capabilities: capabilities | CAP_IBRS_ALL,
        family: 6,
        model,
        hypervisor: false,
        hybrid: false,
        atom_core: false,
        bhi_ctrl: false,
        random: true,
        rtm: false,
        btc_no: false,
        ibpb_brtype: false,
        srso_no: false,
        srso_user_kernel_no: false,
        tsa_no: false,
    }
}

/// An AMD processor of `family` and `model` for the checks below, with
/// `AutoIBRS` and `IBPB`, on bare metal.
const fn amd(family: u32, model: u32) -> Offered {
    let mut offered = intel(model, 0);
    offered.intel = false;
    offered.amd = true;
    offered.family = family;
    offered.capabilities = 0;
    offered.auto_ibrs = true;
    offered.md_clear = false;
    offered
}

/// The N150's Gracemont as its box reports it (`box-2026-10-16`): family 6
/// model 0xbe, `MDS_NO`, `RFDS_CLEAR` (Linux says "Clear Register File"),
/// enhanced IBRS, no `BHI_CTRL`.
const GRACEMONT: Offered = intel(0xbe, CAP_MDS_NO | CAP_RFDS_CLEAR);

/// `processor` with `change` made.
fn with(mut processor: Offered, change: fn(&mut Offered)) -> Offered {
    change(&mut processor);
    processor
}

/// RFDS's verdict on every kind of processor [`rfds`] tells apart, and what
/// the plan makes of it: `VERW` on exit exactly where an affected part's
/// microcode says `RFDS_CLEAR`, and an Intel model the tables do not know
/// taken as affected. With the plan's RFDS arm taken out, or the unknown
/// model answered not affected, this fails.
///
/// # Errors
///
/// The first processor given a verdict or a plan it must not get.
/// Verifies: `L.x86_64.179`
pub(super) fn rfds_decisions() -> Result<(), &'static str> {
    let verw = |processor: &Offered| {
        Plan::for_processor(processor)
            .defences
            .contains(Defences::BUFFERS_CLEARED)
    };
    if rfds(&GRACEMONT) != Rfds::Cleared || !verw(&GRACEMONT) {
        return Err("an RFDS-affected processor with RFDS_CLEAR was not planned VERW on exit");
    }
    let old_microcode = with(GRACEMONT, |o| o.capabilities &= !CAP_RFDS_CLEAR);
    if rfds(&old_microcode) != Rfds::NoMicrocode
        || verw(&old_microcode)
        || rfds_exposure(&old_microcode) != Exposure::Uncovered("microcode without RFDS_CLEAR")
    {
        return Err("an RFDS-affected processor without RFDS_CLEAR was not said NOT covered");
    }
    let immune = with(GRACEMONT, |o| o.capabilities |= CAP_RFDS_NO);
    if !matches!(rfds(&immune), Rfds::NotAffected(_)) || verw(&immune) {
        return Err("a processor saying RFDS_NO was planned VERW for RFDS");
    }
    // Every listed model, Goldmont to Raptor Lake, with old microcode.
    for model in [0x5c, 0x5f, 0x7a, 0x96, 0x86, 0x9c, 0xbe, 0x9a, 0xba, 0xbf] {
        if rfds(&intel(model, CAP_MDS_NO)) != Rfds::NoMicrocode {
            return Err("a model on Linux's RFDS list was not taken as affected");
        }
    }
    // Alder and Raptor Lake: affected only with E-cores.
    let alder = intel(0x97, CAP_MDS_NO);
    if !matches!(rfds(&alder), Rfds::NotAffected(_))
        || rfds(&with(alder, |o| o.hybrid = true)) != Rfds::NoMicrocode
        || rfds(&with(intel(0xb7, 0), |o| o.atom_core = true)) != Rfds::NoMicrocode
    {
        return Err("an Alder or Raptor Lake was judged by other than its E-cores");
    }
    // A model Linux knows and leaves off the list, on bare metal and as a
    // guest, where the model need not be the host's.
    let kaby = intel(0x8e, 0);
    if !matches!(rfds(&kaby), Rfds::NotAffected(_)) || verw(&with(kaby, |o| o.md_clear = false)) {
        return Err("a model off Linux's RFDS list was taken as affected");
    }
    if rfds(&with(kaby, |o| o.hypervisor = true)) != Rfds::NoMicrocode {
        return Err("an Intel guest without RFDS_NO was not taken as RFDS-affected");
    }
    // A guest whose hypervisor hides IA32_ARCH_CAPABILITIES altogether.
    if rfds(&with(GRACEMONT, |o| {
        o.capabilities = 0;
        o.hypervisor = true;
    })) != Rfds::NoMicrocode
    {
        return Err("an Intel guest without IA32_ARCH_CAPABILITIES was not taken as RFDS-affected");
    }
    // Fail closed: models and families the tables do not know.
    for unknown in [
        intel(0xd0, 0),
        intel(0x01, 0),
        with(intel(0x01, 0), |o| o.family = 19),
    ] {
        if rfds(&unknown) != Rfds::NoMicrocode
            || rfds(&with(unknown, |o| o.capabilities |= CAP_RFDS_CLEAR)) != Rfds::Cleared
        {
            return Err("an Intel model unknown to Linux's tables was not taken as RFDS-affected");
        }
    }
    if !matches!(rfds(&amd(0x1a, 0x44)), Rfds::NotAffected(_)) {
        return Err("an AMD processor was taken as RFDS-affected");
    }
    Ok(())
}

/// BHI's verdict on every kind of processor [`bhi`] tells apart, and what the
/// plan makes of it: `BHI_DIS_S` in `IA32_SPEC_CTRL` where `BHI_CTRL` offers
/// it, else the branch-history loop on every entry, `BHI_NO` lifting
/// neither; AMD's, and Intel's without enhanced IBRS on bare metal, not
/// affected. With the plan's loop arm taken out, this fails.
///
/// # Errors
///
/// The first processor given a verdict or a plan it must not get.
/// Verifies: `L.x86_64.180`
pub(super) fn bhi_decisions() -> Result<(), &'static str> {
    let planned = |processor: &Offered| {
        let plan = Plan::for_processor(processor);
        (
            plan.defences.contains(Defences::BHB_LOOP),
            plan.defences.contains(Defences::BHI_DIS_S),
            plan.spec_ctrl & SPEC_CTRL_BHI_DIS_S != 0,
        )
    };
    if bhi(&GRACEMONT) != Bhi::Loop || planned(&GRACEMONT) != (true, false, false) {
        return Err("a BHI-affected processor without BHI_CTRL was not planned the BHB loop");
    }
    let controlled = with(GRACEMONT, |o| o.bhi_ctrl = true);
    if bhi(&controlled) != Bhi::HistoryDisabled || planned(&controlled) != (false, true, true) {
        return Err("a processor offering BHI_CTRL was not planned BHI_DIS_S");
    }
    // BHI_NO is not trusted alone, as Linux 6.18.54 does not trust it.
    let claims = with(GRACEMONT, |o| o.capabilities |= CAP_BHI_NO);
    if planned(&claims) != (true, false, false)
        || planned(&with(claims, |o| o.bhi_ctrl = true)) != (false, true, true)
    {
        return Err("a processor saying BHI_NO was left without a BHI defence");
    }
    // A guest that hides enhanced IBRS is affected; a bare Intel part
    // without it is Spectre v2's case, and AMD's are not.
    let hidden = with(GRACEMONT, |o| {
        o.capabilities = 0;
        o.hypervisor = true;
    });
    if bhi(&hidden) != Bhi::Loop {
        return Err("an Intel guest hiding enhanced IBRS was not taken as BHI-affected");
    }
    let skylake = with(intel(0x5e, 0), |o| o.capabilities = 0);
    if !matches!(bhi(&skylake), Bhi::NotAffected(_)) || planned(&skylake) != (false, false, false) {
        return Err("a processor without enhanced IBRS was given a BHI defence on bare metal");
    }
    let zen5 = amd(0x1a, 0x44);
    if !matches!(bhi(&zen5), Bhi::NotAffected(_))
        || planned(&with(zen5, |o| o.hypervisor = true)) != (false, false, false)
    {
        return Err("an AMD processor was given a BHI defence");
    }
    Ok(())
}

/// The verdict on each of the rest of Linux's hazards for processors that
/// must get it: the N150's Gracemont affected by none of them, an Ice Lake
/// by ITS, MMIO stale data and SRBDS, a part with TSX by TAA, a Skylake by
/// Retbleed without enhanced IBRS and covered with it, a Zen 3 by SRSO and
/// TSA, a Zen 5 covered for SRSO, an Intel guest by ITS, and VMSCAPE by
/// nothing.
///
/// # Errors
///
/// The first processor given a verdict it must not get.
/// Verifies: `L.x86_64.181`
pub(super) fn exposure_decisions() -> Result<(), &'static str> {
    let verdicts =
        |processor: &Offered| other_exposures(processor, Plan::for_processor(processor).defences);
    let uncovered = |exposure: Exposure| matches!(exposure, Exposure::Uncovered(_));
    let affected = |exposure: Exposure| !matches!(exposure, Exposure::NotAffected(_));
    if verdicts(&GRACEMONT).into_iter().any(affected) {
        return Err("the N150's Gracemont was taken as affected by one of Linux's other hazards");
    }
    let [its, mmio, srbds, ..] = verdicts(&intel(0x7e, 0));
    if !uncovered(its) || !uncovered(mmio) || !uncovered(srbds) {
        return Err("an Ice Lake was not said NOT covered for ITS, MMIO stale data and SRBDS");
    }
    if affected(verdicts(&intel(0x7e, CAP_ITS_NO))[0]) {
        return Err("a processor saying ITS_NO was taken as ITS-affected");
    }
    if !uncovered(verdicts(&with(GRACEMONT, |o| o.hypervisor = true))[0]) {
        return Err("an Intel guest without ITS_NO was not taken as ITS-affected");
    }
    if !uncovered(verdicts(&with(GRACEMONT, |o| o.rtm = true))[3]) {
        return Err("a processor with TSX and no TAA_NO was not said NOT covered for TAA");
    }
    let skylake = with(intel(0x5e, 0), |o| o.capabilities = 0);
    if !uncovered(verdicts(&skylake)[4])
        || verdicts(&intel(0x5e, 0))[4] != Exposure::Covered("eIBRS")
    {
        return Err("a Skylake's Retbleed was judged other than by enhanced IBRS");
    }
    let [.., retbleed, srso, tsa, vmscape] = verdicts(&amd(0x19, 0x21));
    if affected(retbleed) || !uncovered(srso) || !uncovered(tsa) || affected(vmscape) {
        return Err("a Zen 3 was not said NOT covered for SRSO and TSA alone");
    }
    if !uncovered(verdicts(&amd(0x17, 0x31))[4]) {
        return Err("a Zen 2 without BTC_NO was not said NOT covered for Retbleed");
    }
    let zen5 = with(amd(0x1a, 0x44), |o| {
        o.srso_user_kernel_no = true;
        o.ibpb_brtype = true;
        o.tsa_no = true;
        o.btc_no = true;
    });
    let zen5_verdicts = verdicts(&zen5);
    if !matches!(zen5_verdicts[5], Exposure::Covered(_)) || zen5_verdicts.into_iter().any(uncovered)
    {
        return Err("a Zen 5 with SRSO_USER_KERNEL_NO and IBPB_BRTYPE was not covered for SRSO");
    }
    // QEMU's qemu64, the gates' model: family 15, which no list names.
    if verdicts(&amd(0xf, 0x6b)).into_iter().any(affected) {
        return Err("qemu64 was taken as affected by one of Linux's other hazards");
    }
    Ok(())
}

/// ERAPS stands in for the in-domain refill only where every `CR3` write
/// flushes: never on a machine with PCIDs on (`docs/PCID.md` §4). Held two
/// ways: what the plan decided against the machine-wide flag, and the
/// decision itself asked again for this processor described as offering
/// ERAPS, which with `PCIDE` set must say no. With the decision's `CR4` test
/// taken out, the second fails on a boot offering `+pcid,+eraps`.
///
/// Verifies: `L.x86_64.220`
pub(super) fn eraps_never_beside_pcids() -> Result<(), &'static str> {
    if super::ERAPS_EMPTIES.load(core::sync::atomic::Ordering::Relaxed) && super::super::pcid::on()
    {
        return Err("ERAPS stands in for the return stack refill on a machine with PCIDs on");
    }
    let here = super::cpu::read_cr4() & super::CR4_PCIDE != 0;
    let described = Offered {
        eraps: true,
        ..super::offered()
    };
    if here && super::eraps_empties_on_switch(&described) {
        return Err(
            "the ERAPS decision would claim a processor whose CR3 writes keep translations",
        );
    }
    Ok(())
}
