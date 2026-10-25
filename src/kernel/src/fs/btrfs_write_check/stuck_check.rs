//! One file whose writeback fails holds back no other file's commit (F-96's
//! residual, `docs/backlog/p1.md` p1-035).
//!
//! A writable mount's sync -- the periodic commit's, `sync`'s, unmount's --
//! writes every live file's dirty pages back, then commits. Before
//! 2026-10-25 the first file whose writeback failed with anything but
//! `ENOSPC` ended the sync before the commit, so one file that failed every
//! time stopped every periodic commit for good, while an `fsync` of any
//! other still committed. On the N150 a file replaced seven minutes before
//! a power cut was empty after it.
//!
//! This mounts the stage 12 disk writable over a page store that can make
//! one file's pages refuse to be read (`ENOMEM`, as a copy refused memory
//! would), writes that file and then another, and syncs with the failure
//! on. The sync must answer `ENOMEM`, the first failure; and a read-only
//! mount of the same disk -- which reads only what a commit put there --
//! must find the other file whole. With the failure off, the next sync must
//! commit the first file as well.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicBool, Ordering};

use ferrix_btrfs_vfs::rw::RwBtrfs;
use ferrix_vfs::tmpfs::{PageSource, Pages, Storage};
use ferrix_vfs::{Errno, FileSystem, Inode, NewNode};

use super::{crc32c, pattern};
use crate::fs;
use crate::fs::pages::VmoStorage;

/// The directory the check writes in, on the volume's root.
const DIRECTORY: &[u8] = b"stuck-check";

/// The two files: the one whose writeback fails, made first so that a sync
/// meets it first, and the one that must be committed all the same.
const FAILING: &[u8] = b"failing";
const KEPT: &[u8] = b"kept";

/// Their lengths: several pages each, one of them past an extent's worth of
/// a single page.
const FAILING_LEN: usize = 48 * 1024;
const KEPT_LEN: usize = 96 * 1024;

/// A page store over the kernel's own whose next allocation makes pages
/// that refuse every read while `failing` is set.
#[derive(Debug)]
struct Poisoning {
    next: AtomicBool,
    failing: Arc<AtomicBool>,
}

/// One file's pages, poisoned or not.
#[derive(Debug)]
struct Poisoned {
    pages: Box<dyn Pages>,
    failing: Option<Arc<AtomicBool>>,
}

impl Pages for Poisoned {
    fn read(&self, offset: u64, buf: &mut [u8]) -> ferrix_vfs::Result<()> {
        if self
            .failing
            .as_ref()
            .is_some_and(|failing| failing.load(Ordering::SeqCst))
        {
            return Err(Errno::ENOMEM);
        }
        self.pages.read(offset, buf)
    }
    fn write(&self, offset: u64, data: &[u8]) -> ferrix_vfs::Result<()> {
        self.pages.write(offset, data)
    }
    fn discard_from(&self, offset: u64) {
        self.pages.discard_from(offset);
    }
    fn committed_bytes(&self) -> u64 {
        self.pages.committed_bytes()
    }
    fn resize(&self, len: u64) {
        self.pages.resize(len);
    }
    fn object(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        self.pages.object()
    }
    fn mapped_writes(&self) -> (Vec<u64>, bool) {
        self.pages.mapped_writes()
    }
}

impl Poisoning {
    fn wrap(&self, pages: Box<dyn Pages>) -> Box<dyn Pages> {
        let poisoned = self.next.swap(false, Ordering::SeqCst);
        Box::new(Poisoned {
            pages,
            failing: poisoned.then(|| Arc::clone(&self.failing)),
        })
    }
}

impl Storage for Poisoning {
    fn allocate(&self) -> ferrix_vfs::Result<Box<dyn Pages>> {
        Ok(self.wrap(VmoStorage.allocate()?))
    }
    fn allocate_with(&self, source: Arc<dyn PageSource>) -> ferrix_vfs::Result<Box<dyn Pages>> {
        Ok(self.wrap(VmoStorage.allocate_with(source)?))
    }
    fn max_file_size(&self) -> u64 {
        VmoStorage.max_file_size()
    }
    fn parker(&self) -> &dyn ferrix_sync::Parker {
        VmoStorage.parker()
    }
    fn capacity(&self) -> (u64, u64) {
        VmoStorage.capacity()
    }
}

/// Make `name` in `dir` holding `len` bytes of file `seed`.
fn write_new(
    dir: &Arc<dyn Inode>,
    name: &[u8],
    seed: u64,
    len: usize,
) -> Result<Arc<dyn Inode>, &'static str> {
    let file = dir
        .create(name, NewNode::Regular, 0o644)
        .map_err(|_| "stuck-check: a file could not be made")?;
    let data = pattern(seed, 0, len);
    match file.write_at(0, &data, false) {
        Ok((written, _)) if written == len => Ok(file),
        _ => Err("stuck-check: a file could not be written"),
    }
}

/// Whether the committed volume on `rdev` holds `name` with `len` bytes of
/// file `seed`, read through a read-only mount of its own.
fn committed(rdev: u64, name: &[u8], seed: u64, len: usize) -> Result<bool, &'static str> {
    let reader = fs::btrfs::mount(rdev).map_err(|_| "stuck-check: no read-only mount")?;
    let Ok(dir) = reader.root().lookup(DIRECTORY) else {
        return Ok(false);
    };
    let Ok(file) = dir.lookup(name) else {
        return Ok(false);
    };
    if file.metadata().size != len as u64 {
        return Ok(false);
    }
    let mut back = vec![0u8; len];
    let read = file
        .read_at(0, &mut back)
        .map_err(|_| "stuck-check: a committed file could not be read")?;
    Ok(read == len && crc32c(&back) == crc32c(&pattern(seed, 0, len)))
}

/// Run the check on the stage 12 disk, which nothing else has mounted.
///
/// # Errors
///
/// What did not hold, as a sentence.
pub(super) fn run(rdev: u64) -> Result<(), &'static str> {
    let device = fs::devfs::block_device(rdev).ok_or("stuck-check: the disk is gone")?;
    let disk = fs::btrfs::Disk::new(device).map_err(|_| "stuck-check: no disk handle")?;
    let storage = Arc::new(Poisoning {
        next: AtomicBool::new(false),
        failing: Arc::new(AtomicBool::new(false)),
    });
    let volume = RwBtrfs::mount(
        disk,
        rdev,
        Arc::clone(&storage) as Arc<dyn Storage>,
        fs::clock(),
        &crate::sync::SchedParker,
        |why| crate::console::println!("  btrfs    {why}"),
    )
    .map_err(|_| "stuck-check: the disk would not mount writable")?;
    let root = volume.root();
    let dir = root
        .create(DIRECTORY, NewNode::Directory, 0o755)
        .map_err(|_| "stuck-check: the directory could not be made")?;
    storage.next.store(true, Ordering::SeqCst);
    let failing = write_new(&dir, FAILING, 41, FAILING_LEN)?;
    storage.next.store(false, Ordering::SeqCst);
    let kept = write_new(&dir, KEPT, 43, KEPT_LEN)?;

    storage.failing.store(true, Ordering::SeqCst);
    let answered = volume.sync();
    storage.failing.store(false, Ordering::SeqCst);
    if answered != Err(Errno::ENOMEM) {
        return Err("stuck-check: a sync past a file whose writeback failed did not answer ENOMEM");
    }
    if !committed(rdev, KEPT, 43, KEPT_LEN)? {
        return Err(
            "stuck-check: a file whose writeback failed kept another file's writes out of the commit",
        );
    }
    volume
        .sync()
        .map_err(|_| "stuck-check: the sync after the failure ended failed")?;
    if !committed(rdev, FAILING, 41, FAILING_LEN)? {
        return Err(
            "stuck-check: the file whose writeback had failed was not committed at the next sync",
        );
    }
    // Leave the volume as the stage 12 check had it.
    dir.unlink(FAILING)
        .map_err(|_| "stuck-check: a file could not be removed")?;
    dir.unlink(KEPT)
        .map_err(|_| "stuck-check: a file could not be removed")?;
    drop((failing, kept));
    root.rmdir(DIRECTORY)
        .map_err(|_| "stuck-check: the directory could not be removed")?;
    volume
        .sync()
        .map_err(|_| "stuck-check: the last sync failed")?;
    Ok(())
}
