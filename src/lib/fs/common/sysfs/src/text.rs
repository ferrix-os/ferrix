//! Writing numbers and names into a byte buffer.

use alloc::vec::Vec;
use core::fmt;

/// A byte buffer `write!` can format into.
#[derive(Debug)]
struct Sink<'a>(&'a mut Vec<u8>);

impl fmt::Write for Sink<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.0.extend_from_slice(text.as_bytes());
        Ok(())
    }
}

/// Append formatted text.
///
/// Formatting into a vector cannot fail — the sink never refuses and every
/// argument is a number or a string — so the result is dropped here, once,
/// rather than at every call.
pub(crate) fn put(out: &mut Vec<u8>, arguments: fmt::Arguments<'_>) {
    let _ = fmt::Write::write_fmt(&mut Sink(out), arguments);
}
