//! Records over the socket hyprix hands the helper: one record a packet
//! (`SOCK_SEQPACKET`), descriptors beside it (`SCM_RIGHTS`).

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd as _, OwnedFd, RawFd};

use compositor_gpu_protocol::MAX_RECORD;

/// The most descriptors one record carries.
const MAX_FDS: usize = 16;

/// Bytes of a control message carrying `fds` descriptors.
fn space(fds: usize) -> usize {
    // SAFETY: CMSG_SPACE only computes a size.
    unsafe { libc::CMSG_SPACE(u32::try_from(fds * size_of::<RawFd>()).unwrap_or(0)) as usize }
}

/// One end of the socket.
#[derive(Debug)]
pub struct Wire {
    fd: OwnedFd,
}

impl Wire {
    /// The socket at `fd`.
    #[must_use]
    pub const fn new(fd: OwnedFd) -> Self {
        Self { fd }
    }

    /// The socket.
    #[must_use]
    pub fn fd(&self) -> BorrowedFd<'_> {
        use std::os::fd::AsFd as _;
        self.fd.as_fd()
    }

    /// Send `record` with `fds`.
    ///
    /// # Errors
    ///
    /// The socket's.
    pub fn send(&self, record: &[u8], fds: &[BorrowedFd<'_>]) -> io::Result<()> {
        let mut iov = libc::iovec {
            iov_base: record.as_ptr().cast_mut().cast(),
            iov_len: record.len(),
        };
        let mut control = vec![0_u8; space(fds.len().max(1))];
        // SAFETY: an all-zero msghdr is a valid empty one.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &raw mut iov;
        msg.msg_iovlen = 1;
        if !fds.is_empty() {
            msg.msg_control = control.as_mut_ptr().cast();
            msg.msg_controllen = space(fds.len());
            // SAFETY: the control buffer is `msg_controllen` bytes, room for
            // one header and `fds.len()` descriptors.
            let header = unsafe { libc::CMSG_FIRSTHDR(&raw const msg) };
            if header.is_null() {
                return Err(io::Error::other("no room for descriptors"));
            }
            let raw: Vec<RawFd> = fds.iter().map(AsRawFd::as_raw_fd).collect();
            let bytes = raw.len() * size_of::<RawFd>();
            // SAFETY: CMSG_LEN only computes a size.
            let len = unsafe { libc::CMSG_LEN(u32::try_from(bytes).unwrap_or(0)) } as usize;
            // SAFETY: `header` points into `control`, which has room for it
            // and its data (CMSG_SPACE above), and is aligned for it.
            let header = unsafe { &mut *header };
            header.cmsg_level = libc::SOL_SOCKET;
            header.cmsg_type = libc::SCM_RIGHTS;
            header.cmsg_len = len;
            // SAFETY: CMSG_DATA is inside `control` with `bytes` of room.
            let data = unsafe { libc::CMSG_DATA(header) };
            // SAFETY: as above; the source is `bytes` long.
            unsafe { std::ptr::copy_nonoverlapping(raw.as_ptr().cast::<u8>(), data, bytes) };
        }
        loop {
            // SAFETY: `msg` and what it points to live across the call.
            let sent =
                unsafe { libc::sendmsg(self.fd.as_raw_fd(), &raw const msg, libc::MSG_NOSIGNAL) };
            if sent >= 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    /// The next record and the descriptors beside it; `None` at the end.
    ///
    /// # Errors
    ///
    /// The socket's.
    pub fn receive(&self) -> io::Result<Option<(Vec<u8>, Vec<OwnedFd>)>> {
        let mut record = vec![0_u8; MAX_RECORD];
        let mut iov = libc::iovec {
            iov_base: record.as_mut_ptr().cast(),
            iov_len: record.len(),
        };
        let mut control = vec![0_u8; space(MAX_FDS)];
        // SAFETY: an all-zero msghdr is a valid empty one.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &raw mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = control.len();
        let got = loop {
            // SAFETY: `msg` and its buffers live across the call.
            let got =
                unsafe { libc::recvmsg(self.fd.as_raw_fd(), &raw mut msg, libc::MSG_CMSG_CLOEXEC) };
            if got >= 0 {
                break got;
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        };
        let mut fds = Vec::new();
        // SAFETY: `msg` was filled by recvmsg; the macros walk its control
        // buffer within `msg_controllen`.
        let mut header = unsafe { libc::CMSG_FIRSTHDR(&raw const msg) };
        while !header.is_null() {
            // SAFETY: a header recvmsg wrote, inside the control buffer.
            let held = unsafe { &*header };
            let (level, kind, len) = (held.cmsg_level, held.cmsg_type, held.cmsg_len);
            if level == libc::SOL_SOCKET && kind == libc::SCM_RIGHTS {
                // SAFETY: CMSG_LEN(0) is the header's own size.
                let empty = unsafe { libc::CMSG_LEN(0) } as usize;
                let count = len.saturating_sub(empty) / size_of::<RawFd>();
                // SAFETY: the data follows the header, `count` descriptors.
                let data = unsafe { libc::CMSG_DATA(header) }.cast::<RawFd>();
                for at in 0..count {
                    // SAFETY: inside the data: `count` descriptors follow.
                    let each = unsafe { data.add(at) };
                    // SAFETY: as above, unaligned as cmsg data may be.
                    let raw = unsafe { each.read_unaligned() };
                    // SAFETY: a descriptor the kernel installed for us.
                    fds.push(unsafe { OwnedFd::from_raw_fd(raw) });
                }
            }
            // SAFETY: walking the same control buffer.
            header = unsafe { libc::CMSG_NXTHDR(&raw const msg, header) };
        }
        if got == 0 && fds.is_empty() {
            return Ok(None);
        }
        if msg.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
            return Err(io::Error::other("a record or its descriptors cut short"));
        }
        record.truncate(usize::try_from(got).unwrap_or(0));
        Ok(Some((record, fds)))
    }
}
