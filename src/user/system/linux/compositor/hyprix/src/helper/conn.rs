//! The socket to hyprix-gpu: records and their descriptors, and the
//! process at its other end (`docs/GPU-COMPOSITOR.md` §3).

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

use compositor_gpu_protocol::{Header, MAX_PLANES, MAX_RECORD, Message};

/// The most descriptors one record carries: an output set or a dmabuf's
/// planes.
const MAX_FDS: usize = MAX_PLANES;

/// One end of a `SOCK_SEQPACKET` pair, each record a message.
#[derive(Debug)]
pub(crate) struct Socket {
    fd: OwnedFd,
    serial: u32,
}

/// A message that arrived, with what rode on it.
#[derive(Debug)]
pub(crate) struct Received {
    /// Its header.
    #[expect(
        dead_code,
        reason = "the serial is for answers matched to requests, which version 1 does not need"
    )]
    pub(crate) header: Header,
    /// The message.
    pub(crate) message: Message,
    /// Its descriptors, in order.
    pub(crate) fds: Vec<OwnedFd>,
}

impl Socket {
    /// A connected pair: this process's end and the one to hand on.
    ///
    /// # Errors
    ///
    /// Whatever `socketpair` said.
    pub(crate) fn pair() -> io::Result<(Self, OwnedFd)> {
        let mut fds = [0 as RawFd; 2];
        #[expect(
            unsafe_code,
            reason = "AUDIT: socketpair is not in std for SOCK_SEQPACKET; it writes two descriptors into a two-element array"
        )]
        // SAFETY: `fds` is a live array of the two ints the call writes.
        let made = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                fds.as_mut_ptr(),
            )
        };
        if made < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((Self::new(own(fds[0])), own(fds[1])))
    }

    /// This process's end of a pair made elsewhere: the tests' fake helper.
    pub(crate) const fn new(fd: OwnedFd) -> Self {
        Self { fd, serial: 0 }
    }

    /// Send `message` with `fds` riding on it; its serial.
    ///
    /// # Errors
    ///
    /// A message that does not encode, a count of descriptors that is not
    /// the message's, or whatever `sendmsg` said (`EPIPE` for a helper that
    /// has gone).
    pub(crate) fn send(&mut self, message: &Message, fds: &[BorrowedFd<'_>]) -> io::Result<u32> {
        if fds.len() != message.fds() || fds.len() > MAX_FDS {
            return Err(io::Error::other("a record with the wrong descriptors"));
        }
        self.serial = self.serial.wrapping_add(1).max(1);
        let record = message
            .encode(self.serial)
            .map_err(|error| io::Error::other(error.to_string()))?;
        let raw: Vec<RawFd> = fds.iter().map(AsRawFd::as_raw_fd).collect();
        send_record(self.fd.as_raw_fd(), &record, &raw)?;
        Ok(self.serial)
    }

    /// The next record, waiting at most until `deadline`: `None` when the
    /// time ran out.
    ///
    /// # Errors
    ///
    /// The helper's end closed (`UnexpectedEof`), a record that does not
    /// decode or whose descriptors are not the header's, or whatever
    /// `recvmsg` said.
    pub(crate) fn receive(&mut self, deadline: Instant) -> io::Result<Option<Received>> {
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if !readable(self.fd.as_raw_fd(), left)? {
                // poll counts whole milliseconds, and may wake a little
                // before the deadline: only the deadline ends the wait.
                if Instant::now() >= deadline {
                    return Ok(None);
                }
                continue;
            }
            let Some((bytes, fds)) = receive_record(self.fd.as_raw_fd())? else {
                continue;
            };
            let (header, message) = Message::decode(&bytes)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
            if fds.len() != usize::from(header.fds) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "a record whose descriptors are not its header's",
                ));
            }
            return Ok(Some(Received {
                header,
                message,
                fds,
            }));
        }
    }
}

/// Whether `fd` is readable within `left`.
fn readable(fd: RawFd, left: Duration) -> io::Result<bool> {
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = i32::try_from(left.as_micros().div_ceil(1000)).unwrap_or(i32::MAX);
    #[expect(
        unsafe_code,
        reason = "AUDIT: poll is not in std; it reads and writes one live pollfd"
    )]
    // SAFETY: `poll` is one live pollfd and the count says one.
    let ready = unsafe { libc::poll(&raw mut poll, 1, millis) };
    match ready {
        0 => Ok(false),
        n if n > 0 => Ok(true),
        _ => {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                Ok(false)
            } else {
                Err(error)
            }
        }
    }
}

/// Room for [`MAX_FDS`] descriptors in a control message, aligned for
/// `cmsghdr`.
#[repr(C, align(8))]
struct Control([u8; 64]);

fn send_record(fd: RawFd, record: &[u8], fds: &[RawFd]) -> io::Result<()> {
    let mut iov = libc::iovec {
        iov_base: record.as_ptr().cast_mut().cast(),
        iov_len: record.len(),
    };
    let mut control = Control([0; 64]);
    #[expect(
        unsafe_code,
        reason = "AUDIT: msghdr is a plain C struct; zeroed is its empty value"
    )]
    // SAFETY: every field of msghdr is an integer or a pointer, for which
    // zero is valid.
    let mut header: libc::msghdr = unsafe { core::mem::zeroed() };
    header.msg_iov = &raw mut iov;
    header.msg_iovlen = 1;
    if !fds.is_empty() {
        let space = cmsg_space(fds.len());
        if space > control.0.len() {
            return Err(io::Error::other("too many fds"));
        }
        header.msg_control = control.0.as_mut_ptr().cast();
        header.msg_controllen = space as _;
        let Some(first) = first_header(&header) else {
            return Err(io::Error::other("no room for the descriptors"));
        };
        write_rights(first, fds);
    }
    loop {
        #[expect(
            unsafe_code,
            reason = "AUDIT: sendmsg of a msghdr whose iovec and control buffer live across the call"
        )]
        // SAFETY: as the reason says.
        let sent = unsafe { libc::sendmsg(fd, &raw const header, libc::MSG_NOSIGNAL) };
        if sent >= 0 {
            return if usize::try_from(sent).ok() == Some(record.len()) {
                Ok(())
            } else {
                Err(io::Error::other("a record sent in part"))
            };
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn receive_record(fd: RawFd) -> io::Result<Option<(Vec<u8>, Vec<OwnedFd>)>> {
    let mut bytes = vec![0u8; MAX_RECORD + 1];
    let mut iov = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let mut control = Control([0; 64]);
    #[expect(
        unsafe_code,
        reason = "AUDIT: msghdr is a plain C struct; zeroed is its empty value"
    )]
    // SAFETY: every field of msghdr is an integer or a pointer, for which
    // zero is valid.
    let mut header: libc::msghdr = unsafe { core::mem::zeroed() };
    header.msg_iov = &raw mut iov;
    header.msg_iovlen = 1;
    header.msg_control = control.0.as_mut_ptr().cast();
    header.msg_controllen = control.0.len() as _;
    #[expect(
        unsafe_code,
        reason = "AUDIT: recvmsg into a msghdr whose iovec and control buffer live across the call"
    )]
    // SAFETY: as the reason says.
    let got = unsafe { libc::recvmsg(fd, &raw mut header, libc::MSG_CMSG_CLOEXEC) };
    if got < 0 {
        let error = io::Error::last_os_error();
        return match error.kind() {
            io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock => Ok(None),
            _ => Err(error),
        };
    }
    // Descriptors first, so none leaks whatever the bytes are.
    let mut fds = Vec::new();
    let mut next = first_header(&header);
    while let Some(at) = next {
        fds.extend(rights_in(at).into_iter().map(own));
        next = next_header(&header, at);
    }
    if got == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "hyprix-gpu closed its end",
        ));
    }
    if header.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "a record or its descriptors over the limit",
        ));
    }
    bytes.truncate(usize::try_from(got).unwrap_or(0));
    Ok(Some((bytes, fds)))
}

/// `CMSG_FIRSTHDR`: the first control message, if there is one.
fn first_header(message: &libc::msghdr) -> Option<*mut libc::cmsghdr> {
    #[expect(
        unsafe_code,
        reason = "AUDIT: CMSG_FIRSTHDR is a macro with no safe form; it only reads msg_control and msg_controllen, which the caller set to a buffer and its length"
    )]
    // SAFETY: `message`'s `msg_control` points at a buffer of
    // `msg_controllen` bytes for as long as this borrow lives.
    let header = unsafe { libc::CMSG_FIRSTHDR(message) };
    (!header.is_null()).then_some(header)
}

/// `CMSG_NXTHDR`: the control message after `header`, if there is one.
fn next_header(message: &libc::msghdr, header: *mut libc::cmsghdr) -> Option<*mut libc::cmsghdr> {
    #[expect(
        unsafe_code,
        reason = "AUDIT: CMSG_NXTHDR is a macro with no safe form; it stays inside the buffer msg_controllen bounds"
    )]
    // SAFETY: `header` came from `first_header` or `next_header` on this same
    // `message`, so it is inside its control buffer.
    let next = unsafe { libc::CMSG_NXTHDR(message, header) };
    (!next.is_null()).then_some(next)
}

/// The descriptors an `SCM_RIGHTS` control message carries.
fn rights_in(header: *mut libc::cmsghdr) -> Vec<RawFd> {
    #[expect(
        unsafe_code,
        reason = "AUDIT: one unaligned read of a cmsghdr the kernel wrote, into a copy on the stack"
    )]
    // SAFETY: `header` points at a `cmsghdr` the kernel filled in, inside a
    // control buffer that outlives this read.
    let copy: libc::cmsghdr = unsafe { header.read_unaligned() };
    if copy.cmsg_level != libc::SOL_SOCKET || copy.cmsg_type != libc::SCM_RIGHTS {
        return Vec::new();
    }
    let payload = widen(copy.cmsg_len).saturating_sub(cmsg_len(0));
    let data = payload_of(header).cast_const();
    (0..payload / size_of::<RawFd>())
        .map(|index| read_fd(data.wrapping_add(index)))
        .collect()
}

/// `CMSG_DATA`: where a control message's payload starts.
fn payload_of(header: *mut libc::cmsghdr) -> *mut RawFd {
    #[expect(
        unsafe_code,
        reason = "AUDIT: CMSG_DATA is a macro with no safe form; it is an offset from the header it is given"
    )]
    // SAFETY: `header` points at a control message inside a buffer whose
    // length covers its payload.
    let data = unsafe { libc::CMSG_DATA(header) };
    data.cast::<RawFd>()
}

/// One descriptor of an `SCM_RIGHTS` payload.
fn read_fd(at: *const RawFd) -> RawFd {
    #[expect(
        unsafe_code,
        reason = "AUDIT: one unaligned read of an int the kernel wrote, inside the payload its length reported"
    )]
    // SAFETY: `at` is inside the payload, which the caller bounded by the
    // length the kernel gave.
    unsafe {
        at.read_unaligned()
    }
}

/// Fill in a header as `SCM_RIGHTS` carrying `fds`.
fn write_rights(header: *mut libc::cmsghdr, fds: &[RawFd]) {
    #[expect(
        unsafe_code,
        reason = "AUDIT: cmsghdr is a plain C structure of integers, for which all-zero is a value"
    )]
    // SAFETY: every field is an integer.
    let mut copy: libc::cmsghdr = unsafe { core::mem::zeroed() };
    copy.cmsg_level = libc::SOL_SOCKET;
    copy.cmsg_type = libc::SCM_RIGHTS;
    copy.cmsg_len = cmsg_len(size_of_val(fds)) as _;
    #[expect(
        unsafe_code,
        reason = "AUDIT: one unaligned write of a cmsghdr into a control buffer this process owns and sized for it"
    )]
    // SAFETY: `header` is the first header of a control buffer of at least
    // `cmsg_space(fds.len())` bytes.
    unsafe {
        header.write_unaligned(copy);
    }
    let data = payload_of(header);
    for (index, &fd) in fds.iter().enumerate() {
        #[expect(
            unsafe_code,
            reason = "AUDIT: one unaligned write of an int, inside a payload sized for these descriptors"
        )]
        // SAFETY: as the reason says.
        unsafe {
            data.wrapping_add(index).write_unaligned(fd);
        }
    }
}

/// `CMSG_LEN(bytes)`.
fn cmsg_len(bytes: usize) -> usize {
    let bytes = u32::try_from(bytes).unwrap_or(u32::MAX);
    #[expect(
        unsafe_code,
        reason = "AUDIT: CMSG_LEN is declared unsafe by libc and is pure arithmetic"
    )]
    // SAFETY: arithmetic only.
    let len = unsafe { libc::CMSG_LEN(bytes) };
    len as usize
}

/// `CMSG_SPACE` of `count` descriptors.
fn cmsg_space(count: usize) -> usize {
    let bytes = u32::try_from(count * size_of::<RawFd>()).unwrap_or(u32::MAX);
    #[expect(
        unsafe_code,
        reason = "AUDIT: CMSG_SPACE is declared unsafe by libc and is pure arithmetic"
    )]
    // SAFETY: arithmetic only.
    let space = unsafe { libc::CMSG_SPACE(bytes) };
    space as usize
}

/// Take ownership of a descriptor the kernel made for this process.
fn own(raw: RawFd) -> OwnedFd {
    #[expect(
        unsafe_code,
        reason = "AUDIT: a descriptor socketpair or SCM_RIGHTS just made, which nothing else in this process holds"
    )]
    // SAFETY: as the reason says.
    unsafe {
        OwnedFd::from_raw_fd(raw)
    }
}

/// A control message's length as a `usize`: a `size_t` against glibc and a
/// `u32` against musl, and hyprix is built for both.
fn widen<T: TryInto<usize>>(value: T) -> usize {
    value.try_into().unwrap_or(0)
}
