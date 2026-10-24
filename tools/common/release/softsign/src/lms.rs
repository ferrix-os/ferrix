//! LMS key generation and signing for SHA-256/192 and
//! `LMOTS_SHA256_N24_W4` (RFC 8554 §4-§5, RFC 9858), with the private keys
//! derived from a seed as RFC 8554 Appendix A does.
//!
//! Host code only: it lives in this development tool and no image links
//! it (`docs/SIGNING.md` §5.1). It holds no state of its own; the caller
//! decides which leaf to use and must never use one twice ([`crate::token`]
//! is what enforces that).

use ferrix_sha256::{Sha256, finish_192};

/// `n`: SHA-256/192.
pub const N: usize = 24;
/// `p` of `LMOTS_SHA256_N24_W4`.
pub const P: usize = 51;
/// `LMOTS_SHA256_N24_W4`.
pub const LMOTS_SHA256_N24_W4: u32 = 0x0000_0007;
/// `LMS_SHA256_M24_H10`, `_H15`, `_H20`.
pub const LMS_SHA256_M24_H10: u32 = 0x0000_000B;
/// See [`LMS_SHA256_M24_H10`].
pub const LMS_SHA256_M24_H15: u32 = 0x0000_000C;
/// See [`LMS_SHA256_M24_H10`].
pub const LMS_SHA256_M24_H20: u32 = 0x0000_000D;

const MAX_DIGIT: u8 = 15;
const D_PBLC: [u8; 2] = [0x80, 0x80];
const D_MESG: [u8; 2] = [0x81, 0x81];
const D_LEAF: [u8; 2] = [0x82, 0x82];
const D_INTR: [u8; 2] = [0x83, 0x83];

/// The height of an LMS type this signer makes.
#[must_use]
pub const fn height(lms_type: u32) -> Option<u32> {
    match lms_type {
        LMS_SHA256_M24_H10 => Some(10),
        LMS_SHA256_M24_H15 => Some(15),
        LMS_SHA256_M24_H20 => Some(20),
        _ => None,
    }
}

/// A private key: its type, `I` and seed. Which leaf is next is the
/// caller's to track.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Key {
    /// Its LMS type.
    pub lms_type: u32,
    /// The tree's identifier `I`.
    pub id: [u8; 16],
    /// The seed every one-time key is derived from.
    pub seed: [u8; N],
}

fn h192(parts: &[&[u8]]) -> [u8; N] {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update(part);
    }
    finish_192(hash)
}

/// The digits of `Q || Cksm(Q)` at `w = 4` (RFC 8554 §4.4).
fn digits(q_digest: &[u8; N]) -> [u8; P] {
    let mut out = [0u8; P];
    let mut sum: u32 = 0;
    for (n, byte) in q_digest.iter().enumerate() {
        let (hi, lo) = (byte >> 4, byte & 0xF);
        if let Some(slot) = out.get_mut(2 * n) {
            *slot = hi;
        }
        if let Some(slot) = out.get_mut(2 * n + 1) {
            *slot = lo;
        }
        sum += u32::from(MAX_DIGIT - hi) + u32::from(MAX_DIGIT - lo);
    }
    let checksum = (sum << 4).to_be_bytes();
    let [_, _, c0, c1] = checksum;
    for (n, digit) in [c0 >> 4, c0 & 0xF, c1 >> 4].into_iter().enumerate() {
        if let Some(slot) = out.get_mut(2 * N + n) {
            *slot = digit;
        }
    }
    out
}

impl Key {
    /// The tree height.
    #[must_use]
    pub fn height(&self) -> u32 {
        height(self.lms_type).unwrap_or(0)
    }

    /// The number of leaves, `2^h`.
    #[must_use]
    pub fn leaves(&self) -> u32 {
        1u32 << self.height()
    }

    /// `x_q[i]` (RFC 8554 Appendix A).
    fn secret(&self, q: u32, i: u16) -> [u8; N] {
        h192(&[
            &self.id,
            &q.to_be_bytes(),
            &i.to_be_bytes(),
            &[0xFF],
            &self.seed,
        ])
    }

    /// The randomiser `C` of leaf `q`, derived as the reference
    /// implementation and RFC 9858's vectors derive it.
    fn randomiser(&self, q: u32) -> [u8; N] {
        h192(&[
            &self.id,
            &q.to_be_bytes(),
            &0xFFFDu16.to_be_bytes(),
            &[0xFF],
            &self.seed,
        ])
    }

    /// Chain `value` of chain `i` from digit `from` to `to`.
    fn chain(&self, q: u32, i: u16, from: u8, to: u8, mut value: [u8; N]) -> [u8; N] {
        for j in from..to {
            value = h192(&[&self.id, &q.to_be_bytes(), &i.to_be_bytes(), &[j], &value]);
        }
        value
    }

    /// The LM-OTS public key `K` of leaf `q`.
    fn ots_public(&self, q: u32) -> [u8; N] {
        let mut hash = Sha256::new();
        hash.update(&self.id);
        hash.update(&q.to_be_bytes());
        hash.update(&D_PBLC);
        for i in 0..P {
            let i = u16::try_from(i).unwrap_or(0);
            let top = self.chain(q, i, 0, MAX_DIGIT, self.secret(q, i));
            hash.update(&top);
        }
        finish_192(hash)
    }

    /// Every node of the Merkle tree, `T[1]` to `T[2^(h+1) - 1]`, indexed
    /// by node number (index 0 unused).
    #[must_use]
    pub fn tree(&self) -> Vec<[u8; N]> {
        let leaves = self.leaves();
        let total = usize::try_from(2 * leaves).unwrap_or(0);
        let mut nodes = vec![[0u8; N]; total];
        for q in 0..leaves {
            let r = leaves + q;
            let leaf = h192(&[&self.id, &r.to_be_bytes(), &D_LEAF, &self.ots_public(q)]);
            if let Some(slot) = nodes.get_mut(usize::try_from(r).unwrap_or(0)) {
                *slot = leaf;
            }
        }
        for r in (1..leaves).rev() {
            let left = nodes
                .get(usize::try_from(2 * r).unwrap_or(0))
                .copied()
                .unwrap_or_default();
            let right = nodes
                .get(usize::try_from(2 * r + 1).unwrap_or(0))
                .copied()
                .unwrap_or_default();
            let node = h192(&[&self.id, &r.to_be_bytes(), &D_INTR, &left, &right]);
            if let Some(slot) = nodes.get_mut(usize::try_from(r).unwrap_or(0)) {
                *slot = node;
            }
        }
        nodes
    }

    /// The LMS public key, given the tree's root `T[1]`.
    #[must_use]
    pub fn public_key(&self, root: &[u8; N]) -> Vec<u8> {
        let mut out = Vec::with_capacity(48);
        out.extend_from_slice(&self.lms_type.to_be_bytes());
        out.extend_from_slice(&LMOTS_SHA256_N24_W4.to_be_bytes());
        out.extend_from_slice(&self.id);
        out.extend_from_slice(root);
        out
    }

    /// The LMS signature of `message` with leaf `q`, given the tree.
    /// The caller guarantees `q` was never used before.
    #[must_use]
    pub fn sign(&self, q: u32, message: &[u8], tree: &[[u8; N]]) -> Vec<u8> {
        let c = self.randomiser(q);
        let q_digest = h192(&[&self.id, &q.to_be_bytes(), &D_MESG, &c, message]);
        let mut out = Vec::new();
        out.extend_from_slice(&q.to_be_bytes());
        out.extend_from_slice(&LMOTS_SHA256_N24_W4.to_be_bytes());
        out.extend_from_slice(&c);
        for (i, digit) in digits(&q_digest).into_iter().enumerate() {
            let i = u16::try_from(i).unwrap_or(0);
            out.extend_from_slice(&self.chain(q, i, 0, digit, self.secret(q, i)));
        }
        out.extend_from_slice(&self.lms_type.to_be_bytes());
        let mut node = self.leaves() + q;
        while node > 1 {
            let sibling = tree
                .get(usize::try_from(node ^ 1).unwrap_or(0))
                .copied()
                .unwrap_or_default();
            out.extend_from_slice(&sibling);
            node >>= 1;
        }
        out
    }
}
