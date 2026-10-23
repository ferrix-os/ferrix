//! The store's directories: which segments there are, in order, and their
//! bytes (`docs/LOGGING.md` §8.1).
//!
//! The persistent store is `/var/log/journal`, the volatile one
//! `/run/log/journal`, used when the persistent one cannot be made (a
//! read-only root). A reader reads both, so records the service kept in
//! `/run` before it could reach the disk are not missed.

use alloc::string::String;
use alloc::vec::Vec;

use ferrix_lode_core::segment::{self, Header};

use crate::sys;

/// The persistent store.
pub(crate) const PERSISTENT: &[u8] = b"/var/log/journal";
/// The volatile store.
pub(crate) const VOLATILE: &[u8] = b"/run/log/journal";

/// One segment file.
#[derive(Debug, Clone)]
pub(crate) struct Segment {
    /// Its directory, open.
    pub(crate) dir: usize,
    /// Its file name.
    pub(crate) name: String,
    /// The first sequence number its name says.
    pub(crate) first: u64,
}

/// An open store directory.
#[derive(Debug)]
pub(crate) struct Dir {
    /// Its descriptor.
    pub(crate) fd: usize,
}

impl Dir {
    /// Open `path` for reading its names.
    pub(crate) fn open(path: &[u8]) -> Result<Dir, sys::Errno> {
        let fd = sys::open(path, sys::O_RDONLY)?;
        Ok(Dir { fd })
    }

    /// The segments in this directory.
    pub(crate) fn segments(&self) -> Result<Vec<Segment>, sys::Errno> {
        let _ = sys::lseek(self.fd, 0, sys::SEEK_SET);
        let mut found = Vec::new();
        for name in sys::list_dir(self.fd)? {
            let Ok(name) = String::from_utf8(name) else {
                continue;
            };
            if !segment::is_segment_name(&name) {
                continue;
            }
            if let Some(first) = segment::seqnum_of_name(&name) {
                found.push(Segment {
                    dir: self.fd,
                    name,
                    first,
                });
            }
        }
        Ok(found)
    }
}

/// Every segment in `dirs`, oldest first by sequence.
pub(crate) fn all_segments(dirs: &[Dir]) -> Vec<Segment> {
    let mut all: Vec<Segment> = dirs
        .iter()
        .filter_map(|dir| dir.segments().ok())
        .flatten()
        .collect();
    all.sort_by_key(|segment| segment.first);
    all
}

/// A segment's header, read from its first bytes.
pub(crate) fn header(segment: &Segment) -> Option<Header> {
    let fd = sys::openat(
        segment.dir,
        segment.name.as_bytes(),
        sys::O_RDONLY | sys::O_CLOEXEC,
        0,
    )
    .ok()?;
    let mut buf = [0_u8; 512];
    let got = sys::read(fd, &mut buf).unwrap_or(0);
    sys::close(fd);
    Header::decode(buf.get(..got)?).map(|(header, _)| header)
}

/// A segment's bytes from `offset` on.
pub(crate) fn bytes(segment: &Segment, offset: usize) -> Option<Vec<u8>> {
    sys::read_file_from(segment.dir, segment.name.as_bytes(), offset).ok()
}
