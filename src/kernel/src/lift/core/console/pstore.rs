use crate::arch;
use crate::console::log;
use crate::console::println;
use crate::mm;
use core::sync::atomic::AtomicU8;
use core::sync::atomic::AtomicU64;
use core::sync::atomic::Ordering;
use ferrix_bootinfo::BootView;
use ferrix_bootinfo::MemKind;
use ferrix_bootinfo::PAGE_SIZE;
use ferrix_bootinfo::pstore;
use ferrix_bootinfo::pstore::HEADER_BYTES;
use ferrix_bootinfo::pstore::STOP_OFFSET;
use ferrix_bootinfo::pstore::Stop;
use ferrix_bootinfo::pstore::TABLE_COUNT_OFFSET;
use ferrix_bootinfo::pstore::TABLE_ENTRIES;
use ferrix_bootinfo::pstore::TABLE_OFFSET;
use ferrix_bootinfo::pstore::WRITTEN_OFFSET;
use ferrix_sync::Once;
pub fn stop(stop: Stop) -> bool {
    false
}

pub fn record(bytes: &[u8]) {}
