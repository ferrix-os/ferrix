//! Stage 8's self-checks for the root the kernel built.
//!
//! Two properties the host tests of `src/lib/fs/vfs` cannot establish, because both
//! are about this machine rather than the logic. That the archive the loader
//! handed over is the one the build wrote, unpacked intact through the direct
//! map — hard link and symbolic link included. And that tmpfs over VMO pages
//! stores what it is given, gives zeros where nothing was written, and gives
//! every frame back once the file is gone.
//!
//! And, in [`run_calls`], a third: that pipes, a FIFO and the calls about
//! filesystems answer as a program meets them. Those waits and wake-ups, the
//! user-memory byte layouts and the descriptors left behind are the kernel's,
//! not the library's, so they are driven through the system call handlers
//! against a process built for the check. Last, `mount -t proc` and `mount -t
//! devtmpfs` go in by syscall number, as an init script's do, and are read
//! through, listed in `/proc/mounts` and unmounted again.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_linux_abi::nr::Syscall;
use ferrix_linux_abi::types::{
    AT_FDCWD, AT_REMOVEDIR, F_GETFD, F_GETFL, FALLOC_FL_KEEP_SIZE, FALLOC_FL_PUNCH_HOLE,
    FD_CLOEXEC, FIOCLEX, FIONBIO, FIONCLEX, MAP_ANONYMOUS, MAP_PRIVATE, MS_NODEV, MS_NOEXEC,
    MS_NOSUID, MS_RELATIME, O_APPEND, O_CLOEXEC, O_CREAT, O_NONBLOCK, O_RDONLY, O_RDWR, O_TRUNC,
    O_WRONLY, PROT_READ, PROT_WRITE, SEEK_CUR, SPLICE_F_NONBLOCK,
};
use ferrix_vfs::pipe::{PIPE_CAPACITY, PIPEFS_MAGIC};
use ferrix_vfs::tmpfs::{PageSource, Pages, Storage, TMPFS_MAGIC};
use ferrix_vfs::{Errno, FileType, Namespace, NewNode, OpenFlags, RenameMode};
use ferrix_vma::VmaFlags;

use crate::fs::pages::VmoStorage;
use crate::fs::{self, Report as Built};
use crate::mm;
use crate::syscall::check as syscall_check;
use crate::syscall::memory::{self, MmapRequest, OffsetUnit};
use crate::syscall::process::{self, Process};
use crate::syscall::{fd, file, fsctl, pipe, uaccess};
use crate::user::space::{Access, AddressSpace, FileMapping, FilePlace};
use crate::user::vmo::Vmo;

/// The marker `tools/common/xtask/src/initramfs.rs` writes, byte for byte.
const MARKER: &[u8] = b"unpacked by the kernel from a cpio archive the loader handed it\n";

/// Where the marker is unpacked.
const MARKER_PATH: &[u8] = b"/etc/ferrix/initramfs";

/// What the checks measured, for the boot log.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Report {
    /// Whether the archive's marker, link and symbolic link were checked.
    pub(crate) initramfs_verified: bool,
    /// Pages a file under `/tmp` committed while the check wrote it.
    pub(crate) pages: u64,
    /// Pages a store over a page source filled from it.
    pub(crate) filled: u64,
    /// Frames the tmpfs check cost once the file was gone. Zero, or the page
    /// store is leaking.
    pub(crate) leaked: i64,
}

/// Run them. `Err` names the first thing that was not true.
pub(crate) fn run(built: &Built) -> Result<Report, &'static str> {
    let initramfs_verified = match built.unpacked {
        Some(_) => {
            check_the_archive_unpacked_intact()?;
            true
        }
        None => false,
    };
    check_tmp_is_its_own_mount()?;
    check_tmpfs_io_sleeps()?;

    // Twice, measured on the second, for the reason `syscall::check` gives:
    // the heap keeps the last page of a size class it has used, and a single
    // run cannot tell that apart from a leak.
    let _warm = check_page_stores()?;
    crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)?;
    let window = mm::FrameWindow::open();
    let (pages, filled) = check_page_stores()?;
    crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)?;
    let leaked = window.kept();
    // Checked, not only printed: a count nothing tests would boot green
    // through the very leak it exists to show. The number and its sign are
    // printed first: a count that fell is frames the check kept, one that
    // rose is something outside it freeing inside the window.
    if leaked != 0 {
        crate::console::println!("  tmpfs    {leaked} frames across the second run");
        window.report("tmpfs");
        return Err(if leaked > 0 {
            "the tmpfs check kept frames it did not give back"
        } else {
            "the free frame count rose across the tmpfs check: something outside it freed frames in the window"
        });
    }

    Ok(Report {
        initramfs_verified,
        pages,
        filled,
        leaked,
    })
}

/// A tmpfs file's I/O lock waits on the scheduler (F-70): the kernel's store
/// lends the parker every kernel `SleepLock` uses, never the spinning
/// default, whose waiter would hold its processor while a write fills or a
/// truncation shoots down.
fn check_tmpfs_io_sleeps() -> Result<(), &'static str> {
    let parker = alloc::format!("{:?}", VmoStorage.parker());
    if parker != alloc::format!("{:?}", crate::sync::SchedParker) {
        return Err("tmpfs's store lends a parker that is not the scheduler's");
    }
    Ok(())
}

/// Read a whole file through the namespace.
fn read_all(ns: &Namespace, path: &[u8]) -> Result<Vec<u8>, &'static str> {
    let ctx = ns.context();
    let read = OpenFlags {
        read: true,
        ..OpenFlags::default()
    };
    let file = ns
        .open(&ctx, None, path, &read, 0)
        .map_err(|_| "a file the check needs would not open")?;
    let mut contents = Vec::new();
    let mut chunk = [0_u8; 97];
    loop {
        let count = file
            .read(&mut chunk)
            .map_err(|_| "a file the check opened would not read")?;
        if count == 0 {
            return Ok(contents);
        }
        contents.extend_from_slice(chunk.get(..count).ok_or("a read overran its buffer")?);
    }
}

/// The marker reads back exactly, and both of its other names reach it.
fn check_the_archive_unpacked_intact() -> Result<(), &'static str> {
    let ns = fs::namespace();
    let ctx = ns.context();
    if read_all(ns, MARKER_PATH)? != MARKER {
        return Err("the initramfs marker does not hold what the build wrote");
    }

    let marker = ns
        .resolve(&ctx, None, MARKER_PATH, false)
        .and_then(|at| ns.stat(&at))
        .map_err(|_| "the initramfs marker would not stat")?;
    let link = ns
        .resolve(&ctx, None, b"/etc/ferrix/initramfs.link", false)
        .and_then(|at| ns.stat(&at))
        .map_err(|_| "the initramfs hard link is missing")?;
    if link.metadata.ino != marker.metadata.ino || marker.metadata.nlink != 2 {
        return Err("the initramfs hard link is a copy, not a second name");
    }

    let symlink = b"/etc/ferrix/initramfs.symlink";
    let kind = ns
        .resolve(&ctx, None, symlink, false)
        .and_then(|at| ns.stat(&at))
        .map_err(|_| "the initramfs symbolic link is missing")?
        .metadata
        .kind;
    if kind != FileType::Symlink || read_all(ns, symlink)? != MARKER {
        return Err("the initramfs symbolic link does not lead to the marker");
    }

    // The whole-file read that loading a program goes through: through the
    // link, as `execve` of `/bin/sh` will be, and refusing what is not a file.
    let whole = fs::read_file(&ctx, None, symlink);
    if whole.as_deref() != Ok(MARKER) {
        return Err("reading a whole file through a symbolic link did not give the marker");
    }
    if fs::read_file(&ctx, None, b"/etc/ferrix") != Err(Errno::EISDIR) {
        return Err("reading a directory as a whole file was not refused");
    }
    // And the read that must stay beneath a directory -- the display core's
    // of `drm.edid_firmware`'s file under `/lib/firmware` -- reads a file
    // there, and follows no link, not even one that stays inside.
    if fs::read_file_beneath(&ctx, b"/etc", b"ferrix/initramfs").as_deref() != Ok(MARKER) {
        return Err("a whole-file read beneath a directory did not give the marker");
    }
    if fs::read_file_beneath(&ctx, b"/etc", b"ferrix/initramfs.symlink") != Err(Errno::ELOOP) {
        return Err("a whole-file read beneath a directory followed a symbolic link");
    }
    if fs::read_file_beneath(&ctx, b"/etc/ferrix", b"../ferrix/initramfs") != Err(Errno::EINVAL) {
        return Err("a whole-file read beneath a directory climbed out through `..`");
    }
    Ok(())
}

/// `/tmp` is a separate filesystem from the root, sticky and world-writable.
fn check_tmp_is_its_own_mount() -> Result<(), &'static str> {
    let ns = fs::namespace();
    let ctx = ns.context();
    let root = ns.stat(&ctx.root).map_err(|_| "the root would not stat")?;
    let tmp = ns
        .resolve(&ctx, None, b"/tmp", true)
        .and_then(|at| ns.stat(&at))
        .map_err(|_| "/tmp is missing")?;
    if tmp.dev == root.dev {
        return Err("/tmp is not a filesystem of its own");
    }
    if tmp.metadata.permissions != 0o1777 {
        return Err("/tmp is not sticky and world-writable");
    }
    Ok(())
}

/// A byte of the pattern the tmpfs check writes, by its offset.
fn pattern(at: usize) -> u8 {
    (at.wrapping_mul(31) ^ (at >> 12)) as u8
}

/// Write across pages, read back, truncate into a page, grow again, rename,
/// and remove. Returns the pages the file had committed.
///
/// Verifies: L.user.41
fn check_tmpfs_stores_pages() -> Result<u64, &'static str> {
    let ns = fs::namespace();
    let ctx = ns.context();
    let page = usize::try_from(PAGE_SIZE).map_err(|_| "the page size does not fit")?;
    let len = page * 3 + 123;
    let data: Vec<u8> = (0..len).map(pattern).collect();

    let create = OpenFlags {
        read: true,
        write: true,
        create: true,
        exclusive: true,
        ..OpenFlags::default()
    };
    let file = ns
        .open(&ctx, None, b"/tmp/stage8-check", &create, 0o600)
        .map_err(|_| "a file could not be created under /tmp")?;
    for piece in data.chunks(1000) {
        if file.write(piece) != Ok(piece.len()) {
            return Err("a write to a tmpfs file came back short");
        }
    }
    let stat = ns
        .stat(file.location())
        .map_err(|_| "the file would not stat")?;
    let pages = stat.metadata.blocks * 512 / PAGE_SIZE;
    if stat.metadata.size != len as u64 || pages != 4 {
        return Err("the file's size or committed pages are not what was written");
    }
    if read_all(ns, b"/tmp/stage8-check")? != data {
        return Err("a tmpfs file read back different bytes");
    }

    // Into the second page, then out past where the data used to end: the
    // old bytes must not reappear.
    let keep = page + 10;
    file.set_len(keep as u64)
        .and_then(|()| file.set_len(len as u64))
        .map_err(|_| "a tmpfs file would not truncate and grow")?;
    let mut back = vec![0_u8; len];
    if file.read_at(0, &mut back) != Ok(len) {
        return Err("a regrown tmpfs file read back short");
    }
    let head_intact = back.get(..keep) == data.get(..keep);
    let tail_zero = back
        .get(keep..)
        .is_some_and(|tail| tail.iter().all(|&b| b == 0));
    if !head_intact || !tail_zero {
        return Err("truncating a tmpfs file did not discard what it cut off");
    }

    ns.rename(
        &ctx,
        (None, b"/tmp/stage8-check"),
        (None, b"/tmp/stage8-check.moved"),
        RenameMode::NoReplace,
    )
    .map_err(|_| "a tmpfs file would not rename")?;
    if read_all(ns, b"/tmp/stage8-check.moved")?.len() != len {
        return Err("a renamed tmpfs file lost its contents");
    }
    ns.unlink(&ctx, None, b"/tmp/stage8-check.moved")
        .map_err(|_| "a tmpfs file would not unlink")?;
    drop(file);
    Ok(pages)
}

/// Both page stores the kernel hands a filesystem: tmpfs's, and one over a
/// page source. Returns the pages the tmpfs file committed and the pages the
/// source filled.
fn check_page_stores() -> Result<(u64, u64), &'static str> {
    let pages = check_tmpfs_stores_pages()?;
    let filled = check_a_store_fills_from_its_source()?;
    let faulted = check_a_mapping_faults_in_its_source()?;
    Ok((pages, filled + faulted))
}

/// The byte at `address` in `space`, faulted in for reading as a program's
/// load would be.
fn byte_through(space: &AddressSpace, address: u64) -> Result<u8, &'static str> {
    space
        .with_page(address, Access::READ, |at| {
            // SAFETY: `with_page` passes the direct-map address of `address`,
            // translated under the space's lock, which it holds while this
            // runs, so the frame stays mapped there for the read.
            unsafe { core::ptr::read(at as *const u8) }
        })
        .map_err(|_| "a fault through a mapping of a store over a page source failed")
}

/// A mapping of a store over a page source faults its pages in from the
/// source, as a read would, and never as zeros: the store's object is what
/// the mapping maps, and a page no read has reached is absent from it.
///
/// A shared mapping's read shows the source's byte and fills a run of 32
/// pages at once, as a read does. A private mapping's first write to a page
/// nothing has read copies the source's page, not zeros, and leaves the
/// store's page as the source has it. That is what a program run from btrfs
/// does to its libraries: glibc's linker reads their dynamic sections through
/// a private mapping, and before the fault filled pages it read zeros there.
/// Returns the pages the faults filled.
fn check_a_mapping_faults_in_its_source() -> Result<u64, &'static str> {
    const PAGES: u64 = 64;
    let source = Arc::new(CheckSource {
        calls: AtomicUsize::new(0),
        most: AtomicUsize::new(0),
        fail_at: AtomicU64::new(u64::MAX),
        lie: AtomicBool::new(false),
    });
    let store = VmoStorage
        .allocate_with(Arc::clone(&source) as Arc<dyn PageSource>)
        .map_err(|_| "a store over a page source was refused")?;
    store.resize(PAGES * PAGE_SIZE);
    let object = store
        .object()
        .and_then(|object| object.downcast::<Vmo>().ok())
        .ok_or("a store over a page source has no object to map")?;
    let space = AddressSpace::new().map_err(|_| "no address space for the mapping check")?;
    let mapping = || FileMapping {
        file: Arc::new(()) as Arc<dyn Any + Send + Sync>,
        may_write: false,
    };
    let len = PAGES * PAGE_SIZE;
    let shared = space
        .map_file(
            FilePlace::Anywhere(None),
            len,
            VmaFlags {
                shared: true,
                ..VmaFlags::READ
            },
            Arc::clone(&object),
            0,
            mapping(),
        )
        .map_err(|_| "could not map a store over a page source shared")?;
    let private = space
        .map_file(
            FilePlace::Anywhere(None),
            len,
            VmaFlags::READ_WRITE,
            Arc::clone(&object),
            0,
            mapping(),
        )
        .map_err(|_| "could not map a store over a page source privately")?;
    let outcome = fault_in_from_source(&space, shared, private, &object, &source);
    let _ = space.unmap(shared, len);
    let _ = space.unmap(private, len);
    let filled = object.committed() as u64;
    outcome.map(|()| filled)
}

/// The body of [`check_a_mapping_faults_in_its_source`], with its two
/// mappings of `object` at `shared` and `private`.
///
/// Verifies: L.user.37
fn fault_in_from_source(
    space: &AddressSpace,
    shared: u64,
    private: u64,
    object: &Vmo,
    source: &CheckSource,
) -> Result<(), &'static str> {
    let calls = || source.calls.load(Ordering::Relaxed);
    if byte_through(space, shared + 5 * PAGE_SIZE + 7)? != source_byte(5, 7) {
        return Err(
            "a shared mapping of a store over a page source read something other than the source's byte",
        );
    }
    if calls() != 1 || object.committed() != 32 {
        return Err("a fault through a mapping did not fill a run of 32 pages from the source");
    }

    // Page 50: no read and no fault has reached it. The private write copies
    // it, so it has to be there to copy.
    let at = private + 50 * PAGE_SIZE + 9;
    space
        .with_page(at, Access::WRITE, |byte| {
            // SAFETY: as in `byte_through`; the write fault made the page this
            // mapping's own copy, which the direct map may write.
            unsafe { core::ptr::write(byte as *mut u8, 0xA5) }
        })
        .map_err(|_| "a write fault through a private mapping of a store failed")?;
    if byte_through(space, at)? != 0xA5 || byte_through(space, at + 1)? != source_byte(50, 10) {
        return Err(
            "a private write to a page nothing had read copied zeros, not the source's page",
        );
    }
    let mut kept = [0_u8; 2];
    object
        .read_page(50, 9, &mut kept)
        .map_err(|_| "the store's own page was not there to read")?;
    if kept != [source_byte(50, 9), source_byte(50, 10)] {
        return Err("a private write reached the store's page");
    }
    Ok(())
}

/// A byte of what [`CheckSource`] fills page `index` with, at `at` within it.
fn source_byte(index: u64, at: usize) -> u8 {
    (index as u8).wrapping_mul(37) ^ (at as u8)
}

/// A page source over nothing, for the check: page `index` holds
/// [`source_byte`], and it can be told to answer short, fail or lie.
#[derive(Debug)]
struct CheckSource {
    /// How many times it has been asked.
    calls: AtomicUsize,
    /// The most pages it fills in one call; zero for as many as asked.
    most: AtomicUsize,
    /// A call starting at this page fails with `EIO`.
    fail_at: AtomicU64,
    /// Whether it claims one page more than it was asked for.
    lie: AtomicBool,
}

impl PageSource for CheckSource {
    fn fill_range(&self, first: u64, pages: &mut [&mut [u8]]) -> ferrix_vfs::Result<usize> {
        let _ = self.calls.fetch_add(1, Ordering::Relaxed);
        if first == self.fail_at.load(Ordering::Relaxed) {
            return Err(Errno::EIO);
        }
        let most = self.most.load(Ordering::Relaxed);
        let count = if most == 0 {
            pages.len()
        } else {
            pages.len().min(most)
        };
        for (index, page) in (first..).zip(pages.iter_mut()).take(count) {
            for (at, byte) in page.iter_mut().enumerate() {
                *byte = source_byte(index, at);
            }
        }
        if self.lie.load(Ordering::Relaxed) {
            return Ok(pages.len() + 1);
        }
        Ok(count)
    }
}

/// Whether `buf`, read from byte `offset` of a store over a [`CheckSource`],
/// holds the source's bytes.
fn holds_source_bytes(buf: &[u8], offset: u64) -> bool {
    buf.iter().zip(offset..).all(|(&byte, at)| {
        usize::try_from(at % PAGE_SIZE)
            .is_ok_and(|within| byte == source_byte(at / PAGE_SIZE, within))
    })
}

/// A store over a page source fills what a read reaches in runs of at most 32
/// pages and keeps them, fills a page a write covers only in part before the
/// write, and fills in pieces from a source that answers short. Then
/// [`check_a_store_distrusts_and_cuts_its_source`]. Returns the pages filled.
fn check_a_store_fills_from_its_source() -> Result<u64, &'static str> {
    let page = usize::try_from(PAGE_SIZE).map_err(|_| "the page size does not fit")?;
    let source = Arc::new(CheckSource {
        calls: AtomicUsize::new(0),
        most: AtomicUsize::new(0),
        fail_at: AtomicU64::new(u64::MAX),
        lie: AtomicBool::new(false),
    });
    let store = VmoStorage
        .allocate_with(Arc::clone(&source) as Arc<dyn PageSource>)
        .map_err(|_| "a store over a page source was refused")?;
    let calls = || source.calls.load(Ordering::Relaxed);

    // From the middle of page 0 to 100 bytes into page 40: 41 pages, which
    // is one fill of 32 and one of 9.
    let offset = PAGE_SIZE / 2;
    let mut buf = vec![0_u8; 40 * page + 100];
    store
        .read(offset, &mut buf)
        .map_err(|_| "a read of a store over a page source failed")?;
    if !holds_source_bytes(&buf, offset) {
        return Err("a store over a page source read back something other than the source's bytes");
    }
    if calls() != 2 || store.committed_bytes() != 41 * PAGE_SIZE {
        return Err("a read did not fill its pages from the source in runs of at most 32");
    }
    store
        .read(offset, &mut buf)
        .map_err(|_| "a second read of a store over a page source failed")?;
    if calls() != 2 {
        return Err("a store asked its source again for pages it already holds");
    }

    // A write into the middle of page 50 keeps the page's other bytes.
    let written = b"written over the source";
    store
        .write(50 * PAGE_SIZE + 10, written)
        .map_err(|_| "a write into a store over a page source failed")?;
    let mut one = vec![0_u8; page];
    store
        .read(50 * PAGE_SIZE, &mut one)
        .map_err(|_| "a read of a partly written page failed")?;
    let around = one
        .get(..10)
        .is_some_and(|head| holds_source_bytes(head, 50 * PAGE_SIZE))
        && one.get(10 + written.len()..).is_some_and(|tail| {
            holds_source_bytes(tail, 50 * PAGE_SIZE + 10 + written.len() as u64)
        });
    if calls() != 3 || one.get(10..10 + written.len()) != Some(&written[..]) || !around {
        return Err(
            "a write covering part of a page did not fill the rest of it from the source first",
        );
    }

    // A source that answers at most 3 pages a call: 10 pages in 4 calls.
    source.most.store(3, Ordering::Relaxed);
    let mut ten = vec![0_u8; 10 * page];
    store
        .read(60 * PAGE_SIZE, &mut ten)
        .map_err(|_| "a read from a source that answers short failed")?;
    source.most.store(0, Ordering::Relaxed);
    if calls() != 7 || !holds_source_bytes(&ten, 60 * PAGE_SIZE) {
        return Err("a store did not keep going after a source answered short");
    }

    let filled = store.committed_bytes() / PAGE_SIZE;
    check_a_store_distrusts_and_cuts_its_source(store.as_ref(), &source)?;
    Ok(filled)
}

/// A fill that fails or claims more than it was asked for keeps nothing, and a
/// cut keeps the part of its page before it, reads zeros past it, and never
/// asks the source for what it cut.
fn check_a_store_distrusts_and_cuts_its_source(
    store: &dyn Pages,
    source: &CheckSource,
) -> Result<(), &'static str> {
    let page = usize::try_from(PAGE_SIZE).map_err(|_| "the page size does not fit")?;
    let mut one = vec![0_u8; page];
    let held = store.committed_bytes();

    source.fail_at.store(80, Ordering::Relaxed);
    let failed = store.read(80 * PAGE_SIZE, &mut one);
    source.fail_at.store(u64::MAX, Ordering::Relaxed);
    source.lie.store(true, Ordering::Relaxed);
    let lied = store.read(90 * PAGE_SIZE, &mut one);
    source.lie.store(false, Ordering::Relaxed);
    if failed != Err(Errno::EIO) || lied != Err(Errno::EIO) || store.committed_bytes() != held {
        return Err("a fill that failed or claimed too much was not EIO, or kept pages");
    }

    // Cut 7 bytes into page 20: page 20 keeps those, page 25 was held and
    // goes, and neither asks the source again.
    store.discard_from(20 * PAGE_SIZE + 7);
    let calls = source.calls.load(Ordering::Relaxed);
    store
        .read(20 * PAGE_SIZE, &mut one)
        .map_err(|_| "a read of a cut page failed")?;
    let cut_kept = one
        .get(..7)
        .is_some_and(|head| holds_source_bytes(head, 20 * PAGE_SIZE))
        && one
            .get(7..)
            .is_some_and(|tail| tail.iter().all(|&b| b == 0));
    store
        .read(25 * PAGE_SIZE, &mut one)
        .map_err(|_| "a read past a cut failed")?;
    let past_zero = one.iter().all(|&b| b == 0);
    if !cut_kept || !past_zero || source.calls.load(Ordering::Relaxed) != calls {
        return Err("a cut store did not read zeros past the cut without asking its source");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Pipes and the calls about filesystems
//
// Every path and buffer lives in the check process's own memory, so what is
// checked is what a program meets: the flag words, the byte layouts, the
// errors, and which descriptors are left behind. No step waits: every read has
// something to read or is non-blocking, and every FIFO writer opens after its
// reader. The boot task has no process to be woken for.
// ---------------------------------------------------------------------------

/// What the pipe and filesystem call checks measured, for the boot log.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CallsReport {
    /// Bytes the checks moved through a pipe, a FIFO, `sendfile`, `splice`
    /// and `copy_file_range`.
    pub(crate) bytes: u64,
    /// Frames the second run cost. Zero, or a pipe, a FIFO's pipe or a file
    /// outlives its last descriptor.
    pub(crate) leaked: i64,
}

/// Where the checks keep their paths and buffers in the process's page.
const AT_FDS: u64 = 0;
/// `statfs`'s path.
const AT_TMP: u64 = 16;
/// The FIFO's path.
const AT_FIFO: u64 = 32;
/// The file `truncate`, `fallocate` and `sendfile` work on.
const AT_FILE: u64 = 64;
/// Where `sendfile` copies it to.
const AT_COPY: u64 = 96;
/// The bytes written into pipes.
const AT_DATA: u64 = 128;
/// Where reads land.
const AT_BACK: u64 = 256;
/// Where `statfs` writes, room for the widest layout.
const AT_STATFS: u64 = 512;
/// `sendfile`'s offset.
const AT_OFFSET: u64 = 768;
/// The file the `/dev/shm` check makes; in the room left after the offset.
const AT_SHM_FILE: u64 = 776;
/// The directory a second procfs is mounted on.
const AT_PROC_DIR: u64 = 800;
/// The directory a second devtmpfs is mounted on.
const AT_DEV_DIR: u64 = 832;
/// `proc`, as a type and as a source.
const AT_PROC_TYPE: u64 = 864;
/// `devtmpfs`, as a type and as a source.
const AT_DEVTMPFS_TYPE: u64 = 880;
/// `sysfs`, mounted and unmounted.
const AT_SYSFS_TYPE: u64 = 896;
/// An options string procfs has on Linux and does not read here.
const AT_OPTIONS: u64 = 912;
/// `devpts`, which is not a type here; after the options' 18 bytes.
const AT_DEVPTS_TYPE: u64 = 932;
/// `self` in the second procfs.
const AT_PROC_SELF: u64 = 944;
/// `/proc/self`, to compare it with.
const AT_SELF: u64 = 976;
/// `zero` in the second devtmpfs.
const AT_DEV_ZERO: u64 = 1000;
/// `/proc/mounts`.
const AT_MOUNTS: u64 = 1024;
/// The check process's own `stat` in the second procfs, written at run time:
/// room for the longest pid.
const AT_STAT: u64 = 1040;
/// Where the link `self` in the second procfs is read to.
const AT_LINK: u64 = 1104;
/// Where `/proc/self` is read to.
const AT_LINK_PROC: u64 = 1136;
/// Where zeros are read to.
const AT_ZEROS: u64 = 1168;
/// `/dev/null`, which `splice` drains a pipe into; after the zeros' 16.
const AT_DEV_NULL: u64 = 1184;
/// The two `iovec`s the FIFO's `readv` fills, after `/dev/null`'s ten.
const AT_IOVEC: u64 = 1200;
/// `/proc/<pid>/fd/<fd>` of a pipe's write end, written at run time: room
/// for the longest pid and descriptor, after the `iovec`s' 32.
const AT_FD_PATH: u64 = 1232;
/// `/dev/stdin`.
const AT_DEV_STDIN: u64 = 1264;
/// Where the link `/dev/stdin` is read to.
const AT_STDIN_LINK: u64 = 1280;
/// Where `/proc/mounts` is read to, to the end of the page.
const AT_LISTING: u64 = 1536;

const TMP: &[u8] = b"/tmp\0";
const FIFO: &[u8] = b"/tmp/stage8-fifo\0";
const FILE: &[u8] = b"/tmp/stage8-calls\0";
const COPY: &[u8] = b"/tmp/stage8-calls.copy\0";
const DATA: &[u8] = b"through a pipe\n";
const PROC_DIR: &[u8] = b"/tmp/stage8-proc\0";
const DEV_DIR: &[u8] = b"/tmp/stage8-dev\0";
const PROC_TYPE: &[u8] = b"proc\0";
const DEVTMPFS_TYPE: &[u8] = b"devtmpfs\0";
const SYSFS_TYPE: &[u8] = b"sysfs\0";
const DEVPTS_TYPE: &[u8] = b"devpts\0";
const OPTIONS: &[u8] = b"hidepid=invisible\0";
const PROC_SELF: &[u8] = b"/tmp/stage8-proc/self\0";
const SELF: &[u8] = b"/proc/self\0";
const DEV_ZERO: &[u8] = b"/tmp/stage8-dev/zero\0";
const MOUNTS: &[u8] = b"/proc/mounts\0";
const SHM_FILE: &[u8] = b"/dev/shm/stage8\0";
const DEV_NULL: &[u8] = b"/dev/null\0";
const DEV_STDIN: &[u8] = b"/dev/stdin\0";

/// The flags an init script mounts `/proc` with.
const PROC_FLAGS: u32 = MS_NOSUID | MS_NODEV | MS_NOEXEC | MS_RELATIME;
/// The flags an init script mounts `/dev` with.
const DEVTMPFS_FLAGS: u32 = MS_NOSUID | MS_RELATIME;

/// `AT_FDCWD`, as a register carries it.
const CWD: u64 = AT_FDCWD as i64 as u64;

/// An address no program may write, for the check that `pipe2` hands back
/// nothing it could not deliver.
const KERNEL_ADDRESS: u64 = u64::MAX - 0xFFF;

/// Run the pipe and filesystem call checks: twice, measured on the second,
/// for the reason [`run`] gives.
pub(crate) fn run_calls() -> Result<CallsReport, &'static str> {
    let process = process::new_for_check()
        .map_err(|_| "could not make a process for the file system calls")?;
    let _warm = check_the_calls(&process)?;
    let cached = fs::namespace().cached();
    crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)?;
    let window = mm::FrameWindow::open();
    let bytes = check_the_calls(&process)?;
    // The run mounts a fresh procfs and devtmpfs and walks into them. A dentry
    // of either that outlives its unmount is heap the frame count only notices
    // when the growth crosses a page, which is how this check once failed one
    // boot in many; so the cache is required not to grow at all.
    let cache_growth = i64::try_from(fs::namespace().cached()).unwrap_or(i64::MAX)
        - i64::try_from(cached).unwrap_or(i64::MAX);
    if cache_growth != 0 {
        crate::console::println!("  pipes    dentry cache {cache_growth:+} across the second run");
        return Err("the pipe and filesystem call checks left dentries behind in the cache");
    }
    crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)?;
    let leaked = window.kept();
    // Checked, not only printed: a count nothing tests would boot green
    // through the very leak it exists to show. The number and its sign are
    // printed first, so the report says which of the two it was.
    if leaked != 0 {
        crate::console::println!("  pipes    {leaked} frames across the second run");
        window.report("pipes");
    }
    if leaked < 0 {
        return Err(
            "the free frame count rose across the pipe and filesystem call checks: something outside them freed frames in the window",
        );
    }
    if leaked > 0 {
        return Err("the pipe and filesystem call checks kept frames they did not give back");
    }
    Ok(CallsReport { bytes, leaked })
}

/// A path without the terminator a program's copy carries.
fn name(path: &[u8]) -> &[u8] {
    path.strip_suffix(b"\0").unwrap_or(path)
}

/// One run: stage the page, check, and clean up whatever happened.
fn check_the_calls(process: &Process) -> Result<u64, &'static str> {
    let page = memory::sys_mmap(
        process,
        &MmapRequest {
            addr: 0,
            len: PAGE_SIZE,
            prot: PROT_READ | PROT_WRITE,
            flags: MAP_ANONYMOUS | MAP_PRIVATE,
            fd: -1,
            offset: 0,
            unit: OffsetUnit::Bytes,
        },
    )
    .map_err(|_| "a page for the file system call checks was refused")?;
    let page = u64::try_from(page).map_err(|_| "mmap returned an impossible address")?;
    for (offset, bytes) in [
        (AT_TMP, TMP),
        (AT_FIFO, FIFO),
        (AT_FILE, FILE),
        (AT_COPY, COPY),
        (AT_DATA, DATA),
        (AT_PROC_DIR, PROC_DIR),
        (AT_DEV_DIR, DEV_DIR),
        (AT_PROC_TYPE, PROC_TYPE),
        (AT_DEVTMPFS_TYPE, DEVTMPFS_TYPE),
        (AT_SYSFS_TYPE, SYSFS_TYPE),
        (AT_OPTIONS, OPTIONS),
        (AT_DEVPTS_TYPE, DEVPTS_TYPE),
        (AT_PROC_SELF, PROC_SELF),
        (AT_SELF, SELF),
        (AT_DEV_ZERO, DEV_ZERO),
        (AT_MOUNTS, MOUNTS),
        (AT_SHM_FILE, SHM_FILE),
        (AT_DEV_NULL, DEV_NULL),
        (AT_DEV_STDIN, DEV_STDIN),
    ] {
        uaccess::copy_to_user(process.space(), page + offset, bytes)
            .map_err(|_| "could not stage the file system call checks")?;
    }

    let outcome = check_a_pipe_carries_bytes_and_then_ends(process, page)
        .and_then(|piped| check_a_pipe_refuses_as_linux_does(process, page).map(|()| piped))
        .and_then(|piped| check_a_packet_pipe_keeps_writes_apart(process, page).map(|()| piped))
        .and_then(|piped| check_fionbio_reaches_pipes_and_sockets(process, page).map(|()| piped))
        .and_then(|piped| {
            check_a_pipe_keeps_what_a_bad_buffer_missed(process, page).map(|()| piped)
        })
        .and_then(|piped| check_a_stream_read_takes_all_there_is(process, page).map(|()| piped))
        .and_then(|piped| check_a_fifo_is_one_pipe(process, page).map(|fifo| piped + fifo))
        .and_then(|piped| {
            check_a_pipe_opens_again_through_proc(process, page).map(|again| piped + again)
        })
        .and_then(|bytes| check_statfs_says_tmp_is_tmpfs(process, page).map(|()| bytes))
        .and_then(|bytes| check_truncate_and_fallocate_grow(process, page).map(|()| bytes))
        .and_then(|bytes| check_a_new_file_is_dated_now().map(|()| bytes))
        .and_then(|bytes| check_sendfile_copies_a_file(process, page).map(|sent| bytes + sent))
        .and_then(|bytes| check_splice_moves_bytes(process, page).map(|moved| bytes + moved))
        .and_then(|bytes| check_splice_keeps_what_the_output_refused(process, page).map(|()| bytes))
        .and_then(|bytes| {
            check_copy_file_range_copies_a_file(process, page).map(|copied| bytes + copied)
        })
        .and_then(|bytes| check_dev_shm_holds_a_file(process, page).map(|()| bytes))
        .and_then(|bytes| check_proc_and_devtmpfs_mount(process, page).map(|()| bytes));

    // Cleaned up whatever happened, so a failure reports itself and not also
    // leaked frames.
    for fd in 3..32 {
        let _ = fd::sys_close(process, fd);
    }
    for dir in [AT_PROC_DIR, AT_DEV_DIR] {
        let _ = by_number(process, Syscall::Umount2, [page + dir, 0, 0, 0, 0, 0]);
        let _ = by_number(
            process,
            Syscall::Unlinkat,
            [CWD, page + dir, u64::from(AT_REMOVEDIR), 0, 0, 0],
        );
    }
    let ns = fs::namespace();
    let ctx = ns.context();
    for path in [FIFO, FILE, COPY, SHM_FILE] {
        let _ = ns.unlink(&ctx, None, name(path));
    }
    let _ = memory::sys_munmap(process, page, PAGE_SIZE);
    outcome
}

/// Require a handler to have answered `want`.
fn answers(got: Result<usize, Errno>, want: usize, what: &'static str) -> Result<(), &'static str> {
    if got == Ok(want) { Ok(()) } else { Err(what) }
}

/// Require a handler to have refused with `errno`.
fn refuses(
    got: Result<usize, Errno>,
    errno: Errno,
    what: &'static str,
) -> Result<(), &'static str> {
    if got == Err(errno) { Ok(()) } else { Err(what) }
}

/// A descriptor a handler returned.
fn descriptor(got: Result<usize, Errno>, what: &'static str) -> Result<i32, &'static str> {
    got.ok().and_then(|fd| i32::try_from(fd).ok()).ok_or(what)
}

/// `len` bytes of the process's memory at `at`.
fn read_back(process: &Process, at: u64, len: usize) -> Result<Vec<u8>, &'static str> {
    let mut out = vec![0_u8; len];
    uaccess::copy_from_user(process.space(), at, &mut out)
        .map_err(|_| "the check could not read its own page back")?;
    Ok(out)
}

/// The two descriptors `pipe2` wrote.
fn pair(process: &Process, page: u64) -> Result<(i32, i32), &'static str> {
    let bytes = read_back(process, page + AT_FDS, 8)?;
    let [a, b, c, d, e, f, g, h] = <[u8; 8]>::try_from(bytes).map_err(|_| "a short pair")?;
    Ok((
        i32::from_le_bytes([a, b, c, d]),
        i32::from_le_bytes([e, f, g, h]),
    ))
}

/// The filesystem magic number `statfs` wrote: the first four bytes in every
/// layout, because `f_type` comes first and the magic numbers fit in 32 bits.
fn magic(process: &Process, page: u64) -> Result<u64, &'static str> {
    let bytes = read_back(process, page + AT_STATFS, 4)?;
    let word = <[u8; 4]>::try_from(bytes).map_err(|_| "a short magic number")?;
    Ok(u64::from(u32::from_le_bytes(word)))
}

/// The size of the file at `path`, as `stat` reports it.
fn size_is(path: &[u8], want: u64, what: &'static str) -> Result<(), &'static str> {
    let ns = fs::namespace();
    let size = ns
        .resolve(&ns.context(), None, name(path), true)
        .and_then(|at| ns.stat(&at))
        .map_err(|_| "a file the check made would not stat")?
        .metadata
        .size;
    if size == want { Ok(()) } else { Err(what) }
}

/// Bytes written into a pipe come out of it; a pipe says it is on pipefs and
/// cannot be sought or synced; and once its writer is closed its reader reads
/// end of file.
fn check_a_pipe_carries_bytes_and_then_ends(
    process: &Process,
    page: u64,
) -> Result<u64, &'static str> {
    answers(
        pipe::sys_pipe2(process, page + AT_FDS, 0),
        0,
        "pipe2 was refused",
    )?;
    let (reader, writer) = pair(process, page)?;
    let len = DATA.len();
    answers(
        file::sys_write(process, writer, page + AT_DATA, len as u64),
        len,
        "a write into a pipe came back short",
    )?;
    answers(
        file::sys_read(process, reader, page + AT_BACK, 64),
        len,
        "a read from a pipe did not return what was queued",
    )?;
    if read_back(process, page + AT_BACK, len)? != DATA {
        return Err("a pipe gave back different bytes than were written into it");
    }

    answers(
        fsctl::sys_fstatfs(process, reader, page + AT_STATFS),
        0,
        "fstatfs on a pipe was refused",
    )?;
    if magic(process, page)? != PIPEFS_MAGIC {
        return Err("fstatfs on a pipe did not report PIPEFS_MAGIC");
    }
    refuses(
        fd::sys_lseek(process, reader, 0, SEEK_CUR),
        Errno::ESPIPE,
        "a pipe could be sought",
    )?;
    refuses(
        fsctl::sys_fsync(process, writer, false),
        Errno::EINVAL,
        "fsync on a pipe was not EINVAL",
    )?;

    answers(
        fd::sys_close(process, writer),
        0,
        "a pipe's write end would not close",
    )?;
    answers(
        file::sys_read(process, reader, page + AT_BACK, 64),
        0,
        "a pipe with no writer left did not read end of file",
    )?;
    answers(
        fd::sys_close(process, reader),
        0,
        "a pipe's read end would not close",
    )?;
    Ok(len as u64)
}

/// `pipe2(O_DIRECT)`, a packet pipe: two writes read back as two reads however
/// large the reader's buffer, and a read too small for a packet takes what fits
/// and leaves nothing of it behind, as Linux's packet pipes do. The Steam
/// client's controller code asserts without one.
fn check_a_packet_pipe_keeps_writes_apart(
    process: &Process,
    page: u64,
) -> Result<(), &'static str> {
    let direct = crate::arch::OPEN_FLAGS.direct;
    answers(
        pipe::sys_pipe2(process, page + AT_FDS, direct | O_NONBLOCK),
        0,
        "pipe2 with O_DIRECT was refused",
    )?;
    let (reader, writer) = pair(process, page)?;
    // "through" and " a pipe\n": two packets.
    let (first, second) = (7_u64, DATA.len() as u64 - 7);
    for (at, len) in [(0, first), (first, second), (0, first)] {
        answers(
            file::sys_write(process, writer, page + AT_DATA + at, len),
            len as usize,
            "a write into a packet pipe was short",
        )?;
    }
    for len in [first, second] {
        answers(
            file::sys_read(process, reader, page + AT_BACK, 64),
            len as usize,
            "a packet pipe's read did not stop at the end of one write",
        )?;
    }
    answers(
        file::sys_read(process, reader, page + AT_BACK, 3),
        3,
        "a short read of a packet did not take what fit",
    )?;
    refuses(
        file::sys_read(process, reader, page + AT_BACK, 64),
        Errno::EAGAIN,
        "a short read of a packet left the rest of it in the pipe",
    )?;
    for end in [reader, writer] {
        answers(
            fd::sys_close(process, end),
            0,
            "a packet pipe's end would not close",
        )?;
    }
    Ok(())
}

/// An empty non-blocking pipe answers `EAGAIN`; a write with no reader left is
/// `EPIPE`; `pipe2` takes only its flags; and a pair it cannot hand back is
/// closed again.
fn check_a_pipe_refuses_as_linux_does(process: &Process, page: u64) -> Result<(), &'static str> {
    answers(
        pipe::sys_pipe2(process, page + AT_FDS, O_NONBLOCK | O_CLOEXEC),
        0,
        "pipe2 with O_NONBLOCK and O_CLOEXEC was refused",
    )?;
    let (reader, writer) = pair(process, page)?;
    refuses(
        file::sys_read(process, reader, page + AT_BACK, 1),
        Errno::EAGAIN,
        "an empty non-blocking pipe did not answer EAGAIN",
    )?;
    answers(
        fd::sys_fcntl(process, writer, F_GETFD, 0),
        FD_CLOEXEC as usize,
        "pipe2's O_CLOEXEC did not reach the descriptor",
    )?;
    answers(
        fd::sys_close(process, reader),
        0,
        "a pipe's read end would not close",
    )?;
    refuses(
        file::sys_write(process, writer, page + AT_DATA, 1),
        Errno::EPIPE,
        "a write with no reader left was not EPIPE",
    )?;
    answers(
        fd::sys_close(process, writer),
        0,
        "a pipe's write end would not close",
    )?;

    refuses(
        pipe::sys_pipe2(process, page + AT_FDS, O_APPEND),
        Errno::EINVAL,
        "pipe2 accepted a flag it does not take",
    )?;
    refuses(
        pipe::sys_pipe2(process, KERNEL_ADDRESS, 0),
        Errno::EFAULT,
        "pipe2 into memory the program cannot write was not EFAULT",
    )?;
    refuses(
        fd::sys_close(process, 3),
        Errno::EBADF,
        "a pipe2 that failed left a descriptor behind",
    )
}

/// `ioctl(FIONBIO)` is every file's, not only a terminal's: on a pipe made
/// blocking it makes a read of the empty pipe `EAGAIN` and `F_GETFL` report
/// `O_NONBLOCK`, and with zero it clears the flag again; on an `AF_UNIX`
/// socket it does the same; and an unreadable argument is `EFAULT`.
/// `FIOCLEX` and `FIONCLEX` set and clear close-on-exec, reading nothing.
fn check_fionbio_reaches_pipes_and_sockets(
    process: &Process,
    page: u64,
) -> Result<(), &'static str> {
    let fionbio = |fd: i32, on: u32| -> Result<usize, Errno> {
        uaccess::copy_to_user(process.space(), page + AT_OFFSET, &on.to_le_bytes())
            .map_err(|_| Errno::EFAULT)?;
        fd::sys_ioctl(process, fd, FIONBIO, page + AT_OFFSET)
    };
    let nonblocking = |fd: i32| {
        fd::sys_fcntl(process, fd, F_GETFL, 0).map(|flags| flags as u32 & O_NONBLOCK != 0)
    };
    answers(
        pipe::sys_pipe2(process, page + AT_FDS, 0),
        0,
        "pipe2 was refused",
    )?;
    let (reader, writer) = pair(process, page)?;
    answers(fionbio(reader, 1), 0, "FIONBIO on a pipe was refused")?;
    if nonblocking(reader) != Ok(true) {
        return Err("FIONBIO on a pipe did not set O_NONBLOCK");
    }
    refuses(
        file::sys_read(process, reader, page + AT_BACK, 1),
        Errno::EAGAIN,
        "a pipe FIONBIO made non-blocking did not answer EAGAIN",
    )?;
    answers(fionbio(reader, 0), 0, "FIONBIO off on a pipe was refused")?;
    if nonblocking(reader) != Ok(false) {
        return Err("FIONBIO with zero did not clear O_NONBLOCK");
    }
    refuses(
        fd::sys_ioctl(process, reader, FIONBIO, KERNEL_ADDRESS),
        Errno::EFAULT,
        "FIONBIO with an unreadable argument was not EFAULT",
    )?;
    // The close-on-exec pair read no argument, so an unreadable one is fine.
    answers(
        fd::sys_ioctl(process, reader, FIOCLEX, KERNEL_ADDRESS),
        0,
        "FIOCLEX on a pipe was refused",
    )?;
    answers(
        fd::sys_fcntl(process, reader, F_GETFD, 0),
        FD_CLOEXEC as usize,
        "FIOCLEX did not set close-on-exec",
    )?;
    answers(
        fd::sys_ioctl(process, reader, FIONCLEX, KERNEL_ADDRESS),
        0,
        "FIONCLEX on a pipe was refused",
    )?;
    answers(
        fd::sys_fcntl(process, reader, F_GETFD, 0),
        0,
        "FIONCLEX did not clear close-on-exec",
    )?;
    answers(
        fd::sys_close(process, reader),
        0,
        "a pipe's read end would not close",
    )?;
    answers(
        fd::sys_close(process, writer),
        0,
        "a pipe's write end would not close",
    )?;

    const AF_UNIX: u64 = 1;
    const SOCK_STREAM: u64 = 1;
    answers(
        by_number(
            process,
            Syscall::Socketpair,
            [AF_UNIX, SOCK_STREAM, 0, page + AT_FDS, 0, 0],
        ),
        0,
        "socketpair was refused",
    )?;
    let (one, other) = pair(process, page)?;
    answers(fionbio(one, 1), 0, "FIONBIO on a socket was refused")?;
    refuses(
        file::sys_read(process, one, page + AT_BACK, 1),
        Errno::EAGAIN,
        "a socket FIONBIO made non-blocking did not answer EAGAIN",
    )?;
    answers(fd::sys_close(process, one), 0, "a socket would not close")?;
    answers(fd::sys_close(process, other), 0, "a socket would not close")
}

/// A read from a pipe into memory the program cannot write is `EFAULT` and
/// leaves every byte in the pipe, as Linux 7.0 does, measured: `read` and
/// `readv` into a bad buffer took nothing, and a read after them had all
/// eight. A `readv` whose first segment is good and second bad keeps what
/// the second missed. There Linux answers `EFAULT` too, having copied into
/// the first segment and consumed none of it -- it consumes a pipe by
/// buffer pages, which merge small writes -- and this answers the four the
/// first segment took; either way no byte is lost, which is what is checked.
fn check_a_pipe_keeps_what_a_bad_buffer_missed(
    process: &Process,
    page: u64,
) -> Result<(), &'static str> {
    answers(
        pipe::sys_pipe2(process, page + AT_FDS, O_NONBLOCK),
        0,
        "pipe2 was refused",
    )?;
    let (reader, writer) = pair(process, page)?;
    let len = DATA.len();
    answers(
        file::sys_write(process, writer, page + AT_DATA, len as u64),
        len,
        "a write into a pipe came back short",
    )?;
    refuses(
        file::sys_read(process, reader, KERNEL_ADDRESS, 8),
        Errno::EFAULT,
        "a read from a pipe into a bad buffer was not EFAULT",
    )?;
    put_iovecs(process, page, &[KERNEL_ADDRESS, 4, KERNEL_ADDRESS, 4])?;
    refuses(
        file::sys_readv(process, reader, page + AT_IOVEC, 2),
        Errno::EFAULT,
        "a readv from a pipe into bad buffers was not EFAULT",
    )?;
    put_iovecs(process, page, &[page + AT_BACK, 4, KERNEL_ADDRESS, 4])?;
    let took = file::sys_readv(process, reader, page + AT_IOVEC, 2)
        .map_err(|_| "a readv from a pipe into a good and a bad segment took nothing")?;
    let rest = file::sys_read(process, reader, page + AT_BACK + took as u64, 64)
        .map_err(|_| "a pipe read after EFAULT was refused")?;
    if took + rest != len || read_back(process, page + AT_BACK, len)? != DATA {
        return Err("a pipe lost the bytes a read into a bad buffer could not take");
    }
    answers(
        fd::sys_close(process, reader),
        0,
        "a pipe's read end would not close",
    )?;
    answers(
        fd::sys_close(process, writer),
        0,
        "a pipe's write end would not close",
    )
}

/// How much the fill check reads at once: sixteen pages, as a program
/// reading /dev/zero in 64 KiB blocks does.
const FILL: u64 = 16 * PAGE_SIZE;

/// A read of a pipe or a memory device takes all they have, as Linux's does,
/// rather than one page and then a stop -- measured on a 7.0 host: `readv`
/// of a pipe holding six bytes into two segments of four is 6, `read` of
/// 65536 from a pipe holding 8292 bytes written in three pieces is 8292, and
/// from /dev/zero `readv` into two segments of eight is 16 and `read` and
/// `pread` of 65536 are 65536. The pipe is a blocking one with its writer
/// open, so a read that waited for more rather than returning what was
/// there would hang the boot here.
fn check_a_stream_read_takes_all_there_is(
    process: &Process,
    page: u64,
) -> Result<(), &'static str> {
    let region = memory::sys_mmap(
        process,
        &MmapRequest {
            addr: 0,
            len: FILL + PAGE_SIZE,
            prot: PROT_READ | PROT_WRITE,
            flags: MAP_ANONYMOUS | MAP_PRIVATE,
            fd: -1,
            offset: 0,
            unit: OffsetUnit::Bytes,
        },
    )
    .map_err(|_| "no memory for the stream fill check")?;
    let region = u64::try_from(region).map_err(|_| "mmap returned an impossible address")?;
    let outcome = fill_from_a_pipe(process, page, region)
        .and_then(|()| fill_from_a_socket(process, page, region))
        .and_then(|()| fill_from_zero(process, page, region));
    let _ = memory::sys_munmap(process, region, FILL + PAGE_SIZE);
    outcome
}

/// The pipe half of [`check_a_stream_read_takes_all_there_is`].
fn fill_from_a_pipe(process: &Process, page: u64, region: u64) -> Result<(), &'static str> {
    answers(
        pipe::sys_pipe2(process, page + AT_FDS, 0),
        0,
        "pipe2 was refused",
    )?;
    let (reader, writer) = pair(process, page)?;
    answers(
        file::sys_write(process, writer, page + AT_DATA, 6),
        6,
        "a write into a pipe came back short",
    )?;
    put_iovecs(process, page, &[page + AT_BACK, 4, page + AT_BACK + 4, 4])?;
    answers(
        file::sys_readv(process, reader, page + AT_IOVEC, 2),
        6,
        "readv of a pipe holding six bytes into two segments of four did not take all six",
    )?;
    if read_back(process, page + AT_BACK, 6)? != DATA.get(..6).unwrap_or_default() {
        return Err("readv of a pipe gave back different bytes than were written");
    }
    for piece in [PAGE_SIZE, PAGE_SIZE, 100] {
        answers(
            file::sys_write(process, writer, region, piece),
            usize::try_from(piece).unwrap_or(0),
            "a write into a pipe came back short",
        )?;
    }
    answers(
        file::sys_read(process, reader, region, FILL),
        usize::try_from(2 * PAGE_SIZE + 100).unwrap_or(0),
        "a read of a pipe holding more than a page stopped at the first page",
    )?;
    answers(
        fd::sys_close(process, reader),
        0,
        "a pipe's read end would not close",
    )?;
    answers(
        fd::sys_close(process, writer),
        0,
        "a pipe's write end would not close",
    )
}

/// A connected pair of Unix stream sockets, `socketpair`'s two descriptors.
fn stream_socket_pair(process: &Process, page: u64) -> Result<(i32, i32), &'static str> {
    const AF_UNIX: u64 = 1;
    const SOCK_STREAM: u64 = 1;
    answers(
        by_number(
            process,
            Syscall::Socketpair,
            [AF_UNIX, SOCK_STREAM, 0, page + AT_FDS, 0, 0],
        ),
        0,
        "socketpair was refused",
    )?;
    pair(process, page)
}

/// The Unix stream socket part of [`check_a_stream_read_takes_all_there_is`]:
/// a socket reads on through what is queued as a pipe does, measured on a
/// 7.0 host -- `readv` of six bytes into two segments of four is 6, and
/// `read` of 65536 from writes of 4096, 4096 and 100 is 8292. Blocking, with
/// the peer open, so a read that waited for more would hang the boot here.
fn fill_from_a_socket(process: &Process, page: u64, region: u64) -> Result<(), &'static str> {
    let (one, other) = stream_socket_pair(process, page)?;
    answers(
        file::sys_write(process, one, page + AT_DATA, 6),
        6,
        "a write into a socket came back short",
    )?;
    put_iovecs(process, page, &[page + AT_BACK, 4, page + AT_BACK + 4, 4])?;
    answers(
        file::sys_readv(process, other, page + AT_IOVEC, 2),
        6,
        "readv of a socket holding six bytes into two segments of four did not take all six",
    )?;
    if read_back(process, page + AT_BACK, 6)? != DATA.get(..6).unwrap_or_default() {
        return Err("readv of a socket gave back different bytes than were written");
    }
    for piece in [PAGE_SIZE, PAGE_SIZE, 100] {
        answers(
            file::sys_write(process, one, region, piece),
            usize::try_from(piece).unwrap_or(0),
            "a write into a socket came back short",
        )?;
    }
    answers(
        file::sys_read(process, other, region, FILL),
        usize::try_from(2 * PAGE_SIZE + 100).unwrap_or(0),
        "a read of a socket holding more than a page stopped at the first page",
    )?;
    answers(fd::sys_close(process, one), 0, "a socket would not close")?;
    answers(fd::sys_close(process, other), 0, "a socket would not close")
}

/// The /dev/zero half of [`check_a_stream_read_takes_all_there_is`]. Its
/// path is staged past the buffer, in the region's last page.
fn fill_from_zero(process: &Process, page: u64, region: u64) -> Result<(), &'static str> {
    let path = region + FILL;
    uaccess::copy_to_user(process.space(), path, b"/dev/zero\0")
        .map_err(|_| "could not stage /dev/zero's path")?;
    uaccess::copy_to_user(process.space(), region, &vec![0xA5; FILL as usize])
        .map_err(|_| "could not stage a buffer for zeros")?;
    let zero = descriptor(
        by_number(
            process,
            Syscall::Openat,
            [CWD, path, u64::from(O_RDONLY), 0, 0, 0],
        ),
        "/dev/zero would not open",
    )?;
    let filled = file::sys_read(process, zero, region, FILL);
    let clean = read_back(process, region, FILL as usize)?
        .iter()
        .all(|&b| b == 0);
    let positioned = file::sys_pread64(process, zero, region, FILL, 1000);
    put_iovecs(process, page, &[page + AT_BACK, 8, page + AT_BACK + 8, 8])?;
    let vectored = file::sys_readv(process, zero, page + AT_IOVEC, 2);
    answers(fd::sys_close(process, zero), 0, "/dev/zero would not close")?;
    answers(
        filled,
        FILL as usize,
        "a read of 64 KiB from /dev/zero was not filled",
    )?;
    if !clean {
        return Err("a read of 64 KiB from /dev/zero left something other than zeros");
    }
    answers(
        positioned,
        FILL as usize,
        "a pread of 64 KiB from /dev/zero was not filled",
    )?;
    answers(
        vectored,
        16,
        "readv of /dev/zero into two segments of eight did not fill both",
    )
}

/// Write `words` as native words at [`AT_IOVEC`]: the `iovec` array a
/// `readv` is handed, a base and a length each.
fn put_iovecs(process: &Process, page: u64, words: &[u64]) -> Result<(), &'static str> {
    let word = size_of::<usize>();
    for (index, value) in (0_u64..).zip(words) {
        let bytes = value.to_le_bytes();
        let native = bytes.get(..word).ok_or("impossible pointer width")?;
        uaccess::copy_to_user(
            process.space(),
            page + AT_IOVEC + index * word as u64,
            native,
        )
        .map_err(|_| "could not stage an iovec")?;
    }
    Ok(())
}

/// A FIFO under /tmp is one pipe for every opener: a non-blocking writer with
/// no reader is `ENXIO`, and once a reader is open what one descriptor writes
/// the other reads.
fn check_a_fifo_is_one_pipe(process: &Process, page: u64) -> Result<u64, &'static str> {
    let ns = fs::namespace();
    ns.mknod(&ns.context(), None, name(FIFO), NewNode::Fifo, 0o600)
        .map_err(|_| "a FIFO could not be made under /tmp")?;
    refuses(
        fd::sys_openat(process, AT_FDCWD, page + AT_FIFO, O_WRONLY | O_NONBLOCK, 0),
        Errno::ENXIO,
        "a non-blocking open of a FIFO for writing, with no reader, was not ENXIO",
    )?;
    let reader = descriptor(
        fd::sys_openat(process, AT_FDCWD, page + AT_FIFO, O_RDONLY | O_NONBLOCK, 0),
        "a FIFO would not open for reading",
    )?;
    // A blocking open, which does not wait: a reader is already there.
    let writer = descriptor(
        fd::sys_openat(process, AT_FDCWD, page + AT_FIFO, O_WRONLY, 0),
        "a FIFO would not open for writing",
    )?;
    let len = DATA.len();
    answers(
        file::sys_write(process, writer, page + AT_DATA, len as u64),
        len,
        "a write into a FIFO came back short",
    )?;
    answers(
        file::sys_read(process, reader, page + AT_BACK, 64),
        len,
        "a second open of a FIFO did not read what the first wrote",
    )?;
    if read_back(process, page + AT_BACK, len)? != DATA {
        return Err("a FIFO gave back different bytes than were written into it");
    }
    refuses(
        file::sys_read(process, reader, page + AT_BACK, 64),
        Errno::EAGAIN,
        "a drained FIFO with a writer still open did not answer EAGAIN",
    )?;
    check_a_fifo_reads_as_its_pipe(process, page, reader, writer)?;
    answers(
        fd::sys_close(process, writer),
        0,
        "a FIFO's writer would not close",
    )?;
    answers(
        fd::sys_close(process, reader),
        0,
        "a FIFO's reader would not close",
    )?;
    Ok(len as u64)
}

/// A pipe's end opens again through `/proc/<pid>/fd/<fd>`, as bash's
/// `>(...)` opens `/dev/fd/63`: the new descriptor writes into the same pipe,
/// the old one can close without ending it, and closing the new one does --
/// the reopened end holds nothing of the old one open. And `/dev/stdin` is
/// the link every Linux `/dev` carries to it.
fn check_a_pipe_opens_again_through_proc(
    process: &Process,
    page: u64,
) -> Result<u64, &'static str> {
    answers(
        pipe::sys_pipe2(process, page + AT_FDS, 0),
        0,
        "pipe2 was refused",
    )?;
    let (reader, writer) = pair(process, page)?;
    let path = alloc::format!("/proc/{}/fd/{writer}\0", process.pid());
    uaccess::copy_to_user(process.space(), page + AT_FD_PATH, path.as_bytes())
        .map_err(|_| "could not stage a pipe's /proc/<pid>/fd path")?;
    let again = descriptor(
        fd::sys_openat(process, AT_FDCWD, page + AT_FD_PATH, O_WRONLY, 0),
        "a pipe's write end would not open again through /proc/<pid>/fd",
    )?;
    answers(
        fd::sys_close(process, writer),
        0,
        "a pipe's write end would not close",
    )?;
    let len = DATA.len();
    answers(
        file::sys_write(process, again, page + AT_DATA, len as u64),
        len,
        "a write into a pipe opened through /proc/<pid>/fd came back short",
    )?;
    answers(
        file::sys_read(process, reader, page + AT_BACK, 64),
        len,
        "a pipe did not read what its end opened through /proc/<pid>/fd wrote",
    )?;
    if read_back(process, page + AT_BACK, len)? != DATA {
        return Err("a pipe opened through /proc/<pid>/fd gave back different bytes");
    }
    answers(
        fd::sys_close(process, again),
        0,
        "a pipe's end opened through /proc/<pid>/fd would not close",
    )?;
    answers(
        file::sys_read(process, reader, page + AT_BACK, 64),
        0,
        "a pipe whose writers opened through /proc/<pid>/fd had all closed did not read end of \
         file",
    )?;
    answers(
        fd::sys_close(process, reader),
        0,
        "a pipe's read end would not close",
    )?;
    let want = b"/proc/self/fd/0";
    answers(
        by_number(
            process,
            Syscall::Readlinkat,
            [CWD, page + AT_DEV_STDIN, page + AT_STDIN_LINK, 32, 0, 0],
        ),
        want.len(),
        "/dev/stdin is not a link of /proc/self/fd/0's length",
    )?;
    if read_back(process, page + AT_STDIN_LINK, want.len())? != want {
        return Err("/dev/stdin does not lead to /proc/self/fd/0");
    }
    Ok(len as u64)
}

/// `readv` of a FIFO under /tmp stops where it stops on an anonymous pipe.
///
/// The read loop asks whether the file is a stream, and a stream stops at the
/// first segment that took anything, since asking again may wait. It asks
/// what reads go to, and a FIFO's node on tmpfs is no stream but its pipe is.
/// Asked of the node, as it once was, `readv` read on into the next segment
/// as a file's does, and a blocking reader would have waited there for bytes
/// its writer might never send.
fn check_a_fifo_reads_as_its_pipe(
    process: &Process,
    page: u64,
    reader: i32,
    writer: i32,
) -> Result<(), &'static str> {
    answers(
        pipe::sys_pipe2(process, page + AT_FDS, O_NONBLOCK),
        0,
        "pipe2 was refused",
    )?;
    let (pipe_reader, pipe_writer) = pair(process, page)?;
    let piped = readv_after_write(process, page, pipe_reader, pipe_writer);
    answers(
        fd::sys_close(process, pipe_reader),
        0,
        "a pipe's read end would not close",
    )?;
    answers(
        fd::sys_close(process, pipe_writer),
        0,
        "a pipe's write end would not close",
    )?;
    if readv_after_write(process, page, reader, writer)? != piped? {
        return Err(
            "readv of a FIFO under /tmp did not stop where a pipe's does: it read the FIFO as a \
             file with a position rather than as its pipe",
        );
    }
    Ok(())
}

/// Write [`DATA`] into `writer`, `readv` it from `reader` into two segments,
/// four bytes and then sixty, and read whatever that left. Returns what the
/// `readv` took.
fn readv_after_write(
    process: &Process,
    page: u64,
    reader: i32,
    writer: i32,
) -> Result<usize, &'static str> {
    let len = DATA.len();
    answers(
        file::sys_write(process, writer, page + AT_DATA, len as u64),
        len,
        "a write into a pipe came back short",
    )?;
    put_iovecs(process, page, &[page + AT_BACK, 4, page + AT_BACK + 4, 60])?;
    let took = file::sys_readv(process, reader, page + AT_IOVEC, 2)
        .map_err(|_| "readv of a pipe with bytes in it was refused")?;
    let rest = if took < len {
        file::sys_read(process, reader, page + AT_BACK + took as u64, 64)
            .map_err(|_| "a read after a short readv was refused")?
    } else {
        0
    };
    if took + rest != len || read_back(process, page + AT_BACK, len)? != DATA {
        return Err("a readv and the read after it did not give back what was written");
    }
    Ok(took)
}

/// `statfs` of /tmp reports tmpfs in this word size's layout, and in
/// ARMv7-A's packed `statfs64`, whose size argument takes the kernel's 84 and
/// musl's 88 and nothing else.
fn check_statfs_says_tmp_is_tmpfs(process: &Process, page: u64) -> Result<(), &'static str> {
    answers(
        fsctl::sys_statfs(process, page + AT_TMP, page + AT_STATFS),
        0,
        "statfs of /tmp was refused",
    )?;
    if magic(process, page)? != TMPFS_MAGIC {
        return Err("statfs of /tmp did not report TMPFS_MAGIC");
    }
    for size in [84, 88] {
        answers(
            fsctl::sys_statfs64(process, page + AT_TMP, size, page + AT_STATFS),
            0,
            "statfs64 refused the size the kernel or musl passes",
        )?;
    }
    // TMPFS_MAGIC, then a block size of 4096, both 32 bits little-endian.
    if read_back(process, page + AT_STATFS, 8)? != [0x94, 0x19, 0x02, 0x01, 0, 0x10, 0, 0] {
        return Err("statfs64 did not pack the magic number and block size first");
    }
    refuses(
        fsctl::sys_statfs64(process, page + AT_TMP, 120, page + AT_STATFS),
        Errno::EINVAL,
        "statfs64 accepted a size that is neither structure's",
    )
}

/// `truncate` by path and `fallocate` on a descriptor both grow a file under
/// /tmp; `fallocate` never shrinks one, keeps its size when asked to, and
/// refuses a mode it does not have.
fn check_truncate_and_fallocate_grow(process: &Process, page: u64) -> Result<(), &'static str> {
    let made = descriptor(
        fd::sys_openat(
            process,
            AT_FDCWD,
            page + AT_FILE,
            O_RDWR | O_CREAT | O_TRUNC,
            0o644,
        ),
        "a file could not be created under /tmp",
    )?;
    let len = DATA.len();
    answers(
        file::sys_write(process, made, page + AT_DATA, len as u64),
        len,
        "a write to a file under /tmp came back short",
    )?;
    answers(
        fsctl::sys_truncate(process, page + AT_FILE, 5000),
        0,
        "truncate to a larger size was refused",
    )?;
    size_is(FILE, 5000, "truncate did not grow the file")?;
    answers(
        fsctl::sys_fallocate(process, made, 0, 0, 9000),
        0,
        "fallocate was refused",
    )?;
    size_is(FILE, 9000, "fallocate did not grow the file")?;
    answers(
        fsctl::sys_fallocate(process, made, 0, 0, 100),
        0,
        "fallocate of a range inside the file was refused",
    )?;
    answers(
        fsctl::sys_fallocate(process, made, FALLOC_FL_KEEP_SIZE, 0, 20_000),
        0,
        "fallocate with FALLOC_FL_KEEP_SIZE was refused",
    )?;
    size_is(
        FILE,
        9000,
        "fallocate shrank the file, or grew it despite KEEP_SIZE",
    )?;
    refuses(
        fsctl::sys_fallocate(
            process,
            made,
            FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE,
            0,
            1,
        ),
        Errno::EOPNOTSUPP,
        "fallocate accepted a mode it does not have",
    )?;
    refuses(
        fsctl::sys_truncate(process, page + AT_FILE, -1),
        Errno::EINVAL,
        "truncate to a negative length was not EINVAL",
    )?;
    refuses(
        fsctl::sys_truncate(process, page + AT_TMP, 0),
        Errno::EISDIR,
        "truncate of a directory was not EISDIR",
    )?;
    answers(
        fsctl::sys_fsync(process, made, false),
        0,
        "fsync of a file was refused",
    )?;
    answers(fd::sys_close(process, made), 0, "a file would not close")
}

/// `sendfile` copies a file: from an offset it reads there, moves the offset
/// and leaves the file position alone; without one it moves the position; and
/// an input not open for reading is `EBADF`.
fn check_sendfile_copies_a_file(process: &Process, page: u64) -> Result<u64, &'static str> {
    let source = descriptor(
        fd::sys_openat(process, AT_FDCWD, page + AT_FILE, O_RDONLY, 0),
        "the file to send would not open",
    )?;
    let copy = descriptor(
        fd::sys_openat(
            process,
            AT_FDCWD,
            page + AT_COPY,
            O_WRONLY | O_CREAT | O_TRUNC,
            0o644,
        ),
        "a file to send into could not be created",
    )?;
    uaccess::copy_to_user(process.space(), page + AT_OFFSET, &10_u64.to_le_bytes())
        .map_err(|_| "could not stage sendfile's offset")?;
    answers(
        pipe::sys_sendfile(process, copy, source, page + AT_OFFSET, 1 << 20),
        8990,
        "sendfile from an offset did not send the rest of the file",
    )?;
    if read_back(process, page + AT_OFFSET, 8)? != 9000_u64.to_le_bytes() {
        return Err("sendfile did not move its offset past what it sent");
    }
    answers(
        fd::sys_lseek(process, source, 0, SEEK_CUR),
        0,
        "sendfile with an offset moved the file position",
    )?;
    answers(
        pipe::sys_sendfile(process, copy, source, 0, 1 << 20),
        9000,
        "sendfile from the file position did not send the whole file",
    )?;
    answers(
        fd::sys_lseek(process, source, 0, SEEK_CUR),
        9000,
        "sendfile without an offset did not move the file position",
    )?;
    refuses(
        pipe::sys_sendfile(process, source, copy, 0, 1),
        Errno::EBADF,
        "sendfile read from a descriptor open only for writing",
    )?;
    answers(fd::sys_close(process, copy), 0, "the copy would not close")?;
    answers(
        fd::sys_close(process, source),
        0,
        "the source would not close",
    )?;

    // The copy is the file from byte 10, then the whole file again: its first
    // bytes are the data's from byte 10 on.
    let ns = fs::namespace();
    let read = OpenFlags {
        read: true,
        ..OpenFlags::default()
    };
    let copied = ns
        .open(&ns.context(), None, name(COPY), &read, 0)
        .map_err(|_| "the copy sendfile made would not open")?;
    let mut head = [0_u8; 5];
    let expected = DATA.get(10..15).unwrap_or_default();
    if copied.read_at(0, &mut head) != Ok(5) || head.as_slice() != expected {
        return Err("sendfile copied different bytes than the file holds");
    }
    size_is(
        COPY,
        17_990,
        "sendfile's copy is not as long as what it sent",
    )?;
    check_sendfile_takes_the_inputs_linux_does(process, page)?;
    check_sendfile_keeps_what_a_pipe_has_no_room_for(process, page)?;
    Ok(17_990)
}

/// `sendfile` from a socket into a pipe with less room than the socket
/// holds sends what fits and leaves the rest in the socket, in order.
/// Measured on a 7.0 host: 10000 bytes in a Unix stream socket, into a pipe
/// with one page free, send 1808 and the socket still reads the other 8192;
/// into a full non-blocking pipe `EAGAIN`, all 10000 still there. A socket
/// cannot un-read, and `sendfile` read it for as much as it was asked and
/// then lost what a non-blocking pipe did not take. Here the pipe is filled
/// from `/dev/zero` to ten bytes short, and the socket holds fifteen.
fn check_sendfile_keeps_what_a_pipe_has_no_room_for(
    process: &Process,
    page: u64,
) -> Result<(), &'static str> {
    const ROOM: usize = 10;
    const AF_UNIX: u64 = 1;
    const SOCK_STREAM: u64 = 1;
    answers(
        pipe::sys_pipe2(process, page + AT_FDS, O_NONBLOCK),
        0,
        "pipe2 for sendfile from a socket was refused",
    )?;
    let (reader, writer) = pair(process, page)?;
    // Non-blocking, so that a socket emptied by the bug fails the read
    // below rather than waiting on it.
    let nonblocking = u64::from(O_NONBLOCK);
    answers(
        by_number(
            process,
            Syscall::Socketpair,
            [AF_UNIX, SOCK_STREAM | nonblocking, 0, page + AT_FDS, 0, 0],
        ),
        0,
        "socketpair for sendfile into a full pipe was refused",
    )?;
    let (one, other) = pair(process, page)?;
    uaccess::copy_to_user(process.space(), page + AT_BACK, b"/dev/zero\0")
        .map_err(|_| "could not stage /dev/zero's name")?;
    let zero = descriptor(
        fd::sys_openat(process, AT_FDCWD, page + AT_BACK, O_RDONLY, 0),
        "/dev/zero would not open to fill a pipe",
    )?;
    let null = descriptor(
        fd::sys_openat(process, AT_FDCWD, page + AT_DEV_NULL, O_WRONLY, 0),
        "/dev/null would not open to drain a pipe",
    )?;
    let fill = PIPE_CAPACITY - ROOM;
    let held = DATA.len();

    answers(
        pipe::sys_sendfile(process, writer, zero, 0, fill as u64),
        fill,
        "sendfile from /dev/zero did not fill a pipe to what it had room for",
    )?;
    answers(
        file::sys_write(process, other, page + AT_DATA, held as u64),
        held,
        "a write into a socket came back short",
    )?;
    // What was left in the socket is judged before what the call answered,
    // since losing it is what this is about.
    let sent = pipe::sys_sendfile(process, writer, one, 0, held as u64);
    answers(
        file::sys_read(process, one, page + AT_BACK, 64),
        held - ROOM,
        "sendfile from a socket into a pipe lost what the pipe had no room for",
    )?;
    answers(
        sent,
        ROOM,
        "sendfile from a socket into a pipe did not send what the pipe had room for",
    )?;
    if read_back(process, page + AT_BACK, held - ROOM)? != DATA.get(ROOM..).unwrap_or_default() {
        return Err("what sendfile left in a socket is not what the pipe had no room for");
    }
    answers(
        pipe::sys_splice(process, reader, 0, null, 0, fill as u64, 0),
        fill,
        "splice did not drain a filled pipe into /dev/null",
    )?;
    answers(
        file::sys_read(process, reader, page + AT_BACK, 64),
        ROOM,
        "sendfile from a socket did not queue what it sent",
    )?;
    if read_back(process, page + AT_BACK, ROOM)? != DATA.get(..ROOM).unwrap_or_default() {
        return Err("sendfile from a socket queued different bytes than the socket held");
    }
    for fd in [reader, writer, one, other, zero, null] {
        answers(
            fd::sys_close(process, fd),
            0,
            "a descriptor the full-pipe sendfile check used would not close",
        )?;
    }
    Ok(())
}

/// `sendfile` reads only what Linux's does, measured on a 7.0 host: from a
/// pipe it is `EINVAL` into a file, a socket and a pipe, and the pipe keeps
/// its bytes, though a count of zero into a pipe is 0; from a socket it is
/// `EINVAL` into a file and sends into a pipe; from an eventfd it is `EINVAL`
/// into both; and from `/dev/zero` it sends into a file.
fn check_sendfile_takes_the_inputs_linux_does(
    process: &Process,
    page: u64,
) -> Result<(), &'static str> {
    answers(
        pipe::sys_pipe2(process, page + AT_FDS, O_NONBLOCK),
        0,
        "pipe2 for sendfile was refused",
    )?;
    let (reader, writer) = pair(process, page)?;
    answers(
        pipe::sys_pipe2(process, page + AT_FDS, O_NONBLOCK),
        0,
        "a second pipe for sendfile was refused",
    )?;
    let (reader2, writer2) = pair(process, page)?;
    let (one, other) = stream_socket_pair(process, page)?;
    let copy = descriptor(
        fd::sys_openat(process, AT_FDCWD, page + AT_COPY, O_WRONLY, 0),
        "the file to send into would not open",
    )?;
    let events = descriptor(
        crate::syscall::eventfd::sys_eventfd2(process, 1, 0),
        "an eventfd for sendfile was refused",
    )?;
    // The devtmpfs `AT_DEV_ZERO` names is mounted later: the root's own,
    // staged where the reads below land afterwards.
    uaccess::copy_to_user(process.space(), page + AT_BACK, b"/dev/zero\0")
        .map_err(|_| "could not stage /dev/zero's name")?;
    let zero = descriptor(
        fd::sys_openat(process, AT_FDCWD, page + AT_BACK, O_RDONLY, 0),
        "/dev/zero would not open for sendfile",
    )?;

    sendfile_from_a_pipe(process, page, [reader, writer, writer2, one, copy])?;
    answers(
        file::sys_write(process, other, page + AT_DATA, 2),
        2,
        "a write into a socket came back short",
    )?;
    refuses(
        pipe::sys_sendfile(process, copy, one, 0, 2),
        Errno::EINVAL,
        "sendfile from a socket into a file was not EINVAL",
    )?;
    answers(
        pipe::sys_sendfile(process, writer2, one, 0, 2),
        2,
        "sendfile from a socket into a pipe did not send",
    )?;
    answers(
        file::sys_read(process, reader2, page + AT_BACK, 64),
        2,
        "sendfile from a socket into a pipe did not queue what it sent",
    )?;

    for (out, what) in [
        (copy, "sendfile from an eventfd into a file was not EINVAL"),
        (
            writer2,
            "sendfile from an eventfd into a pipe was not EINVAL",
        ),
    ] {
        refuses(
            pipe::sys_sendfile(process, out, events, 0, 8),
            Errno::EINVAL,
            what,
        )?;
    }
    answers(
        pipe::sys_sendfile(process, copy, zero, 0, 5),
        5,
        "sendfile from /dev/zero into a file did not send",
    )?;

    for fd in [
        reader, writer, reader2, writer2, one, other, copy, events, zero,
    ] {
        answers(
            fd::sys_close(process, fd),
            0,
            "a descriptor the sendfile check used would not close",
        )?;
    }
    Ok(())
}

/// The pipe part of [`check_sendfile_takes_the_inputs_linux_does`]: six bytes
/// in the pipe `fds` begins with, refused into the file, the socket and the
/// second pipe, and still all there afterwards.
fn sendfile_from_a_pipe(process: &Process, page: u64, fds: [i32; 5]) -> Result<(), &'static str> {
    let [reader, writer, writer2, one, copy] = fds;
    answers(
        file::sys_write(process, writer, page + AT_DATA, 6),
        6,
        "a write into a pipe came back short",
    )?;
    for (out, what) in [
        (copy, "sendfile from a pipe into a file was not EINVAL"),
        (one, "sendfile from a pipe into a socket was not EINVAL"),
        (writer2, "sendfile from a pipe into a pipe was not EINVAL"),
    ] {
        refuses(
            pipe::sys_sendfile(process, out, reader, 0, 6),
            Errno::EINVAL,
            what,
        )?;
    }
    answers(
        pipe::sys_sendfile(process, writer2, reader, 0, 0),
        0,
        "sendfile of nothing from a pipe into a pipe was not 0",
    )?;
    answers(
        file::sys_read(process, reader, page + AT_BACK, 64),
        6,
        "a refused sendfile took bytes out of the pipe",
    )?;
    if read_back(process, page + AT_BACK, 6)? != DATA.get(..6).unwrap_or_default() {
        return Err("a refused sendfile changed what the pipe held");
    }

    Ok(())
}

/// The descriptors the `splice` checks work with.
#[derive(Clone, Copy)]
struct Spliced {
    reader: i32,
    writer: i32,
    reader2: i32,
    writer2: i32,
    null: i32,
    source: i32,
}

/// `splice` moves bytes out of a pipe into `/dev/null`, which is how GNU grep
/// drains its input; from a file at an offset into a pipe, moving the offset
/// and not the file position; and from one pipe into another. It refuses as
/// Linux does.
fn check_splice_moves_bytes(process: &Process, page: u64) -> Result<u64, &'static str> {
    answers(
        pipe::sys_pipe2(process, page + AT_FDS, O_NONBLOCK),
        0,
        "pipe2 for splice was refused",
    )?;
    let (reader, writer) = pair(process, page)?;
    answers(
        pipe::sys_pipe2(process, page + AT_FDS, O_NONBLOCK),
        0,
        "a second pipe for splice was refused",
    )?;
    let (reader2, writer2) = pair(process, page)?;
    let null = descriptor(
        fd::sys_openat(process, AT_FDCWD, page + AT_DEV_NULL, O_WRONLY, 0),
        "/dev/null would not open for splice",
    )?;
    let source = descriptor(
        fd::sys_openat(process, AT_FDCWD, page + AT_FILE, O_RDONLY, 0),
        "the file to splice would not open",
    )?;
    let fds = Spliced {
        reader,
        writer,
        reader2,
        writer2,
        null,
        source,
    };
    let moved = check_splice_through_pipes(process, page, fds)?;
    check_splice_waits_as_the_pipe_says(process, page, fds)?;
    check_splice_refuses(process, page, fds)?;
    for fd in [source, null, reader, writer, reader2, writer2] {
        answers(
            fd::sys_close(process, fd),
            0,
            "a descriptor splice used would not close",
        )?;
    }
    Ok(moved)
}

/// The moves [`check_splice_moves_bytes`] makes: a pipe into `/dev/null`, a
/// file at an offset into a pipe, and that pipe into the second.
fn check_splice_through_pipes(
    process: &Process,
    page: u64,
    fds: Spliced,
) -> Result<u64, &'static str> {
    let len = DATA.len();
    answers(
        file::sys_write(process, fds.writer, page + AT_DATA, len as u64),
        len,
        "a write into a pipe to splice came back short",
    )?;
    // By number, as grep makes it: an `ENOSYS` from a missing table entry or
    // dispatch line is what sent curl's configure looking for another grep.
    answers(
        by_number(
            process,
            Syscall::Splice,
            [register(fds.reader), 0, register(fds.null), 0, 1 << 16, 0],
        ),
        len,
        "splice from a pipe into /dev/null did not take what the pipe held",
    )?;
    refuses(
        pipe::sys_splice(
            process,
            fds.reader,
            0,
            fds.null,
            0,
            1 << 16,
            SPLICE_F_NONBLOCK,
        ),
        Errno::EAGAIN,
        "splice from an empty pipe under SPLICE_F_NONBLOCK did not answer EAGAIN",
    )?;

    uaccess::copy_to_user(process.space(), page + AT_OFFSET, &10_u64.to_le_bytes())
        .map_err(|_| "could not stage splice's offset")?;
    answers(
        pipe::sys_splice(process, fds.source, page + AT_OFFSET, fds.writer, 0, 5, 0),
        5,
        "splice from a file at an offset into a pipe did not move five bytes",
    )?;
    if read_back(process, page + AT_OFFSET, 8)? != 15_u64.to_le_bytes() {
        return Err("splice did not move its offset past what it moved");
    }
    answers(
        fd::sys_lseek(process, fds.source, 0, SEEK_CUR),
        0,
        "splice with an offset moved the file position",
    )?;
    answers(
        pipe::sys_splice(process, fds.reader, 0, fds.writer2, 0, 64, 0),
        5,
        "splice from one pipe into another did not move what the first held",
    )?;
    answers(
        file::sys_read(process, fds.reader2, page + AT_BACK, 64),
        5,
        "the pipe splice filled did not give back five bytes",
    )?;
    if read_back(process, page + AT_BACK, 5)?.as_slice() != DATA.get(10..15).unwrap_or_default() {
        return Err("splice moved different bytes than the file holds");
    }
    Ok(len as u64 + 10)
}

/// With no flag, whether `splice` waits on a pipe is the pipe's own
/// `O_NONBLOCK`, and never the other descriptor's -- measured on a Linux 7.0
/// host, where a blocking descriptor on the other end changes nothing. Both
/// pipes here are `O_NONBLOCK`; `/dev/null` and the file are not. Out of an
/// empty pipe is `EAGAIN`, and so is into a full one; before, both waited,
/// for bytes and for room nothing would ever bring.
fn check_splice_waits_as_the_pipe_says(
    process: &Process,
    page: u64,
    fds: Spliced,
) -> Result<(), &'static str> {
    refuses(
        pipe::sys_splice(process, fds.reader, 0, fds.null, 0, 1 << 16, 0),
        Errno::EAGAIN,
        "splice from an empty O_NONBLOCK pipe with no flag did not answer EAGAIN",
    )?;
    let mut full = false;
    for _ in 0..1024 {
        if file::sys_write(process, fds.writer2, page, PAGE_SIZE) == Err(Errno::EAGAIN) {
            full = true;
            break;
        }
    }
    if !full {
        return Err("an O_NONBLOCK pipe never filled");
    }
    refuses(
        pipe::sys_splice(process, fds.source, 0, fds.writer2, 0, 5, 0),
        Errno::EAGAIN,
        "splice into a full O_NONBLOCK pipe with no flag did not answer EAGAIN",
    )?;
    for _ in 0..1024 {
        match pipe::sys_splice(process, fds.reader2, 0, fds.null, 0, 1 << 16, 0) {
            Ok(_) => {}
            Err(Errno::EAGAIN) => return Ok(()),
            Err(_) => break,
        }
    }
    Err("a full O_NONBLOCK pipe would not drain into /dev/null")
}

/// What `splice` refuses, as Linux does: nothing to move is 0 before anything
/// is looked at, and two descriptors neither of which is a pipe, an offset
/// for a pipe, two ends of one pipe and an unknown flag are refused.
fn check_splice_refuses(process: &Process, page: u64, fds: Spliced) -> Result<(), &'static str> {
    answers(
        pipe::sys_splice(process, fds.source, 0, fds.writer, 0, 0, 0),
        0,
        "splice of nothing did not answer 0",
    )?;
    refuses(
        pipe::sys_splice(process, fds.source, 0, fds.null, 0, 1, 0),
        Errno::EINVAL,
        "splice between two descriptors neither of which is a pipe was not EINVAL",
    )?;
    refuses(
        pipe::sys_splice(process, fds.reader, page + AT_OFFSET, fds.null, 0, 1, 0),
        Errno::ESPIPE,
        "splice took an offset for a pipe",
    )?;
    refuses(
        pipe::sys_splice(process, fds.reader, 0, fds.writer, 0, 1, 0),
        Errno::EINVAL,
        "splice joined the two ends of one pipe",
    )?;
    refuses(
        pipe::sys_splice(process, fds.reader, 0, fds.null, 0, 1, 0x10),
        Errno::EINVAL,
        "splice took a flag it does not know",
    )
}

/// `splice` out of a pipe leaves in the pipe what the output refused, as
/// Linux does -- measured on a 7.0 host: into a full non-blocking socket it
/// is `EAGAIN` and the pipe still holds all six bytes. Then into the same
/// socket with room for two: what the socket did not take is still at the
/// pipe's front, in order. Linux would take all six there: it charges a
/// socket by its buffers rather than its bytes, and into a full socket
/// whose peer had read 20000 bytes it spliced all 60006 a pipe held. So
/// what is checked is that the socket took some and not all, so that the
/// path ran, and that no byte was lost or moved.
fn check_splice_keeps_what_the_output_refused(
    process: &Process,
    page: u64,
) -> Result<(), &'static str> {
    // Non-blocking, so a pipe that lost its bytes answers the read below
    // with EAGAIN rather than hanging the boot.
    answers(
        pipe::sys_pipe2(process, page + AT_FDS, O_NONBLOCK),
        0,
        "pipe2 was refused",
    )?;
    let (reader, writer) = pair(process, page)?;
    let (one, other) = stream_socket_pair(process, page)?;
    uaccess::copy_to_user(process.space(), page + AT_OFFSET, &1_u32.to_le_bytes())
        .map_err(|_| "could not stage FIONBIO's argument")?;
    answers(
        fd::sys_ioctl(process, one, FIONBIO, page + AT_OFFSET),
        0,
        "FIONBIO on a socket was refused",
    )?;
    let mut full = false;
    for _ in 0..1024 {
        if file::sys_write(process, one, page, PAGE_SIZE) == Err(Errno::EAGAIN) {
            full = true;
            break;
        }
    }
    if !full {
        return Err("a non-blocking socket never filled");
    }
    answers(
        file::sys_write(process, writer, page + AT_DATA, 6),
        6,
        "a write into a pipe came back short",
    )?;
    refuses(
        pipe::sys_splice(process, reader, 0, one, 0, 6, 0),
        Errno::EAGAIN,
        "splice from a pipe into a full non-blocking socket was not EAGAIN",
    )?;
    answers(
        file::sys_read(process, reader, page + AT_BACK, 64),
        6,
        "splice into a socket that took nothing lost bytes from the pipe",
    )?;
    if read_back(process, page + AT_BACK, 6)? != DATA.get(..6).unwrap_or_default() {
        return Err("splice into a socket that took nothing changed what the pipe held");
    }
    answers(
        file::sys_write(process, writer, page + AT_DATA, 6),
        6,
        "a write into a pipe came back short",
    )?;
    answers(
        file::sys_read(process, other, page + AT_BACK, 2),
        2,
        "a full socket's peer would not read",
    )?;
    let taken = pipe::sys_splice(process, reader, 0, one, 0, 6, 0)
        .map_err(|_| "splice from a pipe into a socket with room for part was refused")?;
    if taken == 0 || taken >= 6 {
        return Err("splice into a socket with room for part did not take part");
    }
    answers(
        file::sys_read(process, reader, page + AT_BACK, 64),
        6 - taken,
        "splice into a socket that took part lost the rest from the pipe",
    )?;
    if read_back(process, page + AT_BACK, 6 - taken)? != DATA.get(taken..6).unwrap_or_default() {
        return Err("splice into a socket that took part left the rest out of order");
    }
    for fd in [reader, writer, one, other] {
        answers(
            fd::sys_close(process, fd),
            0,
            "a descriptor the splice check used would not close",
        )?;
    }
    Ok(())
}

/// `copy_file_range` copies a file as `sendfile` does, with and without an
/// offset, answers 0 at the input's end, and refuses flags, a pipe and a
/// descriptor open the wrong way.
fn check_copy_file_range_copies_a_file(process: &Process, page: u64) -> Result<u64, &'static str> {
    let source = descriptor(
        fd::sys_openat(process, AT_FDCWD, page + AT_FILE, O_RDONLY, 0),
        "the file to copy_file_range would not open",
    )?;
    let copy = descriptor(
        fd::sys_openat(
            process,
            AT_FDCWD,
            page + AT_COPY,
            O_WRONLY | O_CREAT | O_TRUNC,
            0o644,
        ),
        "a file to copy_file_range into could not be created",
    )?;
    uaccess::copy_to_user(process.space(), page + AT_OFFSET, &10_u64.to_le_bytes())
        .map_err(|_| "could not stage copy_file_range's offset")?;
    let at_offset = [
        register(source),
        page + AT_OFFSET,
        register(copy),
        0,
        1 << 20,
        0,
    ];
    answers(
        by_number(process, Syscall::CopyFileRange, at_offset),
        8990,
        "copy_file_range from an offset did not copy the rest of the file",
    )?;
    if read_back(process, page + AT_OFFSET, 8)? != 9000_u64.to_le_bytes() {
        return Err("copy_file_range did not move its offset past what it copied");
    }
    answers(
        fd::sys_lseek(process, source, 0, SEEK_CUR),
        0,
        "copy_file_range with an offset moved the file position",
    )?;
    answers(
        fd::sys_lseek(process, copy, 0, SEEK_CUR),
        8990,
        "copy_file_range without an output offset did not move the output's position",
    )?;
    answers(
        pipe::sys_copy_file_range(process, source, 0, copy, 0, 1 << 20, 0),
        9000,
        "copy_file_range from the file position did not copy the whole file",
    )?;
    answers(
        pipe::sys_copy_file_range(process, source, 0, copy, 0, 1, 0),
        0,
        "copy_file_range at the input's end did not answer 0",
    )?;
    check_copy_file_range_refuses(process, page, source, copy)?;
    answers(fd::sys_close(process, copy), 0, "the copy would not close")?;
    answers(
        fd::sys_close(process, source),
        0,
        "the copied file would not close",
    )?;
    copied_from_byte_ten()?;
    size_is(
        COPY,
        17_990,
        "copy_file_range's copy is not as long as what it copied",
    )?;
    Ok(17_990)
}

/// What `copy_file_range` refuses: any flag, a pipe, and an input open only
/// for writing.
fn check_copy_file_range_refuses(
    process: &Process,
    page: u64,
    source: i32,
    copy: i32,
) -> Result<(), &'static str> {
    refuses(
        pipe::sys_copy_file_range(process, source, 0, copy, 0, 1, 1),
        Errno::EINVAL,
        "copy_file_range took a flag",
    )?;
    answers(
        pipe::sys_pipe2(process, page + AT_FDS, O_NONBLOCK),
        0,
        "pipe2 for copy_file_range was refused",
    )?;
    let (reader, writer) = pair(process, page)?;
    let from_a_pipe = pipe::sys_copy_file_range(process, reader, 0, copy, 0, 1, 0);
    for fd in [reader, writer] {
        answers(fd::sys_close(process, fd), 0, "a pipe would not close")?;
    }
    refuses(
        from_a_pipe,
        Errno::EINVAL,
        "copy_file_range copied out of a pipe",
    )?;
    refuses(
        pipe::sys_copy_file_range(process, copy, 0, source, 0, 1, 0),
        Errno::EBADF,
        "copy_file_range read from a descriptor open only for writing",
    )
}

/// The copy `copy_file_range` made at [`COPY`] starts with the data's bytes
/// from byte 10 on, as a copy of the file from an offset of 10 must.
fn copied_from_byte_ten() -> Result<(), &'static str> {
    let ns = fs::namespace();
    let read = OpenFlags {
        read: true,
        ..OpenFlags::default()
    };
    let copied = ns
        .open(&ns.context(), None, name(COPY), &read, 0)
        .map_err(|_| "the copy copy_file_range made would not open")?;
    let mut head = [0_u8; 5];
    let expected = DATA.get(10..15).unwrap_or_default();
    if copied.read_at(0, &mut head) != Ok(5) || head.as_slice() != expected {
        return Err("copy_file_range copied different bytes than the file holds");
    }
    Ok(())
}

/// Make `call` by its number, as a program on this architecture would.
fn by_number(process: &Process, call: Syscall, args: [u64; 6]) -> Result<usize, Errno> {
    syscall_check::call_by_number(process, call, args)
}

/// A descriptor, as a register carries it.
fn register(fd: i32) -> u64 {
    u64::from(fd.unsigned_abs())
}

/// The file the truncate check just wrote is dated within a minute of
/// `CLOCK_REALTIME`, whether firmware set that clock or it counts from 1970:
/// a program comparing the two, as automake's "newly created file is older
/// than distributed files" does, must find a new file new.
fn check_a_new_file_is_dated_now() -> Result<(), &'static str> {
    let ns = fs::namespace();
    let written = ns
        .resolve(&ns.context(), None, name(FILE), true)
        .and_then(|at| ns.stat(&at))
        .map_err(|_| "the file the truncate check wrote would not stat")?
        .metadata
        .mtime;
    let now = crate::syscall::time::realtime_nanos() / 1_000_000_000;
    let now = i64::try_from(now).unwrap_or(i64::MAX);
    if written.tv_sec.abs_diff(now) > 60 {
        return Err("a file written now is not dated by CLOCK_REALTIME");
    }
    Ok(())
}

/// `/dev/shm` is a directory a program can make a file in, and `/proc/mounts`
/// says what it is.
///
/// devfs cannot create a name and has no storage, so the directory alone would
/// be useless: what makes `/dev/shm` work is the tmpfs [`fs::init`] mounts over
/// it. This is the check that the mount is there and is the thing walked into
/// — a file created, written, read back and removed — rather than the empty
/// devfs node underneath, which would take the `create` and answer `EROFS`.
///
/// ferrousli's named semaphores are the first caller: `sem_open` makes a
/// 32-byte file here and maps it shared. Chromium's shared memory is the other,
/// when its `memfd_create` path is not taken.
fn check_dev_shm_holds_a_file(process: &Process, page: u64) -> Result<(), &'static str> {
    let made = descriptor(
        fd::sys_openat(
            process,
            AT_FDCWD,
            page + AT_SHM_FILE,
            O_RDWR | O_CREAT | O_TRUNC,
            0o600,
        ),
        "a file could not be created in /dev/shm",
    )?;
    let len = DATA.len();
    answers(
        file::sys_write(process, made, page + AT_DATA, len as u64),
        len,
        "a write to a file in /dev/shm came back short",
    )?;
    answers(
        file::sys_pread64(process, made, page + AT_BACK, len as u64, 0),
        len,
        "a file in /dev/shm did not read back what was written to it",
    )?;
    let mut back = vec![0_u8; len];
    uaccess::copy_from_user(process.space(), page + AT_BACK, &mut back)
        .map_err(|_| "what /dev/shm read back could not be copied out")?;
    if back != DATA {
        return Err("a file in /dev/shm read back bytes other than the ones written");
    }
    size_is(
        SHM_FILE,
        len as u64,
        "a file in /dev/shm has the wrong size",
    )?;
    answers(
        fd::sys_close(process, made),
        0,
        "a file in /dev/shm would not close",
    )?;

    let listing = read_mounts(process, page)?;
    let line = &b"tmpfs /dev/shm tmpfs rw 0 0\n"[..];
    if !listing.windows(line.len()).any(|window| window == line) {
        return Err("/proc/mounts does not list a tmpfs on /dev/shm");
    }

    let ns = fs::namespace();
    ns.unlink(&ns.context(), None, name(SHM_FILE))
        .map_err(|_| "a file in /dev/shm would not unlink")
}

/// `mount -t proc` and `mount -t devtmpfs`, by number, as an init script makes
/// them: each on a directory under /tmp, with the flags scripts pass and, for
/// proc, an option it does not read. Through the second procfs the check
/// process finds itself, and `self` answers as `/proc/self` does; from the
/// second devtmpfs `zero` reads zeros; `/proc/mounts` lists both; both unmount;
/// and `sysfs` mounts and unmounts, while `devpts`, which does not exist, is
/// `ENODEV`.
fn check_proc_and_devtmpfs_mount(process: &Process, page: u64) -> Result<(), &'static str> {
    for dir in [AT_PROC_DIR, AT_DEV_DIR] {
        answers(
            by_number(process, Syscall::Mkdirat, [CWD, page + dir, 0o755, 0, 0, 0]),
            0,
            "a directory to mount on could not be made under /tmp",
        )?;
    }
    check_a_second_procfs(process, page)?;
    check_sendfile_takes_procfs_files_as_linux_does(process, page)?;
    check_a_second_devtmpfs(process, page)?;
    check_both_are_listed_and_unmount(process, page)
}

/// `sendfile` sends from the procfs files Linux's sends from, and refuses
/// the rest and `/dev/null` for having no `splice_read`, as `splice` into a
/// pipe does; measured on a 7.0 host (`syscall::pipe`'s `splices_out`).
/// Through the second procfs: `version` and the check process's `mounts`
/// send five bytes into a pipe and into a socket; its `status`, `net/dev`
/// and the root's `/dev/null` are `EINVAL` into both and from `splice`, and
/// 0 for a count of zero. These used to be sent from, `/dev/null` as the
/// empty stream it reads as.
fn check_sendfile_takes_procfs_files_as_linux_does(
    process: &Process,
    page: u64,
) -> Result<(), &'static str> {
    answers(
        pipe::sys_pipe2(process, page + AT_FDS, O_NONBLOCK),
        0,
        "pipe2 for sendfile from procfs was refused",
    )?;
    let (reader, writer) = pair(process, page)?;
    let (one, other) = stream_socket_pair(process, page)?;
    let pid = process.pid();
    let inputs = [
        (String::from("/tmp/stage8-proc/version\0"), true),
        (alloc::format!("/tmp/stage8-proc/{pid}/mounts\0"), true),
        (alloc::format!("/tmp/stage8-proc/{pid}/status\0"), false),
        (String::from("/tmp/stage8-proc/net/dev\0"), false),
        (String::from("/dev/null\0"), false),
    ];
    let mut outcome = Ok(());
    for (path, sends) in &inputs {
        outcome = sendfile_from_a_generated_file(
            process,
            page,
            path,
            *sends,
            [reader, writer, one, other],
        );
        if outcome.is_err() {
            break;
        }
    }
    for fd in [reader, writer, one, other] {
        let _ = fd::sys_close(process, fd);
    }
    outcome
}

/// One input of [`check_sendfile_takes_procfs_files_as_linux_does`]: `path`
/// sends five bytes into the pipe and the socket `fds` holds, or is refused
/// into both.
fn sendfile_from_a_generated_file(
    process: &Process,
    page: u64,
    path: &str,
    sends: bool,
    fds: [i32; 4],
) -> Result<(), &'static str> {
    let [reader, writer, one, other] = fds;
    uaccess::copy_to_user(process.space(), page + AT_STAT, path.as_bytes())
        .map_err(|_| "could not stage a file to sendfile from")?;
    let input = descriptor(
        fd::sys_openat(process, AT_FDCWD, page + AT_STAT, O_RDONLY, 0),
        "a procfs file or /dev/null would not open for sendfile",
    )?;
    let judged = if sends {
        answers(
            pipe::sys_sendfile(process, writer, input, 0, 5),
            5,
            "sendfile from /proc/version or /proc/<pid>/mounts into a pipe did not send",
        )
        .and_then(|()| {
            answers(
                file::sys_read(process, reader, page + AT_LISTING, 64),
                5,
                "sendfile from procfs into a pipe did not queue what it sent",
            )
        })
        .and_then(|()| {
            answers(
                pipe::sys_sendfile(process, one, input, 0, 5),
                5,
                "sendfile from /proc/version or /proc/<pid>/mounts into a socket did not send",
            )
        })
        .and_then(|()| {
            answers(
                file::sys_read(process, other, page + AT_LISTING, 64),
                5,
                "sendfile from procfs into a socket did not queue what it sent",
            )
        })
    } else {
        refuses(
            pipe::sys_sendfile(process, writer, input, 0, 5),
            Errno::EINVAL,
            "sendfile from /proc/<pid>/status, /proc/net/dev or /dev/null into a pipe was not EINVAL",
        )
        .and_then(|()| {
            refuses(
                pipe::sys_sendfile(process, one, input, 0, 5),
                Errno::EINVAL,
                "sendfile from /proc/<pid>/status, /proc/net/dev or /dev/null into a socket was not \
                 EINVAL",
            )
        })
        .and_then(|()| {
            refuses(
                pipe::sys_splice(process, input, 0, writer, 0, 5, 0),
                Errno::EINVAL,
                "splice from /proc/<pid>/status, /proc/net/dev or /dev/null into a pipe was not \
                 EINVAL",
            )
        })
        .and_then(|()| {
            answers(
                pipe::sys_sendfile(process, writer, input, 0, 0),
                0,
                "sendfile of nothing from a file with no splice_read was not 0",
            )
        })
    };
    let _ = fd::sys_close(process, input);
    judged
}

/// `mount -t proc` with the flags and an option: `self` in it answers as
/// `/proc/self` does, and the check process is found in it by its pid.
fn check_a_second_procfs(process: &Process, page: u64) -> Result<(), &'static str> {
    answers(
        by_number(
            process,
            Syscall::Mount,
            [
                page + AT_PROC_TYPE,
                page + AT_PROC_DIR,
                page + AT_PROC_TYPE,
                u64::from(PROC_FLAGS),
                page + AT_OPTIONS,
                0,
            ],
        ),
        0,
        "mount -t proc with nosuid, nodev, noexec, relatime and hidepid= was refused",
    )?;
    // `self` is the reader's pid, and `ENOENT` to a task with no process,
    // which the boot task is. What matters is that a second procfs answers
    // exactly as the first.
    let through_mount = by_number(
        process,
        Syscall::Readlinkat,
        [CWD, page + AT_PROC_SELF, page + AT_LINK, 32, 0, 0],
    );
    let through_proc = by_number(
        process,
        Syscall::Readlinkat,
        [CWD, page + AT_SELF, page + AT_LINK_PROC, 32, 0, 0],
    );
    let same = match (through_mount, through_proc) {
        (Ok(a), Ok(b)) => {
            a == b
                && read_back(process, page + AT_LINK, a)?
                    == read_back(process, page + AT_LINK_PROC, b)?
        }
        (Err(a), Err(b)) => a == b,
        _ => false,
    };
    if !same {
        return Err("self in a second procfs does not answer as /proc/self does");
    }
    // The same processes: the check's own, found by its pid.
    let pid = process.pid();
    let stat = alloc::format!("/tmp/stage8-proc/{pid}/stat\0");
    uaccess::copy_to_user(process.space(), page + AT_STAT, stat.as_bytes())
        .map_err(|_| "could not stage a path into a second procfs")?;
    let own = descriptor(
        by_number(
            process,
            Syscall::Openat,
            [CWD, page + AT_STAT, u64::from(O_RDONLY), 0, 0, 0],
        ),
        "the check process's stat would not open through a second procfs",
    )?;
    let read = by_number(
        process,
        Syscall::Read,
        [register(own), page + AT_LINK, 32, 0, 0, 0],
    )
    .map_err(|_| "the check process's stat would not read through a second procfs")?;
    answers(
        by_number(process, Syscall::Close, [register(own), 0, 0, 0, 0, 0]),
        0,
        "a file in a second procfs would not close",
    )?;
    let prefix = alloc::format!("{pid} (");
    if !read_back(process, page + AT_LINK, read)?.starts_with(prefix.as_bytes()) {
        return Err("a second procfs does not show the check process as /proc does");
    }
    Ok(())
}

/// `mount -t devtmpfs` with the flags: `zero` in it reads zeros.
fn check_a_second_devtmpfs(process: &Process, page: u64) -> Result<(), &'static str> {
    answers(
        by_number(
            process,
            Syscall::Mount,
            [
                page + AT_DEVTMPFS_TYPE,
                page + AT_DEV_DIR,
                page + AT_DEVTMPFS_TYPE,
                u64::from(DEVTMPFS_FLAGS),
                0,
                0,
            ],
        ),
        0,
        "mount -t devtmpfs with nosuid and relatime was refused",
    )?;
    uaccess::copy_to_user(process.space(), page + AT_ZEROS, &[0xA5; 16])
        .map_err(|_| "could not stage a buffer for zeros")?;
    let zero = descriptor(
        by_number(
            process,
            Syscall::Openat,
            [CWD, page + AT_DEV_ZERO, u64::from(O_RDONLY), 0, 0, 0],
        ),
        "zero in a second devtmpfs would not open",
    )?;
    answers(
        by_number(
            process,
            Syscall::Read,
            [register(zero), page + AT_ZEROS, 16, 0, 0, 0],
        ),
        16,
        "zero in a second devtmpfs did not fill a read",
    )?;
    answers(
        by_number(process, Syscall::Close, [register(zero), 0, 0, 0, 0, 0]),
        0,
        "zero in a second devtmpfs would not close",
    )?;
    if read_back(process, page + AT_ZEROS, 16)? != [0; 16] {
        return Err("zero in a second devtmpfs read something other than zeros");
    }
    Ok(())
}

/// `/proc/mounts` lists both mounts as Linux prints them; both unmount and
/// their directories can be removed; a sysfs mounts where the procfs was and
/// unmounts; and `devpts` is `ENODEV`.
fn check_both_are_listed_and_unmount(process: &Process, page: u64) -> Result<(), &'static str> {
    let listing = read_mounts(process, page)?;
    for line in [
        &b"proc /tmp/stage8-proc proc rw,nosuid,nodev,noexec,relatime 0 0\n"[..],
        b"devtmpfs /tmp/stage8-dev devtmpfs rw,nosuid,relatime 0 0\n",
    ] {
        if !listing.windows(line.len()).any(|window| window == line) {
            return Err("/proc/mounts does not list a proc or devtmpfs mount as Linux prints it");
        }
    }

    for dir in [AT_PROC_DIR, AT_DEV_DIR] {
        answers(
            by_number(process, Syscall::Umount2, [page + dir, 0, 0, 0, 0, 0]),
            0,
            "umount2 of a proc or devtmpfs mount was refused",
        )?;
    }
    refuses(
        by_number(
            process,
            Syscall::Umount2,
            [page + AT_PROC_DIR, 0, 0, 0, 0, 0],
        ),
        Errno::EINVAL,
        "umount2 of a directory nothing is mounted on any more was not EINVAL",
    )?;
    answers(
        by_number(
            process,
            Syscall::Mount,
            [
                page + AT_SYSFS_TYPE,
                page + AT_PROC_DIR,
                page + AT_SYSFS_TYPE,
                0,
                0,
                0,
            ],
        ),
        0,
        "mount -t sysfs was refused",
    )?;
    answers(
        by_number(
            process,
            Syscall::Umount2,
            [page + AT_PROC_DIR, 0, 0, 0, 0, 0],
        ),
        0,
        "umount2 of a sysfs mount was refused",
    )?;
    refuses(
        by_number(
            process,
            Syscall::Mount,
            [
                page + AT_DEVPTS_TYPE,
                page + AT_PROC_DIR,
                page + AT_DEVPTS_TYPE,
                0,
                0,
                0,
            ],
        ),
        Errno::ENODEV,
        "mount -t devpts, a type that does not exist, was not ENODEV",
    )?;
    for dir in [AT_PROC_DIR, AT_DEV_DIR] {
        answers(
            by_number(
                process,
                Syscall::Unlinkat,
                [CWD, page + dir, u64::from(AT_REMOVEDIR), 0, 0, 0],
            ),
            0,
            "a directory unmounted from could not be removed",
        )?;
    }
    Ok(())
}

/// The whole of `/proc/mounts`, read by number in pieces.
fn read_mounts(process: &Process, page: u64) -> Result<Vec<u8>, &'static str> {
    let mounts = descriptor(
        by_number(
            process,
            Syscall::Openat,
            [CWD, page + AT_MOUNTS, u64::from(O_RDONLY), 0, 0, 0],
        ),
        "/proc/mounts would not open",
    )?;
    let room = PAGE_SIZE - AT_LISTING;
    let mut listing = Vec::new();
    let outcome = loop {
        match by_number(
            process,
            Syscall::Read,
            [register(mounts), page + AT_LISTING, room, 0, 0, 0],
        ) {
            Ok(0) => break Ok(()),
            Ok(count) if listing.len() < 4 * PAGE_SIZE as usize => {
                listing.extend_from_slice(&read_back(process, page + AT_LISTING, count)?);
            }
            Ok(_) => break Err("/proc/mounts did not end"),
            Err(_) => break Err("/proc/mounts would not read"),
        }
    };
    let _ = by_number(process, Syscall::Close, [register(mounts), 0, 0, 0, 0, 0]);
    outcome.map(|()| listing)
}
