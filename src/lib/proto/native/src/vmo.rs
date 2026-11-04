//! Virtual memory objects: pages, not a mapping.
//!
//! Mapping a VMO is [`crate::pending`]'s, until its handler lands.

use ferrix_native_abi::nr;

use crate::call::{Call, Syscall};
use crate::error::{Error, decode_handle, decode_unit};
use crate::handle::{Object, OwnedHandle, object_handle, register};

object_handle!(
    /// A VMO.
    Vmo
);

/// `vmo_create`: an anonymous VMO of `bytes`, rounded up to whole pages.
///
/// # Errors
///
/// [`Error::NoMemory`] past the kernel's limit; [`Error::NoHandles`].
pub fn create<S: Syscall>(sys: S, bytes: usize) -> Result<Vmo<S>, Error> {
    let handle = decode_handle(Call::new(nr::VMO_CREATE).value(bytes).make(sys))?;
    Ok(Vmo::from_owned(OwnedHandle::from_raw(sys, handle)))
}

impl<S: Syscall> Vmo<S> {
    /// `vmo_digest`: a sealed VMO's measurement (`docs/microkernel/K14.md` §4).
    ///
    /// # Errors
    ///
    /// [`Error::BadState`] when it is not sealed.
    pub fn digest(&self, out: &mut [u8; 32]) -> Result<(), Error> {
        decode_unit(
            Call::new(nr::VMO_DIGEST)
                .value(register(self.handle()))
                .output(out.as_mut_slice())
                .make(self.syscall()),
        )
    }

    /// `vmo_read`: fill `buffer` from the VMO, starting at `offset`.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgs`] past the VMO's end; [`Error::AccessDenied`]
    /// without `READ`.
    pub fn read(&self, buffer: &mut [u8], offset: u64) -> Result<(), Error> {
        let count = buffer.len();
        let offset = offset.to_ne_bytes();
        let value = Call::new(nr::VMO_READ)
            .value(register(self.handle()))
            .output(buffer)
            .value(count)
            .input(&offset)
            .make(self.syscall());
        decode_unit(value)
    }

    /// `vmo_write`: copy `buffer` into the VMO, starting at `offset`.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgs`] past the VMO's end; [`Error::AccessDenied`]
    /// without `WRITE`.
    pub fn write(&self, buffer: &[u8], offset: u64) -> Result<(), Error> {
        let offset = offset.to_ne_bytes();
        let value = Call::new(nr::VMO_WRITE)
            .value(register(self.handle()))
            .input(buffer)
            .value(buffer.len())
            .input(&offset)
            .make(self.syscall());
        decode_unit(value)
    }

    /// `vmo_hold`: every page of this VMO committed and held in place for as long
    /// as the handle answered lives (`docs/microkernel/BTRFSD.md`, BD.2).
    ///
    /// # Errors
    ///
    /// [`Error::BadState`] for a kind that cannot be held; the status otherwise.
    pub fn hold_whole(&self) -> Result<OwnedHandle<S>, Error> {
        let value = Call::new(nr::VMO_HOLD)
            .value(register(self.handle()))
            .make(self.syscall());
        Ok(OwnedHandle::from_raw(self.syscall(), decode_handle(value)?))
    }

    /// `vmo_get_size`: the VMO's size in bytes.
    ///
    /// # Errors
    ///
    /// [`Error::WrongType`] for a handle to something else.
    pub fn size(&self) -> Result<u64, Error> {
        let mut size = [0_u8; 8];
        let value = Call::new(nr::VMO_GET_SIZE)
            .value(register(self.handle()))
            .output(&mut size)
            .make(self.syscall());
        decode_unit(value).map(|()| u64::from_ne_bytes(size))
    }
}
