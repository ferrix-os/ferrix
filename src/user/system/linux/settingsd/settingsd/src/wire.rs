//! Records to and from a client's connection, with deadlines: a client
//! that says nothing holds a thread for no longer than it is given.

use std::io;
use std::os::fd::RawFd;
use std::time::Instant;

use ferrix_auth_client::send_packet;
use ferrix_settings_proto::{MAX_RECORD, ToClient};

/// Send `record` to `fd`.
///
/// # Errors
///
/// A record that will not encode, or the socket's error.
pub(crate) fn send(fd: RawFd, record: &ToClient) -> io::Result<()> {
    let packet = record.encode().map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "a record that will not encode")
    })?;
    send_packet(fd, packet.bytes())
}

/// The next packet from `fd`, read into `buffer`, if one comes before
/// `deadline`: `None` on the deadline, an error, the other end closing, or
/// a packet longer than a record.
pub(crate) fn receive_within(fd: RawFd, buffer: &mut [u8], deadline: Instant) -> Option<&[u8]> {
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let millis = libc::c_int::try_from(left.as_millis()).unwrap_or(libc::c_int::MAX);
        // SAFETY: `poll` is one valid `pollfd`, and one is the count given.
        let ready = unsafe { libc::poll(&raw mut poll, 1, millis.max(1)) };
        if ready < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return None;
        }
        if ready > 0 {
            break;
        }
    }
    // SAFETY: `buffer` is valid for writes of its whole length, which is
    // what is passed.
    let got = unsafe { libc::recv(fd, buffer.as_mut_ptr().cast(), buffer.len(), 0) };
    let got = usize::try_from(got)
        .ok()
        .filter(|&got| got > 0 && got <= MAX_RECORD)?;
    buffer.get(..got)
}
