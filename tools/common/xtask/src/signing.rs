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

/// The registry of hardware families (`docs/SIGNING.md` §1.4).
const FAMILIES: &str = "tools/common/data/hardware-families.toml";

/// The two build inputs every build of the UEFI loader is given
/// (`docs/SIGNING.md` §10.6; its `build.rs` refuses a build without them):
/// `FERRIX_ANCHORS`, the trust-policy block, and `FERRIX_SLOT_BUILD`, what
/// the build is. A development image (profile `none`) takes
/// `development-off` and every family; an image of a profile takes
/// `release-<its slot_signing>` and its `slot_families`.
///
/// # Errors
///
/// A profile file or the family registry that does not read.
pub(crate) fn loader_env() -> Result<Vec<(&'static str, String)>> {
    let run = crate::flags::this_run()?;
    loader_env_for(&run.profile, &run.level)
}

/// [`loader_env`] for a run of `profile` (`none` for a development image)
/// at `level`.
fn loader_env_for(name: &str, level: &str) -> Result<Vec<(&'static str, String)>> {
    let root = crate::paths::workspace_root();
    let read = |path: &std::path::Path| {
        std::fs::read_to_string(path)
            .map_err(|error| Error::new(format!("reading {}: {error}", path.display())))
    };
    let families_file = root.join(FAMILIES);
    let families = crate::flags::toml::parse(FAMILIES, &read(&families_file)?)?;
    let id_of = |name: &str| -> Result<i64> {
        families
            .get("family")
            .and_then(|table| table.get(name))
            .and_then(|family| family.get("id"))
            .and_then(|id| match id {
                crate::flags::toml::Value::Int(id) => Some(*id),
                _ => None,
            })
            .ok_or_else(|| Error::new(format!("{FAMILIES}: no family {name}")))
    };
    let (table, profile_byte, names): (String, u8, Vec<String>) = if name == "none" {
        let names = families
            .get("family")
            .map(crate::flags::toml::Value::entries)
            .unwrap_or_default()
            .iter()
            .map(|(name, _)| name.clone())
            .collect();
        ("development-off".to_owned(), 0xFF, names)
    } else {
        let path = crate::flags::profile_dir().join(format!("{name}.toml"));
        let profile = crate::flags::toml::parse(&path.display().to_string(), &read(&path)?)?;
        let mode = profile
            .get("image")
            .and_then(|image| image.get("slot_signing"))
            .and_then(crate::flags::toml::Value::as_str)
            .ok_or_else(|| Error::new(format!("{}: no image.slot_signing", path.display())))?;
        let names = match profile.get("slot_families") {
            Some(crate::flags::toml::Value::Array(items)) => items
                .iter()
                .map(|item| item.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| {
                    Error::new(format!("{}: slot_families is not names", path.display()))
                })?,
            _ => {
                return Err(Error::new(format!(
                    "{}: no slot_families list",
                    path.display()
                )));
            }
        };
        let byte = match name {
            "max" => 1,
            "fast" => 2,
            "present" => 3,
            "desktop" => 4,
            other => return Err(Error::new(format!("no manifest profile byte for {other}"))),
        };
        (format!("release-{mode}"), byte, names)
    };
    let ids = names
        .iter()
        .map(|name| id_of(name).map(|id| id.to_string()))
        .collect::<Result<Vec<_>>>()?;
    let claimed = u8::from(level != "none");
    let table = gate_table().unwrap_or(table);
    Ok(vec![
        ("FERRIX_ANCHORS", table),
        (
            "FERRIX_SLOT_BUILD",
            format!(
                "profile={profile_byte} claimed={claimed} families={}",
                ids.join(",")
            ),
        ),
    ])
}

/// Refuse an image whose loader or uncompressed initramfs holds a
/// development key, when this build is a claimed profile's: called where
/// each image's parts are final (`fat::write_image_carrying`, `flash`).
pub(crate) fn refuse_in_image(loader: &[u8], initramfs: &[u8]) -> Result<()> {
    refuse_development_anchors("the UEFI loader", loader)?;
    refuse_development_anchors("the initramfs", initramfs)
}

/// Where the loader's trust-policy blocks are (`tools/common/gen/gen-anchors.py`).
const BLOCKS: &str = "src/boot/common/uefi/anchors";

/// A PE section's characteristics: writable.
const IMAGE_SCN_MEM_WRITE: u32 = 0x8000_0000;

/// The table this run's loader must carry: what [`loader_env`] names, or
/// the signing gate's own while it builds its image.
fn expected_table() -> Result<String> {
    if let Some(table) = gate_table() {
        return Ok(table);
    }
    Ok(loader_env()?
        .into_iter()
        .find(|(key, _)| *key == "FERRIX_ANCHORS")
        .map(|(_, table)| table)
        .unwrap_or_default())
}

/// Set by the signing gate alone while it builds its relinked loader and
/// the image under its own output directory (`docs/SIGNING.md` §6.4,
/// §10.6): the table that loader carries in place of the run's.
static GATE_TABLE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// The signing gate's table, while it is set.
fn gate_table() -> Option<String> {
    GATE_TABLE.lock().ok().and_then(|table| table.clone())
}

/// Set, or with `None` clear, the signing gate's table.
pub(crate) fn set_gate_table(table: Option<&str>) {
    if let Ok(mut held) = GATE_TABLE.lock() {
        *held = table.map(str::to_owned);
    }
}

/// Refuse a loader that does not carry exactly the trust-policy block
/// `table` names: one `FXPOLICY` block, byte for byte the generated one,
/// and on a PE loader rustc made, in a read-only `.fxanch` section of 512
/// bytes (`docs/SIGNING.md` §10.6, the release-image scan; ledger 4016
/// (4)). The ARMv7-A loader's PE is converted from an ELF whose `.fxanch`
/// lands inside the read-only section, so it is held to the block alone.
pub(crate) fn check_loader_policy_as(
    what: &str,
    loader: &[u8],
    table: &str,
    rustc_pe: bool,
) -> Result<()> {
    let path = crate::paths::workspace_root()
        .join(BLOCKS)
        .join(format!("{table}.bin"));
    let wanted = std::fs::read(&path)
        .map_err(|error| Error::new(format!("reading {}: {error}", path.display())))?;
    // Every generated block, counted whole: the magic alone also sits in
    // the parser's code as an immediate.
    let count = |block: &[u8]| -> Vec<usize> {
        loader
            .windows(block.len().max(1))
            .enumerate()
            .filter(|(_, window)| *window == block)
            .map(|(at, _)| at)
            .collect()
    };
    for other in [
        "release-off",
        "release-required",
        "development-off",
        "development-required",
    ] {
        if other == table {
            continue;
        }
        let path = crate::paths::workspace_root()
            .join(BLOCKS)
            .join(format!("{other}.bin"));
        let block = std::fs::read(&path)
            .map_err(|error| Error::new(format!("reading {}: {error}", path.display())))?;
        if block != wanted && !count(&block).is_empty() {
            return Err(Error::new(format!(
                "{what} holds the {other} trust-policy block; this build must carry {table} \
                 (docs/SIGNING.md 10.6)"
            )));
        }
    }
    let found = count(&wanted);
    let [at] = found[..] else {
        return Err(Error::new(format!(
            "{what} holds {} copies of the {table} trust-policy block, not one (docs/SIGNING.md \
             10.6)",
            found.len()
        )));
    };
    if let Some(section) = pe_section(loader, b".fxanch") {
        if section.size != 512 || section.characteristics & IMAGE_SCN_MEM_WRITE != 0 {
            return Err(Error::new(format!(
                "{what}'s .fxanch is {} bytes with characteristics {:#x}: it must be 512 \
                 read-only bytes",
                section.size, section.characteristics
            )));
        }
        if section.raw != at {
            return Err(Error::new(format!(
                "{what}'s trust-policy block is not in .fxanch"
            )));
        }
    } else if rustc_pe {
        return Err(Error::new(format!("{what} has no .fxanch section")));
    }
    Ok(())
}

/// [`check_loader_policy_as`] with this run's table, for `arch`'s loader.
pub(crate) fn check_loader_policy(
    arch: crate::paths::Arch,
    what: &str,
    loader: &[u8],
) -> Result<()> {
    check_loader_policy_as(what, loader, &expected_table()?, !arch.loader_is_elf())
}

/// One PE section's raw place and size.
pub(crate) struct PeSection {
    /// File offset of its raw data.
    pub(crate) raw: usize,
    /// Its virtual size.
    pub(crate) size: u32,
    /// Its characteristics.
    pub(crate) characteristics: u32,
}

/// The section of a PE image named `name`, if there is one.
pub(crate) fn pe_section(image: &[u8], name: &[u8]) -> Option<PeSection> {
    let le32 = |at: usize| {
        image
            .get(at..at + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap_or([0; 4])))
    };
    let le16 = |at: usize| {
        image
            .get(at..at + 2)
            .map(|b| u16::from_le_bytes(b.try_into().unwrap_or([0; 2])))
    };
    if !image.starts_with(b"MZ") {
        return None;
    }
    let pe = le32(0x3c)? as usize;
    if image.get(pe..pe + 4)? != b"PE\0\0" {
        return None;
    }
    let sections = usize::from(le16(pe + 6)?);
    let optional = usize::from(le16(pe + 20)?);
    let table = pe + 24 + optional;
    (0..sections).find_map(|index| {
        let header = table + index * 40;
        let raw_name = image.get(header..header + 8)?;
        let end = raw_name.iter().position(|&b| b == 0).unwrap_or(8);
        (raw_name.get(..end)? == name).then(|| {
            Some(PeSection {
                size: le32(header + 8)?,
                raw: le32(header + 20)? as usize,
                characteristics: le32(header + 36)?,
            })
        })?
    })
}

/// The byte ranges of a PE image that a reproducible link derives from the
/// rest (lld-link's /Brepro), or that firmware ignores: the COFF timestamp,
/// the optional header's checksum, and each debug directory entry's
/// timestamp and `CodeView` signature (GUID and age). None of them carries
/// code or data of its own.
fn pe_derived(image: &[u8]) -> Vec<std::ops::Range<usize>> {
    let le32 = |at: usize| -> Option<usize> {
        Some(u32::from_le_bytes(image.get(at..at + 4)?.try_into().ok()?) as usize)
    };
    let le16 = |at: usize| -> Option<usize> {
        Some(usize::from(u16::from_le_bytes(
            image.get(at..at + 2)?.try_into().ok()?,
        )))
    };
    let mut out = Vec::new();
    let Some(pe) = le32(0x3c) else {
        return out;
    };
    out.push(pe + 8..pe + 12);
    let optional = pe + 24;
    out.push(optional + 64..optional + 68);
    // PE32+ keeps its data directories at 112, PE32 at 96; the debug one is
    // the seventh.
    let directories = match le16(optional) {
        Some(0x20b) => optional + 112,
        _ => optional + 96,
    };
    let (Some(rva), Some(size)) = (le32(directories + 6 * 8), le32(directories + 6 * 8 + 4)) else {
        return out;
    };
    let sections = le16(pe + 6).unwrap_or(0);
    let table = optional + le16(pe + 20).unwrap_or(0);
    let file_of = |rva: usize| {
        (0..sections).find_map(|i| {
            let header = table + i * 40;
            let (virtual_at, raw_size, raw_at) =
                (le32(header + 12)?, le32(header + 16)?, le32(header + 20)?);
            (rva >= virtual_at && rva < virtual_at + raw_size).then(|| raw_at + (rva - virtual_at))
        })
    };
    let Some(first) = file_of(rva) else {
        return out;
    };
    for entry in (first..first + size).step_by(28) {
        out.push(entry + 4..entry + 8);
        if let Some(raw) = le32(entry + 24)
            && image.get(raw..raw + 4) == Some(b"RSDS")
        {
            out.push(raw + 4..raw + 24);
        }
    }
    out
}

/// What [`pe_derived`] skips in `image`, checked to be exactly the COFF
/// timestamp, the checksum, and per debug directory entry its timestamp and,
/// when it is a `CodeView` record, its signature: the number of debug entries
/// and of `CodeView` signatures (ledger 4082 (4)).
///
/// # Errors
///
/// A range of any other place or size.
pub(crate) fn derived_fields(image: &[u8]) -> Result<(usize, usize)> {
    let ranges = pe_derived(image);
    let pe = image
        .get(0x3c..0x40)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| Error::new("not a PE image"))? as usize;
    let [timestamp, checksum, rest @ ..] = &ranges[..] else {
        return Err(Error::new("the PE image has no header fields to skip"));
    };
    if *timestamp != (pe + 8..pe + 12) || *checksum != (pe + 24 + 64..pe + 24 + 68) {
        return Err(Error::new(
            "the skipped header fields are not the timestamp and checksum",
        ));
    }
    let (mut entries, mut signatures) = (0, 0);
    for range in rest {
        match range.len() {
            4 => entries += 1,
            20 if image.get(range.start - 4..range.start) == Some(b"RSDS") => signatures += 1,
            _ => {
                return Err(Error::new(format!(
                    "a skipped range {range:?} is neither a debug timestamp nor a CodeView signature"
                )));
            }
        }
    }
    Ok((entries, signatures))
}

/// Two builds of one loader from one commit and one block: the same bytes,
/// but for the fields [`pe_derived`] names, which a reproducible link
/// derives from inputs outside the file (the program database).
///
/// # Errors
///
/// Any other difference, or a different length.
pub(crate) fn compare_rebuilt(first: &[u8], second: &[u8]) -> Result<()> {
    let derived = pe_derived(first);
    let differs = first.len() != second.len()
        || derived != pe_derived(second)
        || first
            .iter()
            .zip(second)
            .enumerate()
            .any(|(at, (x, y))| x != y && !derived.iter().any(|range| range.contains(&at)));
    if differs {
        Err(Error::new(
            "the loader does not rebuild byte for byte from one commit and one block",
        ))
    } else {
        Ok(())
    }
}

/// The signing gate's byte comparison (`docs/SIGNING.md` §6.4): two PE
/// loaders built from one commit with different trust-policy blocks may
/// differ only inside `.fxanch` and in the fields [`pe_derived`] names. The offsets of the
/// first few differences outside them, or `Ok` when there are none.
///
/// # Errors
///
/// Either image without a `.fxanch` section, the two of different
/// lengths or with the section in different places, or a difference
/// outside it.
pub(crate) fn compare_relinked(shipped: &[u8], gate: &[u8]) -> Result<()> {
    let (Some(a), Some(b)) = (
        pe_section(shipped, b".fxanch"),
        pe_section(gate, b".fxanch"),
    ) else {
        return Err(Error::new("a loader to compare has no .fxanch section"));
    };
    if shipped.len() != gate.len() || a.raw != b.raw || a.size != b.size {
        return Err(Error::new(format!(
            "the relinked loader is laid out otherwise: {} bytes, .fxanch at {:#x} (+{}) against {} \
             bytes, at {:#x} (+{})",
            gate.len(),
            b.raw,
            b.size,
            shipped.len(),
            a.raw,
            a.size
        )));
    }
    let section = a.raw..a.raw + a.size as usize;
    let derived = pe_derived(shipped);
    if derived != pe_derived(gate) {
        return Err(Error::new(
            "the relinked loader's PE headers are laid out otherwise",
        ));
    }
    let outside: Vec<usize> = shipped
        .iter()
        .zip(gate)
        .enumerate()
        .filter(|(at, (x, y))| {
            x != y && !section.contains(at) && !derived.iter().any(|range| range.contains(at))
        })
        .map(|(at, _)| at)
        .take(8)
        .collect();
    if outside.is_empty() {
        Ok(())
    } else {
        Err(Error::new(format!(
            "the relinked loader differs from the shipped one outside .fxanch, at {outside:#x?} \
             (docs/SIGNING.md 6.4: only the anchor table may differ)"
        )))
    }
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

    /// What each profile's loader is built as (ledger 4082 (2)): its
    /// table, its manifest profile byte, claimed exactly when its level is
    /// not none, and its `slot_families`' ids.
    #[test]
    fn each_profile_builds_its_loader_as_its_file_says() {
        let env = |profile: &str, level: &str| loader_env_for(profile, level).unwrap();
        assert_eq!(
            env("present", "present"),
            vec![
                ("FERRIX_ANCHORS", "release-off".to_owned()),
                (
                    "FERRIX_SLOT_BUILD",
                    "profile=3 claimed=1 families=257,513,769".to_owned()
                ),
            ]
        );
        assert_eq!(
            env("fast", "sc2"),
            vec![
                ("FERRIX_ANCHORS", "release-required".to_owned()),
                (
                    "FERRIX_SLOT_BUILD",
                    "profile=2 claimed=1 families=513".to_owned()
                ),
            ]
        );
        assert_eq!(
            env("max", "sc4"),
            vec![
                ("FERRIX_ANCHORS", "release-off".to_owned()),
                (
                    "FERRIX_SLOT_BUILD",
                    "profile=1 claimed=1 families=".to_owned()
                ),
            ]
        );
        assert_eq!(
            env("desktop", "none"),
            vec![
                ("FERRIX_ANCHORS", "release-off".to_owned()),
                (
                    "FERRIX_SLOT_BUILD",
                    "profile=4 claimed=0 families=257,258,513,769,770".to_owned()
                ),
            ]
        );
        // A development image: the development table, profile 0xFF, every
        // family, no claim.
        let development = env("none", "none");
        assert_eq!(
            development[0],
            ("FERRIX_ANCHORS", "development-off".to_owned())
        );
        assert!(
            development[1]
                .1
                .starts_with("profile=255 claimed=0 families=257,258,513,514,769,770")
        );
        // The same profile at level none is not claimed.
        assert!(env("present", "none")[1].1.contains("claimed=0"));
    }

    /// A PE32+ image of one section holding a debug directory of one
    /// `CodeView` entry, as lld-link lays it out.
    fn tiny_pe() -> Vec<u8> {
        let mut image = vec![0u8; 0x600];
        image[..2].copy_from_slice(b"MZ");
        image[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        let pe = 0x80;
        image[pe..pe + 4].copy_from_slice(b"PE\0\0");
        image[pe + 6..pe + 8].copy_from_slice(&1u16.to_le_bytes());
        image[pe + 8..pe + 12].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        image[pe + 20..pe + 22].copy_from_slice(&240u16.to_le_bytes());
        let optional = pe + 24;
        image[optional..optional + 2].copy_from_slice(&0x20bu16.to_le_bytes());
        // The debug directory, the seventh: RVA 0x1000, one entry.
        let debug = optional + 112 + 6 * 8;
        image[debug..debug + 4].copy_from_slice(&0x1000u32.to_le_bytes());
        image[debug + 4..debug + 8].copy_from_slice(&28u32.to_le_bytes());
        // One section: virtual 0x1000, 0x200 raw bytes at 0x400.
        let section = optional + 240;
        image[section..section + 8].copy_from_slice(b".rdata\0\0");
        image[section + 12..section + 16].copy_from_slice(&0x1000u32.to_le_bytes());
        image[section + 16..section + 20].copy_from_slice(&0x200u32.to_le_bytes());
        image[section + 20..section + 24].copy_from_slice(&0x400u32.to_le_bytes());
        // The entry: its timestamp, and its data at file 0x440, an RSDS.
        image[0x404..0x408].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        image[0x414..0x418].copy_from_slice(&0x1040u32.to_le_bytes());
        image[0x418..0x41c].copy_from_slice(&0x440u32.to_le_bytes());
        image[0x440..0x444].copy_from_slice(b"RSDS");
        image
    }

    /// The fields the comparisons skip are exactly the timestamp, the
    /// checksum, a debug entry's timestamp and its `CodeView` GUID and age
    /// (ledger 4082 (4)); a byte beside them is compared.
    #[test]
    fn the_skipped_pe_fields_are_exactly_the_derived_ones() {
        let image = tiny_pe();
        assert_eq!(derived_fields(&image).unwrap(), (1, 1));
        let ranges = pe_derived(&image);
        assert_eq!(
            ranges,
            vec![0x88..0x8c, 0xd8..0xdc, 0x404..0x408, 0x444..0x458]
        );
        let mut rebuilt = image.clone();
        for range in &ranges {
            for at in range.clone() {
                rebuilt[at] ^= 0xFF;
            }
        }
        assert!(compare_rebuilt(&image, &rebuilt).is_ok());
        for beside in [0x8c, 0xd7, 0x408, 0x458] {
            let mut other = image.clone();
            other[beside] ^= 1;
            assert!(compare_rebuilt(&image, &other).is_err(), "byte {beside:#x}");
        }
    }

    /// A block of `table`, as gen-anchors.py wrote it.
    fn block_of(table: &str) -> Vec<u8> {
        std::fs::read(
            crate::paths::workspace_root()
                .join(BLOCKS)
                .join(format!("{table}.bin")),
        )
        .unwrap()
    }

    /// The release-image scan: a loader holding exactly its table's block
    /// passes; another table's, two blocks, none, or one byte changed is
    /// refused (docs/SIGNING.md §10.6; ledger 4016 (4)).
    #[test]
    fn a_loader_must_carry_exactly_its_tables_block() {
        let mut loader = vec![0u8; 3000];
        loader.extend_from_slice(&block_of("release-off"));
        loader.extend_from_slice(&[0u8; 100]);
        assert!(check_loader_policy_as("l", &loader, "release-off", false).is_ok());
        assert!(check_loader_policy_as("l", &loader, "release-required", false).is_err());
        assert!(check_loader_policy_as("l", &loader, "development-off", false).is_err());
        // A rustc PE without a .fxanch section.
        assert!(check_loader_policy_as("l", &loader, "release-off", true).is_err());
        // Its own block and another table's beside it: refused by the
        // other-table rule alone.
        let mut beside = loader.clone();
        beside.extend_from_slice(&block_of("development-off"));
        assert!(check_loader_policy_as("l", &beside, "release-off", false).is_err());
        let mut two = loader.clone();
        two.extend_from_slice(&block_of("release-off"));
        assert!(check_loader_policy_as("l", &two, "release-off", false).is_err());
        assert!(check_loader_policy_as("l", &[0u8; 4096], "release-off", false).is_err());
        let mut changed = loader.clone();
        changed[3000 + 200] ^= 1;
        assert!(check_loader_policy_as("l", &changed, "release-off", false).is_err());
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
