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
/// Nothing is reported: the sink grows the vector as the text needs, and a
/// growth the heap refuses goes to the allocation error handler, which stops
/// the machine (FX-0008). So this is only for text of a bounded size; text
/// whose length a program controls, `/proc/<pid>/maps` (finding F-97), is
/// written with [`put_within`] into room reserved fallibly first.
pub(crate) fn put(out: &mut Vec<u8>, arguments: fmt::Arguments<'_>) {
    let _ = fmt::Write::write_fmt(&mut Sink(out), arguments);
}

/// Pad with spaces until the line begun at `line_start` is `column` bytes
/// long, as `seq_pad` does. A line already that long is left alone.
pub(crate) fn pad_to(out: &mut Vec<u8>, line_start: usize, column: usize) {
    let target = line_start.saturating_add(column);
    if out.len() < target {
        out.resize(target, b' ');
    }
}

/// A line's room ran out: what `put_within` and `push_within` answer
/// instead of growing the buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Full;

/// A byte buffer `write!` formats into within the capacity it already has,
/// refusing what would not fit rather than allocating.
#[derive(Debug)]
struct Within<'a>(&'a mut Vec<u8>);

impl fmt::Write for Within<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        push_within(self.0, text.as_bytes()).map_err(|Full| fmt::Error)
    }
}

/// Append formatted text within `out`'s capacity, or refuse with [`Full`]
/// and nothing allocated: a refused write may leave part of the text, and the
/// caller abandons the line.
pub(crate) fn put_within(out: &mut Vec<u8>, arguments: fmt::Arguments<'_>) -> Result<(), Full> {
    fmt::Write::write_fmt(&mut Within(out), arguments).map_err(|fmt::Error| Full)
}

/// Append `bytes` within `out`'s capacity, or refuse with [`Full`], appending
/// nothing.
pub(crate) fn push_within(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), Full> {
    if out.capacity() - out.len() < bytes.len() {
        return Err(Full);
    }
    // Within the capacity checked above: no allocation.
    out.extend_from_slice(bytes);
    Ok(())
}
