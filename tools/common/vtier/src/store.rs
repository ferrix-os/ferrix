//! Fixed-size blocks in several device buffers, each with a checksum.

/// A block: the most one blocking transfer moves.
pub const BLOCK: usize = 128 * 1024;
/// `errno`s the store answers with.
pub const EIO: i32 = 5;
/// No block left.
pub const ENOSPC: i32 = 28;

/// Where the bytes are: one region of memory addressed by byte offset, with
/// every transfer a whole block inside one buffer ([`Layout`]).
pub trait Backend {
    /// Fill `to` from `at`, or say the device's error.
    fn read(&mut self, at: u64, to: &mut [u8]) -> Result<(), i32>;
    /// Write `from` at `at`, or say the device's error.
    fn write(&mut self, at: u64, from: &[u8]) -> Result<(), i32>;
    /// Wait until everything asked is done.
    fn finish(&mut self) -> Result<(), i32> {
        Ok(())
    }
}

/// Which block of which file a read wants: part of what its checksum is over, so
/// that a right block in the wrong place reads as wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tag {
    /// The inode.
    pub ino: u64,
    /// The block's place in the file.
    pub index: u64,
}

/// Memory in the host: FUSE's own cost, and the tests.
pub struct Ram(pub Vec<u8>);

impl Backend for Ram {
    fn read(&mut self, at: u64, to: &mut [u8]) -> Result<(), i32> {
        let at = usize::try_from(at).map_err(|_| EIO)?;
        to.copy_from_slice(self.0.get(at..at + to.len()).ok_or(EIO)?);
        Ok(())
    }
    fn write(&mut self, at: u64, from: &[u8]) -> Result<(), i32> {
        let at = usize::try_from(at).map_err(|_| EIO)?;
        self.0
            .get_mut(at..at + from.len())
            .ok_or(EIO)?
            .copy_from_slice(from);
        Ok(())
    }
}

impl std::fmt::Debug for Ram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Ram({} bytes)", self.0.len())
    }
}

/// How `total` bytes split into buffers no bigger than the driver's largest
/// allocation (6.3 GB on the 3090), each a whole number of blocks, so that no
/// block crosses two.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    /// Blocks in a full buffer.
    pub per_buffer: u64,
    /// Blocks in all.
    pub blocks: u64,
}

impl Layout {
    /// The split for `total` bytes under an allocation limit of `cap`.
    #[must_use]
    pub fn new(total: u64, cap: u64) -> Self {
        let block = BLOCK as u64;
        Self {
            per_buffer: (cap / block).max(1),
            blocks: total / block,
        }
    }
    /// The size in bytes of each buffer to allocate.
    #[must_use]
    pub fn buffers(self) -> Vec<u64> {
        let mut left = self.blocks;
        let mut sizes = Vec::new();
        while left > 0 {
            let here = left.min(self.per_buffer);
            sizes.push(here * BLOCK as u64);
            left -= here;
        }
        sizes
    }
    /// Which buffer a byte offset is in, and where in it.
    #[must_use]
    pub fn place(self, at: u64) -> (usize, u64) {
        let bytes = self.per_buffer * BLOCK as u64;
        (
            usize::try_from(at / bytes).unwrap_or(usize::MAX),
            at % bytes,
        )
    }
}

/// What has gone wrong in this store's life. Anything but a clean one makes a
/// run on it an infrastructure failure.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Health {
    /// Why the store refuses everything, once it does.
    pub dead: Option<String>,
    /// Blocks that did not read back as written.
    pub crc_errors: u64,
    /// Transfers the device refused.
    pub device_errors: u64,
    /// Writes refused for want of a block.
    pub no_space: u64,
}

impl Health {
    /// The status file's text.
    #[must_use]
    pub fn render(&self, instance: &str, seq: u64, used: u64, blocks: u64) -> String {
        format!(
            "instance={instance}\nseq={seq}\ninject={}\nstate={}\ncrc_errors={}\ndevice_errors={}\nno_space={}\nused_blocks={used}\nblocks={blocks}\nwhy={}\n",
            if cfg!(feature = "fault-inject") {
                "yes"
            } else {
                "no"
            },
            if self.dead.is_some() { "dead" } else { "ok" },
            self.crc_errors,
            self.device_errors,
            self.no_space,
            self.dead.as_deref().unwrap_or(""),
        )
    }
}

/// The blocks.
pub struct Store<B> {
    backend: B,
    /// Each block's checksum and how many times it has been written or freed.
    /// In the host's memory, never on the card: the card is what is being checked.
    table: Vec<(u32, u64)>,
    free: Vec<u32>,
    /// What has gone wrong.
    pub health: Health,
    /// Counts up each time [`Self::health`] changes, for the status file.
    pub changes: u64,
}

impl<B> std::fmt::Debug for Store<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("free", &self.free.len())
            .field("health", &self.health)
            .finish_non_exhaustive()
    }
}

impl<B: Backend> Store<B> {
    /// A store of `blocks` empty blocks on `backend`.
    #[must_use]
    pub fn new(backend: B, blocks: u32) -> Self {
        Self {
            backend,
            table: vec![(0, 0); blocks as usize],
            free: (0..blocks).rev().collect(),
            health: Health::default(),
            changes: 0,
        }
    }

    /// Blocks in all.
    #[must_use]
    pub fn blocks(&self) -> u64 {
        self.table.len() as u64
    }
    /// Blocks not in use.
    #[must_use]
    pub fn free_blocks(&self) -> u64 {
        self.free.len() as u64
    }
    /// Whether the store has stopped answering.
    #[must_use]
    pub fn is_dead(&self) -> bool {
        self.health.dead.is_some()
    }

    fn die(&mut self, why: String) -> i32 {
        if self.health.dead.is_none() {
            self.health.dead = Some(why);
        }
        self.changes += 1;
        EIO
    }

    /// A free block, or `ENOSPC`, counted.
    ///
    /// # Errors
    /// `ENOSPC` when none is left.
    pub fn take(&mut self) -> Result<u32, i32> {
        if let Some(block) = self.free.pop() {
            return Ok(block);
        }
        self.health.no_space += 1;
        self.changes += 1;
        Err(ENOSPC)
    }

    /// Give a block back.
    pub fn give(&mut self, block: u32) {
        // Its old contents are nobody's now: a read of them must not pass.
        self.table[block as usize].1 += 1;
        self.free.push(block);
    }

    /// Read a whole block as `tag`'s, and check it.
    ///
    /// # Errors
    /// `EIO`, and the store is dead from here on, on a device error or a
    /// block that is not what was last written there for `tag`.
    pub fn read(&mut self, block: u32, tag: Tag, to: &mut [u8]) -> Result<(), i32> {
        debug_assert_eq!(to.len(), BLOCK, "a read is of a whole block");
        if self.is_dead() {
            return Err(EIO);
        }
        let at = u64::from(block) * BLOCK as u64;
        #[cfg(feature = "fault-inject")]
        if crate::inject::flip_now() {
            let mut byte = [0];
            self.backend.read(at + 7, &mut byte).ok();
            byte[0] ^= 1;
            self.backend.write(at + 7, &byte).ok();
        }
        if let Err(error) = self.backend.read(at, to) {
            self.health.device_errors += 1;
            return Err(self.die(format!("device read: {error}")));
        }
        let (crc, generation) = self.table[block as usize];
        if checksum(tag, generation, to) != crc {
            self.health.crc_errors += 1;
            return Err(self.die(format!("block {block} is not what was written ({tag:?})")));
        }
        Ok(())
    }

    /// Write a whole block as `tag`'s and remember its checksum.
    ///
    /// # Errors
    /// `EIO`, and the store is dead from here on, on a device error.
    pub fn write(&mut self, block: u32, tag: Tag, from: &[u8]) -> Result<(), i32> {
        debug_assert_eq!(from.len(), BLOCK, "a write is of a whole block");
        if self.is_dead() {
            return Err(EIO);
        }
        let entry = &mut self.table[block as usize];
        entry.1 += 1;
        entry.0 = checksum(tag, entry.1, from);
        if let Err(error) = self.backend.write(u64::from(block) * BLOCK as u64, from) {
            self.health.device_errors += 1;
            return Err(self.die(format!("device write: {error}")));
        }
        Ok(())
    }

    /// A barrier: everything done on the device, and the status file due again.
    ///
    /// # Errors
    /// `EIO`, and the store is dead from here on, on a device error.
    pub fn barrier(&mut self) -> Result<(), i32> {
        if self.is_dead() {
            return Err(EIO);
        }
        if let Err(error) = self.backend.finish() {
            self.health.device_errors += 1;
            return Err(self.die(format!("device finish: {error}")));
        }
        self.changes += 1;
        Ok(())
    }

    /// The backend, for a test.
    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }
}

/// The checksum of a block: over which file, which place, which write of the
/// block this is, and then its bytes.
#[must_use]
pub fn checksum(tag: Tag, generation: u64, data: &[u8]) -> u32 {
    let mut seed = [0u8; 24];
    seed[..8].copy_from_slice(&tag.ino.to_le_bytes());
    seed[8..16].copy_from_slice(&tag.index.to_le_bytes());
    seed[16..].copy_from_slice(&generation.to_le_bytes());
    !crc32c_more(crc32c_more(u32::MAX, &seed), data)
}

/// CRC32C of `data`.
#[must_use]
pub fn crc32c(data: &[u8]) -> u32 {
    !crc32c_more(u32::MAX, data)
}

/// Carry on a CRC32C, with the processor's instruction where it has one.
fn crc32c_more(state: u32, data: &[u8]) -> u32 {
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("sse4.2") {
        // SAFETY: SSE4.2 was just detected, the one thing the function requires.
        return unsafe { crc32c_x86(state, data) };
    }
    crc32c_plain(state, data)
}

/// CRC32C by the instruction.
///
/// # Safety
/// The processor must have SSE4.2.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.2")]
unsafe fn crc32c_x86(state: u32, data: &[u8]) -> u32 {
    use std::arch::x86_64::{_mm_crc32_u8, _mm_crc32_u64};
    let mut crc = u64::from(state);
    let (words, tail) = data.as_chunks::<8>();
    for word in words {
        crc = _mm_crc32_u64(crc, u64::from_le_bytes(*word));
    }
    let mut crc = crc as u32;
    for byte in tail {
        crc = _mm_crc32_u8(crc, *byte);
    }
    crc
}

/// CRC32C a bit at a time: the reference for the instruction, and other hosts.
fn crc32c_plain(state: u32, data: &[u8]) -> u32 {
    let mut crc = state;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0x82F6_3B78
            } else {
                crc >> 1
            };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(blocks: u32) -> Store<Ram> {
        Store::new(Ram(vec![0; blocks as usize * BLOCK]), blocks)
    }

    const A: Tag = Tag { ino: 2, index: 0 };
    const B: Tag = Tag { ino: 2, index: 1 };

    #[test]
    fn the_checksum_is_crc32c_on_either_path() {
        // RFC 3720's vector, and a length past the eight-byte words.
        assert_eq!(!crc32c_plain(u32::MAX, b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        let long: Vec<u8> = (0..=255u8).cycle().take(1021).collect();
        assert_eq!(crc32c(&long), !crc32c_plain(u32::MAX, &long));
    }

    #[test]
    fn a_flipped_bit_is_refused_and_the_store_refuses_everything_after() {
        let mut s = store(2);
        let block = s.take().unwrap();
        let other = s.take().unwrap();
        s.write(block, A, &vec![7; BLOCK]).unwrap();
        s.write(other, B, &vec![9; BLOCK]).unwrap();
        s.backend_mut().0[block as usize * BLOCK + 3] ^= 1;
        let mut back = vec![0; BLOCK];
        assert_eq!(s.read(block, A, &mut back), Err(EIO));
        assert_eq!(s.health.crc_errors, 1);
        assert!(s.is_dead());
        assert!(
            s.health
                .dead
                .as_deref()
                .unwrap()
                .contains("not what was written")
        );
        // A good block, and a write: refused too, since the card is not trusted now.
        assert_eq!(s.read(other, B, &mut back), Err(EIO));
        assert_eq!(s.write(other, B, &vec![1; BLOCK]), Err(EIO));
    }

    #[test]
    fn a_right_block_in_the_wrong_place_does_not_pass() {
        // The same bytes, so that only the seed can tell them apart.
        let mut s = store(2);
        let block = s.take().unwrap();
        s.write(block, A, &vec![7; BLOCK]).unwrap();
        let mut back = vec![0; BLOCK];
        s.read(block, A, &mut back).unwrap();
        assert_eq!(s.read(block, B, &mut back), Err(EIO));
        assert_eq!(s.health.crc_errors, 1);
    }

    #[test]
    fn another_inodes_block_does_not_pass() {
        let mut s = store(1);
        let block = s.take().unwrap();
        s.write(block, A, &vec![7; BLOCK]).unwrap();
        let mut back = vec![0; BLOCK];
        assert_eq!(s.read(block, Tag { ino: 3, index: 0 }, &mut back), Err(EIO));
    }

    #[test]
    fn a_freed_block_read_back_does_not_pass_though_its_bytes_are_still_there() {
        let mut s = store(1);
        let block = s.take().unwrap();
        s.write(block, A, &vec![7; BLOCK]).unwrap();
        s.give(block);
        let mut back = vec![0; BLOCK];
        assert_eq!(s.read(block, A, &mut back), Err(EIO));
    }

    #[test]
    fn an_old_version_restored_does_not_pass() {
        // A write the card lost leaves what was there before.
        let mut s = store(1);
        let block = s.take().unwrap();
        s.write(block, A, &vec![1; BLOCK]).unwrap();
        let old = s.backend_mut().0.clone();
        s.write(block, A, &vec![2; BLOCK]).unwrap();
        s.backend_mut().0 = old;
        let mut back = vec![0; BLOCK];
        assert_eq!(s.read(block, A, &mut back), Err(EIO));
    }

    #[test]
    fn the_same_bytes_written_again_are_a_new_generation_and_read_back() {
        let mut s = store(1);
        let block = s.take().unwrap();
        let first = {
            s.write(block, A, &vec![1; BLOCK]).unwrap();
            s.table[block as usize]
        };
        s.write(block, A, &vec![1; BLOCK]).unwrap();
        assert_ne!(s.table[block as usize], first);
        let mut back = vec![0; BLOCK];
        s.read(block, A, &mut back).unwrap();
        assert_eq!(back, vec![1; BLOCK]);
    }

    #[test]
    fn a_barrier_on_a_good_store_says_so_and_on_a_dead_one_refuses() {
        let mut s = store(1);
        s.barrier().unwrap();
        assert_eq!(s.changes, 1);
        let block = s.take().unwrap();
        s.write(block, A, &vec![1; BLOCK]).unwrap();
        s.backend_mut().0[0] ^= 1;
        let mut back = vec![0; BLOCK];
        s.read(block, A, &mut back).unwrap_err();
        assert_eq!(s.barrier(), Err(EIO));
    }

    struct Failing;
    impl Backend for Failing {
        fn read(&mut self, _: u64, _: &mut [u8]) -> Result<(), i32> {
            Err(-5)
        }
        fn write(&mut self, _: u64, _: &[u8]) -> Result<(), i32> {
            Err(-5)
        }
    }

    #[test]
    fn a_device_error_kills_the_store() {
        let mut s = Store::new(Failing, 1);
        let block = s.take().unwrap();
        assert_eq!(s.write(block, A, &vec![0; BLOCK]), Err(EIO));
        assert_eq!(s.health.device_errors, 1);
        assert!(s.is_dead());
        let said = s.health.render("x", 3, 0, 1);
        assert!(said.contains("state=dead") && said.contains("seq=3"));
        assert!(said.contains(if cfg!(feature = "fault-inject") {
            "inject=yes"
        } else {
            "inject=no"
        }));
    }

    #[test]
    fn running_out_is_counted_and_a_block_given_back_is_there_again() {
        let mut s = store(1);
        let block = s.take().unwrap();
        assert_eq!(s.take(), Err(ENOSPC));
        assert_eq!(s.health.no_space, 1);
        assert!(!s.is_dead());
        s.give(block);
        assert_eq!(s.take(), Ok(block));
    }

    #[test]
    fn buffers_hold_whole_blocks_under_the_cap() {
        let cap = 6_324_011_008;
        let layout = Layout::new(20 << 30, cap);
        let sizes = layout.buffers();
        assert_eq!(sizes.len(), 4);
        assert!(sizes.iter().all(|&s| s <= cap && s % BLOCK as u64 == 0));
        assert_eq!(sizes.iter().sum::<u64>(), layout.blocks * BLOCK as u64);
        // The last block of the first buffer and the first of the second.
        let full = layout.per_buffer * BLOCK as u64;
        assert_eq!(layout.place(full - BLOCK as u64), (0, full - BLOCK as u64));
        assert_eq!(layout.place(full), (1, 0));
        assert_eq!(layout.place(3 * full + BLOCK as u64), (3, BLOCK as u64));
    }
}
