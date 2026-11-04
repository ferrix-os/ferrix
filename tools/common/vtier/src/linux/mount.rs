//! Mounting through the setuid `fusermount3`, which passes back the device.
use std::ffi::c_void;
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command;

#[repr(C)]
struct IoVec {
    base: *mut c_void,
    length: usize,
}
#[repr(C)]
struct MsgHdr {
    name: *mut c_void,
    name_length: u32,
    iov: *mut IoVec,
    iov_length: usize,
    control: *mut c_void,
    control_length: usize,
    flags: i32,
}
unsafe extern "C" {
    fn recvmsg(fd: i32, message: *mut MsgHdr, flags: i32) -> isize;
    fn fcntl(fd: i32, command: i32, argument: i32) -> i32;
}
const SOL_SOCKET: i32 = 1;
const SCM_RIGHTS: i32 = 1;
const F_SETFD: i32 = 2;

/// The caller's uid and gid: this process's, read off what it owns.
pub(super) fn ids() -> (u32, u32) {
    std::fs::metadata("/proc/self").map_or((0, 0), |m| (m.uid(), m.gid()))
}

/// The file descriptor in a message's control data.
fn take_fd(socket: RawFd) -> Result<OwnedFd, String> {
    let mut byte = [0u8; 1];
    let mut iov = IoVec {
        base: byte.as_mut_ptr().cast(),
        length: 1,
    };
    // cmsghdr (length 8, level 4, type 4) and one int, aligned to 8.
    let mut control = [0u64; 4];
    let mut message = MsgHdr {
        name: std::ptr::null_mut(),
        name_length: 0,
        iov: &raw mut iov,
        iov_length: 1,
        control: control.as_mut_ptr().cast(),
        control_length: size_of_val(&control),
        flags: 0,
    };
    // SAFETY: `message` and all it points at live across the call, with the
    // lengths given; the kernel writes no further than they say.
    let got = unsafe { recvmsg(socket, &raw mut message, 0) };
    if got <= 0 || message.control_length < 20 {
        return Err("fusermount3 passed back no device".into());
    }
    let level = (control[1] & 0xffff_ffff) as i32;
    let kind = (control[1] >> 32) as i32;
    if level != SOL_SOCKET || kind != SCM_RIGHTS {
        return Err("fusermount3 sent something else".into());
    }
    // SAFETY: SCM_RIGHTS put a descriptor new to this process here, which nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd((control[2] & 0xffff_ffff) as i32) })
}

/// Mount at `mountpoint`, and return the FUSE device. A store that dies leaves a
/// mount answering `ENOTCONN`, which is an error for anyone using it; `gate.sh`
/// clears it with `fusermount3 -uz` before the next store.
pub(super) fn mount(mountpoint: &Path) -> Result<File, String> {
    let (ours, theirs) = UnixStream::pair().map_err(|e| format!("socketpair: {e}"))?;
    let their_fd = theirs.as_raw_fd();
    // The child must inherit it: std marks sockets close-on-exec.
    // SAFETY: F_SETFD on a descriptor `theirs` holds open, with no pointer involved.
    if unsafe { fcntl(their_fd, F_SETFD, 0) } != 0 {
        return Err("could not hand fusermount3 its socket".into());
    }
    let mut child = Command::new("fusermount3")
        .env("_FUSE_COMMFD", their_fd.to_string())
        .args([
            "-o",
            "nosuid,nodev,noatime,default_permissions,fsname=vtier,subtype=vtier",
            "--",
        ])
        .arg(mountpoint)
        .spawn()
        .map_err(|e| format!("fusermount3: {e}"))?;
    drop(theirs);
    let device = take_fd(ours.as_raw_fd());
    let waited = child.wait().map_err(|e| format!("fusermount3: {e}"))?;
    if !waited.success() {
        return Err(format!("fusermount3: {waited}"));
    }
    device.map(File::from)
}
