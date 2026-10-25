//! Whether the UEFI loader may start a slot (`docs/SIGNING.md` §10.6).
//!
//! The loader reads a slot's header page and, for header version 2, its
//! manifest block, then asks [`decide`]. The answer is [`Start`], with the
//! header whose blobs the loader must then match on the one copy it starts,
//! or one [`Refusal`]. The loader starts nothing else (H.SIGN.7).
//!
//! What is trusted comes from the **trust-policy block**, 512 bytes the
//! loader carries in a section of its own (`.fxanch`): the slot-signing mode
//! and the slot kind's anchors. What the build is comes from [`Build`]: its
//! profile and arch bytes and the families it accepts. Neither is read from
//! the disk.
//!
//! `no_std`, no `unsafe`, no allocation: every buffer is the caller's.

#![no_std]
#![forbid(unsafe_code)]
#![deny(clippy::arithmetic_side_effects)]

use ferrix_sigverify::lms::HSS_PUBLIC_KEY_LEN;
use ferrix_sigverify::{Anchor, Expect, Kind};
use ferrix_slots::slot::{HEADER_LEN, MANIFEST_LEN, SlotError, SlotHeader, VERSION_1, VERSION_2};

#[cfg(test)]
mod tests;

/// The trust-policy block's length.
pub const BLOCK_LEN: usize = 512;
/// The most anchors the block holds.
pub const MAX_ANCHORS: usize = 8;
/// The most families a build accepts.
pub const MAX_FAMILIES: usize = 4;

/// What every block begins with.
pub const BLOCK_MAGIC: [u8; 8] = *b"FXPOLICY";
/// The block format this crate reads.
pub const BLOCK_VERSION: u16 = 1;
/// Where the anchor entries start.
pub const ENTRIES_AT: usize = 16;
/// Bytes per anchor entry: kind `u16`, anchor id `u16`, the HSS public key.
pub const ENTRY_LEN: usize = 4 + HSS_PUBLIC_KEY_LEN;
/// One past the last entry; zero from here to the end of the block.
const ENTRIES_END: usize = ENTRIES_AT + MAX_ANCHORS * ENTRY_LEN;

/// A slot manifest's payload: header bytes 0..4064, which the seal is the
/// SHA-256 of (`docs/SIGNING.md` §1.2).
pub const SEALED_LEN: u64 = (HEADER_LEN - 32) as u64;

/// Whether unsigned slots may start (`docs/SIGNING.md` §10.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Header version 1 starts on its hashes; version 2 is verified.
    Off = 0,
    /// Only a version-2 slot whose manifest verifies starts.
    Required = 1,
}

/// The part of the block a [`Refusal::Block`] names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockField {
    /// Fewer than [`BLOCK_LEN`] bytes.
    Short,
    /// Not [`BLOCK_MAGIC`].
    Magic,
    /// Not [`BLOCK_VERSION`].
    Version,
    /// Not a [`Mode`].
    Mode,
    /// More than [`MAX_ANCHORS`] anchors.
    Count,
    /// An anchor of a kind other than the slot kind.
    Kind,
    /// A reserved byte, an unused entry or the tail is not zero.
    Padding,
}

/// Why a slot is not started (`docs/SIGNING.md` §9.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The loader's own trust-policy block is malformed: every slot is
    /// refused.
    Block(BlockField),
    /// The slot header does not decode.
    Header(SlotError),
    /// A version-1 slot where signing is required.
    Unsigned,
    /// A version-2 slot whose manifest is refused, or does not cover this
    /// header.
    Manifest(ferrix_sigverify::Refusal),
}

/// The trust policy a block holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    /// The slot-signing mode.
    pub mode: Mode,
    anchors: [Anchor; MAX_ANCHORS],
    count: usize,
}

impl Policy {
    /// The slot kind's anchors.
    #[must_use]
    pub fn anchors(&self) -> &[Anchor] {
        self.anchors.get(..self.count).unwrap_or(&[])
    }
}

/// What the running build is.
#[derive(Clone, Copy, Debug)]
pub struct Build<'a> {
    /// Its profile byte (`docs/SIGNING.md` §1.2; 0xFF for a development
    /// image).
    pub profile: u8,
    /// Its arch byte: 1 x86-64, 2 AArch64, 3 ARMv7-A.
    pub arch: u8,
    /// Whether its profile carries a claim: then only `families`, exactly,
    /// with no generic fallback (§1.4).
    pub claimed: bool,
    /// The families its profile accepts.
    pub families: &'a [u32],
}

/// A slot the loader may start, once every blob of `header` matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Start {
    /// The header the blobs are matched against.
    pub header: SlotHeader,
    /// The signature that let it start: `None` for a version-1 slot under
    /// [`Mode::Off`].
    pub signed: Option<Signed>,
}

/// The manifest that verified.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Signed {
    /// The anchor that verified it.
    pub anchor_id: u16,
    /// Its security version.
    pub security_version: u32,
}

/// Where the loader goes when no slot starts, or the disk has none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rescue {
    /// The ESP's `KERNEL.ELF`, `INITRD.IMG` and `CMDLINE.TXT`, unverified.
    UnsignedFiles,
    /// The ESP's `RESCUE.SLT`, a slot image decided as a slot is; the
    /// unsigned files are not read (§9.2).
    SlotImage,
}

/// Read a trust-policy block.
///
/// # Errors
///
/// [`Refusal::Block`], naming the first field that is wrong.
pub fn parse_block(block: &[u8]) -> Result<Policy, Refusal> {
    let block = block
        .get(..BLOCK_LEN)
        .ok_or(Refusal::Block(BlockField::Short))?;
    if block.get(..8) != Some(&BLOCK_MAGIC[..]) {
        return Err(Refusal::Block(BlockField::Magic));
    }
    if le16(block, 8) != BLOCK_VERSION {
        return Err(Refusal::Block(BlockField::Version));
    }
    let mode = match byte(block, 10) {
        0 => Mode::Off,
        1 => Mode::Required,
        _ => return Err(Refusal::Block(BlockField::Mode)),
    };
    let count = usize::from(byte(block, 11));
    if count > MAX_ANCHORS {
        return Err(Refusal::Block(BlockField::Count));
    }
    let zero = |from: usize, to: usize| {
        block
            .get(from..to)
            .is_some_and(|b| b.iter().all(|&x| x == 0))
    };
    if !zero(12, ENTRIES_AT) || !zero(ENTRIES_END, BLOCK_LEN) {
        return Err(Refusal::Block(BlockField::Padding));
    }
    Ok(Policy {
        mode,
        anchors: anchor_entries(block, count)?,
        count,
    })
}

/// The block's `count` anchor entries, every unused entry zero.
fn anchor_entries(block: &[u8], count: usize) -> Result<[Anchor; MAX_ANCHORS], Refusal> {
    let mut anchors = [Anchor {
        kind: Kind::Slot,
        id: 0,
        public_key: [0; HSS_PUBLIC_KEY_LEN],
    }; MAX_ANCHORS];
    for (index, anchor) in anchors.iter_mut().enumerate() {
        // At most MAX_ANCHORS entries of ENTRY_LEN after ENTRIES_AT: inside
        // the block, whose length was checked above.
        let at = ENTRIES_AT.saturating_add(index.saturating_mul(ENTRY_LEN));
        let entry = block
            .get(at..at.saturating_add(ENTRY_LEN))
            .ok_or(Refusal::Block(BlockField::Short))?;
        if index >= count {
            if entry.iter().any(|&x| x != 0) {
                return Err(Refusal::Block(BlockField::Padding));
            }
            continue;
        }
        if le16(entry, 0) != Kind::Slot.code() {
            return Err(Refusal::Block(BlockField::Kind));
        }
        anchor.id = le16(entry, 2);
        for (to, from) in anchor.public_key.iter_mut().zip(entry.iter().skip(4)) {
            *to = *from;
        }
    }
    Ok(anchors)
}

/// The block for `mode` and these anchors: what `xtask gen-anchors`
/// writes, and the tests build.
#[must_use]
pub fn encode_block(mode: Mode, anchors: &[Anchor]) -> [u8; BLOCK_LEN] {
    let mut out = [0u8; BLOCK_LEN];
    put(&mut out, 0, &BLOCK_MAGIC);
    put(&mut out, 8, &BLOCK_VERSION.to_le_bytes());
    put(&mut out, 10, &[mode as u8]);
    let count = anchors.len().min(MAX_ANCHORS);
    put(&mut out, 11, &[u8::try_from(count).unwrap_or(0)]);
    for (index, anchor) in anchors.iter().take(MAX_ANCHORS).enumerate() {
        let at = ENTRIES_AT.saturating_add(index.saturating_mul(ENTRY_LEN));
        put(&mut out, at, &anchor.kind.code().to_le_bytes());
        put(&mut out, at.saturating_add(2), &anchor.id.to_le_bytes());
        put(&mut out, at.saturating_add(4), &anchor.public_key);
    }
    out
}

/// The generic family of an arch byte (`hardware-families.toml`): the high
/// byte the arch, the low byte 1.
#[must_use]
pub const fn generic_family(arch: u8) -> u32 {
    ((arch as u32) << 8) | 1
}

/// The families a manifest may name on this machine (§1.4): with a claim,
/// the profile's families that are the machine's own; without one, the
/// profile's families that are the machine's own or its arch's generic one.
fn accepted(build: &Build<'_>, family: u32, out: &mut [u32; MAX_FAMILIES]) -> usize {
    let generic = generic_family(build.arch);
    let mut n = 0usize;
    for &f in build.families.iter().take(MAX_FAMILIES) {
        let fits = f == family || (!build.claimed && f == generic);
        if fits && let Some(slot) = out.get_mut(n) {
            *slot = f;
            n = n.saturating_add(1);
        }
    }
    n
}

/// Whether the slot whose header page is `header_page` may start.
///
/// * `block`: the loader's trust-policy block;
/// * `build`: what the running build is;
/// * `family`: the machine's family, by the registry's rule (the arch's
///   generic one when nothing more specific is recognised);
/// * `floor`: the slot kind's floor (zero while `rollback = absent`);
/// * `manifest`: bytes 4096..8192 of the slot, read only for version 2;
/// * `slot_bytes`: the partition's (or the rescue file's) length.
///
/// # Errors
///
/// The [`Refusal`] that keeps the slot from starting.
pub fn decide(
    block: &[u8],
    build: &Build<'_>,
    family: u32,
    floor: u32,
    header_page: &[u8],
    manifest: Option<&[u8]>,
    slot_bytes: u64,
) -> Result<Start, Refusal> {
    let policy = parse_block(block)?;
    let header = SlotHeader::decode(header_page, slot_bytes).map_err(Refusal::Header)?;
    match header.version {
        VERSION_1 => match policy.mode {
            Mode::Off => Ok(Start {
                header,
                signed: None,
            }),
            Mode::Required => Err(Refusal::Unsigned),
        },
        VERSION_2 => {
            let block = manifest
                .filter(|m| m.len() == MANIFEST_LEN)
                .ok_or(Refusal::Manifest(ferrix_sigverify::Refusal::Malformed(
                    ferrix_sigverify::Field::Block,
                )))?;
            let mut families = [0u32; MAX_FAMILIES];
            let n = accepted(build, family, &mut families);
            let expect = Expect {
                kind: Kind::Slot,
                target: None,
                profile: build.profile,
                arch: build.arch,
                families: families.get(..n).unwrap_or(&[]),
                subject: None,
                floor,
            };
            let verified = ferrix_sigverify::verify(block, &expect, policy.anchors(), &[])
                .map_err(Refusal::Manifest)?;
            // The payload is header bytes 0..4064, which the seal hashes.
            verified
                .check_payload(SEALED_LEN, &header.seal())
                .map_err(Refusal::Manifest)?;
            Ok(Start {
                header,
                signed: Some(Signed {
                    anchor_id: verified.anchor_id,
                    security_version: verified.security_version,
                }),
            })
        }
        // decode accepts only the two versions.
        _ => Err(Refusal::Header(SlotError::Version)),
    }
}

/// What the ESP offers when no slot starts (§9.2): the unsigned files
/// only while signing is off.
///
/// # Errors
///
/// The block's own refusal: a malformed block offers nothing.
pub fn rescue(block: &[u8]) -> Result<Rescue, Refusal> {
    Ok(match parse_block(block)?.mode {
        Mode::Off => Rescue::UnsignedFiles,
        Mode::Required => Rescue::SlotImage,
    })
}

fn byte(bytes: &[u8], at: usize) -> u8 {
    bytes.get(at).copied().unwrap_or(0)
}

fn le16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([byte(bytes, at), byte(bytes, at.saturating_add(1))])
}

fn put(out: &mut [u8], at: usize, bytes: &[u8]) {
    for (to, from) in out.iter_mut().skip(at).zip(bytes) {
        *to = *from;
    }
}
