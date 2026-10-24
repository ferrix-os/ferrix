//! SHA-256 (FIPS 180-4), and SHA-256/192, its first 24 bytes (RFC 9858
//! §2.1, FIPS 180-4 §7).
//!
//! The one SHA-256 the product signing verifier, `ferrix-sigverify`, is
//! built on (`docs/SIGNING.md` §5.1). It was `ferrix-slots`' own; that crate
//! moves onto this one in slice S4, so that one implementation is qualified
//! once. `no_std`, no `unsafe`, no allocation: a running hash is 112 bytes
//! of state on its caller's stack.

#![no_std]
#![forbid(unsafe_code)]
#![deny(clippy::arithmetic_side_effects)]

#[cfg(test)]
mod tests;

/// The first 32 bits of the fractional parts of the cube roots of the first
/// 64 primes.
const K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

/// The initial hash value.
const H0: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];

/// A running SHA-256.
#[derive(Clone, Debug)]
pub struct Sha256 {
    state: [u32; 8],
    block: [u8; 64],
    filled: usize,
    length: u64,
}

impl Default for Sha256 {
    fn default() -> Sha256 {
        Sha256::new()
    }
}

impl Sha256 {
    /// An empty hash.
    #[must_use]
    pub const fn new() -> Sha256 {
        Sha256 {
            state: H0,
            block: [0; 64],
            filled: 0,
            length: 0,
        }
    }

    /// Add `bytes`.
    pub fn update(&mut self, mut bytes: &[u8]) {
        self.length = self.length.wrapping_add(bytes.len() as u64);
        while !bytes.is_empty() {
            let free = self.block.get_mut(self.filled..).unwrap_or_default();
            let take = free.len().min(bytes.len());
            let (head, rest) = bytes.split_at(take);
            for (slot, byte) in free.iter_mut().zip(head) {
                *slot = *byte;
            }
            // `take` is at most the 64 - `filled` bytes left in the block.
            self.filled = self.filled.saturating_add(take);
            bytes = rest;
            if self.filled >= 64 {
                let block = self.block;
                compress(&mut self.state, &block);
                self.filled = 0;
            }
        }
    }

    /// The digest.
    #[must_use]
    pub fn finish(mut self) -> [u8; 32] {
        let bits = self.length.wrapping_mul(8);
        self.update(&[0x80]);
        while self.filled != 56 {
            self.update(&[0]);
        }
        self.update(&bits.to_be_bytes());
        let mut out = [0u8; 32];
        for (chunk, word) in out.chunks_exact_mut(4).zip(self.state) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

/// The digest of `bytes`.
#[must_use]
pub fn digest(bytes: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(bytes);
    hash.finish()
}

/// SHA-256/192 of what `hash` was given: its digest without the final 64
/// bits (RFC 9858 §2.1), with SHA-256's own initial value.
#[must_use]
pub fn finish_192(hash: Sha256) -> [u8; 24] {
    let full = hash.finish();
    let mut out = [0u8; 24];
    for (o, f) in out.iter_mut().zip(full) {
        *o = f;
    }
    out
}

/// The SHA-256/192 digest of `bytes`.
#[must_use]
pub fn digest_192(bytes: &[u8]) -> [u8; 24] {
    let mut hash = Sha256::new();
    hash.update(bytes);
    finish_192(hash)
}

/// One 64-byte block into the state. The message schedule is kept as the
/// last 16 words, shifted one place a round, so that every index is a
/// constant.
fn compress(state: &mut [u32; 8], block: &[u8; 64]) {
    let mut w = [0u32; 16];
    for (word, bytes) in w.iter_mut().zip(block.chunks_exact(4)) {
        *word = bytes.iter().fold(0u32, |acc, &b| (acc << 8) | u32::from(b));
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for (round, k) in K.iter().enumerate() {
        // From round 16 on, the next schedule word from the last 16:
        // w[t] = s1(w[t-2]) + w[t-7] + s0(w[t-15]) + w[t-16].
        let wt = if round >= 16 {
            let [w0, w1, .., w9, _, _, _, _, w14, _] = w;
            let s0 = w1.rotate_right(7) ^ w1.rotate_right(18) ^ (w1 >> 3);
            let s1 = w14.rotate_right(17) ^ w14.rotate_right(19) ^ (w14 >> 10);
            w.rotate_left(1);
            let next = w0.wrapping_add(s0).wrapping_add(w9).wrapping_add(s1);
            let [.., last] = &mut w;
            *last = next;
            next
        } else {
            w.get(round).copied().unwrap_or(0)
        };
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ (!e & g);
        let t1 = h
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(*k)
            .wrapping_add(wt);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(maj);
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }
    for (word, add) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
        *word = word.wrapping_add(add);
    }
}
