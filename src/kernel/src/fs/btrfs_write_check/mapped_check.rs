//! K3b from ring 3 (`docs/microkernel/K3b.md`): a program's own store through a
//! shared mapping of a file on the writable disk marks its page, and `fsync`
//! carries it, or fails closed with no memory for the list (ledger 5981 (3), (4)).

use alloc::sync::Arc;
use core::any::Any;

use ferrix_elf::Class;
use ferrix_native_abi::nr;
use ferrix_native_abi::rights::Rights;
use ferrix_native_abi::signals::Signals;
use ferrix_vfs::Errno;
use ferrix_vma::VmaFlags;

use super::{pattern, write_file};
use crate::arch;
use crate::discovery::first_image_check::load_for_check;
use crate::fs;
use crate::object::Object;
use crate::object::check::{SCRATCH, Side, reg};
use crate::object::job::{Job, KILLED_STATUS};
use crate::syscall::image;
use crate::syscall::process;
use crate::user::cache;
use crate::user::space::{FileMapping, FilePlace};
use crate::user::vmo::{PageState, Vmo};

/// Where the writable disk is.
const MOUNT_POINT: &[u8] = b"/mnt-rw";
/// The file.
const FILE: &[u8] = b"/mnt-rw/mapped-check";
/// Its pages.
const PAGES: usize = 4;
/// The page the program writes, and where the file is mapped in it.
const PAGE: u64 = 2;
/// Where the program's store lands: the mapping is at `0x6000_0000` less `PAGE`'s.
const MAP_AT: u64 = 0x6000_0000 - PAGE * 4096;
/// The byte.
const STORED: u8 = 0xAB;

/// Where a deadline is staged.
const DEADLINE_AT: u64 = SCRATCH + 0x300;
/// What a wait saw.
const OBSERVED_AT: u64 = SCRATCH + 0x308;
/// A name.
const NAME_AT: u64 = SCRATCH + 0x200;
/// How long the program may take.
const PATIENCE_NANOS: u64 = 60_000_000_000;

/// Commit the volume.
fn sync() -> Result<(), &'static str> {
    let ns = fs::namespace();
    let ctx = ns.context();
    ns.resolve(&ctx, None, MOUNT_POINT, true)
        .map_err(|_| "the mount point could not be resolved")?
        .mount
        .filesystem()
        .sync()
        .map_err(|_| "the volume could not be synced around a reclaim")
}

/// The object of the file at `path`.
fn object_of(path: &[u8]) -> Result<Arc<Vmo>, &'static str> {
    let ns = fs::namespace();
    let ctx = ns.context();
    ns.resolve(&ctx, None, path, true)
        .map_err(|_| "a file reclaim is checked on is gone")?
        .inode()
        .map_err(|_| "a file reclaim is checked on has no inode")?
        .mapping()
        .and_then(|object| object.downcast::<Vmo>().ok())
        .ok_or("a file of a writable mount has no object to map")
}

/// Read `out.len()` bytes of the file at `path`.
fn read_at(path: &[u8], offset: u64, out: &mut [u8]) -> Result<(), &'static str> {
    let ns = fs::namespace();
    let ctx = ns.context();
    let inode = ns
        .resolve(&ctx, None, path, true)
        .map_err(|_| "a file read back is gone")?
        .inode()
        .map_err(|_| "a file read back has no inode")?;
    match inode.read_at(offset, out) {
        Ok(read) if read == out.len() => Ok(()),
        _ => Err("a file could not be read back"),
    }
}

/// The program's image, for this kernel's machine.
fn program() -> alloc::vec::Vec<u8> {
    let class = if size_of::<usize>() == 8 {
        Class::Elf64
    } else {
        Class::Elf32
    };
    image::build_with(
        class,
        arch::ARCH.elf_machine(),
        image::Shape::Good,
        arch::programs_check::STORE,
    )
}

/// Run the store, in a process with the file mapped.
fn run_the_store(object: Arc<Vmo>) -> Result<(), &'static str> {
    let side = Side::new()?;
    let root = Job::new_root().map_err(|_| "mapped: no job")?;
    let job = side
        .process
        .with_handles(|table| table.insert(Object::Job(root), Rights::JOB))
        .map_err(|_| "mapped: no room for a job")?;
    side.put(NAME_AT, b"store")?;
    let child = side.handle(
        nr::PROCESS_MAKE,
        &[reg(job), NAME_AT, 5],
        "mapped: process_make refused",
    )?;
    let host = side
        .process
        .with_handles(|table| match table.get(child) {
            Ok((Object::Process(made), _)) => made.control().and_then(|held| held.host()),
            _ => None,
        })
        .ok_or("mapped: the program's process is gone")?;
    let space = host.core().own_space();
    let at = load_for_check(space, &program()).map_err(|_| "mapped: the program would not load")?;
    let _ = space
        .map_file(
            FilePlace::Fixed(MAP_AT),
            (PAGES as u64) * 4096,
            VmaFlags {
                shared: true,
                ..VmaFlags::READ_WRITE
            },
            object,
            0,
            FileMapping {
                file: Arc::new(()) as Arc<dyn Any + Send + Sync>,
                may_write: true,
            },
        )
        .map_err(|_| "mapped: the file would not map")?;
    let _ = side
        .call(nr::PROCESS_RUN, &[reg(child), at.entry, at.stack, 0])
        .map_err(|_| "mapped: the program would not run")?;
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    side.put(DEADLINE_AT, &deadline.to_ne_bytes())?;
    let _ = side
        .call(
            nr::OBJECT_WAIT_ONE,
            &[
                reg(child),
                u64::from(Signals::TERMINATED.0),
                DEADLINE_AT,
                OBSERVED_AT,
            ],
        )
        .map_err(|_| "mapped: the program never ended")?;
    let status = side.process.with_handles(|table| match table.get(child) {
        Ok((Object::Process(made), _)) => made.exit_status(),
        _ => None,
    });
    side.close_everything();
    process::kill(&side.process, KILLED_STATUS);
    if status != Some(0) {
        return Err("mapped: the program's store did not go through");
    }
    Ok(())
}

/// Run the check.
///
/// # Errors
///
/// The first thing that did not hold.
///
/// Verifies: L.user.206, L.user.209
pub(super) fn run() -> Result<(), &'static str> {
    let body = pattern(61, 0, PAGES * 4096);
    write_file(FILE, &body)?;
    sync()?;
    let object = object_of(FILE)?;
    run_the_store(Arc::clone(&object))?;
    if object.page_state(PAGE) != PageState::Dirty {
        return Err("mapped: a program's store through a mapping did not make its page Dirty");
    }
    if object.page_state(PAGE - 1) == PageState::Dirty {
        return Err("mapped: a page the program did not write is Dirty");
    }

    // With no memory for the list, `fsync` fails closed and nothing changes state.
    let ns = fs::namespace();
    let ctx = ns.context();
    let inode = ns
        .resolve(&ctx, None, FILE, true)
        .map_err(|_| "mapped: the file is gone")?
        .inode()
        .map_err(|_| "mapped: the file has no inode")?;
    let task = crate::sched::current_id().ok_or("mapped: no task")?;
    crate::fallible::inject_once(task, 1, false);
    let answer = inode.fsync(false);
    let _ = crate::fallible::stop_injecting();
    if answer != Err(Errno::ENOMEM) {
        return Err("mapped: fsync with no memory for the list did not answer ENOMEM");
    }
    if object.page_state(PAGE) != PageState::Dirty {
        return Err("mapped: a failed list left its page other than Dirty");
    }

    inode.fsync(false).map_err(|_| "mapped: fsync failed")?;
    if object.page_state(PAGE) != PageState::Clean {
        return Err("mapped: a page written back is not Clean");
    }
    // The disk has it: dropped from the cache and read again.
    sync()?;
    let all = cache::totals().disk;
    let _ = cache::reclaim(crate::object::quota::NONE, all);
    let mut read = [0_u8; 1];
    read_at(FILE, PAGE * 4096, &mut read)?;
    if read[0] != STORED {
        return Err("mapped: the file does not read what the program stored");
    }
    crate::console::println!(
        "  btrfs-rw a program's store through a shared mapping marked its page, fsync with no \
         memory for the list answered ENOMEM and left it Dirty, and the next wrote it"
    );
    Ok(())
}
