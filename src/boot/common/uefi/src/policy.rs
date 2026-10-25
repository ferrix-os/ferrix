//! The loader's trust policy (`docs/SIGNING.md` §10.6): the 512-byte block
//! in `.fxanch`, and what the build is.
//!
//! xtask names the block (`FERRIX_ANCHORS`) and the build
//! (`FERRIX_SLOT_BUILD`); `build.rs` refuses a build without either. The
//! block is read through [`core::hint::black_box`] of its address, so that
//! no anchor is folded into code outside the section: the signing gate
//! relinks the loader with only this section changed and compares the two
//! byte for byte outside it (ledger 4016 (5)).

use ferrix_boot_policy::{BLOCK_LEN, Build};

include!(concat!(env!("OUT_DIR"), "/slot_build.rs"));

include!(concat!(env!("OUT_DIR"), "/trust_policy.rs"));

const _: () = assert!(
    TRUST_POLICY.len() == BLOCK_LEN,
    "the trust-policy block is 512 bytes"
);

/// The trust-policy block.
pub(crate) fn block() -> &'static [u8] {
    core::hint::black_box(&TRUST_POLICY)
}

/// What this build is.
pub(crate) const BUILD: Build<'static> = Build {
    profile: PROFILE,
    arch: ARCH,
    claimed: CLAIMED,
    families: FAMILIES,
};

/// The machine's family by the registry's rule (`hardware-families.toml`).
/// No specific family's rule is built yet (§10.6): the SOYO M4 Pro's
/// SMBIOS strings are not recorded, and the DK1 card has no slot path. So
/// every machine is its arch's generic family, which a claimed profile
/// without it refuses (ledger 4016 (6)).
pub(crate) const fn family() -> u32 {
    ferrix_boot_policy::generic_family(ARCH)
}
