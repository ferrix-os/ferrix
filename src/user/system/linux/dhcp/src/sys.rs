//! The two sockets the client needs and std does not offer: an rtnetlink
//! socket, which applies a lease, and an `AF_PACKET` socket, which carries
//! DHCP while the interface has no address to send from.
//!
//! This is the only `unsafe` in the program: the `libc` calls, each with
//! what makes it sound beside it. Everything they carry is built and read
//! in the library, on the host.

use std::io;
use std::mem::{size_of, zeroed};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::time::Duration;

use dhcp::rtnl::{self, Outcome};

/// `ETH_P_IP`, in network order as a packet socket's protocol is given.
const IP_PROTOCOL: u16 = 0x0800_u16.to_be();

/// The longest netlink reply read at once: a dump of a few interfaces and
/// addresses is far less.
const NETLINK_ROOM: usize = 64 * 1024;

/// How long a netlink request may go unanswered: the kernel answers at
/// once, so this only bounds a kernel that never does.
const NETLINK_WAIT: Duration = Duration::from_secs(5);

/// The longest frame payload read: an Ethernet MTU.
const FRAME_ROOM: usize = 1500;

/// The last error, as an `io::Error` saying which call made it.
fn failed(call: &str) -> io::Error {
    let error = io::Error::last_os_error();
    io::Error::new(error.kind(), format!("{call}: {error}"))
}

/// An rtnetlink socket, and the sequence numbers of its requests.
#[derive(Debug)]
pub(crate) struct Netlink {
    fd: OwnedFd,
    seq: u32,
}

impl Netlink {
    /// A `NETLINK_ROUTE` socket, bound for the kernel to name it.
    pub(crate) fn open() -> io::Result<Netlink> {
        // SAFETY: a plain call with no pointers.
        let raw = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                libc::NETLINK_ROUTE,
            )
        };
        if raw < 0 {
            return Err(failed("socket(AF_NETLINK)"));
        }
        // SAFETY: `raw` was just made and nothing else owns it.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: all-zero is a valid `sockaddr_nl`: the family is set below,
        // and port 0 asks the kernel to choose one.
        let mut address: libc::sockaddr_nl = unsafe { zeroed() };
        address.nl_family = libc::sa_family_t::try_from(libc::AF_NETLINK).unwrap_or(16);
        // SAFETY: `address` is a `sockaddr_nl` and the length says so.
        let bound = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                (&raw const address).cast(),
                socklen::<libc::sockaddr_nl>(),
            )
        };
        if bound < 0 {
            return Err(failed("bind(AF_NETLINK)"));
        }
        Ok(Netlink { fd, seq: 0 })
    }

    /// Send the request `build` writes for the next sequence number, and
    /// read its answer, handing each part of a dump to `part`.
    ///
    /// # Errors
    ///
    /// A send or receive that failed; a refusal is `Ok(Err(errno))`.
    pub(crate) fn call(
        &mut self,
        build: impl FnOnce(u32) -> Vec<u8>,
        mut part: impl FnMut(u16, &[u8]),
    ) -> io::Result<Result<(), i32>> {
        self.seq = self.seq.wrapping_add(1);
        let seq = self.seq;
        let request = build(seq);
        // SAFETY: all-zero is a valid `sockaddr_nl`: port 0 is the kernel.
        let mut kernel: libc::sockaddr_nl = unsafe { zeroed() };
        kernel.nl_family = libc::sa_family_t::try_from(libc::AF_NETLINK).unwrap_or(16);
        // SAFETY: `request` is readable for its length, and `kernel` is a
        // `sockaddr_nl` whose length is given.
        let sent = unsafe {
            libc::sendto(
                self.fd.as_raw_fd(),
                request.as_ptr().cast(),
                request.len(),
                0,
                (&raw const kernel).cast(),
                socklen::<libc::sockaddr_nl>(),
            )
        };
        if sent < 0 {
            return Err(failed("sendto(AF_NETLINK)"));
        }
        let mut buffer = vec![0_u8; NETLINK_ROOM];
        loop {
            if !wait_readable(&self.fd, Some(NETLINK_WAIT))? {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the kernel did not answer a netlink request",
                ));
            }
            // SAFETY: `buffer` is writable for its whole length.
            let read = unsafe {
                libc::recv(
                    self.fd.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    0,
                )
            };
            let Ok(read) = usize::try_from(read) else {
                return Err(failed("recv(AF_NETLINK)"));
            };
            match rtnl::outcome(buffer.get(..read).unwrap_or_default(), seq, &mut part) {
                Outcome::More => {}
                Outcome::Done => return Ok(Ok(())),
                Outcome::Refused(errno) => return Ok(Err(errno)),
            }
        }
    }
}

/// An `AF_PACKET` datagram socket for IPv4 on one interface.
#[derive(Debug)]
pub(crate) struct Packet {
    fd: OwnedFd,
    index: i32,
}

impl Packet {
    /// The socket, bound to interface `index` and to IPv4 frames.
    pub(crate) fn open(index: u32) -> io::Result<Packet> {
        let index = i32::try_from(index)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "an interface index"))?;
        // SAFETY: a plain call with no pointers.
        let raw = unsafe {
            libc::socket(
                libc::AF_PACKET,
                libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
                i32::from(IP_PROTOCOL),
            )
        };
        if raw < 0 {
            return Err(failed("socket(AF_PACKET)"));
        }
        // SAFETY: `raw` was just made and nothing else owns it.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let address = link_address(index, None);
        // SAFETY: `address` is a `sockaddr_ll` and the length says so.
        let bound = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                (&raw const address).cast(),
                socklen::<libc::sockaddr_ll>(),
            )
        };
        if bound < 0 {
            return Err(failed("bind(AF_PACKET)"));
        }
        Ok(Packet { fd, index })
    }

    /// Send `packet`, an IPv4 packet, to the hardware address `to`.
    pub(crate) fn send(&self, to: [u8; 6], packet: &[u8]) -> io::Result<()> {
        let address = link_address(self.index, Some(to));
        // SAFETY: `packet` is readable for its length, and `address` is a
        // `sockaddr_ll` whose length is given.
        let sent = unsafe {
            libc::sendto(
                self.fd.as_raw_fd(),
                packet.as_ptr().cast(),
                packet.len(),
                0,
                (&raw const address).cast(),
                socklen::<libc::sockaddr_ll>(),
            )
        };
        if sent < 0 {
            return Err(failed("sendto(AF_PACKET)"));
        }
        Ok(())
    }

    /// Wait up to `timeout`, or for ever with none, for a frame to read:
    /// whether one came. A wait cut short by a signal is one with none.
    pub(crate) fn wait(&self, timeout: Option<Duration>) -> io::Result<bool> {
        wait_readable(&self.fd, timeout)
    }

    /// The frame waiting, as its IPv4 packet, and the hardware address it
    /// came from.
    pub(crate) fn receive(&self) -> io::Result<(Vec<u8>, [u8; 6])> {
        let mut buffer = vec![0_u8; FRAME_ROOM];
        let mut from = link_address(self.index, None);
        let mut from_len = socklen::<libc::sockaddr_ll>();
        // SAFETY: `buffer` is writable for its length, and `from` is a
        // `sockaddr_ll` of the length `from_len` gives.
        let read = unsafe {
            libc::recvfrom(
                self.fd.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                libc::MSG_DONTWAIT,
                (&raw mut from).cast(),
                &raw mut from_len,
            )
        };
        let Ok(read) = usize::try_from(read) else {
            return Err(failed("recvfrom(AF_PACKET)"));
        };
        buffer.truncate(read);
        let mut mac = [0_u8; 6];
        for (to, byte) in mac.iter_mut().zip(from.sll_addr) {
            *to = byte;
        }
        Ok((buffer, mac))
    }
}

/// A `sockaddr_ll` for IPv4 on interface `index`, to hardware address `to`.
fn link_address(index: i32, to: Option<[u8; 6]>) -> libc::sockaddr_ll {
    // SAFETY: all-zero is a valid `sockaddr_ll`; the fields that matter are
    // set below.
    let mut address: libc::sockaddr_ll = unsafe { zeroed() };
    address.sll_family = u16::try_from(libc::AF_PACKET).unwrap_or(17);
    address.sll_protocol = IP_PROTOCOL;
    address.sll_ifindex = index;
    if let Some(to) = to {
        address.sll_halen = 6;
        for (slot, byte) in address.sll_addr.iter_mut().zip(to) {
            *slot = byte;
        }
    }
    address
}

/// Wait up to `timeout`, or for ever with none, for `fd` to have something
/// to read: whether it has. A wait cut short by a signal is one with none.
fn wait_readable(fd: &OwnedFd, timeout: Option<Duration>) -> io::Result<bool> {
    let millis = timeout.map_or(-1, |timeout| {
        // Rounded up, so a wait for a timer that is a fraction of a
        // millisecond away does not spin until it is due.
        i32::try_from(timeout.as_micros().div_ceil(1000)).unwrap_or(i32::MAX)
    });
    let mut poll = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `poll` is one `pollfd`, and the count says one.
    let ready = unsafe { libc::poll(&raw mut poll, 1, millis) };
    if ready < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(io::Error::new(error.kind(), format!("poll: {error}")));
    }
    Ok(ready > 0)
}

/// The size of `T`, as a socket call's length.
fn socklen<T>() -> libc::socklen_t {
    libc::socklen_t::try_from(size_of::<T>()).unwrap_or(0)
}

/// Four random bytes from the kernel, for transaction identifiers; the time
/// of day stands in where there are none.
pub(crate) fn seed() -> u32 {
    use std::io::Read as _;
    let mut bytes = [0_u8; 4];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .is_ok()
    {
        return u32::from_ne_bytes(bytes);
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(1, |since| {
            since.subsec_nanos() ^ u32::try_from(since.as_secs() & 0xFFFF_FFFF).unwrap_or(0)
        })
}
