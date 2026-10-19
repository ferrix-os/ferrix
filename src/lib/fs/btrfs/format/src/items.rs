//! Leaf item payloads, and the key types that select them.
//!
//! A leaf item is a key and an opaque blob; the key's `item_type` byte says how
//! to read the blob. This module covers the types the read path needs — inodes,
//! directory entries, backrefs, file extents, subvolume roots, devices and data
//! checksums — and nothing to do with allocation, backrefs into the extent tree
//! or free-space accounting.
//!
//! Several item types pack more than one record into a single payload, because
//! btrfs keys collide by design: two directory entries whose names hash to the
//! same value share a key and therefore share an item. Anything that can repeat
//! is exposed as an iterator that reports a malformed record rather than
//! stopping quietly.

use crate::tree::{BtrfsKey, KEY_SIZE};
use crate::{
    BtrfsError, array_at, crc32c::crc32c_update, slice_at, truncated, u8_at, u16_at, u32_at, u64_at,
};

// ---------------------------------------------------------------------------
// Key types
// ---------------------------------------------------------------------------

/// The stat data of a file or directory.
pub const INODE_ITEM_KEY: u8 = 1;
/// A back-reference from an inode to a directory that names it.
pub const INODE_REF_KEY: u8 = 12;
/// The extended form of `INODE_REF`, used when a directory grows too large for
/// all the refs to fit in one item.
pub const INODE_EXTREF_KEY: u8 = 13;
/// An extended attribute, stored with the same layout as a directory entry.
pub const XATTR_ITEM_KEY: u8 = 24;
/// A directory entry keyed by the hash of its name — the lookup path.
pub const DIR_ITEM_KEY: u8 = 84;
/// The same directory entry keyed by its position — the `readdir` path.
pub const DIR_INDEX_KEY: u8 = 96;
/// One extent of a file's contents.
pub const EXTENT_DATA_KEY: u8 = 108;
/// A run of data checksums.
pub const EXTENT_CSUM_KEY: u8 = 128;
/// The root of a subvolume or of one of the internal trees.
pub const ROOT_ITEM_KEY: u8 = 132;
/// A device belonging to the volume.
pub const DEV_ITEM_KEY: u8 = 216;
/// A chunk of the logical address space; see [`crate::chunk`].
pub const CHUNK_ITEM_KEY: u8 = 228;

/// The root tree, which holds a `ROOT_ITEM` for every other tree.
pub const ROOT_TREE_OBJECTID: u64 = 1;
/// The extent tree: what is allocated and who references it.
pub const EXTENT_TREE_OBJECTID: u64 = 2;
/// The chunk tree: the logical-to-physical map.
pub const CHUNK_TREE_OBJECTID: u64 = 3;
/// The device tree: which parts of each device are in use.
pub const DEV_TREE_OBJECTID: u64 = 4;
/// The top-level subvolume: the one mounted unless the root tree names another
/// as the default.
pub const FS_TREE_OBJECTID: u64 = 5;
/// The root tree's one directory. Its `default` entry names the default
/// subvolume.
pub const ROOT_TREE_DIR_OBJECTID: u64 = 6;
/// The checksum tree.
pub const CSUM_TREE_OBJECTID: u64 = 7;
/// The first object id a file may use; everything below is reserved.
pub const FIRST_FREE_OBJECTID: u64 = 256;
/// The last object id a file may use: Linux's `BTRFS_LAST_FREE_OBJECTID`,
/// `-256` as a `u64`. The ids above it are reserved for special objects.
pub const LAST_FREE_OBJECTID: u64 = 0u64.wrapping_sub(256);
/// Object id every `EXTENT_CSUM` item in the checksum tree is filed under:
/// Linux's `BTRFS_EXTENT_CSUM_OBJECTID`, `-10` as a `u64`.
pub const EXTENT_CSUM_OBJECTID: u64 = 0u64.wrapping_sub(10);

/// Inode flag: the file's data has no checksums, so none are looked up or
/// verified. Linux's `BTRFS_INODE_NODATASUM`, bit 0.
pub const INODE_NODATASUM: u64 = 1 << 0;
/// Object id under which `DEV_ITEM`s are filed in the chunk tree.
pub const DEV_ITEMS_OBJECTID: u64 = 1;

/// Mask selecting the file-type bits of an inode's `mode`.
pub const S_IFMT: u32 = 0xF000;
/// `mode` file type: FIFO.
pub const S_IFIFO: u32 = 0x1000;
/// `mode` file type: character device.
pub const S_IFCHR: u32 = 0x2000;
/// `mode` file type: directory.
pub const S_IFDIR: u32 = 0x4000;
/// `mode` file type: block device.
pub const S_IFBLK: u32 = 0x6000;
/// `mode` file type: regular file.
pub const S_IFREG: u32 = 0x8000;
/// `mode` file type: symbolic link.
pub const S_IFLNK: u32 = 0xA000;
/// `mode` file type: socket.
pub const S_IFSOCK: u32 = 0xC000;

/// Directory entry type: unknown.
pub const FT_UNKNOWN: u8 = 0;
/// Directory entry type: regular file.
pub const FT_REG_FILE: u8 = 1;
/// Directory entry type: directory.
pub const FT_DIR: u8 = 2;
/// Directory entry type: character device.
pub const FT_CHRDEV: u8 = 3;
/// Directory entry type: block device.
pub const FT_BLKDEV: u8 = 4;
/// Directory entry type: FIFO.
pub const FT_FIFO: u8 = 5;
/// Directory entry type: socket.
pub const FT_SOCK: u8 = 6;
/// Directory entry type: symbolic link.
pub const FT_SYMLINK: u8 = 7;
/// Not a directory entry at all: an extended attribute sharing the layout.
pub const FT_XATTR: u8 = 8;

/// Extent contents are stored verbatim.
pub const COMPRESS_NONE: u8 = 0;
/// Extent contents are zlib-compressed.
pub const COMPRESS_ZLIB: u8 = 1;
/// Extent contents are LZO-compressed.
pub const COMPRESS_LZO: u8 = 2;
/// Extent contents are zstd-compressed.
pub const COMPRESS_ZSTD: u8 = 3;

/// `EXTENT_DATA` type: the file contents live in the item itself.
pub const FILE_EXTENT_INLINE: u8 = 0;
/// `EXTENT_DATA` type: the file contents live in an allocated extent.
pub const FILE_EXTENT_REG: u8 = 1;
/// `EXTENT_DATA` type: an extent is allocated but has never been written, so it
/// reads as zeroes whatever the disk holds.
pub const FILE_EXTENT_PREALLOC: u8 = 2;

/// Bytes in an on-disk `INODE_ITEM`.
pub const INODE_ITEM_SIZE: usize = 160;

/// Bytes in a `DIR_ITEM` header, before the name and data.
pub const DIR_ITEM_HEADER_SIZE: usize = 30;

/// The longest name a directory entry or inode back-reference may carry:
/// Linux's `BTRFS_NAME_LEN`, and the 255 every Unix filesystem agrees on.
pub const NAME_LEN: usize = 255;

/// The longest extended attribute name: Linux's `XATTR_NAME_MAX`.
pub const XATTR_NAME_MAX: usize = 255;

/// Bytes in an `EXTENT_DATA` header, before the inline data or the extent
/// reference.
pub const EXTENT_DATA_HEADER_SIZE: usize = 21;

/// Bytes in a regular or preallocated `EXTENT_DATA` item: the header and four
/// `u64`s. Unlike an inline extent, its size is fixed.
pub const FILE_EXTENT_ITEM_SIZE: usize = 53;

/// Bytes in an on-disk `DEV_ITEM`.
pub const DEV_ITEM_SIZE: usize = 98;

// ---------------------------------------------------------------------------
// Name hashing
// ---------------------------------------------------------------------------

/// The hash btrfs uses as the `offset` of a `DIR_ITEM` key.
///
/// It is CRC-32C seeded with `!1` and, unlike a checksum, *not* complemented at
/// the end. That asymmetry is not decorative: it is why a directory lookup
/// cannot be built on the ordinary [`crate::crc32c::crc32c`] function, and
/// getting it wrong produces a hash that is right for no name at all, so every
/// lookup misses while `readdir` still works.
#[must_use]
pub fn name_hash(name: &[u8]) -> u64 {
    u64::from(crc32c_update(!1u32, name))
}

// ---------------------------------------------------------------------------
// Timestamps
// ---------------------------------------------------------------------------

/// A btrfs timestamp: seconds since the epoch and a nanosecond remainder.
///
/// Twelve bytes on disk, and `sec` is signed in the kernel's reading, so a
/// pre-1970 timestamp arrives here as a very large `u64`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Timespec {
    /// Whole seconds since the Unix epoch.
    pub sec: u64,
    /// Nanoseconds within the second.
    pub nsec: u32,
}

impl Timespec {
    /// Bytes a timestamp occupies on disk.
    pub const SIZE: usize = 12;

    /// Read a timestamp at `at`, or `None` past the end.
    #[must_use]
    pub fn parse(bytes: &[u8], at: usize) -> Option<Self> {
        Some(Timespec {
            sec: u64_at(bytes, at)?,
            nsec: u32_at(bytes, at.checked_add(8)?)?,
        })
    }
}

// ---------------------------------------------------------------------------
// INODE_ITEM
// ---------------------------------------------------------------------------

/// The stat data of one file, directory or device node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InodeItem {
    /// Transaction the inode was created in.
    pub generation: u64,
    /// Transaction the inode was last changed in.
    pub transid: u64,
    /// Logical file size in bytes. For a directory this is not a byte count but
    /// the summed length of its entry names.
    pub size: u64,
    /// Bytes actually allocated, which is smaller than `size` for a sparse or
    /// compressed file and larger for a preallocated one.
    pub nbytes: u64,
    /// Unused since the free-space cache moved out of the filesystem tree.
    pub block_group: u64,
    /// Hard link count.
    pub nlink: u32,
    /// Owning user id.
    pub uid: u32,
    /// Owning group id.
    pub gid: u32,
    /// File type and permission bits; see [`S_IFMT`].
    pub mode: u32,
    /// Device number, for a character or block device node.
    pub rdev: u64,
    /// Inode flags: `NODATASUM`, `NODATACOW`, `IMMUTABLE` and the rest.
    pub flags: u64,
    /// Sequence number bumped on every change, exported as the NFS change
    /// attribute.
    pub sequence: u64,
    /// Last access time.
    pub atime: Timespec,
    /// Last inode change time.
    pub ctime: Timespec,
    /// Last modification time.
    pub mtime: Timespec,
    /// Creation time, which btrfs records and POSIX does not.
    pub otime: Timespec,
}

impl InodeItem {
    /// Parse an `INODE_ITEM` payload.
    pub fn parse(bytes: &[u8]) -> Result<Self, BtrfsError> {
        let get = || {
            Some(InodeItem {
                generation: u64_at(bytes, 0)?,
                transid: u64_at(bytes, 8)?,
                size: u64_at(bytes, 16)?,
                nbytes: u64_at(bytes, 24)?,
                block_group: u64_at(bytes, 32)?,
                nlink: u32_at(bytes, 40)?,
                uid: u32_at(bytes, 44)?,
                gid: u32_at(bytes, 48)?,
                mode: u32_at(bytes, 52)?,
                rdev: u64_at(bytes, 56)?,
                flags: u64_at(bytes, 64)?,
                sequence: u64_at(bytes, 72)?,
                // Bytes 80..112 are four reserved `u64`s.
                atime: Timespec::parse(bytes, 112)?,
                ctime: Timespec::parse(bytes, 124)?,
                mtime: Timespec::parse(bytes, 136)?,
                otime: Timespec::parse(bytes, 148)?,
            })
        };
        get().ok_or_else(|| truncated(INODE_ITEM_SIZE, bytes.len()))
    }

    /// The file-type bits of `mode`.
    #[must_use]
    pub const fn file_type(&self) -> u32 {
        self.mode & S_IFMT
    }

    /// Whether this inode is a directory.
    #[must_use]
    pub const fn is_dir(&self) -> bool {
        self.file_type() == S_IFDIR
    }

    /// Whether this inode is a regular file.
    #[must_use]
    pub const fn is_file(&self) -> bool {
        self.file_type() == S_IFREG
    }

    /// Whether this inode is a symbolic link. The target is stored as the
    /// inode's file contents, normally in a single inline extent.
    #[must_use]
    pub const fn is_symlink(&self) -> bool {
        self.file_type() == S_IFLNK
    }
}

// ---------------------------------------------------------------------------
// INODE_REF
// ---------------------------------------------------------------------------

/// One back-reference: a name this inode is known by in one directory.
///
/// The directory is the key's `offset`, not part of the payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InodeRef<'a> {
    /// Position of the entry within that directory, matching the `DIR_INDEX`
    /// key offset.
    pub index: u64,
    /// The name. Raw bytes: btrfs stores whatever the caller passed and only
    /// forbids NUL and `/`, so it need not be UTF-8.
    pub name: &'a [u8],
}

/// Iterator over the back-references packed into one `INODE_REF` item.
///
/// A hard-linked file has one record per name in the same parent directory, so
/// the item repeats.
#[derive(Debug, Clone, Copy)]
pub struct InodeRefIter<'a> {
    bytes: &'a [u8],
    at: usize,
    done: bool,
}

impl<'a> InodeRefIter<'a> {
    /// Iterate the records in an `INODE_REF` payload.
    #[must_use]
    pub const fn new(bytes: &'a [u8]) -> Self {
        InodeRefIter {
            bytes,
            at: 0,
            done: false,
        }
    }

    /// Parse the record at the cursor, returning it and the next cursor.
    ///
    /// As Linux's `check_inode_ref`: a back-reference repeats the name of a
    /// directory entry, so its name is between one and [`NAME_LEN`] bytes. A
    /// longer one is not a name any directory could hold, and an empty one
    /// would let a payload of bare ten-byte headers pass as a list of links.
    fn record(&self) -> Option<(InodeRef<'a>, usize)> {
        let index = u64_at(self.bytes, self.at)?;
        let name_len = usize::from(u16_at(self.bytes, self.at.checked_add(8)?)?);
        if name_len == 0 || name_len > NAME_LEN {
            return None;
        }
        let name_at = self.at.checked_add(10)?;
        let name = slice_at(self.bytes, name_at, name_len)?;
        Some((InodeRef { index, name }, name_at.checked_add(name_len)?))
    }
}

impl<'a> Iterator for InodeRefIter<'a> {
    type Item = Result<InodeRef<'a>, BtrfsError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done || self.at >= self.bytes.len() {
            return None;
        }
        match self.record() {
            Some((entry, next)) => {
                self.at = next;
                Some(Ok(entry))
            }
            None => {
                self.done = true;
                Some(Err(BtrfsError::BadItem {
                    item_type: INODE_REF_KEY,
                }))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// INODE_EXTREF
// ---------------------------------------------------------------------------

/// The key offset an `INODE_EXTREF` item is filed under: Linux's
/// `btrfs_extref_hash`, `crc32c(parent_objectid, name, len)`.
///
/// The kernel's `crc32c` is the raw register, as in [`name_hash`]: seeded with
/// the parent's low 32 bits, since that is all a `u32` seed keeps, and not
/// complemented at the end.
#[must_use]
pub fn extref_hash(parent: u64, name: &[u8]) -> u64 {
    let low = parent.to_le_bytes();
    let seed = u32::from_le_bytes([low[0], low[1], low[2], low[3]]);
    u64::from(crc32c_update(seed, name))
}

/// One extended back-reference: a name this inode is known by, and the
/// directory that name is in.
///
/// Volumes with `EXTENDED_IREF` use it once an inode's `INODE_REF` for one
/// directory would no longer fit in a leaf, as with many hard links. Unlike an
/// [`InodeRef`], the directory is in the record, and the key's `offset` is
/// [`extref_hash`] of directory and name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InodeExtref<'a> {
    /// The directory holding the name.
    pub parent: u64,
    /// Position of the entry within that directory, matching the `DIR_INDEX`
    /// key offset.
    pub index: u64,
    /// The name, as raw bytes.
    pub name: &'a [u8],
}

/// Iterator over the records packed into one `INODE_EXTREF` item.
///
/// Names whose hashes collide share an item, so it may hold more than one.
#[derive(Debug, Clone, Copy)]
pub struct InodeExtrefIter<'a> {
    bytes: &'a [u8],
    at: usize,
    done: bool,
}

impl<'a> InodeExtrefIter<'a> {
    /// Iterate the records in an `INODE_EXTREF` payload from an fs tree.
    #[must_use]
    pub const fn new(bytes: &'a [u8]) -> Self {
        InodeExtrefIter {
            bytes,
            at: 0,
            done: false,
        }
    }

    /// Parse the record at the cursor, returning it and the next cursor.
    ///
    /// As Linux's `check_inode_extref` for a leaf of an fs tree: a parent from
    /// [`FIRST_FREE_OBJECTID`] to [`LAST_FREE_OBJECTID`], since only such ids
    /// are directories a file can be in, and a name between one and
    /// [`NAME_LEN`] bytes that fits in the payload.
    fn record(&self) -> Option<(InodeExtref<'a>, usize)> {
        let parent = u64_at(self.bytes, self.at)?;
        let index = u64_at(self.bytes, self.at.checked_add(8)?)?;
        let name_len = usize::from(u16_at(self.bytes, self.at.checked_add(16)?)?);
        if !(FIRST_FREE_OBJECTID..=LAST_FREE_OBJECTID).contains(&parent)
            || name_len == 0
            || name_len > NAME_LEN
        {
            return None;
        }
        let name_at = self.at.checked_add(18)?;
        let name = slice_at(self.bytes, name_at, name_len)?;
        Some((
            InodeExtref {
                parent,
                index,
                name,
            },
            name_at.checked_add(name_len)?,
        ))
    }
}

impl<'a> Iterator for InodeExtrefIter<'a> {
    type Item = Result<InodeExtref<'a>, BtrfsError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done || self.at >= self.bytes.len() {
            return None;
        }
        match self.record() {
            Some((entry, next)) => {
                self.at = next;
                Some(Ok(entry))
            }
            None => {
                self.done = true;
                Some(Err(BtrfsError::BadItem {
                    item_type: INODE_EXTREF_KEY,
                }))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// DIR_ITEM and DIR_INDEX
// ---------------------------------------------------------------------------

/// One directory entry, or one extended attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirItem<'a> {
    /// Key of what the entry points at: an `INODE_ITEM` key for a file in this
    /// subvolume, or a `ROOT_ITEM` key when the entry is a subvolume mount
    /// point — which is how a subvolume boundary appears while walking a path.
    pub location: BtrfsKey,
    /// Transaction the entry was created in.
    pub transid: u64,
    /// Entry type; see [`FT_REG_FILE`] and friends. Present so `readdir` need
    /// not read the target inode.
    pub kind: u8,
    /// The name, as raw bytes.
    pub name: &'a [u8],
    /// Attribute value, empty for a real directory entry and the payload for an
    /// `XATTR_ITEM`.
    pub data: &'a [u8],
}

/// Iterator over the entries packed into one `DIR_ITEM` payload.
///
/// More than one entry shares an item when their names hash to the same value.
/// That is rare but entirely legal, so a lookup must compare the name of every
/// entry in the item rather than taking the first.
#[derive(Debug, Clone, Copy)]
pub struct DirItemIter<'a> {
    bytes: &'a [u8],
    at: usize,
    item_type: u8,
    done: bool,
}

impl<'a> DirItemIter<'a> {
    /// Iterate the entries in a `DIR_ITEM`, `DIR_INDEX` or `XATTR_ITEM`
    /// payload. `item_type` is the key type the payload was filed under: it
    /// decides whether the entries must be extended attributes or must not
    /// be, and names the type in an error.
    #[must_use]
    pub const fn new(bytes: &'a [u8], item_type: u8) -> Self {
        DirItemIter {
            bytes,
            at: 0,
            item_type,
            done: false,
        }
    }

    /// Whether an entry's type and lengths are ones btrfs writes.
    ///
    /// The per-entry checks of Linux's `check_dir_item`: a type in
    /// `FT_REG_FILE..=FT_XATTR`, and `FT_XATTR` exactly when the key is an
    /// `XATTR_ITEM`; a name of at most [`NAME_LEN`] bytes, or
    /// [`XATTR_NAME_MAX`] for an attribute; and no data behind anything but
    /// an attribute. Linux's other bound, that name and data together fit
    /// `BTRFS_MAX_XATTR_SIZE`, needs no check here: an entry is inside its
    /// payload, the payload is inside one node after at least one item
    /// descriptor, and that is exactly the room `BTRFS_MAX_XATTR_SIZE` measures.
    fn plausible(&self, kind: u8, name_len: usize, data_len: usize) -> bool {
        let xattr = kind == FT_XATTR;
        let longest = if xattr { XATTR_NAME_MAX } else { NAME_LEN };
        (FT_REG_FILE..=FT_XATTR).contains(&kind)
            && xattr == (self.item_type == XATTR_ITEM_KEY)
            && name_len <= longest
            && (xattr || data_len == 0)
    }

    /// Parse the entry at the cursor, returning it and the next cursor.
    fn record(&self) -> Option<(DirItem<'a>, usize)> {
        let location = BtrfsKey::parse(self.bytes, self.at)?;
        let transid = u64_at(self.bytes, self.at.checked_add(KEY_SIZE)?)?;
        let data_len = usize::from(u16_at(self.bytes, self.at.checked_add(25)?)?);
        let name_len = usize::from(u16_at(self.bytes, self.at.checked_add(27)?)?);
        let kind = u8_at(self.bytes, self.at.checked_add(29)?)?;
        if !self.plausible(kind, name_len, data_len) {
            return None;
        }
        let name_at = self.at.checked_add(DIR_ITEM_HEADER_SIZE)?;
        let name = slice_at(self.bytes, name_at, name_len)?;
        let data_at = name_at.checked_add(name_len)?;
        let data = slice_at(self.bytes, data_at, data_len)?;
        Some((
            DirItem {
                location,
                transid,
                kind,
                name,
                data,
            },
            data_at.checked_add(data_len)?,
        ))
    }
}

impl<'a> Iterator for DirItemIter<'a> {
    type Item = Result<DirItem<'a>, BtrfsError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done || self.at >= self.bytes.len() {
            return None;
        }
        match self.record() {
            Some((entry, next)) => {
                self.at = next;
                Some(Ok(entry))
            }
            None => {
                self.done = true;
                Some(Err(BtrfsError::BadItem {
                    item_type: self.item_type,
                }))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// EXTENT_DATA
// ---------------------------------------------------------------------------

/// Where a non-inline extent's bytes actually are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileExtent {
    /// *Logical* address of the allocated extent, or zero for a hole. A hole is
    /// not an error and not a short file: it reads as zeroes.
    pub disk_bytenr: u64,
    /// Size of the whole allocated extent, which may be larger than this
    /// reference uses.
    pub disk_num_bytes: u64,
    /// Offset of this reference within that extent, non-zero when a write split
    /// an existing extent instead of allocating a new one.
    pub offset: u64,
    /// How many bytes of the file this item covers, starting at the key's
    /// offset.
    pub num_bytes: u64,
}

impl FileExtent {
    /// Whether this extent is a hole rather than allocated storage.
    #[must_use]
    pub const fn is_hole(&self) -> bool {
        self.disk_bytenr == 0
    }

    /// The logical address this reference actually begins at.
    ///
    /// `disk_bytenr` names the extent, not the position inside it; forgetting
    /// to add `offset` reads the right extent from the wrong place, which for a
    /// file rewritten in the middle is a plausible-looking wrong answer.
    #[must_use]
    pub fn start(&self) -> Option<u64> {
        if self.is_hole() {
            None
        } else {
            self.disk_bytenr.checked_add(self.offset)
        }
    }
}

/// The part of an `EXTENT_DATA` item that depends on its type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtentDataBody<'a> {
    /// The file's bytes are in the item, immediately after the 21-byte header.
    /// Used for very small files, and for symlink targets. If `compression` is
    /// non-zero these bytes are compressed and `ram_bytes` is their expanded
    /// length.
    Inline(&'a [u8]),
    /// The file's bytes are in an allocated extent elsewhere.
    Regular(FileExtent),
    /// Space is reserved but was never written, so the extent reads as zeroes
    /// regardless of what is on the disk.
    Prealloc(FileExtent),
}

/// One extent of a file's contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtentData<'a> {
    /// Transaction that wrote this extent.
    pub generation: u64,
    /// Length of the data once decompressed.
    pub ram_bytes: u64,
    /// Compression algorithm; see [`COMPRESS_ZSTD`] and friends.
    pub compression: u8,
    /// Encryption algorithm. Always zero; btrfs has never shipped one.
    pub encryption: u8,
    /// Reserved for an alternative encoding of the extent.
    pub other_encoding: u16,
    /// The raw type byte, kept alongside `body` so an unexpected value is
    /// visible.
    pub kind: u8,
    /// Inline data, or the reference to the extent holding it.
    pub body: ExtentDataBody<'a>,
}

impl<'a> ExtentData<'a> {
    /// Parse an `EXTENT_DATA` payload.
    ///
    /// The header is 21 bytes and is followed either by the file's bytes
    /// (inline) or by four `u64`s naming an extent (regular and prealloc), so
    /// the type byte has to be read before the rest of the item has a length.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, BtrfsError> {
        let header = || {
            Some((
                u64_at(bytes, 0)?,
                u64_at(bytes, 8)?,
                u8_at(bytes, 16)?,
                u8_at(bytes, 17)?,
                u16_at(bytes, 18)?,
                u8_at(bytes, 20)?,
            ))
        };
        let (generation, ram_bytes, compression, encryption, other_encoding, kind) =
            header().ok_or_else(|| truncated(EXTENT_DATA_HEADER_SIZE, bytes.len()))?;

        let body = match kind {
            FILE_EXTENT_INLINE => {
                ExtentDataBody::Inline(bytes.get(EXTENT_DATA_HEADER_SIZE..).unwrap_or_default())
            }
            FILE_EXTENT_REG | FILE_EXTENT_PREALLOC => {
                let extent = Self::extent(bytes)?;
                if kind == FILE_EXTENT_REG {
                    ExtentDataBody::Regular(extent)
                } else {
                    ExtentDataBody::Prealloc(extent)
                }
            }
            _ => {
                return Err(BtrfsError::BadItem {
                    item_type: EXTENT_DATA_KEY,
                });
            }
        };
        Ok(ExtentData {
            generation,
            ram_bytes,
            compression,
            encryption,
            other_encoding,
            kind,
            body,
        })
    }

    /// Read the four `u64`s that follow the header of a non-inline extent.
    fn extent(bytes: &[u8]) -> Result<FileExtent, BtrfsError> {
        let get = || {
            Some(FileExtent {
                disk_bytenr: u64_at(bytes, 21)?,
                disk_num_bytes: u64_at(bytes, 29)?,
                offset: u64_at(bytes, 37)?,
                num_bytes: u64_at(bytes, 45)?,
            })
        };
        get().ok_or_else(|| truncated(FILE_EXTENT_ITEM_SIZE, bytes.len()))
    }

    /// Parse an `EXTENT_DATA` item and check it against the key it is filed
    /// under and the volume's `sectorsize`.
    ///
    /// [`ExtentData::parse`] reads the fields; this is the form a file read
    /// uses, because what makes an extent safe to follow is how its fields
    /// relate to each other, to its key and to the sector grid, and `parse`
    /// sees none of those. It mirrors Linux's `check_extent_data_item`, and
    /// adds the bounds that decide which bytes a read lands on — see
    /// `check_reference`.
    pub fn parse_item(
        key: &BtrfsKey,
        bytes: &'a [u8],
        sectorsize: u32,
    ) -> Result<Self, BtrfsError> {
        let extent = Self::parse(bytes)?;
        extent.check(key, bytes.len(), u64::from(sectorsize))?;
        Ok(extent)
    }

    /// The checks every kind of extent shares, then the kind's own.
    ///
    /// As `check_extent_data_item`: a compression type Linux defines, no
    /// encryption or other encoding (btrfs has never written either, so a
    /// non-zero value is an encoding this reader would misread), and a file
    /// offset on a sector boundary. An inline extent starts the file, at
    /// offset zero, and uncompressed it holds exactly `ram_bytes` bytes — the
    /// length a read takes from it.
    fn check(&self, key: &BtrfsKey, item_size: usize, sector: u64) -> Result<(), BtrfsError> {
        if self.compression > COMPRESS_ZSTD {
            return Err(BtrfsError::UnsupportedCompression(self.compression));
        }
        let consistent = self.encryption == 0
            && self.other_encoding == 0
            && key.offset.checked_rem(sector) == Some(0)
            && match self.body {
                ExtentDataBody::Inline(data) => {
                    key.offset == 0
                        && (!self.is_uncompressed() || data.len() as u64 == self.ram_bytes)
                }
                ExtentDataBody::Regular(file) | ExtentDataBody::Prealloc(file) => {
                    item_size == FILE_EXTENT_ITEM_SIZE && self.check_reference(key, &file, sector)
                }
            };
        if consistent {
            Ok(())
        } else {
            Err(BtrfsError::BadItem {
                item_type: EXTENT_DATA_KEY,
            })
        }
    }

    /// Whether a regular or preallocated extent's reference stays inside
    /// the extent it names.
    ///
    /// From `check_extent_data_item`: every size and address a whole number of
    /// sectors, and the key's offset plus `num_bytes` not wrapping. Beyond it,
    /// a non-empty range, and the range this item uses — `offset` to
    /// `offset + num_bytes` — inside the extent: inside `disk_num_bytes` when
    /// the bytes are stored as they are, inside `ram_bytes` when they expand
    /// from a compressed extent. Without that last bound a reference reaching
    /// past its extent reads whatever the allocator put next, which is another
    /// file's data, and passes every checksum on the way.
    fn check_reference(&self, key: &BtrfsKey, file: &FileExtent, sector: u64) -> bool {
        let aligned = [
            self.ram_bytes,
            file.disk_bytenr,
            file.disk_num_bytes,
            file.offset,
            file.num_bytes,
        ]
        .into_iter()
        .all(|value| value.checked_rem(sector) == Some(0));
        let extent_len = if self.is_uncompressed() {
            file.disk_num_bytes
        } else {
            self.ram_bytes
        };
        // A hole names no extent, so there is nothing to stay inside.
        let inside = file
            .offset
            .checked_add(file.num_bytes)
            .is_some_and(|used| file.is_hole() || used <= extent_len);
        aligned && file.num_bytes != 0 && inside && key.offset.checked_add(file.num_bytes).is_some()
    }

    /// The file offset just past this extent, when it is filed under `key`.
    ///
    /// Linux's `btrfs_file_extent_end`: a regular or preallocated extent ends
    /// `num_bytes` after its key, and an inline one at its expanded length
    /// rounded up to a whole sector, because the rest of that sector belongs
    /// to it. `None` if the end does not fit in a `u64`.
    #[must_use]
    pub fn end(&self, key: &BtrfsKey, sectorsize: u32) -> Option<u64> {
        match self.body {
            ExtentDataBody::Inline(_) => key
                .offset
                .checked_add(self.ram_bytes)?
                .checked_next_multiple_of(u64::from(sectorsize)),
            ExtentDataBody::Regular(file) | ExtentDataBody::Prealloc(file) => {
                key.offset.checked_add(file.num_bytes)
            }
        }
    }

    /// The extent reference, for a regular or preallocated extent.
    #[must_use]
    pub const fn file_extent(&self) -> Option<FileExtent> {
        match self.body {
            ExtentDataBody::Regular(extent) | ExtentDataBody::Prealloc(extent) => Some(extent),
            ExtentDataBody::Inline(_) => None,
        }
    }

    /// The inline bytes, for an inline extent.
    #[must_use]
    pub const fn inline_data(&self) -> Option<&'a [u8]> {
        match self.body {
            ExtentDataBody::Inline(data) => Some(data),
            _ => None,
        }
    }

    /// Whether the extent's bytes are stored verbatim.
    #[must_use]
    pub const fn is_uncompressed(&self) -> bool {
        self.compression == COMPRESS_NONE
    }
}

// ---------------------------------------------------------------------------
// ROOT_ITEM
// ---------------------------------------------------------------------------

/// The root of a subvolume or of an internal tree.
///
/// The on-disk item begins with a whole embedded `INODE_ITEM`, which nothing in
/// the read path needs, so this struct starts after it. Only the fields up to
/// `level` are parsed; the rest of the item is snapshot bookkeeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootItem {
    /// Transaction that last wrote this root.
    pub generation: u64,
    /// Object id of the subvolume's root directory, normally
    /// [`FIRST_FREE_OBJECTID`].
    pub root_dirid: u64,
    /// *Logical* address of the tree's top node — the reason this item is worth
    /// parsing at all.
    pub bytenr: u64,
    /// Quota limit, in bytes, or zero for none.
    pub byte_limit: u64,
    /// Bytes referenced by this tree.
    pub bytes_used: u64,
    /// Generation of the last snapshot taken of this subvolume.
    pub last_snapshot: u64,
    /// Root flags; bit 0 marks a read-only subvolume.
    pub flags: u64,
    /// Reference count.
    pub refs: u32,
    /// Height of the tree at `bytenr`, which the reader needs before it can
    /// tell a leaf from an internal node.
    pub level: u8,
}

impl RootItem {
    /// Offset of the first field after the embedded `INODE_ITEM`.
    const BASE: usize = INODE_ITEM_SIZE;

    /// Bytes that must be present for everything this struct reads.
    pub const MIN_SIZE: usize = 239;

    /// Parse a `ROOT_ITEM` payload.
    pub fn parse(bytes: &[u8]) -> Result<Self, BtrfsError> {
        let base = Self::BASE;
        let get = || {
            Some(RootItem {
                generation: u64_at(bytes, base)?,
                root_dirid: u64_at(bytes, base.checked_add(8)?)?,
                bytenr: u64_at(bytes, base.checked_add(16)?)?,
                byte_limit: u64_at(bytes, base.checked_add(24)?)?,
                bytes_used: u64_at(bytes, base.checked_add(32)?)?,
                last_snapshot: u64_at(bytes, base.checked_add(40)?)?,
                flags: u64_at(bytes, base.checked_add(48)?)?,
                refs: u32_at(bytes, base.checked_add(56)?)?,
                // 220..237 is `drop_progress`, 237 is `drop_level`, and the
                // tree height sits immediately after them.
                level: u8_at(bytes, base.checked_add(78)?)?,
            })
        };
        get().ok_or_else(|| truncated(Self::MIN_SIZE, bytes.len()))
    }

    /// The embedded `INODE_ITEM`, for a caller that wants the subvolume's
    /// timestamps.
    pub fn inode(bytes: &[u8]) -> Result<InodeItem, BtrfsError> {
        InodeItem::parse(bytes)
    }
}

// ---------------------------------------------------------------------------
// DEV_ITEM
// ---------------------------------------------------------------------------

/// A device belonging to the volume.
///
/// The same 98 bytes appear both as a `DEV_ITEM` in the chunk tree and embedded
/// in the superblock, which is how a device can identify itself before any tree
/// has been read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DevItem {
    /// Identifier a chunk's stripes refer to.
    pub devid: u64,
    /// Size of the device in bytes.
    pub total_bytes: u64,
    /// Bytes of it allocated to chunks.
    pub bytes_used: u64,
    /// Preferred I/O alignment.
    pub io_align: u32,
    /// Preferred I/O width.
    pub io_width: u32,
    /// Minimum I/O size.
    pub sector_size: u32,
    /// Device type flags.
    pub type_bits: u64,
    /// Generation the device was last written in.
    pub generation: u64,
    /// Bytes at the start of the device that chunks may not use.
    pub start_offset: u64,
    /// Device grouping hint.
    pub dev_group: u32,
    /// Seek speed hint, unused.
    pub seek_speed: u8,
    /// Bandwidth hint, unused.
    pub bandwidth: u8,
    /// This device's UUID, which is what a stripe's `dev_uuid` must match.
    pub uuid: [u8; 16],
    /// UUID of the filesystem the device belongs to.
    pub fsid: [u8; 16],
}

impl DevItem {
    /// Parse a `DEV_ITEM` payload.
    pub fn parse(bytes: &[u8]) -> Result<Self, BtrfsError> {
        let get = || {
            Some(DevItem {
                devid: u64_at(bytes, 0)?,
                total_bytes: u64_at(bytes, 8)?,
                bytes_used: u64_at(bytes, 16)?,
                io_align: u32_at(bytes, 24)?,
                io_width: u32_at(bytes, 28)?,
                sector_size: u32_at(bytes, 32)?,
                type_bits: u64_at(bytes, 36)?,
                generation: u64_at(bytes, 44)?,
                start_offset: u64_at(bytes, 52)?,
                dev_group: u32_at(bytes, 60)?,
                seek_speed: u8_at(bytes, 64)?,
                bandwidth: u8_at(bytes, 65)?,
                uuid: array_at::<16>(bytes, 66)?,
                fsid: array_at::<16>(bytes, 82)?,
            })
        };
        get().ok_or_else(|| truncated(DEV_ITEM_SIZE, bytes.len()))
    }
}

// ---------------------------------------------------------------------------
// EXTENT_CSUM
// ---------------------------------------------------------------------------

/// A run of data checksums, one per sector.
///
/// The item's payload is a bare array of little-endian `u32`s with no header;
/// the logical address the run starts at is the key's `offset`, and the stride
/// is the filesystem's `sectorsize`. Both therefore have to be supplied from
/// outside the payload.
#[derive(Debug, Clone, Copy)]
pub struct CsumItem<'a> {
    logical_start: u64,
    sectorsize: u32,
    bytes: &'a [u8],
}

impl<'a> CsumItem<'a> {
    /// Wrap an `EXTENT_CSUM` payload starting at logical address
    /// `logical_start`.
    ///
    /// Returns an error for a zero `sectorsize`, which would make every
    /// address-to-index division undefined.
    pub fn new(logical_start: u64, sectorsize: u32, bytes: &'a [u8]) -> Result<Self, BtrfsError> {
        if sectorsize == 0 {
            return Err(BtrfsError::BadSectorSize(sectorsize));
        }
        Ok(CsumItem {
            logical_start,
            sectorsize,
            bytes,
        })
    }

    /// How many sector checksums the item holds. A trailing partial checksum is
    /// not counted.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.bytes.len() / 4
    }

    /// Whether the item holds no complete checksum.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The checksum at `index`, or `None` past the end.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<u32> {
        u32_at(self.bytes, index.checked_mul(4)?)
    }

    /// The checksum covering the sector containing `logical`, or `None` if this
    /// item does not cover that address.
    #[must_use]
    pub fn checksum_for(&self, logical: u64) -> Option<u32> {
        let within = logical.checked_sub(self.logical_start)?;
        let index = usize::try_from(within / u64::from(self.sectorsize)).ok()?;
        if index >= self.len() {
            return None;
        }
        self.get(index)
    }

    /// The logical address this run of checksums begins at.
    #[must_use]
    pub const fn logical_start(&self) -> u64 {
        self.logical_start
    }
}
