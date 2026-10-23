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
//! * [`compress`]: sealed segments in zstd frames.
//! * [`summary`]: what a sealed segment holds, for a reader to skip it.
//! * [`session`]: what a seat's owner tells lode of sessions.
//! * [`upload`]: store-and-forward to a collector.
//! * [`alert`]: rules with an action: a notification, a unit started.
//! * [`regex`]: `-g`'s regular expressions, in linear time.
//! * [`units`]: units' output as init hands it over, paired with its
//!   console copy.
//! * [`access`]: whose segments a record goes to, and how a user is handed
//!   theirs.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod access;
pub mod alert;
pub mod compress;
pub mod console;
pub mod crc;
pub mod cursor;
pub mod fields;
pub mod filter;
pub mod format;
pub mod ingest;
pub mod kmsg;
pub mod limit;
pub mod protocols;
pub mod record;
pub mod regex;
pub mod retention;
pub mod segment;
pub mod session;
pub mod summary;
pub mod time;
pub mod units;
pub mod upload;

#[cfg(test)]
mod tests;
