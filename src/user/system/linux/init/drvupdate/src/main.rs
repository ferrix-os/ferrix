//! `/bin/drvupdate`: put a new version of a driver on its devices without a
//! reboot (`docs/DEVMGR.md` §4.1).
//!
//! ```text
//! drvupdate DRIVER IMAGE [LOCATION]
//! ```
//!
//! `DRIVER` is the driver's program name (`gpu`, `blk`, ...), `IMAGE` the
//! new version's file, and `LOCATION` the device's PCI address as `devmgr`
//! prints it (`00:02.0`); without it every device the driver drives is
//! updated, one at a time. The image goes to `drvupdated` over the abstract
//! socket `ferrix.devmgr.update`, which takes it from root alone; `devmgr`
//! answers, and this prints the answer.
//!
//! The server is checked too: an abstract name belongs to whoever binds it
//! first, so a server whose `SO_PEERCRED` uid is not root's is refused
//! before the image is sent (the certification consultant's C6,
//! 2026-10-07).
//!
//! Exit status: 0 when every device named is updated, 1 for any other
//! answer or a failure, 2 for a command line it does not take.

use std::io::{self, Read as _, Write as _};
use std::os::fd::AsRawFd as _;
use std::os::linux::net::SocketAddrExt as _;
use std::os::unix::net::{SocketAddr, UnixStream};
use std::process::ExitCode;
use std::{fs, ptr};

use ferrix_drvupdate_proto::{ANSWER_BYTES, ANY, Answer, Outcome, Request, SOCKET_NAME};

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let (driver, image, location) = match arguments.as_slice() {
        [driver, image] => (driver, image, Some(ANY)),
        [driver, image, location] => (driver, image, parse_location(location)),
        _ => (&String::new(), &String::new(), None),
    };
    let Some(location) = location.filter(|_| !driver.is_empty()) else {
        say("usage: drvupdate DRIVER IMAGE [LOCATION]");
        return ExitCode::from(2);
    };
    match update(driver, image, location) {
        Ok(answer) => {
            say(&format!(
                "drvupdate: {driver}: {} ({} of {} devices updated)",
                answer.outcome, answer.updated, answer.tried
            ));
            if answer.outcome == Outcome::Updated {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(error) => {
            say(&format!("drvupdate: {driver}: {error}"));
            ExitCode::from(1)
        }
    }
}

/// Send the image, and read `devmgr`'s answer.
fn update(driver: &str, image: &str, location: u32) -> io::Result<Answer> {
    let bytes = fs::read(image).map_err(|error| io::Error::other(format!("{image}: {error}")))?;
    let length = u64::try_from(bytes.len()).map_err(io::Error::other)?;
    let request = Request::new(driver.as_bytes(), location, length)
        .ok_or_else(|| io::Error::other("a driver's name is 1 to 31 bytes"))?;
    let address = SocketAddr::from_abstract_name(SOCKET_NAME)?;
    let mut stream = UnixStream::connect_addr(&address)
        .map_err(|error| io::Error::other(format!("drvupdated is not there: {error}")))?;
    if peer_uid(&stream)? != 0 {
        return Err(io::Error::other(
            "the socket's server is not root's: refused, nothing sent",
        ));
    }
    // A request the helper refuses from its header -- an image past 8 MiB
    // -- is answered and closed before the bytes are all sent: the answer
    // is read whether or not the writes got through.
    let sent = stream
        .write_all(&request.encode())
        .and_then(|()| stream.write_all(&bytes));
    let mut answer = [0_u8; ANSWER_BYTES];
    if let Err(error) = stream.read_exact(&mut answer) {
        return Err(sent.err().unwrap_or(error));
    }
    Answer::decode(&answer).map_err(|why| io::Error::other(format!("an answer of {why:?}")))
}

/// `00:02.0`, or `0001:00:02.0`, as the PCI address word `devmgr` and
/// START carry.
fn parse_location(text: &str) -> Option<u32> {
    let mut parts: Vec<&str> = text.split(':').collect();
    let slot = parts.pop()?;
    let bus = u32::from_str_radix(parts.pop()?, 16).ok()?;
    let segment = match parts.as_slice() {
        [] => 0,
        [segment] => u32::from_str_radix(segment, 16).ok()?,
        _ => return None,
    };
    let (device, function) = slot.split_once('.')?;
    let device = u32::from_str_radix(device, 16).ok()?;
    let function = function.parse::<u32>().ok()?;
    if bus > 0xFF || device > 0x1F || function > 7 || segment > 0xFFFF {
        return None;
    }
    Some((segment << 16) | (bus << 8) | (device << 3) | function)
}

/// The uid of the process at the other end of `stream`.
fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: u32::MAX,
        gid: u32::MAX,
    };
    let mut size = libc::socklen_t::try_from(size_of::<libc::ucred>()).unwrap_or(0);
    // SAFETY: `credentials` is writable for `size` bytes, and `size` is
    // writable.
    let ret = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            ptr::from_mut(&mut credentials).cast(),
            &mut size,
        )
    };
    if ret == 0 {
        Ok(credentials.uid)
    } else {
        Err(io::Error::last_os_error())
    }
}

/// A line on standard error, where a command's diagnosis goes.
fn say(line: &str) {
    let _ = writeln!(io::stderr(), "{line}");
}
