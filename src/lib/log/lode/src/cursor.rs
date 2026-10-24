//! Where a reader stopped: `i=<seqnum>;b=<boot>`, both hex.
//!
//! The store's sequence number grows across boots on one machine, so it
//! alone says where to go on; the boot is there for a person reading it, and
//! so that a cursor from another machine's store is seen not to fit
//! (`--after-cursor` with a boot this store does not have reads from the
//! start). journald's cursors have the same `key=value;` shape.

use alloc::string::String;

use crate::record::Record;
use crate::segment::push_hex;

/// `record`'s cursor.
#[must_use]
pub fn of(record: &Record) -> String {
    let mut text = alloc::format!("i={:x};b=", record.seqnum);
    push_hex(&mut text, &record.boot_id);
    text
}

/// A cursor's sequence number.
#[must_use]
pub fn seqnum(cursor: &str) -> Option<u64> {
    cursor
        .trim()
        .split(';')
        .find_map(|part| part.strip_prefix("i="))
        .and_then(|hex| u64::from_str_radix(hex, 16).ok())
}
