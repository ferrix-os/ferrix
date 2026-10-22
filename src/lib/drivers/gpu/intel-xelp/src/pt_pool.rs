//! The pages the PPGTTs' tables live in, grown a chunk at a time.
//!
//! i915 allocates each page table from the system's memory as a bind needs
//! it (`alloc_pt_dma`), so a program's binds are limited by memory, not by
//! a pool. A fixed pool ran out on the N150 when Endfield's world load bound
//! about 1.9 GiB (a table per 2 MiB, every process's VMs together): the
//! bind failed, the execbuffer with it, and anv marked the device lost. The
//! pool here starts with one chunk of [`CHUNK_PAGES`] pages and takes
//! another from its [`ChunkStore`] whenever every page is in use, up to
//! `MAX` chunks.
//!
//! Each chunk keeps its own index in its first [`META_PAGES`] pages: the
//! device address of every page in page order, then the table pages'
//! addresses sorted, so a table's entries are found by a binary search
//! rather than a scan.

use crate::ppgtt::PtMemory;

/// Pages in one chunk: 4 MiB.
pub const CHUNK_PAGES: usize = 1024;
/// The chunk's own index pages at its start.
pub const META_PAGES: usize = 4;
/// The table pages one chunk gives.
pub const TABLES_PER_CHUNK: usize = CHUNK_PAGES - META_PAGES;
/// Eight-byte words in a page.
const WORDS: usize = 512;
/// The first index page of the sorted addresses.
const SORTED_PAGE: usize = 2;
/// Bitmap words per chunk.
const USED_WORDS: usize = CHUNK_PAGES / 64;

/// Where the chunks' memory comes from: pages the device reads, written and
/// read eight bytes at a time.
pub trait ChunkStore {
    /// Make chunk `chunk` (always the next one) and write its pages' device
    /// addresses into `device`; `false` when there is no memory for it.
    fn grow(&mut self, chunk: usize, device: &mut [u64; CHUNK_PAGES]) -> bool;
    /// Word `word` (0..512) of page `page` of chunk `chunk`.
    fn read(&mut self, chunk: usize, page: usize, word: usize) -> u64;
    /// Write word `word` of page `page` of chunk `chunk`, as the GPU's walk
    /// will see it.
    fn write(&mut self, chunk: usize, page: usize, word: usize, value: u64);
}

#[derive(Clone, Copy, Debug)]
struct Chunk {
    used: [u64; USED_WORDS],
    free: usize,
    /// The lowest and highest table page's device address.
    low: u64,
    high: u64,
}

/// Page tables in up to `MAX` chunks of a [`ChunkStore`].
#[derive(Debug)]
pub struct PtPool<S, const MAX: usize> {
    store: S,
    chunks: [Option<Chunk>; MAX],
    count: usize,
}

impl<S: ChunkStore, const MAX: usize> PtPool<S, MAX> {
    /// A pool over `store` with its first chunk made; `None` when the
    /// store has none to give.
    pub fn new(store: S) -> Option<Self> {
        let mut pool = Self {
            store,
            chunks: [None; MAX],
            count: 0,
        };
        pool.grow().then_some(pool)
    }

    /// The store, for what else the platform keeps in it.
    pub fn store(&mut self) -> &mut S {
        &mut self.store
    }

    /// Chunks made so far.
    #[must_use]
    pub const fn chunks(&self) -> usize {
        self.count
    }

    /// Table pages in use.
    #[must_use]
    pub fn in_use(&self) -> usize {
        self.chunks
            .iter()
            .flatten()
            .map(|chunk| TABLES_PER_CHUNK - chunk.free)
            .sum()
    }

    fn grow(&mut self) -> bool {
        let index = self.count;
        if index >= MAX {
            return false;
        }
        let mut device = [0_u64; CHUNK_PAGES];
        if !self.store.grow(index, &mut device) {
            return false;
        }
        for (page, address) in device.iter().enumerate() {
            self.store
                .write(index, page / WORDS, page % WORDS, address & !0xfff);
        }
        // The table pages' addresses, the page number in the low 12 bits.
        let mut sorted = [0_u64; TABLES_PER_CHUNK];
        for (slot, page) in sorted.iter_mut().zip(META_PAGES..CHUNK_PAGES) {
            *slot = (device.get(page).copied().unwrap_or(0) & !0xfff) | page as u64;
        }
        sorted.sort_unstable();
        for (at, entry) in sorted.iter().enumerate() {
            self.store
                .write(index, SORTED_PAGE + at / WORDS, at % WORDS, *entry);
        }
        let mut used = [0_u64; USED_WORDS];
        if let Some(first) = used.first_mut() {
            *first = (1 << META_PAGES) - 1;
        }
        let low = sorted.first().map_or(0, |entry| entry & !0xfff);
        let high = sorted.last().map_or(0, |entry| entry & !0xfff);
        if let Some(cell) = self.chunks.get_mut(index) {
            *cell = Some(Chunk {
                used,
                free: TABLES_PER_CHUNK,
                low,
                high,
            });
        }
        self.count += 1;
        true
    }

    /// The chunk and page of the table at device address `table`.
    fn find(&mut self, table: u64) -> Option<(usize, usize)> {
        let address = table & !0xfff;
        for index in 0..self.count {
            let Some(chunk) = self.chunks.get(index).copied().flatten() else {
                continue;
            };
            if address < chunk.low || address > chunk.high {
                continue;
            }
            let (mut low, mut high) = (0, TABLES_PER_CHUNK);
            while low < high {
                let middle = low + (high - low) / 2;
                let entry = self
                    .store
                    .read(index, SORTED_PAGE + middle / WORDS, middle % WORDS);
                let at = entry & !0xfff;
                if at == address {
                    return Some((index, (entry & 0xfff) as usize));
                }
                if at < address {
                    low = middle + 1;
                } else {
                    high = middle;
                }
            }
        }
        None
    }

    fn take(&mut self) -> Option<(usize, usize)> {
        for (index, cell) in self.chunks.iter_mut().enumerate() {
            let Some(chunk) = cell.as_mut() else {
                continue;
            };
            if chunk.free == 0 {
                continue;
            }
            for (word_index, word) in chunk.used.iter_mut().enumerate() {
                if *word != u64::MAX {
                    let bit = (!*word).trailing_zeros() as usize;
                    *word |= 1 << bit;
                    chunk.free -= 1;
                    return Some((index, word_index * 64 + bit));
                }
            }
        }
        None
    }
}

impl<S: ChunkStore, const MAX: usize> PtMemory for PtPool<S, MAX> {
    fn alloc_pt(&mut self) -> Option<u64> {
        let (chunk, page) = match self.take() {
            Some(found) => found,
            None => {
                if !self.grow() {
                    return None;
                }
                self.take()?
            }
        };
        Some(self.store.read(chunk, page / WORDS, page % WORDS))
    }

    fn free_pt(&mut self, table: u64) {
        let Some((index, page)) = self.find(table) else {
            return;
        };
        if let Some(chunk) = self.chunks.get_mut(index).and_then(Option::as_mut)
            && let Some(word) = chunk.used.get_mut(page / 64)
            && *word & (1 << (page % 64)) != 0
        {
            *word &= !(1 << (page % 64));
            chunk.free += 1;
        }
    }

    fn read_entry(&mut self, table: u64, index: usize) -> u64 {
        match self.find(table) {
            Some((chunk, page)) if index < WORDS => self.store.read(chunk, page, index),
            _ => 0,
        }
    }

    fn write_entry(&mut self, table: u64, index: usize, value: u64) {
        if let Some((chunk, page)) = self.find(table)
            && index < WORDS
        {
            self.store.write(chunk, page, index, value);
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::vec;
    use std::vec::Vec;

    use super::{CHUNK_PAGES, ChunkStore, PtPool, TABLES_PER_CHUNK, WORDS};
    use crate::ppgtt::PtMemory;

    /// Chunks in host memory, their pages at scattered device addresses.
    #[derive(Default)]
    struct Store {
        chunks: Vec<Vec<u64>>,
        refuse_after: Option<usize>,
    }

    impl ChunkStore for Store {
        fn grow(&mut self, chunk: usize, device: &mut [u64; CHUNK_PAGES]) -> bool {
            if self.refuse_after.is_some_and(|limit| chunk >= limit) {
                return false;
            }
            for (page, slot) in device.iter_mut().enumerate() {
                // Out of order, interleaved between chunks.
                let scrambled = (page * 769) % CHUNK_PAGES;
                *slot = 0x1_0000_0000 + ((scrambled * 8 + chunk) as u64) * 4096;
            }
            self.chunks.push(vec![0; CHUNK_PAGES * WORDS]);
            true
        }

        fn read(&mut self, chunk: usize, page: usize, word: usize) -> u64 {
            self.chunks[chunk][page * WORDS + word]
        }

        fn write(&mut self, chunk: usize, page: usize, word: usize, value: u64) {
            self.chunks[chunk][page * WORDS + word] = value;
        }
    }

    #[test]
    fn grows_past_one_chunk_and_stops_at_max() {
        let mut pool: PtPool<Store, 3> = PtPool::new(Store::default()).unwrap();
        let mut tables = Vec::new();
        while let Some(table) = pool.alloc_pt() {
            tables.push(table);
        }
        assert_eq!(tables.len(), 3 * TABLES_PER_CHUNK);
        assert_eq!(pool.chunks(), 3);
        let mut unique = tables.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), tables.len(), "a table handed out twice");
        // Every table's entries are its own.
        for (n, table) in tables.iter().enumerate() {
            pool.write_entry(*table, n % WORDS, n as u64 + 1);
        }
        for (n, table) in tables.iter().enumerate() {
            assert_eq!(pool.read_entry(*table, n % WORDS), n as u64 + 1);
        }
        // A freed table is handed out again.
        let back = tables[TABLES_PER_CHUNK + 7];
        pool.free_pt(back);
        assert_eq!(pool.in_use(), 3 * TABLES_PER_CHUNK - 1);
        assert_eq!(pool.alloc_pt(), Some(back));
        assert_eq!(pool.alloc_pt(), None);
    }

    #[test]
    fn a_store_out_of_memory_is_no_table() {
        let store = Store {
            refuse_after: Some(1),
            ..Store::default()
        };
        let mut pool: PtPool<Store, 8> = PtPool::new(store).unwrap();
        for _ in 0..TABLES_PER_CHUNK {
            assert!(pool.alloc_pt().is_some());
        }
        assert_eq!(pool.alloc_pt(), None);
        assert_eq!(pool.chunks(), 1);
        // An unknown address reads as zero and is not written.
        pool.write_entry(0xdead_0000, 3, 9);
        assert_eq!(pool.read_entry(0xdead_0000, 3), 0);
    }
}
