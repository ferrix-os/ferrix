//! Address spaces by handle (`docs/microkernel/K2.md`).
//!
//! A [`Space`] names an address space: one made empty by [`create`], or a
//! process's own, taken by its creator before it starts with
//! [`Process::space`]. Mapping into it, protecting, unmapping and reading
//! out of it each need a right on the handle (`MAP`, `READ`), and an
//! executable mapping needs `EXECUTE` on the space handle and on the VMO
//! handle, which only [`Vmo::executable`] gives.

use ferrix_native_abi::nr;
use ferrix_native_abi::rights::Requested;
use ferrix_native_abi::types::{MAP_EXECUTE, MAP_READ, MAP_WRITE};

use crate::call::{Call, Syscall};
use crate::error::{Error, decode, decode_handle, decode_unit};
use crate::handle::{Object, OwnedHandle, object_handle, register, rights_register};
use crate::pending::Process;
use crate::vmo::Vmo;

object_handle!(
    /// An address space.
    Space
);

/// What a mapping by handle may do. Never writable and executable: the
/// kernel refuses that whatever the rights (ASR-2), so it cannot be asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// `MAP_READ`.
    Read,
    /// `MAP_READ | MAP_WRITE`.
    ReadWrite,
    /// `MAP_READ | MAP_EXECUTE`.
    ReadExecute,
}

impl Access {
    /// The register value.
    #[must_use]
    pub const fn register(self) -> usize {
        match self {
            Access::Read => MAP_READ as usize,
            Access::ReadWrite => (MAP_READ | MAP_WRITE) as usize,
            Access::ReadExecute => (MAP_READ | MAP_EXECUTE) as usize,
        }
    }
}

/// `space_create` (0x1079): a new, empty address space, owned by the
/// caller's job.
///
/// # Errors
///
/// [`Error::NoMemory`]; [`Error::NoHandles`].
pub fn create<S: Syscall>(sys: S) -> Result<Space<S>, Error> {
    let handle = decode_handle(Call::new(nr::SPACE_CREATE).make(sys))?;
    Ok(Space::from_owned(OwnedHandle::from_raw(sys, handle)))
}

impl<S: Syscall> Space<S> {
    /// `space_map` (0x107A): map `length` bytes of `vmo` from `offset`, both
    /// whole pages, at `at` or wherever the kernel finds room, and return the
    /// address. Shared, as `vmo_map`'s.
    ///
    /// # Errors
    ///
    /// [`Error::AccessDenied`] without `MAP` on the space, `MAP` and `READ`
    /// on the VMO, `WRITE` on the VMO for [`Access::ReadWrite`], or
    /// `EXECUTE` on both for [`Access::ReadExecute`]; [`Error::InvalidArgs`]
    /// for a range that is not whole pages, past the VMO's end, or over a
    /// mapping.
    pub fn map(
        &self,
        vmo: &Vmo<S>,
        at: Option<usize>,
        length: usize,
        access: Access,
        offset: u64,
    ) -> Result<usize, Error> {
        let offset = offset.to_ne_bytes();
        let value = Call::new(nr::SPACE_MAP)
            .value(register(self.handle()))
            .value(register(vmo.handle()))
            .value(at.unwrap_or(0))
            .value(length)
            .value(access.register())
            .input(&offset)
            .make(self.syscall());
        decode(value)
    }

    /// `space_unmap` (0x107B): unmap whatever lies in `length` bytes at
    /// `at`, both whole pages.
    ///
    /// # Errors
    ///
    /// [`Error::AccessDenied`] without `MAP`; [`Error::InvalidArgs`].
    pub fn unmap(&self, at: usize, length: usize) -> Result<(), Error> {
        let value = Call::new(nr::SPACE_UNMAP)
            .value(register(self.handle()))
            .value(at)
            .value(length)
            .make(self.syscall());
        decode_unit(value)
    }

    /// `space_protect` (0x107C): change the protection of a range every page
    /// of which a handle mapped, within what each region's handle allowed.
    ///
    /// # Errors
    ///
    /// [`Error::AccessDenied`] without `MAP` (and `EXECUTE` for
    /// [`Access::ReadExecute`]) or past a region's ceiling;
    /// [`Error::BadState`] for a range that reaches a region no handle
    /// mapped; [`Error::InvalidArgs`].
    pub fn protect(&self, at: usize, length: usize, access: Access) -> Result<(), Error> {
        let value = Call::new(nr::SPACE_PROTECT)
            .value(register(self.handle()))
            .value(at)
            .value(length)
            .value(access.register())
            .make(self.syscall());
        decode_unit(value)
    }

    /// `space_read` (0x107E): copy from `at` in the space into `buffer`, at
    /// most `nr::SPACE_COPY_MAX` bytes, and return how many were copied:
    /// short at the first page that cannot be read.
    ///
    /// # Errors
    ///
    /// [`Error::AccessDenied`] without `READ`; [`Error::Fault`] if the first
    /// page cannot be read; [`Error::NoMemory`] at the space owner's limit.
    pub fn read(&self, at: usize, buffer: &mut [u8]) -> Result<usize, Error> {
        let count = buffer.len();
        let value = Call::new(nr::SPACE_READ)
            .value(register(self.handle()))
            .value(at)
            .output(buffer)
            .value(count)
            .make(self.syscall());
        decode(value)
    }
}

impl<S: Syscall> Process<S> {
    /// `process_space` (0x1035): a handle to this process's address space,
    /// with `rights`, while it has not started. The handle keeps its rights
    /// after the start.
    ///
    /// # Errors
    ///
    /// [`Error::AccessDenied`] without `MANAGE`, or for a process this
    /// handle did not make; [`Error::BadState`] once it has started;
    /// [`Error::InvalidArgs`] for a right a space does not carry.
    pub fn space(&self, rights: Requested) -> Result<Space<S>, Error> {
        let value = Call::new(nr::PROCESS_SPACE)
            .value(register(self.handle()))
            .value(rights_register(rights))
            .make(self.syscall());
        let handle = decode_handle(value)?;
        Ok(Space::from_owned(OwnedHandle::from_raw(
            self.syscall(),
            handle,
        )))
    }
}

impl<S: Syscall> Vmo<S> {
    /// `vmo_executable` (0x1027): this handle, replaced by one that carries
    /// `EXECUTE` and not `WRITE`. The handle given is closed either way it
    /// goes in the kernel; on a refusal it is still this one.
    ///
    /// # Errors
    ///
    /// [`Error::AccessDenied`] without `MAP` and `READ`, with the handle
    /// back.
    pub fn executable(self) -> Result<Vmo<S>, (Error, Vmo<S>)> {
        let sys = self.syscall();
        let value = Call::new(nr::VMO_EXECUTABLE)
            .value(register(self.handle()))
            .value(0)
            .make(sys);
        match decode_handle(value) {
            Ok(handle) => {
                let _ = self.into_owned().into_raw();
                Ok(Vmo::from_owned(OwnedHandle::from_raw(sys, handle)))
            }
            Err(error) => Err((error, self)),
        }
    }
}
