//! Stage 9's `space` line: an address space mapped, protected, unmapped and
//! read by handle (`docs/microkernel/K2.md`, slice A).
//!
//! Every call is made through the architecture's own system call entry
//! (`arch::drive_native_words`), by a thread of a check's process
//! (`syscall::check::spawn_in`), as `write_read_check`'s are: what is checked
//! is what a program's call gets, on each architecture. The process is in a
//! job of its own, the caller's job `C`; the child it makes with
//! `process_create` is in a sibling, the owner's job `O`.
//!
//! The cases:
//! 1. `space_create` makes a space, one object charged to `C`; a VMO mapped
//!    into it by handle reads back through `space_read` as the VMO holds it.
//! 2. An executable mapping needs `EXECUTE` on the VMO handle and on the
//!    space handle: refused without either, made with both. `vmo_executable`
//!    gives a handle with `EXECUTE` and without `WRITE`, closes the one it
//!    took, and no duplicate adds `EXECUTE`.
//! 3. Writable and executable together is `INVALID_ARGS` from `space_map` and
//!    `space_protect`, and refused again by the address space itself under
//!    its lock, for a region whose handle carried both (ASR-2, two layers).
//! 4. `space_protect` keeps each region within what its handle allowed, and
//!    refuses a region no handle mapped with `BAD_STATE`.
//! 5. Each call is refused without its right; `space_unmap` takes the range
//!    away; a read across the end of a mapping is short.
//! 6. `process_space` gives the space of a created process before it starts,
//!    and only through a handle with `MANAGE` that `process_create` made;
//!    a page a by-handle read faults in there is charged to `O` and not to
//!    `C`, and at `O`'s limit the read is `NO_MEMORY` with nothing killed and
//!    nothing charged to `C`. After the start `process_space` is `BAD_STATE`,
//!    and a `READ`-only space handle taken before it still reads the running
//!    child's text and maps nothing (the review's condition 1).
//! 7. `space_unmap` of the running child's text through the creator's handle
//!    takes its translation down wherever it runs: the child faults and is
//!    killed by `SIGSEGV`.
//! 8. The sweep of every user root the check reached finds no leaf writable
//!    and executable, and some executable.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_elf::Class;
use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::rights::{Rights, SAME_RIGHTS};
use ferrix_native_abi::status;
use ferrix_native_abi::types::{MAP_EXECUTE, MAP_READ, MAP_WRITE, PROCESS_KILLED, ProcessStatus};
use ferrix_vma::VmaFlags;

use super::Object;
use super::job::Job;
use super::process::{Host, ProcessRef};
use super::quota::Resource;
use crate::arch;
use crate::sched::Task;
use crate::sync::SpinLock;
use crate::syscall::check::spawn_in;
use crate::syscall::image::{self, Shape};
use crate::syscall::process::{self, Process};
use crate::syscall::uaccess;
use crate::user::space::{AddressSpace, Ceiling, ProtectRefused};

/// The check process's scratch: staged arguments and read-back buffers.
const SCRATCH: u64 = 0x6000_0000;
/// Its length: the child's image fits, with room for the rest beside it.
const SCRATCH_LEN: u64 = 8 * PAGE_SIZE;
/// Where a `*u64` offset is staged.
const OFFSET: u64 = SCRATCH;
/// Where a `process_status` is written.
const STATUS: u64 = SCRATCH + 0x40;
/// Where the child's name is staged.
const NAME: u64 = SCRATCH + 0x80;
/// Where bytes are staged and read back.
const BUFFER: u64 = SCRATCH + PAGE_SIZE;
/// Where the child's image is staged: the last pages of the scratch.
const IMAGE: u64 = SCRATCH + 4 * PAGE_SIZE;
/// What the check writes into a VMO and reads back through a mapping.
const SECRET: &[u8] = b"mapped by handle";
/// The child's name.
const CHILD_NAME: &[u8] = b"k2-child";

/// Where the read-write mapping goes in the made space.
const AT_RW: u64 = 0x5000_0000;
/// Where the read-only mapping goes.
const AT_RO: u64 = 0x5001_0000;
/// Where the executable mapping goes.
const AT_RX: u64 = 0x5002_0000;
/// Where a region no handle mapped is put, by the kernel, as a program's own.
const AT_OWN: u64 = 0x5003_0000;
/// Where the kernel-internal regions of case 3 go.
const AT_BOTH: u64 = 0x5004_0000;
/// Where the owner's pages go in the child's space.
const AT_OWNED: u64 = 0x5005_0000;

/// How long the child may take to fault once its text is gone, and the
/// thread to finish.
const PATIENCE_NANOS: u64 = 20_000_000_000;
/// How often a wait for a task's end looks.
const POLL_NANOS: u64 = 1_000_000;

/// What the check found, for the boot line.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Report {
    /// Maps, protects and unmaps made by handle that succeeded.
    pub(crate) changes: u32,
    /// Refusals, each as specified.
    pub(crate) refusals: u32,
    /// User leaves the W^X sweep walked.
    pub(crate) leaves: u64,
    /// Of them, executable.
    pub(crate) executable: u64,
    /// Bytes the owner's job rose by for one by-handle read.
    pub(crate) owner_bytes: u64,
}

/// Handed to the check's thread.
struct Setup {
    /// The process the thread runs in.
    process: Arc<Process>,
    /// The owner's job, `O`.
    owner: Arc<Job>,
    /// The caller's job, `C`.
    caller: Arc<Job>,
    /// `O`, in the process's table, with every job right.
    owner_handle: Handle,
    /// A process handle with `MANAGE` and no way back to its process.
    blind: Handle,
}

/// Handed to the thread.
static SETUP: SpinLock<Option<Setup>> = SpinLock::new(None);
/// What the thread found.
static FOUND: SpinLock<Option<Result<Report, &'static str>>> = SpinLock::new(None);

/// Run the check. See the module.
///
/// # Errors
///
/// The first thing that was not so.
///
/// Verifies: H.MEM.24, L.object.232, L.object.233, L.object.234
/// Verifies: L.object.235, L.object.236, L.object.238, L.object.240
/// Verifies: L.user.158, L.user.159, L.user.162
pub(crate) fn run() -> Result<Report, &'static str> {
    let tree = Job::new_root().map_err(|_| "no memory for the space check's jobs")?;
    let caller = tree
        .new_child()
        .map_err(|_| "the space check's caller job was refused")?;
    let owner = tree
        .new_child()
        .map_err(|_| "the space check's owner job was refused")?;
    let process = process::new_for_check().map_err(|_| "no process for the space check")?;
    process
        .move_new_to(&caller)
        .map_err(|_| "the space check's process could not join its job")?;
    let _ = process
        .space()
        .map_anonymous(SCRATCH, SCRATCH_LEN, VmaFlags::READ_WRITE)
        .map_err(|_| "could not map the space check's scratch")?;
    let core = Host::core(&*process);
    let owner_handle = process
        .with_handles(|table| table.insert(Object::Job(Arc::clone(&owner)), Rights::JOB))
        .map_err(|_| "no room for the space check's job")?;
    let blind = process
        .with_handles(|table| table.insert(Object::Process(ProcessRef::new(core)), Rights::PROCESS))
        .map_err(|_| "no room for the space check's blind handle")?;
    *FOUND.lock() = None;
    *SETUP.lock() = Some(Setup {
        process: Arc::clone(&process),
        owner: Arc::clone(&owner),
        caller: Arc::clone(&caller),
        owner_handle,
        blind,
    });
    let task = spawn_in(&process, "space check", in_the_process, None)?;
    let deadline = crate::timer::now_nanos().saturating_add(2 * PATIENCE_NANOS);
    if process.wait_for_exit(deadline).is_none() {
        return Err("the space check's thread never finished");
    }
    wait_dead(&task, deadline, "the space check's thread never ended")?;
    let _ = owner.kill(super::job::KILLED_STATUS);
    FOUND
        .lock()
        .take()
        .ok_or("the space check's thread found nothing")?
}

/// The thread: the cases, then the end of its process.
fn in_the_process(_argument: usize) {
    let setup = SETUP.lock().take();
    let outcome = match setup {
        Some(setup) => cases(&setup),
        None => Err("the space check's thread was given nothing"),
    };
    *FOUND.lock() = Some(outcome);
    process::exit_current(0)
}

/// Wait until `task` has ended, by `deadline`.
fn wait_dead(task: &Task, deadline: u64, what: &'static str) -> Result<(), &'static str> {
    while !task.is_dead() {
        if crate::timer::now_nanos() >= deadline {
            return Err(what);
        }
        crate::sched::sleep_for(POLL_NANOS);
    }
    Ok(())
}

/// A native call through the entry, from this thread.
fn call(number: usize, args: &[u64]) -> Result<usize, Errno> {
    let mut registers = [0_u64; 6];
    for (slot, value) in registers.iter_mut().zip(args) {
        *slot = *value as usize as u64;
    }
    let (answer, _) = arch::drive_native_words(number, registers);
    if answer < 0 {
        Err(Errno(answer.unsigned_abs() as u16))
    } else {
        Ok(answer as usize)
    }
}

/// A call that answers a handle.
fn made(number: usize, args: &[u64], what: &'static str) -> Result<Handle, &'static str> {
    let value = call(number, args).map_err(|_| what)?;
    u32::try_from(value).map(Handle).map_err(|_| what)
}

/// A handle as a register.
fn reg(handle: Handle) -> u64 {
    u64::from(handle.0)
}

/// A refusal that must be exactly `wanted`.
fn refused(
    answer: Result<usize, Errno>,
    wanted: Errno,
    what: &'static str,
    report: &mut Report,
) -> Result<(), &'static str> {
    if answer != Err(wanted) {
        return Err(what);
    }
    report.refusals += 1;
    Ok(())
}

/// A duplicate of `handle` with exactly `rights`.
fn duplicate(handle: Handle, rights: Rights) -> Result<Handle, &'static str> {
    made(
        nr::HANDLE_DUPLICATE,
        &[reg(handle), u64::from(rights.0)],
        "space self-check failed: a handle could not be duplicated",
    )
}

/// The space check's own memory, through its own space.
struct Scratch<'a> {
    /// The space.
    space: &'a AddressSpace,
}

impl Scratch<'_> {
    /// Put `bytes` at `at`.
    fn put(&self, at: u64, bytes: &[u8]) -> Result<(), &'static str> {
        uaccess::copy_to_user(self.space, at, bytes)
            .map_err(|_| "space self-check failed: the scratch could not be written")
    }

    /// Read `len` bytes at `at`.
    fn get(&self, at: u64, len: usize) -> Result<Vec<u8>, &'static str> {
        let mut out = vec![0_u8; len];
        uaccess::copy_from_user(self.space, at, &mut out)
            .map_err(|_| "space self-check failed: the scratch could not be read")?;
        Ok(out)
    }
}

/// `space_map` of `vmo` into `space` at `at`, one page from offset zero.
fn map(space: Handle, vmo: Handle, at: u64, protection: u32) -> Result<usize, Errno> {
    call(
        nr::SPACE_MAP,
        &[
            reg(space),
            reg(vmo),
            at,
            PAGE_SIZE,
            u64::from(protection),
            OFFSET,
        ],
    )
}

/// `space_read` of `len` bytes at `at` into [`BUFFER`].
fn read(space: Handle, at: u64, len: u64) -> Result<usize, Errno> {
    call(nr::SPACE_READ, &[reg(space), at, BUFFER, len])
}

/// The cases. See the module.
fn cases(setup: &Setup) -> Result<Report, &'static str> {
    let mut report = Report::default();
    let scratch = Scratch {
        space: setup.process.space(),
    };
    scratch.put(OFFSET, &0_u64.to_ne_bytes())?;
    let (space, spaces) = made_space(setup, &scratch, &mut report)?;
    execute_needs_both(&scratch, space, &mut report)?;
    write_and_execute_refused(space, &spaces, &mut report)?;
    protect_within_the_ceiling(&scratch, space, &spaces, &mut report)?;
    each_call_needs_its_right(&scratch, space, &mut report)?;
    let child_space = a_created_process(setup, &scratch, &mut report)?;
    let roots = vec![spaces, child_space];
    for swept in &roots {
        let wx = crate::mm::check::sweep_user_root(swept).map_err(
            |_| "space self-check failed: a by-handle space holds a leaf writable and executable",
        )?;
        report.leaves += wx.leaves;
        report.executable += wx.executable;
    }
    if report.executable == 0 {
        return Err("space self-check failed: the user-root sweep found nothing executable");
    }
    a_running_childs_text_unmapped(setup, &scratch, &mut report)?;
    let _ = call(nr::HANDLE_CLOSE, &[reg(space)]);
    Ok(report)
}

/// Case 1: a made space, a VMO mapped into it, read back by handle; the
/// space one object charged to the caller's job.
fn made_space(
    setup: &Setup,
    scratch: &Scratch<'_>,
    report: &mut Report,
) -> Result<(Handle, Arc<AddressSpace>), &'static str> {
    let objects_before = objects(&setup.caller)?;
    let space = made(
        nr::SPACE_CREATE,
        &[],
        "space self-check failed: space_create failed",
    )?;
    if objects(&setup.caller)? != objects_before + 1 {
        return Err("space self-check failed: a made space was not one object of its maker's job");
    }
    let held = setup
        .process
        .with_handles(|table| match table.get(space) {
            Ok((Object::Space(space), _)) => Some(Arc::clone(space.space())),
            _ => None,
        })
        .ok_or("space self-check failed: space_create did not answer a space")?;
    if held.region_count() != 0 {
        return Err("space self-check failed: a made space was not empty");
    }
    let vmo = made(
        nr::VMO_CREATE,
        &[2 * PAGE_SIZE],
        "space self-check failed: vmo_create failed",
    )?;
    scratch.put(BUFFER, SECRET)?;
    scratch.put(OFFSET, &0x10_u64.to_ne_bytes())?;
    let _ = call(
        nr::VMO_WRITE,
        &[reg(vmo), BUFFER, SECRET.len() as u64, OFFSET],
    )
    .map_err(|_| "space self-check failed: vmo_write failed")?;
    scratch.put(OFFSET, &0_u64.to_ne_bytes())?;
    if map(space, vmo, AT_RW, MAP_READ | MAP_WRITE) != Ok(AT_RW as usize) {
        return Err("space self-check failed: a VMO was not mapped by handle where asked");
    }
    report.changes += 1;
    scratch.put(BUFFER, &[0; 32])?;
    if read(space, AT_RW + 0x10, SECRET.len() as u64) != Ok(SECRET.len()) {
        return Err("space self-check failed: space_read did not read a mapped page");
    }
    if scratch.get(BUFFER, SECRET.len())? != SECRET {
        return Err("space self-check failed: a mapping by handle did not show the VMO's bytes");
    }
    // A read-only view of the same VMO, for case 4.
    let read_only = duplicate(vmo, Rights::MAP | Rights::READ)?;
    if map(space, read_only, AT_RO, MAP_READ) != Ok(AT_RO as usize) {
        return Err("space self-check failed: a read-only VMO handle was not mapped by handle");
    }
    report.changes += 1;
    for closing in [vmo, read_only] {
        let _ = call(nr::HANDLE_CLOSE, &[reg(closing)]);
    }
    Ok((space, held))
}

/// The caller's job's object count.
fn objects(job: &Job) -> Result<u64, &'static str> {
    job.usage(Resource::Objects)
        .map(|usage| usage.used)
        .ok_or("space self-check failed: the caller's job has no quota")
}

/// A job's memory.
fn memory(job: &Job) -> Result<u64, &'static str> {
    job.usage(Resource::Memory)
        .map(|usage| usage.used)
        .ok_or("space self-check failed: a job of the check has no quota")
}

/// Case 2: `EXECUTE` on both handles, and `vmo_executable`.
fn execute_needs_both(
    scratch: &Scratch<'_>,
    space: Handle,
    report: &mut Report,
) -> Result<(), &'static str> {
    let vmo = made(
        nr::VMO_CREATE,
        &[PAGE_SIZE],
        "space self-check failed: vmo_create failed",
    )?;
    refused(
        map(space, vmo, AT_RX, MAP_READ | MAP_EXECUTE),
        status::ACCESS_DENIED,
        "space self-check failed: a VMO handle without EXECUTE was mapped executable",
        report,
    )?;
    refused(
        call(
            nr::HANDLE_DUPLICATE,
            &[reg(vmo), u64::from((Rights::READ | Rights::EXECUTE).0)],
        ),
        status::ACCESS_DENIED,
        "space self-check failed: a duplicate added EXECUTE to a VMO handle",
        report,
    )?;
    let runnable = made(
        nr::VMO_EXECUTABLE,
        &[reg(vmo), 0],
        "space self-check failed: vmo_executable failed",
    )?;
    refused(
        call(nr::VMO_GET_SIZE, &[reg(vmo), BUFFER]),
        status::BAD_HANDLE,
        "space self-check failed: vmo_executable left the handle it replaced open",
        report,
    )?;
    scratch.put(BUFFER, SECRET)?;
    refused(
        call(
            nr::VMO_WRITE,
            &[reg(runnable), BUFFER, SECRET.len() as u64, OFFSET],
        ),
        status::ACCESS_DENIED,
        "space self-check failed: a VMO handle made executable could still write",
        report,
    )?;
    let no_execute = duplicate(space, Rights(Rights::SPACE.0 & !Rights::EXECUTE.0))?;
    refused(
        map(no_execute, runnable, AT_RX, MAP_READ | MAP_EXECUTE),
        status::ACCESS_DENIED,
        "space self-check failed: a space handle without EXECUTE took an executable mapping",
        report,
    )?;
    if map(space, runnable, AT_RX, MAP_READ | MAP_EXECUTE) != Ok(AT_RX as usize) {
        return Err(
            "space self-check failed: an executable mapping through two EXECUTE handles \
                    was refused",
        );
    }
    report.changes += 1;
    // Faulted in, so the sweep has an executable leaf to see.
    if read(space, AT_RX, 8) != Ok(8) {
        return Err("space self-check failed: an executable mapping by handle could not be read");
    }
    for closing in [runnable, no_execute] {
        let _ = call(nr::HANDLE_CLOSE, &[reg(closing)]);
    }
    Ok(())
}

/// Case 3: writable and executable, refused at decoding and by the space.
fn write_and_execute_refused(
    space: Handle,
    held: &Arc<AddressSpace>,
    report: &mut Report,
) -> Result<(), &'static str> {
    let both = MAP_READ | MAP_WRITE | MAP_EXECUTE;
    refused(
        map(space, Handle(0), AT_BOTH, both),
        status::INVALID_ARGS,
        "space self-check failed: space_map took a writable and executable protection",
        report,
    )?;
    refused(
        call(
            nr::SPACE_PROTECT,
            &[reg(space), AT_RW, PAGE_SIZE, u64::from(both)],
        ),
        status::INVALID_ARGS,
        "space self-check failed: space_protect took a writable and executable protection",
        report,
    )?;
    // The second layer, asked directly as a decoder that let the word through
    // would ask it: a handle that carried both rights still gets no region
    // both writable and executable.
    let vmo = crate::user::vmo::Vmo::new_anonymous(1)
        .map_err(|_| "space self-check failed: no memory for a VMO")?;
    let both_rights = Ceiling::Handle {
        write: true,
        execute: true,
    };
    let rwx = VmaFlags {
        write: true,
        execute: true,
        ..VmaFlags::READ
    };
    if held
        .map_by_handle(
            Some(AT_BOTH),
            PAGE_SIZE,
            Arc::clone(&vmo),
            0,
            rwx,
            both_rights,
        )
        .is_ok()
    {
        return Err(
            "space self-check failed: an address space mapped a region writable and \
                    executable",
        );
    }
    report.refusals += 1;
    let _ = held
        .map_by_handle(
            Some(AT_BOTH),
            PAGE_SIZE,
            vmo,
            0,
            VmaFlags::READ_WRITE,
            both_rights,
        )
        .map_err(|_| "space self-check failed: a writable mapping by handle was refused")?;
    if held.protect_by_handle(AT_BOTH, PAGE_SIZE, rwx) != Err(ProtectRefused::Beyond) {
        return Err(
            "space self-check failed: an address space made a region writable and \
                    executable",
        );
    }
    report.refusals += 1;
    let runnable = VmaFlags {
        execute: true,
        ..VmaFlags::READ
    };
    held.protect_by_handle(AT_BOTH, PAGE_SIZE, runnable)
        .map_err(|_| {
            "space self-check failed: a region whose handle carried EXECUTE could not \
                      be made executable"
        })?;
    report.changes += 1;
    Ok(())
}

/// Case 4: protection within each region's ceiling, and only by handle.
fn protect_within_the_ceiling(
    scratch: &Scratch<'_>,
    space: Handle,
    held: &Arc<AddressSpace>,
    report: &mut Report,
) -> Result<(), &'static str> {
    let protect = |at: u64, protection: u32| {
        call(
            nr::SPACE_PROTECT,
            &[reg(space), at, PAGE_SIZE, u64::from(protection)],
        )
    };
    refused(
        protect(AT_RO, MAP_READ | MAP_WRITE),
        status::ACCESS_DENIED,
        "space self-check failed: a region from a read-only VMO handle was made writable",
        report,
    )?;
    refused(
        protect(AT_RX, MAP_READ | MAP_WRITE),
        status::ACCESS_DENIED,
        "space self-check failed: a region from an executable VMO handle was made writable",
        report,
    )?;
    refused(
        protect(AT_RW, MAP_READ | MAP_EXECUTE),
        status::ACCESS_DENIED,
        "space self-check failed: a region from a VMO handle without EXECUTE was made executable",
        report,
    )?;
    for protection in [MAP_READ, MAP_READ | MAP_WRITE] {
        let _ = protect(AT_RW, protection)
            .map_err(|_| "space self-check failed: a protection within its ceiling was refused")?;
        report.changes += 1;
    }
    // A region the program made itself, as Linux `mmap` makes one.
    let _ = held
        .map_anonymous(AT_OWN, PAGE_SIZE, VmaFlags::READ_WRITE)
        .map_err(|_| "space self-check failed: no room for a program's own region")?;
    refused(
        protect(AT_OWN, MAP_READ),
        status::BAD_STATE,
        "space self-check failed: space_protect changed a region no handle mapped",
        report,
    )?;
    // Nothing changed by the refusals: the bytes are still readable.
    if read(space, AT_RW + 0x10, SECRET.len() as u64) != Ok(SECRET.len())
        || scratch.get(BUFFER, SECRET.len())? != SECRET
    {
        return Err("space self-check failed: a refused protection changed a mapping");
    }
    Ok(())
}

/// Case 5: every call refused without its right; unmap; a short read.
fn each_call_needs_its_right(
    scratch: &Scratch<'_>,
    space: Handle,
    report: &mut Report,
) -> Result<(), &'static str> {
    let _ = scratch;
    let without = |right: Rights| duplicate(space, Rights(Rights::SPACE.0 & !right.0));
    let no_read = without(Rights::READ)?;
    refused(
        read(no_read, AT_RW, 8),
        status::ACCESS_DENIED,
        "space self-check failed: space_read read without READ",
        report,
    )?;
    let no_map = without(Rights::MAP)?;
    let vmo = made(
        nr::VMO_CREATE,
        &[PAGE_SIZE],
        "space self-check failed: vmo_create failed",
    )?;
    refused(
        map(no_map, vmo, AT_BOTH + PAGE_SIZE, MAP_READ),
        status::ACCESS_DENIED,
        "space self-check failed: space_map mapped without MAP",
        report,
    )?;
    refused(
        call(nr::SPACE_UNMAP, &[reg(no_map), AT_RW, PAGE_SIZE]),
        status::ACCESS_DENIED,
        "space self-check failed: space_unmap unmapped without MAP",
        report,
    )?;
    refused(
        call(
            nr::SPACE_PROTECT,
            &[reg(no_map), AT_RW, PAGE_SIZE, u64::from(MAP_READ)],
        ),
        status::ACCESS_DENIED,
        "space self-check failed: space_protect changed a protection without MAP",
        report,
    )?;
    refused(
        read(vmo, AT_RW, 8),
        status::WRONG_TYPE,
        "space self-check failed: space_read took a VMO for a space",
        report,
    )?;
    // Short at the end of a mapping: one page mapped, two asked for.
    let edge = AT_RW + PAGE_SIZE - 8;
    if read(space, edge, 16) != Ok(8) {
        return Err("space self-check failed: a read across a mapping's end was not short");
    }
    let _ = call(nr::SPACE_UNMAP, &[reg(space), AT_RO, PAGE_SIZE])
        .map_err(|_| "space self-check failed: space_unmap failed")?;
    report.changes += 1;
    refused(
        read(space, AT_RO, 8),
        status::FAULT,
        "space self-check failed: an unmapped page was still read by handle",
        report,
    )?;
    for closing in [no_read, no_map, vmo] {
        let _ = call(nr::HANDLE_CLOSE, &[reg(closing)]);
    }
    Ok(())
}

/// The image the child runs: it spins, so it is still running when its
/// creator reads it, until its text is taken away.
fn child_image() -> Vec<u8> {
    let class = if size_of::<usize>() == 8 {
        Class::Elf64
    } else {
        Class::Elf32
    };
    image::build_with(
        class,
        arch::ARCH.elf_machine(),
        Shape::Good,
        arch::USER_SPIN_PROGRAM,
    )
}

/// Case 6: `process_space` before and after the start, and who pays.
/// Answers the child's space for the sweep, and leaves the child running
/// with its handles at [`CHILD`].
fn a_created_process(
    setup: &Setup,
    scratch: &Scratch<'_>,
    report: &mut Report,
) -> Result<Arc<AddressSpace>, &'static str> {
    let image = child_image();
    if image.len() as u64 > SCRATCH + SCRATCH_LEN - IMAGE {
        return Err("space self-check failed: the child's image does not fit the scratch");
    }
    scratch.put(IMAGE, &image)?;
    let vmo = made(
        nr::VMO_CREATE,
        &[image.len() as u64],
        "space self-check failed: vmo_create for the child's image failed",
    )?;
    scratch.put(OFFSET, &0_u64.to_ne_bytes())?;
    let _ = call(
        nr::VMO_WRITE,
        &[reg(vmo), IMAGE, image.len() as u64, OFFSET],
    )
    .map_err(|_| "space self-check failed: the child's image could not be written")?;
    scratch.put(NAME, CHILD_NAME)?;
    let child = made(
        nr::PROCESS_CREATE,
        &[
            reg(setup.owner_handle),
            reg(vmo),
            NAME,
            CHILD_NAME.len() as u64,
        ],
        "space self-check failed: process_create failed",
    )?;
    let _ = call(nr::HANDLE_CLOSE, &[reg(vmo)]);

    let same = u64::from(SAME_RIGHTS);
    refused(
        call(nr::PROCESS_SPACE, &[reg(setup.blind), same]),
        status::ACCESS_DENIED,
        "space self-check failed: process_space reached into a process its handle cannot reach",
        report,
    )?;
    let watcher = duplicate(child, Rights::WAIT)?;
    refused(
        call(nr::PROCESS_SPACE, &[reg(watcher), same]),
        status::ACCESS_DENIED,
        "space self-check failed: process_space answered a process handle without MANAGE",
        report,
    )?;
    refused(
        call(
            nr::PROCESS_SPACE,
            &[reg(child), u64::from((Rights::READ | Rights::MANAGE).0)],
        ),
        status::INVALID_ARGS,
        "space self-check failed: process_space gave a right a space does not carry",
        report,
    )?;
    let full = made(
        nr::PROCESS_SPACE,
        &[reg(child), same],
        "space self-check failed: process_space refused the creator before the start",
    )?;
    let reader = made(
        nr::PROCESS_SPACE,
        &[reg(child), u64::from(Rights::READ.0)],
        "space self-check failed: process_space refused a READ-only space handle",
    )?;
    let child_space = setup
        .process
        .with_handles(|table| match table.get(full) {
            Ok((Object::Space(space), _)) => Some(Arc::clone(space.space())),
            _ => None,
        })
        .ok_or("space self-check failed: process_space did not answer a space")?;

    // Who pays: a page the by-handle read faults in is the owner's.
    owner_pays(setup, scratch, full, report)?;
    after_the_start(scratch, child, reader, report)?;
    let _ = call(nr::HANDLE_CLOSE, &[reg(watcher)]);
    *CHILD.lock() = Some((child, full));
    Ok(child_space)
}

/// Case 6 after the start: `process_space` is refused, and the `READ`-only
/// handle from before it reads the running child and maps nothing.
fn after_the_start(
    scratch: &Scratch<'_>,
    child: Handle,
    reader: Handle,
    report: &mut Report,
) -> Result<(), &'static str> {
    let same = u64::from(SAME_RIGHTS);
    let _ = call(nr::PROCESS_START, &[reg(child), 0])
        .map_err(|_| "space self-check failed: the child could not be started")?;
    refused(
        call(nr::PROCESS_SPACE, &[reg(child), same]),
        status::BAD_STATE,
        "space self-check failed: process_space answered after the process started",
        report,
    )?;
    // The creator's READ-only handle from before the start reads the running
    // child, and maps nothing.
    let text = arch::USER_SPIN_PROGRAM.len() as u64;
    if read(reader, image::ENTRY, text) != Ok(text as usize)
        || scratch.get(BUFFER, text as usize)? != arch::USER_SPIN_PROGRAM
    {
        return Err(
            "space self-check failed: a pre-start READ handle did not read the running \
                    child's text",
        );
    }
    let vmo = made(
        nr::VMO_CREATE,
        &[PAGE_SIZE],
        "space self-check failed: vmo_create failed",
    )?;
    refused(
        map(reader, vmo, AT_OWNED + PAGE_SIZE, MAP_READ),
        status::ACCESS_DENIED,
        "space self-check failed: a READ-only space handle mapped into a running child",
        report,
    )?;
    for closing in [vmo, reader] {
        let _ = call(nr::HANDLE_CLOSE, &[reg(closing)]);
    }
    Ok(())
}

/// The child's process handle and its full space handle, from case 6 to
/// case 7.
static CHILD: SpinLock<Option<(Handle, Handle)>> = SpinLock::new(None);

/// Case 6's charging: the owner pays, the caller does not, and at the
/// owner's limit the read is refused with nothing killed.
fn owner_pays(
    setup: &Setup,
    scratch: &Scratch<'_>,
    space: Handle,
    report: &mut Report,
) -> Result<(), &'static str> {
    let vmo = made(
        nr::VMO_CREATE,
        &[2 * PAGE_SIZE],
        "space self-check failed: vmo_create failed",
    )?;
    scratch.put(OFFSET, &0_u64.to_ne_bytes())?;
    let mapped = call(
        nr::SPACE_MAP,
        &[
            reg(space),
            reg(vmo),
            AT_OWNED,
            2 * PAGE_SIZE,
            u64::from(MAP_READ | MAP_WRITE),
            OFFSET,
        ],
    );
    if mapped != Ok(AT_OWNED as usize) {
        return Err("space self-check failed: a VMO was not mapped into a created process");
    }
    let (owner_before, caller_before) = (memory(&setup.owner)?, memory(&setup.caller)?);
    if read(space, AT_OWNED, 8) != Ok(8) {
        return Err("space self-check failed: a by-handle read of an untouched page failed");
    }
    let (owner_after, caller_after) = (memory(&setup.owner)?, memory(&setup.caller)?);
    if owner_after < owner_before + PAGE_SIZE {
        return Err(
            "space self-check failed: a page a by-handle read faulted in was not charged \
                    to the space's owner",
        );
    }
    if caller_after != caller_before {
        return Err(
            "space self-check failed: a page a by-handle read faulted in was charged to \
                    the caller",
        );
    }
    report.owner_bytes = owner_after - owner_before;
    // At the owner's limit: refused, nothing killed, the caller untouched.
    let kills = super::oom::kills();
    let _ = setup.owner.set_limit(Resource::Memory, owner_after);
    refused(
        read(space, AT_OWNED + PAGE_SIZE, 8),
        status::NO_MEMORY,
        "space self-check failed: a by-handle read past the owner's limit was not NO_MEMORY",
        report,
    )?;
    let _ = setup.owner.set_limit(Resource::Memory, u64::MAX);
    if super::oom::kills() != kills || memory(&setup.caller)? != caller_before {
        return Err(
            "space self-check failed: a by-handle read at the owner's limit killed or \
                    charged the caller",
        );
    }
    let _ = call(nr::HANDLE_CLOSE, &[reg(vmo)]);
    Ok(())
}

/// Case 7: the running child's text unmapped through the creator's handle
/// from before the start; the child faults wherever it runs.
fn a_running_childs_text_unmapped(
    _setup: &Setup,
    scratch: &Scratch<'_>,
    report: &mut Report,
) -> Result<(), &'static str> {
    let (child, full) = CHILD
        .lock()
        .take()
        .ok_or("space self-check failed: no child to unmap")?;
    let text = image::ENTRY & !(PAGE_SIZE - 1);
    let _ = call(nr::SPACE_UNMAP, &[reg(full), text, PAGE_SIZE])
        .map_err(|_| "space self-check failed: a running child's text could not be unmapped")?;
    report.changes += 1;
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    scratch.put(STATUS, &[0; 8])?;
    loop {
        let _ = call(nr::PROCESS_STATUS, &[reg(child), STATUS])
            .map_err(|_| "space self-check failed: process_status failed")?;
        let bytes = scratch.get(STATUS, 8)?;
        let mut state = [0_u8; 4];
        let mut value = [0_u8; 4];
        state.copy_from_slice(bytes.get(..4).unwrap_or(&[0; 4]));
        value.copy_from_slice(bytes.get(4..8).unwrap_or(&[0; 4]));
        let found = ProcessStatus {
            state: u32::from_ne_bytes(state),
            value: u32::from_ne_bytes(value),
        };
        if found.state == PROCESS_KILLED && found.value == 11 {
            break;
        }
        if found.state != 0 {
            return Err(
                "space self-check failed: a child whose text was unmapped ended other \
                        than by SIGSEGV",
            );
        }
        if crate::timer::now_nanos() >= deadline {
            return Err(
                "space self-check failed: a running child whose text was unmapped by \
                        handle kept running",
            );
        }
        crate::sched::sleep_for(POLL_NANOS);
    }
    for closing in [full, child] {
        let _ = call(nr::HANDLE_CLOSE, &[reg(closing)]);
    }
    Ok(())
}
