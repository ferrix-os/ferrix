//! The request bridge: nvrm's end of the kernel's chardev core
//! (`docs/NVIDIA.md` §4.4, N1e).
//!
//! [`nvos_chardev_start`] makes the device's control channel, says HELLO
//! with the minors nvrm serves, and waits for READY. [`nvos_chardev_next`]
//! takes the next REQUEST. [`nvos_chardev_reply`] answers one, and the two
//! copies reach the waiting program's memory while its request is
//! outstanding: they back a client's `copy_in` and `copy_out`
//! (`client.rs`), so RM's `os_memcpy_from_user` and `os_memcpy_to_user`
//! read and write the program that called.

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, Ordering};

use ferrix_chardevctl::message::{self, Hello, MAX_NODES, Message, Op, VERSION};
use ferrix_native::channel::Channel;
use ferrix_native::handle::{Object, OwnedHandle};
use ferrix_native::{Deadline, Handle};
use ferrix_native_abi::nr;
use ferrix_native_abi::signals::Signals;

use crate::device;
use crate::futex::{self, Kernel};
use crate::log::say;
use crate::status::{self, NvStatus};

/// The control channel's handle, 0 before [`nvos_chardev_start`].
static CONTROL: AtomicU32 = AtomicU32::new(0);

/// A request as the C dispatcher takes it (`struct nvos_request` in
/// `include/nvos.h`).
#[repr(C)]
#[derive(Debug)]
pub struct Request {
    /// Its id, which the reply and the copies name.
    pub id: u64,
    /// The file's identity.
    pub file: u64,
    /// 1 open, 2 ioctl, 3 release, 4 mmap.
    pub op: u32,
    /// The node's minor.
    pub minor: u32,
    /// The program's process id.
    pub pid: u32,
    /// Its effective user id.
    pub euid: u32,
    /// Its effective group id.
    pub egid: u32,
    /// The ioctl's command.
    pub cmd: u32,
    /// The ioctl's argument, raw; an mmap's file offset.
    pub arg: u64,
    /// An mmap's length in pages.
    pub pages: u32,
    /// Unused.
    pub reserved: u32,
}

/// The control channel, borrowed for one call: nvrm holds it for its life.
fn control() -> Option<core::mem::ManuallyDrop<Channel<Kernel>>> {
    let handle = CONTROL.load(Ordering::Acquire);
    (handle != 0).then(|| {
        core::mem::ManuallyDrop::new(Channel::from_owned(OwnedHandle::from_raw(
            Kernel,
            Handle(handle),
        )))
    })
}

/// A native call's result: a negative errno, or the value.
fn native(number: usize, a: [usize; 6]) -> Result<usize, i32> {
    // SAFETY: every caller passes the chardev calls' arguments, whose
    // pointers name buffers valid for their length.
    let result = unsafe { futex::call6(number, a) };
    let signed = result as isize;
    if (-4095..0).contains(&signed) {
        Err(i32::try_from(signed).unwrap_or(-5))
    } else {
        Ok(result)
    }
}

/// Make the control channel, say HELLO listing `count` minors, and wait for
/// READY. `NV_OK`, or why not, said.
///
/// # Safety
///
/// `minors` holds `count` minors.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nvos_chardev_start(minors: *const u16, count: u32) -> NvStatus {
    let count = usize::try_from(count).unwrap_or(0);
    if count == 0 || count > MAX_NODES || minors.is_null() {
        return status::INVALID_ARGUMENT;
    }
    let device = device::handle();
    if device == 0 {
        return status::INVALID_STATE;
    }
    let handle = match native(nr::CHARDEV_CONTROL_CREATE, [device as usize, 0, 0, 0, 0, 0]) {
        Ok(handle) => u32::try_from(handle).unwrap_or(0),
        Err(errno) => {
            say!("chardev_control_create refused: errno {}", -errno);
            return status::NOT_SUPPORTED;
        }
    };
    CONTROL.store(handle, Ordering::Release);
    let Some(channel) = control() else {
        return status::INVALID_STATE;
    };
    let mut hello = Hello {
        version: VERSION,
        location: device::location(),
        count,
        minors: [0; MAX_NODES],
    };
    // SAFETY: the caller vouches for `count` minors.
    let given = unsafe { core::slice::from_raw_parts(minors, count) };
    for (slot, minor) in hello.minors.iter_mut().zip(given) {
        *slot = *minor;
    }
    if let Err(why) = channel.write(Message::Hello(hello).encode().as_bytes()) {
        say!("chardev HELLO not written: {why:?}");
        return status::INVALID_STATE;
    }
    let mut bytes = [0_u8; message::MAX_BYTES];
    let mut handles = [Handle(0); 1];
    if let Err(why) = channel.wait_one(Signals::READABLE | Signals::PEER_CLOSED, Deadline::Never) {
        say!("chardev wait for READY: {why:?}");
        return status::INVALID_STATE;
    }
    let received = match channel.read(&mut bytes, &mut handles) {
        Ok(received) => received,
        Err(why) => {
            say!("chardev READY not read: {why:?}");
            return status::INVALID_STATE;
        }
    };
    match Message::decode(bytes.get(..received.bytes).unwrap_or(&[])) {
        Ok(Message::Ready(published)) => {
            say!("chardev: {published} node(s) published");
            status::OK
        }
        Ok(Message::Refused(refusal)) => {
            say!("chardev: refused: {refusal}");
            status::INVALID_STATE
        }
        _ => {
            say!("chardev: no READY");
            status::INVALID_STATE
        }
    }
}

/// Wait for the next REQUEST and write it to `out`. `NV_OK`, or
/// `NV_ERR_INVALID_STATE` when the channel is gone.
///
/// # Safety
///
/// `out` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nvos_chardev_next(out: *mut Request) -> NvStatus {
    let Some(channel) = control() else {
        return status::INVALID_STATE;
    };
    let mut bytes = [0_u8; message::MAX_BYTES];
    let mut handles = [Handle(0); 1];
    loop {
        match channel.read(&mut bytes, &mut handles) {
            Ok(received) => {
                let Ok(Message::Request(request)) =
                    Message::decode(bytes.get(..received.bytes).unwrap_or(&[]))
                else {
                    say!("chardev: a message that is not a REQUEST");
                    continue;
                };
                let op = match request.op {
                    Op::Open => 1,
                    Op::Ioctl => 2,
                    Op::Release => 3,
                    Op::Mmap => 4,
                    Op::DmabufRelease => 5,
                };
                // SAFETY: the caller vouches for `out`.
                unsafe {
                    out.write(Request {
                        id: request.id,
                        file: request.file,
                        op,
                        minor: u32::from(request.minor),
                        pid: request.pid,
                        euid: request.euid,
                        egid: request.egid,
                        cmd: request.cmd,
                        arg: request.arg,
                        pages: request.pages,
                        reserved: 0,
                    });
                }
                return status::OK;
            }
            Err(ferrix_native::channel::ReadError::Failed(ferrix_native::Error::ShouldWait)) => {
                if channel
                    .wait_one(Signals::READABLE | Signals::PEER_CLOSED, Deadline::Never)
                    .is_err()
                {
                    return status::INVALID_STATE;
                }
            }
            Err(why) => {
                say!("chardev: the control channel failed: {why:?}");
                return status::INVALID_STATE;
            }
        }
    }
}

/// Answer request `id`: `status` zero or a negative errno, `value` an
/// ioctl's return. 0, or a negative errno.
#[unsafe(no_mangle)]
pub extern "C" fn nvos_chardev_reply(id: u64, status: i32, value: i64) -> i32 {
    let control = CONTROL.load(Ordering::Acquire) as usize;
    match native(
        nr::CHARDEV_REPLY,
        [
            control,
            id as usize,
            status as isize as usize,
            value as usize,
            0,
            0,
        ],
    ) {
        Ok(_) => 0,
        Err(errno) => errno,
    }
}

/// The identity of the file the waiting program's descriptor `fd` names,
/// for request `id`, if it is one of nvrm's; otherwise a negative errno.
#[unsafe(no_mangle)]
pub extern "C" fn nvos_chardev_file(id: u64, fd: i32) -> i64 {
    let control = CONTROL.load(Ordering::Acquire) as usize;
    match native(
        nr::CHARDEV_FILE,
        [control, id as usize, fd as isize as usize, 0, 0, 0],
    ) {
        Ok(file) => i64::try_from(file).unwrap_or(-9),
        Err(errno) => i64::from(errno),
    }
}

/// A dmabuf of the whole VMO `vmo` (a handle in nvrm's table), named by
/// `cookie`, as a new descriptor in the program request `id` is for:
/// nvidia-drm's PRIME_HANDLE_TO_FD (N3b, `docs/NVIDIA.md` §4.4). `flags` are
/// `DMABUF_WRITABLE` and `DMABUF_CLOEXEC`. The descriptor number, or a
/// negative errno. A cookie still live gives a new descriptor of the same
/// dmabuf; the kernel says when the last one goes (`DmabufRelease`).
#[unsafe(no_mangle)]
pub extern "C" fn nvos_chardev_dmabuf_install(id: u64, vmo: u32, cookie: u64, flags: u32) -> i64 {
    let control = CONTROL.load(Ordering::Acquire) as usize;
    match native(
        nr::CHARDEV_DMABUF_INSTALL,
        [
            control,
            id as usize,
            vmo as usize,
            cookie as usize,
            flags as usize,
            0,
        ],
    ) {
        Ok(fd) => i64::try_from(fd).unwrap_or(-9),
        Err(errno) => i64::from(errno),
    }
}

/// A name-only dmabuf of `size` bytes, named by `cookie`, as a new
/// descriptor in the program request `id` is for: PRIME_HANDLE_TO_FD of a
/// buffer in video memory (N3b, `docs/NVIDIA.md` §4.4). It has no VMO and
/// cannot be mapped; only this driver's resolve gives its cookie back.
/// Answers as [`nvos_chardev_dmabuf_install`].
#[unsafe(no_mangle)]
pub extern "C" fn nvos_chardev_dmabuf_install_name(
    id: u64,
    size: u64,
    cookie: u64,
    flags: u32,
) -> i64 {
    let control = CONTROL.load(Ordering::Acquire) as usize;
    match native(
        nr::CHARDEV_DMABUF_INSTALL,
        [
            control,
            id as usize,
            0,
            cookie as usize,
            (u64::from(flags) | ferrix_native_abi::types::DMABUF_NAME_ONLY) as usize,
            size as usize,
        ],
    ) {
        Ok(fd) => i64::try_from(fd).unwrap_or(-9),
        Err(errno) => i64::from(errno),
    }
}

/// The cookie of the dmabuf the waiting program's descriptor `fd` names,
/// for request `id`, into `cookie`: nvidia-drm's PRIME_FD_TO_HANDLE. 0 only
/// for one this control made; otherwise a negative errno.
///
/// # Safety
///
/// `cookie` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nvos_chardev_dmabuf_resolve(id: u64, fd: i32, cookie: *mut u64) -> i32 {
    let control = CONTROL.load(Ordering::Acquire) as usize;
    let mut found = 0_u64;
    let result = native(
        nr::CHARDEV_DMABUF_RESOLVE,
        [
            control,
            id as usize,
            fd as isize as usize,
            core::ptr::from_mut(&mut found) as usize,
            0,
            0,
        ],
    );
    match result {
        Ok(_) => {
            // SAFETY: the caller vouches for `cookie`.
            unsafe { cookie.write(found) };
            0
        }
        Err(errno) => errno,
    }
}

/// A fence, unsignalled, named by `cookie`, as a new descriptor in the
/// program request `id` is for: nvidia-drm's SEMSURF_FENCE_CREATE (N3b sync,
/// `docs/NVIDIA.md` §4.6). It reads as signalled with ETIMEDOUT after
/// `deadline_ms` (0 is 5 s, at most 10 s) and with ENODEV once nvrm is
/// gone. `flags` is `NVOS_SYNC_CLOEXEC`. The descriptor, or a negative errno.
#[unsafe(no_mangle)]
pub extern "C" fn nvos_chardev_sync_install(
    id: u64,
    cookie: u64,
    deadline_ms: u32,
    flags: u32,
) -> i64 {
    let control = CONTROL.load(Ordering::Acquire) as usize;
    match native(
        nr::CHARDEV_SYNC_INSTALL,
        [
            control,
            id as usize,
            cookie as usize,
            deadline_ms as usize,
            flags as usize,
            0,
        ],
    ) {
        Ok(fd) => i64::try_from(fd).unwrap_or(-9),
        Err(errno) => i64::from(errno),
    }
}

/// Signal the fence `cookie` with `status`, 0 or a negative errno, once.
/// 0, or a negative errno: a fence already signalled, or past its deadline,
/// is refused.
#[unsafe(no_mangle)]
pub extern "C" fn nvos_chardev_sync_signal(cookie: u64, status: i32) -> i32 {
    let control = CONTROL.load(Ordering::Acquire) as usize;
    match native(
        nr::CHARDEV_SYNC_SIGNAL,
        [
            control,
            cookie as usize,
            i64::from(status) as usize,
            0,
            0,
            0,
        ],
    ) {
        Ok(_) => 0,
        Err(errno) => errno,
    }
}

/// The cookie of the fence the waiting program's descriptor `fd` names, for
/// request `id`, into `cookie`, if this control made it: 1 if it is
/// signalled, 0 if not, or a negative errno.
///
/// # Safety
///
/// `cookie` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nvos_chardev_sync_resolve(id: u64, fd: i32, cookie: *mut u64) -> i32 {
    let control = CONTROL.load(Ordering::Acquire) as usize;
    let mut found = 0_u64;
    let result = native(
        nr::CHARDEV_SYNC_RESOLVE,
        [
            control,
            id as usize,
            fd as isize as usize,
            core::ptr::from_mut(&mut found) as usize,
            0,
            0,
        ],
    );
    match result {
        Ok(signalled) => {
            // SAFETY: the caller vouches for `cookie`.
            unsafe { cookie.write(found) };
            i32::from(signalled != 0)
        }
        Err(errno) => errno,
    }
}

/// Answer mmap request `id`: `value` a VMO handle with `offset` its byte
/// offset, or a physical address, by `kind` (`ferrix_chardevctl::message`'s
/// `MAP_*`). 0, or a negative errno.
#[unsafe(no_mangle)]
pub extern "C" fn nvos_chardev_reply_map(id: u64, value: u64, offset: u64, kind: u64) -> i32 {
    let control = CONTROL.load(Ordering::Acquire) as usize;
    match native(
        nr::CHARDEV_REPLY,
        [
            control,
            id as usize,
            0,
            value as usize,
            offset as usize,
            kind as usize,
        ],
    ) {
        Ok(_) => 0,
        Err(errno) => errno,
    }
}

/// Copy `length` bytes from the waiting program's `from` into nvrm's `to`,
/// for request `id`. 0, or a negative errno.
///
/// # Safety
///
/// `to` holds `length` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nvos_chardev_copy_in(
    id: u64,
    to: *mut c_void,
    from: u64,
    length: u32,
) -> i32 {
    let control = CONTROL.load(Ordering::Acquire) as usize;
    match native(
        nr::CHARDEV_COPY_IN,
        [
            control,
            id as usize,
            from as usize,
            to as usize,
            length as usize,
            0,
        ],
    ) {
        Ok(_) => 0,
        Err(errno) => errno,
    }
}

/// Copy `length` bytes from nvrm's `from` to the waiting program's `to`,
/// for request `id`. 0, or a negative errno.
///
/// # Safety
///
/// `from` holds `length` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nvos_chardev_copy_out(
    id: u64,
    to: u64,
    from: *const c_void,
    length: u32,
) -> i32 {
    let control = CONTROL.load(Ordering::Acquire) as usize;
    match native(
        nr::CHARDEV_COPY_OUT,
        [
            control,
            id as usize,
            to as usize,
            from as usize,
            length as usize,
            0,
        ],
    ) {
        Ok(_) => 0,
        Err(errno) => errno,
    }
}
