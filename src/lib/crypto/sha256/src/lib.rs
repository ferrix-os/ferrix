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

    /// Add `bytes`. A partial block first fills the buffer; whole blocks
    /// are then compressed straight from `bytes`, and what is left is kept.
    pub fn update(&mut self, mut bytes: &[u8]) {
        self.length = self.length.wrapping_add(bytes.len() as u64);
        if self.filled != 0 {
            let free = self.block.get_mut(self.filled..).unwrap_or_default();
            let take = free.len().min(bytes.len());
            let (head, rest) = bytes.split_at(take);
            for (slot, byte) in free.iter_mut().zip(head) {
                *slot = *byte;
            }
            // `take` is at most the 64 - `filled` bytes left in the block.
            self.filled = self.filled.saturating_add(take);
            bytes = rest;
            if self.filled < 64 {
                return;
            }
            let block = self.block;
            compress(&mut self.state, &block);
            self.filled = 0;
        }
        let (blocks, rest) = bytes.as_chunks::<64>();
        for block in blocks {
            compress(&mut self.state, block);
        }
        for (slot, byte) in self.block.iter_mut().zip(rest) {
            *slot = *byte;
        }
        self.filled = rest.len();
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

/// One round of FIPS 180-4 §6.2.2 step 3, with the working variables
/// renamed rather than moved: the caller passes them rotated by one each
/// round, so that only `d` and `h` are written.
macro_rules! round {
    ($a:ident, $b:ident, $c:ident, $d:ident, $e:ident, $f:ident, $g:ident, $h:ident, $w:expr, $k:expr) => {{
        let t1 = $h
            .wrapping_add($e.rotate_right(6) ^ $e.rotate_right(11) ^ $e.rotate_right(25))
            .wrapping_add(($e & $f) ^ (!$e & $g))
            .wrapping_add($k)
            .wrapping_add($w);
        let t2 = ($a.rotate_right(2) ^ $a.rotate_right(13) ^ $a.rotate_right(22))
            .wrapping_add(($a & $b) ^ ($a & $c) ^ ($b & $c));
        $d = $d.wrapping_add(t1);
        $h = t1.wrapping_add(t2);
    }};
}

/// Schedule word `t` (FIPS 180-4 §6.2.2 step 1) into the 16-word ring, in
/// place of word `t - 16`: `w[t] = s1(w[t-2]) + w[t-7] + s0(w[t-15]) +
/// w[t-16]`, every index a literal (`t mod 16` and its neighbours').
macro_rules! sched {
    ($w:ident, $i:literal, $i2:literal, $i7:literal, $i15:literal) => {{
        let s0 = $w[$i15].rotate_right(7) ^ $w[$i15].rotate_right(18) ^ ($w[$i15] >> 3);
        let s1 = $w[$i2].rotate_right(17) ^ $w[$i2].rotate_right(19) ^ ($w[$i2] >> 10);
        $w[$i] = $w[$i]
            .wrapping_add(s0)
            .wrapping_add($w[$i7])
            .wrapping_add(s1);
        $w[$i]
    }};
}

/// Rounds 0 to 31 of [`compress`]: the working variables come back in order, since 32 is a multiple of 8.
#[inline(always)]
fn rounds_0_to_31(v: &mut [u32; 8], w: &mut [u32; 16]) {
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *v;
    round!(a, b, c, d, e, f, g, h, w[0], K[0]);
    round!(h, a, b, c, d, e, f, g, w[1], K[1]);
    round!(g, h, a, b, c, d, e, f, w[2], K[2]);
    round!(f, g, h, a, b, c, d, e, w[3], K[3]);
    round!(e, f, g, h, a, b, c, d, w[4], K[4]);
    round!(d, e, f, g, h, a, b, c, w[5], K[5]);
    round!(c, d, e, f, g, h, a, b, w[6], K[6]);
    round!(b, c, d, e, f, g, h, a, w[7], K[7]);
    round!(a, b, c, d, e, f, g, h, w[8], K[8]);
    round!(h, a, b, c, d, e, f, g, w[9], K[9]);
    round!(g, h, a, b, c, d, e, f, w[10], K[10]);
    round!(f, g, h, a, b, c, d, e, w[11], K[11]);
    round!(e, f, g, h, a, b, c, d, w[12], K[12]);
    round!(d, e, f, g, h, a, b, c, w[13], K[13]);
    round!(c, d, e, f, g, h, a, b, w[14], K[14]);
    round!(b, c, d, e, f, g, h, a, w[15], K[15]);
    round!(a, b, c, d, e, f, g, h, sched!(w, 0, 14, 9, 1), K[16]);
    round!(h, a, b, c, d, e, f, g, sched!(w, 1, 15, 10, 2), K[17]);
    round!(g, h, a, b, c, d, e, f, sched!(w, 2, 0, 11, 3), K[18]);
    round!(f, g, h, a, b, c, d, e, sched!(w, 3, 1, 12, 4), K[19]);
    round!(e, f, g, h, a, b, c, d, sched!(w, 4, 2, 13, 5), K[20]);
    round!(d, e, f, g, h, a, b, c, sched!(w, 5, 3, 14, 6), K[21]);
    round!(c, d, e, f, g, h, a, b, sched!(w, 6, 4, 15, 7), K[22]);
    round!(b, c, d, e, f, g, h, a, sched!(w, 7, 5, 0, 8), K[23]);
    round!(a, b, c, d, e, f, g, h, sched!(w, 8, 6, 1, 9), K[24]);
    round!(h, a, b, c, d, e, f, g, sched!(w, 9, 7, 2, 10), K[25]);
    round!(g, h, a, b, c, d, e, f, sched!(w, 10, 8, 3, 11), K[26]);
    round!(f, g, h, a, b, c, d, e, sched!(w, 11, 9, 4, 12), K[27]);
    round!(e, f, g, h, a, b, c, d, sched!(w, 12, 10, 5, 13), K[28]);
    round!(d, e, f, g, h, a, b, c, sched!(w, 13, 11, 6, 14), K[29]);
    round!(c, d, e, f, g, h, a, b, sched!(w, 14, 12, 7, 15), K[30]);
    round!(b, c, d, e, f, g, h, a, sched!(w, 15, 13, 8, 0), K[31]);
    *v = [a, b, c, d, e, f, g, h];
}

/// Rounds 32 to 63 of [`compress`].
#[inline(always)]
fn rounds_32_to_63(v: &mut [u32; 8], w: &mut [u32; 16]) {
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *v;
    round!(a, b, c, d, e, f, g, h, sched!(w, 0, 14, 9, 1), K[32]);
    round!(h, a, b, c, d, e, f, g, sched!(w, 1, 15, 10, 2), K[33]);
    round!(g, h, a, b, c, d, e, f, sched!(w, 2, 0, 11, 3), K[34]);
    round!(f, g, h, a, b, c, d, e, sched!(w, 3, 1, 12, 4), K[35]);
    round!(e, f, g, h, a, b, c, d, sched!(w, 4, 2, 13, 5), K[36]);
    round!(d, e, f, g, h, a, b, c, sched!(w, 5, 3, 14, 6), K[37]);
    round!(c, d, e, f, g, h, a, b, sched!(w, 6, 4, 15, 7), K[38]);
    round!(b, c, d, e, f, g, h, a, sched!(w, 7, 5, 0, 8), K[39]);
    round!(a, b, c, d, e, f, g, h, sched!(w, 8, 6, 1, 9), K[40]);
    round!(h, a, b, c, d, e, f, g, sched!(w, 9, 7, 2, 10), K[41]);
    round!(g, h, a, b, c, d, e, f, sched!(w, 10, 8, 3, 11), K[42]);
    round!(f, g, h, a, b, c, d, e, sched!(w, 11, 9, 4, 12), K[43]);
    round!(e, f, g, h, a, b, c, d, sched!(w, 12, 10, 5, 13), K[44]);
    round!(d, e, f, g, h, a, b, c, sched!(w, 13, 11, 6, 14), K[45]);
    round!(c, d, e, f, g, h, a, b, sched!(w, 14, 12, 7, 15), K[46]);
    round!(b, c, d, e, f, g, h, a, sched!(w, 15, 13, 8, 0), K[47]);
    round!(a, b, c, d, e, f, g, h, sched!(w, 0, 14, 9, 1), K[48]);
    round!(h, a, b, c, d, e, f, g, sched!(w, 1, 15, 10, 2), K[49]);
    round!(g, h, a, b, c, d, e, f, sched!(w, 2, 0, 11, 3), K[50]);
    round!(f, g, h, a, b, c, d, e, sched!(w, 3, 1, 12, 4), K[51]);
    round!(e, f, g, h, a, b, c, d, sched!(w, 4, 2, 13, 5), K[52]);
    round!(d, e, f, g, h, a, b, c, sched!(w, 5, 3, 14, 6), K[53]);
    round!(c, d, e, f, g, h, a, b, sched!(w, 6, 4, 15, 7), K[54]);
    round!(b, c, d, e, f, g, h, a, sched!(w, 7, 5, 0, 8), K[55]);
    round!(a, b, c, d, e, f, g, h, sched!(w, 8, 6, 1, 9), K[56]);
    round!(h, a, b, c, d, e, f, g, sched!(w, 9, 7, 2, 10), K[57]);
    round!(g, h, a, b, c, d, e, f, sched!(w, 10, 8, 3, 11), K[58]);
    round!(f, g, h, a, b, c, d, e, sched!(w, 11, 9, 4, 12), K[59]);
    round!(e, f, g, h, a, b, c, d, sched!(w, 12, 10, 5, 13), K[60]);
    round!(d, e, f, g, h, a, b, c, sched!(w, 13, 11, 6, 14), K[61]);
    round!(c, d, e, f, g, h, a, b, sched!(w, 14, 12, 7, 15), K[62]);
    round!(b, c, d, e, f, g, h, a, sched!(w, 15, 13, 8, 0), K[63]);
    *v = [a, b, c, d, e, f, g, h];
}

/// One 64-byte block into the state. The 64 rounds are
/// written out, the message schedule kept as a ring of 16 words: every
/// index is a literal, so nothing is bounds-checked, nothing is computed
/// for an index at run time, and no word is moved but the one replaced
/// (the measured cost on the DK1's Cortex-A7, 44.4 cycles per byte against
/// 127.7 for the version this replaces: `docs/certification/
/// MEMORY-AND-TIMING.md` §2.2z).
fn compress(state: &mut [u32; 8], block: &[u8; 64]) {
    let mut w = [0u32; 16];
    for (word, bytes) in w.iter_mut().zip(block.chunks_exact(4)) {
        *word = bytes.iter().fold(0u32, |acc, &b| (acc << 8) | u32::from(b));
    }
    let mut v = *state;
    rounds_0_to_31(&mut v, &mut w);
    rounds_32_to_63(&mut v, &mut w);
    for (word, add) in state.iter_mut().zip(v) {
        *word = word.wrapping_add(add);
    }
}
