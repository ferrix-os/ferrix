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
use ferrix_lode_core::{access, compress};

use crate::sys;

/// The persistent store.
pub(crate) const PERSISTENT: &[u8] = b"/var/log/journal";
/// The volatile store.
pub(crate) const VOLATILE: &[u8] = b"/run/log/journal";

/// One segment file.
#[derive(Debug, Clone)]
pub(crate) struct Segment {
    /// Its store directory, open.
    pub(crate) dir: usize,
    /// Its path in the store: its file name, after `user-<uid>/` for a
    /// user's segment (`ferrix_lode_core::access`).
    pub(crate) name: String,
    /// The first sequence number its name says.
    pub(crate) first: u64,
    /// The file and its summary, open, when lode handed them to a user who
    /// cannot open them by name (`ferrix_lode_core::access`).
    pub(crate) passed: Option<(usize, Option<usize>)>,
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

    /// The segments in this directory, and in each user's directory in it.
    pub(crate) fn segments(&self) -> Result<Vec<Segment>, sys::Errno> {
        let _ = sys::lseek(self.fd, 0, sys::SEEK_SET);
        let mut found = Vec::new();
        for name in sys::list_dir(self.fd)? {
            let Ok(name) = String::from_utf8(name) else {
                continue;
            };
            if access::user_of_dir(&name).is_some() {
                user_segments(self.fd, &name, &mut found);
                continue;
            }
            if let Some(first) = segment_first(&name) {
                found.push(Segment {
                    dir: self.fd,
                    name,
                    first,
                    passed: None,
                });
            }
        }
        Ok(found)
    }
}

/// The segments in the user's directory `user` of the store `dir`, into
/// `found`.
fn user_segments(dir: usize, user: &str, found: &mut Vec<Segment>) {
    let Ok(fd) = sys::openat(dir, user.as_bytes(), sys::O_RDONLY | sys::O_CLOEXEC, 0) else {
        return;
    };
    for inner in sys::list_dir(fd).unwrap_or_default() {
        let Ok(inner) = String::from_utf8(inner) else {
            continue;
        };
        if let Some(first) = segment_first(&inner) {
            found.push(Segment {
                dir,
                name: alloc::format!("{user}/{inner}"),
                first,
                passed: None,
            });
        }
    }
    sys::close(fd);
}

/// The first sequence number of the segment file `name`, if it is one.
fn segment_first(name: &str) -> Option<u64> {
    if !segment::is_segment_name(name) {
        return None;
    }
    segment::seqnum_of_name(name)
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

/// The segments lode hands the caller, a user who reads their own
/// (`ferrix_lode_core::access`): each open, its summary beside it.
pub(crate) fn passed_segments() -> Result<Vec<Segment>, String> {
    use ferrix_lode_core::access::{self, Packet};
    let fd = crate::cli::connect_kind(access::SOCKET.as_bytes(), sys::SOCK_SEQPACKET).map_err(
        |error| {
            alloc::format!(
                "{}: errno {} (lode hands users their own records, and must be running)",
                access::SOCKET,
                error.0
            )
        },
    )?;
    if sys::write_all(fd, access::REQUEST).is_err() {
        sys::close(fd);
        return Err(String::from("asking lode for your records failed"));
    }
    let mut files: Vec<(String, usize)> = Vec::new();
    let mut buf = [0_u8; 1024];
    let ended = loop {
        let Some(received) = sys::receive_waiting(fd, &mut buf) else {
            break false;
        };
        if received.length == 0 {
            break false;
        }
        match Packet::decode(buf.get(..received.length).unwrap_or_default()) {
            Some(Packet::File(name)) => {
                if let Some(passed) = received.passed {
                    files.push((name, passed));
                }
            }
            Some(Packet::End(_)) => break true,
            None => {
                if let Some(passed) = received.passed {
                    sys::close(passed);
                }
            }
        }
    };
    sys::close(fd);
    if !ended {
        for (_, passed) in files {
            sys::close(passed);
        }
        return Err(String::from("lode ended its answer early"));
    }
    let summaries: Vec<(String, usize)> = files
        .iter()
        .filter(|(name, _)| name.ends_with(".sum"))
        .cloned()
        .collect();
    let mut found: Vec<Segment> = files
        .into_iter()
        .filter_map(|(name, passed)| {
            let first = segment_first(access::base_name(&name))?;
            let summary = summaries
                .iter()
                .find(|(each, _)| each.strip_suffix(".sum") == Some(name.as_str()))
                .map(|(_, fd)| *fd);
            Some(Segment {
                dir: sys::AT_FDCWD,
                name,
                first,
                passed: Some((passed, summary)),
            })
        })
        .collect();
    found.sort_by_key(|segment| segment.first);
    Ok(found)
}

/// Open the file `name` in `dir` for reading.
fn open_in(dir: usize, name: &str) -> Option<usize> {
    sys::openat(dir, name.as_bytes(), sys::O_RDONLY | sys::O_CLOEXEC, 0).ok()
}

/// The segment's file, open for reading, and whether to close it after:
/// a passed one stays open for the next look.
fn open_segment(segment: &Segment) -> Option<(usize, bool)> {
    match segment.passed {
        Some((fd, _)) => Some((fd, false)),
        None => open_in(segment.dir, &segment.name).map(|fd| (fd, true)),
    }
}

/// A segment's header, read from its first bytes, compressed or not.
pub(crate) fn header(segment: &Segment) -> Option<Header> {
    let (fd, close) = open_segment(segment)?;
    let start = head(fd);
    if close {
        sys::close(fd);
    }
    compress::header(&start)
}

/// A file's first 512 bytes, or fewer.
fn head(fd: usize) -> Vec<u8> {
    let mut buf = alloc::vec![0_u8; 512];
    let got = sys::lseek(fd, 0, sys::SEEK_SET)
        .ok()
        .and_then(|_| sys::read(fd, &mut buf).ok())
        .unwrap_or(0);
    buf.truncate(got);
    buf
}

/// Whether `segment` is compressed.
pub(crate) fn segment_compressed(segment: &Segment) -> bool {
    match segment.passed {
        Some((fd, _)) => compress::is_compressed(head(fd).get(..8).unwrap_or_default()),
        None => is_compressed(segment.dir, &segment.name),
    }
}

/// Whether the file `name` in `dir` is a compressed segment.
pub(crate) fn is_compressed(dir: usize, name: &str) -> bool {
    let Ok(fd) = sys::openat(dir, name.as_bytes(), sys::O_RDONLY | sys::O_CLOEXEC, 0) else {
        return false;
    };
    let mut buf = [0_u8; 8];
    let got = sys::read(fd, &mut buf).unwrap_or(0);
    sys::close(fd);
    compress::is_compressed(buf.get(..got).unwrap_or_default())
}

/// A segment's bytes from `offset` on, as written: a compressed one
/// unpacked first, which gives the same bytes at the same offsets.
pub(crate) fn bytes(segment: &Segment, offset: usize) -> Option<Vec<u8>> {
    match segment.passed {
        Some((fd, _)) => {
            let whole = sys::read_fd_from(fd, 0).ok()?;
            let whole = if compress::is_compressed(whole.get(..8).unwrap_or_default()) {
                compress::expand(whole)
            } else {
                whole
            };
            Some(whole.get(offset..).map(Vec::from).unwrap_or_default())
        }
        None => bytes_in(segment.dir, &segment.name, offset),
    }
}

/// [`bytes`] by directory and name.
pub(crate) fn bytes_in(dir: usize, name: &str, offset: usize) -> Option<Vec<u8>> {
    if is_compressed(dir, name) {
        let whole = compress::expand(sys::read_file_from(dir, name.as_bytes(), 0).ok()?);
        return Some(whole.get(offset..).map(Vec::from).unwrap_or_default());
    }
    sys::read_file_from(dir, name.as_bytes(), offset).ok()
}

/// A segment's size in bytes.
pub(crate) fn size(segment: &Segment) -> u64 {
    open_segment(segment).map_or(0, |(fd, close)| {
        let size = sys::lseek(fd, 0, sys::SEEK_END).unwrap_or(0);
        if close {
            sys::close(fd);
        }
        size as u64
    })
}

/// Write `<name>.sum` beside the sealed segment `name` in `dir`, from its
/// records (`ferrix_lode_core::summary`): a new file, then renamed over any
/// old one, so a reader sees one summary or the other, whole.
pub(crate) fn write_summary(dir: usize, name: &str) {
    use ferrix_lode_core::summary::Summary;
    let Some(bytes) = bytes_in(dir, name, 0) else {
        return;
    };
    let Some(scanned) = segment::scan(&bytes, 0) else {
        return;
    };
    let records: Vec<_> = scanned
        .records
        .into_iter()
        .map(|found| found.record)
        .collect();
    let summary = Summary::of(&records).encode();
    let fresh = access::temporary(name, ".sum.new");
    let Ok(fd) = sys::openat(
        dir,
        fresh.as_bytes(),
        sys::O_WRONLY | sys::O_CREAT | sys::O_TRUNC | sys::O_CLOEXEC,
        0o640,
    ) else {
        return;
    };
    let written = sys::write_all(fd, &summary);
    sys::close(fd);
    if written.is_ok() {
        let _ = sys::renameat(
            dir,
            fresh.as_bytes(),
            alloc::format!("{name}.sum").as_bytes(),
        );
    }
}

/// A segment's summary, if it has one that reads.
pub(crate) fn summary(segment: &Segment) -> Option<ferrix_lode_core::summary::Summary> {
    let bytes = match segment.passed {
        Some((_, summary)) => sys::read_fd_from(summary?, 0).ok()?,
        None => {
            let name = alloc::format!("{}.sum", segment.name);
            sys::read_file_from(segment.dir, name.as_bytes(), 0).ok()?
        }
    };
    ferrix_lode_core::summary::Summary::decode(&bytes)
}
