//! Writing a large image file by changing only the blocks that differ.
//!
//! A boot image is 512 MiB (`fat.rs`), 173 MiB of it data and the rest zeros,
//! and a build after a one-line kernel edit changes about 16 KiB of it
//! (`docs/TEST-TIME.md`, *Caching and assembly*). Written whole, as it was
//! until 2026-10-15, each build waited 22 to 65 s on a busy disk for 512 MiB
//! of writeback, a build that changed nothing included. [`write()`] reads the
//! file already there block by block, compares, and writes only the blocks
//! that differ, so the file afterwards holds exactly `bytes` -- the same
//! bytes, length and all, a fresh write leaves -- having written kilobytes.
//!
//! A new file is made sparse: its length is set first, and a block of zeros
//! is never written into the hole that leaves. A shorter file is extended the
//! same way, a longer one cut to length.
//!
//! The file is changed in place, as the truncating write it replaces changed
//! it: a machine still running on the old image sees the new blocks, as it
//! saw the new file before. What is compared is what the file holds now, so
//! a block a guest wrote since the last build is found different and written
//! back.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::{Error, Result};

/// The unit compared and written: 64 KiB, so a 512 MiB image is 8,192
/// reads from the page cache, and an edit that changes a few bytes writes
/// one block.
const BLOCK: usize = 64 * 1024;

/// What [`write()`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Written {
    /// Bytes written: the blocks that differed.
    pub(crate) bytes: u64,
    /// Whether the file was made, rather than changed.
    pub(crate) made: bool,
}

/// Make the file at `path` hold exactly `bytes`, writing only the blocks
/// that differ from what it holds now.
///
/// # Errors
///
/// When the file cannot be opened, read, written or resized.
pub(crate) fn write(path: &Path, bytes: &[u8]) -> Result<Written> {
    let fail = |what: &str, error: std::io::Error| {
        Error::new(format!("{what} {}: {error}", path.display()))
    };
    let made = !path.exists();
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|error| fail("opening", error))?;
    let length = u64::try_from(bytes.len()).map_err(|_| Error::new("an image over 2^64 bytes"))?;
    let held = file
        .metadata()
        .map_err(|error| fail("reading the length of", error))?
        .len();
    if held != length {
        // Cut to length or extended with a hole, which reads as zeros.
        file.set_len(length)
            .map_err(|error| fail("resizing", error))?;
    }
    // What the file held before is only there up to the shorter of the two
    // lengths; past it, the file is the hole `set_len` left.
    let compared = usize::try_from(held.min(length)).unwrap_or(bytes.len());
    let mut old = vec![0_u8; BLOCK];
    let mut written = 0_u64;
    for (index, new) in bytes.chunks(BLOCK).enumerate() {
        let at = index * BLOCK;
        let same = if at < compared {
            let old = old
                .get_mut(..new.len())
                .ok_or_else(|| Error::new("a block longer than BLOCK"))?;
            let _ = file
                .seek(SeekFrom::Start(at as u64))
                .map_err(|error| fail("seeking in", error))?;
            file.read_exact(old)
                .map_err(|error| fail("reading", error))?;
            old == new
        } else {
            new.iter().all(|&byte| byte == 0)
        };
        if same {
            continue;
        }
        let _ = file
            .seek(SeekFrom::Start(at as u64))
            .map_err(|error| fail("seeking in", error))?;
        file.write_all(new)
            .map_err(|error| fail("writing", error))?;
        written += new.len() as u64;
    }
    Ok(Written {
        bytes: written,
        made,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file of its own under the system's temporary directory.
    fn scratch(name: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("ferrix-image-file-{}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// Bytes with data and zeros in blocks of both kinds, and a tail that is
    /// not a whole block.
    fn image(seed: u8) -> Vec<u8> {
        let mut bytes = vec![0_u8; 5 * BLOCK + 123];
        for (index, byte) in bytes[..BLOCK + 77].iter_mut().enumerate() {
            *byte = (index as u8).wrapping_mul(31).wrapping_add(seed);
        }
        bytes[3 * BLOCK + 9] = seed | 1;
        bytes[5 * BLOCK + 100] = 0xa5;
        bytes
    }

    fn holds(path: &Path, bytes: &[u8]) {
        assert!(
            std::fs::read(path).unwrap() == bytes,
            "{} differs",
            path.display()
        );
    }

    #[test]
    fn a_new_file_holds_the_bytes_and_skips_zero_blocks() {
        let path = scratch("new");
        let bytes = image(1);
        let written = write(&path, &bytes).unwrap();
        holds(&path, &bytes);
        assert!(written.made);
        // Blocks 0, 1, 3 and the tail hold data; 2 and 4 are zeros.
        assert_eq!(written.bytes, (3 * BLOCK + 123) as u64);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn the_same_bytes_again_write_nothing() {
        let path = scratch("same");
        let bytes = image(2);
        let _ = write(&path, &bytes).unwrap();
        let written = write(&path, &bytes).unwrap();
        holds(&path, &bytes);
        assert_eq!(
            written,
            Written {
                bytes: 0,
                made: false
            }
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn one_changed_byte_writes_its_block_and_no_other() {
        let path = scratch("one");
        let mut bytes = image(3);
        let _ = write(&path, &bytes).unwrap();
        bytes[2 * BLOCK + 5] = 0x5a;
        let written = write(&path, &bytes).unwrap();
        holds(&path, &bytes);
        assert_eq!(written.bytes, BLOCK as u64);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn data_where_the_new_bytes_are_zero_is_cleared() {
        let path = scratch("clear");
        let _ = write(&path, &[0xff_u8; 3 * BLOCK]).unwrap();
        let bytes = vec![0_u8; 3 * BLOCK];
        let written = write(&path, &bytes).unwrap();
        holds(&path, &bytes);
        assert_eq!(written.bytes, (3 * BLOCK) as u64);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_longer_file_is_cut_and_a_shorter_one_extended() {
        let path = scratch("length");
        let _ = write(&path, &[0x11_u8; 7 * BLOCK + 1]).unwrap();
        let bytes = image(4);
        let _ = write(&path, &bytes).unwrap();
        holds(&path, &bytes);

        std::fs::write(&path, [0x22_u8; 100]).unwrap();
        let _ = write(&path, &bytes).unwrap();
        holds(&path, &bytes);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn what_the_file_holds_now_is_compared_not_what_was_written_last() {
        // A guest that wrote into its image between two builds.
        let path = scratch("guest");
        let bytes = image(5);
        let _ = write(&path, &bytes).unwrap();
        let mut file = OpenOptions::new().write(true).open(&path).unwrap();
        let _ = file.seek(SeekFrom::Start(4 * BLOCK as u64 + 3)).unwrap();
        file.write_all(b"guest").unwrap();
        drop(file);
        let written = write(&path, &bytes).unwrap();
        holds(&path, &bytes);
        assert_eq!(written.bytes, BLOCK as u64);
        std::fs::remove_file(path).unwrap();
    }
}
