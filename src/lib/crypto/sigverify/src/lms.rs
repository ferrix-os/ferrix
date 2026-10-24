//! LM-OTS, LMS and HSS verification (RFC 8554 §4.6, §5.4.2, §6.3) over
//! SHA-256/192 (RFC 9858).
//!
//! The algorithms are written once for every SHA-256/192 set (LM-OTS
//! W1-W8, LMS H5-H25), with each set's constants from its type code. What
//! a caller may accept is a separate [`Allowed`] list: [`hss_verify`]
//! holds every signature to [`PRODUCT`], the sets `docs/SIGNING.md` §2.1
//! allows -- `LMOTS_SHA256_N24_W4` and `LMS_SHA256_M24_H10`, `_H15` and
//! `_H20`, at one or two levels -- and refuses every other code. The wider
//! list exists so that the published vectors of the other sets (RFC 9858
//! A.1, NIST ACVP) run through the same code, in host tests.
//!
//! Verification only: there is no key generation, no signing and no
//! private state here. Every input is a slice its caller lends, every
//! length is checked before it is read, and no function allocates. The
//! work per level is bounded by the type codes' constants: under
//! [`PRODUCT`] at most `p * (2^w - 1)` = 765 chain hashes, the K and Q
//! hashes, and `h` tree hashes (`docs/SIGNING.md` §8.5).

use ferrix_sha256::{Sha256, finish_192};

/// The hash length `n` (and `m`) of every allowed set: SHA-256/192.
pub const N: usize = 24;
/// The number of Winternitz chains `p` of `LMOTS_SHA256_N24_W4`.
pub const P: usize = 51;

/// `LMOTS_SHA256_N24_W4` (RFC 9858 Table 1; IANA).
pub const LMOTS_SHA256_N24_W4: u32 = 0x0000_0007;
/// `LMS_SHA256_M24_H10` (RFC 9858 Table 2; IANA).
pub const LMS_SHA256_M24_H10: u32 = 0x0000_000B;
/// `LMS_SHA256_M24_H15`.
pub const LMS_SHA256_M24_H15: u32 = 0x0000_000C;
/// `LMS_SHA256_M24_H20`.
pub const LMS_SHA256_M24_H20: u32 = 0x0000_000D;

/// The type codes a caller accepts, each with its constants.
#[derive(Clone, Copy, Debug)]
pub struct Allowed {
    /// LM-OTS types and their constants (RFC 9858 Table 1).
    ots: &'static [(u32, Ots)],
    /// LMS types and their heights (RFC 9858 Table 2).
    lms: &'static [(u32, u32)],
}

/// The product set (`docs/SIGNING.md` §2.1): `LMOTS_SHA256_N24_W4` with
/// `LMS_SHA256_M24_H10`, `_H15` or `_H20`.
pub const PRODUCT: Allowed = Allowed {
    ots: &[(LMOTS_SHA256_N24_W4, Ots { w: 4, p: 51, ls: 4 })],
    lms: &[
        (LMS_SHA256_M24_H10, 10),
        (LMS_SHA256_M24_H15, 15),
        (LMS_SHA256_M24_H20, 20),
    ],
};

/// The SHA-256/192 sets published vectors exist for outside [`PRODUCT`]:
/// LM-OTS W1, W2 and W8, and LMS H5 (RFC 9858 A.1, NIST ACVP), for host
/// tests only.
#[cfg(test)]
pub(crate) const EVERY_SHA256_192: Allowed = Allowed {
    ots: &[
        (
            5,
            Ots {
                w: 1,
                p: 200,
                ls: 8,
            },
        ),
        (
            6,
            Ots {
                w: 2,
                p: 101,
                ls: 6,
            },
        ),
        (LMOTS_SHA256_N24_W4, Ots { w: 4, p: 51, ls: 4 }),
        (8, Ots { w: 8, p: 26, ls: 0 }),
    ],
    lms: &[
        (0x0A, 5),
        (LMS_SHA256_M24_H10, 10),
        (LMS_SHA256_M24_H15, 15),
        (LMS_SHA256_M24_H20, 20),
    ],
};

impl Allowed {
    fn ots(&self, code: u32) -> Option<Ots> {
        self.ots.iter().find(|(c, _)| *c == code).map(|(_, o)| *o)
    }

    fn lms(&self, code: u32) -> Option<u32> {
        self.lms.iter().find(|(c, _)| *c == code).map(|(_, h)| *h)
    }
}

/// One LM-OTS set's constants (RFC 9858 Table 1).
#[derive(Clone, Copy, Debug)]
struct Ots {
    /// `w`.
    w: u32,
    /// `p`.
    p: usize,
    /// `ls`.
    ls: u32,
}

#[expect(
    clippy::arithmetic_side_effects,
    reason = "AUDIT: w is 1, 2, 4 or 8 and p at most 200, from the Allowed tables, so 2^w - 1 is at most 255, 4 + 24 + 200 * 24 is 4,828 and 192 / w at least 24: no operation can overflow, underflow or divide by zero"
)]
impl Ots {
    const fn max_digit(self) -> u32 {
        (1 << self.w) - 1
    }

    const fn signature_len(self) -> usize {
        4 + N + self.p * N
    }

    /// The message's digits, before the checksum's: `8n / w`.
    const fn message_digits(self) -> usize {
        N * 8 / (self.w as usize)
    }
}

/// The domain separators of RFC 8554 §4.3 and §5.3.
const D_PBLC: [u8; 2] = [0x80, 0x80];
const D_MESG: [u8; 2] = [0x81, 0x81];
const D_LEAF: [u8; 2] = [0x82, 0x82];
const D_INTR: [u8; 2] = [0x83, 0x83];

/// An LM-OTS signature: type, `C`, and `p` chain values.
pub const OTS_SIGNATURE_LEN: usize = 4 + N + P * N;
/// An LMS public key: LMS type, LM-OTS type, `I`, `T[1]` (RFC 8554 §5.3).
pub const LMS_PUBLIC_KEY_LEN: usize = 4 + 4 + 16 + N;
/// An HSS public key: `L`, then the top LMS public key (RFC 8554 §6.1).
pub const HSS_PUBLIC_KEY_LEN: usize = 4 + LMS_PUBLIC_KEY_LEN;
/// The most levels an allowed key has.
pub const MAX_LEVELS: usize = 2;
/// The longest allowed HSS signature: two levels of H20, 3,532 bytes.
pub const HSS_SIGNATURE_MAX: usize = 4 + 2 * lms_signature_len(20) + LMS_PUBLIC_KEY_LEN;

/// Why a signature did not verify.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// A type code outside the allowed set, or two that disagree.
    TypeCode,
    /// `L` outside 1..=2, or `Nspk + 1` not `L`.
    Levels,
    /// A signature or key not exactly as long as its types say.
    Length,
    /// A leaf index `q` at or past `2^h`.
    Leaf,
    /// Well formed, but the candidate root is not the key's.
    BadSignature,
}

/// The length of a product-set LMS signature with tree height `h`
/// (RFC 8554 §5.4).
#[must_use]
#[expect(
    clippy::arithmetic_side_effects,
    reason = "AUDIT: callers pass an allowed height, at most 25, so the sum is at most 1,860"
)]
pub const fn lms_signature_len(h: usize) -> usize {
    4 + OTS_SIGNATURE_LEN + 4 + h * N
}

/// The tree height of a product-set LMS type.
#[must_use]
pub fn height(lms_type: u32) -> Option<u32> {
    PRODUCT.lms(lms_type)
}

/// A cursor over borrowed bytes that never reads past their end.
struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    const fn new(bytes: &'a [u8]) -> Reader<'a> {
        Reader { rest: bytes }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], Error> {
        let (head, rest) = self.rest.split_at_checked(len).ok_or(Error::Length)?;
        self.rest = rest;
        Ok(head)
    }

    fn array<const L: usize>(&mut self) -> Result<[u8; L], Error> {
        let bytes = self.take(L)?;
        <[u8; L]>::try_from(bytes).map_err(|_| Error::Length)
    }

    fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_be_bytes(self.array::<4>()?))
    }

    const fn is_empty(&self) -> bool {
        self.rest.is_empty()
    }
}

/// An LMS public key, parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LmsKey {
    /// Its LMS type.
    pub lms_type: u32,
    /// Its LM-OTS type.
    pub ots_type: u32,
    /// The tree's identifier `I`.
    pub id: [u8; 16],
    /// The tree's root `T[1]`.
    pub root: [u8; N],
}

impl LmsKey {
    /// Parse exactly [`LMS_PUBLIC_KEY_LEN`] bytes, refusing a type outside
    /// [`PRODUCT`].
    pub fn parse(bytes: &[u8]) -> Result<LmsKey, Error> {
        LmsKey::parse_in(&PRODUCT, bytes)
    }

    pub(crate) fn parse_in(allowed: &Allowed, bytes: &[u8]) -> Result<LmsKey, Error> {
        let mut r = Reader::new(bytes);
        let lms_type = r.u32()?;
        let ots_type = r.u32()?;
        let id = r.array::<16>()?;
        let root = r.array::<N>()?;
        if !r.is_empty() {
            return Err(Error::Length);
        }
        if allowed.lms(lms_type).is_none() || allowed.ots(ots_type).is_none() {
            return Err(Error::TypeCode);
        }
        Ok(LmsKey {
            lms_type,
            ots_type,
            id,
            root,
        })
    }
}

/// Digit `i` of `s` at Winternitz parameter `w` (RFC 8554 §3.1.3,
/// `coef`): `(2^w - 1) & (byte(S, floor(i * w / 8)) >> (8 - (w * (i %
/// (8 / w)) + w)))`.
#[expect(
    clippy::arithmetic_side_effects,
    reason = "AUDIT: w is 1, 2, 4 or 8 and i below p + checksum digits (at most 208), so i * w is at most 1,664, 8 / w is at least 1, and w * (i % (8 / w)) + w is at most 8: no operation overflows, underflows or divides by zero"
)]
fn coef(s: &[u8; N + 2], i: usize, w: u32) -> u32 {
    let w_bits = w as usize;
    let per_byte = 8 / w_bits;
    let byte = u32::from(s.get(i * w_bits / 8).copied().unwrap_or(0));
    let shift = 8 - (w_bits * (i % per_byte) + w_bits);
    ((1u32 << w) - 1) & (byte >> shift)
}

/// One step of a Winternitz chain: `H(I || u32str(q) || u16str(i) ||
/// u8str(j) || tmp)`, 47 bytes, one SHA-256 block.
fn chain_step(id: &[u8; 16], q: u32, i: u16, j: u8, tmp: &[u8; N]) -> [u8; N] {
    let mut hash = Sha256::new();
    hash.update(id);
    hash.update(&q.to_be_bytes());
    hash.update(&i.to_be_bytes());
    hash.update(&[j]);
    hash.update(tmp);
    finish_192(hash)
}

/// `Q || Cksm(Q)`: the message digest and its checksum (RFC 8554 §4.4),
/// shifted by `ls`, as the 26 bytes the digits are read from.
fn with_checksum(ots: Ots, digest: &[u8; N]) -> Result<[u8; N + 2], Error> {
    let mut digits = [0u8; N + 2];
    for (d, q_byte) in digits.iter_mut().zip(digest) {
        *d = *q_byte;
    }
    let mut sum: u32 = 0;
    for i in 0..ots.message_digits() {
        // A digit is at most 2^w - 1, so the difference is never negative.
        sum = sum.wrapping_add(ots.max_digit().saturating_sub(coef(&digits, i, ots.w)));
    }
    let checksum = u16::try_from(sum << ots.ls).map_err(|_| Error::Length)?;
    let [.., high, low] = &mut digits;
    [*high, *low] = checksum.to_be_bytes();
    Ok(digits)
}

/// RFC 8554 Algorithm 4b: the LM-OTS public key candidate `Kc` that
/// `signature` gives for `message` under `I` and `q`. `expected` is the
/// key's LM-OTS type, which the signature's own must equal. The caller,
/// [`lms_verify_in`], hands over exactly the bytes the key's types say.
fn ots_candidate(
    allowed: &Allowed,
    expected: u32,
    id: &[u8; 16],
    q: u32,
    signature: &[u8],
    message: &[u8],
) -> Result<[u8; N], Error> {
    let mut r = Reader::new(signature);
    let code = r.u32()?;
    if code != expected {
        return Err(Error::TypeCode);
    }
    let ots = allowed.ots(code).ok_or(Error::TypeCode)?;
    let c = r.take(N)?;

    let mut hash = Sha256::new();
    hash.update(id);
    hash.update(&q.to_be_bytes());
    hash.update(&D_MESG);
    hash.update(c);
    hash.update(message);
    let digits = with_checksum(ots, &finish_192(hash))?;

    let mut key = Sha256::new();
    key.update(id);
    key.update(&q.to_be_bytes());
    key.update(&D_PBLC);
    for chain in 0..ots.p {
        let mut tmp = r.array::<N>()?;
        let index = u16::try_from(chain).map_err(|_| Error::Length)?;
        for j in coef(&digits, chain, ots.w)..ots.max_digit() {
            let step = u8::try_from(j).map_err(|_| Error::Length)?;
            tmp = chain_step(id, q, index, step, &tmp);
        }
        key.update(&tmp);
    }
    Ok(finish_192(key))
}

/// The length of an LMS signature under `key`'s types.
#[expect(
    clippy::arithmetic_side_effects,
    reason = "AUDIT: an allowed OTS signature is at most 4,828 bytes and h at most 25, so the sum is at most 5,436"
)]
fn signature_len_for(allowed: &Allowed, key: &LmsKey) -> Result<usize, Error> {
    let h = allowed.lms(key.lms_type).ok_or(Error::TypeCode)?;
    let ots = allowed.ots(key.ots_type).ok_or(Error::TypeCode)?;
    let h = usize::try_from(h).map_err(|_| Error::Length)?;
    Ok(4 + ots.signature_len() + 4 + h * N)
}

/// RFC 8554 Algorithm 6a: whether `signature` is `key`'s LMS signature of
/// `message` under `allowed`. Its caller, [`hss_verify_in`], hands over
/// exactly the [`signature_len_for`] bytes `key`'s types say; the
/// signature's own LMS type must be the key's.
pub(crate) fn lms_verify_in(
    allowed: &Allowed,
    key: &LmsKey,
    message: &[u8],
    signature: &[u8],
) -> Result<(), Error> {
    let h = allowed.lms(key.lms_type).ok_or(Error::TypeCode)?;
    let ots_len = allowed
        .ots(key.ots_type)
        .ok_or(Error::TypeCode)?
        .signature_len();
    let mut r = Reader::new(signature);
    let q = r.u32()?;
    let ots = r.take(ots_len)?;
    let signed_type = r.u32()?;
    if signed_type != key.lms_type {
        return Err(Error::TypeCode);
    }
    if q >= (1u32 << h) {
        return Err(Error::Leaf);
    }
    let candidate = ots_candidate(allowed, key.ots_type, &key.id, q, ots, message)?;
    let root = root_from(&key.id, (1u32 << h) | q, &candidate, &mut r)?;
    if root == key.root {
        Ok(())
    } else {
        Err(Error::BadSignature)
    }
}

/// RFC 8554 Algorithm 6a's climb: the root from leaf node `node`'s
/// candidate and the path `r` holds, one sibling a level.
fn root_from(
    id: &[u8; 16],
    mut node: u32,
    candidate: &[u8; N],
    r: &mut Reader<'_>,
) -> Result<[u8; N], Error> {
    let mut hash = Sha256::new();
    hash.update(id);
    hash.update(&node.to_be_bytes());
    hash.update(&D_LEAF);
    hash.update(candidate);
    let mut tmp = finish_192(hash);
    while node > 1 {
        let sibling = r.take(N)?;
        let parent = node >> 1;
        let mut hash = Sha256::new();
        hash.update(id);
        hash.update(&parent.to_be_bytes());
        hash.update(&D_INTR);
        if node & 1 == 1 {
            hash.update(sibling);
            hash.update(&tmp);
        } else {
            hash.update(&tmp);
            hash.update(sibling);
        }
        tmp = finish_192(hash);
        node = parent;
    }
    Ok(tmp)
}

/// The trees an HSS signature went through, top first: the identifiers a
/// revocation names (`docs/SIGNING.md` §7.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chain {
    levels: usize,
    ids: [[u8; 16]; MAX_LEVELS],
}

impl Chain {
    /// The trees' identifiers `I`, top first.
    #[must_use]
    pub fn ids(&self) -> &[[u8; 16]] {
        self.ids.get(..self.levels).unwrap_or(&[])
    }
}

/// The HSS public key's level count `L`, and its top LMS key.
fn parse_hss_key_in(
    allowed: &Allowed,
    public_key: &[u8; HSS_PUBLIC_KEY_LEN],
) -> Result<(usize, LmsKey), Error> {
    let mut r = Reader::new(public_key);
    let levels = r.u32()?;
    let top = LmsKey::parse_in(allowed, r.take(LMS_PUBLIC_KEY_LEN)?)?;
    let levels = usize::try_from(levels).map_err(|_| Error::Levels)?;
    if !(1..=MAX_LEVELS).contains(&levels) {
        return Err(Error::Levels);
    }
    Ok((levels, top))
}

/// One `signed_pub_key` of RFC 8554 §6.2: the next level's LMS public
/// key, verified under `key`.
fn lower_key(allowed: &Allowed, key: &LmsKey, r: &mut Reader<'_>) -> Result<LmsKey, Error> {
    let signed = r.take(signature_len_for(allowed, key)?)?;
    let next_bytes = r.take(LMS_PUBLIC_KEY_LEN)?;
    let next = LmsKey::parse_in(allowed, next_bytes)?;
    lms_verify_in(allowed, key, next_bytes, signed)?;
    Ok(next)
}

/// RFC 8554 Algorithm 6: whether `signature` is the HSS signature of
/// `message` under `public_key`. The signature must be exactly as long as
/// its levels' types say, with `Nspk + 1 = L`.
pub fn hss_verify(
    public_key: &[u8; HSS_PUBLIC_KEY_LEN],
    message: &[u8],
    signature: &[u8],
) -> Result<Chain, Error> {
    hss_verify_in(&PRODUCT, public_key, message, signature)
}

/// [`hss_verify`] under `allowed`.
pub(crate) fn hss_verify_in(
    allowed: &Allowed,
    public_key: &[u8; HSS_PUBLIC_KEY_LEN],
    message: &[u8],
    signature: &[u8],
) -> Result<Chain, Error> {
    let (levels, top) = parse_hss_key_in(allowed, public_key)?;
    let mut r = Reader::new(signature);
    let nspk = usize::try_from(r.u32()?).map_err(|_| Error::Levels)?;
    if nspk.checked_add(1) != Some(levels) {
        return Err(Error::Levels);
    }
    let mut chain = Chain {
        levels,
        ids: [top.id, [0; 16]],
    };
    let mut key = top;
    if levels == MAX_LEVELS {
        key = lower_key(allowed, &key, &mut r)?;
        let [_, second] = &mut chain.ids;
        *second = key.id;
    }
    let last = r.take(signature_len_for(allowed, &key)?)?;
    if !r.is_empty() {
        return Err(Error::Length);
    }
    lms_verify_in(allowed, &key, message, last)?;
    Ok(chain)
}
