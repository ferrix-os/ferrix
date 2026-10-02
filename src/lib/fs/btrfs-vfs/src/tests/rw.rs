//! Tests for the writable mount, through the [`Inode`] trait the VFS calls.
//!
//! What they check is that the trait's contract holds over a real volume:
//! what a write puts in comes back out of a read, out of a fresh mount after
//! a commit, and out of the stage 11 reader, which knows nothing of this
//! crate; and that a file unlinked while something holds it stays readable
//! until that goes, as POSIX says.

extern crate std;

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::vec;

use ferrix_btrfs::chunk::ChunkMapEntry;
use ferrix_btrfs::volume::{Device, ReadKind, Volume};
use ferrix_sync::SpinParker;
use ferrix_vfs::tmpfs::HeapStorage;
use ferrix_vfs::tmpfs::Storage;
use ferrix_vfs::{Clock, Errno, FileSystem, FileType, Inode, Metadata, NewNode, Timespec};

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ferrix_btrfs::BtrfsError;

use super::{BLOCK, IMAGE_SIZE};
use crate::rw::RwBtrfs;

/// An empty volume with `mkfs.btrfs`'s defaults, which the write path starts
/// from.
const BLANK: &[u8] = include_bytes!("../../../btrfs/testdata/blank.img.packed");
/// A volume whose default subvolume is not the top-level tree: not writable.
const SUBVOL: &[u8] = include_bytes!("../../../btrfs/testdata/default-subvol.img.packed");

/// A packed image in memory that can be written to, shared by every clone,
/// as one disk is.
#[derive(Clone, Debug)]
struct Disk(Arc<Mutex<BTreeMap<u64, [u8; BLOCK]>>>);

impl Disk {
    fn new(packed: &[u8]) -> Disk {
        let blocks = packed
            .chunks_exact(8 + BLOCK)
            .map(|record| {
                let offset = u64::from_le_bytes(record[..8].try_into().unwrap());
                (offset, record[8..].try_into().unwrap())
            })
            .collect();
        Disk(Arc::new(Mutex::new(blocks)))
    }
}

impl Device for Disk {
    fn read_at(
        &mut self,
        physical: u64,
        buf: &mut [u8],
        _kind: ReadKind,
    ) -> Result<(), BtrfsError> {
        if physical
            .checked_add(buf.len() as u64)
            .is_none_or(|end| end > IMAGE_SIZE)
        {
            return Err(BtrfsError::DeviceRead { physical });
        }
        let blocks = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut done = 0;
        while done < buf.len() {
            let at = physical + done as u64;
            let base = at - at % BLOCK as u64;
            let within = (at - base) as usize;
            let take = (BLOCK - within).min(buf.len() - done);
            match blocks.get(&base) {
                Some(block) => {
                    buf[done..done + take].copy_from_slice(&block[within..within + take]);
                }
                None => buf[done..done + take].fill(0),
            }
            done += take;
        }
        Ok(())
    }
}

impl ferrix_btrfs_write::WriteDevice for Disk {
    fn write_at(&mut self, physical: u64, data: &[u8]) -> ferrix_btrfs_write::Result<()> {
        if physical
            .checked_add(data.len() as u64)
            .is_none_or(|end| end > IMAGE_SIZE)
        {
            return Err(ferrix_btrfs_write::Error::DeviceWrite { physical });
        }
        let mut blocks = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut done = 0;
        while done < data.len() {
            let at = physical + done as u64;
            let base = at - at % BLOCK as u64;
            let within = (at - base) as usize;
            let take = (BLOCK - within).min(data.len() - done);
            let block = blocks.entry(base).or_insert([0; BLOCK]);
            block[within..within + take].copy_from_slice(&data[done..done + take]);
            done += take;
        }
        Ok(())
    }

    fn flush(&mut self) -> ferrix_btrfs_write::Result<()> {
        Ok(())
    }
}

/// A clock that stands still, so a test's timestamps are its own.
#[derive(Debug)]
struct Fixed;

impl Clock for Fixed {
    fn now(&self) -> Timespec {
        Timespec {
            tv_sec: 1_790_000_000,
            tv_nsec: 0,
        }
    }
}

/// What mounts have said through their `notice`.
static NOTICES: Mutex<Vec<std::string::String>> = Mutex::new(Vec::new());

fn notice(what: core::fmt::Arguments<'_>) {
    NOTICES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(std::format!("{what}"));
}

fn storage() -> Arc<dyn Storage> {
    Arc::new(HeapStorage::new(1 << 30))
}

fn mount(disk: &Disk) -> Arc<RwBtrfs<Disk>> {
    RwBtrfs::mount(
        disk.clone(),
        0x0800_0010,
        storage(),
        Arc::new(Fixed),
        &SpinParker,
        notice,
    )
    .expect("the blank volume mounts for writing")
}

/// Make a file with `data` in it under `dir`.
fn make_file(dir: &Arc<dyn Inode>, name: &[u8], data: &[u8]) -> Arc<dyn Inode> {
    let file = dir.create(name, NewNode::Regular, 0o644).unwrap();
    if !data.is_empty() {
        assert_eq!(file.write_at(0, data, false).unwrap().0, data.len());
    }
    file
}

/// Read a whole file through the trait.
fn read_all(file: &Arc<dyn Inode>) -> Vec<u8> {
    let size = usize::try_from(file.metadata().size).unwrap();
    let mut out = vec![0u8; size];
    let read = file.read_at(0, &mut out).unwrap();
    out.truncate(read);
    out
}

/// What the stage 11 reader makes of the volume: it knows nothing of the
/// write path, so it is the check that the bytes are really btrfs.
fn read_back_with_the_reader(disk: &Disk, path: &[&[u8]]) -> Option<Metadata> {
    let mut device = disk.clone();
    let mut node = vec![0u8; 65536];
    let volume = Volume::open(&mut device, vec![ChunkMapEntry::EMPTY; 256], &mut node).unwrap();
    let subvolume = volume.default_subvolume();
    let mut ino = subvolume.root_dir();
    for part in path {
        let entry = subvolume
            .lookup(&mut device, ino, part, &mut node)
            .unwrap()?;
        match entry.target {
            ferrix_btrfs::fs::Target::Inode(next) => ino = next,
            ferrix_btrfs::fs::Target::Subvolume(_) => return None,
        }
    }
    let item = subvolume.inode(&mut device, ino, &mut node).unwrap()?;
    crate::metadata(ino, &item, volume.sectorsize()).ok()
}

#[test]
fn a_tree_made_through_the_trait_survives_a_remount() {
    let disk = Disk::new(BLANK);
    let data = vec![7u8; 100_000];
    {
        let fs = mount(&disk);
        let root = fs.root();
        let dir = root.create(b"dir", NewNode::Directory, 0o755).unwrap();
        let file = make_file(&dir, b"file", &data);
        let small = make_file(&root, b"small", b"inline bytes");
        let link = root
            .create(b"link", NewNode::Symlink(b"dir/file"), 0o777)
            .unwrap();
        assert_eq!(link.read_link().unwrap(), b"dir/file");
        assert_eq!(read_all(&file), data);
        assert_eq!(read_all(&small), b"inline bytes");
        file.fsync(false).unwrap();
        fs.sync().unwrap();
    }
    // A fresh mount, from the bytes alone.
    let fs = mount(&disk);
    let dir = fs.root().lookup(b"dir").unwrap();
    let file = dir.lookup(b"file").unwrap();
    assert_eq!(file.metadata().size, data.len() as u64);
    assert_eq!(read_all(&file), data);
    assert_eq!(
        read_all(&fs.root().lookup(b"small").unwrap()),
        b"inline bytes"
    );
    assert_eq!(
        fs.root().lookup(b"link").unwrap().read_link().unwrap(),
        b"dir/file"
    );
    // And the reader agrees about the file.
    let meta = read_back_with_the_reader(&disk, &[b"dir", b"file"]).expect("the reader finds it");
    assert_eq!(meta.size, data.len() as u64);
    assert_eq!(meta.kind, FileType::Regular);
}

#[test]
fn writes_land_where_they_are_asked_to() {
    let disk = Disk::new(BLANK);
    let fs = mount(&disk);
    let root = fs.root();
    let file = make_file(&root, b"file", &vec![1u8; 20_000]);
    // Overwrite the middle, append past the end, and write into a hole.
    let _ = file.write_at(4096, &[2u8; 100], false).unwrap();
    let _ = file.write_at(0, &[3u8; 7], true).unwrap();
    let _ = file.write_at(100_000, &[4u8; 10], false).unwrap();
    let mut expected = vec![1u8; 20_000];
    expected[4096..4196].fill(2);
    expected.extend_from_slice(&[3u8; 7]);
    expected.resize(100_000, 0);
    expected.extend_from_slice(&[4u8; 10]);
    assert_eq!(read_all(&file), expected);
    fs.sync().unwrap();
    let fs = mount(&disk);
    let file = fs.root().lookup(b"file").unwrap();
    assert_eq!(read_all(&file), expected, "after a commit and a remount");
}

/// A store whose "mapping" is the store itself, written as a program writes
/// a shared mapping: directly, with no word to the mount. It says what was
/// written as the kernel's object does, every page it holds.
#[derive(Debug)]
struct MappedStorage(HeapStorage);

#[derive(Debug)]
struct Mapped {
    pages: Box<dyn ferrix_vfs::tmpfs::Pages>,
    /// Pages written through the "mapping", and whether one was made.
    written: Mutex<(BTreeMap<u64, ()>, bool)>,
}

impl Mapped {
    /// Write through the mapping.
    fn write(&self, offset: u64, data: &[u8]) {
        self.pages.write(offset, data).unwrap();
        let mut written = self.written.lock().unwrap();
        let first = offset / 4096;
        let last = (offset + data.len() as u64 - 1) / 4096;
        for page in first..=last {
            let _ = written.0.insert(page, ());
        }
        written.1 = true;
    }
}

/// The store the mount holds: the object is shared with the "mapping".
#[derive(Debug)]
struct MappedPages(Arc<Mapped>);

impl ferrix_vfs::tmpfs::Pages for MappedPages {
    fn read(&self, offset: u64, buf: &mut [u8]) -> ferrix_vfs::Result<()> {
        self.0.pages.read(offset, buf)
    }
    fn write(&self, offset: u64, data: &[u8]) -> ferrix_vfs::Result<()> {
        self.0.pages.write(offset, data)
    }
    fn discard_from(&self, offset: u64) {
        self.0.pages.discard_from(offset);
    }
    fn committed_bytes(&self) -> u64 {
        self.0.pages.committed_bytes()
    }
    fn resize(&self, len: u64) {
        self.0.pages.resize(len);
    }
    fn object(&self) -> Option<Arc<dyn core::any::Any + Send + Sync>> {
        Some(Arc::clone(&self.0) as Arc<dyn core::any::Any + Send + Sync>)
    }
    fn mapped_writes(&self) -> (Vec<u64>, bool) {
        let mut written = self.0.written.lock().unwrap();
        let pages = if core::mem::take(&mut written.1) {
            written.0.keys().copied().collect()
        } else {
            Vec::new()
        };
        (pages, Arc::strong_count(&self.0) > 1)
    }
}

impl Storage for MappedStorage {
    fn allocate(&self) -> ferrix_vfs::Result<Box<dyn ferrix_vfs::tmpfs::Pages>> {
        self.wrap(self.0.allocate()?)
    }
    fn allocate_with(
        &self,
        source: Arc<dyn ferrix_vfs::tmpfs::PageSource>,
    ) -> ferrix_vfs::Result<Box<dyn ferrix_vfs::tmpfs::Pages>> {
        self.wrap(self.0.allocate_with(source)?)
    }
    fn max_file_size(&self) -> u64 {
        self.0.max_file_size()
    }
}

impl MappedStorage {
    #[expect(clippy::unnecessary_wraps, reason = "the shape `Storage` wants")]
    fn wrap(
        &self,
        pages: Box<dyn ferrix_vfs::tmpfs::Pages>,
    ) -> ferrix_vfs::Result<Box<dyn ferrix_vfs::tmpfs::Pages>> {
        Ok(Box::new(MappedPages(Arc::new(Mapped {
            pages,
            written: Mutex::new((BTreeMap::new(), false)),
        }))))
    }
}

#[test]
fn what_a_shared_mapping_writes_is_kept_and_written_back() {
    // A linker writes its output through a shared mapping: the file is made,
    // cut to its length, mapped, written and let go, and nothing tells the
    // mount which pages changed. They must outlive the file's last reference
    // and reach the disk at the next commit.
    let disk = Disk::new(BLANK);
    let fs = RwBtrfs::mount(
        disk.clone(),
        0x0800_0010,
        Arc::new(MappedStorage(HeapStorage::new(1 << 30))),
        Arc::new(Fixed),
        &SpinParker,
        notice,
    )
    .unwrap();
    let file = make_file(&fs.root(), b"linked", b"");
    file.set_len(3 * 4096).unwrap();
    let object = file.mapping().expect("the store is mapped");
    let mapped = object.downcast::<Mapped>().unwrap();
    // A commit between handing the object out and the first write: the
    // kernel counts a mapping in only after `mapping` answers.
    fs.sync().unwrap();
    mapped.write(0, b"\x7fELF");
    mapped.write(2 * 4096 + 10, b"tail");
    drop(mapped);
    drop(file);
    let mut expected = vec![0u8; 3 * 4096];
    expected[..4].copy_from_slice(b"\x7fELF");
    expected[2 * 4096 + 10..2 * 4096 + 14].copy_from_slice(b"tail");
    let file = fs.root().lookup(b"linked").unwrap();
    assert_eq!(read_all(&file), expected, "while the file is closed");
    drop(file);
    fs.sync().unwrap();
    let fs = mount(&disk);
    assert_eq!(
        read_all(&fs.root().lookup(b"linked").unwrap()),
        expected,
        "after a commit and a remount"
    );
}

#[test]
fn a_write_answers_the_offset_just_past_it() {
    // What `write` moves the file's position to, as the trait says. The
    // kernel copies a write in 4 KiB pieces, so an answer of the start put
    // every piece over the first, and every file a program wrote on the
    // volume with `write` was one page long.
    let disk = Disk::new(BLANK);
    let fs = mount(&disk);
    let file = make_file(&fs.root(), b"file", b"");
    assert_eq!(file.write_at(0, &[1u8; 4096], false).unwrap(), (4096, 4096));
    assert_eq!(
        file.write_at(4096, &[2u8; 100], false).unwrap(),
        (100, 4196)
    );
    assert_eq!(file.write_at(0, &[3u8; 10], true).unwrap(), (10, 4206));
    assert_eq!(file.write_at(8, &[4u8; 2], false).unwrap(), (2, 10));
    assert_eq!(file.metadata().size, 4206);
}

#[test]
fn what_is_written_outlives_the_last_reference_to_its_file() {
    // A build closes each object file and opens it again to archive it, and
    // nothing need hold the inode in between: the VFS keeps a bounded number
    // of names. The writes must wait for their writeback rather than go with
    // the inode object, and a commit between two writes must not leave the
    // file as long as it was then. Found by reading, while looking for why
    // `cargo xtask test-selfhost`'s rustc read its object files back short.
    let disk = Disk::new(BLANK);
    let fs = mount(&disk);
    let head = vec![1u8; 10_000];
    let tail = vec![2u8; 30_000];
    let file = make_file(&fs.root(), b"object", &head);
    fs.sync().unwrap();
    let _ = file.write_at(head.len() as u64, &tail, false).unwrap();
    let _ = file.write_at(0, &[3u8; 64], false).unwrap();
    drop(file);
    let mut expected = head;
    expected.extend_from_slice(&tail);
    expected[..64].fill(3);
    let file = fs.root().lookup(b"object").unwrap();
    assert_eq!(file.metadata().size, expected.len() as u64);
    assert_eq!(read_all(&file), expected);
    // One never committed at all, made and let go.
    drop(make_file(&fs.root(), b"fresh", b"not yet on the disk"));
    assert_eq!(
        read_all(&fs.root().lookup(b"fresh").unwrap()),
        b"not yet on the disk"
    );
    // And the writeback they were kept for reaches the disk.
    drop(file);
    fs.sync().unwrap();
    let fs = mount(&disk);
    assert_eq!(read_all(&fs.root().lookup(b"object").unwrap()), expected);
    assert_eq!(
        read_all(&fs.root().lookup(b"fresh").unwrap()),
        b"not yet on the disk"
    );
}

#[test]
fn truncation_cuts_and_extends() {
    let disk = Disk::new(BLANK);
    let fs = mount(&disk);
    let file = make_file(&fs.root(), b"file", &vec![9u8; 30_000]);
    file.set_len(5000).unwrap();
    assert_eq!(read_all(&file), vec![9u8; 5000]);
    file.set_len(9000).unwrap();
    let mut expected = vec![9u8; 5000];
    expected.resize(9000, 0);
    assert_eq!(read_all(&file), expected, "the gap reads as zeros");
    fs.sync().unwrap();
    let fs = mount(&disk);
    assert_eq!(read_all(&fs.root().lookup(b"file").unwrap()), expected);
}

#[test]
fn a_listing_names_every_entry_once() {
    let disk = Disk::new(BLANK);
    let fs = mount(&disk);
    let root = fs.root();
    let dir = root.create(b"dir", NewNode::Directory, 0o755).unwrap();
    for index in 0..50 {
        let name = std::format!("f{index:02}");
        let _ = make_file(&dir, name.as_bytes(), b"x");
    }
    let _ = dir.create(b"sub", NewNode::Directory, 0o755).unwrap();
    let mut names = Vec::new();
    let mut cursor = 0;
    // In pieces, resuming from the cursor, the way `getdents64` asks.
    loop {
        let mut piece = Vec::new();
        dir.read_dir(cursor, &mut |entry| {
            piece.push((entry.name.to_vec(), entry.kind, entry.next));
            piece.len() < 7
        })
        .unwrap();
        let Some(&(_, _, next)) = piece.last() else {
            break;
        };
        cursor = next;
        names.extend(piece.into_iter().map(|(name, kind, _)| (name, kind)));
    }
    assert_eq!(names.len(), 51);
    assert!(names.contains(&(b"f00".to_vec(), FileType::Regular)));
    assert!(names.contains(&(b"sub".to_vec(), FileType::Directory)));
    let mut sorted: Vec<Vec<u8>> = names.iter().map(|(name, _)| name.clone()).collect();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), 51, "no entry is listed twice");
}

#[test]
fn renaming_and_unlinking_move_names_about() {
    let disk = Disk::new(BLANK);
    let fs = mount(&disk);
    let root = fs.root();
    let dir = root.create(b"dir", NewNode::Directory, 0o755).unwrap();
    let file = make_file(&root, b"file", b"contents");
    root.link(b"hard", &file).unwrap();
    assert_eq!(file.metadata().nlink, 2);
    root.rename(b"file", &dir, b"moved", false).unwrap();
    assert_eq!(root.lookup(b"file").unwrap_err(), Errno::ENOENT);
    assert_eq!(read_all(&dir.lookup(b"moved").unwrap()), b"contents");
    root.unlink(b"hard").unwrap();
    assert_eq!(dir.lookup(b"moved").unwrap().metadata().nlink, 1);
    // A directory with something in it will not go, and an empty one will.
    assert_eq!(root.rmdir(b"dir").unwrap_err(), Errno::ENOTEMPTY);
    dir.unlink(b"moved").unwrap();
    root.rmdir(b"dir").unwrap();
    assert_eq!(root.lookup(b"dir").unwrap_err(), Errno::ENOENT);
    fs.sync().unwrap();
    let fs = mount(&disk);
    assert_eq!(fs.root().lookup(b"dir").unwrap_err(), Errno::ENOENT);
}

#[test]
fn a_file_unlinked_while_open_stays_readable() {
    let disk = Disk::new(BLANK);
    let fs = mount(&disk);
    let root = fs.root();
    let file = make_file(&root, b"doomed", b"still here");
    root.unlink(b"doomed").unwrap();
    assert_eq!(root.lookup(b"doomed").unwrap_err(), Errno::ENOENT);
    assert_eq!(file.metadata().nlink, 0);
    assert_eq!(read_all(&file), b"still here", "the open file still reads");
    let ino = file.metadata().ino;
    fs.sync().unwrap();
    drop(file);
    // The next operation drains what the drop left, and the inode goes.
    let _ = root.lookup(b"anything");
    fs.sync().unwrap();
    let fs = mount(&disk);
    assert_eq!(fs.root().lookup(b"doomed").unwrap_err(), Errno::ENOENT);
    assert!(ino >= 256);
}

#[test]
fn a_volume_with_a_subvolume_is_not_writable() {
    let disk = Disk::new(SUBVOL);
    let failed = RwBtrfs::mount(
        disk,
        0x0800_0010,
        storage(),
        Arc::new(Fixed),
        &SpinParker,
        notice,
    );
    assert_eq!(
        failed.err(),
        Some(Errno::EROFS),
        "subvolumes are refused, not damaged"
    );
}

#[test]
fn statfs_says_it_is_btrfs() {
    let disk = Disk::new(BLANK);
    let fs = mount(&disk);
    let stat = fs.statfs();
    assert_eq!(stat.magic, crate::BTRFS_SUPER_MAGIC);
    assert_eq!(stat.block_size, 4096);
    assert!(stat.blocks_free > 0 && stat.blocks_free <= stat.blocks);
}

#[test]
fn a_full_volume_answers_enospc_and_stays_usable() {
    // A `dd` into a nearly full /data latched the whole mount into EIO: the
    // writes went into the page cache, and the commit that made them extents
    // ran out half-way and aborted. A write must hear ENOSPC itself, and the
    // volume must go on: what was there reads back, and deleting makes room.
    let disk = Disk::new(BLANK);
    let fs = mount(&disk);
    let root = fs.root();
    let keep = make_file(&root, b"keep", &[5u8; 10_000]);
    fs.sync().unwrap();
    let room = fs.statfs().blocks_available * 4096;
    assert_eq!(
        room,
        volume_room(&disk),
        "statfs is the volume's room exactly"
    );
    let big = root.create(b"big", NewNode::Regular, 0o644).unwrap();
    let piece = vec![9u8; 100_000];
    let mut offset = 0u64;
    let error = loop {
        match big.write_at(offset, &piece, false) {
            Ok((n, _)) => offset += n as u64,
            Err(error) => break error,
        }
        assert!(
            offset <= room,
            "wrote {offset} bytes, past the {room} statfs offered"
        );
    };
    assert_eq!(error, Errno::ENOSPC);
    assert_eq!(
        offset, room,
        "a write stopped short of the room statfs offered"
    );
    assert_eq!(fs.statfs().blocks_available, 0);
    // Everything `write` took is kept.
    fs.sync().unwrap();
    assert_eq!(big.metadata().size, offset);
    assert!(read_all(&big).iter().all(|&byte| byte == 9));
    assert_eq!(read_all(&keep), vec![5u8; 10_000]);
    assert_eq!(volume_room(&disk), 0);
    // Deleted, the room comes back at the commit: a write straight after
    // the unlink commits for it rather than answer ENOSPC.
    root.unlink(b"big").unwrap();
    drop(big);
    drop(make_file(&root, b"again", &[3u8; 1 << 20]));
    fs.sync().unwrap();
    assert_eq!(
        fs.statfs().blocks_available * 4096,
        volume_room(&disk),
        "statfs is the volume's room exactly"
    );
    assert_eq!(volume_room(&disk), room - (1 << 20));
    fs.sync().unwrap();
    drop((keep, root, fs));
    let fs = mount(&disk);
    let again = fs.root().lookup(b"again").unwrap();
    assert_eq!(read_all(&again), vec![3u8; 1 << 20]);
    assert!(fs.root().lookup(b"big").is_err());
}

/// What the volume on `disk` can take, as a fresh open of its last commit
/// measures it.
fn volume_room(disk: &Disk) -> u64 {
    ferrix_btrfs_write::WriteVolume::open(disk.clone())
        .unwrap()
        .data_room()
}

/// A disk whose writes, and then reads, can be made to fail.
#[derive(Clone, Debug)]
struct Failing {
    disk: Disk,
    fail: Arc<std::sync::atomic::AtomicBool>,
    fail_reads: Arc<std::sync::atomic::AtomicBool>,
}

impl Device for Failing {
    fn read_at(&mut self, physical: u64, buf: &mut [u8], kind: ReadKind) -> Result<(), BtrfsError> {
        if self.fail_reads.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(BtrfsError::DeviceRead { physical });
        }
        self.disk.read_at(physical, buf, kind)
    }
}

impl ferrix_btrfs_write::WriteDevice for Failing {
    fn write_at(&mut self, physical: u64, data: &[u8]) -> ferrix_btrfs_write::Result<()> {
        if self.fail.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(ferrix_btrfs_write::Error::DeviceWrite { physical });
        }
        self.disk.write_at(physical, data)
    }

    fn flush(&mut self) -> ferrix_btrfs_write::Result<()> {
        Ok(())
    }
}

#[test]
fn an_aborted_transaction_leaves_the_mount_read_only_at_its_last_commit() {
    let disk = Disk::new(BLANK);
    let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let device = Failing {
        disk: disk.clone(),
        fail: Arc::clone(&fail),
        fail_reads: Arc::default(),
    };
    let fs = RwBtrfs::mount(
        device,
        0x0800_0011,
        storage(),
        Arc::new(Fixed),
        &SpinParker,
        notice,
    )
    .unwrap();
    let root = fs.root();
    let kept = make_file(&root, b"kept", &[1u8; 50_000]);
    fs.sync().unwrap();
    let lost = make_file(&root, b"lost", &[2u8; 50_000]);
    fail.store(true, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(fs.sync(), Err(Errno::EIO), "the commit's write fails");
    fail.store(false, std::sync::atomic::Ordering::Relaxed);
    // From here: reads, at the last commit; no changes.
    assert_eq!(fs.root().lookup(b"kept").map(|_| ()), Ok(()));
    assert_eq!(read_all(&kept), vec![1u8; 50_000]);
    assert!(
        fs.root().lookup(b"lost").is_err(),
        "the aborted transaction is gone"
    );
    assert_eq!(lost.write_at(0, b"more", false).err(), Some(Errno::EROFS));
    // A truncation is refused before it cuts what the cache holds.
    assert_eq!(kept.set_len(0).err(), Some(Errno::EROFS));
    assert_eq!(read_all(&kept), vec![1u8; 50_000]);
    assert_eq!(
        root.create(b"new", NewNode::Regular, 0o644).err(),
        Some(Errno::EROFS)
    );
    assert_eq!(fs.statfs().blocks_available, 0);
    assert_eq!(fs.sync(), Ok(()));
    let said = NOTICES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert!(
        said.iter()
            .any(|line| line.contains("device write") && line.contains("read-only")),
        "the mount said why: {said:?}"
    );
    // And the disk holds that last commit.
    drop((kept, lost, root, fs));
    let fs = mount(&disk);
    assert!(fs.root().lookup(b"kept").is_ok());
    assert!(fs.root().lookup(b"lost").is_err());
}

#[test]
fn an_aborted_volume_whose_last_commit_cannot_be_read_says_so_once() {
    let disk = Disk::new(BLANK);
    let (fail, fail_reads) = (Arc::default(), Arc::default());
    let device = Failing {
        disk,
        fail: Arc::clone(&fail),
        fail_reads: Arc::clone(&fail_reads),
    };
    let fs = RwBtrfs::mount(
        device,
        0x0800_0012,
        storage(),
        Arc::new(Fixed),
        &SpinParker,
        notice,
    )
    .unwrap();
    let root = fs.root();
    drop(make_file(&root, b"lost", &[2u8; 50_000]));
    let said = |what: &str| {
        NOTICES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|line| line.contains(what))
            .count()
    };
    fail.store(true, std::sync::atomic::Ordering::Relaxed);
    fail_reads.store(true, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(fs.sync(), Err(Errno::EIO));
    for _ in 0..3 {
        assert_eq!(root.lookup(b"lost").err(), Some(Errno::EIO));
    }
    assert_eq!(
        said("cannot be read again"),
        1,
        "said once, not at every use"
    );
}

#[test]
fn creates_on_full_trees_answer_enospc_and_the_mount_stays_writable() {
    // 22,738 creates on this fixture used to end in an aborted transaction
    // and a read-only mount, losing everything since the last commit.
    let disk = Disk::new(BLANK);
    let fs = mount(&disk);
    let root = fs.root();
    let name = |n: u32| std::format!("f{n:0>200}");
    let mut made = 0u32;
    let error = loop {
        match root.create(name(made).as_bytes(), NewNode::Regular, 0o644) {
            Ok(_) => made += 1,
            Err(error) => break error,
        }
        assert!(made < 200_000, "the trees never filled");
    };
    assert_eq!(error, Errno::ENOSPC, "a create on full trees");
    assert_eq!(
        root.create(b"one-more", NewNode::Regular, 0o644).err(),
        Some(Errno::ENOSPC),
        "the mount stays writable"
    );
    fs.sync().expect("the commit's room was kept back");
    for n in 0..64 {
        root.unlink(name(n).as_bytes())
            .expect("a deletion on full trees");
    }
    fs.sync().unwrap();
    drop(
        root.create(b"again", NewNode::Regular, 0o644)
            .expect("room a deletion made"),
    );
    fs.sync().unwrap();
    drop((root, fs));
    let fs = mount(&disk);
    assert!(fs.root().lookup(b"again").is_ok());
    assert!(fs.root().lookup(name(made - 1).as_bytes()).is_ok());
    assert!(fs.root().lookup(name(0).as_bytes()).is_err());
}

#[test]
fn a_writeback_the_volume_has_no_room_for_keeps_its_pages() {
    // Pages a shared mapping wrote took no room at the write, so the
    // writeback is where the volume turns out full: the file that does not
    // fit keeps its pages dirty, the rest is committed, the mount stays
    // writable, and once room is made the pages reach the disk.
    let disk = Disk::new(BLANK);
    let fs = RwBtrfs::mount(
        disk.clone(),
        0x0800_0013,
        Arc::new(MappedStorage(HeapStorage::new(1 << 30))),
        Arc::new(Fixed),
        &SpinParker,
        notice,
    )
    .unwrap();
    let root = fs.root();
    let room = fs.statfs().blocks_available * 4096;
    let filler = make_file(
        &root,
        b"filler",
        &vec![1u8; usize::try_from(room - (1 << 20)).unwrap()],
    );
    let mapped_len = 4u64 << 20;
    let file = make_file(&root, b"mapped", b"");
    file.set_len(mapped_len).unwrap();
    let mapped = file.mapping().unwrap().downcast::<Mapped>().unwrap();
    for page in 0..mapped_len / 4096 {
        mapped.write(page * 4096, &[7u8; 4096]);
    }
    drop(mapped);
    assert_eq!(
        fs.sync(),
        Err(Errno::ENOSPC),
        "the mapped file does not fit"
    );
    assert_eq!(volume_room(&disk), 0, "what fit was committed");
    drop(
        root.create(b"writable", NewNode::Regular, 0o644)
            .expect("the mount stays writable"),
    );
    drop(filler);
    root.unlink(b"filler").unwrap();
    fs.sync().expect("room for the kept pages");
    drop((file, root, fs));
    let fs = mount(&disk);
    let file = fs.root().lookup(b"mapped").unwrap();
    assert_eq!(
        read_all(&file),
        vec![7u8; usize::try_from(mapped_len).unwrap()]
    );
}

/// A disk that runs out of memory on every read and write while `starved`
/// is set, as one reading and writing through buffers it allocates does.
#[derive(Clone, Debug)]
struct Starved {
    disk: Disk,
    starved: Arc<std::sync::atomic::AtomicBool>,
}

impl Starved {
    fn starve(&self, on: bool) {
        self.starved.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    fn is_starved(&self) -> bool {
        self.starved.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Device for Starved {
    fn read_at(&mut self, physical: u64, buf: &mut [u8], kind: ReadKind) -> Result<(), BtrfsError> {
        if self.is_starved() {
            return Err(BtrfsError::OutOfMemory);
        }
        self.disk.read_at(physical, buf, kind)
    }
}

impl ferrix_btrfs_write::WriteDevice for Starved {
    fn write_at(&mut self, physical: u64, data: &[u8]) -> ferrix_btrfs_write::Result<()> {
        if self.is_starved() {
            return Err(ferrix_btrfs_write::Error::OutOfMemory);
        }
        self.disk.write_at(physical, data)
    }

    fn flush(&mut self) -> ferrix_btrfs_write::Result<()> {
        Ok(())
    }
}

#[test]
fn memory_running_out_is_enomem_and_the_mount_keeps_its_last_commit() {
    let disk = Disk::new(BLANK);
    let device = Starved {
        disk: disk.clone(),
        starved: Arc::default(),
    };
    let fs = RwBtrfs::mount(
        device.clone(),
        0x0800_0013,
        storage(),
        Arc::new(Fixed),
        &SpinParker,
        notice,
    )
    .unwrap();
    let root = fs.root();
    let kept = make_file(&root, b"kept", &[1u8; 50_000]);
    fs.sync().unwrap();
    drop(make_file(&root, b"lost", &[2u8; 50_000]));
    // The commit cannot write for memory: ENOMEM, not EIO, and the
    // transaction is gone as after any failed commit.
    device.starve(true);
    assert_eq!(fs.sync(), Err(Errno::ENOMEM));
    device.starve(false);
    assert_eq!(read_all(&kept), vec![1u8; 50_000]);
    assert!(fs.root().lookup(b"lost").is_err());
    drop((kept, root, fs));
    let fs = mount(&disk);
    assert!(fs.root().lookup(b"kept").is_ok());
    assert!(fs.root().lookup(b"lost").is_err());
}
