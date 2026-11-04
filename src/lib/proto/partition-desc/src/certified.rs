//! `etc/ferrix/certified`: the images a grant may name
//! (`docs/microkernel/K14.md` §5). Magic `FXCL`, version u32 = 1, count u32, then
//! `count` × (digest 32 bytes, name 32 bytes), at most [`MOST`]. A malformed file
//! reads as no list at all.

/// The first word.
const MAGIC: [u8; 4] = *b"FXCL";
/// The most entries.
pub const MOST: usize = 256;
/// Bytes of the header.
const HEADER: usize = 12;
/// Bytes of an entry.
const ENTRY: usize = 64;

/// A list, checked whole.
#[derive(Clone, Copy, Debug)]
pub struct List<'a> {
    bytes: &'a [u8],
    count: usize,
}

/// Read `bytes`; `None` for anything but a well-formed list.
#[must_use]
pub fn parse(bytes: &[u8]) -> Option<List<'_>> {
    if bytes.get(..4)? != &MAGIC[..] {
        return None;
    }
    let word = |at: usize| {
        bytes
            .get(at..at + 4)
            .and_then(|four| <[u8; 4]>::try_from(four).ok())
            .map(u32::from_le_bytes)
    };
    if word(4)? != 1 {
        return None;
    }
    let count = usize::try_from(word(8)?).ok()?;
    if count > MOST || bytes.len() != HEADER + count * ENTRY {
        return None;
    }
    Some(List { bytes, count })
}

impl<'a> List<'a> {
    /// How many.
    #[must_use]
    pub const fn count(&self) -> usize {
        self.count
    }

    /// Each digest, in order.
    pub fn digests(&self) -> impl Iterator<Item = [u8; 32]> + use<'a> {
        let bytes = self.bytes;
        (0..self.count).filter_map(move |index| {
            let at = HEADER + index * ENTRY;
            bytes
                .get(at..at + 32)
                .and_then(|digest| <[u8; 32]>::try_from(digest).ok())
        })
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::vec::Vec;

    use super::*;

    fn file(count: u32) -> Vec<u8> {
        let mut bytes = Vec::from(MAGIC);
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.extend_from_slice(&count.to_le_bytes());
        for index in 0..count {
            bytes.extend_from_slice(&[index as u8; 32]);
            bytes.extend_from_slice(&[0; 32]);
        }
        bytes
    }

    #[test]
    fn reads_what_was_written() {
        let bytes = file(2);
        let list = parse(&bytes).expect("parses");
        assert_eq!(list.count(), 2);
        assert_eq!(list.digests().nth(1), Some([1; 32]));
    }

    #[test]
    fn refuses_what_is_wrong_and_never_reads_past_the_end() {
        let good = file(2);
        for length in 0..good.len() {
            assert!(parse(&good[..length]).is_none(), "{length}");
        }
        let mut bad = good.clone();
        bad[0] = 0;
        assert!(parse(&bad).is_none());
        let mut bad = good.clone();
        bad[8] = 200;
        assert!(parse(&bad).is_none());
        let mut bad = good;
        bad[4] = 2;
        assert!(parse(&bad).is_none());
        assert!(parse(&file(MOST as u32 + 1)).is_none());
    }
}
