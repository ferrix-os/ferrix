//! Argon2id, RFC 9106: the hash `authd` keeps passwords under
//! (`docs/AUTH.md` §5.1).
//!
//! # What is here
//!
//! * [`blake2b`], RFC 7693, which Argon2 is built on.
//! * [`hash`], Argon2id version 1.3 (`0x13`) over memory the caller gives
//!   it: the only type and version `authd` writes, so the only ones here.
//!   Argon2d and Argon2i would be a line each, and nothing asks for them.
//! * [`phc`], the `$argon2id$v=19$m=…,t=…,p=…$salt$hash` string a hash is
//!   stored as, and [`phc::Encoded::verify`], which checks a password
//!   against one in constant time.
//!
//! # Memory
//!
//! Argon2's cost is the memory it fills: `m` KiB, one [`Block`] a KiB. The
//! crate allocates nothing. The caller hands [`hash`] at least
//! [`Params::blocks`] blocks, so a program that must not allocate on its
//! check path can hold them for good, and `authd` can bound them with its
//! cgroup. Every block is zeroed before [`hash`] returns, whatever it
//! returns, since the blocks are all derived from the password.
//!
//! # Checked against
//!
//! RFC 9106 §5.3's Argon2id vector (secret and associated data, four
//! lanes), and further vectors from the `RustCrypto` `argon2` crate 0.5.3 run
//! on the host: one lane and several, an output longer than 64 bytes (which
//! takes `H'`'s long path), an empty password, memory that is not a
//! multiple of four lanes, and the floor parameters `authd` uses (19 MiB,
//! two passes). `BLAKE2b` against RFC 7693's "abc" and Python's `hashlib`.

#![no_std]
#![forbid(unsafe_code)]

pub mod blake2b;
pub mod phc;

#[cfg(test)]
mod tests;

use blake2b::Blake2b;

/// The Argon2 version implemented: 1.3.
pub const VERSION: u32 = 0x13;

/// Argon2id's type number, `y` in RFC 9106 §3.2.
const TYPE_ID: u32 = 2;

/// 64-bit words in a block.
pub const BLOCK_WORDS: usize = 128;

/// Bytes in a block.
pub const BLOCK_BYTES: usize = 1024;

/// Slices a pass is cut into: `SL` in RFC 9106 §3.4.
const SLICES: usize = 4;

/// Addresses one address block holds (RFC 9106 §3.4.1.2).
const ADDRESSES: usize = 128;

/// The most lanes RFC 9106 allows: `2^24 - 1`.
pub const MAX_LANES: u32 = 0x00FF_FFFF;

/// The shortest salt RFC 9106 allows, and the shortest accepted here.
pub const MIN_SALT: usize = 8;

/// The shortest tag RFC 9106 allows.
pub const MIN_OUT: usize = 4;

/// One 1 KiB block of Argon2's memory.
#[derive(Clone)]
pub struct Block(pub [u64; BLOCK_WORDS]);

impl Block {
    /// A block of zeros.
    pub const ZERO: Block = Block([0; BLOCK_WORDS]);

    /// `self ^= other`, word by word.
    fn xor(&mut self, other: &Block) {
        for (word, with) in self.0.iter_mut().zip(other.0.iter()) {
            *word ^= with;
        }
    }

    /// The block as the 1024 bytes RFC 9106 hashes: little-endian words.
    fn to_bytes(&self) -> [u8; BLOCK_BYTES] {
        let mut bytes = [0_u8; BLOCK_BYTES];
        for (chunk, word) in bytes.as_chunks_mut::<8>().0.iter_mut().zip(self.0.iter()) {
            *chunk = word.to_le_bytes();
        }
        bytes
    }

    /// The block 1024 bytes stand for.
    fn from_bytes(bytes: &[u8; BLOCK_BYTES]) -> Block {
        let mut block = Block::ZERO;
        for (word, chunk) in block.0.iter_mut().zip(bytes.as_chunks::<8>().0) {
            *word = u64::from_le_bytes(*chunk);
        }
        block
    }
}

impl Default for Block {
    fn default() -> Block {
        Block::ZERO
    }
}

impl core::fmt::Debug for Block {
    /// A block is derived from a password, so its words are never shown.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Block(..)")
    }
}

/// Argon2id's cost: memory, passes and lanes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Params {
    /// `m`: KiB of memory, at least eight per lane.
    pub memory_kib: u32,
    /// `t`: passes over the memory, at least one.
    pub passes: u32,
    /// `p`: lanes, 1 to [`MAX_LANES`].
    pub lanes: u32,
}

impl Params {
    /// The blocks [`hash`] fills, `m'` in RFC 9106 §3.2: `m` rounded down to
    /// a multiple of four blocks a lane.
    ///
    /// # Errors
    ///
    /// [`Error::Params`] for parameters RFC 9106 does not allow.
    pub fn blocks(&self) -> Result<usize, Error> {
        if self.passes == 0
            || self.lanes == 0
            || self.lanes > MAX_LANES
            || u64::from(self.memory_kib) < 8 * u64::from(self.lanes)
        {
            return Err(Error::Params);
        }
        let quarter = SLICES as u64 * u64::from(self.lanes);
        let blocks = (u64::from(self.memory_kib) / quarter) * quarter;
        usize::try_from(blocks).map_err(|_| Error::Params)
    }
}

/// What is hashed.
#[derive(Clone, Copy)]
pub struct Inputs<'a> {
    /// `P`, the password.
    pub password: &'a [u8],
    /// `S`, the salt: at least [`MIN_SALT`] bytes.
    pub salt: &'a [u8],
    /// `K`, a secret key; usually empty.
    pub secret: &'a [u8],
    /// `X`, associated data; usually empty.
    pub associated: &'a [u8],
}

impl core::fmt::Debug for Inputs<'_> {
    /// Only the lengths: the password is a password.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Inputs")
            .field("password", &self.password.len())
            .field("salt", &self.salt.len())
            .field("secret", &self.secret.len())
            .field("associated", &self.associated.len())
            .finish()
    }
}

/// Why a hash could not be computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Memory, passes or lanes outside what RFC 9106 allows.
    Params,
    /// A salt shorter than [`MIN_SALT`].
    Salt,
    /// A tag shorter than [`MIN_OUT`].
    Output,
    /// An input longer than `2^32 - 1` bytes.
    TooLong,
    /// Fewer blocks than [`Params::blocks`] were given.
    Memory,
}

/// Argon2id of `inputs` under `params`, written into all of `out`, using the
/// first [`Params::blocks`] of `memory`. Every block of `memory` it used is
/// zero when it returns.
///
/// # Errors
///
/// [`Error`]; nothing is written to `out` on an error.
pub fn hash(
    params: &Params,
    inputs: &Inputs<'_>,
    memory: &mut [Block],
    out: &mut [u8],
) -> Result<(), Error> {
    let blocks = params.blocks()?;
    if inputs.salt.len() < MIN_SALT {
        return Err(Error::Salt);
    }
    if out.len() < MIN_OUT {
        return Err(Error::Output);
    }
    let memory = memory.get_mut(..blocks).ok_or(Error::Memory)?;
    let mut h0 = initial_hash(params, inputs, out.len())?;
    let result = fill(params, &h0, memory, out);
    h0.fill(0);
    for block in memory.iter_mut() {
        block.0.fill(0);
    }
    let _ = core::hint::black_box(&memory);
    let _ = core::hint::black_box(&h0);
    result
}

/// The four byte little-endian length of `bytes`, which Argon2 prefixes
/// each input with.
fn length(bytes: &[u8]) -> Result<[u8; 4], Error> {
    u32::try_from(bytes.len())
        .map(u32::to_le_bytes)
        .map_err(|_| Error::TooLong)
}

/// `H0`, RFC 9106 §3.2 step 1.
fn initial_hash(params: &Params, inputs: &Inputs<'_>, out_len: usize) -> Result<[u8; 64], Error> {
    let tag = u32::try_from(out_len).map_err(|_| Error::TooLong)?;
    let mut hasher = Blake2b::new(64);
    for word in [
        params.lanes,
        tag,
        params.memory_kib,
        params.passes,
        VERSION,
        TYPE_ID,
    ] {
        hasher.update(&word.to_le_bytes());
    }
    for input in [
        inputs.password,
        inputs.salt,
        inputs.secret,
        inputs.associated,
    ] {
        hasher.update(&length(input)?);
        hasher.update(input);
    }
    let mut h0 = [0_u8; 64];
    hasher.finalize(&mut h0);
    Ok(h0)
}

/// `H'`, RFC 9106 §3.3: `BLAKE2b` stretched to `out.len()` bytes, over the
/// concatenation of `parts`.
fn variable_hash(out: &mut [u8], parts: &[&[u8]]) -> Result<(), Error> {
    let total = length(out)?;
    if out.len() <= 64 {
        let mut hasher = Blake2b::new(out.len());
        hasher.update(&total);
        for part in parts {
            hasher.update(part);
        }
        hasher.finalize(out);
        return Ok(());
    }
    let mut v = [0_u8; 64];
    let mut hasher = Blake2b::new(64);
    hasher.update(&total);
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize(&mut v);
    // Each V_i but the last gives its first 32 bytes; the last is the whole
    // of a digest, 33 to 64 bytes long, of the V_i before it.
    let mut rest = &mut *out;
    loop {
        let (head, tail) = rest.split_at_mut(32);
        head.copy_from_slice(v.get(..32).unwrap_or(&[0; 32]));
        rest = tail;
        let previous = v;
        if rest.len() <= 64 {
            let mut last = Blake2b::new(rest.len());
            last.update(&previous);
            last.finalize(rest);
            v.fill(0);
            return Ok(());
        }
        let mut next = Blake2b::new(64);
        next.update(&previous);
        next.finalize(&mut v);
    }
}

/// Steps 2 to 7 of RFC 9106 §3.2 over `memory`, which is exactly
/// [`Params::blocks`] long.
fn fill(params: &Params, h0: &[u8; 64], memory: &mut [Block], out: &mut [u8]) -> Result<(), Error> {
    let lanes = params.lanes as usize;
    let lane_len = memory.len() / lanes;
    let geometry = Geometry {
        lanes,
        lane_len,
        segment: lane_len / SLICES,
        blocks: memory.len() as u64,
        passes: u64::from(params.passes),
    };
    let mut bytes = [0_u8; BLOCK_BYTES];
    for lane in 0..lanes {
        let lane_word = u32::try_from(lane)
            .map_err(|_| Error::Params)?
            .to_le_bytes();
        for column in 0_u32..2 {
            variable_hash(&mut bytes, &[h0, &column.to_le_bytes(), &lane_word])?;
            let at = lane * lane_len + column as usize;
            *memory.get_mut(at).ok_or(Error::Memory)? = Block::from_bytes(&bytes);
        }
    }
    bytes.fill(0);
    for pass in 0..geometry.passes {
        for slice in 0..SLICES {
            for lane in 0..lanes {
                fill_segment(&geometry, memory, Position { pass, slice, lane })?;
            }
        }
    }
    let mut last = memory.get(lane_len - 1).ok_or(Error::Memory)?.clone();
    for lane in 1..lanes {
        last.xor(
            memory
                .get(lane * lane_len + lane_len - 1)
                .ok_or(Error::Memory)?,
        );
    }
    let mut final_bytes = last.to_bytes();
    let written = variable_hash(out, &[&final_bytes]);
    final_bytes.fill(0);
    last.0.fill(0);
    let _ = core::hint::black_box(&final_bytes);
    written
}

/// The shape of the memory, fixed for one hash.
#[derive(Debug, Clone, Copy)]
struct Geometry {
    lanes: usize,
    lane_len: usize,
    segment: usize,
    /// `m'`, as the address blocks name it.
    blocks: u64,
    passes: u64,
}

/// Which segment is being filled.
#[derive(Debug, Clone, Copy)]
struct Position {
    pass: u64,
    slice: usize,
    lane: usize,
}

/// Fill one segment, RFC 9106 §3.4 with the reference implementation's
/// order: each block is `G` of the one before it and one chosen by
/// [`reference`], and from the second pass on it is also combined by XOR with what
/// the block held.
fn fill_segment(geometry: &Geometry, memory: &mut [Block], at: Position) -> Result<(), Error> {
    // Argon2id: the first half of the first pass chooses its references
    // from a counter, so their order says nothing of the password; the rest
    // chooses from the memory itself.
    let independent = at.pass == 0 && at.slice < SLICES / 2;
    let mut input = Block::ZERO;
    let mut addresses = Block::ZERO;
    if independent {
        let words = [
            at.pass,
            at.lane as u64,
            at.slice as u64,
            geometry.blocks,
            geometry.passes,
            u64::from(TYPE_ID),
        ];
        for (word, value) in input.0.iter_mut().zip(words) {
            *word = value;
        }
    }
    let start = if at.pass == 0 && at.slice == 0 { 2 } else { 0 };
    if independent && start != 0 {
        next_addresses(&mut addresses, &mut input);
    }
    let lane_len = geometry.lane_len;
    let first = at.lane * lane_len + at.slice * geometry.segment + start;
    // The block before the first: the lane's last when the segment starts a
    // lane (every pass after the first), else the one just before.
    let mut previous = if first.is_multiple_of(lane_len) {
        first + lane_len - 1
    } else {
        first - 1
    };
    for (current, index) in (first..).zip(start..geometry.segment) {
        let random = if independent {
            if index % ADDRESSES == 0 {
                next_addresses(&mut addresses, &mut input);
            }
            *addresses.0.get(index % ADDRESSES).ok_or(Error::Memory)?
        } else {
            *memory
                .get(previous)
                .and_then(|block| block.0.first())
                .ok_or(Error::Memory)?
        };
        let ref_lane = if at.pass == 0 && at.slice == 0 {
            at.lane
        } else {
            ((random >> 32) % geometry.lanes as u64) as usize
        };
        let ref_index = reference(geometry, at, index, ref_lane == at.lane, random as u32);
        let next = compress(
            memory.get(previous).ok_or(Error::Memory)?,
            memory
                .get(ref_lane * lane_len + ref_index)
                .ok_or(Error::Memory)?,
        );
        let block = memory.get_mut(current).ok_or(Error::Memory)?;
        if at.pass == 0 {
            *block = next;
        } else {
            block.xor(&next);
        }
        previous = current;
    }
    input.0.fill(0);
    addresses.0.fill(0);
    Ok(())
}

/// The next address block: `G(0, G(0, Z))` over the input block `Z`, whose
/// counter word goes up by one first (RFC 9106 §3.4.1.2).
fn next_addresses(addresses: &mut Block, input: &mut Block) {
    if let Some(counter) = input.0.get_mut(6) {
        *counter = counter.wrapping_add(1);
    }
    let zero = Block::ZERO;
    let once = compress(&zero, input);
    *addresses = compress(&zero, &once);
}

/// Which block of the reference lane the block at `index` of the segment
/// refers to, from `J1` (RFC 9106 §3.4.2): somewhere in the blocks already
/// filled that the rules let it see, biased toward the recent ones.
fn reference(geometry: &Geometry, at: Position, index: usize, same_lane: bool, j1: u32) -> usize {
    let segment = geometry.segment as u64;
    let index = index as u64;
    let lane_len = geometry.lane_len as u64;
    let slice = at.slice as u64;
    // The blocks it may refer to: those finished in this lane before it,
    // or, in another lane, those in finished segments only.
    let area = match (at.pass, same_lane) {
        (0, _) if at.slice == 0 => index - 1,
        (0, true) => slice * segment + index - 1,
        (0, false) if index == 0 => slice * segment - 1,
        (0, false) => slice * segment,
        (_, true) => lane_len - segment + index - 1,
        (_, false) if index == 0 => lane_len - segment - 1,
        (_, false) => lane_len - segment,
    };
    let j1 = u64::from(j1);
    let x = (j1 * j1) >> 32;
    let relative = area - 1 - ((area * x) >> 32);
    let start = if at.pass == 0 || at.slice == SLICES - 1 {
        0
    } else {
        (slice + 1) * segment
    };
    ((start + relative) % lane_len) as usize
}

/// Argon2's `GB`: `BLAKE2b`'s `G` with a multiplication added to each
/// addition, which is what makes a block expensive on hardware that adds
/// cheaply (RFC 9106 §3.6).
fn gb(a: u64, b: u64) -> u64 {
    let low = |word: u64| word & 0xFFFF_FFFF;
    a.wrapping_add(b)
        .wrapping_add(2_u64.wrapping_mul(low(a)).wrapping_mul(low(b)))
}

/// The permutation `P` over the sixteen words of `q` that `at` names.
#[expect(
    clippy::indexing_slicing,
    reason = "AUDIT: `q` is a [u64; 128]; `at` holds indices below 128 (ROWS and COLUMNS), and `v` is a [u64; 16] indexed by constants"
)]
fn permute(q: &mut [u64; BLOCK_WORDS], at: &[usize; 16]) {
    let mut v = [0_u64; 16];
    for (word, &i) in v.iter_mut().zip(at) {
        *word = q[i];
    }
    for [a, b, c, d] in [
        [0, 4, 8, 12],
        [1, 5, 9, 13],
        [2, 6, 10, 14],
        [3, 7, 11, 15],
        [0, 5, 10, 15],
        [1, 6, 11, 12],
        [2, 7, 8, 13],
        [3, 4, 9, 14],
    ] {
        v[a] = gb(v[a], v[b]);
        v[d] = (v[d] ^ v[a]).rotate_right(32);
        v[c] = gb(v[c], v[d]);
        v[b] = (v[b] ^ v[c]).rotate_right(24);
        v[a] = gb(v[a], v[b]);
        v[d] = (v[d] ^ v[a]).rotate_right(16);
        v[c] = gb(v[c], v[d]);
        v[b] = (v[b] ^ v[c]).rotate_right(63);
    }
    for (&word, &i) in v.iter().zip(at) {
        q[i] = word;
    }
}

/// The words of each row of the 8 × 8 matrix of 16-byte registers a block
/// is, for `P`'s first sweep.
#[expect(
    clippy::indexing_slicing,
    reason = "AUDIT: evaluated at compile time, where an index out of range is a build error, not a panic"
)]
const ROWS: [[usize; 16]; 8] = {
    let mut rows = [[0; 16]; 8];
    let mut row = 0;
    while row < 8 {
        let mut i = 0;
        while i < 16 {
            rows[row][i] = row * 16 + i;
            i += 1;
        }
        row += 1;
    }
    rows
};

/// The words of each column of the same matrix, for its second: register
/// `(r, c)` is words `16r + 2c` and `16r + 2c + 1`.
#[expect(
    clippy::indexing_slicing,
    reason = "AUDIT: evaluated at compile time, where an index out of range is a build error, not a panic"
)]
const COLUMNS: [[usize; 16]; 8] = {
    let mut columns = [[0; 16]; 8];
    let mut column = 0;
    while column < 8 {
        let mut r = 0;
        while r < 8 {
            columns[column][2 * r] = 16 * r + 2 * column;
            columns[column][2 * r + 1] = 16 * r + 2 * column + 1;
            r += 1;
        }
        column += 1;
    }
    columns
};

/// The compression function `G(X, Y)`, RFC 9106 §3.5.
fn compress(x: &Block, y: &Block) -> Block {
    let mut r = x.clone();
    r.xor(y);
    let mut q = r.0;
    for row in &ROWS {
        permute(&mut q, row);
    }
    for column in &COLUMNS {
        permute(&mut q, column);
    }
    let mut z = Block(q);
    z.xor(&r);
    r.0.fill(0);
    z
}

/// Whether `a` and `b` are equal, in a time that depends only on their
/// lengths: a check of a guessed hash must not say how many bytes it got
/// right.
#[must_use]
pub fn equal(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let difference = a.iter().zip(b).fold(0_u8, |acc, (x, y)| acc | (x ^ y));
    core::hint::black_box(difference) == 0
}
