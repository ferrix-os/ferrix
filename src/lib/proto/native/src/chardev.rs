//! The chardev core's driver calls (`docs/NVIDIA.md` §4.4): answering the
//! requests the kernel forwards on a control channel made with
//! [`crate::device::Device::chardev_control`], and reaching the memory and
//! descriptors of the program waiting in one.
//!
//! Each names the control channel and a request id from a REQUEST
//! (`ferrix_chardevctl::message`), and works only while that request is
//! outstanding: once answered or abandoned, the id names nothing.

use ferrix_native_abi::nr;

use crate::call::{Call, Syscall};
use crate::channel::Channel;
use crate::error::{Error, decode, decode_unit};
use crate::handle::{Object, register};

/// `chardev_reply`: answer `request` with `status` (zero or a negative
/// errno) and `value` (an ioctl's return).
///
/// # Errors
///
/// [`Error::BadState`] for a request not outstanding on this control,
/// [`Error::InvalidArgs`] for a status that is neither, and
/// [`Error::WrongType`] for a channel that is not a chardev control.
pub fn reply<S: Syscall>(
    control: &Channel<S>,
    request: u64,
    status: i32,
    value: i64,
) -> Result<(), Error> {
    let value = Call::new(nr::CHARDEV_REPLY)
        .value(register(control.handle()))
        .value(request as usize)
        .value(status as isize as usize)
        .value(value as usize)
        .make(control.syscall());
    decode_unit(value)
}

/// `chardev_copy_in`: fill `buffer` from the waiting program's memory at
/// `client`.
///
/// # Errors
///
/// As [`reply`], and [`Error::Fault`] for memory the program cannot read.
pub fn copy_in<S: Syscall>(
    control: &Channel<S>,
    request: u64,
    client: u64,
    buffer: &mut [u8],
) -> Result<(), Error> {
    let length = buffer.len();
    let value = Call::new(nr::CHARDEV_COPY_IN)
        .value(register(control.handle()))
        .value(request as usize)
        .value(client as usize)
        .output(buffer)
        .value(length)
        .make(control.syscall());
    decode_unit(value)
}

/// `chardev_copy_out`: write `buffer` into the waiting program's memory at
/// `client`.
///
/// # Errors
///
/// As [`copy_in`].
pub fn copy_out<S: Syscall>(
    control: &Channel<S>,
    request: u64,
    client: u64,
    buffer: &[u8],
) -> Result<(), Error> {
    let length = buffer.len();
    let value = Call::new(nr::CHARDEV_COPY_OUT)
        .value(register(control.handle()))
        .value(request as usize)
        .value(client as usize)
        .input(buffer)
        .value(length)
        .make(control.syscall());
    decode_unit(value)
}

/// `chardev_file`: the identity, on this control, of the file the waiting
/// program's descriptor `fd` names.
///
/// # Errors
///
/// As [`reply`], and [`Error::BadHandle`] for a descriptor that is not one
/// of this control's files.
pub fn file<S: Syscall>(control: &Channel<S>, request: u64, fd: i32) -> Result<u64, Error> {
    let value = Call::new(nr::CHARDEV_FILE)
        .value(register(control.handle()))
        .value(request as usize)
        .value(fd as isize as usize)
        .make(control.syscall());
    decode(value).map(|file| file as u64)
}

/// What [`dmabuf_install`] answered: the descriptor in the waiting program,
/// and whether this call made the dmabuf.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Installed {
    /// The descriptor.
    pub fd: i32,
    /// Whether the call made it, rather than finding one alive for the
    /// cookie; a `DmabufRelease` for the cookie follows exactly when it did.
    pub made: bool,
}

/// `chardev_dmabuf_install`: a dmabuf over the whole of `vmo`, named by
/// `cookie`, as a new descriptor in the program waiting in `request`
/// (`docs/NVIDIA.md` §4.4, N3b). `flags` are
/// `ferrix_native_abi::types::DMABUF_WRITABLE` and `DMABUF_CLOEXEC`;
/// whether it was made is always asked.
///
/// # Errors
///
/// As [`reply`]; [`Error::AccessDenied`] without `READ` and `TRANSFER` on
/// `vmo` (and `WRITE` for a writable one), [`Error::InvalidArgs`] for a VMO
/// that is not plain anonymous memory, [`Error::AlreadyBound`] for a cookie
/// alive over another VMO, and [`Error::LimitReached`] past the control's
/// dmabufs or the program's descriptors.
pub fn dmabuf_install<S: Syscall>(
    control: &Channel<S>,
    request: u64,
    vmo: &crate::vmo::Vmo<S>,
    cookie: u64,
    flags: u64,
) -> Result<Installed, Error> {
    use ferrix_native_abi::types::{DMABUF_MADE, DMABUF_TELL_MADE};
    let value = Call::new(nr::CHARDEV_DMABUF_INSTALL)
        .value(register(control.handle()))
        .value(request as usize)
        .value(register(vmo.handle()))
        .value(cookie as usize)
        .value((flags | DMABUF_TELL_MADE) as usize)
        .make(control.syscall());
    decode(value).map(|answer| Installed {
        fd: (answer & !DMABUF_MADE) as i32,
        made: answer & DMABUF_MADE != 0,
    })
}

/// `chardev_dmabuf_resolve`: the cookie of the dmabuf the waiting program's
/// descriptor `fd` names, if this control made it.
///
/// # Errors
///
/// As [`reply`], and [`Error::BadHandle`] for any other descriptor.
pub fn dmabuf_resolve<S: Syscall>(
    control: &Channel<S>,
    request: u64,
    fd: i32,
) -> Result<u64, Error> {
    let mut cookie = [0u8; 8];
    let value = Call::new(nr::CHARDEV_DMABUF_RESOLVE)
        .value(register(control.handle()))
        .value(request as usize)
        .value(fd as isize as usize)
        .output(&mut cookie)
        .make(control.syscall());
    decode_unit(value).map(|()| u64::from_ne_bytes(cookie))
}
