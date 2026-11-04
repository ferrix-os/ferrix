//! devmgr's one architecture difference, behind its facade
//! (`tools/common/check/check-crate-layering.sh`, rule 4).

/// Whether devmgr runs on AArch64, where the admission rule asks for
/// translation alone until GICv3 ITS rows exist (the customer's G4,
/// `docs/DMA-GATE.md`).
pub(crate) const AARCH64: bool = cfg!(target_arch = "aarch64");

/// Whether devmgr runs on ARMv7-A, which is outside the claim until an
/// SMMU is driven (the customer's G4 (a)): a driver may be started there on
/// a node the kernel depends on (`docs/DRIVER-MODEL.md` §3, D4).
pub(crate) const ARMV7A: bool = cfg!(target_arch = "arm");
