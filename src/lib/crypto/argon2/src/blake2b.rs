//! `BLAKE2b`, RFC 7693: the hash Argon2 is built on.
//!
//! Unkeyed, with any output length from 1 to 64 bytes, fed in pieces. Argon2
//! needs no key, no salt and no personalisation, so neither are here. It
//! needs a variable output length, both to hash its inputs into the 64-byte
//! `H0` and for `H'`, which chains 64-byte digests into one of any length
//! (RFC 9106 §3.3).

/// The most bytes one digest has.
pub const MAX_OUT: usize = 64;

/// Bytes in one message block.
const BLOCK: usize = 128;

/// The initialisation vector: SHA-512's, RFC 7693 §2.6.
const IV: [u64; 8] = [
    0x6a09_e667_f3bc_c908,
    0xbb67_ae85_84ca_a73b,
    0x3c6e_f372_fe94_f82b,
    0xa54f_f53a_5f1d_36f1,
    0x510e_527f_ade6_82d1,
    0x9b05_688c_2b3e_6c1f,
    0x1f83_d9ab_fb41_bd6b,
    0x5be0_cd19_137e_2179,
];

/// The message schedule, RFC 7693 §2.7. Rounds 10 and 11 use rows 0 and 1
/// again.
const SIGMA: [[u8; 16]; 12] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
    [11, 8, 12, 0, 5, 2, 15, 13, 10, 14, 3, 6, 7, 1, 9, 4],
    [7, 9, 3, 1, 13, 12, 11, 14, 2, 6, 5, 10, 4, 0, 15, 8],
    [9, 0, 5, 7, 2, 4, 10, 15, 14, 1, 11, 12, 6, 8, 3, 13],
    [2, 12, 6, 10, 0, 11, 8, 3, 4, 13, 7, 5, 15, 14, 1, 9],
    [12, 5, 1, 15, 14, 13, 4, 10, 0, 7, 6, 3, 9, 2, 8, 11],
    [13, 11, 7, 14, 12, 1, 3, 9, 5, 0, 15, 4, 8, 6, 2, 10],
    [6, 15, 14, 9, 11, 3, 0, 8, 12, 2, 13, 7, 1, 4, 10, 5],
    [10, 2, 8, 4, 7, 6, 1, 5, 15, 11, 9, 14, 3, 12, 13, 0],
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
];

/// The mixing function `G`, RFC 7693 §3.1.
#[expect(
    clippy::indexing_slicing,
    reason = "AUDIT: every caller passes constant indices below 16 into a [u64; 16]"
)]
#[expect(
    clippy::many_single_char_names,
    reason = "RFC 7693 §3.1's names, so the function can be read against it"
)]
fn mix(v: &mut [u64; 16], a: usize, b: usize, c: usize, d: usize, x: u64, y: u64) {
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(x);
    v[d] = (v[d] ^ v[a]).rotate_right(32);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(24);
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(y);
    v[d] = (v[d] ^ v[a]).rotate_right(16);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(63);
}

/// The compression function `F`, RFC 7693 §3.2, over one 128-byte block.
/// `counter` is the bytes hashed so far including this block's, and `last`
/// marks the final block.
#[expect(
    clippy::indexing_slicing,
    reason = "AUDIT: `v` and `m` are [u64; 16] and every index is a SIGMA entry, all below 16, or a constant below 16"
)]
fn compress(h: &mut [u64; 8], block: &[u8; BLOCK], counter: u128, last: bool) {
    let mut m = [0_u64; 16];
    for (word, bytes) in m.iter_mut().zip(block.as_chunks::<8>().0) {
        *word = u64::from_le_bytes(*bytes);
    }
    let mut v = [0_u64; 16];
    v[..8].copy_from_slice(h);
    v[8..].copy_from_slice(&IV);
    v[12] ^= counter as u64;
    v[13] ^= (counter >> 64) as u64;
    if last {
        v[14] = !v[14];
    }
    for s in &SIGMA {
        let at = |i: usize| m[usize::from(s[i])];
        mix(&mut v, 0, 4, 8, 12, at(0), at(1));
        mix(&mut v, 1, 5, 9, 13, at(2), at(3));
        mix(&mut v, 2, 6, 10, 14, at(4), at(5));
        mix(&mut v, 3, 7, 11, 15, at(6), at(7));
        mix(&mut v, 0, 5, 10, 15, at(8), at(9));
        mix(&mut v, 1, 6, 11, 12, at(10), at(11));
        mix(&mut v, 2, 7, 8, 13, at(12), at(13));
        mix(&mut v, 3, 4, 9, 14, at(14), at(15));
    }
    for (i, word) in h.iter_mut().enumerate() {
        *word ^= v[i] ^ v[i + 8];
    }
}

/// A `BLAKE2b` digest being computed.
#[derive(Clone)]
pub struct Blake2b {
    h: [u64; 8],
    /// Bytes waiting to be compressed. A full buffer is compressed only when
    /// more bytes arrive, because the last block is compressed differently.
    buffer: [u8; BLOCK],
    filled: usize,
    /// Bytes compressed so far.
    counter: u128,
    out_len: usize,
}

impl core::fmt::Debug for Blake2b {
    /// Its state is derived from what it hashed, which may be a password, so
    /// none of it is shown.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Blake2b")
            .field("out_len", &self.out_len)
            .finish_non_exhaustive()
    }
}

impl Blake2b {
    /// A digest of `out_len` bytes, clamped to 1..=64.
    #[must_use]
    pub fn new(out_len: usize) -> Blake2b {
        let out_len = out_len.clamp(1, MAX_OUT);
        let mut h = IV;
        // Parameter block word 0: digest length, no key, fanout 1, depth 1.
        h[0] ^= 0x0101_0000 ^ out_len as u64;
        Blake2b {
            h,
            buffer: [0; BLOCK],
            filled: 0,
            counter: 0,
            out_len,
        }
    }

    /// Hash `data` after what was hashed before.
    pub fn update(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            if self.filled == BLOCK {
                self.counter += BLOCK as u128;
                compress(&mut self.h, &self.buffer, self.counter, false);
                self.filled = 0;
            }
            let room = BLOCK - self.filled;
            let take = room.min(data.len());
            let (now, rest) = data.split_at(take);
            if let Some(slot) = self.buffer.get_mut(self.filled..self.filled + take) {
                slot.copy_from_slice(now);
            }
            self.filled += take;
            data = rest;
        }
    }

    /// Finish, writing the digest's first `out.len()` bytes (at most its
    /// length) into `out`.
    pub fn finalize(mut self, out: &mut [u8]) {
        self.counter += self.filled as u128;
        if let Some(tail) = self.buffer.get_mut(self.filled..) {
            tail.fill(0);
        }
        compress(&mut self.h, &self.buffer, self.counter, true);
        let mut bytes = [0_u8; MAX_OUT];
        for (chunk, word) in bytes.as_chunks_mut::<8>().0.iter_mut().zip(self.h) {
            *chunk = word.to_le_bytes();
        }
        let n = out.len().min(self.out_len);
        if let (Some(to), Some(from)) = (out.get_mut(..n), bytes.get(..n)) {
            to.copy_from_slice(from);
        }
        bytes.fill(0);
        self.buffer.fill(0);
        self.h.fill(0);
        let _ = core::hint::black_box(&bytes);
        let _ = core::hint::black_box(&self);
    }
}

/// The digest of `data`, `out.len()` bytes long (1 to 64).
pub fn digest(data: &[u8], out: &mut [u8]) {
    let mut hasher = Blake2b::new(out.len());
    hasher.update(data);
    hasher.finalize(out);
}
