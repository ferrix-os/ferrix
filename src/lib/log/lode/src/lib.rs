//! lode's core: what a record is, how a segment holds records, how the
//! kernel log's records are read, the filter language, and the output
//! formats (`docs/LOGGING.md`).
//!
//! lode is the log manager: one native program that is the service (`lode
//! serve`, started by init in a sealed job) and the reader (`lode`, and
//! `journalctl` through a link). Everything either role does that can be a
//! pure function of bytes is here, so `cargo test`, Miri and the fuzzers
//! reach it; the program around it makes the system calls.
//!
//! * [`record`]: a record, journald's `FIELD=value` pairs with the store's
//!   stamps beside them.
//! * [`segment`]: the on-disk format, a header and checksummed entries,
//!   appended and never rewritten, and the scan that stops at a torn tail.
//! * [`kmsg`]: `/dev/kmsg`'s record format, read.
//! * [`console`]: who wrote a line the console carried, as far as its text
//!   says.
//! * [`filter`]: the filter language's core.
//! * [`time`]: civil dates, and the time forms `--since` takes.
//! * [`format`]: the output formats.
//! * [`cursor`]: where a reader stopped.
//! * [`ingest`]: records made from what the service reads, and about itself.
//! * [`protocols`]: syslog, systemd's native protocol and stdout streams, read.
//! * [`limit`]: floods held -- the rules, the folding of repeats, the limiter.
//! * [`retention`]: which records are kept how long.
//! * [`session`]: what a seat's owner tells lode of sessions.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod console;
pub mod crc;
pub mod cursor;
pub mod filter;
pub mod format;
pub mod ingest;
pub mod kmsg;
pub mod limit;
pub mod protocols;
pub mod record;
pub mod retention;
pub mod segment;
pub mod session;
pub mod time;

#[cfg(test)]
mod tests;
