//! devmgr's one architecture difference, behind its facade
//! (`tools/common/check/check-crate-layering.sh`, rule 4).

/// Whether devmgr runs on AArch64, where the admission rule asks for
/// translation alone until GICv3 ITS rows exist (the customer's G4,
/// `docs/DMA-GATE.md`).
pub(crate) const AARCH64: bool = cfg!(target_arch = "aarch64");
