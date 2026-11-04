//! The messages.
//!
//! A request is [`REQUEST_HEADER`] bytes and then up to two names; an answer is
//! always [`ANSWER_BYTES`]. Nothing here trusts a length it did not compare.

use ferrix_linux_abi::errno::Errno;
use ferrix_vfs::{FileType, Metadata, SetAttributes, StatFs, Timespec};

/// Bytes of a request before its names.
pub const REQUEST_HEADER: usize = 40;
/// The longest name, as Linux's `NAME_MAX`.
pub const NAME_MAX: usize = 255;
/// The most bytes of a request.
pub const REQUEST_MAX: usize = REQUEST_HEADER + 2 * NAME_MAX;
/// Bytes of an answer: the status, the node, the value, the end and the attributes.
pub const ANSWER_BYTES: usize = 24 + ATTR_BYTES;
/// Bytes of the attributes in an answer.
pub const ATTR_BYTES: usize = 112;
/// Bytes of the VMO a channel shares, its one request's slot.
pub const SLOT_BYTES: usize = 256 * 1024;
/// The bytes of a [`Op::Hello`], which is a whole request.
pub const FIRST_NODE: u32 = 1;
/// The most live nodes of one channel; past it a lookup answers `ENFILE`.
pub const MAX_NODES: usize = 65_536;

/// What a request asks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum Op {
    /// Control channel, first request: the handle is its slot's VMO; `a` and `b` the
    /// wall clock's seconds and nanoseconds, which a writable mount's times are read
    /// from. The answer carries the events channel's far end.
    Hello = 1,
    /// Control channel: this disk's volume. Its name is the disk's; `a` is 1 for
    /// writable; `b` is its `st_dev`; the handles are the new channel's slot VMO and
    /// its server's end. The answer's attributes are the root's.
    Open = 2,
    /// Control channel: the volume's label on this disk, in the slot.
    Label = 3,
    /// Control channel: `b` sectors from sector `a` of the named disk, in the slot.
    BlockRead = 4,
    /// Control channel: `b` sectors at sector `a` of the named disk, from the slot.
    BlockWrite = 5,
    /// Control channel: the named disk's flush.
    BlockFlush = 6,
    /// A volume's channel: the client has unmounted. Every node goes, and with the
    /// last channel of a disk the mount: a later open reads the disk afresh.
    Close = 7,
    /// What the name is in this directory.
    Lookup = 16,
    /// The client forgets a node.
    Forget = 17,
    /// A node's attributes.
    GetAttr = 18,
    /// `a`, the slot's [`SetAttributes`].
    SetAttr = 19,
    /// `b` bytes from `a`, into the slot.
    Read = 20,
    /// `b` bytes at `a`, from the slot; `c` is 1 for append.
    Write = 21,
    /// A directory's entries from cursor `a`, in the slot.
    ReadDir = 22,
    /// A new node named, `c` its kind's mode bits, `a` its permissions, `b` its
    /// device, and the slot a symbolic link's target of `d` bytes.
    Create = 23,
    /// In this directory, the name for node `a`.
    Link = 24,
    /// In this directory, the name's end.
    Unlink = 25,
    /// In this directory, the empty directory's end.
    Rmdir = 26,
    /// This directory's first name, to node `a`'s second name, replacing if `c`.
    Rename = 27,
    /// A symbolic link's target, in the slot.
    ReadLink = 28,
    /// The file's length is `a`.
    SetLen = 29,
    /// Grow the file to at least `a`.
    GrowTo = 30,
    /// The file's bytes durable; `c` is 1 for data only.
    Fsync = 31,
    /// The volume's bytes durable.
    Sync = 32,
    /// The volume's numbers, in the slot.
    StatFs = 33,
}

impl Op {
    /// The op this word names; `None` for anything else.
    #[must_use]
    pub const fn from_raw(raw: u32) -> Option<Op> {
        Some(match raw {
            1 => Op::Hello,
            2 => Op::Open,
            3 => Op::Label,
            4 => Op::BlockRead,
            5 => Op::BlockWrite,
            6 => Op::BlockFlush,
            7 => Op::Close,
            16 => Op::Lookup,
            17 => Op::Forget,
            18 => Op::GetAttr,
            19 => Op::SetAttr,
            20 => Op::Read,
            21 => Op::Write,
            22 => Op::ReadDir,
            23 => Op::Create,
            24 => Op::Link,
            25 => Op::Unlink,
            26 => Op::Rmdir,
            27 => Op::Rename,
            28 => Op::ReadLink,
            29 => Op::SetLen,
            30 => Op::GrowTo,
            31 => Op::Fsync,
            32 => Op::Sync,
            33 => Op::StatFs,
            _ => return None,
        })
    }
}

/// A request, its names borrowed from the message.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Request<'a> {
    /// What it asks.
    pub op: Op,
    /// The node it asks of.
    pub node: u32,
    /// The first argument.
    pub a: u64,
    /// The second.
    pub b: u64,
    /// The third.
    pub c: u32,
    /// The fourth.
    pub d: u32,
    /// The first name; empty for none.
    pub name: &'a [u8],
    /// The second name; empty for none.
    pub second: &'a [u8],
}

/// Why bytes are no request. All of them answer `EINVAL`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Malformed {
    /// Shorter than its header or than its lengths say, or longer.
    Length,
    /// An op this version does not have.
    Op,
    /// A name past [`NAME_MAX`], or holding `/` or NUL, or `.` or `..`.
    Name,
}

fn word(bytes: &[u8], at: usize) -> u32 {
    bytes
        .get(at..at + 4)
        .and_then(|four| <[u8; 4]>::try_from(four).ok())
        .map_or(0, u32::from_le_bytes)
}

fn long(bytes: &[u8], at: usize) -> u64 {
    bytes
        .get(at..at + 8)
        .and_then(|eight| <[u8; 8]>::try_from(eight).ok())
        .map_or(0, u64::from_le_bytes)
}

fn put(out: &mut [u8], at: usize, bytes: &[u8]) {
    if let Some(place) = out.get_mut(at..at + bytes.len()) {
        place.copy_from_slice(bytes);
    }
}

/// Whether `name` can be one name in a directory. A disk's name passes too.
#[must_use]
pub fn name_is_good(name: &[u8]) -> bool {
    name.len() <= NAME_MAX
        && !name.iter().any(|&byte| byte == b'/' || byte == 0)
        && name != b"."
        && name != b".."
}

impl<'a> Request<'a> {
    /// A request with its arguments at nothing.
    #[must_use]
    pub const fn new(op: Op, node: u32) -> Request<'a> {
        Request {
            op,
            node,
            a: 0,
            b: 0,
            c: 0,
            d: 0,
            name: &[],
            second: &[],
        }
    }

    /// Read `bytes`, every length compared.
    ///
    /// # Errors
    ///
    /// [`Malformed`].
    pub fn decode(bytes: &'a [u8]) -> Result<Request<'a>, Malformed> {
        let header = bytes.get(..REQUEST_HEADER).ok_or(Malformed::Length)?;
        let op = Op::from_raw(word(header, 0)).ok_or(Malformed::Op)?;
        let first_len = (word(header, 32) & 0xFFFF) as usize;
        let second_len = (word(header, 32) >> 16) as usize;
        if first_len > NAME_MAX || second_len > NAME_MAX {
            return Err(Malformed::Name);
        }
        if bytes.len() != REQUEST_HEADER + first_len + second_len {
            return Err(Malformed::Length);
        }
        let name = bytes
            .get(REQUEST_HEADER..REQUEST_HEADER + first_len)
            .ok_or(Malformed::Length)?;
        let second = bytes
            .get(REQUEST_HEADER + first_len..)
            .ok_or(Malformed::Length)?;
        if !name_is_good(name) || !name_is_good(second) {
            return Err(Malformed::Name);
        }
        if word(header, 36) != 0 {
            return Err(Malformed::Length);
        }
        Ok(Request {
            op,
            node: word(header, 4),
            a: long(header, 8),
            b: long(header, 16),
            c: word(header, 24),
            d: word(header, 28),
            name,
            second,
        })
    }

    /// Write this into `out`, answering how many bytes; `None` for a name past
    /// [`NAME_MAX`] or an `out` too short.
    pub fn encode(&self, out: &mut [u8; REQUEST_MAX]) -> Option<usize> {
        if self.name.len() > NAME_MAX || self.second.len() > NAME_MAX {
            return None;
        }
        put(out, 0, &(self.op as u32).to_le_bytes());
        put(out, 4, &self.node.to_le_bytes());
        put(out, 8, &self.a.to_le_bytes());
        put(out, 16, &self.b.to_le_bytes());
        put(out, 24, &self.c.to_le_bytes());
        put(out, 28, &self.d.to_le_bytes());
        let lengths = self.name.len() as u32 | (self.second.len() as u32) << 16;
        put(out, 32, &lengths.to_le_bytes());
        put(out, 36, &[0; 4]);
        put(out, REQUEST_HEADER, self.name);
        put(out, REQUEST_HEADER + self.name.len(), self.second);
        Some(REQUEST_HEADER + self.name.len() + self.second.len())
    }
}

/// An answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Answer {
    /// Zero, or the positive errno.
    pub status: u32,
    /// A new node, or 0.
    pub node: u32,
    /// A length, or what the op says.
    pub value: u64,
    /// A write's offset just past it.
    pub end: u64,
    /// A node's attributes, for the ops that answer them.
    pub attr: Option<Metadata>,
}

impl Answer {
    /// Fine, and nothing to say.
    pub const OK: Answer = Answer {
        status: 0,
        node: 0,
        value: 0,
        end: 0,
        attr: None,
    };

    /// The refusal with this errno.
    #[must_use]
    pub const fn refused(errno: Errno) -> Answer {
        Answer {
            status: errno.0 as u32,
            node: 0,
            value: 0,
            end: 0,
            attr: None,
        }
    }

    /// `value`, with nothing else.
    #[must_use]
    pub const fn value(value: u64) -> Answer {
        Answer {
            status: 0,
            node: 0,
            value,
            end: 0,
            attr: None,
        }
    }

    /// As the channel carries it.
    #[must_use]
    pub fn encode(&self) -> [u8; ANSWER_BYTES] {
        let mut out = [0; ANSWER_BYTES];
        put(&mut out, 0, &self.status.to_le_bytes());
        put(&mut out, 4, &self.node.to_le_bytes());
        put(&mut out, 8, &self.value.to_le_bytes());
        put(&mut out, 16, &self.end.to_le_bytes());
        if let Some(attr) = self.attr {
            encode_attr(&attr, &mut out[24..]);
        }
        out
    }

    /// Read an answer. The client trusts its server for its volume's contents;
    /// a malformed form is still `None`, never a guess.
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Answer> {
        if bytes.len() != ANSWER_BYTES {
            return None;
        }
        Some(Answer {
            status: word(bytes, 0),
            node: word(bytes, 4),
            value: long(bytes, 8),
            end: long(bytes, 16),
            attr: decode_attr(bytes.get(24..)?),
        })
    }
}

fn encode_attr(attr: &Metadata, out: &mut [u8]) {
    put(out, 0, &attr.ino.to_le_bytes());
    put(out, 8, &attr.kind.mode_bits().to_le_bytes());
    put(out, 12, &attr.permissions.to_le_bytes());
    put(out, 16, &attr.nlink.to_le_bytes());
    put(out, 20, &attr.uid.to_le_bytes());
    put(out, 24, &attr.gid.to_le_bytes());
    put(out, 28, &attr.block_size.to_le_bytes());
    put(out, 32, &attr.size.to_le_bytes());
    put(out, 40, &attr.rdev.to_le_bytes());
    put(out, 48, &attr.blocks.to_le_bytes());
    for (index, time) in [attr.atime, attr.mtime, attr.ctime].iter().enumerate() {
        put(out, 56 + index * 16, &time.tv_sec.to_le_bytes());
        put(out, 64 + index * 16, &time.tv_nsec.to_le_bytes());
    }
}

fn decode_time(bytes: &[u8], at: usize) -> Timespec {
    Timespec {
        tv_sec: long(bytes, at) as i64,
        tv_nsec: long(bytes, at + 8) as i64,
    }
}

/// Attributes, or `None` where the kind's mode bits are none: an answer without.
fn decode_attr(bytes: &[u8]) -> Option<Metadata> {
    Some(Metadata {
        ino: long(bytes, 0),
        kind: FileType::from_mode(word(bytes, 8))?,
        permissions: word(bytes, 12),
        nlink: word(bytes, 16),
        uid: word(bytes, 20),
        gid: word(bytes, 24),
        block_size: word(bytes, 28),
        size: long(bytes, 32),
        rdev: long(bytes, 40),
        blocks: long(bytes, 48),
        atime: decode_time(bytes, 56),
        mtime: decode_time(bytes, 72),
        ctime: decode_time(bytes, 88),
    })
}

/// Bytes of [`SetAttributes`] in a slot.
pub const SET_ATTR_BYTES: usize = 64;

/// `change` as it goes in a slot: a word of which fields, then each.
#[must_use]
pub fn encode_set_attr(change: &SetAttributes) -> [u8; SET_ATTR_BYTES] {
    let mut out = [0; SET_ATTR_BYTES];
    let mut present = 0_u32;
    if let Some(value) = change.permissions {
        present |= 1;
        put(&mut out, 4, &value.to_le_bytes());
    }
    if let Some(value) = change.uid {
        present |= 2;
        put(&mut out, 8, &value.to_le_bytes());
    }
    if let Some(value) = change.gid {
        present |= 4;
        put(&mut out, 12, &value.to_le_bytes());
    }
    if let Some(time) = change.atime {
        present |= 8;
        put(&mut out, 16, &time.tv_sec.to_le_bytes());
        put(&mut out, 24, &time.tv_nsec.to_le_bytes());
    }
    if let Some(time) = change.mtime {
        present |= 16;
        put(&mut out, 32, &time.tv_sec.to_le_bytes());
        put(&mut out, 40, &time.tv_nsec.to_le_bytes());
    }
    put(&mut out, 0, &present.to_le_bytes());
    out
}

/// Read [`encode_set_attr`]'s form; `None` for a bit it does not write, or a
/// permission past the mode's.
#[must_use]
pub fn decode_set_attr(bytes: &[u8; SET_ATTR_BYTES]) -> Option<SetAttributes> {
    let present = word(bytes, 0);
    if present & !0b1_1111 != 0 {
        return None;
    }
    let permissions = (present & 1 != 0).then(|| word(bytes, 4));
    if permissions.is_some_and(|mode| mode > 0o7777) {
        return None;
    }
    Some(SetAttributes {
        permissions,
        uid: (present & 2 != 0).then(|| word(bytes, 8)),
        gid: (present & 4 != 0).then(|| word(bytes, 12)),
        atime: (present & 8 != 0).then(|| decode_time(bytes, 16)),
        mtime: (present & 16 != 0).then(|| decode_time(bytes, 32)),
    })
}

/// Bytes of [`StatFs`] in a slot.
pub const STATFS_BYTES: usize = 64;

/// `stats` as it goes in a slot.
#[must_use]
pub fn encode_statfs(stats: &StatFs) -> [u8; STATFS_BYTES] {
    let mut out = [0; STATFS_BYTES];
    for (index, value) in [
        stats.magic,
        stats.block_size,
        stats.blocks,
        stats.blocks_free,
        stats.blocks_available,
        stats.files,
        stats.files_free,
        stats.name_max,
    ]
    .iter()
    .enumerate()
    {
        put(&mut out, index * 8, &value.to_le_bytes());
    }
    out
}

/// Read [`encode_statfs`]'s form.
#[must_use]
pub fn decode_statfs(bytes: &[u8; STATFS_BYTES]) -> StatFs {
    StatFs {
        magic: long(bytes, 0),
        block_size: long(bytes, 8),
        blocks: long(bytes, 16),
        blocks_free: long(bytes, 24),
        blocks_available: long(bytes, 32),
        files: long(bytes, 40),
        files_free: long(bytes, 48),
        name_max: long(bytes, 56),
    }
}

/// The bytes of a directory entry's fixed part in a slot.
pub const DIRENT_HEADER: usize = 20;

/// One entry at `at` in `slot`: ino u64, next u64, the kind's mode's top byte and
/// the name's length, then the name. How many bytes it took; `None` if it does not
/// fit, which ends the answer.
pub fn encode_dirent(
    ino: u64,
    next: u64,
    kind: FileType,
    name: &[u8],
    out: &mut [u8],
) -> Option<usize> {
    let total = DIRENT_HEADER.checked_add(name.len())?;
    if name.len() > NAME_MAX || total > out.len() {
        return None;
    }
    put(out, 0, &ino.to_le_bytes());
    put(out, 8, &next.to_le_bytes());
    put(out, 16, &(kind.mode_bits() >> 12).to_le_bytes()[..2]);
    put(out, 18, &(name.len() as u16).to_le_bytes());
    put(out, DIRENT_HEADER, name);
    Some(total)
}

/// One entry from the front of `bytes`: ino, next, kind, name and what it took.
#[must_use]
pub fn decode_dirent(bytes: &[u8]) -> Option<(u64, u64, FileType, &[u8], usize)> {
    let header = bytes.get(..DIRENT_HEADER)?;
    let shifted = u32::from(u16::from_le_bytes([*header.get(16)?, *header.get(17)?]));
    let kind = FileType::from_mode(shifted << 12)?;
    let len = usize::from(u16::from_le_bytes([*header.get(18)?, *header.get(19)?]));
    if len > NAME_MAX {
        return None;
    }
    let name = bytes.get(DIRENT_HEADER..DIRENT_HEADER + len)?;
    Some((
        long(header, 0),
        long(header, 8),
        kind,
        name,
        DIRENT_HEADER + len,
    ))
}

/// A disk the server has: control channel's event from server to client.
pub const EVENT_BYTES: usize = 40;

/// What the events channel says of a disk.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DiskUp {
    /// Its name, NUL-padded.
    pub name: [u8; 8],
    /// Sectors.
    pub sectors: u64,
    /// Bytes of a sector.
    pub sector_size: u32,
    /// `ferrix_blkring`'s device flags.
    pub flags: u32,
    /// Where the device is: its location's word.
    pub location: u32,
    /// The driver's most sectors of a request.
    pub max_sectors: u32,
}

impl DiskUp {
    /// As the channel carries it.
    #[must_use]
    pub fn encode(&self) -> [u8; EVENT_BYTES] {
        let mut out = [0; EVENT_BYTES];
        put(&mut out, 0, &self.name);
        put(&mut out, 8, &self.sectors.to_le_bytes());
        put(&mut out, 16, &self.sector_size.to_le_bytes());
        put(&mut out, 20, &self.flags.to_le_bytes());
        put(&mut out, 24, &self.location.to_le_bytes());
        put(&mut out, 28, &self.max_sectors.to_le_bytes());
        out
    }

    /// Read it; `None` for any other length.
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<DiskUp> {
        if bytes.len() != EVENT_BYTES {
            return None;
        }
        let mut name = [0; 8];
        name.copy_from_slice(bytes.get(..8)?);
        Some(DiskUp {
            name,
            sectors: long(bytes, 8),
            sector_size: word(bytes, 16),
            flags: word(bytes, 20),
            location: word(bytes, 24),
            max_sectors: word(bytes, 28),
        })
    }
}
