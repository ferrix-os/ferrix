//! Calls whose handlers are not on main.
//!
//! Each wrapper makes the real call. A kernel without the handler answers
//! `ENOSYS`, which arrives as [`Error::Unsupported`], so a program built
//! today fails cleanly on today's kernel and works unchanged on the one that
//! implements the call.
//!
//! All of them are stage 9's, owned by ferrix-4b.
//!
//! * **Process creation**, `0x1030` and `0x1031`. The numbers were copied here
//!   while `src/lib/proto/native-abi`'s table left `0x1030..=0x1037` free for them;
//!   they are on the table now and re-exported from it. The argument lists
//!   are the handler's: `(job, image_vmo, name_ptr, name_len)` and
//!   `(process, bootstrap or 0)`.
//! * **A process's bootstrap and its end**, `0x1032` to `0x1034`: init's K3
//!   and K6 (`docs/INIT.md` §16), `process_give`, `process_bootstrap` and
//!   `process_status`.
//! * **`vmo_map`**, `0x1024`, whose number is on main's table and whose handler
//!   and `MAP_READ`/`MAP_WRITE` constants are not. The constants are copied here
//!   until they land in `src/lib/proto/native-abi`'s `types`.

use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::types::{self, ProcessStatus};

use crate::call::{Call, Syscall};
use crate::error::{Error, decode, decode_handle, decode_unit};
use crate::handle::{Object, OwnedHandle, object_handle, register};
use crate::job::Job;
use crate::port::Port;
use crate::vmo::Vmo;

/// `process_create`: `(job, image_vmo, name_ptr, name_len)` → process handle.
pub use ferrix_native_abi::nr::PROCESS_CREATE;

/// `process_start`: `(process, bootstrap)`, the bootstrap a channel handle.
/// It leaves the caller, becomes the new process's first handle, and its value
/// arrives in the first argument register at `_start`, zero meaning none.
pub use ferrix_native_abi::nr::PROCESS_START;

object_handle!(
    /// A process, made by [`create_process`] and not necessarily started.
    Process
);

/// `process_create`: a process in `job`, from the ELF image in `elf`, named
/// `name`. Not started.
///
/// # Errors
///
/// [`Error::Unsupported`] on a kernel without the call;
/// [`Error::AccessDenied`] without `MANAGE` on `job`, or when the caller's
/// own job is sealed and `job` is outside its tree (`types::JOB_SEALED`).
pub fn create_process<S: Syscall>(
    job: &Job<S>,
    elf: &Vmo<S>,
    name: &str,
) -> Result<Process<S>, Error> {
    let value = Call::new(PROCESS_CREATE)
        .value(register(job.handle()))
        .value(register(elf.handle()))
        .input(name.as_bytes())
        .value(name.len())
        .make(job.syscall());
    let handle = decode_handle(value)?;
    Ok(Process::from_owned(OwnedHandle::from_raw(
        job.syscall(),
        handle,
    )))
}

impl<S: Syscall> Process<S> {
    /// `process_start`: run the process, giving it `bootstrap`.
    ///
    /// The handle leaves this process only if the call succeeds, so a failure
    /// gives it back.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] on a kernel without the call, with `bootstrap`.
    pub fn start(&self, bootstrap: OwnedHandle<S>) -> Result<(), (Error, OwnedHandle<S>)> {
        let value = Call::new(PROCESS_START)
            .value(register(self.handle()))
            .value(register(bootstrap.raw()))
            .make(self.syscall());
        match decode_unit(value) {
            Ok(()) => {
                let _ = bootstrap.into_raw();
                Ok(())
            }
            Err(error) => Err((error, bootstrap)),
        }
    }

    /// `process_status`: whether the process has ended, and how: its exit
    /// code or the signal that killed it.
    ///
    /// # Errors
    ///
    /// [`Error::AccessDenied`] without `WAIT`; [`Error::WrongType`].
    pub fn status(&self) -> Result<ProcessStatus, Error> {
        let mut out = [0_u8; 8];
        let value = Call::new(nr::PROCESS_STATUS)
            .value(register(self.handle()))
            .output(&mut out)
            .make(self.syscall());
        decode_unit(value)?;
        let [s0, s1, s2, s3, v0, v1, v2, v3] = out;
        Ok(ProcessStatus {
            state: u32::from_ne_bytes([s0, s1, s2, s3]),
            value: u32::from_ne_bytes([v0, v1, v2, v3]),
        })
    }

    /// Queue a packet carrying `key` on `port` when the process ends.
    ///
    /// No call of its own: `object_wait_async` for `TERMINATED`. The packet is
    /// a `PACKET_SIGNAL` whose signals include `TERMINATED`, and it arrives
    /// after the process's handles and descriptors are closed, or at once if
    /// the process has already ended.
    ///
    /// # Errors
    ///
    /// As [`Object::wait_async`].
    pub fn notify_on_exit(&self, port: &Port<S>, key: u64) -> Result<(), Error> {
        self.wait_async(port, Signals::TERMINATED, key)
    }
}

/// `devmgr_start`: ask the kernel to start `devmgr` in `job`, with the
/// starter the kernel gave pid 1 (`docs/INIT.md` §7.3, L12). The kernel
/// loads it and hands it its devices itself; what comes back is a handle to
/// the process, to wait on and read how it ended.
///
/// # Errors
///
/// [`Error::AlreadyBound`] while a `devmgr` it started lives,
/// [`Error::BadState`] after one ended and before its drivers have,
/// [`Error::AccessDenied`] or [`Error::WrongType`] for a handle that is not
/// the starter or a job it may manage.
pub fn start_devmgr<S: Syscall>(
    starter: &OwnedHandle<S>,
    job: &Job<S>,
) -> Result<Process<S>, Error> {
    let value = Call::new(nr::DEVMGR_START)
        .value(register(starter.raw()))
        .value(register(job.handle()))
        .make(job.syscall());
    let handle = decode_handle(value)?;
    Ok(Process::from_owned(OwnedHandle::from_raw(
        job.syscall(),
        handle,
    )))
}

/// What one [`audit_read`] answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditRead {
    /// Records copied to the front of the buffer, 64 bytes each.
    pub copied: usize,
    /// The number to read from next.
    pub next: u64,
    /// Records between the number asked for and the first copied that the
    /// ring no longer held.
    pub lost: u64,
    /// The boot's audit id.
    pub id: u128,
}

/// `audit_read`: copy records of the audit ring `which`
/// (`types::AUDIT_HIGH`, `AUDIT_REFUSALS`, `AUDIT_BOOT`) numbered `from` or
/// later into `buffer`, as many whole 64-byte records as fit, at most
/// `nr::AUDIT_READ_MAX` (`docs/certification/AUDIT.md` §4).
///
/// # Errors
///
/// [`Error::AccessDenied`] without `READ`, [`Error::WrongType`] for a
/// handle that is not the audit record's, [`Error::InvalidArgs`] for a
/// `which` that names no ring.
pub fn audit_read<S: Syscall>(
    sys: S,
    audit: &OwnedHandle<S>,
    which: u64,
    from: u64,
    buffer: &mut [u8],
) -> Result<AuditRead, Error> {
    let count = (buffer.len() / 64).min(nr::AUDIT_READ_MAX as usize);
    let mut answer = [0_u8; types::AUDIT_ANSWER_WORDS * 8];
    if let Some(slot) = answer.get_mut(8..16) {
        slot.copy_from_slice(&from.to_ne_bytes());
    }
    decode_unit(
        Call::new(nr::AUDIT_READ)
            .value(register(audit.raw()))
            .value(which as usize)
            .output(buffer)
            .value(count)
            .output(&mut answer)
            .make(sys),
    )?;
    let word = |at: usize| {
        answer
            .get(at * 8..at * 8 + 8)
            .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
            .map_or(0, u64::from_ne_bytes)
    };
    Ok(AuditRead {
        copied: usize::try_from(word(0)).unwrap_or(0),
        next: word(1),
        lost: word(2),
        id: u128::from(word(3)) | (u128::from(word(4)) << 64),
    })
}

/// `process_give`: move `bootstrap` into the bootstrap slot of the caller's
/// child `pid`, which has not yet completed an `execve`. The handle leaves
/// this process only if the call succeeds.
///
/// # Errors
///
/// [`Error::NoProcess`], [`Error::NotChild`], [`Error::AlreadyBound`] for a
/// child given one before, [`Error::BadState`] for one past its `execve` or
/// ended, [`Error::AccessDenied`] without `TRANSFER`; with `bootstrap`.
pub fn give_bootstrap<S: Syscall>(
    sys: S,
    pid: u32,
    bootstrap: OwnedHandle<S>,
) -> Result<(), (Error, OwnedHandle<S>)> {
    let value = Call::new(nr::PROCESS_GIVE)
        .value(pid as usize)
        .value(register(bootstrap.raw()))
        .make(sys);
    match decode_unit(value) {
        Ok(()) => {
            let _ = bootstrap.into_raw();
            Ok(())
        }
        Err(error) => Err((error, bootstrap)),
    }
}

/// `process_bootstrap`: this process's bootstrap handle, the first time;
/// `None` after that, or when it was given none.
///
/// # Errors
///
/// [`Error::NoHandles`] with a full table, and the handle is kept for a later
/// call.
pub fn take_bootstrap<S: Syscall>(sys: S) -> Result<Option<OwnedHandle<S>>, Error> {
    let value = decode(Call::new(nr::PROCESS_BOOTSTRAP).make(sys))?;
    match u32::try_from(value) {
        Ok(0) => Ok(None),
        Ok(raw) => Ok(Some(OwnedHandle::from_raw(sys, Handle(raw)))),
        Err(_) => Err(Error::Unexpected(value)),
    }
}

/// `vmo_map`'s protection bit for a readable mapping.
///
/// Owned by stage 9 (ferrix-4b), who adds it to `src/lib/proto/native-abi`'s `types`
/// with the handler; copied here until then.
pub const MAP_READ: u32 = 1;

/// `vmo_map`'s protection bit for a writable mapping, only ever with
/// [`MAP_READ`].
///
/// Owned by stage 9 (ferrix-4b), as [`MAP_READ`].
pub const MAP_WRITE: u32 = 2;

/// What a mapping of a VMO may do.
///
/// The only two the handler accepts, so no other can be asked for: every
/// mapping is readable, none is executable, and a writable one needs `WRITE`
/// on the handle as well as `READ` and `MAP`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protection {
    /// `MAP_READ`.
    Read,
    /// `MAP_READ | MAP_WRITE`.
    ReadWrite,
}

impl Protection {
    /// The register value.
    #[must_use]
    pub const fn register(self) -> usize {
        match self {
            Protection::Read => MAP_READ as usize,
            Protection::ReadWrite => (MAP_READ | MAP_WRITE) as usize,
        }
    }
}

impl<S: Syscall> Vmo<S> {
    /// `vmo_map` (0x1024): map `length` bytes of the VMO from `offset`, both
    /// whole pages, at `at` or wherever the kernel finds room, and return the
    /// address.
    ///
    /// The mapping is always shared, outlives the handle, and refuses Linux's
    /// `mremap` and `mprotect`. Safe at a fixed address because the handler
    /// refuses a range overlapping any mapping: it can add memory to this
    /// process, never replace memory it has.
    ///
    /// # Errors
    ///
    /// [`Error::WrongType`] for a handle that is not a VMO;
    /// [`Error::AccessDenied`] without `MAP` and `READ`, or `WRITE` for
    /// [`Protection::ReadWrite`]; [`Error::InvalidArgs`] for a length or
    /// offset that is not whole pages, a range past the VMO's end, or an
    /// address overlapping a mapping; [`Error::NoMemory`] with no address
    /// space left; [`Error::Unsupported`] until the handler lands.
    pub fn map(
        &self,
        at: Option<usize>,
        length: usize,
        protection: Protection,
        offset: u64,
    ) -> Result<usize, Error> {
        let offset = offset.to_ne_bytes();
        let value = Call::new(nr::VMO_MAP)
            .value(register(self.handle()))
            .value(at.unwrap_or(0))
            .value(length)
            .value(protection.register())
            .input(&offset)
            .make(self.syscall());
        decode(value)
    }
}
