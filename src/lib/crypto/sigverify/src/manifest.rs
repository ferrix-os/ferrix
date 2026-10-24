//! The signed manifest of `docs/SIGNING.md` §1.2: one 4 KiB block, its
//! first 512 bytes the signed message, then an HSS signature.
//!
//! [`verify`] checks, in this order:
//!
//! 1. the structure: magic, format, lengths, the signature's length bound,
//!    and every byte that must be zero;
//! 2. the kind against the caller's expected kind, before anything else of
//!    the body is used;
//! 3. the signature, under the anchor of the caller's expected kind (for a
//!    revocation, its target kind) with the signed `anchor_id`;
//! 4. what the signed fields bind: profile, arch, family, subject, the
//!    revocations the caller holds, and the floor.
//!
//! Only then does it return a [`Verified`], whose payload the caller still
//! compares ([`Verified::check_payload`]).

use crate::lms::{self, HSS_PUBLIC_KEY_LEN};

/// The block's length.
pub const BLOCK_LEN: usize = 4096;
/// The signed message's length: bytes `0..BODY_LEN` of the block.
pub const BODY_LEN: usize = 512;
/// The longest signature the block takes (`docs/SIGNING.md` §1.2).
pub const SIGNATURE_MAX: usize = 3532;
/// The most trees one revocation names.
pub const REVOKED_MAX: usize = 20;

const MAGIC: &[u8; 8] = b"FXSIGMAN";
const FORMAT: u16 = 1;

/// A manifest's kind (`docs/SIGNING.md` §1.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum Kind {
    /// A system slot image.
    Slot = 1,
    /// A configuration package.
    Config = 2,
    /// A driver image.
    Driver = 3,
    /// A revocation of one kind's trees.
    Revocation = 4,
}

impl Kind {
    /// The kind a code names.
    #[must_use]
    pub const fn from_code(code: u16) -> Option<Kind> {
        match code {
            1 => Some(Kind::Slot),
            2 => Some(Kind::Config),
            3 => Some(Kind::Driver),
            4 => Some(Kind::Revocation),
            _ => None,
        }
    }

    /// Its code.
    #[must_use]
    pub const fn code(self) -> u16 {
        self as u16
    }
}

/// The field of the block a [`Refusal::Malformed`] names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    /// The block is not [`BLOCK_LEN`] bytes.
    Block,
    /// Bytes 0..8.
    Magic,
    /// Bytes 8..10.
    Format,
    /// Bytes 10..12.
    BodyLen,
    /// Bytes 12..14: not a kind.
    Kind,
    /// Bytes 14..16.
    Flags,
    /// Bytes 22: not a profile.
    Profile,
    /// Byte 23: not an arch.
    Arch,
    /// Bytes 28..32.
    Reserved,
    /// Bytes 72..104, non-zero in a slot manifest.
    Subject,
    /// Bytes 144..160, non-zero outside a slot manifest.
    Floors,
    /// Bytes 160..496, wrong for the kind: set outside a revocation, or a
    /// count past [`REVOKED_MAX`], or an entry past the count.
    Revocation,
    /// Bytes 164..176 or 496..512.
    Padding,
    /// Bytes 512..516: past [`SIGNATURE_MAX`] or the block.
    SignatureLen,
    /// After the signature: not zero.
    Trailer,
}

/// Why a manifest, or what it covers, is refused (`docs/SIGNING.md` §9.3).
/// One enum, so that every verifier names its reasons alike.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// A structural rule of §1.2, naming the field.
    Malformed(Field),
    /// A type code outside the allowed set.
    TypeCode,
    /// `L` outside 1..=2, or `Nspk + 1` not the anchor's `L`.
    Levels,
    /// No anchor of the expected kind has the signed `anchor_id`.
    UnknownAnchor,
    /// The signature does not verify (wrong length, leaf or root).
    BadSignature,
    /// A tree the signature went through is revoked for this kind.
    Revoked,
    /// A revocation that lists its own signing key (`docs/SIGNING.md`
    /// §7.4 (a)).
    RevokesItself,
    /// The kind is not the one expected.
    WrongKind,
    /// The profile is not the one expected.
    WrongProfile,
    /// The arch is not the one expected.
    WrongArch,
    /// The family is not one the caller accepts.
    WrongFamily,
    /// The subject is not the one expected.
    WrongSubject,
    /// A configuration or driver manifest checked with no subject: the
    /// caller must name the package or driver it will act on.
    NoSubject,
    /// The security version is below the floor.
    BelowFloor,
    /// The payload is not the one the manifest names.
    PayloadMismatch,
    /// A caller's refusal: no manifest where signing is required.
    Unsigned,
    /// A caller's refusal: the floors' storage is not provisioned.
    RollbackUnprovisioned,
}

impl From<lms::Error> for Refusal {
    fn from(error: lms::Error) -> Refusal {
        match error {
            lms::Error::TypeCode => Refusal::TypeCode,
            lms::Error::Levels => Refusal::Levels,
            lms::Error::Length | lms::Error::Leaf | lms::Error::BadSignature => {
                Refusal::BadSignature
            }
        }
    }
}

/// One trusted public key (`docs/SIGNING.md` §6.1): the kind it verifies,
/// its id, and its HSS public key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Anchor {
    /// The kind it verifies: [`Kind::Slot`], [`Kind::Config`] or
    /// [`Kind::Driver`]. A revocation is verified by its target kind's
    /// anchors.
    pub kind: Kind,
    /// Its `anchor_id`: 0 and 1 for product keys, `0x8000` and up for
    /// development keys (`docs/SIGNING.md` §6.4).
    pub id: u16,
    /// Its HSS public key (RFC 8554 §6.1).
    pub public_key: [u8; HSS_PUBLIC_KEY_LEN],
}

/// What the caller expects of a manifest.
#[derive(Clone, Copy, Debug)]
pub struct Expect<'a> {
    /// The kind it will act on.
    pub kind: Kind,
    /// For [`Kind::Revocation`] only: the kind whose trees it revokes.
    pub target: Option<Kind>,
    /// The running build's profile byte.
    pub profile: u8,
    /// The running build's arch byte.
    pub arch: u8,
    /// The hardware families it accepts, in no order.
    pub families: &'a [u32],
    /// The subject the caller will act on: required for a configuration
    /// or driver manifest (`None` refuses it), compared when given for any
    /// kind.
    pub subject: Option<&'a [u8; 32]>,
    /// The floor of this kind (`docs/SIGNING.md` §7).
    pub floor: u32,
}

/// A manifest that verified.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Verified {
    /// Its kind.
    pub kind: Kind,
    /// Its security version.
    pub security_version: u32,
    /// The anchor that verified it.
    pub anchor_id: u16,
    /// Its release sequence, informational.
    pub release: u64,
    /// Its label, informational.
    pub label: [u8; 32],
    /// The payload's length.
    pub payload_len: u64,
    /// The payload's SHA-256.
    pub payload_sha256: [u8; 32],
    /// Slot manifests: the floors this release sets for kinds 1 to 4.
    pub floors: [u32; 4],
    /// Revocations: the target kind, and the trees revoked.
    revocation: Option<(Kind, usize, [[u8; 16]; REVOKED_MAX])>,
}

impl Verified {
    /// Whether the payload the caller holds is the one this manifest names.
    pub fn check_payload(&self, len: u64, sha256: &[u8; 32]) -> Result<(), Refusal> {
        if len == self.payload_len && *sha256 == self.payload_sha256 {
            Ok(())
        } else {
            Err(Refusal::PayloadMismatch)
        }
    }

    /// A revocation's target kind and the trees it revokes.
    #[must_use]
    pub fn revoked(&self) -> Option<(Kind, &[[u8; 16]])> {
        let (kind, count, ids) = self.revocation.as_ref()?;
        Some((*kind, ids.get(..*count).unwrap_or(&[])))
    }
}

/// `L` bytes from `at`. Every field read is inside the [`BLOCK_LEN`]
/// block, which `header` checked first.
fn bytes<const L: usize>(block: &[u8], at: usize) -> [u8; L] {
    let mut out = [0u8; L];
    for (o, b) in out.iter_mut().zip(block.iter().skip(at)) {
        *o = *b;
    }
    out
}

fn byte(block: &[u8], at: usize) -> u8 {
    block.get(at).copied().unwrap_or(0)
}

fn le16(block: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(bytes::<2>(block, at))
}

fn le32(block: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes::<4>(block, at))
}

fn le64(block: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes::<8>(block, at))
}

/// Whether bytes `from..to` are zero. The block is [`BLOCK_LEN`] bytes
/// whenever this is asked, and every range a caller names is inside it.
fn zero(block: &[u8], from: usize, to: usize) -> bool {
    block.iter().take(to).skip(from).all(|&b| b == 0)
}

/// The fixed header of §1.2: magic, format, lengths, kind, flags,
/// profile, arch and the reserved word. Returns the kind.
fn header(block: &[u8]) -> Result<Kind, Refusal> {
    use Refusal::Malformed as M;
    if block.len() != BLOCK_LEN {
        return Err(M(Field::Block));
    }
    if bytes::<8>(block, 0) != *MAGIC {
        return Err(M(Field::Magic));
    }
    if le16(block, 8) != FORMAT {
        return Err(M(Field::Format));
    }
    if usize::from(le16(block, 10)) != BODY_LEN {
        return Err(M(Field::BodyLen));
    }
    let kind = Kind::from_code(le16(block, 12)).ok_or(M(Field::Kind))?;
    if le16(block, 14) != 0 {
        return Err(M(Field::Flags));
    }
    if !matches!(byte(block, 22), 1..=4 | 0xFF) {
        return Err(M(Field::Profile));
    }
    if !matches!(byte(block, 23), 1..=3) {
        return Err(M(Field::Arch));
    }
    if le32(block, 28) != 0 {
        return Err(M(Field::Reserved));
    }
    Ok(kind)
}

/// The fields only some kinds carry, and the padding: a slot's subject is
/// zero, only a slot has floors, only a revocation has its fields, and an
/// entry past a revocation's count is zero.
fn kind_fields(block: &[u8], kind: Kind) -> Result<(), Refusal> {
    use Refusal::Malformed as M;
    if kind == Kind::Slot && !zero(block, 72, 104) {
        return Err(M(Field::Subject));
    }
    if kind != Kind::Slot && !zero(block, 144, 160) {
        return Err(M(Field::Floors));
    }
    let revocation_ok = if kind == Kind::Revocation {
        let count = usize::from(le16(block, 162));
        let target = Kind::from_code(le16(block, 160));
        count <= REVOKED_MAX
            && matches!(target, Some(Kind::Slot | Kind::Config | Kind::Driver))
            && zero(block, entry_offset(count), 496)
    } else {
        zero(block, 160, 164) && zero(block, 176, 496)
    };
    if !revocation_ok {
        return Err(M(Field::Revocation));
    }
    if !zero(block, 164, 176) || !zero(block, 496, BODY_LEN) {
        return Err(M(Field::Padding));
    }
    Ok(())
}

/// The signature: `sig_len` bytes after the body's length word, at most
/// [`SIGNATURE_MAX`], with every byte after it zero.
fn signature_of(block: &[u8]) -> Result<&[u8], Refusal> {
    use Refusal::Malformed as M;
    let sig_len = usize::try_from(le32(block, BODY_LEN)).map_err(|_| M(Field::SignatureLen))?;
    if sig_len > SIGNATURE_MAX {
        return Err(M(Field::SignatureLen));
    }
    let sig_start = BODY_LEN.saturating_add(4);
    let sig_end = sig_start.saturating_add(sig_len);
    let signature = block
        .get(sig_start..sig_end)
        .ok_or(M(Field::SignatureLen))?;
    if !zero(block, sig_end, BLOCK_LEN) {
        return Err(M(Field::Trailer));
    }
    Ok(signature)
}

/// The structure of §1.2 (step 1): returns the kind and the signature.
fn structure(block: &[u8]) -> Result<(Kind, &[u8]), Refusal> {
    let kind = header(block)?;
    kind_fields(block, kind)?;
    Ok((kind, signature_of(block)?))
}

/// The offset of revocation entry `n`.
fn entry_offset(n: usize) -> usize {
    176usize.saturating_add(n.saturating_mul(16))
}

/// The anchor of the caller's expected kind (a revocation's: its target
/// kind's) with the manifest's signed anchor id.
fn anchor_for<'a>(
    block: &[u8],
    expect: &Expect<'_>,
    anchors: &'a [Anchor],
) -> Result<&'a Anchor, Refusal> {
    let anchor_kind = match expect.kind {
        Kind::Revocation => expect.target.ok_or(Refusal::WrongKind)?,
        other => other,
    };
    let anchor_id = le16(block, 20);
    anchors
        .iter()
        .find(|a| a.kind == anchor_kind && a.id == anchor_id)
        .ok_or(Refusal::UnknownAnchor)
}

/// Step 4's bound fields: profile, arch, family and subject.
fn bindings(block: &[u8], expect: &Expect<'_>) -> Result<(), Refusal> {
    if byte(block, 22) != expect.profile {
        return Err(Refusal::WrongProfile);
    }
    if byte(block, 23) != expect.arch {
        return Err(Refusal::WrongArch);
    }
    if !expect.families.contains(&le32(block, 24)) {
        return Err(Refusal::WrongFamily);
    }
    // A configuration or driver names what it is for: a caller that names
    // no subject for one is refused, never let through (ledger 3803 (1)).
    match (expect.kind, expect.subject) {
        (Kind::Config | Kind::Driver, None) => return Err(Refusal::NoSubject),
        (_, Some(subject)) if bytes::<32>(block, 72) != *subject => {
            return Err(Refusal::WrongSubject);
        }
        _ => {}
    }
    Ok(())
}

/// A revocation's target kind, count and trees, checked against the
/// target the caller expects.
type Revocation = (Kind, usize, [[u8; 16]; REVOKED_MAX]);

fn revocation_of(block: &[u8], expect: &Expect<'_>) -> Result<Option<Revocation>, Refusal> {
    if Kind::from_code(le16(block, 12)) != Some(Kind::Revocation) {
        return Ok(None);
    }
    let target = Kind::from_code(le16(block, 160)).ok_or(Refusal::Malformed(Field::Revocation))?;
    if Some(target) != expect.target {
        return Err(Refusal::WrongKind);
    }
    let count = usize::from(le16(block, 162));
    let mut ids = [[0u8; 16]; REVOKED_MAX];
    for (n, id) in ids.iter_mut().enumerate().take(count) {
        *id = bytes::<16>(block, entry_offset(n));
    }
    Ok(Some((target, count, ids)))
}

/// The revocations: a tree the signature went through that the caller
/// holds as revoked, or that the manifest, a revocation, lists itself.
fn revocations(
    chain: &lms::Chain,
    revoked: &[[u8; 16]],
    revocation: Option<&Revocation>,
) -> Result<(), Refusal> {
    if chain.ids().iter().any(|id| revoked.contains(id)) {
        return Err(Refusal::Revoked);
    }
    if let Some((_, count, ids)) = revocation {
        let listed = ids.get(..*count).unwrap_or(&[]);
        if chain.ids().iter().any(|id| listed.contains(id)) {
            return Err(Refusal::RevokesItself);
        }
    }
    Ok(())
}

/// Verify `block` against what the caller expects, its anchors and the
/// trees it holds as revoked for the anchors' kind (`docs/SIGNING.md`
/// §1.2, §7.4).
pub fn verify(
    block: &[u8],
    expect: &Expect<'_>,
    anchors: &[Anchor],
    revoked: &[[u8; 16]],
) -> Result<Verified, Refusal> {
    let (kind, signature) = structure(block)?;
    if kind != expect.kind {
        return Err(Refusal::WrongKind);
    }
    let anchor = anchor_for(block, expect, anchors)?;
    let body = block
        .get(..BODY_LEN)
        .ok_or(Refusal::Malformed(Field::Block))?;
    let chain = lms::hss_verify(&anchor.public_key, body, signature)?;
    bindings(block, expect)?;
    let revocation = revocation_of(block, expect)?;
    revocations(&chain, revoked, revocation.as_ref())?;
    let security_version = le32(block, 16);
    if security_version < expect.floor {
        return Err(Refusal::BelowFloor);
    }
    let mut floors = [0u32; 4];
    for (n, floor) in floors.iter_mut().enumerate() {
        *floor = le32(block, 144usize.saturating_add(n.saturating_mul(4)));
    }
    Ok(Verified {
        kind,
        security_version,
        anchor_id: anchor.id,
        release: le64(block, 32),
        label: bytes::<32>(block, 40),
        payload_len: le64(block, 104),
        payload_sha256: bytes::<32>(block, 112),
        floors,
        revocation,
    })
}
