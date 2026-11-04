//! Regions, shared processes and threads (`docs/microkernel/K11.md`), and the
//! parts of restricted mode (`docs/microkernel/K1.md`) that are ordinary calls.
//!
//! `RESTRICTED_ENTER`, `RESTRICTED_RESUME` and `RESTRICTED_CONTINUE` answer in
//! more registers than a [`Syscall`] can see: `ferrix_rt::restricted` makes
//! those.

use ferrix_native_abi::nr;
use ferrix_native_abi::rights::Requested;

pub use ferrix_native_abi::restricted::{
    CONTEXT_BYTES, Context, ENTER_ALL_CALLS_EXIT, ENTER_COPY_MISS_EXITS, FaultKind, HANDLE_KEEP,
    Reason, SHARED_BASE, VECTORS_BYTES,
};

use crate::Handle;
use crate::call::{Call, Syscall};
use crate::error::{Error, decode, decode_handle, decode_unit};
use crate::handle::{Object, OwnedHandle, object_handle, register, rights_register};
use crate::job::Job;
use crate::pending::Process;
use crate::space::Space;

object_handle!(
    /// A partition's shared half and its shared handle table.
    Region
);

object_handle!(
    /// A thread, which [`Thread::kick`] takes out of restricted mode.
    Thread
);

impl<S: Syscall> Thread<S> {
    /// `restricted_kick`: exit this thread from restricted mode, now or at its
    /// next entry.
    ///
    /// # Errors
    ///
    /// [`Error::AccessDenied`] without `SIGNAL`; [`Error::BadState`] if the
    /// thread has ended or belongs to no region.
    pub fn kick(&self) -> Result<(), Error> {
        decode_unit(
            Call::new(nr::RESTRICTED_KICK)
                .value(register(self.handle()))
                .make(self.syscall()),
        )
    }
}

/// `thread_create`: a thread of the caller's own process, in normal mode.
///
/// # Errors
///
/// [`Error::InvalidArgs`] for an entry or stack outside the caller's reach;
/// [`Error::BadState`] from restricted mode; the job's task limit as
/// [`Error::ShouldWait`].
pub fn thread_create<S: Syscall>(
    syscall: S,
    entry: usize,
    stack: usize,
    argument: usize,
) -> Result<Thread<S>, Error> {
    let owned = decode_handle(
        Call::new(nr::THREAD_CREATE)
            .value(entry)
            .value(stack)
            .value(argument)
            .make(syscall),
    )?;
    Ok(Thread::from_owned(OwnedHandle::from_raw(syscall, owned)))
}

/// `process_start_shared`: start `process`, a process of a region, at `entry`
/// on `stack`, in normal mode, with `bootstrap`, if any, moved to the shared
/// table and its value in the first argument register.
///
/// # Errors
///
/// [`Error::InvalidArgs`] for an entry or stack out of reach;
/// [`Error::BadState`] for a process of no region, or started.
pub fn process_start_shared<S: Syscall>(
    process: &Process<S>,
    entry: usize,
    stack: usize,
    bootstrap: Option<OwnedHandle<S>>,
) -> Result<(), Error> {
    let value = bootstrap.map_or(0, |handle| register(handle.into_raw()));
    decode_unit(
        Call::new(nr::PROCESS_START_SHARED)
            .value(register(process.handle()))
            .value(entry)
            .value(stack)
            .value(value)
            .make(process.syscall()),
    )
}

/// `thread_exit`: end the calling thread, or its process with `status`. It
/// comes back only if refused.
///
/// # Errors
///
/// [`Error::BadState`] from restricted mode, where the program's own exit is a
/// Linux call for `linuxd` to answer.
pub fn thread_exit<S: Syscall>(syscall: S, status: i32, whole_process: bool) -> Result<(), Error> {
    decode_unit(
        Call::new(nr::THREAD_EXIT)
            .value(status as u32 as usize)
            .value(usize::from(whole_process))
            .make(syscall),
    )
}

/// `thread_self`: a handle to the calling thread.
///
/// # Errors
///
/// [`Error::BadState`] from restricted mode; [`Error::NoHandles`].
pub fn thread_self<S: Syscall>(syscall: S) -> Result<Thread<S>, Error> {
    let owned = decode_handle(Call::new(nr::THREAD_SELF).make(syscall))?;
    Ok(Thread::from_owned(OwnedHandle::from_raw(syscall, owned)))
}

/// `handle_to_shared`: move a handle of this process's private table to the
/// shared table, with `rights`, a subset of what it holds. The number is
/// in the private table, so it comes as a [`Handle`], never as an owned one.
///
/// # Errors
///
/// [`Error::BadHandle`] for a number not in the private table;
/// [`Error::AccessDenied`] without `TRANSFER`, or `DUPLICATE` under
/// [`HANDLE_KEEP`]; [`Error::InvalidArgs`] for wider rights.
pub fn handle_to_shared<S: Syscall>(
    syscall: S,
    private: Handle,
    rights: Requested,
    options: usize,
) -> Result<Handle, Error> {
    decode_handle(
        Call::new(nr::HANDLE_TO_SHARED)
            .value(register(private))
            .value(rights_register(rights))
            .value(options)
            .make(syscall),
    )
}

/// `handle_to_private`: move a handle of the shared table to this process's
/// private one; the answer is a number there, which only restricted mode can
/// use.
///
/// # Errors
///
/// As [`handle_to_shared`].
pub fn handle_to_private<S: Syscall>(
    syscall: S,
    shared: Handle,
    rights: Requested,
) -> Result<Handle, Error> {
    decode_handle(
        Call::new(nr::HANDLE_TO_PRIVATE)
            .value(register(shared))
            .value(rights_register(rights))
            .make(syscall),
    )
}

/// `restricted_context_read`: the program's registers as the last exit left
/// them, the flags normalised so that an entry accepts them.
///
/// # Errors
///
/// [`Error::BadState`] with no context held, or from restricted mode.
pub fn context_read<S: Syscall>(syscall: S) -> Result<Context, Error> {
    let mut bytes = [0_u8; CONTEXT_BYTES];
    decode_unit(
        Call::new(nr::RESTRICTED_CONTEXT_READ)
            .output(&mut bytes)
            .make(syscall),
    )?;
    Ok(Context::from_bytes(&bytes))
}

/// `restricted_vectors_read`: the program's vector state, as the standard
/// image, into `out`, which must hold [`VECTORS_BYTES`].
///
/// # Errors
///
/// [`Error::BadState`] with no context held; [`Error::InvalidArgs`] for a
/// short `out`.
pub fn vectors_read<S: Syscall>(syscall: S, out: &mut [u8]) -> Result<usize, Error> {
    let length = out.len();
    decode(
        Call::new(nr::RESTRICTED_VECTORS_READ)
            .output(out)
            .value(length)
            .make(syscall),
    )
}

/// `restricted_vectors_write`: replace the program's vector state with the
/// image `image`, which the kernel checks.
///
/// # Errors
///
/// [`Error::InvalidArgs`] for an image the processor would refuse.
pub fn vectors_write<S: Syscall>(syscall: S, image: &[u8]) -> Result<(), Error> {
    decode_unit(
        Call::new(nr::RESTRICTED_VECTORS_WRITE)
            .input(image)
            .value(image.len())
            .make(syscall),
    )
}

/// `restricted_copy_window`: the code of this region in `[start, end)` may
/// fault on the restricted half, and lands at `landing` if it does.
///
/// # Errors
///
/// [`Error::InvalidArgs`] for a range or landing outside the region.
pub fn copy_window<S: Syscall>(
    syscall: S,
    start: usize,
    end: usize,
    landing: usize,
) -> Result<(), Error> {
    decode_unit(
        Call::new(nr::RESTRICTED_COPY_WINDOW)
            .value(start)
            .value(end)
            .value(landing)
            .make(syscall),
    )
}

/// `region_create`: a region whose processes live under `job`, and a handle
/// to its space, to map `linuxd` into.
///
/// # Errors
///
/// [`Error::AccessDenied`] without `MANAGE` on the job; [`Error::NoMemory`].
pub fn region_create<S: Syscall>(job: &Job<S>) -> Result<(Region<S>, Space<S>), Error> {
    let sys = job.syscall();
    let mut space = [0_u8; 8];
    let handle = decode_handle(
        Call::new(nr::REGION_CREATE)
            .value(register(job.handle()))
            .output(&mut space)
            .make(sys),
    )?;
    let region = Region::from_owned(OwnedHandle::from_raw(sys, handle));
    let space = u32::try_from(u64::from_ne_bytes(space)).map_err(|_| Error::InvalidArgs)?;
    Ok((
        region,
        Space::from_owned(OwnedHandle::from_raw(sys, Handle(space))),
    ))
}

impl<S: Syscall> Region<S> {
    /// `process_create_shared`: a process of this region in `job`, unstarted,
    /// over a fresh restricted space, whose handle comes with it.
    ///
    /// # Errors
    ///
    /// [`Error::AccessDenied`] without `MANAGE` on the region and on the job;
    /// [`Error::BadState`] for a job outside the region's, or a region with no
    /// process left.
    pub fn process_create(
        &self,
        job: &Job<S>,
        name: &[u8],
    ) -> Result<(Process<S>, Space<S>), Error> {
        let sys = self.syscall();
        let mut out = [0_u8; 8];
        let handle = decode_handle(
            Call::new(nr::PROCESS_CREATE_SHARED)
                .value(register(self.handle()))
                .value(register(job.handle()))
                .value(0)
                .input(name)
                .value(name.len())
                .output(&mut out)
                .make(sys),
        )?;
        let process = Process::from_owned(OwnedHandle::from_raw(sys, handle));
        let space = u32::try_from(u64::from_ne_bytes(out)).map_err(|_| Error::InvalidArgs)?;
        Ok((
            process,
            Space::from_owned(OwnedHandle::from_raw(sys, Handle(space))),
        ))
    }

    /// `process_create_shared` over `space`, which it consumes: from
    /// `space_clone`, this is `fork`.
    ///
    /// # Errors
    ///
    /// As [`Region::process_create`]; [`Error::BadState`] for a space already
    /// bound.
    pub fn process_create_over(
        &self,
        job: &Job<S>,
        space: Space<S>,
        name: &[u8],
    ) -> Result<Process<S>, Error> {
        let sys = self.syscall();
        let mut out = [0_u8; 8];
        let handle = decode_handle(
            Call::new(nr::PROCESS_CREATE_SHARED)
                .value(register(self.handle()))
                .value(register(job.handle()))
                .value(register(space.handle()))
                .input(name)
                .value(name.len())
                .output(&mut out)
                .make(sys),
        )?;
        drop(space);
        Ok(Process::from_owned(OwnedHandle::from_raw(sys, handle)))
    }
}

/// `job_declare_buffer`: `vmo` is a buffer lower levels may write and `job`'s level
/// may map (`docs/microkernel/K17.md`).
///
/// # Errors
///
/// [`Error::AccessDenied`] without `SET_LIMIT` on the job or `MAP` on the VMO;
/// [`Error::BadState`] for one declared before or with a pager.
pub fn job_declare_buffer<S: Syscall>(job: &Job<S>, vmo: &crate::vmo::Vmo<S>) -> Result<(), Error> {
    decode_unit(
        Call::new(nr::JOB_DECLARE_BUFFER)
            .value(register(job.handle()))
            .value(register(vmo.handle()))
            .make(job.syscall()),
    )
}
