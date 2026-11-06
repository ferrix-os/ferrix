//! The kernel's random number generator: `ChaCha20`, used with fast key
//! erasure.
//!
//! # The generator
//!
//! [`Crng`] holds a 256-bit key and a block counter. Every 64-byte block it
//! computes replaces the key with its first 32 bytes and hands out the other
//! 32. A key read out of memory therefore says nothing about the bytes already
//! handed out: they came from blocks under keys that no longer exist. This is
//! the construction Linux's `crng` and OpenBSD's `arc4random` use, and the
//! reason for it is backtracking resistance, not speed.
//!
//! # Seeding
//!
//! Where the bytes come from is the kernel's business: firmware's random
//! number protocol, a CPU's random instruction, the jitter between two reads
//! of a counter. [`Crng::mix`] folds any of them into the key and then
//! replaces the key with a block computed under it, so each input is spread
//! over the whole key. An input nobody can predict makes the key
//! unpredictable, whatever else was mixed in with it. An input somebody can
//! predict adds nothing, and costs nothing either. [`Crng::credit`] records
//! how many bits of the inputs the caller vouches for, and [`Crng::seeded`]
//! answers whether that reached 256.
//!
//! # Checked against
//!
//! The block function against RFC 8439's own test vector, and against
//! OpenSSL's `chacha20` keystream, including the counter carrying from its
//! low word into its high one.

#![no_std]
#![forbid(unsafe_code)]

/// Bytes in a key.
pub const KEY_BYTES: usize = 32;

/// Bytes in one `ChaCha20` block.
pub const BLOCK_BYTES: usize = 64;

/// Bits of entropy [`Crng::seeded`] waits for.
pub const SEEDED_BITS: u32 = 256;

/// "expand 32-byte k", the four constant words.
const SIGMA: [u32; 4] = [0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574];

/// One `ChaCha20` quarter round on four words of `s`.
#[expect(
    clippy::indexing_slicing,
    reason = "AUDIT: every caller passes constant indices below 16 into a [u32; 16]"
)]
fn quarter_round(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(16);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(12);
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(8);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(7);
}

/// The `ChaCha20` block under `key` at the 64-bit block `counter`, with the
/// 64-bit `nonce`: Bernstein's original layout, words 12 and 13 the counter
/// and 14 and 15 the nonce. RFC 8439's 32-bit counter and 96-bit nonce are the
/// same twelve words split differently.
#[must_use]
pub fn block(key: &[u8; KEY_BYTES], counter: u64, nonce: u64) -> [u8; BLOCK_BYTES] {
    let mut state = [0_u32; 16];
    state[..4].copy_from_slice(&SIGMA);
    for (word, bytes) in state[4..12].iter_mut().zip(key.as_chunks::<4>().0) {
        *word = u32::from_le_bytes(*bytes);
    }
    state[12] = counter as u32;
    state[13] = (counter >> 32) as u32;
    state[14] = nonce as u32;
    state[15] = (nonce >> 32) as u32;

    let mut working = state;
    for _ in 0..10 {
        quarter_round(&mut working, 0, 4, 8, 12);
        quarter_round(&mut working, 1, 5, 9, 13);
        quarter_round(&mut working, 2, 6, 10, 14);
        quarter_round(&mut working, 3, 7, 11, 15);
        quarter_round(&mut working, 0, 5, 10, 15);
        quarter_round(&mut working, 1, 6, 11, 12);
        quarter_round(&mut working, 2, 7, 8, 13);
        quarter_round(&mut working, 3, 4, 9, 14);
    }

    let mut out = [0_u8; BLOCK_BYTES];
    for ((bytes, word), initial) in out
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(working)
        .zip(state)
    {
        bytes.copy_from_slice(&word.wrapping_add(initial).to_le_bytes());
    }
    out
}

/// A `ChaCha20` generator with fast key erasure. See the module documentation.
pub struct Crng {
    key: [u8; KEY_BYTES],
    counter: u64,
    credited: u32,
}

impl core::fmt::Debug for Crng {
    /// Never the key: a debug print is a log line.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Crng")
            .field("counter", &self.counter)
            .field("credited", &self.credited)
            .finish_non_exhaustive()
    }
}

impl Default for Crng {
    fn default() -> Self {
        Self::new()
    }
}

impl Crng {
    /// A generator with an all-zero key and no entropy credited. Its output
    /// is a fixed sequence until something unpredictable is mixed in.
    #[must_use]
    pub const fn new() -> Crng {
        Crng {
            key: [0; KEY_BYTES],
            counter: 0,
            credited: 0,
        }
    }

    /// Fold `input` into the key and replace the key with a block computed
    /// under the result.
    ///
    /// `input` is folded by XOR into the key cyclically, 32 bytes at a time, with a
    /// fresh key between rounds, so an input longer than a key is not simply
    /// cancelled out by its own repetition.
    pub fn mix(&mut self, input: &[u8]) {
        for chunk in input.chunks(KEY_BYTES) {
            for (key, byte) in self.key.iter_mut().zip(chunk) {
                *key ^= *byte;
            }
            let _ = self.rekey();
        }
        if input.is_empty() {
            let _ = self.rekey();
        }
    }

    /// Record that the inputs mixed so far carried `bits` of entropy, as the
    /// caller judges its source.
    pub fn credit(&mut self, bits: u32) {
        self.credited = self.credited.saturating_add(bits);
    }

    /// Bits of entropy credited so far.
    #[must_use]
    pub const fn credited(&self) -> u32 {
        self.credited
    }

    /// Whether at least [`SEEDED_BITS`] have been credited.
    #[must_use]
    pub const fn seeded(&self) -> bool {
        self.credited >= SEEDED_BITS
    }

    /// Replace the key with the first half of the next block, and return the
    /// block's second half.
    fn rekey(&mut self) -> [u8; KEY_BYTES] {
        let out = block(&self.key, self.counter, 0);
        self.counter = self.counter.wrapping_add(1);
        let (key, rest) = out.split_at(KEY_BYTES);
        self.key.copy_from_slice(key);
        let mut half = [0_u8; KEY_BYTES];
        half.copy_from_slice(rest);
        half
    }

    /// Fill `out` with output, one block per 32 bytes, the key replaced by
    /// every block.
    pub fn fill(&mut self, out: &mut [u8]) {
        for chunk in out.chunks_mut(KEY_BYTES) {
            let half = self.rekey();
            chunk.copy_from_slice(half.get(..chunk.len()).unwrap_or(&[]));
        }
    }
}

#[cfg(test)]
mod tests;
