//! Files and directories: the edits a VFS asks for, as fs-tree items.
//!
//! Every file operation is a handful of items in the top-level fs tree that
//! must agree with each other, and `btrfs check` compares all of them:
//!
//! * a name is three items — a `DIR_ITEM` keyed by the name's hash, a
//!   `DIR_INDEX` keyed by a sequence number, and an `INODE_REF` from the inode
//!   back to the directory — and a directory's `size` is the sum of its
//!   entries' name lengths counted twice, once per item;
//! * a file's `nlink` is its number of names; a directory's is always one,
//!   because btrfs does not count `..` or subdirectories;
//! * a file whose `nlink` reached zero while still open keeps its items and
//!   gets an `ORPHAN_ITEM`, so a crash before it is closed leaves something
//!   to clean up rather than a leaked inode ([`WriteVolume::evict`] and the
//!   orphan cleanup at open);
//! * a file's `nbytes` is the bytes its extents hold: inline bytes, and the
//!   length of every extent that is not a hole;
//! * and every regular extent's sectors have checksums, unless the inode is
//!   `NODATASUM`.
//!
//! Holes are not recorded: every volume this writer opens has `NO_HOLES`
//! (`mkfs.btrfs` has set it by default since 5.15), and one without it is
//! refused by [`WriteVolume::open`].
//!
//! Timestamps come from the caller: this crate has no clock.

use alloc::vec::Vec;

use ferrix_btrfs::crc32c;
use ferrix_btrfs::items::{
    DIR_INDEX_KEY, DIR_ITEM_KEY, DirItemIter, EXTENT_DATA_KEY, ExtentData, ExtentDataBody,
    FILE_EXTENT_INLINE, FILE_EXTENT_REG, FIRST_FREE_OBJECTID, FS_TREE_OBJECTID, INODE_EXTREF_KEY,
    INODE_ITEM_KEY, INODE_ITEM_SIZE, INODE_REF_KEY, InodeItem, InodeRefIter, LAST_FREE_OBJECTID,
    NAME_LEN, Timespec, extref_hash, name_hash,
};
use ferrix_btrfs::tree::{BtrfsKey, HEADER_SIZE, ITEM_SIZE};

use crate::bytes::{put, put_key, put_u8, put_u16, put_u32, put_u64};
use crate::extent::Backref;
use crate::volume::Need;
use crate::{Error, Result, WriteDevice, WriteVolume, fallible};

/// The tree every file lives in: the top-level subvolume, the only one this
/// writer opens.
pub const FS_TREE: u64 = FS_TREE_OBJECTID;
/// Object id orphan items are filed under.
pub const ORPHAN_OBJECTID: u64 = 0u64.wrapping_sub(5);
/// Key type of an orphan item.
pub const ORPHAN_ITEM_KEY: u8 = 48;
/// Linux's default `max_inline`: the largest file stored in its leaf.
pub const MAX_INLINE: u64 = 2048;
/// The largest extent this writer makes, Linux's `BTRFS_MAX_EXTENT_SIZE`.
pub const MAX_EXTENT: u64 = 128 * 1024 * 1024;
/// Bytes of a file extent item's header, before the extent reference.
const FILE_EXTENT_HEADER: usize = 21;
/// Bytes of a regular file extent item.
const FILE_EXTENT_SIZE: usize = 53;

/// One entry a listing reports: its index, the inode it names, its entry
/// type and its name.
pub type DirRecord = (u64, u64, u8, Vec<u8>);

/// A file extent as the fs tree records it, owned.
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(Clone))]
struct FileExtentItem {
    /// File offset: the key's offset.
    start: u64,
    generation: u64,
    ram_bytes: u64,
    compression: u8,
    kind: u8,
    /// Inline bytes, for an inline extent.
    inline: Vec<u8>,
    disk_bytenr: u64,
    disk_num_bytes: u64,
    offset: u64,
    num_bytes: u64,
}

impl FileExtentItem {
    fn parse(key: &BtrfsKey, data: &[u8], sectorsize: u32) -> Result<Self> {
        let extent = ExtentData::parse_item(key, data, sectorsize)?;
        let mut item = FileExtentItem {
            start: key.offset,
            generation: extent.generation,
            ram_bytes: extent.ram_bytes,
            compression: extent.compression,
            kind: extent.kind,
            inline: Vec::new(),
            disk_bytenr: 0,
            disk_num_bytes: 0,
            offset: 0,
            num_bytes: 0,
        };
        match extent.body {
            ExtentDataBody::Inline(bytes) => item.inline = fallible::copy(bytes)?,
            ExtentDataBody::Regular(file) | ExtentDataBody::Prealloc(file) => {
                item.disk_bytenr = file.disk_bytenr;
                item.disk_num_bytes = file.disk_num_bytes;
                item.offset = file.offset;
                item.num_bytes = file.num_bytes;
            }
        }
        Ok(item)
    }

    const fn is_inline(&self) -> bool {
        self.kind == FILE_EXTENT_INLINE
    }

    /// File offset just past what this extent covers. An inline extent covers
    /// its sector, as Linux's `btrfs_file_extent_end` says.
    fn end(&self, sectorsize: u64) -> u64 {
        if self.is_inline() {
            self.ram_bytes.next_multiple_of(sectorsize)
        } else {
            self.start.saturating_add(self.num_bytes)
        }
    }

    /// What this extent adds to its inode's `nbytes`.
    fn counted_bytes(&self) -> u64 {
        if self.is_inline() {
            self.ram_bytes
        } else if self.disk_bytenr == 0 {
            0
        } else {
            self.num_bytes
        }
    }

    /// The back-reference the extent it names holds for it.
    fn backref(&self, ino: u64) -> Option<Backref> {
        (!self.is_inline() && self.disk_bytenr != 0).then(|| Backref::Data {
            root: FS_TREE,
            objectid: ino,
            offset: self.start.wrapping_sub(self.offset),
        })
    }

    /// A piece of this regular extent: `num_bytes` of it from `offset`
    /// into the extent, at file offset `start`.
    const fn piece(&self, start: u64, offset: u64, num_bytes: u64) -> FileExtentItem {
        FileExtentItem {
            start,
            generation: self.generation,
            ram_bytes: self.ram_bytes,
            compression: self.compression,
            kind: self.kind,
            inline: Vec::new(),
            disk_bytenr: self.disk_bytenr,
            disk_num_bytes: self.disk_num_bytes,
            offset,
            num_bytes,
        }
    }

    /// The item's payload. [`Error::ItemTooLarge`] for inline bytes no item
    /// could hold.
    fn encode(&self) -> Result<Vec<u8>> {
        let len = if self.is_inline() {
            FILE_EXTENT_HEADER
                .checked_add(self.inline.len())
                .ok_or(Error::ItemTooLarge)?
        } else {
            FILE_EXTENT_SIZE
        };
        let mut out = fallible::zeroed(len)?;
        self.encode_into(&mut out).ok_or(Error::ItemTooLarge)?;
        Ok(out)
    }

    /// Lay the payload out in `out`, which is exactly its size.
    fn encode_into(&self, out: &mut [u8]) -> Option<()> {
        put_u64(out, 0, self.generation)?;
        put_u64(out, 8, self.ram_bytes)?;
        put_u8(out, 16, self.compression)?;
        put_u8(out, 20, self.kind)?;
        if self.is_inline() {
            put(out, FILE_EXTENT_HEADER, &self.inline)?;
        } else {
            put_u64(out, 21, self.disk_bytenr)?;
            put_u64(out, 29, self.disk_num_bytes)?;
            put_u64(out, 37, self.offset)?;
            put_u64(out, 45, self.num_bytes)?;
        }
        Some(())
    }
}

/// An `INODE_ITEM` payload.
pub fn encode_inode(item: &InodeItem) -> Result<Vec<u8>> {
    let mut out = fallible::zeroed(INODE_ITEM_SIZE)?;
    let mut fields = || -> Option<()> {
        put_u64(&mut out, 0, item.generation)?;
        put_u64(&mut out, 8, item.transid)?;
        put_u64(&mut out, 16, item.size)?;
        put_u64(&mut out, 24, item.nbytes)?;
        put_u64(&mut out, 32, item.block_group)?;
        put_u32(&mut out, 40, item.nlink)?;
        put_u32(&mut out, 44, item.uid)?;
        put_u32(&mut out, 48, item.gid)?;
        put_u32(&mut out, 52, item.mode)?;
        put_u64(&mut out, 56, item.rdev)?;
        put_u64(&mut out, 64, item.flags)?;
        put_u64(&mut out, 72, item.sequence)?;
        for (at, time) in [
            (112, item.atime),
            (124, item.ctime),
            (136, item.mtime),
            (148, item.otime),
        ] {
            put_u64(&mut out, at, time.sec)?;
            put_u32(&mut out, at + 8, time.nsec)?;
        }
        Some(())
    };
    // Every offset is a constant inside the 160 bytes just allocated.
    let _ = fields();
    Ok(out)
}

/// The directory-entry type for an inode's mode.
#[must_use]
pub const fn entry_type(mode: u32) -> u8 {
    use ferrix_btrfs::items as it;
    match mode & it::S_IFMT {
        it::S_IFREG => it::FT_REG_FILE,
        it::S_IFDIR => it::FT_DIR,
        it::S_IFCHR => it::FT_CHRDEV,
        it::S_IFBLK => it::FT_BLKDEV,
        it::S_IFIFO => it::FT_FIFO,
        it::S_IFSOCK => it::FT_SOCK,
        it::S_IFLNK => it::FT_SYMLINK,
        _ => it::FT_UNKNOWN,
    }
}

/// `header` zero bytes and then `name`, for a record `fill` lays out;
/// [`Error::ItemTooLarge`] for a name the record cannot hold.
fn encode_named(
    header: usize,
    name: &[u8],
    fill: impl FnOnce(&mut [u8]) -> Option<()>,
) -> Result<Vec<u8>> {
    let mut out = fallible::zeroed(header.checked_add(name.len()).ok_or(Error::ItemTooLarge)?)?;
    fill(&mut out)
        .and_then(|()| put(&mut out, header, name))
        .ok_or(Error::ItemTooLarge)?;
    Ok(out)
}

/// A directory entry payload, for a `DIR_ITEM` or `DIR_INDEX`.
fn encode_dir_entry(ino: u64, transid: u64, kind: u8, name: &[u8]) -> Result<Vec<u8>> {
    encode_named(30, name, |out| {
        put_key(out, 0, &BtrfsKey::new(ino, INODE_ITEM_KEY, 0))?;
        put_u64(out, 17, transid)?;
        put_u16(out, 25, 0)?;
        put_u16(out, 27, u16::try_from(name.len()).ok()?)?;
        put_u8(out, 29, kind)
    })
}

/// One `INODE_REF` record.
fn encode_ref(index: u64, name: &[u8]) -> Result<Vec<u8>> {
    encode_named(10, name, |out| {
        put_u64(out, 0, index)?;
        put_u16(out, 8, u16::try_from(name.len()).ok()?)
    })
}

/// One `INODE_EXTREF` record.
fn encode_extref(parent: u64, index: u64, name: &[u8]) -> Result<Vec<u8>> {
    encode_named(18, name, |out| {
        put_u64(out, 0, parent)?;
        put_u64(out, 8, index)?;
        put_u16(out, 16, u16::try_from(name.len()).ok()?)
    })
}

/// A name a directory may hold: one to 255 bytes, not `.` or `..`, with no
/// `/` or NUL.
fn check_name(name: &[u8]) -> Result<()> {
    if name.len() > NAME_LEN {
        return Err(Error::NameTooLong);
    }
    let special = name.is_empty() || name == b"." || name == b"..";
    if special || name.iter().any(|&b| b == b'/' || b == 0) {
        return Err(Error::InvalidName);
    }
    Ok(())
}

const fn inode_key(ino: u64) -> BtrfsKey {
    BtrfsKey::new(ino, INODE_ITEM_KEY, 0)
}

/// What a new inode is.
#[derive(Debug, Clone, Copy)]
pub struct NewInode {
    /// Type and permission bits.
    pub mode: u32,
    /// Owner.
    pub uid: u32,
    /// Group.
    pub gid: u32,
    /// Device number, for a device node.
    pub rdev: u64,
    /// Creation time, used for every timestamp.
    pub now: Timespec,
}

impl<D: WriteDevice> WriteVolume<D> {
    /// The stat data of `ino`.
    pub fn inode(&mut self, ino: u64) -> Result<Option<InodeItem>> {
        self.get(FS_TREE, &inode_key(ino))?
            .map(|data| InodeItem::parse(&data).map_err(Error::from))
            .transpose()
    }

    fn require_inode(&mut self, ino: u64) -> Result<InodeItem> {
        self.inode(ino)?.ok_or(Error::NotFound)
    }

    /// Replace the stat data of `ino`, stamping it with this transaction.
    pub fn write_inode(&mut self, ino: u64, item: &InodeItem) -> Result<()> {
        let mut item = *item;
        item.transid = self.transid;
        item.sequence = item.sequence.wrapping_add(1);
        self.update(FS_TREE, inode_key(ino), encode_inode(&item)?)
    }

    /// The inode `name` in directory `dir` names, and its entry type.
    pub fn lookup(&mut self, dir: u64, name: &[u8]) -> Result<Option<(u64, u8)>> {
        let key = BtrfsKey::new(dir, DIR_ITEM_KEY, name_hash(name));
        let Some(data) = self.get(FS_TREE, &key)? else {
            return Ok(None);
        };
        for entry in DirItemIter::new(&data, DIR_ITEM_KEY) {
            let entry = entry?;
            if entry.name == name {
                if entry.location.item_type != INODE_ITEM_KEY {
                    return Err(Error::Unsupported(crate::Unsupported::Subvolumes));
                }
                return Ok(Some((entry.location.objectid, entry.kind)));
            }
        }
        Ok(None)
    }

    /// The inode number the next new inode gets: one past the highest in
    /// the tree, as Linux's `btrfs_find_highest_objectid`.
    fn next_inode_number(&mut self) -> Result<u64> {
        let probe = BtrfsKey::new(LAST_FREE_OBJECTID, u8::MAX, u64::MAX);
        let highest = self
            .prev_item(FS_TREE, &probe)?
            .map_or(0, |(key, _)| key.objectid);
        let next = highest.max(FIRST_FREE_OBJECTID - 1).saturating_add(1);
        if next > LAST_FREE_OBJECTID {
            return Err(Error::NoSpace);
        }
        Ok(next)
    }

    /// The next `DIR_INDEX` number in `dir`: one past its last, and at least
    /// two, leaving `.` and `..` their conventional positions.
    fn next_dir_index(&mut self, dir: u64) -> Result<u64> {
        let probe = BtrfsKey::new(dir, DIR_INDEX_KEY, u64::MAX);
        let last = self
            .prev_item(FS_TREE, &probe)?
            .filter(|(key, _)| key.objectid == dir && key.item_type == DIR_INDEX_KEY)
            .map(|(key, _)| key.offset);
        Ok(last.map_or(2, |index| index.saturating_add(1)).max(2))
    }

    /// Create a file, directory, symlink or device node named `name` in
    /// `dir`. Returns its inode number. [`Error::Exists`] if the name is
    /// taken. A symlink's target is written separately, with
    /// [`WriteVolume::set_symlink`].
    fn create_inner(&mut self, dir: u64, name: &[u8], new: &NewInode) -> Result<u64> {
        check_name(name)?;
        if self.lookup(dir, name)?.is_some() {
            return Err(Error::Exists);
        }
        let parent = self.require_inode(dir)?;
        if !parent.is_dir() {
            return Err(Error::NotDir);
        }
        let ino = self.next_inode_number()?;
        let item = InodeItem {
            generation: self.transid,
            transid: self.transid,
            size: 0,
            nbytes: 0,
            block_group: 0,
            nlink: 1,
            uid: new.uid,
            gid: new.gid,
            mode: new.mode,
            rdev: new.rdev,
            flags: 0,
            sequence: 0,
            atime: new.now,
            ctime: new.now,
            mtime: new.now,
            otime: new.now,
        };
        // FALLIBLE: the tree's own insert, which reports running out of memory.
        self.insert(FS_TREE, inode_key(ino), encode_inode(&item)?)?;
        self.add_name(dir, name, ino, entry_type(new.mode), new.now)?;
        Ok(ino)
    }

    /// Give `ino` another name, `name` in `dir`. Directories cannot be
    /// linked; the caller refuses that before asking.
    fn link_inner(&mut self, dir: u64, name: &[u8], ino: u64, now: Timespec) -> Result<()> {
        check_name(name)?;
        if self.lookup(dir, name)?.is_some() {
            return Err(Error::Exists);
        }
        let mut item = self.require_inode(ino)?;
        item.nlink = item.nlink.checked_add(1).ok_or(Error::TooManyLinks)?;
        item.ctime = now;
        self.write_inode(ino, &item)?;
        self.add_name(dir, name, ino, entry_type(item.mode), now)?;
        // A file linked again after its last name went is no orphan.
        if item.nlink == 1 {
            let _ =
                self.delete_if_present(&BtrfsKey::new(ORPHAN_OBJECTID, ORPHAN_ITEM_KEY, ino))?;
        }
        Ok(())
    }

    /// The three items of a name, and the directory's size and times.
    fn add_name(&mut self, dir: u64, name: &[u8], ino: u64, kind: u8, now: Timespec) -> Result<()> {
        let index = self.next_dir_index(dir)?;
        let entry = encode_dir_entry(ino, self.transid, kind, name)?;
        let hash_key = BtrfsKey::new(dir, DIR_ITEM_KEY, name_hash(name));
        let mut shared = self.get(FS_TREE, &hash_key)?.unwrap_or_default();
        fallible::extend_from_slice(&mut shared, &entry)?;
        self.put(FS_TREE, hash_key, shared)?;
        // FALLIBLE: the tree's own insert.
        self.insert(FS_TREE, BtrfsKey::new(dir, DIR_INDEX_KEY, index), entry)?;
        self.add_ref(ino, dir, index, name)?;
        let mut parent = self.require_inode(dir)?;
        parent.size = parent.size.saturating_add(2 * name.len() as u64);
        parent.mtime = now;
        parent.ctime = now;
        self.write_inode(dir, &parent)
    }

    /// The back-reference from `ino` to its name in `parent`: appended to the
    /// `INODE_REF` for that directory while it fits in a leaf, and as an
    /// `INODE_EXTREF` once it does not.
    fn add_ref(&mut self, ino: u64, parent: u64, index: u64, name: &[u8]) -> Result<()> {
        let key = BtrfsKey::new(ino, INODE_REF_KEY, parent);
        let mut refs = self.get(FS_TREE, &key)?.unwrap_or_default();
        let record = encode_ref(index, name)?;
        if refs.len().saturating_add(record.len()) <= self.max_item_size() / 2 || refs.is_empty() {
            fallible::extend_from_slice(&mut refs, &record)?;
            return self.put(FS_TREE, key, refs);
        }
        let key = BtrfsKey::new(ino, INODE_EXTREF_KEY, extref_hash(parent, name));
        let mut refs = self.get(FS_TREE, &key)?.unwrap_or_default();
        fallible::extend_from_slice(&mut refs, &encode_extref(parent, index, name)?)?;
        if refs.len() > self.max_item_size() / 2 {
            return Err(Error::TooManyLinks);
        }
        self.put(FS_TREE, key, refs)
    }

    /// Linux's `BTRFS_MAX_ITEM_SIZE`.
    fn max_item_size(&self) -> usize {
        (self.nodesize() as usize)
            .saturating_sub(HEADER_SIZE)
            .saturating_sub(ITEM_SIZE)
    }

    /// Delete an item if it exists. Whether it did.
    fn delete_if_present(&mut self, key: &BtrfsKey) -> Result<bool> {
        if self.get(FS_TREE, key)?.is_some() {
            self.delete(FS_TREE, key)?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Remove the name `name` from `dir`, returning the inode it named. The
    /// inode loses a link; one that had its last, and is not a directory,
    /// becomes an orphan, which [`WriteVolume::evict`] deletes once nothing
    /// holds it open. A directory must be empty; the caller checks.
    fn unlink_inner(&mut self, dir: u64, name: &[u8], now: Timespec) -> Result<u64> {
        let (ino, _) = self.lookup(dir, name)?.ok_or(Error::NotFound)?;
        let index = self.remove_ref(ino, dir, name)?;
        self.remove_entries(dir, name, index, now)?;
        let mut item = self.require_inode(ino)?;
        item.nlink = item.nlink.saturating_sub(1);
        item.ctime = now;
        self.write_inode(ino, &item)?;
        if item.nlink == 0 {
            // FALLIBLE: the tree's own insert.
            self.insert(
                FS_TREE,
                BtrfsKey::new(ORPHAN_OBJECTID, ORPHAN_ITEM_KEY, ino),
                Vec::new(),
            )?;
        }
        Ok(ino)
    }

    /// Drop the directory entry items of `name` in `dir`, and shrink the
    /// directory.
    fn remove_entries(&mut self, dir: u64, name: &[u8], index: u64, now: Timespec) -> Result<()> {
        let hash_key = BtrfsKey::new(dir, DIR_ITEM_KEY, name_hash(name));
        let shared = self
            .get(FS_TREE, &hash_key)?
            .ok_or(Error::Inconsistent("name without its DIR_ITEM"))?;
        let kept = without_record(&shared, name, |data, at| {
            let entry = DirItemIter::new(data.get(at..)?, DIR_ITEM_KEY)
                .next()?
                .ok()?;
            Some((entry.name, 30 + entry.name.len() + entry.data.len()))
        })?;
        if kept.is_empty() {
            self.delete(FS_TREE, &hash_key)?;
        } else {
            self.update(FS_TREE, hash_key, kept)?;
        }
        self.delete(FS_TREE, &BtrfsKey::new(dir, DIR_INDEX_KEY, index))?;
        let mut parent = self.require_inode(dir)?;
        parent.size = parent.size.saturating_sub(2 * name.len() as u64);
        parent.mtime = now;
        parent.ctime = now;
        self.write_inode(dir, &parent)
    }

    /// Drop the back-reference from `ino` to `name` in `parent`, returning the
    /// directory index it recorded.
    fn remove_ref(&mut self, ino: u64, parent: u64, name: &[u8]) -> Result<u64> {
        let key = BtrfsKey::new(ino, INODE_REF_KEY, parent);
        if let Some(refs) = self.get(FS_TREE, &key)? {
            let mut index = None;
            for record in InodeRefIter::new(&refs) {
                let record = record?;
                if record.name == name {
                    index = Some(record.index);
                }
            }
            if let Some(index) = index {
                let kept = without_record(&refs, name, |data, at| {
                    let record = InodeRefIter::new(data.get(at..)?).next()?.ok()?;
                    Some((record.name, 10 + record.name.len()))
                })?;
                if kept.is_empty() {
                    self.delete(FS_TREE, &key)?;
                } else {
                    self.update(FS_TREE, key, kept)?;
                }
                return Ok(index);
            }
        }
        let key = BtrfsKey::new(ino, INODE_EXTREF_KEY, extref_hash(parent, name));
        let refs = self
            .get(FS_TREE, &key)?
            .ok_or(Error::Inconsistent("name without its INODE_REF"))?;
        let mut index = None;
        for record in ferrix_btrfs::items::InodeExtrefIter::new(&refs) {
            let record = record?;
            if record.name == name && record.parent == parent {
                index = Some(record.index);
            }
        }
        let index = index.ok_or(Error::Inconsistent("name without its INODE_REF"))?;
        let kept = without_record(&refs, name, |data, at| {
            let record = ferrix_btrfs::items::InodeExtrefIter::new(data.get(at..)?)
                .next()?
                .ok()?;
            let named: &[u8] = if record.parent == parent {
                record.name
            } else {
                &[]
            };
            Some((named, 18 + record.name.len()))
        })?;
        if kept.is_empty() {
            self.delete(FS_TREE, &key)?;
        } else {
            self.update(FS_TREE, key, kept)?;
        }
        Ok(index)
    }

    /// Move the name `old` in `old_dir` to `new` in `new_dir`. If `new`
    /// exists it is unlinked first; the caller has checked that replacing it
    /// is allowed.
    fn rename_inner(
        &mut self,
        old_dir: u64,
        old: &[u8],
        new_dir: u64,
        new: &[u8],
        now: Timespec,
    ) -> Result<()> {
        check_name(new)?;
        let (ino, kind) = self.lookup(old_dir, old)?.ok_or(Error::NotFound)?;
        if old_dir == new_dir && old == new {
            return Ok(());
        }
        if self.lookup(new_dir, new)?.is_some() {
            let _ = self.unlink(new_dir, new, now)?;
        }
        let index = self.remove_ref(ino, old_dir, old)?;
        self.remove_entries(old_dir, old, index, now)?;
        self.add_name(new_dir, new, ino, kind, now)?;
        let mut item = self.require_inode(ino)?;
        item.ctime = now;
        self.write_inode(ino, &item)
    }

    /// Delete everything of an inode whose last name is gone and which
    /// nothing holds open: its items, its extents' references, and its
    /// orphan item.
    fn evict_inner(&mut self, ino: u64) -> Result<()> {
        let item = self.require_inode(ino)?;
        if item.nlink != 0 {
            return Err(Error::Inconsistent("evicted an inode that still has names"));
        }
        self.drop_extents(ino, 0, u64::MAX)?;
        let from = BtrfsKey::new(ino, 0, 0);
        let to = BtrfsKey::new(ino, u8::MAX, u64::MAX);
        for (key, _) in self.range(FS_TREE, &from, &to)? {
            self.delete(FS_TREE, &key)?;
        }
        let _ = self.delete_if_present(&BtrfsKey::new(ORPHAN_OBJECTID, ORPHAN_ITEM_KEY, ino))?;
        Ok(())
    }

    /// List directory `dir` from index `from`: each entry's index, inode
    /// number, entry type and name, in the order `readdir` reports them.
    ///
    /// Entries naming another subvolume are left out, as the read path does,
    /// since this writer does not cross into one.
    pub fn read_dir(&mut self, dir: u64, from: u64) -> Result<Vec<DirRecord>> {
        let start = BtrfsKey::new(dir, DIR_INDEX_KEY, from);
        let end = BtrfsKey::new(dir, DIR_INDEX_KEY, u64::MAX);
        let mut out = Vec::new();
        for (key, data) in self.range(FS_TREE, &start, &end)? {
            let entry = DirItemIter::new(&data, DIR_INDEX_KEY)
                .next()
                .transpose()?
                .ok_or(Error::Inconsistent("empty DIR_INDEX item"))?;
            if entry.location.item_type != INODE_ITEM_KEY {
                continue;
            }
            fallible::push(
                &mut out,
                (
                    key.offset,
                    entry.location.objectid,
                    entry.kind,
                    fallible::copy(entry.name)?,
                ),
            )?;
        }
        Ok(out)
    }

    /// Whether directory `ino` holds no entries. A directory's size is the
    /// sum of its entries' names, twice over, so an empty one has size zero.
    pub fn dir_is_empty(&mut self, ino: u64) -> Result<bool> {
        let item = self.require_inode(ino)?;
        if !item.is_dir() {
            return Err(Error::NotDir);
        }
        Ok(item.size == 0)
    }

    /// The volume's size and what is allocated of it, for `statfs`.
    pub fn capacity(&self) -> (u64, u64) {
        (self.geometry.device_size, self.space.used())
    }

    /// The inode numbers orphan items name: files whose last name went while
    /// open, left behind by a crash or an unclean unmount.
    pub fn orphans(&mut self) -> Result<Vec<u64>> {
        let from = BtrfsKey::new(ORPHAN_OBJECTID, ORPHAN_ITEM_KEY, 0);
        let to = BtrfsKey::new(ORPHAN_OBJECTID, ORPHAN_ITEM_KEY, u64::MAX);
        fallible::collect(
            self.range(FS_TREE, &from, &to)?
                .into_iter()
                .map(|(key, _)| key.offset),
        )
    }

    /// Store `target` as the contents of symlink `ino`: one inline extent.
    fn set_symlink_inner(&mut self, ino: u64, target: &[u8]) -> Result<()> {
        let most = self.max_item_size().saturating_sub(FILE_EXTENT_HEADER);
        if target.is_empty() || target.len() > most || target.len() >= self.sectorsize() as usize {
            return Err(Error::NameTooLong);
        }
        self.write_inline(ino, target)
    }

    /// Replace every extent of `ino` with one inline extent holding `data`.
    fn write_inline(&mut self, ino: u64, data: &[u8]) -> Result<()> {
        let extent = FileExtentItem {
            start: 0,
            generation: self.transid,
            ram_bytes: data.len() as u64,
            compression: 0,
            kind: FILE_EXTENT_INLINE,
            inline: fallible::copy(data)?,
            disk_bytenr: 0,
            disk_num_bytes: 0,
            offset: 0,
            num_bytes: 0,
        };
        // Made before the old extents go, so running out of memory for it
        // changes nothing.
        let item = extent.encode()?;
        self.drop_extents(ino, 0, u64::MAX)?;
        let key = BtrfsKey::new(ino, EXTENT_DATA_KEY, 0);
        // FALLIBLE: the tree's own insert.
        self.insert(FS_TREE, key, item)?;
        let mut item = self.require_inode(ino)?;
        item.size = data.len() as u64;
        item.nbytes = data.len() as u64;
        self.write_inode(ino, &item)
    }

    /// Every file extent of `ino` overlapping `[start, end)`.
    fn extents_overlapping(
        &mut self,
        ino: u64,
        start: u64,
        end: u64,
    ) -> Result<Vec<FileExtentItem>> {
        let sector = self.sectorsize();
        let probe = BtrfsKey::new(ino, EXTENT_DATA_KEY, start);
        let first = self
            .prev_item(FS_TREE, &probe)?
            .filter(|(key, _)| key.objectid == ino && key.item_type == EXTENT_DATA_KEY)
            .map_or(probe, |(key, _)| key);
        let last = BtrfsKey::new(ino, EXTENT_DATA_KEY, end.saturating_sub(1));
        let mut out = Vec::new();
        for (key, data) in self.range(FS_TREE, &first, &last)? {
            let extent = FileExtentItem::parse(&key, &data, sector)?;
            if extent.end(u64::from(sector)) > start && extent.start < end {
                fallible::push(&mut out, extent)?;
            }
        }
        Ok(out)
    }

    /// Remove `[start, end)` from the file's extents: extents inside it go,
    /// extents across its edges are cut to the parts outside it. `start` is
    /// sector-aligned; `end` is too, or `u64::MAX`. The inode's `nbytes` is
    /// adjusted; its size is not.
    ///
    /// A cut piece of a regular extent keeps naming the same extent, at a
    /// different `offset` into it, and its back-reference is the same one —
    /// the key offset less the extent offset does not change — so cutting an
    /// extent in two leaves one reference counted twice, as btrfs does.
    fn drop_extents_inner(&mut self, ino: u64, start: u64, end: u64) -> Result<()> {
        let sector = u64::from(self.sectorsize());
        let mut removed = 0u64;
        let mut added = 0u64;
        for extent in self.extents_overlapping(ino, start, end)? {
            let key = BtrfsKey::new(ino, EXTENT_DATA_KEY, extent.start);
            // What is kept of the extent is made before it goes, so running
            // out of memory for it changes nothing.
            let mut kept = [None, None];
            if extent.is_inline() {
                // An inline extent covers the file's start; one that starts
                // before the cut keeps its leading bytes.
                if start > 0 {
                    let keep = usize::try_from(start.min(extent.ram_bytes))
                        .map_err(|_| Error::ItemTooLarge)?;
                    let head = FileExtentItem {
                        ram_bytes: keep as u64,
                        inline: fallible::copy(extent.inline.get(..keep).unwrap_or_default())?,
                        ..extent.piece(extent.start, extent.offset, extent.num_bytes)
                    };
                    let bytes = head.encode()?;
                    kept[0] = Some((head, bytes));
                }
            } else {
                let extent_end = extent.end(sector);
                if extent.start < start {
                    let head = extent.piece(extent.start, extent.offset, start - extent.start);
                    let bytes = head.encode()?;
                    kept[0] = Some((head, bytes));
                }
                if extent_end > end {
                    let offset = extent.offset.saturating_add(end - extent.start);
                    let tail = extent.piece(end, offset, extent_end - end);
                    let bytes = tail.encode()?;
                    kept[1] = Some((tail, bytes));
                }
            }
            self.delete(FS_TREE, &key)?;
            removed = removed.saturating_add(extent.counted_bytes());
            if let Some(backref) = extent.backref(ino) {
                self.refs
                    .add(extent.disk_bytenr, extent.disk_num_bytes, None, backref, -1)?;
            }
            for (piece, bytes) in kept.into_iter().flatten() {
                added = added.saturating_add(self.insert_piece(ino, &piece, bytes)?);
            }
        }
        if removed != 0 || added != 0 {
            let mut item = self.require_inode(ino)?;
            item.nbytes = item.nbytes.saturating_sub(removed).saturating_add(added);
            self.write_inode(ino, &item)?;
        }
        Ok(())
    }

    /// Insert one piece of a cut extent, whose item is `bytes`, with its
    /// reference. Returns what it adds to `nbytes`.
    fn insert_piece(&mut self, ino: u64, piece: &FileExtentItem, bytes: Vec<u8>) -> Result<u64> {
        let key = BtrfsKey::new(ino, EXTENT_DATA_KEY, piece.start);
        // FALLIBLE: the tree's own insert, which reports running out of memory.
        self.insert(FS_TREE, key, bytes)?;
        if let Some(backref) = piece.backref(ino) {
            self.refs
                .add(piece.disk_bytenr, piece.disk_num_bytes, None, backref, 1)?;
        }
        Ok(piece.counted_bytes())
    }

    /// Write `data` into file `ino` at `offset`, which is sector-aligned, and
    /// make the file at least `size` bytes long.
    ///
    /// `data` is whole sectors except perhaps the last, which is written
    /// padded with zeros: the file's size says where its bytes end. A file
    /// that is small enough, and written whole from the start, becomes one
    /// inline extent; anything else becomes regular extents, allocated fresh
    /// — btrfs never overwrites data in place — with their checksums.
    fn write_file_inner(&mut self, ino: u64, offset: u64, data: &[u8], size: u64) -> Result<()> {
        let sector = u64::from(self.sectorsize());
        if !offset.is_multiple_of(sector) {
            return Err(Error::Inconsistent("unaligned file write"));
        }
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or(Error::ItemTooLarge)?;
        let item = self.require_inode(ino)?;
        let size = size.max(item.size).max(end);
        if offset == 0 && end == size && size <= MAX_INLINE && size < sector && size > 0 {
            return self.write_inline(ino, data);
        }
        // The room is measured before anything changes: a write that ran out
        // half-way would leave half a file in the transaction, and abort it.
        // An inline extent, which only a file shorter than a sector has,
        // takes a sector of its own when it is turned regular below.
        let unlined = if offset > 0 && item.size > 0 && item.size < sector {
            sector
        } else {
            0
        };
        if self.data_room() < end.next_multiple_of(sector) - offset + unlined {
            return Err(Error::NoSpace);
        }
        // A write from the start replaces an inline extent outright; one
        // further in must keep its bytes, as a regular extent.
        if offset > 0 {
            self.unline(ino)?;
        }
        self.write_regular(ino, offset, data)?;
        let mut item = self.require_inode(ino)?;
        item.size = size;
        self.write_inode(ino, &item)
    }

    /// Write `data` at the sector-aligned `offset` as new regular extents,
    /// replacing whatever extents covered that range.
    fn write_regular(&mut self, ino: u64, offset: u64, data: &[u8]) -> Result<()> {
        let sector = u64::from(self.sectorsize());
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or(Error::ItemTooLarge)?;
        let aligned_end = end.next_multiple_of(sector);
        self.drop_extents(ino, offset, aligned_end)?;
        let mut done = 0u64;
        while offset.saturating_add(done) < aligned_end {
            let want = (aligned_end - offset - done).min(MAX_EXTENT);
            // The buffer is made before the space is taken, at the most the
            // extent can be, so running out of memory for it takes nothing.
            let mut chunk =
                fallible::with_capacity(usize::try_from(want).map_err(|_| Error::ItemTooLarge)?)?;
            let (at, len) = self.alloc_data(want, sector)?;
            let from = usize::try_from(done).map_err(|_| Error::ItemTooLarge)?;
            let upto =
                usize::try_from(done.saturating_add(len)).map_err(|_| Error::ItemTooLarge)?;
            fallible::extend_from_slice(
                &mut chunk,
                data.get(from..upto.min(data.len())).unwrap_or_default(),
            )?;
            fallible::resize(
                &mut chunk,
                usize::try_from(len).map_err(|_| Error::ItemTooLarge)?,
                0,
            )?;
            self.write_data(at, &chunk)?;
            let piece = FileExtentItem {
                start: offset.saturating_add(done),
                generation: self.transid,
                ram_bytes: len,
                compression: 0,
                kind: FILE_EXTENT_REG,
                inline: Vec::new(),
                disk_bytenr: at,
                disk_num_bytes: len,
                offset: 0,
                num_bytes: len,
            };
            let key = BtrfsKey::new(ino, EXTENT_DATA_KEY, piece.start);
            // FALLIBLE: the tree's own insert, which reports running out of memory.
            self.insert(FS_TREE, key, piece.encode()?)?;
            if let Some(backref) = piece.backref(ino) {
                self.refs.add(at, len, None, backref, 1)?;
            }
            let mut item = self.require_inode(ino)?;
            item.nbytes = item.nbytes.saturating_add(len);
            self.write_inode(ino, &item)?;
            done = done.saturating_add(len);
        }
        Ok(())
    }

    /// Turn an inline extent into a regular one, because the file is about to
    /// hold more than a leaf may.
    fn unline(&mut self, ino: u64) -> Result<()> {
        let key = BtrfsKey::new(ino, EXTENT_DATA_KEY, 0);
        let Some(data) = self.get(FS_TREE, &key)? else {
            return Ok(());
        };
        let extent = FileExtentItem::parse(&key, &data, self.sectorsize())?;
        if !extent.is_inline() {
            return Ok(());
        }
        let mut bytes =
            fallible::zeroed(usize::try_from(extent.ram_bytes).map_err(|_| Error::ItemTooLarge)?)?;
        self.copy_extent(&extent, 0, &mut bytes)?;
        self.drop_extents(ino, 0, u64::from(self.sectorsize()))?;
        self.write_regular(ino, 0, &bytes)
    }

    /// Write data to every copy of `[at, at + data.len())`, and its checksums
    /// into the checksum tree.
    fn write_data(&mut self, at: u64, data: &[u8]) -> Result<()> {
        for physical in self.chunks.copies(at, data.len() as u64)? {
            self.device.write_at(physical, data)?;
        }
        let sums: Vec<u32> =
            fallible::collect(data.chunks(self.sectorsize() as usize).map(crc32c))?;
        crate::csum::insert_sums(self, at, &sums)
    }

    /// Cut file `ino` to `size` bytes: extents past it go, and an inline
    /// extent is shortened. Bytes between `size` and the end of its sector
    /// must already be zero on disk or in the caller's cache; the caller
    /// writes that last sector back.
    fn truncate_inner(&mut self, ino: u64, size: u64) -> Result<()> {
        let sector = u64::from(self.sectorsize());
        let key = BtrfsKey::new(ino, EXTENT_DATA_KEY, 0);
        let inline = match self.get(FS_TREE, &key)? {
            Some(data) => Some(FileExtentItem::parse(&key, &data, self.sectorsize())?)
                .filter(FileExtentItem::is_inline),
            None => None,
        };
        match inline {
            Some(extent) if size > 0 && size < extent.ram_bytes => {
                let keep = extent
                    .inline
                    .get(..usize::try_from(size).map_err(|_| Error::ItemTooLarge)?)
                    .unwrap_or_default();
                self.write_inline(ino, keep)?;
            }
            Some(_) if size == 0 => self.drop_extents(ino, 0, u64::MAX)?,
            _ => {
                self.drop_extents(ino, size.next_multiple_of(sector), u64::MAX)?;
                self.zero_tail(ino, size)?;
            }
        }
        let mut item = self.require_inode(ino)?;
        item.size = size;
        self.write_inode(ino, &item)
    }

    /// Zero the bytes of the sector holding `size` that lie past it, so a
    /// file cut there and grown again reads zeros, not what it held before:
    /// Linux's `btrfs_truncate_block`. The sector is rewritten like any other.
    fn zero_tail(&mut self, ino: u64, size: u64) -> Result<()> {
        let sector = u64::from(self.sectorsize());
        let within = size % sector;
        if within == 0 {
            return Ok(());
        }
        let start = size - within;
        // A hole reads as zeros already.
        if self
            .extents_overlapping(ino, start, start + sector)?
            .is_empty()
        {
            return Ok(());
        }
        let mut buf = fallible::zeroed(usize::try_from(sector).map_err(|_| Error::ItemTooLarge)?)?;
        let read = self.read_file(ino, start, &mut buf)?;
        if read == 0 {
            return Ok(());
        }
        buf.get_mut(usize::try_from(within).map_err(|_| Error::ItemTooLarge)?..)
            .unwrap_or_default()
            .fill(0);
        self.write_regular(ino, start, &buf)
    }

    /// Read `ino`'s bytes from `offset` into `out` over this transaction's
    /// trees, decompressing as needed. Returns the length read, short only at
    /// the end of the file. What a writer needs to rewrite part of a sector;
    /// a VFS reads through `ferrix-btrfs`.
    pub fn read_file(&mut self, ino: u64, offset: u64, out: &mut [u8]) -> Result<usize> {
        let item = self.require_inode(ino)?;
        let len = usize::try_from(item.size.saturating_sub(offset))
            .map_or(out.len(), |rest| rest.min(out.len()));
        let out = out.get_mut(..len).unwrap_or_default();
        out.fill(0);
        let end = offset.saturating_add(len as u64);
        let sector = u64::from(self.sectorsize());
        for extent in self.extents_overlapping(ino, offset, end)? {
            let from = extent.start.max(offset);
            let to = extent.end(sector).min(end);
            let range = usize::try_from(from - offset).map_err(|_| Error::ItemTooLarge)?
                ..usize::try_from(to - offset).map_err(|_| Error::ItemTooLarge)?;
            let dest = out.get_mut(range).unwrap_or_default();
            self.copy_extent(&extent, from - extent.start, dest)?;
        }
        Ok(len)
    }

    /// Fill `dest` with an extent's file bytes from `skip` into its range.
    /// Holes and preallocated extents read as the zeros already there.
    fn copy_extent(&mut self, extent: &FileExtentItem, skip: u64, dest: &mut [u8]) -> Result<()> {
        // What the extent holds in the file's terms, and where in it the
        // file's bytes start: the inline bytes as they are, or what a
        // compressed extent expands to.
        let expanded;
        let (plain, start): (&[u8], usize) = if extent.is_inline() {
            if extent.compression == 0 {
                (&extent.inline, 0)
            } else {
                expanded = self.expand(extent.compression, &extent.inline, extent.ram_bytes)?;
                (&expanded, 0)
            }
        } else if extent.disk_bytenr == 0 || extent.kind != FILE_EXTENT_REG {
            return Ok(());
        } else if extent.compression == 0 {
            let at = extent
                .disk_bytenr
                .saturating_add(extent.offset)
                .saturating_add(skip);
            return self.read_data(at, dest);
        } else {
            let mut stored = fallible::zeroed(
                usize::try_from(extent.disk_num_bytes).map_err(|_| Error::ItemTooLarge)?,
            )?;
            self.read_data(extent.disk_bytenr, &mut stored)?;
            expanded = self.expand(extent.compression, &stored, extent.ram_bytes)?;
            let from = usize::try_from(extent.offset).map_err(|_| Error::ItemTooLarge)?;
            (&expanded, from)
        };
        let src = plain
            .get(start..)
            .unwrap_or_default()
            .get(usize::try_from(skip).unwrap_or(usize::MAX)..)
            .unwrap_or_default();
        let n = dest.len().min(src.len());
        if let (Some(d), Some(s)) = (dest.get_mut(..n), src.get(..n)) {
            d.copy_from_slice(s);
        }
        Ok(())
    }

    /// Read `dest.len()` bytes of data at logical `at` from its first copy.
    fn read_data(&mut self, at: u64, dest: &mut [u8]) -> Result<()> {
        let physical = self
            .chunks
            .copies(at, dest.len() as u64)?
            .first()
            .copied()
            .ok_or(Error::Inconsistent("data extent has no copy"))?;
        Ok(self
            .device
            .read_at(physical, dest, ferrix_btrfs::volume::ReadKind::Data)?)
    }

    /// Decompress an extent's stored bytes into `ram_bytes` bytes.
    fn expand(&self, compression: u8, input: &[u8], ram_bytes: u64) -> Result<Vec<u8>> {
        let mut out =
            fallible::zeroed(usize::try_from(ram_bytes).map_err(|_| Error::ItemTooLarge)?)?;
        let mut work = fallible::zeroed(ferrix_btrfs::compress::zstd::Workspace::SIZE)?;
        let mut workspace = ferrix_btrfs::compress::zstd::Workspace::new(&mut work)?;
        let len = ferrix_btrfs::compress::decompress(
            compression,
            input,
            &mut out,
            self.sectorsize(),
            &mut workspace,
        )?;
        out.truncate(len);
        Ok(out)
    }
}

/// The operations a VFS calls. Each runs as one operation: if it fails after
/// changing anything, the transaction is aborted rather than left holding
/// half of it. The documentation of each is on the function it wraps.
impl<D: WriteDevice> WriteVolume<D> {
    /// Create a file, directory, symlink or device node named `name` in
    /// `dir`, returning its inode number; [`Error::Exists`] if the name is
    /// taken. A symlink's target follows with [`WriteVolume::set_symlink`].
    pub fn create(&mut self, dir: u64, name: &[u8], new: &NewInode) -> Result<u64> {
        self.operation(|volume| volume.create_inner(dir, name, new))
    }

    /// Give `ino` the further name `name` in `dir`.
    pub fn link(&mut self, dir: u64, name: &[u8], ino: u64, now: Timespec) -> Result<()> {
        self.operation(|volume| volume.link_inner(dir, name, ino, now))
    }

    /// Remove the name `name` from `dir`, returning the inode it named, which
    /// becomes an orphan if that was its last name.
    pub fn unlink(&mut self, dir: u64, name: &[u8], now: Timespec) -> Result<u64> {
        self.operation_with(Need::FREEING, |volume| volume.unlink_inner(dir, name, now))
    }

    /// Move `old` in `old_dir` to `new` in `new_dir`, replacing `new`.
    pub fn rename(
        &mut self,
        old_dir: u64,
        old: &[u8],
        new_dir: u64,
        new: &[u8],
        now: Timespec,
    ) -> Result<()> {
        self.operation(|volume| volume.rename_inner(old_dir, old, new_dir, new, now))
    }

    /// Delete an orphan inode nothing holds open any more.
    pub fn evict(&mut self, ino: u64) -> Result<()> {
        self.operation_with(Need::FREEING, |volume| volume.evict_inner(ino))
    }

    /// Store `target` as symlink `ino`'s contents.
    pub fn set_symlink(&mut self, ino: u64, target: &[u8]) -> Result<()> {
        self.operation(|volume| volume.set_symlink_inner(ino, target))
    }

    /// Remove `[start, end)` from file `ino`'s extents.
    pub fn drop_extents(&mut self, ino: u64, start: u64, end: u64) -> Result<()> {
        self.operation_with(Need::FREEING, |volume| {
            volume.drop_extents_inner(ino, start, end)
        })
    }

    /// Write `data` into file `ino` at the sector-aligned `offset`, making it
    /// at least `size` bytes long.
    pub fn write_file(&mut self, ino: u64, offset: u64, data: &[u8], size: u64) -> Result<()> {
        self.operation_with(Need::data(data.len() as u64), |volume| {
            volume.write_file_inner(ino, offset, data, size)
        })
    }

    /// Cut file `ino` to `size` bytes.
    pub fn truncate(&mut self, ino: u64, size: u64) -> Result<()> {
        self.operation_with(Need::FREEING, |volume| volume.truncate_inner(ino, size))
    }
}

/// `payload` with the first record whose name is `name` taken out. `record`
/// parses the record at an offset into its name and its length.
fn without_record(
    payload: &[u8],
    name: &[u8],
    record: impl for<'a> Fn(&'a [u8], usize) -> Option<(&'a [u8], usize)>,
) -> Result<Vec<u8>> {
    let mut at = 0usize;
    while at < payload.len() {
        let (found, len) =
            record(payload, at).ok_or(Error::Inconsistent("malformed name record"))?;
        let next = at.saturating_add(len);
        if found == name {
            let before = payload.get(..at).unwrap_or_default();
            let after = payload.get(next..).unwrap_or_default();
            let mut kept = fallible::with_capacity(before.len().saturating_add(after.len()))?;
            fallible::extend_from_slice(&mut kept, before)?;
            fallible::extend_from_slice(&mut kept, after)?;
            return Ok(kept);
        }
        at = next;
    }
    Err(Error::Inconsistent("name record not found"))
}
