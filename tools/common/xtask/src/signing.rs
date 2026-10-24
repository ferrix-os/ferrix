//! Product signing's build-time refusals (`docs/SIGNING.md` §5.2, §6.4).
//!
//! * No kernel holds the verifier: the kernel never links
//!   `ferrix-sigverify` (or the soft signer), and [`refuse_verifier_in_kernel`]
//!   holds that on every kernel xtask builds, by the crates' names and source
//!   paths, which any symbol or panic location of theirs carries.
//! * No claimed profile's image holds a development key: the soft signer's
//!   anchors (`tools/common/data/signing-anchors.toml`, `[development.*]`)
//!   may be in images of level `none` only (the consultant's D1, ledger
//!   3731). [`refuse_in_image`] holds that at the two points every image's
//!   loader and uncompressed initramfs pass through after their last
//!   change -- `fat::write_image_carrying` for every image xtask boots or
//!   writes, and `flash` for a board's card -- and [`refuse_development_anchors`]
//!   on the loader as it is built besides. The match is a key's tree root
//!   `T`, its last 24 bytes, which an anchor table carries whatever layout
//!   it gives the rest (ledger 3843 (3); S4's tables keep `T` contiguous).
//!
//! A build that fails either refusal fails before it is used.

use crate::{Error, Result};

/// The anchors file.
const ANCHORS: &str = "tools/common/data/signing-anchors.toml";

/// What no kernel may hold: the verifier's and the soft signer's crate
/// names, as symbols carry them, and their source directories, as panic
/// locations and debug information carry them.
const NOT_IN_A_KERNEL: &[&str] = &[
    "ferrix_sigverify",
    "ferrix_softsign",
    "crypto/sigverify",
    "release/softsign",
];

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    (0..text.len() / 2)
        .map(|n| {
            text.get(2 * n..2 * n + 2)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
        })
        .collect()
}

fn holds(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// Refuse a kernel image that holds anything of the verifier or the soft
/// signer (`docs/SIGNING.md` §5.2: no verifier in the kernel).
pub(crate) fn refuse_verifier_in_kernel(name: &str, elf: &[u8]) -> Result<()> {
    for needle in NOT_IN_A_KERNEL {
        if holds(elf, needle.as_bytes()) {
            return Err(Error::new(format!(
                "{name} holds `{needle}`: the kernel never links the signature verifier or the \
                 soft signer (docs/SIGNING.md 5.2)"
            )));
        }
    }
    Ok(())
}

/// [`refuse_verifier_in_kernel`] on the file at `path`.
pub(crate) fn refuse_verifier_in_kernel_file(path: &std::path::Path) -> Result<()> {
    let elf = std::fs::read(path)
        .map_err(|error| Error::new(format!("reading {}: {error}", path.display())))?;
    refuse_verifier_in_kernel(&path.display().to_string(), &elf)
}

/// The development keys' tree roots `T` (the last 24 bytes of each
/// 52-byte HSS public key), with their table names.
pub(crate) fn development_keys(text: &str) -> Result<Vec<(String, Vec<u8>)>> {
    let file = crate::flags::toml::parse(ANCHORS, text)?;
    let mut out = Vec::new();
    for (name, anchor) in file
        .get("development")
        .map(crate::flags::toml::Value::entries)
        .unwrap_or(&[])
    {
        let hex = anchor
            .get("public_key")
            .and_then(|key| key.as_str())
            .ok_or_else(|| {
                Error::new(format!("{ANCHORS}: [development.{name}] has no public_key"))
            })?;
        let bytes = unhex(hex)
            .filter(|bytes| bytes.len() == 52)
            .ok_or_else(|| {
                Error::new(format!(
                    "{ANCHORS}: [development.{name}]'s public_key is not 52 bytes of hex"
                ))
            })?;
        out.push((name.clone(), bytes.get(28..).unwrap_or_default().to_vec()));
    }
    Ok(out)
}

/// Refuse `bytes`, a part of an image named `what`, when it holds a
/// development key and `level` is a claimed one (anything but `none`).
pub(crate) fn refuse_development_anchors_at(
    level: &str,
    what: &str,
    bytes: &[u8],
    keys: &[(String, Vec<u8>)],
) -> Result<()> {
    if level == "none" {
        return Ok(());
    }
    for (name, key) in keys {
        if holds(bytes, key) {
            return Err(Error::new(format!(
                "{what} holds the development signing key {name}, and this build is a claimed \
                 profile's (level {level}): development anchors go only into images of level \
                 none (docs/SIGNING.md 6.4, the consultant's D1)"
            )));
        }
    }
    Ok(())
}

/// [`refuse_development_anchors_at`] for this run's level, with the
/// anchors file's development keys. A run whose flags do not resolve is
/// treated as claimed: the refusal fails closed.
pub(crate) fn refuse_development_anchors(what: &str, bytes: &[u8]) -> Result<()> {
    let level =
        crate::flags::this_run().map_or_else(|_| "present".to_owned(), |run| run.level.clone());
    let path = crate::paths::workspace_root().join(ANCHORS);
    let text = std::fs::read_to_string(&path)
        .map_err(|error| Error::new(format!("reading {}: {error}", path.display())))?;
    refuse_development_anchors_at(&level, what, bytes, &development_keys(&text)?)
}

/// Refuse an image whose loader or uncompressed initramfs holds a
/// development key, when this build is a claimed profile's: called where
/// each image's parts are final (`fat::write_image_carrying`, `flash`).
pub(crate) fn refuse_in_image(loader: &[u8], initramfs: &[u8]) -> Result<()> {
    refuse_development_anchors("the UEFI loader", loader)?;
    refuse_development_anchors("the initramfs", initramfs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchors() -> String {
        std::fs::read_to_string(crate::paths::workspace_root().join(ANCHORS)).unwrap()
    }

    /// A kernel holding the verifier's name is refused; one without is not.
    #[test]
    fn a_kernel_holding_the_verifier_is_refused() {
        let mut elf = vec![0u8; 4096];
        assert!(refuse_verifier_in_kernel("k", &elf).is_ok());
        elf.extend_from_slice(b"_ZN16ferrix_sigverify8manifest6verify17h0123456789abcdefE");
        assert!(refuse_verifier_in_kernel("k", &elf).is_err());
        let mut elf = vec![0u8; 64];
        elf.extend_from_slice(b"src/lib/crypto/sigverify/src/lms.rs");
        assert!(refuse_verifier_in_kernel("k", &elf).is_err());
    }

    /// Every development key is read, and a claimed image holding one is
    /// refused while a level-none image holding it is not.
    #[test]
    fn a_development_key_is_refused_only_in_a_claimed_image() {
        let keys = development_keys(&anchors()).unwrap();
        assert_eq!(keys.len(), 3);
        assert!(keys.iter().all(|(_, root)| root.len() == 24));
        let mut image = vec![0u8; 1000];
        assert!(refuse_development_anchors_at("present", "loader", &image, &keys).is_ok());
        image.extend_from_slice(&keys[1].1);
        assert!(refuse_development_anchors_at("present", "loader", &image, &keys).is_err());
        assert!(refuse_development_anchors_at("sc2", "loader", &image, &keys).is_err());
        assert!(refuse_development_anchors_at("none", "loader", &image, &keys).is_ok());
    }

    /// The image check refuses a planted root in either part at this run's
    /// level (the default claimed profile in a test).
    #[test]
    fn an_image_part_holding_a_development_root_is_refused() {
        let keys = development_keys(&anchors()).unwrap();
        let mut part = vec![7u8; 512];
        assert!(refuse_in_image(&part, &part).is_ok());
        part.extend_from_slice(&keys[2].1);
        assert!(refuse_in_image(&part, &[]).is_err());
        assert!(refuse_in_image(&[], &part).is_err());
    }
}
