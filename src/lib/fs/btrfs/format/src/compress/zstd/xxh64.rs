//! XXH64, the hash behind a zstd frame's content checksum.
//!
//! A frame with the checksum flag set ends in the low 32 bits of
//! `XXH64(content, seed 0)`. It is twenty lines of multiply-and-rotate, which
//! is less than the audit a dependency would need.

const PRIME_1: u64 = 0x9E37_79B1_85EB_CA87;
const PRIME_2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const PRIME_3: u64 = 0x1656_67B1_9E37_79F9;
const PRIME_4: u64 = 0x85EB_CA77_C2B2_AE63;
const PRIME_5: u64 = 0x27D4_EB2F_1656_67C5;

/// XXH64 of `input` with `seed`.
pub(super) fn xxh64(input: &[u8], seed: u64) -> u64 {
    let (stripes, tail) = input.as_chunks::<32>();
    let mut hash = if input.len() >= 32 {
        let mut lanes = [
            seed.wrapping_add(PRIME_1).wrapping_add(PRIME_2),
            seed.wrapping_add(PRIME_2),
            seed,
            seed.wrapping_sub(PRIME_1),
        ];
        for stripe in stripes {
            for (lane, word) in lanes.iter_mut().zip(stripe.as_chunks::<8>().0) {
                *lane = round(*lane, u64::from_le_bytes(*word));
            }
        }
        let [a, b, c, d] = lanes;
        let mut hash = a
            .rotate_left(1)
            .wrapping_add(b.rotate_left(7))
            .wrapping_add(c.rotate_left(12))
            .wrapping_add(d.rotate_left(18));
        for lane in lanes {
            hash = (hash ^ round(0, lane))
                .wrapping_mul(PRIME_1)
                .wrapping_add(PRIME_4);
        }
        hash
    } else {
        seed.wrapping_add(PRIME_5)
    };
    // `usize` is at most 64 bits on every target Rust supports.
    hash = hash.wrapping_add(u64::try_from(input.len()).unwrap_or(u64::MAX));
    finish(hash, tail)
}

/// Fold in the fewer than 32 bytes the stripes left over, then avalanche.
fn finish(mut hash: u64, tail: &[u8]) -> u64 {
    let (words, tail) = tail.as_chunks::<8>();
    for word in words {
        hash = (hash ^ round(0, u64::from_le_bytes(*word)))
            .rotate_left(27)
            .wrapping_mul(PRIME_1)
            .wrapping_add(PRIME_4);
    }
    let (halves, tail) = tail.as_chunks::<4>();
    for half in halves {
        let value = u32::from_le_bytes(*half);
        hash = (hash ^ u64::from(value).wrapping_mul(PRIME_1))
            .rotate_left(23)
            .wrapping_mul(PRIME_2)
            .wrapping_add(PRIME_3);
    }
    for &byte in tail {
        hash = (hash ^ u64::from(byte).wrapping_mul(PRIME_5))
            .rotate_left(11)
            .wrapping_mul(PRIME_1);
    }
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(PRIME_2);
    hash ^= hash >> 29;
    hash = hash.wrapping_mul(PRIME_3);
    hash ^ (hash >> 32)
}

/// One lane update.
fn round(lane: u64, input: u64) -> u64 {
    lane.wrapping_add(input.wrapping_mul(PRIME_2))
        .rotate_left(31)
        .wrapping_mul(PRIME_1)
}
