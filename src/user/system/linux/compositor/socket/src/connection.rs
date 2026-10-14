//! One client's socket: bytes and the descriptors that travel beside them.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

use compositor_wire::Fd;

use crate::{BUFFER_BYTES, MAX_FDS_IN};

/// Why a read failed.
#[derive(Debug)]
pub enum RecvError {
    /// The client closed the socket.
    Closed,
    /// Nothing is waiting, on a non-blocking socket.
    WouldBlock,
    /// The call failed.
    Io(io::Error),
    /// The kernel said it truncated the control message, which means
    /// descriptors were dropped. Carrying on would hand the server a message
    /// whose `fd` argument names someone else's descriptor.
    ControlTruncated,
}

/// The most a connection holds queued beyond what the socket took before
/// the client is given up on: Hyprland's `wl_display_set_default_max_buffer_size`.
///
/// libwayland's own default is 4 KiB, and past it the client is dropped.
/// A client that reads keeps its queue near empty; one that has stopped for
/// good would otherwise grow the compositor without end.
pub const MAX_QUEUED: usize = 1 << 20;

/// Why a write failed.
#[derive(Debug)]
pub enum SendError {
    /// More than [`MAX_QUEUED`] bytes are waiting for a client that is not
    /// reading them. The count is how many.
    Overflow(usize),
    /// The call failed.
    Io(io::Error),
}

/// A client's connection: the socket, the bytes not yet made into messages,
/// and the descriptors not yet claimed.
#[derive(Debug)]
pub struct Connection {
    /// The socket and what is queued for it, shared with every [`Sender`]
    /// made from this connection.
    shared: Arc<Shared>,
    /// Bytes read and not yet consumed by whole messages.
    incoming: Vec<u8>,
    /// Descriptors that arrived and have not been claimed by an `fd`
    /// argument. Owned, so one that no message ever claims is closed when the
    /// connection is dropped rather than leaked for the life of the
    /// compositor.
    descriptors: Vec<OwnedFd>,
}

/// The socket and its queue, which a [`Sender`] on another thread writes
/// through too.
#[derive(Debug)]
struct Shared {
    stream: UnixStream,
    queue: Mutex<Queue>,
}

/// What is queued to send.
#[derive(Debug, Default)]
struct Queue {
    /// Bytes queued to send that the socket would not take.
    bytes: Vec<u8>,
    /// Descriptors queued with them, to go with the next write. Copies, so
    /// the caller may close its own as soon as `send` returns.
    fds: Vec<OwnedFd>,
}

/// A second way to write to a connection, from another thread: whole
/// messages, without descriptors, into the same queue as the connection's
/// own, under the same lock (stage 24's input thread, `docs/GAME-PATH.md`
/// §4.7).
///
/// A message is appended whole while the lock is held, and the queue is
/// written from its front, so a sender's message never lands inside one of
/// the connection's, nor one of the connection's inside a sender's. Which
/// objects it may name is the caller's to keep right: the queue only keeps
/// the bytes in order.
#[derive(Debug, Clone)]
pub struct Sender(Arc<Shared>);

impl Sender {
    /// Queue one or more whole messages, and write what the socket will
    /// take.
    ///
    /// # Errors
    ///
    /// As [`Connection::send`].
    pub fn send(&self, bytes: &[u8]) -> Result<(), SendError> {
        let mut queue = self.0.lock();
        queue.bytes.extend_from_slice(bytes);
        self.0.flush(&mut queue)
    }
}

impl Shared {
    /// The queue. A thread that panicked while holding it left whole
    /// messages or none, so its queue is still good to write.
    fn lock(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Write what the socket will take of `queue`.
    fn flush(&self, queue: &mut Queue) -> Result<(), SendError> {
        while !queue.bytes.is_empty() {
            let count = queue.fds.len().min(MAX_FDS_IN);
            let fds: Vec<Fd> = queue
                .fds
                .iter()
                .take(count)
                .map(|fd| Fd(fd.as_raw_fd()))
                .collect();
            let Some(wrote) = self.write_once(&queue.bytes, &fds)? else {
                break;
            };
            let _ = queue.bytes.drain(..wrote.min(queue.bytes.len()));
            if wrote > 0 {
                // In flight with the write, the kernel holding its own
                // reference: the queue's copies can go.
                drop(queue.fds.drain(..count));
            }
        }
        if queue.bytes.len() > MAX_QUEUED {
            return Err(SendError::Overflow(queue.bytes.len()));
        }
        Ok(())
    }

    /// One `sendmsg` of `bytes`, with `fds` beside it.
    ///
    /// `None` when the socket is full; `Some(0)` when the call was
    /// interrupted and is to be made again.
    fn write_once(&self, bytes: &[u8], fds: &[Fd]) -> Result<Option<usize>, SendError> {
        let mut control = [0u8; control_bytes()];
        let mut iov = libc::iovec {
            iov_base: bytes.as_ptr().cast_mut().cast(),
            iov_len: bytes.len(),
        };
        // SAFETY: as in `receive`.
        let mut message: libc::msghdr = unsafe { core::mem::zeroed() };
        message.msg_iov = &raw mut iov;
        message.msg_iovlen = 1;
        let count = fds.len().min(MAX_FDS_IN);
        if count > 0 {
            message.msg_control = control.as_mut_ptr().cast();
            message.msg_controllen = cmsg_space(count) as _;
            if let Some(header) = first_header(&message) {
                write_rights(header, fds.get(..count).unwrap_or(&[]));
            }
        }

        #[expect(
            unsafe_code,
            reason = "AUDIT: sendmsg is not in std with control messages; every pointer in the msghdr is to a local buffer whose length is given beside it"
        )]
        // SAFETY: as in `receive`; `MSG_NOSIGNAL` keeps a closed client from
        // killing the compositor with SIGPIPE.
        let wrote = unsafe {
            libc::sendmsg(
                self.stream.as_raw_fd(),
                &raw const message,
                libc::MSG_NOSIGNAL,
            )
        };
        if wrote < 0 {
            let error = io::Error::last_os_error();
            return match error.kind() {
                io::ErrorKind::WouldBlock => Ok(None),
                io::ErrorKind::Interrupted => Ok(Some(0)),
                _ => Err(SendError::Io(error)),
            };
        }
        Ok(Some(usize::try_from(wrote).unwrap_or(0)))
    }
}

impl Connection {
    /// Take over an accepted stream.
    pub fn new(stream: UnixStream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Self {
            shared: Arc::new(Shared {
                stream,
                queue: Mutex::new(Queue::default()),
            }),
            incoming: Vec::new(),
            descriptors: Vec::new(),
        })
    }

    /// A [`Sender`] that writes into this connection's queue from another
    /// thread. It keeps the socket open while it lives.
    #[must_use]
    pub fn sender(&self) -> Sender {
        Sender(Arc::clone(&self.shared))
    }

    /// The descriptor to wait on.
    #[must_use]
    pub fn as_raw_fd(&self) -> i32 {
        self.shared.stream.as_raw_fd()
    }

    /// The process on the other end, or 0 where the kernel would not say.
    ///
    /// `hyprctl clients` prints each window's pid and `dispatch focuswindow
    /// pid:1234` picks a window out by it, so a compositor that answers
    /// nothing is one both of those are broken against. `SO_PEERCRED` gives
    /// the peer as it was when the socket was made, which is what a client's
    /// pid means: a client that forks afterwards is still the process that
    /// connected.
    #[must_use]
    pub fn peer_pid(&self) -> i32 {
        #[expect(
            unsafe_code,
            reason = "AUDIT: std's UnixStream::peer_cred is still unstable; ucred is three integers with no invariants"
        )]
        // SAFETY: a zeroed `ucred` is three zero integers, which is what a
        // socket with no peer credentials reads back as anyway.
        let mut peer: libc::ucred = unsafe { core::mem::zeroed() };
        let mut length = size_of::<libc::ucred>() as libc::socklen_t;
        #[expect(
            unsafe_code,
            reason = "AUDIT: getsockopt writes at most `length` bytes into a local ucred, and the socket is valid for as long as self"
        )]
        // SAFETY: the pointer is to `peer`, which lives for this call, and
        // `length` is its own size; `getsockopt` writes no more than that.
        let asked = unsafe {
            libc::getsockopt(
                self.shared.stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&raw mut peer).cast(),
                &raw mut length,
            )
        };
        if asked < 0 { 0 } else { peer.pid }
    }

    /// The bytes read and not yet consumed.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.incoming
    }

    /// The descriptors that arrived and have not been claimed, as the numbers
    /// the server sees.
    #[must_use]
    pub fn fds(&self) -> Vec<Fd> {
        self.descriptors
            .iter()
            .map(|fd| Fd(fd.as_raw_fd()))
            .collect()
    }

    /// Whether anything is waiting to be written.
    #[must_use]
    pub fn has_pending_writes(&self) -> bool {
        !self.shared.lock().bytes.is_empty()
    }

    /// Read whatever has arrived into the buffers.
    ///
    /// Gives how many bytes were read.
    pub fn receive(&mut self) -> Result<usize, RecvError> {
        let mut bytes = [0u8; BUFFER_BYTES];
        let mut control = [0u8; control_bytes()];
        let mut iov = libc::iovec {
            iov_base: bytes.as_mut_ptr().cast(),
            iov_len: bytes.len(),
        };
        // SAFETY: `msghdr` is a plain C structure with no invariants beyond
        // its fields being consistent, and zeroing it is what every caller
        // does before filling in the ones it uses.
        let mut message: libc::msghdr = unsafe { core::mem::zeroed() };
        message.msg_iov = &raw mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = control.len() as _;

        #[expect(
            unsafe_code,
            reason = "AUDIT: recvmsg is not in std with control messages; every pointer in the msghdr is to a local buffer whose length is given beside it"
        )]
        // SAFETY: `message` points at `iov` and `control`, both live for this
        // call, and each length is that buffer's own. The socket is valid for
        // as long as `self`.
        let read = unsafe { libc::recvmsg(self.shared.stream.as_raw_fd(), &raw mut message, 0) };
        if read < 0 {
            let error = io::Error::last_os_error();
            return match error.kind() {
                io::ErrorKind::WouldBlock => Err(RecvError::WouldBlock),
                io::ErrorKind::Interrupted => Ok(0),
                _ => Err(RecvError::Io(error)),
            };
        }
        // A truncated control message means descriptors were dropped, and a
        // message whose `fd` argument then names the wrong one is worse than
        // a closed connection.
        if message.msg_flags & libc::MSG_CTRUNC != 0 {
            self.take_descriptors(&message);
            return Err(RecvError::ControlTruncated);
        }
        self.take_descriptors(&message);
        if read == 0 {
            return Err(RecvError::Closed);
        }
        let read = usize::try_from(read).unwrap_or(0);
        self.incoming
            .extend_from_slice(bytes.get(..read).unwrap_or(&[]));
        Ok(read)
    }

    /// Drop the first `bytes` of what was read and the first `fds`
    /// descriptors, which the server has made into messages.
    ///
    /// The descriptors are *not* closed: a claimed one has been handed to the
    /// server, which owns it from then on.
    pub fn consume(&mut self, bytes: usize, fds: usize) {
        let _ = self.incoming.drain(..bytes.min(self.incoming.len()));
        for fd in self.descriptors.drain(..fds.min(self.descriptors.len())) {
            // Handed on: the server closes it when it is done.
            let _ = fd.into_raw_fd_owned();
        }
    }

    /// Queue bytes and descriptors, and write what the socket will take.
    ///
    /// What it will not take stays queued for [`Connection::flush`]: a
    /// socket that is full is a client busy for a moment, not a client gone.
    /// The descriptors are copied into the queue, so the caller may close
    /// its own once this returns, sent or not.
    ///
    /// Descriptors go with the first `sendmsg` that carries any of the
    /// bytes queued with or after them, which is what libwayland does: a
    /// descriptor may arrive before the message naming it, never after.
    ///
    /// # Errors
    ///
    /// [`SendError::Overflow`] when more than [`MAX_QUEUED`] bytes are left
    /// waiting, and [`SendError::Io`] when the socket failed or a descriptor
    /// could not be copied.
    pub fn send(&mut self, bytes: &[u8], fds: &[Fd]) -> Result<(), SendError> {
        let mut queue = self.shared.lock();
        for fd in fds {
            let copy = borrow(fd.0).try_clone_to_owned().map_err(SendError::Io)?;
            queue.fds.push(copy);
        }
        queue.bytes.extend_from_slice(bytes);
        self.shared.flush(&mut queue)
    }

    /// Write what the socket will take of what is queued.
    ///
    /// # Errors
    ///
    /// As [`Connection::send`].
    pub fn flush(&mut self) -> Result<(), SendError> {
        let mut queue = self.shared.lock();
        self.shared.flush(&mut queue)
    }

    /// Take every descriptor a `recvmsg` delivered.
    fn take_descriptors(&mut self, message: &libc::msghdr) {
        let mut header = first_header(message);
        while let Some(current) = header {
            for raw in rights_in(current) {
                self.descriptors.push(own(raw));
            }
            header = next_header(message, current);
        }
    }
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
        reason = "AUDIT: CMSG_NXTHDR is a macro with no safe form; it reads the current header's length and stays inside the buffer msg_controllen bounds"
    )]
    // SAFETY: `header` came from `first_header` or `next_header` on this same
    // `message`, so it is inside its control buffer.
    let next = unsafe { libc::CMSG_NXTHDR(message, header) };
    (!next.is_null()).then_some(next)
}

/// The header's level, type and payload length.
///
/// Read as one structure rather than field by field, because the house rule
/// is one unsafe operation per block and three field reads are three.
fn header_fields(header: *mut libc::cmsghdr) -> (i32, i32, usize) {
    #[expect(
        unsafe_code,
        reason = "AUDIT: one unaligned read of a cmsghdr the kernel wrote, into a copy on the stack"
    )]
    // SAFETY: `header` points at a `cmsghdr` the kernel filled in, inside a
    // control buffer that outlives this read. A control buffer is a byte
    // array, so the read must be unaligned.
    let copy: libc::cmsghdr = unsafe { header.read_unaligned() };
    let payload = widen(copy.cmsg_len).saturating_sub(cmsg_len_zero());
    (copy.cmsg_level, copy.cmsg_type, payload)
}

/// The descriptors an `SCM_RIGHTS` control message carries.
fn rights_in(header: *mut libc::cmsghdr) -> Vec<i32> {
    let (level, kind, payload) = header_fields(header);
    if level != libc::SOL_SOCKET || kind != libc::SCM_RIGHTS {
        return Vec::new();
    }
    let data = payload_of(header).cast_const();
    (0..payload / 4)
        // `wrapping_add` is safe; only the read is not.
        .map(|index| read_fd(data.wrapping_add(index)))
        .collect()
}

/// `CMSG_DATA`: where a control message's payload starts.
fn payload_of(header: *mut libc::cmsghdr) -> *mut i32 {
    #[expect(
        unsafe_code,
        reason = "AUDIT: CMSG_DATA is a macro with no safe form; it is an offset from the header it is given"
    )]
    // SAFETY: `header` points at a control message inside a buffer whose
    // length covers its payload.
    let data = unsafe { libc::CMSG_DATA(header) };
    data.cast::<i32>()
}

/// One descriptor of an `SCM_RIGHTS` payload.
fn read_fd(at: *const i32) -> i32 {
    #[expect(
        unsafe_code,
        reason = "AUDIT: one unaligned read of an int the kernel wrote, inside the payload its length reported"
    )]
    // SAFETY: `at` is inside the control message's payload, which the
    // caller bounded by the length the kernel gave. A control buffer is a
    // byte array, so the read must be unaligned.
    unsafe {
        at.read_unaligned()
    }
}

/// A descriptor the caller holds open, borrowed to be copied.
fn borrow(raw: i32) -> BorrowedFd<'static> {
    #[expect(
        unsafe_code,
        reason = "AUDIT: the descriptor is the caller's, open for the length of the send it was passed to, and is only copied"
    )]
    // SAFETY: `send`'s caller passes descriptors it holds open for the call,
    // and the borrow is used at once to make an owned copy.
    unsafe {
        BorrowedFd::borrow_raw(raw)
    }
}

/// Take ownership of a descriptor the kernel handed over.
fn own(raw: i32) -> OwnedFd {
    #[expect(
        unsafe_code,
        reason = "AUDIT: an SCM_RIGHTS descriptor is the receiver's to close, and nothing else in this process holds this number"
    )]
    // SAFETY: `raw` was created by the kernel for this `recvmsg` and has not
    // been given to anything else.
    unsafe {
        OwnedFd::from_raw_fd(raw)
    }
}

/// Fill in a header as `SCM_RIGHTS` carrying `fds`.
fn write_rights(header: *mut libc::cmsghdr, fds: &[Fd]) {
    set_header(header, cmsg_len(fds.len()));
    let data = payload_of(header);
    for (index, fd) in fds.iter().enumerate() {
        write_fd(data.wrapping_add(index), fd.0);
    }
}

/// Set a header's level, type and length.
///
/// Written as one structure for the reason [`header_fields`] reads one.
fn set_header(header: *mut libc::cmsghdr, len: usize) {
    let mut copy = zeroed_header();
    copy.cmsg_level = libc::SOL_SOCKET;
    copy.cmsg_type = libc::SCM_RIGHTS;
    copy.cmsg_len = len as _;
    #[expect(
        unsafe_code,
        reason = "AUDIT: one unaligned write of a cmsghdr into a control buffer this process owns and sized for it"
    )]
    // SAFETY: `header` is the first `cmsghdr` of a control buffer of at
    // least `cmsg_space(fds.len())` bytes, which is more than one header.
    unsafe {
        header.write_unaligned(copy);
    }
}

/// A `cmsghdr` with every byte zero.
fn zeroed_header() -> libc::cmsghdr {
    #[expect(
        unsafe_code,
        reason = "AUDIT: cmsghdr is a plain C structure of integers, for which all-zero is a value"
    )]
    // SAFETY: every field is an integer, so zero is valid for all of them.
    unsafe {
        core::mem::zeroed()
    }
}

/// Write one descriptor into an `SCM_RIGHTS` payload.
fn write_fd(at: *mut i32, fd: i32) {
    #[expect(
        unsafe_code,
        reason = "AUDIT: one unaligned write of an int, inside a payload the caller sized"
    )]
    // SAFETY: `at` is inside a payload sized for at least this many
    // descriptors by `write_rights`'s caller.
    unsafe {
        at.write_unaligned(fd);
    }
}

/// A control message's length as a `usize`.
///
/// `cmsg_len` is a `size_t` against glibc and a `u32` against musl, and the
/// compositor is built for both: a cast is redundant on one and a conversion
/// is redundant on the other, so this is generic and neither reads as
/// pointless on either.
fn widen<T: TryInto<usize>>(value: T) -> usize {
    value.try_into().unwrap_or(0)
}

/// `CMSG_LEN(0)`: the bytes a header takes before its payload.
fn cmsg_len_zero() -> usize {
    #[expect(
        unsafe_code,
        reason = "AUDIT: CMSG_LEN is declared unsafe by libc and is pure arithmetic"
    )]
    // SAFETY: arithmetic on a constant; it touches no memory.
    unsafe {
        libc::CMSG_LEN(0) as usize
    }
}

/// `CMSG_LEN(count * 4)`: a header and that many descriptors.
fn cmsg_len(count: usize) -> usize {
    let payload = u32::try_from(count.saturating_mul(4)).unwrap_or(u32::MAX);
    #[expect(
        unsafe_code,
        reason = "AUDIT: CMSG_LEN is declared unsafe by libc and is pure arithmetic"
    )]
    // SAFETY: arithmetic on a number; it touches no memory.
    unsafe {
        libc::CMSG_LEN(payload) as usize
    }
}

/// A trait for taking a descriptor out of an `OwnedFd` without closing it.
trait IntoRawOwned {
    fn into_raw_fd_owned(self) -> i32;
}

impl IntoRawOwned for OwnedFd {
    fn into_raw_fd_owned(self) -> i32 {
        use std::os::fd::IntoRawFd;
        self.into_raw_fd()
    }
}

/// Room for one control message carrying `count` descriptors.
const fn cmsg_space(count: usize) -> usize {
    // CMSG_SPACE is not a const function in libc, and its value is the
    // header rounded up to an alignment plus the payload rounded up to one.
    // Both roundings are to `size_of::<usize>()` on every platform Ferrix
    // builds for.
    let align = size_of::<usize>();
    let header = size_of::<libc::cmsghdr>().div_ceil(align) * align;
    let payload = (count * 4).div_ceil(align) * align;
    header + payload
}

/// The control buffer's size: room for [`MAX_FDS_IN`] descriptors.
const fn control_bytes() -> usize {
    cmsg_space(MAX_FDS_IN)
}
