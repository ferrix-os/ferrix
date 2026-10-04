//! The self-check of programs mapped from their files.
//!
//! `execve` maps a program's pages from its file rather than reading the
//! file whole (`syscall/load.rs`), which is what lets it run one larger than
//! the 64 MiB `fs::read_file` will read. This proves that on the machine, in
//! two parts.
//!
//! **Paged in on demand.** A program of 72 MiB is loaded and runs to its exit
//! status, and loading it read three of its 18,437 pages. The file is a real
//! one's shape -- a text segment, a data segment with `.bss`, and a 72 MiB
//! writable segment whose contents are in the file -- served by a page source
//! that counts what it is asked for, the way btrfs serves a file from disk
//! into its page cache. So the counts are reads a disk would have made. Then,
//! through the loaded program's own address space:
//!
//! * a page 40 MiB into the large segment reads the file's byte there, and
//!   the read fills one run of pages, not the file;
//! * a write 50 MiB in reads back in the program and leaves the file's page
//!   as it was: a writable segment's pages are the program's private copies;
//! * the bytes past the segment's file contents, on the page the contents end
//!   on, are zero although the file's are not, and so is the `.bss` after it;
//! * the region is a mapping of the file, at the segment's offset in it.
//!
//! **Run again through `/proc/self/exe`.** The same program as a sparse 72
//! MiB file under `/tmp` is opened as `execve` opens a path -- which refused
//! it with `EFBIG` until the loader mapped files -- and loaded. A fork of that
//! process, as Chrome forks each child, runs `execve("/proc/self/exe")` after
//! the file's name has been removed, and the program runs again to its status.
//! That needs the child to know what its parent was started from, and
//! `/proc/<pid>/exe` to lead to the file rather than to its old name.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicU64, Ordering};

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_elf::{Class, PF_R, PF_W};
use ferrix_vfs::tmpfs::{PageSource, Pages, Storage};
use ferrix_vfs::{Errno, FileType, Inode, Metadata, OpenFile, OpenFlags};

use crate::arch;
use crate::fs::pages::VmoStorage;
use crate::sync::SpinLock;
use crate::syscall::attributes;
use crate::syscall::exec::{self, Executable, ExecveError};
use crate::syscall::image::{self, Extra};
use crate::syscall::load::Source;
use crate::syscall::process::{self, Process};
use crate::syscall::program::ProgramFile;
use crate::syscall::thread::Thread;
use crate::syscall::uaccess;
use crate::user::space::{Access, AddressSpace};

/// The name the counted program goes by: an empty file under `/tmp`, whose
/// contents the check supplies through the open file.
const PATH: &[u8] = b"/tmp/exec-check-large";

/// The name the sparse program is made under, and removed from before the
/// fork runs it again.
const SELF_PATH: &[u8] = b"/tmp/exec-check-self";

/// What `/proc/<pid>/exe` reads as once [`SELF_PATH`] is gone.
const SELF_DELETED: &[u8] = b"/tmp/exec-check-self (deleted)";

/// The path the fork runs.
const PROC_SELF_EXE: &[u8] = b"/proc/self/exe\0";

/// Where the large segment's contents start in the file: past the two pages
/// the headers, text and data take, and a gap.
const LARGE_OFFSET: u64 = 4 * PAGE_SIZE;

/// Where the large segment is linked: 256 MiB, clear of the image's text and
/// data at 4 MiB and inside the user half of every architecture.
const LARGE_VADDR: u64 = 0x1000_0000;

/// Bytes of the large segment in the file: 72 MiB and a little, so that the
/// file is past `fs::read_file`'s 64 MiB and its contents end part-way into
/// a page.
const LARGE_FILESZ: u64 = 72 * 1024 * 1024 + 100;

/// Bytes of it in memory: three pages of `.bss` past the contents.
const LARGE_MEMSZ: u64 = LARGE_FILESZ + 3 * PAGE_SIZE;

/// The file's length.
const FILE_LEN: u64 = LARGE_OFFSET + LARGE_FILESZ;

/// Where the fork's `execve` arguments are staged: a page of the large
/// segment, which the fork writes into its own private copy.
const STAGED: u64 = LARGE_VADDR + 60 * 1024 * 1024;

/// The most pages loading the program may read: the headers' page, the data
/// segment's, and the partial page the large segment's contents end on,
/// with room for the text segment's.
const LOAD_MOST: u64 = 4;

/// The most one fault may fill: a run of `fs::pages::MAX_FILL_RUN`.
const FAULT_MOST: u64 = 32;

/// How long a program run here is given to end.
const PATIENCE_NANOS: u64 = 30_000_000_000;

/// What the fork's task exits with when its `execve` returned.
const EXEC_FAILED: i32 = 99;

/// Where the fork's task finds its staged path and argument vector.
static STAGED_CALL: SpinLock<Option<(u64, u64)>> = SpinLock::new(None);

/// What the fork's `execve` was refused with, if it was.
static REFUSED: SpinLock<Option<i32>> = SpinLock::new(None);

/// `no_new_privs` and `dumpable` as the fork's `execve` left them, read by
/// its task before the program runs.
static AFTER_EXEC: SpinLock<Option<(bool, bool)>> = SpinLock::new(None);

/// What the check measured, for the boot log.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Report {
    /// The program file's length.
    pub(crate) bytes: u64,
    /// Pages of it read to load it.
    pub(crate) loaded: u64,
    /// Pages of it read by the end, touches and the run included.
    pub(crate) read: u64,
    /// Pages the file has.
    pub(crate) pages: u64,
    /// What the program exited with.
    pub(crate) status: i32,
    /// What the fork that ran `/proc/self/exe` exited with.
    pub(crate) self_status: i32,
}

/// Run it. `Ok(None)` on an architecture with no user-mode program to run.
pub(crate) fn run() -> Result<Option<Report>, &'static str> {
    if arch::USER_TEST_PROGRAM.is_empty() {
        return Ok(None);
    }
    let class = if size_of::<usize>() == 8 {
        Class::Elf64
    } else {
        Class::Elf32
    };
    let head = image::build_with_segment(
        class,
        arch::ARCH.elf_machine(),
        arch::USER_TEST_PROGRAM,
        &Extra {
            flags: PF_R | PF_W,
            offset: LARGE_OFFSET,
            vaddr: LARGE_VADDR,
            filesz: LARGE_FILESZ,
            memsz: LARGE_MEMSZ,
        },
    );
    let mut report = check_paging(&head)?;
    report.self_status = check_proc_self_exe(&head)?;
    Ok(Some(report))
}

/// The first part: the program in a counted file, loaded, touched and run.
fn check_paging(head: &[u8]) -> Result<Report, &'static str> {
    let source = Arc::new(ProgramSource {
        head: head.to_vec(),
        pages: AtomicU64::new(0),
    });
    let store: Arc<dyn Pages> = Arc::from(
        VmoStorage
            .allocate_with(Arc::clone(&source) as Arc<dyn PageSource>)
            .map_err(|_| "a store over the large program's page source was refused")?,
    );
    store.resize(FILE_LEN);
    let inode = Arc::new(ProgramInode {
        store: Arc::clone(&store),
    });

    let ns = crate::fs::namespace();
    let ctx = ns.context();
    let placeholder = ns
        .open(&ctx, None, PATH, &create(), 0o755)
        .map_err(|_| "could not make the large program's name under /tmp")?;
    let outcome = load_and_run(&placeholder, inode, store.as_ref(), &source);
    drop(placeholder);
    let _ = ns.unlink(&ctx, None, PATH);
    outcome
}

/// Open for reading and writing, made if missing and emptied if not.
fn create() -> OpenFlags {
    OpenFlags {
        read: true,
        write: true,
        create: true,
        truncate: true,
        ..OpenFlags::default()
    }
}

/// The body of [`check_paging`]: the file is `placeholder`'s name with
/// `inode`'s contents, which `store` holds as `source` fills it.
fn load_and_run(
    placeholder: &OpenFile,
    inode: Arc<ProgramInode>,
    store: &dyn Pages,
    source: &ProgramSource,
) -> Result<Report, &'static str> {
    let read = || source.pages.load(Ordering::Relaxed);
    let flags = OpenFlags {
        read: true,
        ..OpenFlags::default()
    };
    let file = OpenFile::new(placeholder.location().clone(), &flags)
        .and_then(|file| file.with_io(inode))
        .map_err(|_| "could not open the large program's name for reading")?;
    let program =
        ProgramFile::open(file).map_err(|_| "the large program's headers could not be read")?;
    if program.len() != FILE_LEN || program.object().is_none() {
        return Err("the large program's file was opened without its length or its object");
    }

    let process = exec::load_executable(
        Executable {
            image: Source::File(&program),
            exe: PATH,
            exec_fn: PATH,
            set_ids: crate::fs::SetIds::NONE,
            interpreter: None,
        },
        &[PATH],
        &[],
        [0x5a; ferrix_ustack::RANDOM_BYTES],
    )
    .map_err(|_| "a 72 MiB program in a file could not be loaded")?;
    let loaded = read();
    if loaded > LOAD_MOST {
        crate::console::println!(
            "  exec     loading read {loaded} of the program's {} pages",
            FILE_LEN.div_ceil(PAGE_SIZE)
        );
        return Err("loading a 72 MiB program read pages of it that nothing had touched");
    }

    let space = process.space();
    check_the_segment_is_the_files(space)?;
    check_touches(space, store, source)?;

    let status = run_to_its_end(&process)?;
    Ok(Report {
        bytes: FILE_LEN,
        loaded,
        read: read(),
        pages: FILE_LEN.div_ceil(PAGE_SIZE),
        status,
        self_status: 0,
    })
}

/// Start `process` and wait for it to end, requiring the status its code
/// exits with.
///
/// Ended is not gone. `wait_for_exit` returns at the release, while the
/// program's task still holds its process, and through it the address space
/// and the file's pages, until it has left and been reaped. The check waits
/// for that too ([`crate::sched::wait_until_gone`]), so the space goes when
/// the check lets go of the process, here. Otherwise it went whenever the
/// task was reaped, and on a loaded host that was inside the next check's
/// frame window: the signalfd check (FX-0884) once saw 137 frames released
/// and 9 user page tables given back that it had never taken.
fn run_to_its_end(process: &Arc<Process>) -> Result<i32, &'static str> {
    let task = process::start(process).map_err(|_| "a program the check loaded would not start")?;
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let status = process
        .wait_for_exit(deadline)
        .ok_or("a program the check loaded never ended")?;
    crate::sched::wait_until_gone(&task, crate::sched::REAPER_PATIENCE_NANOS)?;
    drop(task);
    if status != arch::USER_TEST_STATUS {
        return Err("a program the check loaded did not exit with its code's status");
    }
    Ok(status)
}

/// The large segment's file pages are one private mapping of the file, from
/// the segment's offset in it, writable, and named by the file. A writable
/// segment's last file page is mapped whole, as Linux's `elf_map` maps it,
/// and its `.bss` tail zeroed in the program's private copy
/// (`syscall/load.rs`, `file_runs` and `zero_tails`), so the mapping ends on
/// the page boundary after the segment's file contents.
fn check_the_segment_is_the_files(space: &AddressSpace) -> Result<(), &'static str> {
    let end = (LARGE_VADDR + LARGE_FILESZ).div_ceil(PAGE_SIZE) * PAGE_SIZE;
    let region = space
        .regions()
        .map_err(|_| "no memory to list the regions")?
        .into_iter()
        .find(|region| region.start == LARGE_VADDR)
        .ok_or("nothing is mapped where the large segment starts")?;
    let Some((id, offset)) = region.file else {
        return Err("the large segment was copied into anonymous memory, not mapped from its file");
    };
    if offset != LARGE_OFFSET || region.end != end {
        return Err(
            "the large segment's mapping is not its file pages, from its offset in the file",
        );
    }
    if !region.flags.write || region.flags.shared || region.flags.execute {
        return Err("the large segment is not mapped privately, readable and writable");
    }
    if space
        .mapped_file(id)
        .and_then(|file| file.downcast::<OpenFile>().ok())
        .is_none()
    {
        return Err("the large segment's mapping does not keep the file it maps");
    }
    Ok(())
}

/// Touch the loaded program's large segment through its own address space.
fn check_touches(
    space: &AddressSpace,
    store: &dyn Pages,
    source: &ProgramSource,
) -> Result<(), &'static str> {
    let read = || source.pages.load(Ordering::Relaxed);

    // A read 40 MiB in: the file's byte, and one run of pages read for it.
    let before = read();
    let into = 40 * 1024 * 1024 + 123;
    if byte_at(space, LARGE_VADDR + into)? != file_byte(LARGE_OFFSET + into) {
        return Err("a page of the large segment read something other than the file's byte");
    }
    let filled = read() - before;
    if filled == 0 || filled > FAULT_MOST {
        return Err("touching one page of the large segment did not read one run of the file");
    }

    // A write 50 MiB in reads back in the program and never reaches the file.
    let into = 50 * 1024 * 1024 + 5;
    let at = LARGE_VADDR + into;
    space
        .with_page(at, Access::WRITE, |byte| {
            // SAFETY: `with_page` passes the direct-map address of `at`,
            // translated for a write under the space's lock, which it holds
            // while this runs; the write fault made the page this mapping's
            // own copy, so the frame is the program's to write.
            unsafe { core::ptr::write(byte as *mut u8, WRITTEN) }
        })
        .map_err(|_| "a write to the large segment faulted")?;
    if byte_at(space, at)? != WRITTEN {
        return Err("a write to the large segment did not read back");
    }
    let mut kept = [0_u8; 1];
    store
        .read(LARGE_OFFSET + into, &mut kept)
        .map_err(|_| "the file's page under the write could not be read")?;
    if kept != [file_byte(LARGE_OFFSET + into)] {
        return Err("a write to a writable segment reached the file it was mapped from");
    }

    // The contents end 100 bytes into a page: the rest of it is `.bss`, and
    // so are the pages after it, although the file's bytes there are not
    // zero.
    let end = LARGE_VADDR + LARGE_FILESZ;
    if byte_at(space, end - 1)? != file_byte(LARGE_OFFSET + LARGE_FILESZ - 1) {
        return Err("the last byte of the large segment's contents is not the file's");
    }
    if byte_at(space, end)? != 0 || byte_at(space, end + 2 * PAGE_SIZE)? != 0 {
        return Err("the large segment's .bss holds the file's bytes rather than zeros");
    }
    Ok(())
}

/// The second part: the program as a sparse file, opened as `execve` opens
/// one, and a fork of it running `/proc/self/exe` once the file's name is
/// gone. Returns the fork's status.
fn check_proc_self_exe(head: &[u8]) -> Result<i32, &'static str> {
    let ns = crate::fs::namespace();
    let ctx = ns.context();
    let file = ns
        .open(&ctx, None, SELF_PATH, &create(), 0o755)
        .map_err(|_| "could not make the sparse program under /tmp")?;
    let written = file.write(head);
    let grown = file.set_len(FILE_LEN);
    drop(file);
    let outcome = if written == Ok(head.len()) && grown.is_ok() {
        fork_and_exec_self()
    } else {
        Err("could not write the sparse program under /tmp")
    };
    let _ = ns.unlink(&ctx, None, SELF_PATH);
    outcome
}

/// The body of [`check_proc_self_exe`], with the file at [`SELF_PATH`].
fn fork_and_exec_self() -> Result<i32, &'static str> {
    let ns = crate::fs::namespace();
    let ctx = ns.context();
    // Opened as `execve` opens a path. Before programs were mapped from their
    // files this was `EFBIG`: the file is past what `read_file` reads whole.
    let (program, exe, set_ids) = crate::fs::open_program(&ctx, None, SELF_PATH)
        .map_err(|_| "execve's open of a 72 MiB program under /tmp was refused")?;
    let parent = exec::load_executable(
        Executable {
            image: Source::File(&program),
            exe: &exe,
            exec_fn: SELF_PATH,
            set_ids,
            interpreter: None,
        },
        &[SELF_PATH],
        &[],
        [0x5a; ferrix_ustack::RANDOM_BYTES],
    )
    .map_err(|_| "the sparse 72 MiB program could not be loaded")?;
    drop(program);
    // The parent gives up new privileges and stops being dumpable before the
    // fork. The fork must inherit both, and its `execve` keep the first and
    // make it dumpable again, as Linux's `begin_new_exec` does for a program
    // run with the caller's own ids.
    attributes::update(&parent, |set| {
        set.no_new_privs = true;
        set.dumpable = false;
    });
    let child = process::fork_for_check(&parent)
        .map_err(|_| "the sparse program's process could not be forked")?;
    let inherited = attributes::get(&child);
    if !inherited.no_new_privs || inherited.dumpable {
        return Err("a fork did not inherit no_new_privs and its parent's dumpability");
    }

    // The name goes: from here only the file itself leads to the program.
    ns.unlink(&ctx, None, SELF_PATH)
        .map_err(|_| "could not remove the sparse program's name")?;
    // Read now, judged after the `execve`: what the call answers is the
    // more telling of the two when both are wrong.
    let link = alloc::format!("/proc/{}/exe", child.pid());
    let reads_as = ns.read_link(&ctx, None, link.as_bytes());

    // `execve("/proc/self/exe", ["/proc/self/exe", NULL], NULL)`, staged in
    // the fork's own memory and made by a task of its own, so that `self` is
    // the fork.
    let argv = STAGED + 64;
    let mut vector = Vec::new();
    for pointer in [STAGED, 0] {
        vector.extend(pointer.to_le_bytes().into_iter().take(size_of::<usize>()));
    }
    uaccess::copy_to_user(child.space(), STAGED, PROC_SELF_EXE)
        .and_then(|()| uaccess::copy_to_user(child.space(), argv, &vector))
        .map_err(|_| "could not stage execve's arguments in the fork")?;
    *STAGED_CALL.lock() = Some((STAGED, argv));
    *REFUSED.lock() = None;
    *AFTER_EXEC.lock() = None;
    let task = crate::sched::spawn_user(
        "exec-self",
        exec_self,
        Arc::new(Thread::leader(&child).map_err(|_| "no memory for a check's thread")?),
        None,
        None,
    )
    .map_err(|_| "no task for the fork that runs /proc/self/exe")?;
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let status = child
        .wait_for_exit(deadline)
        .ok_or("the fork that ran /proc/self/exe never ended")?;
    // Gone, not only ended, for the reason `run_to_its_end` gives.
    crate::sched::wait_until_gone(&task, crate::sched::REAPER_PATIENCE_NANOS)?;
    drop(task);
    if let Some(errno) = REFUSED.lock().take() {
        crate::console::println!("  exec     execve(\"/proc/self/exe\") in a fork: errno {errno}");
        return Err("a fork's execve of /proc/self/exe was refused");
    }
    if status != arch::USER_TEST_STATUS {
        return Err("a fork that ran /proc/self/exe did not exit with the program's status");
    }
    match AFTER_EXEC.lock().take() {
        Some((true, true)) => {}
        Some((false, _)) => return Err("a fork's execve forgot no_new_privs"),
        Some((true, false)) => {
            return Err("a fork's execve of a program with its own ids left it not dumpable");
        }
        None => return Err("the fork that ran /proc/self/exe did not say what execve left"),
    }
    if reads_as != Ok(SELF_DELETED.to_vec()) {
        return Err("a fork's /proc/<pid>/exe does not read as its parent's file, deleted");
    }
    if child.exe() != SELF_PATH {
        return Err("a fork that ran /proc/self/exe was not recorded as the program's file");
    }
    // The parent has never run; it runs now, from the pages the fork shared.
    let _ = run_to_its_end(&parent)?;
    Ok(status)
}

/// The fork's task: make the staged `execve`, and enter the program it
/// loaded, or record the refusal and end.
fn exec_self(_argument: usize) {
    let staged = STAGED_CALL.lock().take();
    let outcome = match (process::current(), staged) {
        (Some(me), Some((path, argv))) => {
            exec::sys_execve(&me, path, argv, 0, size_of::<usize>()).map(|_| ())
        }
        _ => Err(ExecveError::Refused(Errno::EINVAL)),
    };
    match outcome {
        Ok(()) => {
            if let Some(me) = process::current() {
                let left = attributes::get(&me);
                *AFTER_EXEC.lock() = Some((left.no_new_privs, left.dumpable));
            }
            process::run_program(0);
        }
        Err(ExecveError::Refused(errno)) => {
            *REFUSED.lock() = Some(i32::from(errno.0));
            process::exit_current(EXEC_FAILED)
        }
        Err(ExecveError::Lost) => {
            *REFUSED.lock() = Some(-1);
            process::exit_current(EXEC_FAILED)
        }
    }
}

/// What the check writes into the large segment: a byte the file never
/// holds there, since every byte [`file_byte`] makes has its top bit set.
const WRITTEN: u8 = 0x5A;

/// The byte at user address `at` of `space`, faulted in for a read.
fn byte_at(space: &AddressSpace, at: u64) -> Result<u8, &'static str> {
    space
        .with_page(at, Access::READ, |byte| {
            // SAFETY: `with_page` passes the direct-map address of `at`,
            // translated under the space's lock, which it holds while this
            // runs, so the frame stays mapped there for the read.
            unsafe { core::ptr::read(byte as *const u8) }
        })
        .map_err(|_| "a read of the loaded program's memory faulted")
}

/// The file's byte at `offset`, past its first two pages: never zero.
fn file_byte(offset: u64) -> u8 {
    let page = offset / PAGE_SIZE;
    let within = offset % PAGE_SIZE;
    0x80 | ((page as u8).wrapping_mul(37) ^ (within as u8))
}

/// The program's contents: its headers, text and data in the first two
/// pages, and [`file_byte`] everywhere else up to its length. Counts the
/// pages it is asked for.
#[derive(Debug)]
struct ProgramSource {
    head: Vec<u8>,
    pages: AtomicU64,
}

impl PageSource for ProgramSource {
    fn fill_range(&self, first: u64, pages: &mut [&mut [u8]]) -> ferrix_vfs::Result<usize> {
        let _ = self.pages.fetch_add(pages.len() as u64, Ordering::Relaxed);
        for (index, page) in (first..).zip(pages.iter_mut()) {
            let start = index * PAGE_SIZE;
            for (offset, byte) in (start..).zip(page.iter_mut()) {
                *byte = if offset >= FILE_LEN {
                    0
                } else if offset < 2 * PAGE_SIZE {
                    usize::try_from(offset)
                        .ok()
                        .and_then(|at| self.head.get(at))
                        .copied()
                        .unwrap_or(0)
                } else {
                    file_byte(offset)
                };
            }
        }
        Ok(pages.len())
    }
}

/// The program's file: its store, at its length, mapped through its store's
/// object as a file on a disk is.
#[derive(Debug)]
struct ProgramInode {
    store: Arc<dyn Pages>,
}

impl Inode for ProgramInode {
    fn metadata(&self) -> Metadata {
        let now = crate::fs::clock().now();
        Metadata {
            ino: 1,
            kind: FileType::Regular,
            permissions: 0o755,
            nlink: 1,
            uid: 0,
            gid: 0,
            size: FILE_LEN,
            rdev: 0,
            blocks: 0,
            block_size: 4096,
            atime: now,
            mtime: now,
            ctime: now,
        }
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> ferrix_vfs::Result<usize> {
        let left = FILE_LEN.saturating_sub(offset);
        let len = usize::try_from(left).unwrap_or(usize::MAX).min(buf.len());
        let slot = buf.get_mut(..len).ok_or(Errno::EIO)?;
        self.store.read(offset, slot)?;
        Ok(len)
    }

    fn mapping(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        self.store.object()
    }
}
