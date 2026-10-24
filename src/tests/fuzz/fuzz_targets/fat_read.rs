//! Fuzz the read-only FAT32 reader `lode` reads the ESP's
//! `FERRIX/LASTKERNEL.TXT` with (`docs/LOGGING.md` §5).
//!
//! The ESP is bytes any operating system on the machine, or its firmware,
//! may have written. The reader promises an answer for every input.
//!
//! # The properties
//!
//! Not panicking, and ending, are the floor. Beyond it:
//!
//! 1. **A read is bounded by its limit**: a file read with a limit is that
//!    many bytes or fewer, or `TooLarge`.
//! 2. **A read is the same twice**: the reader keeps no state between reads
//!    that could change an answer.
//! 3. **What the root lists is found**: every name the root lists, looked up
//!    by that name, gives an entry (the first of that name).

#![no_main]

use ferrix_fat::{Error, Volume};
use libfuzzer_sys::fuzz_target;

/// The limit the properties read with.
const MOST: usize = 64 * 1024;

fuzz_target!(|data: &[u8]| {
    let Ok(mut volume) = Volume::open(data) else {
        return;
    };
    let first = volume.read_file("FERRIX/LASTKERNEL.TXT", MOST);
    if let Ok(bytes) = &first {
        assert!(bytes.len() <= MOST, "a read past its limit");
    }
    let again = volume.read_file("FERRIX/LASTKERNEL.TXT", MOST);
    assert_eq!(first, again, "two reads of one file disagree");
    if let Ok(entries) = volume.root() {
        for entry in entries.iter().take(32) {
            if entry.name.contains('/') || entry.name.is_empty() {
                continue;
            }
            match volume.find(&entry.name) {
                Ok(_) | Err(Error::Io | Error::Corrupt(_)) => {}
                Err(other) => panic!("a listed name was not found: {other:?}"),
            }
        }
    }
});
