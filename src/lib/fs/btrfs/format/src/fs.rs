//! Files and directories, as a read-only filesystem is asked for them.
//!
//! Inside one fs tree an inode number is an object id, and everything about
//! that inode sorts together under it: the `INODE_ITEM` with its stat data,
//! the `INODE_REF`s naming it, and — for a directory — its entries, or — for a
//! file or symlink — its `EXTENT_DATA` items keyed by file offset.
//!
//! # A directory holds each entry twice
//!
//! Once as a `DIR_ITEM` keyed by the CRC-32C of the name, which is what makes
//! a lookup one seek; and once as a `DIR_INDEX` keyed by a sequence number
//! that only grows, which is what makes `readdir` resumable from a cursor that
//! stays valid while entries are added and removed. [`Subvolume::lookup`] uses
//! the first and [`Subvolume::read_dir`] the second. Index numbers start at 2,
//! leaving 0 and 1 to the `.` and `..` a VFS emits itself.
//!
//! # A file is extents, and gaps
//!
//! Each `EXTENT_DATA` covers a range of the file starting at its key's offset.
//! A range with no item is a hole when the volume has `NO_HOLES`, and so is a
//! regular extent with a zero disk address, and so is a preallocated extent:
//! all three read as zeroes. A read therefore zero-fills first and copies the
//! extents it finds over the top, which handles every hole the same way and
//! cannot leave stale bytes in a range nothing covered.
//!
//! # Compressed extents are decompressed whole
//!
//! A compressed extent cannot be entered in the middle, so reading 4 KiB of it
//! means expanding up to 128 KiB. [`ReadBuffers`] keeps the last extent it
//! expanded, which turns a sequential read through a compressed file from one
//! decompression per call into one per extent.
//!
//! # Subvolumes are named, not entered
//!
//! A directory entry can point at the root of another subvolume. This reader
//! reports that as [`Target::Subvolume`] and goes no further: crossing into it
//! is a mount decision, and stage C's.

use core::ops::ControlFlow;

use crate::chunk::ChunkStorage;
use crate::compress::{self, MAX_UNCOMPRESSED};
use crate::crc32c::crc32c;
use crate::items::{
    CsumItem, DIR_INDEX_KEY, DIR_ITEM_KEY, DirItem, DirItemIter, EXTENT_CSUM_KEY,
    EXTENT_CSUM_OBJECTID, EXTENT_DATA_KEY, ExtentData, ExtentDataBody, FileExtent, INODE_ITEM_KEY,
    INODE_NODATASUM, InodeItem, ROOT_ITEM_KEY, name_hash,
};
use crate::tree::BtrfsKey;
use crate::volume::{Device, ReadKind, TreeRoot, Volume};
use crate::{BtrfsError, truncated};

/// What a directory entry points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// An inode in the same tree, by number.
    Inode(u64),
    /// The root of another subvolume, by tree id. Not crossed by this reader.
    Subvolume(u64),
}

/// A directory entry found by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    /// What the name refers to.
    pub target: Target,
    /// The entry's file type, one of the `FT_*` constants in
    /// [`crate::items`], recorded so a caller need not read the inode.
    pub kind: u8,
}

/// A directory entry produced by [`Subvolume::read_dir`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirEntry<'a> {
    /// The entry's `DIR_INDEX` sequence number. Resuming a listing at
    /// `index + 1` continues after this entry.
    pub index: u64,
    /// The name, as raw bytes.
    pub name: &'a [u8],
    /// What the name refers to.
    pub target: Target,
    /// The entry's file type, one of the `FT_*` constants.
    pub kind: u8,
}

/// The memory a file read works in beyond its node buffer.
///
/// Two buffers of at least [`MAX_UNCOMPRESSED`] bytes, one of at least
/// [`compress::zstd::Workspace::SIZE`] for zstd's tables, and a node buffer of
/// at least the volume's `nodesize` for the checksum tree, handed out as four
/// disjoint borrows. The checksum tree needs a node buffer of its own because
/// data is checked from inside the walk over the file's extents, which holds
/// the read's main one. A tuple of borrows is one; a mount implements it over
/// buffers it owns.
pub trait ExtentBuffers {
    /// The buffer compressed bytes, and data being checked, are read into; the
    /// buffer compressed bytes expand into; the bytes zstd builds its tables
    /// in; and the checksum tree's node buffer.
    fn parts(&mut self) -> (&mut [u8], &mut [u8], &mut [u8], &mut [u8]);
}

impl ExtentBuffers for (&mut [u8], &mut [u8], &mut [u8], &mut [u8]) {
    fn parts(&mut self) -> (&mut [u8], &mut [u8], &mut [u8], &mut [u8]) {
        (&mut *self.0, &mut *self.1, &mut *self.2, &mut *self.3)
    }
}

/// The working memory a file read needs beyond the node buffer.
///
/// Two 128 KiB buffers, the zstd workspace and a checksum-tree node buffer:
/// too large for a kernel stack,
/// and worth keeping between reads because of the extent cache. It owns its
/// [`ExtentBuffers`] rather than borrowing them, so a mount can keep one
/// across calls: rebuilt around borrowed buffers on every call, the cache
/// would never hit, and carrying the cache over to buffers it did not fill
/// would hand back somebody else's bytes.
#[derive(Debug)]
pub struct ReadBuffers<B> {
    buffers: B,
    cached: Option<Expanded>,
}

/// Which extent `plain` currently holds, and how much of it decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Expanded {
    disk_bytenr: u64,
    disk_num_bytes: u64,
    compression: u8,
    len: usize,
    /// Whether the compressed bytes were checked against the checksum tree
    /// before they were expanded. An extent shared with a `NODATASUM` file is
    /// expanded unchecked for it, and must not be handed to a file whose data
    /// is checksummed without being checked first.
    verified: bool,
}

impl<B: ExtentBuffers> ReadBuffers<B> {
    /// Take over caller-allocated memory: the two extent buffers must hold at
    /// least [`MAX_UNCOMPRESSED`] bytes, and zstd's at least [`compress::zstd::Workspace::SIZE`].
    pub fn new(mut buffers: B) -> Result<Self, BtrfsError> {
        let (compressed, plain, zstd, _) = buffers.parts();
        let shortest = compressed.len().min(plain.len());
        if shortest < MAX_UNCOMPRESSED {
            return Err(truncated(MAX_UNCOMPRESSED, shortest));
        }
        // Checked now rather than at the first zstd extent, so memory set up
        // too small fails where it was set up, not in the middle of a read.
        let _: compress::zstd::Workspace<'_> = compress::zstd::Workspace::new(zstd)?;
        Ok(ReadBuffers {
            buffers,
            cached: None,
        })
    }

    /// Expand compressed bytes that live in the item itself.
    fn expand_inline(
        &mut self,
        compression: u8,
        data: &[u8],
        ram_bytes: u64,
        sectorsize: u32,
    ) -> Result<&[u8], BtrfsError> {
        // The buffer is about to be overwritten, so whatever it cached is gone.
        self.cached = None;
        let (_, plain, zstd, _) = self.buffers.parts();
        let out = plain
            .get_mut(..expanded_len(ram_bytes)?)
            .unwrap_or_default();
        let mut workspace = compress::zstd::Workspace::new(zstd)?;
        let len = compress::decompress(compression, data, out, sectorsize, &mut workspace)?;
        Ok(plain.get(..len).unwrap_or_default())
    }

    /// Expand a compressed regular extent, or reuse it if it is the one held.
    ///
    /// With `verify`, the compressed bytes are checked against the checksum
    /// tree before they are expanded, so damage is reported as a checksum
    /// failure rather than as whatever the decoder makes of it.
    fn expand_extent<D: Device, S: ChunkStorage>(
        &mut self,
        volume: &Volume<S>,
        device: &mut D,
        extent: &ExtentData<'_>,
        file: &FileExtent,
        verify: bool,
    ) -> Result<&[u8], BtrfsError> {
        let wanted = Expanded {
            disk_bytenr: file.disk_bytenr,
            disk_num_bytes: file.disk_num_bytes,
            compression: extent.compression,
            len: 0,
            verified: verify,
        };
        let hit = self.cached.filter(|held| {
            let same = Expanded {
                len: 0,
                verified: verify,
                ..*held
            } == wanted;
            same && (held.verified || !verify)
        });
        let (compressed, plain, zstd, node) = self.buffers.parts();
        let len = match hit {
            Some(held) => held.len,
            None => {
                // Forgotten before the read, so a failed read or decode cannot
                // leave the cache naming bytes the buffer no longer holds.
                self.cached = None;
                let stored = usize::try_from(file.disk_num_bytes)
                    .ok()
                    .filter(|&n| n <= MAX_UNCOMPRESSED)
                    .ok_or(BtrfsError::BadItem {
                        item_type: EXTENT_DATA_KEY,
                    })?;
                let input = compressed.get_mut(..stored).unwrap_or_default();
                if verify {
                    read_checked(volume, device, file.disk_bytenr, input, node)?;
                } else {
                    volume.read_logical(device, file.disk_bytenr, input, ReadKind::Data)?;
                }
                let input = compressed.get(..stored).unwrap_or_default();
                let out = plain
                    .get_mut(..expanded_len(extent.ram_bytes)?)
                    .unwrap_or_default();
                let mut workspace = compress::zstd::Workspace::new(zstd)?;
                let len = compress::decompress(
                    extent.compression,
                    input,
                    out,
                    volume.sectorsize(),
                    &mut workspace,
                )?;
                self.cached = Some(Expanded { len, ..wanted });
                len
            }
        };
        Ok(plain.get(..len).unwrap_or_default())
    }

    /// Read `dest.len()` bytes of uncompressed data at logical address `at`,
    /// checking every sector they touch against the checksum tree first.
    ///
    /// Whole sectors are read into the compressed-extent buffer, at most
    /// [`MAX_UNCOMPRESSED`] bytes at a time, checked, and only the part asked
    /// for is copied out. That buffer holds nothing the extent cache names, so
    /// using it here loses nothing.
    fn read_verified<D: Device, S: ChunkStorage>(
        &mut self,
        volume: &Volume<S>,
        device: &mut D,
        at: u64,
        dest: &mut [u8],
    ) -> Result<(), BtrfsError> {
        let sector = u64::from(volume.sectorsize());
        let (scratch, _, _, node) = self.buffers.parts();
        let room = scratch.len().min(MAX_UNCOMPRESSED) as u64;
        let window = room - room % sector.max(1);
        if sector == 0 || window == 0 {
            return Err(truncated(sector as usize, scratch.len()));
        }
        let end = at
            .checked_add(dest.len() as u64)
            .ok_or(BtrfsError::NotMapped(at))?;
        let mut done = 0usize;
        while done < dest.len() {
            let want = at
                .checked_add(done as u64)
                .ok_or(BtrfsError::NotMapped(at))?;
            let first = want - want % sector;
            let last = end
                .checked_add(sector - 1)
                .map(|e| e - e % sector)
                .ok_or(BtrfsError::NotMapped(at))?;
            let span = (last - first).min(window);
            let buf = scratch.get_mut(..span as usize).unwrap_or_default();
            read_checked(volume, device, first, buf, node)?;
            let buf = &*buf;
            let skip = (want - first) as usize;
            let take = (dest.len() - done).min(buf.len().saturating_sub(skip));
            if take == 0 {
                return Err(BtrfsError::NotMapped(at));
            }
            if let (Some(to), Some(from)) =
                (dest.get_mut(done..done + take), buf.get(skip..skip + take))
            {
                to.copy_from_slice(from);
            }
            done += take;
        }
        Ok(())
    }
}

/// One subvolume's tree, read through a [`Volume`].
#[derive(Debug)]
pub struct Subvolume<'v, S> {
    volume: &'v Volume<S>,
    tree: TreeRoot,
    root_dir: u64,
}

// Written out rather than derived: a derive would demand `S: Copy`, and a
// `Subvolume` is only a reference and two numbers whatever `S` is.
impl<S> Clone for Subvolume<'_, S> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<S> Copy for Subvolume<'_, S> {}

impl<S: ChunkStorage> Volume<S> {
    /// The subvolume mounted by default: the one the root tree names, or the
    /// top-level fs tree when it names none.
    #[must_use]
    pub const fn default_subvolume(&self) -> Subvolume<'_, S> {
        Subvolume {
            volume: self,
            tree: self.fs_tree(),
            root_dir: self.root_dir(),
        }
    }
}

impl<S: ChunkStorage> Subvolume<'_, S> {
    /// Inode number of the subvolume's root directory.
    #[must_use]
    pub const fn root_dir(&self) -> u64 {
        self.root_dir
    }

    /// The stat data of inode `ino`, or `None` if the tree has no such inode.
    pub fn inode<D: Device>(
        &self,
        device: &mut D,
        ino: u64,
        node: &mut [u8],
    ) -> Result<Option<InodeItem>, BtrfsError> {
        let key = BtrfsKey::new(ino, INODE_ITEM_KEY, 0);
        let found = self.volume.walk(device, self.tree, key, node, |_, item| {
            if item.key != key {
                return Ok(ControlFlow::Break(None));
            }
            Ok(ControlFlow::Break(Some(InodeItem::parse(item.data)?)))
        })?;
        Ok(found.flatten())
    }

    /// Find `name` in directory `dir`.
    ///
    /// `None` is an ordinary miss. Names that collide under the hash share one
    /// `DIR_ITEM`, so every entry in it is compared.
    pub fn lookup<D: Device>(
        &self,
        device: &mut D,
        dir: u64,
        name: &[u8],
        node: &mut [u8],
    ) -> Result<Option<Entry>, BtrfsError> {
        let key = BtrfsKey::new(dir, DIR_ITEM_KEY, name_hash(name));
        let found = self.volume.walk(device, self.tree, key, node, |_, item| {
            if item.key != key {
                return Ok(ControlFlow::Break(None));
            }
            Ok(ControlFlow::Break(entry_named(item.data, &key, name)?))
        })?;
        Ok(found.flatten())
    }

    /// List directory `dir` from sequence number `from`, handing each entry to
    /// `emit` until it breaks or the directory ends.
    pub fn read_dir<D: Device>(
        &self,
        device: &mut D,
        dir: u64,
        from: u64,
        node: &mut [u8],
        mut emit: impl FnMut(DirEntry<'_>) -> ControlFlow<()>,
    ) -> Result<(), BtrfsError> {
        let start = BtrfsKey::new(dir, DIR_INDEX_KEY, from);
        // Whether the listing ended or `emit` stopped it is the caller's to know.
        let _: Option<()> = self
            .volume
            .walk(device, self.tree, start, node, |_, item| {
                if item.key.objectid != dir || item.key.item_type != DIR_INDEX_KEY {
                    return Ok(ControlFlow::Break(()));
                }
                // A `DIR_INDEX` holds exactly one entry; hash collisions only ever
                // share a `DIR_ITEM`.
                let Some(entry) = DirItemIter::new(item.data, DIR_INDEX_KEY)
                    .next()
                    .transpose()?
                else {
                    return Err(BtrfsError::BadItem {
                        item_type: DIR_INDEX_KEY,
                    });
                };
                check_name(entry.name, DIR_INDEX_KEY)?;
                Ok(emit(DirEntry {
                    index: item.key.offset,
                    name: entry.name,
                    target: target_of(&entry)?,
                    kind: entry.kind,
                }))
            })?;
        Ok(())
    }

    /// Read file `ino` from byte `offset` into `out`, returning the length.
    ///
    /// Stops at end of file, so a short count means end of file and nothing
    /// else. A symlink's target is read the same way, from offset zero.
    pub fn read<D: Device, B: ExtentBuffers>(
        &self,
        device: &mut D,
        ino: u64,
        offset: u64,
        out: &mut [u8],
        node: &mut [u8],
        buffers: &mut ReadBuffers<B>,
    ) -> Result<usize, BtrfsError> {
        let inode = self
            .inode(device, ino, node)?
            .ok_or(BtrfsError::MissingInode(ino))?;
        // A file flagged `NODATASUM` has no checksums to check its data against.
        let verify = inode.flags & INODE_NODATASUM == 0;
        let remaining = inode.size.saturating_sub(offset);
        let len = usize::try_from(remaining).map_or(out.len(), |r| r.min(out.len()));
        let out = out.get_mut(..len).unwrap_or_default();
        out.fill(0);
        if len == 0 {
            return Ok(0);
        }
        let end = offset.saturating_add(len as u64);
        let probe = BtrfsKey::new(ino, EXTENT_DATA_KEY, offset);
        let start = self
            .volume
            .last_at_or_before(device, self.tree, &probe, node)?
            .filter(|key| key.objectid == ino && key.item_type == EXTENT_DATA_KEY)
            .unwrap_or(BtrfsKey::new(ino, EXTENT_DATA_KEY, 0));
        let sectorsize = self.volume.sectorsize();
        // Where the previous extent of this walk ended. Linux's
        // `check_extent_data_item` refuses an extent starting before it: two
        // extents claiming one file range would make the bytes a read returns
        // depend on which it copied last.
        let mut covered: Option<u64> = None;
        // Every way out of the walk leaves `out` complete: extents past `end`
        // break it, and running out of items leaves the zero fill.
        let _: Option<()> = self
            .volume
            .walk(device, self.tree, start, node, |device, item| {
                let key = item.key;
                if key.objectid != ino || key.item_type != EXTENT_DATA_KEY || key.offset >= end {
                    return Ok(ControlFlow::Break(()));
                }
                let bad = BtrfsError::BadItem {
                    item_type: EXTENT_DATA_KEY,
                };
                let extent = ExtentData::parse_item(&key, item.data, sectorsize)?;
                if covered.is_some_and(|previous| previous > key.offset) {
                    return Err(bad);
                }
                covered = Some(extent.end(&key, sectorsize).ok_or(bad)?);
                let place = Placement::new(key.offset, &extent, offset, out.len());
                if let Some(place) = place {
                    self.copy_extent(device, &extent, place, out, buffers, verify)?;
                }
                Ok(ControlFlow::Continue(()))
            })?;
        Ok(len)
    }

    /// Copy the part of one extent that `place` selects into `out`, checking
    /// data read from disk against the checksum tree when `verify` is set.
    ///
    /// Inline extents are covered by their node's checksum, and holes and
    /// preallocated extents have no data to check.
    fn copy_extent<D: Device, B: ExtentBuffers>(
        &self,
        device: &mut D,
        extent: &ExtentData<'_>,
        place: Placement,
        out: &mut [u8],
        buffers: &mut ReadBuffers<B>,
        verify: bool,
    ) -> Result<(), BtrfsError> {
        let dest = out
            .get_mut(place.dest_start..place.dest_end)
            .ok_or(BtrfsError::BadItem {
                item_type: EXTENT_DATA_KEY,
            })?;
        match extent.body {
            ExtentDataBody::Prealloc(_) => Ok(()),
            ExtentDataBody::Regular(file) if file.is_hole() => Ok(()),
            ExtentDataBody::Inline(data) if extent.is_uncompressed() => {
                copy_from(dest, data, place.skip);
                Ok(())
            }
            ExtentDataBody::Inline(data) => {
                let sectorsize = self.volume.sectorsize();
                let plain = buffers.expand_inline(
                    extent.compression,
                    data,
                    extent.ram_bytes,
                    sectorsize,
                )?;
                copy_from(dest, plain, place.skip);
                Ok(())
            }
            ExtentDataBody::Regular(file) if extent.is_uncompressed() => {
                let at = file
                    .disk_bytenr
                    .checked_add(file.offset)
                    .and_then(|a| a.checked_add(place.skip))
                    .ok_or(BtrfsError::NotMapped(file.disk_bytenr))?;
                if verify {
                    buffers.read_verified(self.volume, device, at, dest)
                } else {
                    self.volume.read_logical(device, at, dest, ReadKind::Data)
                }
            }
            ExtentDataBody::Regular(file) => {
                let plain = buffers.expand_extent(self.volume, device, extent, &file, verify)?;
                copy_from(dest, plain, file.offset.saturating_add(place.skip));
                Ok(())
            }
        }
    }
}

/// Where one extent lands in a read.
#[derive(Debug, Clone, Copy)]
struct Placement {
    /// How far into the extent's file range the copied part begins.
    skip: u64,
    /// The copied part's range within the read buffer.
    dest_start: usize,
    dest_end: usize,
}

impl Placement {
    /// Intersect the extent starting at file offset `start` with a read of
    /// `len` bytes at `offset`. `None` when they do not overlap.
    fn new(start: u64, extent: &ExtentData<'_>, offset: u64, len: usize) -> Option<Placement> {
        let covers = match extent.body {
            ExtentDataBody::Inline(data) if extent.is_uncompressed() => data.len() as u64,
            ExtentDataBody::Inline(_) => extent.ram_bytes,
            ExtentDataBody::Regular(file) | ExtentDataBody::Prealloc(file) => file.num_bytes,
        };
        let from = start.max(offset);
        let to = start
            .saturating_add(covers)
            .min(offset.saturating_add(len as u64));
        if from >= to {
            return None;
        }
        Some(Placement {
            skip: from.checked_sub(start)?,
            dest_start: usize::try_from(from.checked_sub(offset)?).ok()?,
            dest_end: usize::try_from(to.checked_sub(offset)?).ok()?,
        })
    }
}

/// Bytes in one CRC-32C in an `EXTENT_CSUM` payload, the only checksum type a
/// volume this crate mounts can have.
const CRC32C_SIZE: usize = 4;

/// What an `EXTENT_CSUM` item that breaks Linux's `check_csum_item` is.
const BAD_CSUM_ITEM: BtrfsError = BtrfsError::BadItem {
    item_type: EXTENT_CSUM_KEY,
};

/// Read `buf.len()` bytes of data at logical address `logical` and check them
/// against the checksum tree, from the first copy that passes.
///
/// Linux's read repair tries every mirror of a data sector whose checksum
/// fails before the read fails with `EIO`; so does this, for a failed read
/// too. Any other error — a damaged checksum tree — is the same whichever
/// copy is read, and is returned at once. Only when every copy fails is the
/// read an error, the first copy's. Nothing is written back.
fn read_checked<D: Device, S: ChunkStorage>(
    volume: &Volume<S>,
    device: &mut D,
    logical: u64,
    buf: &mut [u8],
    node: &mut [u8],
) -> Result<(), BtrfsError> {
    let mut first = None;
    for copy in 0..volume.copies(logical).max(1) {
        let checked = volume
            .read_copy(device, logical, buf, ReadKind::Data, copy)
            .and_then(|()| verify_sectors(volume, device, logical, buf, node));
        match checked {
            Ok(()) => return Ok(()),
            Err(error @ (BtrfsError::DataChecksum { .. } | BtrfsError::DeviceRead { .. })) => {
                let _ = first.get_or_insert(error);
            }
            Err(error) => return Err(error),
        }
    }
    Err(first.unwrap_or(BtrfsError::NotMapped(logical)))
}

/// Check the whole data sectors in `data`, starting at logical address
/// `logical`, against the checksum tree.
///
/// A sector the tree has no checksum for is compared with zero, which is how
/// Linux reads it: `btrfs_lookup_bio_sums` in `fs/btrfs/file-item.c` fills a
/// missing checksum with zeros and warns of a "csum hole", and
/// `btrfs_data_csum_ok` in `fs/btrfs/inode.c` then compares the sector against
/// those zeros and fails the read. Only the data-relocation tree reads a hole
/// as unchecksummed, and this reader never reads that tree.
///
/// The items walked are held to Linux's `check_csum_item`: a key offset on
/// the sector grid, a payload a whole number of checksums long, and no item
/// starting before the one ahead of it ends.
fn verify_sectors<D: Device, S: ChunkStorage>(
    volume: &Volume<S>,
    device: &mut D,
    logical: u64,
    data: &[u8],
    node: &mut [u8],
) -> Result<(), BtrfsError> {
    let sectorsize = volume.sectorsize();
    let sector = u64::from(sectorsize);
    let step = sectorsize as usize;
    if step == 0 || !data.len().is_multiple_of(step) || !logical.is_multiple_of(sector) {
        return Err(BtrfsError::BadItem {
            item_type: EXTENT_DATA_KEY,
        });
    }
    let end = logical
        .checked_add(data.len() as u64)
        .ok_or(BtrfsError::NotMapped(logical))?;
    let check = |at: u64, stored: u32| -> Result<(), BtrfsError> {
        let from = usize::try_from(at - logical).map_err(|_| BtrfsError::NotMapped(at))?;
        let bytes = data
            .get(from..from.saturating_add(step))
            .ok_or(BtrfsError::NotMapped(at))?;
        let computed = crc32c(bytes);
        if computed == stored {
            Ok(())
        } else {
            Err(BtrfsError::DataChecksum {
                logical: at,
                stored,
                computed,
            })
        }
    };

    let tree = volume.csum_tree();
    let probe = BtrfsKey::new(EXTENT_CSUM_OBJECTID, EXTENT_CSUM_KEY, logical);
    let start = volume
        .last_at_or_before(device, tree, &probe, node)?
        .filter(|key| key.objectid == EXTENT_CSUM_OBJECTID && key.item_type == EXTENT_CSUM_KEY)
        .unwrap_or(probe);
    // The first sector not yet checked, and where the last item walked ended.
    let mut next = logical;
    let mut previous_end: Option<u64> = None;
    let _: Option<()> = volume.walk(device, tree, start, node, |_, item| {
        let key = item.key;
        if key.objectid != EXTENT_CSUM_OBJECTID
            || key.item_type != EXTENT_CSUM_KEY
            || key.offset >= end
        {
            return Ok(ControlFlow::Break(()));
        }
        if !key.offset.is_multiple_of(sector) || !item.data.len().is_multiple_of(CRC32C_SIZE) {
            return Err(BAD_CSUM_ITEM);
        }
        if previous_end.is_some_and(|previous| previous > key.offset) {
            return Err(BAD_CSUM_ITEM);
        }
        let sums = CsumItem::new(key.offset, sectorsize, item.data)?;
        let covered = (sums.len() as u64)
            .checked_mul(sector)
            .and_then(|len| key.offset.checked_add(len))
            .ok_or(BAD_CSUM_ITEM)?;
        previous_end = Some(covered);
        while next < key.offset.min(end) {
            check(next, 0)?;
            next += sector;
        }
        while next < covered.min(end) {
            check(next, sums.checksum_for(next).ok_or(BAD_CSUM_ITEM)?)?;
            next += sector;
        }
        Ok(ControlFlow::Continue(()))
    })?;
    while next < end {
        check(next, 0)?;
        next += sector;
    }
    Ok(())
}

/// Copy `src[at..]` into the front of `dest`, as much as both have. Whatever
/// is not covered stays as it was, which for a read is zero.
fn copy_from(dest: &mut [u8], src: &[u8], at: u64) {
    let tail = usize::try_from(at)
        .ok()
        .and_then(|at| src.get(at..))
        .unwrap_or_default();
    let len = dest.len().min(tail.len());
    if let (Some(to), Some(from)) = (dest.get_mut(..len), tail.get(..len)) {
        to.copy_from_slice(from);
    }
}

/// The buffer length a compressed extent of `ram_bytes` expands into.
fn expanded_len(ram_bytes: u64) -> Result<usize, BtrfsError> {
    usize::try_from(ram_bytes)
        .ok()
        .filter(|&n| n <= MAX_UNCOMPRESSED)
        .ok_or(BtrfsError::BadItem {
            item_type: EXTENT_DATA_KEY,
        })
}

/// The entry in the `DIR_ITEM` payload filed under `key` whose name is
/// `name`, if any.
///
/// Every entry in the item is checked, not only the one asked for, and each
/// must hash to the key it is filed under — Linux's `check_dir_item` makes the
/// same comparison. An entry whose name does not hash to its key is one no
/// lookup of its own name can reach, so it was not put there by btrfs; it is
/// reported as damage rather than skipped as a miss.
pub(crate) fn entry_named(
    payload: &[u8],
    key: &BtrfsKey,
    name: &[u8],
) -> Result<Option<Entry>, BtrfsError> {
    let mut found = None;
    for entry in DirItemIter::new(payload, DIR_ITEM_KEY) {
        let entry = entry?;
        if name_hash(entry.name) != key.offset {
            return Err(BtrfsError::BadItem {
                item_type: DIR_ITEM_KEY,
            });
        }
        check_name(entry.name, DIR_ITEM_KEY)?;
        if found.is_none() && entry.name == name {
            found = Some(Entry {
                target: target_of(&entry)?,
                kind: entry.kind,
            });
        }
    }
    Ok(found)
}

/// Refuse a directory entry name that no directory can hold.
///
/// An empty name, `.` or `..`, or one containing `/` or NUL. Linux's
/// tree-checker bounds a name's length and leaves its bytes alone, because on
/// Linux the bytes are checked on the way out: `verify_dirent_name` in
/// `fs/readdir.c` fails `getdents` with `EIO` for an empty name or one with a
/// `/` in it. This reader's names go to a VFS instead, which hands them to
/// `getdents64` and matches them in path walks as they are, and which emits
/// `.` and `..` itself. A `/` there is a name that cannot be looked up, a NUL
/// cuts it short for every C caller, and an on-disk `..` would list a second
/// parent. So the check this reader cannot leave to anyone else is made here,
/// for listings and lookups alike, and reported as damage.
fn check_name(name: &[u8], item_type: u8) -> Result<(), BtrfsError> {
    let special = name.is_empty() || name == b"." || name == b"..";
    if special || name.iter().any(|&byte| byte == b'/' || byte == 0) {
        Err(BtrfsError::BadItem { item_type })
    } else {
        Ok(())
    }
}

/// Decode what a directory entry's location key points at.
fn target_of(entry: &DirItem<'_>) -> Result<Target, BtrfsError> {
    match entry.location.item_type {
        INODE_ITEM_KEY => Ok(Target::Inode(entry.location.objectid)),
        ROOT_ITEM_KEY => Ok(Target::Subvolume(entry.location.objectid)),
        _ => Err(BtrfsError::BadItem {
            item_type: DIR_ITEM_KEY,
        }),
    }
}

#[cfg(test)]
mod tests;
