//! Reader for the cpio "newc" archive format, which is what an initramfs is.
//!
//! The kernel is handed an initramfs by the loader and unpacks it into a ram
//! filesystem before any disk driver exists, so this parser runs in ring 0 on
//! bytes nobody in this tree produced. It therefore borrows the archive and
//! copies nothing, allocates nothing, and answers every input with either a
//! value or a [`CpioError`] — there is no input for which it panics, indexes
//! out of bounds, or reads past the end of the slice.
//!
//! Both `070701` (plain newc) and `070702` (newc with a per-file checksum) are
//! accepted; the two differ only in whether `c_check` is meaningful, which is
//! reported as [`Entry::check`] and otherwise left to the caller.
//!
//! # No unsafe
//!
//! Every field is one bounds-checked slice and an ASCII-hex decode, so the
//! crate carries `#![forbid(unsafe_code)]`. Casting the header to a `repr(C)`
//! struct would be shorter and would put the first filesystem the machine ever
//! sees inside an `unsafe` block for nothing.
//!
//! ```
//! # use ferrix_cpio::{Archive, CpioError, FileType};
//! # fn example(initramfs: &[u8]) -> Result<(), CpioError> {
//! let archive = Archive::new(initramfs);
//!
//! // Size the ram filesystem before unpacking a single entry.
//! let summary = archive.summary()?;
//! let _ = (summary.entries, summary.data_bytes);
//!
//! for entry in archive.entries() {
//!     let entry = entry?;
//!     if !entry.is_safe_path() {
//!         continue;
//!     }
//!     match entry.file_type() {
//!         FileType::Directory => { /* mkdir */ }
//!         FileType::Regular => { /* write entry.data */ }
//!         _ => {}
//!     }
//! }
//! # Ok(())
//! # }
//! ```

#![no_std]
#![forbid(unsafe_code)]

use core::fmt;

// ---------------------------------------------------------------------------
// Format constants
// ---------------------------------------------------------------------------

/// Size of a newc header, in bytes: a 6-character magic followed by thirteen
/// 8-character hexadecimal fields.
pub const HEADER_SIZE: usize = 110;

/// Magic of a plain newc archive.
pub const MAGIC_NEWC: &[u8] = b"070701";

/// Magic of a newc archive whose `c_check` field carries a checksum.
pub const MAGIC_CRC: &[u8] = b"070702";

/// Name of the entry that terminates an archive. It is never reported to the
/// caller.
pub const TRAILER_NAME: &str = "TRAILER!!!";

/// Largest `c_namesize` accepted, including the trailing NUL.
///
/// A name longer than this is not a name a bootable initramfs contains; it is a
/// corrupt header whose length field happened to decode. Rejecting it here
/// keeps a garbage length from being reported as a truncated archive, which
/// would send whoever reads the message after the wrong bug.
pub const MAX_NAME_SIZE: u32 = 4096;

/// Largest number of entries walked before the archive is declared malformed.
///
/// Iteration is already bounded by the archive length, since every entry
/// consumes at least a header. This second bound exists so that a caller who
/// hands us a gigabyte of headers gets an error instead of an unpacking loop
/// that holds the CPU for an unbounded time during early boot.
pub const MAX_ENTRIES: usize = 1 << 20;

/// Mask selecting the file type bits of `c_mode`.
pub const S_IFMT: u32 = 0o170000;
/// `c_mode` file type: FIFO.
pub const S_IFIFO: u32 = 0o010000;
/// `c_mode` file type: character device.
pub const S_IFCHR: u32 = 0o020000;
/// `c_mode` file type: directory.
pub const S_IFDIR: u32 = 0o040000;
/// `c_mode` file type: block device.
pub const S_IFBLK: u32 = 0o060000;
/// `c_mode` file type: regular file.
pub const S_IFREG: u32 = 0o100000;
/// `c_mode` file type: symbolic link.
pub const S_IFLNK: u32 = 0o120000;
/// `c_mode` file type: socket.
pub const S_IFSOCK: u32 = 0o140000;
/// Mask selecting the permission and set-id bits of `c_mode`.
pub const PERMISSION_MASK: u32 = 0o7777;

/// Length of the magic, in bytes.
const MAGIC_LEN: usize = 6;
/// Length of every field after the magic, in ASCII hex characters.
const FIELD_LEN: usize = 8;

/// Offset of `c_ino` within the header.
const OFF_INO: usize = 6;
/// Offset of `c_mode` within the header.
const OFF_MODE: usize = 14;
/// Offset of `c_uid` within the header.
const OFF_UID: usize = 22;
/// Offset of `c_gid` within the header.
const OFF_GID: usize = 30;
/// Offset of `c_nlink` within the header.
const OFF_NLINK: usize = 38;
/// Offset of `c_mtime` within the header.
const OFF_MTIME: usize = 46;
/// Offset of `c_filesize` within the header.
const OFF_FILESIZE: usize = 54;
/// Offset of `c_devmajor` within the header.
const OFF_DEVMAJOR: usize = 62;
/// Offset of `c_devminor` within the header.
const OFF_DEVMINOR: usize = 70;
/// Offset of `c_rdevmajor` within the header.
const OFF_RDEVMAJOR: usize = 78;
/// Offset of `c_rdevminor` within the header.
const OFF_RDEVMINOR: usize = 86;
/// Offset of `c_namesize` within the header.
const OFF_NAMESIZE: usize = 94;
/// Offset of `c_check` within the header.
const OFF_CHECK: usize = 102;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Which header field an error is about.
///
/// Carried by [`CpioError::BadHexField`] so that a rejected archive names the
/// field that failed to decode rather than only the entry it belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Field {
    /// `c_ino`, the inode number.
    Ino,
    /// `c_mode`, the type and permission bits.
    Mode,
    /// `c_uid`, the owning user.
    Uid,
    /// `c_gid`, the owning group.
    Gid,
    /// `c_nlink`, the hard link count.
    Nlink,
    /// `c_mtime`, the modification time.
    Mtime,
    /// `c_filesize`, the length of the file data.
    FileSize,
    /// `c_devmajor`, the major number of the device the file lives on.
    DevMajor,
    /// `c_devminor`, the minor number of the device the file lives on.
    DevMinor,
    /// `c_rdevmajor`, the major number a device node refers to.
    RdevMajor,
    /// `c_rdevminor`, the minor number a device node refers to.
    RdevMinor,
    /// `c_namesize`, the length of the name including its NUL.
    NameSize,
    /// `c_check`, the checksum of the CRC format.
    Check,
}

impl Field {
    /// The field's name as the format description spells it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Field::Ino => "c_ino",
            Field::Mode => "c_mode",
            Field::Uid => "c_uid",
            Field::Gid => "c_gid",
            Field::Nlink => "c_nlink",
            Field::Mtime => "c_mtime",
            Field::FileSize => "c_filesize",
            Field::DevMajor => "c_devmajor",
            Field::DevMinor => "c_devminor",
            Field::RdevMajor => "c_rdevmajor",
            Field::RdevMinor => "c_rdevminor",
            Field::NameSize => "c_namesize",
            Field::Check => "c_check",
        }
    }
}

impl fmt::Display for Field {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why an archive was rejected.
///
/// Every variant but the last two carries the archive offset of the header it
/// is about, because the operator-facing report for a bad initramfs is useless
/// without it: the archive is produced by the build, and the offset is what
/// identifies which entry the producer got wrong.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CpioError {
    /// Fewer than [`HEADER_SIZE`] bytes remain at `offset`.
    TruncatedHeader {
        /// Archive offset where the header begins.
        offset: usize,
    },
    /// The magic at `offset` is neither [`MAGIC_NEWC`] nor [`MAGIC_CRC`].
    BadMagic {
        /// Archive offset where the header begins.
        offset: usize,
    },
    /// A header field holds a character that is not an ASCII hex digit.
    BadHexField {
        /// Archive offset where the header begins.
        offset: usize,
        /// Which field failed to decode.
        field: Field,
    },
    /// `c_namesize` is zero, so the entry has no name at all, not even a NUL.
    EmptyName {
        /// Archive offset where the header begins.
        offset: usize,
    },
    /// `c_namesize` exceeds [`MAX_NAME_SIZE`].
    NameTooLong {
        /// Archive offset where the header begins.
        offset: usize,
        /// The length the header claimed.
        size: u32,
    },
    /// The name runs past the end of the archive.
    TruncatedName {
        /// Archive offset where the header begins.
        offset: usize,
    },
    /// The last byte of the name is not the NUL that `c_namesize` counts.
    UnterminatedName {
        /// Archive offset where the header begins.
        offset: usize,
    },
    /// The name holds a NUL before its terminator, so the length and the string
    /// disagree about where the name ends.
    InteriorNul {
        /// Archive offset where the header begins.
        offset: usize,
    },
    /// The name is not valid UTF-8.
    NameNotUtf8 {
        /// Archive offset where the header begins.
        offset: usize,
    },
    /// The file data runs past the end of the archive.
    TruncatedData {
        /// Archive offset where the header begins.
        offset: usize,
    },
    /// A length in the header would push an offset past the end of the address
    /// space. No real archive does this, and the arithmetic must not wrap
    /// through it into a slice that looks in bounds.
    OffsetOverflow {
        /// Archive offset where the header begins.
        offset: usize,
    },
    /// The archive ended without a trailer entry, so it is a prefix of an
    /// archive rather than an archive.
    MissingTrailer,
    /// More than [`MAX_ENTRIES`] entries were walked without reaching a
    /// trailer.
    TooManyEntries,
}

impl fmt::Display for CpioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CpioError::TruncatedHeader { offset } => {
                write!(f, "header at {offset} is cut short")
            }
            CpioError::BadMagic { offset } => {
                write!(f, "header at {offset} is not a newc header")
            }
            CpioError::BadHexField { offset, field } => {
                write!(f, "{field} in the header at {offset} is not hexadecimal")
            }
            CpioError::EmptyName { offset } => {
                write!(f, "entry at {offset} has a zero-length name")
            }
            CpioError::NameTooLong { offset, size } => {
                write!(f, "entry at {offset} claims a {size}-byte name")
            }
            CpioError::TruncatedName { offset } => {
                write!(f, "name of the entry at {offset} runs past the archive")
            }
            CpioError::UnterminatedName { offset } => {
                write!(f, "name of the entry at {offset} is not NUL-terminated")
            }
            CpioError::InteriorNul { offset } => {
                write!(f, "name of the entry at {offset} holds an interior NUL")
            }
            CpioError::NameNotUtf8 { offset } => {
                write!(f, "name of the entry at {offset} is not UTF-8")
            }
            CpioError::TruncatedData { offset } => {
                write!(f, "data of the entry at {offset} runs past the archive")
            }
            CpioError::OffsetOverflow { offset } => {
                write!(f, "lengths in the header at {offset} overflow an offset")
            }
            CpioError::MissingTrailer => f.write_str("archive ends without a trailer entry"),
            CpioError::TooManyEntries => f.write_str("archive holds implausibly many entries"),
        }
    }
}

// ---------------------------------------------------------------------------
// Entries
// ---------------------------------------------------------------------------

/// What an entry is, decoded from the type bits of `c_mode`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileType {
    /// A regular file. [`Entry::data`] is its contents.
    Regular,
    /// A directory. It carries no data of its own.
    Directory,
    /// A symbolic link. [`Entry::data`] is the target path, without a NUL;
    /// [`Entry::symlink_target`] decodes it.
    Symlink,
    /// A character device node. Its numbers are in [`Entry::rdev_major`] and
    /// [`Entry::rdev_minor`].
    CharDevice,
    /// A block device node. Its numbers are in [`Entry::rdev_major`] and
    /// [`Entry::rdev_minor`].
    BlockDevice,
    /// A named pipe.
    Fifo,
    /// A socket. An archive should not contain one, but the mode can say so.
    Socket,
    /// A type this reader does not recognise, carrying the raw `c_mode & S_IFMT`
    /// bits so the caller can refuse it by number.
    Unknown(u32),
}

impl FileType {
    /// Decode the type bits of a `c_mode` value.
    #[must_use]
    pub const fn from_mode(mode: u32) -> FileType {
        match mode & S_IFMT {
            S_IFREG => FileType::Regular,
            S_IFDIR => FileType::Directory,
            S_IFLNK => FileType::Symlink,
            S_IFCHR => FileType::CharDevice,
            S_IFBLK => FileType::BlockDevice,
            S_IFIFO => FileType::Fifo,
            S_IFSOCK => FileType::Socket,
            other => FileType::Unknown(other),
        }
    }

    /// True for a character or block device node, the two types for which
    /// `c_rdevmajor` and `c_rdevminor` mean anything.
    #[must_use]
    pub const fn is_device(self) -> bool {
        matches!(self, FileType::CharDevice | FileType::BlockDevice)
    }
}

/// One archive entry, borrowed from the archive bytes.
///
/// The trailer is not an entry: iteration stops at it and never yields it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Entry<'a> {
    /// The name, with its NUL terminator stripped. It is the path relative to
    /// the root of the unpacked filesystem, and it is whatever the archive said
    /// — run it past [`Entry::is_safe_path`] before joining it to anything.
    pub name: &'a str,
    /// `c_ino`. Two entries sharing an inode number are hard links to one file.
    pub ino: u32,
    /// `c_mode`: type bits and permission bits together.
    pub mode: u32,
    /// `c_uid`, the owning user.
    pub uid: u32,
    /// `c_gid`, the owning group.
    pub gid: u32,
    /// `c_nlink`, the number of names the file has across the archive.
    pub nlink: u32,
    /// `c_mtime`, seconds since the Unix epoch.
    pub mtime: u32,
    /// `c_devmajor` of the device the file was archived from. Of no use when
    /// unpacking; kept because it pairs with `c_ino` to identify a file.
    pub dev_major: u32,
    /// `c_devminor` of the device the file was archived from.
    pub dev_minor: u32,
    /// `c_rdevmajor`: the major number a device node refers to. Zero for
    /// anything that is not a device node.
    pub rdev_major: u32,
    /// `c_rdevminor`: the minor number a device node refers to. Zero for
    /// anything that is not a device node.
    pub rdev_minor: u32,
    /// `c_check`: a checksum in a `070702` archive, zero in a `070701` one.
    /// This reader reports it and does not verify it.
    pub check: u32,
    /// The file data, borrowed from the archive. Empty for a directory, the
    /// link target for a symbolic link, and empty for every hard link to a file
    /// except the one entry that carries the contents.
    pub data: &'a [u8],
    /// Archive offset of this entry's header, for diagnostics.
    pub header_offset: usize,
}

impl<'a> Entry<'a> {
    /// What this entry is.
    #[must_use]
    pub const fn file_type(&self) -> FileType {
        FileType::from_mode(self.mode)
    }

    /// The permission and set-id bits, with the type bits masked off.
    #[must_use]
    pub const fn permissions(&self) -> u32 {
        self.mode & PERMISSION_MASK
    }

    /// True if this entry is a directory.
    #[must_use]
    pub const fn is_dir(&self) -> bool {
        matches!(self.file_type(), FileType::Directory)
    }

    /// True if this entry is a regular file.
    #[must_use]
    pub const fn is_file(&self) -> bool {
        matches!(self.file_type(), FileType::Regular)
    }

    /// True if this entry is a symbolic link.
    #[must_use]
    pub const fn is_symlink(&self) -> bool {
        matches!(self.file_type(), FileType::Symlink)
    }

    /// The device numbers this entry refers to, or `None` if it is not a device
    /// node.
    ///
    /// The fields are readable directly; this exists so that a caller cannot
    /// mistake the zeros a regular file carries for device 0:0.
    #[must_use]
    pub const fn rdev(&self) -> Option<(u32, u32)> {
        if self.file_type().is_device() {
            Some((self.rdev_major, self.rdev_minor))
        } else {
            None
        }
    }

    /// The target of a symbolic link.
    ///
    /// `None` if this is not a symbolic link or if its target is not UTF-8. The
    /// target is stored as the file data, without a terminator, so a zero-length
    /// data field yields an empty string rather than an error.
    #[must_use]
    pub fn symlink_target(&self) -> Option<&'a str> {
        if self.is_symlink() {
            core::str::from_utf8(self.data).ok()
        } else {
            None
        }
    }

    /// True if the name can be joined to the unpack root without leaving it.
    ///
    /// See [`is_safe_path`].
    #[must_use]
    pub fn is_safe_path(&self) -> bool {
        is_safe_path(self.name)
    }
}

/// True if `name` can be joined to the unpack root without escaping it.
///
/// An initramfs is generated by the build, but it is still a file, and an entry
/// named `../../etc/passwd` unpacked naively writes outside the ram filesystem
/// the kernel just created. Rejected are the empty name, any absolute path, and
/// any path with a `..` component; a NUL anywhere is rejected too, since a name
/// that reaches a C interface would be cut short there and mean a different
/// path than the one that was checked. A `.` component is accepted, because the
/// archives GNU cpio produces name the root itself `.`.
#[must_use]
pub fn is_safe_path(name: &str) -> bool {
    if name.is_empty() || name.starts_with('/') {
        return false;
    }
    if name.as_bytes().contains(&0) {
        return false;
    }
    !name.split('/').any(|component| component == "..")
}

/// Totals for an archive, from one pass over its headers.
///
/// The kernel reads this before unpacking anything, so that the ram filesystem
/// is sized once from the real numbers rather than grown entry by entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Summary {
    /// Number of entries, not counting the trailer.
    pub entries: usize,
    /// Total bytes of file data. Hard links count once, because every link but
    /// the one carrying the contents has a zero `c_filesize`.
    pub data_bytes: u64,
    /// Total bytes of names, counting each NUL terminator, which is what the
    /// directory entries of the unpacked filesystem will cost.
    pub name_bytes: u64,
    /// How many of the entries are directories.
    pub directories: usize,
    /// How many of the entries are regular files.
    pub regular_files: usize,
    /// How many of the entries are symbolic links.
    pub symlinks: usize,
    /// Size of the largest single file, so a caller can reject an archive whose
    /// biggest member will not fit before it starts copying.
    pub largest_file: u64,
}

// ---------------------------------------------------------------------------
// Archive
// ---------------------------------------------------------------------------

/// A newc archive, borrowed.
///
/// Construction never fails and never reads anything: an archive is only as
/// valid as the entry currently being parsed, and holding a `Archive` says
/// nothing about the bytes behind it. Call [`Archive::summary`] to validate the
/// whole thing in one pass before unpacking.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Archive<'a> {
    /// The whole archive. Padding is defined relative to the start of this
    /// slice, so it must begin at the first header.
    bytes: &'a [u8],
}

impl<'a> Archive<'a> {
    /// Wrap the bytes of an archive.
    #[must_use]
    pub const fn new(bytes: &'a [u8]) -> Archive<'a> {
        Archive { bytes }
    }

    /// The bytes this archive was built from.
    #[must_use]
    pub const fn as_bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// Iterate the entries, stopping at the trailer.
    #[must_use]
    pub const fn entries(&self) -> Entries<'a> {
        Entries {
            bytes: self.bytes,
            offset: 0,
            seen: 0,
            done: false,
        }
    }

    /// Walk the whole archive and total it up.
    ///
    /// Fails on the first malformed entry, so a successful summary is also a
    /// guarantee that iterating the archive will not produce an error.
    pub fn summary(&self) -> Result<Summary, CpioError> {
        let mut summary = Summary::default();
        for entry in self.entries() {
            let entry = entry?;
            let size = entry.data.len() as u64;
            let name = entry.name.len() as u64;

            summary.entries = summary.entries.saturating_add(1);
            summary.data_bytes = summary.data_bytes.saturating_add(size);
            summary.name_bytes = summary.name_bytes.saturating_add(name).saturating_add(1);
            if size > summary.largest_file {
                summary.largest_file = size;
            }
            match entry.file_type() {
                FileType::Directory => summary.directories = summary.directories.saturating_add(1),
                FileType::Regular => {
                    summary.regular_files = summary.regular_files.saturating_add(1);
                }
                FileType::Symlink => summary.symlinks = summary.symlinks.saturating_add(1),
                _ => {}
            }
        }
        Ok(summary)
    }

    /// The first entry with this name, or `None` if the archive holds no such
    /// entry.
    ///
    /// Linear, because an initramfs is unpacked in one pass and there is no
    /// index to build one from.
    pub fn find(&self, name: &str) -> Result<Option<Entry<'a>>, CpioError> {
        for entry in self.entries() {
            let entry = entry?;
            if entry.name == name {
                return Ok(Some(entry));
            }
        }
        Ok(None)
    }
}

/// Iterator over the entries of an archive.
///
/// Yields `Err` at most once: an archive that failed to parse at some offset
/// cannot be resynchronised, since every following offset was derived from the
/// field that just turned out to be wrong. The iterator stops after an error
/// rather than guessing.
#[derive(Clone, Copy, Debug)]
pub struct Entries<'a> {
    /// The whole archive, because padding is relative to its start.
    bytes: &'a [u8],
    /// Offset of the next header.
    offset: usize,
    /// Entries yielded so far, checked against [`MAX_ENTRIES`].
    seen: usize,
    /// Set once the trailer or an error has been reached.
    done: bool,
}

impl<'a> Entries<'a> {
    /// Offset of the header the next call will read.
    #[must_use]
    pub const fn offset(&self) -> usize {
        self.offset
    }

    /// Parse one entry, or report the trailer as `Ok(None)`.
    fn step(&mut self) -> Result<Option<Entry<'a>>, CpioError> {
        if self.seen >= MAX_ENTRIES {
            return Err(CpioError::TooManyEntries);
        }
        let parsed = parse_entry(self.bytes, self.offset)?;
        if parsed.entry.name == TRAILER_NAME {
            return Ok(None);
        }
        self.offset = parsed.next;
        self.seen = self.seen.saturating_add(1);
        Ok(Some(parsed.entry))
    }
}

impl<'a> Iterator for Entries<'a> {
    type Item = Result<Entry<'a>, CpioError>;

    fn next(&mut self) -> Option<Result<Entry<'a>, CpioError>> {
        if self.done {
            return None;
        }
        match self.step() {
            Ok(Some(entry)) => Some(Ok(entry)),
            Ok(None) => {
                self.done = true;
                None
            }
            Err(error) => {
                self.done = true;
                Some(Err(error))
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        // Every entry costs at least a header, which is the only bound
        // available without parsing the rest of the archive.
        let left = self.bytes.len().saturating_sub(self.offset) / HEADER_SIZE;
        (0, Some(left))
    }
}

impl core::iter::FusedIterator for Entries<'_> {}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// One parsed entry together with the offset the next header starts at.
#[derive(Clone, Copy, Debug)]
struct Parsed<'a> {
    /// The entry itself.
    entry: Entry<'a>,
    /// Offset of the following header, already rounded up to the 4-byte
    /// boundary the format aligns file data to.
    next: usize,
}

/// The thirteen numeric header fields, decoded.
#[derive(Clone, Copy, Debug)]
struct RawHeader {
    /// `c_ino`.
    ino: u32,
    /// `c_mode`.
    mode: u32,
    /// `c_uid`.
    uid: u32,
    /// `c_gid`.
    gid: u32,
    /// `c_nlink`.
    nlink: u32,
    /// `c_mtime`.
    mtime: u32,
    /// `c_filesize`.
    filesize: u32,
    /// `c_devmajor`.
    dev_major: u32,
    /// `c_devminor`.
    dev_minor: u32,
    /// `c_rdevmajor`.
    rdev_major: u32,
    /// `c_rdevminor`.
    rdev_minor: u32,
    /// `c_namesize`, including the trailing NUL.
    namesize: u32,
    /// `c_check`.
    check: u32,
}

/// Round `value` up to the next multiple of four, or `None` if that would wrap.
const fn align4(value: usize) -> Option<usize> {
    match value.checked_add(3) {
        Some(sum) => Some(sum & !3),
        None => None,
    }
}

/// The value of one ASCII hex digit, or `None` for anything else.
///
/// Both cases are accepted: the format calls for hexadecimal without saying
/// which, and archives in the wild carry both.
const fn hex_digit(byte: u8) -> Option<u32> {
    match byte {
        b'0'..=b'9' => Some((byte - b'0') as u32),
        b'a'..=b'f' => Some((byte - b'a') as u32 + 10),
        b'A'..=b'F' => Some((byte - b'A') as u32 + 10),
        _ => None,
    }
}

/// Decode the eight-character hex field at `at`.
///
/// `None` for a short header or for any character that is not a hex digit. A
/// field is never partially decoded: treating a stray character as a zero is
/// how a garbage header turns into an entry with a plausible size, which is far
/// worse than a rejected archive.
fn hex_field(header: &[u8], at: usize) -> Option<u32> {
    let field = header.get(at..at.checked_add(FIELD_LEN)?)?;
    let mut value: u32 = 0;
    for &byte in field {
        // Eight digits are exactly 32 bits, so this cannot overflow on a
        // well-formed field; the checked forms hold if the width ever changes.
        value = value.checked_mul(16)?.checked_add(hex_digit(byte)?)?;
    }
    Some(value)
}

/// Decode the numeric fields of the header beginning at `offset`.
fn parse_header(header: &[u8], offset: usize) -> Result<RawHeader, CpioError> {
    let magic = header
        .get(..MAGIC_LEN)
        .ok_or(CpioError::TruncatedHeader { offset })?;
    if magic != MAGIC_NEWC && magic != MAGIC_CRC {
        return Err(CpioError::BadMagic { offset });
    }

    let field = |at: usize, which: Field| -> Result<u32, CpioError> {
        hex_field(header, at).ok_or(CpioError::BadHexField {
            offset,
            field: which,
        })
    };

    Ok(RawHeader {
        ino: field(OFF_INO, Field::Ino)?,
        mode: field(OFF_MODE, Field::Mode)?,
        uid: field(OFF_UID, Field::Uid)?,
        gid: field(OFF_GID, Field::Gid)?,
        nlink: field(OFF_NLINK, Field::Nlink)?,
        mtime: field(OFF_MTIME, Field::Mtime)?,
        filesize: field(OFF_FILESIZE, Field::FileSize)?,
        dev_major: field(OFF_DEVMAJOR, Field::DevMajor)?,
        dev_minor: field(OFF_DEVMINOR, Field::DevMinor)?,
        rdev_major: field(OFF_RDEVMAJOR, Field::RdevMajor)?,
        rdev_minor: field(OFF_RDEVMINOR, Field::RdevMinor)?,
        namesize: field(OFF_NAMESIZE, Field::NameSize)?,
        check: field(OFF_CHECK, Field::Check)?,
    })
}

/// Turn the `c_namesize` bytes of a name field into a string.
fn decode_name(field: &[u8], offset: usize) -> Result<&str, CpioError> {
    let (last, head) = field.split_last().ok_or(CpioError::EmptyName { offset })?;
    if *last != 0 {
        return Err(CpioError::UnterminatedName { offset });
    }
    // A NUL before the terminator means the length and the string disagree
    // about where the name ends, and the shorter of the two is what a C caller
    // would see. Rejecting it keeps the checked name and the used name equal.
    if head.contains(&0) {
        return Err(CpioError::InteriorNul { offset });
    }
    core::str::from_utf8(head).map_err(|_| CpioError::NameNotUtf8 { offset })
}

/// Parse the entry whose header begins at `offset`.
///
/// Every length in the header is added with a checked operation and every slice
/// is taken with `get`, so a header claiming absurd sizes produces an error and
/// never a read past the end of `bytes`.
fn parse_entry(bytes: &[u8], offset: usize) -> Result<Parsed<'_>, CpioError> {
    let rest = bytes.get(offset..).unwrap_or(&[]);
    // Running out exactly at an entry boundary is the shape of an archive whose
    // producer forgot the trailer, and is worth distinguishing from a header
    // that was cut in half by a short read.
    if rest.is_empty() {
        return Err(CpioError::MissingTrailer);
    }
    let header = rest
        .get(..HEADER_SIZE)
        .ok_or(CpioError::TruncatedHeader { offset })?;
    let raw = parse_header(header, offset)?;

    if raw.namesize == 0 {
        return Err(CpioError::EmptyName { offset });
    }
    if raw.namesize > MAX_NAME_SIZE {
        return Err(CpioError::NameTooLong {
            offset,
            size: raw.namesize,
        });
    }

    let overflow = CpioError::OffsetOverflow { offset };
    let namesize = usize::try_from(raw.namesize).map_err(|_| overflow)?;
    let filesize = usize::try_from(raw.filesize).map_err(|_| overflow)?;

    let name_start = offset.checked_add(HEADER_SIZE).ok_or(overflow)?;
    let name_end = name_start.checked_add(namesize).ok_or(overflow)?;
    let name_field = bytes
        .get(name_start..name_end)
        .ok_or(CpioError::TruncatedName { offset })?;
    let name = decode_name(name_field, offset)?;

    // The data is aligned against the start of the archive, not against the
    // start of this entry, which is why the parser carries absolute offsets.
    let data_start = align4(name_end).ok_or(overflow)?;
    let data_end = data_start.checked_add(filesize).ok_or(overflow)?;
    let data = bytes
        .get(data_start..data_end)
        .ok_or(CpioError::TruncatedData { offset })?;

    Ok(Parsed {
        entry: Entry {
            name,
            ino: raw.ino,
            mode: raw.mode,
            uid: raw.uid,
            gid: raw.gid,
            nlink: raw.nlink,
            mtime: raw.mtime,
            dev_major: raw.dev_major,
            dev_minor: raw.dev_minor,
            rdev_major: raw.rdev_major,
            rdev_minor: raw.rdev_minor,
            check: raw.check,
            data,
            header_offset: offset,
        },
        next: align4(data_end).ok_or(overflow)?,
    })
}

#[cfg(test)]
mod tests;
