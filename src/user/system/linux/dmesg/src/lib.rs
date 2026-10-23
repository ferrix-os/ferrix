//! Ferrix's `dmesg`: the kernel log, as util-linux's `dmesg` shows it.
//!
//! Installed as `/bin/dmesg` itself, in busybox's place, so that a person and
//! a script get util-linux's output and flags rather than busybox's subset.
//!
//! # Where it reads
//!
//! From `/dev/kmsg` when it opens, one record a read, as util-linux does: the
//! records since the last clear (`lseek(SEEK_DATA)`), and with `-w` every one
//! after them as it comes, or with `-W` only those. Through `syslog(2)`'s
//! `READ_ALL` otherwise, or with `-S`. Both give a record's priority and its
//! time since boot in microseconds; the kernel writes them
//! (`ferrix_linux_abi::kmsg`), and [`record`] reads them back.
//!
//! # What it shows
//!
//! ```text
//! [    1.234567] text                     the default
//! <6>[    1.234567] text                  -r, --raw
//! kern  :info  : [    1.234567] text      -x, --decode
//! [Thu Oct 23 14:01:02 2026] text         -T, --ctime
//! [Oct23 14:01] text, then [  +0.000123]  -H, --human (no pager, no colour)
//! text                                    -t, --notime
//! ```
//!
//! `-l`/`--level` and `-f`/`--facility` keep the records named, by name or
//! number, `-k` the kernel's and `-u` the rest. `-c` clears after printing,
//! `-C` only clears, and `-n`, `-D` and `-E` set the console's level; those
//! need root, as reading does on Ferrix. The parts that are not I/O are here,
//! tested on the host; `main.rs` and `sys.rs` read and write.

pub mod args;
pub mod record;
pub mod show;

#[cfg(test)]
mod tests;
