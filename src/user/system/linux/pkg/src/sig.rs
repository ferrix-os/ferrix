//! The signatures over a repository's index and its packages
//! (`docs/UPDATES.md` §2.2): `docs/SIGNING.md`'s one 4 KiB manifest, of
//! kind 5, beside each file as `<file>.fxsig`, verified by
//! `ferrix-sigverify` on the bytes `pkg` holds in memory and will use.
//!
//! The index's subject is [`INDEX_SUBJECT`], which no package can be named,
//! and its security version is its sequence, so the verifier's floor --
//! the accepted index's sequence -- refuses an older index. A package's
//! subject is its name and its security version zero; the index binds its
//! version by digest. Every manifest names the development profile, the
//! package's architecture and that architecture's generic hardware family:
//! a package is built per architecture, not per board.

use ferrix_sigverify::{Anchor, Expect, Kind, Refusal, verify};

use crate::{Error, this_arch};

include!(concat!(env!("OUT_DIR"), "/anchors.rs"));

/// What a signature's file name adds to its file's.
pub const SUFFIX: &str = ".fxsig";

/// The index's subject.
pub const INDEX_SUBJECT: &str = ":index";

/// The profile byte of a development build (`docs/SIGNING.md` §1.2), the
/// only one a package key exists for.
const DEVELOPMENT: u8 = 0xFF;

/// This architecture's byte in a manifest.
#[must_use]
pub fn arch_byte() -> u8 {
    match this_arch() {
        "x86_64" => 1,
        "aarch64" => 2,
        _ => 3,
    }
}

/// This architecture's generic hardware family (`docs/SIGNING.md` §1.4).
#[must_use]
pub fn family() -> u32 {
    match this_arch() {
        "x86_64" => 0x0101,
        "aarch64" => 0x0201,
        _ => 0x0301,
    }
}

/// `text` as a manifest's subject, NUL padded; `None` past 32 bytes.
fn subject(text: &str) -> Option<[u8; 32]> {
    let mut out = [0u8; 32];
    if text.len() > out.len() {
        return None;
    }
    for (o, b) in out.iter_mut().zip(text.bytes()) {
        *o = b;
    }
    Some(out)
}

/// A refusal of the verifier's, in words.
fn why(refusal: Refusal) -> String {
    let words = match refusal {
        Refusal::BadSignature => "its signature does not verify",
        Refusal::UnknownAnchor => "it is signed by no key this pkg trusts",
        Refusal::WrongSubject => "it is signed for something else",
        Refusal::WrongKind => "it is not a package repository's signature",
        Refusal::PayloadMismatch => "it is not the file its signature names",
        Refusal::BelowFloor => "it is older than the one accepted",
        Refusal::Revoked => "its key is revoked",
        _ => "its signature is refused",
    };
    format!("{words} ({refusal:?})")
}

/// Verify `block`, the manifest beside `payload`, as the signature of the
/// subject `what`, under `anchors`, at or above `floor`. Returns its
/// security version.
///
/// # Errors
///
/// The verifier's refusal, or a payload other than the one it signs, each
/// as a line saying why.
pub fn check(
    block: &[u8],
    what: &str,
    floor: u32,
    payload: &[u8],
    anchors: &[Anchor],
) -> Result<u32, Error> {
    let named = subject(what).ok_or_else(|| Error(format!("{what}: a name past 32 bytes")))?;
    let families = [family()];
    let expect = Expect {
        kind: Kind::Package,
        target: None,
        profile: DEVELOPMENT,
        arch: arch_byte(),
        families: &families,
        subject: Some(&named),
        floor,
    };
    let refused = |refusal| Error(format!("{}: refused", why(refusal)));
    let verified = verify(block, &expect, anchors, &[]).map_err(refused)?;
    let len = u64::try_from(payload.len()).unwrap_or(u64::MAX);
    verified
        .check_payload(len, &ferrix_pkg::index::sha256(payload))
        .map_err(refused)?;
    Ok(verified.security_version)
}
