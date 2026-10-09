//! A server that sends more descriptors than the one asked for leaks none
//! of them into the compositor (the consultant's Z-A2, ledger line 638).
//!
//! Its own test binary, so that no other test opens descriptors while the
//! count is taken.

#![expect(clippy::expect_used, reason = "a test: a refusal is the test failing")]

use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::net::UnixStream;

use compositor_virgl::vtest::receive_one_fd;

/// How many descriptors this process has open.
fn open_fds() -> usize {
    std::fs::read_dir("/proc/self/fd")
        .expect("/proc/self/fd")
        .count()
}

/// Send one byte with `fds` beside it.
fn send(socket: &UnixStream, fds: &[i32]) {
    let mut byte = 0_u8;
    let mut iov = libc::iovec {
        iov_base: (&raw mut byte).cast(),
        iov_len: 1,
    };
    let mut control = [0_u64; 8];
    // SAFETY: a zeroed msghdr is a valid value.
    let mut message: libc::msghdr = unsafe { core::mem::zeroed() };
    message.msg_iov = &raw mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    let data_len = u32::try_from(fds.len() * 4).expect("small");
    // SAFETY: a pure computation.
    let space = unsafe { libc::CMSG_SPACE(data_len) };
    message.msg_controllen = space as usize;
    // SAFETY: the first header of a buffer with room for it.
    let header = unsafe { libc::CMSG_FIRSTHDR(&raw const message) };
    assert!(!header.is_null());
    // SAFETY: a zeroed cmsghdr is a valid value.
    let mut cmsg: libc::cmsghdr = unsafe { core::mem::zeroed() };
    // SAFETY: a pure computation.
    let len = unsafe { libc::CMSG_LEN(data_len) };
    cmsg.cmsg_len = len as usize;
    cmsg.cmsg_level = libc::SOL_SOCKET;
    cmsg.cmsg_type = libc::SCM_RIGHTS;
    // SAFETY: the header's place inside the buffer.
    unsafe { header.write_unaligned(cmsg) };
    // SAFETY: the header's data, inside the buffer.
    let data = unsafe { libc::CMSG_DATA(header) }.cast::<i32>();
    for (index, fd) in fds.iter().enumerate() {
        let at = data.wrapping_add(index);
        // SAFETY: room for `fds.len()` descriptors, which CMSG_SPACE counted.
        unsafe { at.write_unaligned(*fd) };
    }
    // SAFETY: sendmsg reads the byte and the buffer, both alive.
    let sent = unsafe { libc::sendmsg(socket.as_raw_fd(), &raw const message, 0) };
    assert_eq!(sent, 1, "sent");
}

#[test]
fn extra_descriptors_are_closed_not_kept() {
    let (ours, server) = UnixStream::pair().expect("a pair");
    let file = std::fs::File::open("/proc/self/stat").expect("a file to send");
    let fd = file.as_fd().as_raw_fd();

    let before = open_fds();
    send(&server, &[fd, fd]);
    assert!(receive_one_fd(ours.as_raw_fd()).is_err(), "two refused");
    assert_eq!(open_fds(), before, "both received descriptors closed");

    send(&server, &[]);
    assert!(receive_one_fd(ours.as_raw_fd()).is_err(), "none refused");
    assert_eq!(open_fds(), before);

    send(&server, &[fd]);
    let one = receive_one_fd(ours.as_raw_fd()).expect("one taken");
    assert_eq!(open_fds(), before + 1);
    drop(one);
    assert_eq!(open_fds(), before);
}
