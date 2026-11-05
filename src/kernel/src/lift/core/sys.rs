//! The native calls under the shim, in one place (docs/microkernel/M5-LIFT.md §2).

use core::sync::atomic::{AtomicU32, Ordering};

use ferrix_native::address::{self, Word};
use ferrix_native::handle::Deadline;
use ferrix_native::handle::{Object as _, OwnedHandle};
use ferrix_native::lift_calls;
use ferrix_native::region;
use ferrix_native::space::{Access, Space};
use ferrix_native_abi::handle::Handle;
use ferrix_rt::Kernel;

/// Sleep on `word` while it reads 0, until `deadline` (u64::MAX: no limit).
pub fn park(word: &AtomicU32, deadline: u64) {
    let at = core::ptr::from_ref(word) as usize;
    let until = if deadline == u64::MAX {
        Deadline::Never
    } else {
        Deadline::At(deadline)
    };
    crate::sched::wakes_before_stopping();
    let since = crate::sched::account::parking();
    let answer = address::wait(
        Kernel,
        Word::At {
            address: at,
            shared: true,
        },
        0,
        address::MATCH_ANY,
        until,
    );
    crate::sched::account::unparked(since);
    match answer {
        Ok(())
        | Err(ferrix_native::error::Error::TimedOut | ferrix_native::error::Error::ShouldWait) => {}
        Err(why) => crate::println!("linuxd: park: {why:?}"),
    }
}
/// Wake whoever sleeps on `word`.
pub fn unpark(word: &AtomicU32) {
    let at = core::ptr::from_ref(word) as usize;
    let _ = address::wake(
        Kernel,
        Word::At {
            address: at,
            shared: true,
        },
        u32::MAX,
        address::MATCH_ANY,
    );
}
pub fn thread_yield() {
    crate::sched::wakes_before_stopping();
    let _ = lift_calls::thread_yield(Kernel);
}
/// A thread in the calling process (K11's `THREAD_CREATE`).
pub fn thread_create(entry: usize, stack: usize) -> Result<u32, ()> {
    let thread = region::thread_create(Kernel, entry, stack, 0).map_err(|why| {
        crate::println!("linuxd: the kernel refused a thread: {why:?}");
    })?;
    let handle = thread.handle().0;
    core::mem::forget(thread);
    Ok(handle)
}
pub fn thread_exit() -> ! {
    crate::sched::wakes_before_stopping();
    loop {
        let _ = region::thread_exit(Kernel, 0, false);
        core::hint::spin_loop();
    }
}
pub fn kick(thread: u32) {
    if thread != 0 {
        let _ = core::mem::ManuallyDrop::new(region::Thread::from_owned(OwnedHandle::from_raw(
            Kernel,
            Handle(thread),
        )))
        .kick();
    }
}
fn space(handle: u32) -> core::mem::ManuallyDrop<Space<Kernel>> {
    core::mem::ManuallyDrop::new(Space::from_owned(OwnedHandle::from_raw(
        Kernel,
        Handle(handle),
    )))
}
pub fn space_unmap(handle: u32, virt: u64, len: u64) -> bool {
    space(handle).unmap(virt as usize, len as usize).is_ok()
}
/// An arena page into a program's space; the two arena handles are `arena`'s.
pub fn space_fill(handle: u32, page: u64, physical: u64, write: bool, execute: bool) -> bool {
    let frame = physical / 4096;
    let (vmo, offset) = if crate::mm::is_foreign(frame) {
        // A page of a VMO a driver holds, a card's buffer: its own VMO, no arena.
        let Some((vmo, index)) = crate::mm::foreign_page(frame) else {
            return false;
        };
        (vmo, index * 4096)
    } else if execute {
        (arena::executable_handle(), physical)
    } else {
        (arena::writable_handle(), physical)
    };
    place(handle, vmo, &[(page, offset, write, execute)]) == 1
}
/// `SPACE_PLACE`: how many went in.
pub fn place(space: u32, vmo: u32, entries: &[(u64, u64, bool, bool)]) -> usize {
    use ferrix_native_abi::lift::{PLACE_EXECUTE, PLACE_MOST, PLACE_WRITE};
    let mut bytes = [0_u8; PLACE_MOST * 16];
    for (out, &(page, offset, write, execute)) in bytes.chunks_exact_mut(16).zip(entries) {
        let flags = if write { PLACE_WRITE } else { 0 } | if execute { PLACE_EXECUTE } else { 0 };
        let (first, second) = out.split_at_mut(8);
        first.copy_from_slice(&(page | flags).to_le_bytes());
        second.copy_from_slice(&offset.to_le_bytes());
    }
    let length = entries.len().min(PLACE_MOST) * 16;
    match lift_calls::space_place(Kernel, Handle(space), Handle(vmo), &bytes[..length]) {
        Ok(done) => done,
        Err(why) => {
            crate::println!("linuxd: the kernel refused a page: {why:?}");
            0
        }
    }
}
/// The arena's VMO, by handle: one to write it, one that may map it executable.
pub mod arena {
    use core::sync::atomic::{AtomicU32, Ordering};
    use ferrix_native::handle::OwnedHandle;
    use ferrix_native::vmo::Vmo;
    use ferrix_native_abi::handle::Handle;
    use ferrix_rt::Kernel;
    static WRITABLE: AtomicU32 = AtomicU32::new(0);
    static EXECUTABLE: AtomicU32 = AtomicU32::new(0);
    pub fn set(writable: u32, executable: u32) {
        WRITABLE.store(writable, Ordering::Release);
        EXECUTABLE.store(executable, Ordering::Release);
    }
    fn vmo(handle: u32) -> core::mem::ManuallyDrop<Vmo<Kernel>> {
        core::mem::ManuallyDrop::new(Vmo::from_owned(OwnedHandle::from_raw(
            Kernel,
            Handle(handle),
        )))
    }
    pub fn writable_handle() -> u32 {
        WRITABLE.load(Ordering::Acquire)
    }
    pub fn executable_handle() -> u32 {
        EXECUTABLE.load(Ordering::Acquire)
    }
    pub fn writable() -> core::mem::ManuallyDrop<Vmo<Kernel>> {
        vmo(WRITABLE.load(Ordering::Acquire))
    }
    pub fn executable() -> core::mem::ManuallyDrop<Vmo<Kernel>> {
        vmo(EXECUTABLE.load(Ordering::Acquire))
    }
}

/// `THREAD_RESERVE` on the partition's job.
#[cfg(ferrix_schedule = "edf")]
pub fn thread_reserve(
    runtime: u64,
    deadline: u64,
    period: u64,
) -> Result<(), crate::sched::reserve::Refusal> {
    use crate::sched::reserve::Refusal;
    use ferrix_native::Error;
    lift_calls::thread_reserve(
        Kernel,
        Handle(JOB.load(Ordering::Acquire)),
        runtime,
        deadline,
        period,
    )
    .map_err(|why| match why {
        Error::InvalidArgs => Refusal::Malformed,
        Error::BadState => Refusal::Already,
        Error::AccessDenied | Error::BadHandle | Error::WrongType => Refusal::Denied,
        _ => Refusal::Unschedulable,
    })
}

/// The clock (`CLOCK_READ`): 0 is monotonic, 1 realtime.
pub fn clock(which: usize) -> u64 {
    lift_calls::clock_read(Kernel, which).unwrap_or(0)
}
pub fn debug_write(bytes: &[u8]) {
    for piece in bytes.chunks(4096) {
        let _ =
            lift_calls::debug_write(Kernel, Handle(DEBUG_HANDLE.load(Ordering::Relaxed)), piece);
    }
}
static DEBUG_HANDLE: AtomicU32 = AtomicU32::new(0);
pub fn set_debug_handle(handle: u32) {
    DEBUG_HANDLE.store(handle, Ordering::Relaxed);
}

/// The partition's region and job, in the shared table (the boot page's).
static REGION: AtomicU32 = AtomicU32::new(0);
static JOB: AtomicU32 = AtomicU32::new(0);
pub fn set_partition(region: u32, job: u32) {
    REGION.store(region, Ordering::Release);
    JOB.store(job, Ordering::Release);
}
/// A kernel process of the region, for a Linux process: answers it and its
/// restricted space's handles, the space's for `fill`.
pub fn process_new(name: &[u8]) -> Result<(u32, u32), ()> {
    let region = core::mem::ManuallyDrop::new(region::Region::from_owned(OwnedHandle::from_raw(
        Kernel,
        Handle(REGION.load(Ordering::Acquire)),
    )));
    let job = core::mem::ManuallyDrop::new(ferrix_native::job::Job::from_owned(
        OwnedHandle::from_raw(Kernel, Handle(JOB.load(Ordering::Acquire))),
    ));
    let (process, space) = region.process_create(&job, name).map_err(|why| {
        crate::println!("linuxd: a process could not be made: {why:?}");
    })?;
    if let Err(why) = lift_calls::space_window(Kernel, space.handle()) {
        crate::println!("linuxd: a process's space is no window: {why:?}");
    }
    let handles = (process.handle().0, space.handle().0);
    core::mem::forget(process);
    core::mem::forget(space);
    Ok(handles)
}
static KERNEL_JOBS: crate::sync::SpinLock<alloc::vec::Vec<(usize, u32)>> =
    crate::sync::SpinLock::new(alloc::vec::Vec::new());
/// The kernel job standing for a cgroup's job, by its address here.
pub fn kernel_job_of(job: usize) -> Option<u32> {
    let mut jobs = KERNEL_JOBS.lock();
    if let Some((_, handle)) = jobs.iter().find(|(at, _)| *at == job) {
        return Some(*handle);
    }
    let parent = core::mem::ManuallyDrop::new(ferrix_native::job::Job::from_owned(
        OwnedHandle::from_raw(Kernel, Handle(JOB.load(Ordering::Acquire))),
    ));
    let child = parent
        .create_child()
        .map_err(|why| {
            crate::println!("linuxd: a cgroup's job could not be made: {why:?}");
        })
        .ok()?;
    let handle = child.handle().0;
    core::mem::forget(child);
    jobs.push((job, handle));
    Some(handle)
}
/// A cgroup's job was killed: so is the kernel job standing for it, if one does, or a
/// native service in it outlives its stop (ledger 6326, P8).
pub fn kill_kernel_job(job: usize) {
    let found = KERNEL_JOBS
        .lock()
        .iter()
        .find(|(at, _)| *at == job)
        .map(|(_, handle)| *handle);
    if let Some(handle) = found {
        let job = core::mem::ManuallyDrop::new(ferrix_native::job::Job::from_owned(
            OwnedHandle::from_raw(Kernel, Handle(handle)),
        ));
        if let Err(why) = job.kill() {
            crate::println!("linuxd: a cgroup's kernel job would not die: {why:?}");
        }
    }
}
/// A cgroup's job has gone: its kernel job's handle goes with it, so that a new job at
/// the same address gets its own (ledger 6542, item 7).
pub fn forget_kernel_job(job: usize) {
    let mut jobs = KERNEL_JOBS.lock();
    if let Some(at) = jobs.iter().position(|(at, _)| *at == job) {
        let (_, handle) = jobs.swap_remove(at);
        drop(ferrix_native::job::Job::from_owned(OwnedHandle::from_raw(
            Kernel,
            Handle(handle),
        )));
    }
}
/// Another handle to a kernel job, to give away.
pub fn duplicate_job(handle: u32) -> Result<u32, ferrix_native::error::Error> {
    let job = core::mem::ManuallyDrop::new(ferrix_native::job::Job::from_owned(
        OwnedHandle::from_raw(Kernel, Handle(handle)),
    ));
    let copy = job
        .as_owned()
        .duplicate(ferrix_native_abi::rights::Requested::Same)?;
    let raw = copy.raw().0;
    core::mem::forget(copy);
    Ok(raw)
}
/// Start a kernel process's first thread in linuxd's code.
pub fn process_start(process: u32, entry: usize, stack: usize) -> Result<(), ()> {
    let process = core::mem::ManuallyDrop::new(ferrix_native::pending::Process::from_owned(
        OwnedHandle::from_raw(Kernel, Handle(process)),
    ));
    region::process_start_shared(&*process, entry, stack, None).map_err(|why| {
        crate::println!("linuxd: a process could not start: {why:?}");
    })
}
/// The calling thread's own handle, for kicks.
pub fn thread_self() -> u32 {
    region::thread_self(Kernel).map_or(0, |thread| {
        let handle = thread.handle().0;
        core::mem::forget(thread);
        handle
    })
}

/// Let a handle go.
pub fn handle_close(handle: u32) {
    drop(OwnedHandle::from_raw(Kernel, Handle(handle)));
}
/// Whether a thread has gone: asked with no wait.
pub fn thread_ended(handle: u32) -> bool {
    let thread = core::mem::ManuallyDrop::new(region::Thread::from_owned(OwnedHandle::from_raw(
        Kernel,
        Handle(handle),
    )));
    thread
        .wait_one(
            ferrix_native_abi::signals::Signals::TERMINATED,
            Deadline::At(0),
        )
        .is_ok()
}

/// Whoever ends the partition (`reboot(2)`, init's end) wakes linuxd's first thread,
/// whose end the kernel's start of linuxd waits on.
pub static ENDING: AtomicU32 = AtomicU32::new(0);
pub fn end_partition() -> ! {
    ENDING.store(1, Ordering::SeqCst);
    unpark(&ENDING);
    thread_exit()
}
/// linuxd's first thread: until someone ends the partition.
pub fn wait_for_the_end() -> i32 {
    while ENDING.load(Ordering::SeqCst) == 0 {
        park(&ENDING, u64::MAX);
    }
    let ending = ENDING.load(Ordering::SeqCst);
    if ending == 1 { 0 } else { ending as i32 }
}
/// `reboot(2)`: linuxd ends with `status`, and the kernel does what it says.
pub fn end_partition_with(status: i32) -> ! {
    ENDING.store(status as u32, Ordering::SeqCst);
    unpark(&ENDING);
    thread_exit()
}

static CONSOLE_HANDLE: AtomicU32 = AtomicU32::new(0);
pub fn set_console_handle(handle: u32) {
    CONSOLE_HANDLE.store(handle, Ordering::Relaxed);
}
/// One byte of the console's input, if one is there.
pub fn console_byte() -> Option<u8> {
    let mut byte = [0_u8; 1];
    match lift_calls::console_read(
        Kernel,
        Handle(CONSOLE_HANDLE.load(Ordering::Relaxed)),
        &mut byte,
    ) {
        Ok(1) => Some(byte[0]),
        _ => None,
    }
}
