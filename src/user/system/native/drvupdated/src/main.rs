//! `drvupdated`: how a new version of a driver reaches `devmgr`
//! (`docs/DEVMGR.md` §4.1).
//!
//! `devmgr` reads no files, and its loop waits on one port, which cannot
//! watch a descriptor. This helper does the I/O for it. `devmgr` starts it
//! from the image the initramfs carries in `/lib/drivers`, with a channel
//! whose other end `devmgr` keeps. The helper listens on the abstract
//! `AF_UNIX` name `ferrix.devmgr.update`. It takes one connection at a time,
//! closes any peer whose `SO_PEERCRED` uid is not root's, and reads a
//! request and the image's bytes into a VMO of its own making. It hands the
//! request and the VMO to `devmgr` and writes `devmgr`'s answer back to the
//! client. `/bin/drvupdate` is the client.
//!
//! It decides nothing. `devmgr` copies the image before it looks at it,
//! proves it loads, and does the swap and the rollback.
//!
//! The exit status names the step that failed (see [`Step`]); a helper that
//! exits leaves the machine without updates, and `devmgr` says so.

#![no_std]
#![no_main]

use ferrix_drvupdate_proto::{ANSWER_BYTES, Answer, Outcome, REQUEST_BYTES, Request, SOCKET_NAME};
use ferrix_rt::native::channel::Channel;
use ferrix_rt::native::{Deadline, Handle, Object, Signals, vmo};
use ferrix_rt::{Bootstrap, Kernel, linux};

ferrix_rt::entry!(main);

/// Where the helper gave up, as its exit status.
#[repr(i32)]
enum Step {
    /// Started with no channel to `devmgr`.
    NoBootstrap = 1,
    /// The socket could not be made, bound or listened on.
    Socket = 2,
    /// `accept4` failed.
    Accept = 3,
}

/// Root's uid: the only peer an update is taken from.
const ROOT: u32 = 0;

/// Connections that may wait while one is served.
const BACKLOG: usize = 4;

/// How long one read of the client's may wait, in seconds: a client that
/// stops sending is closed rather than left holding the helper.
const READ_PATIENCE: usize = 10;

/// Bytes read from the socket at a time.
const CHUNK: usize = 2048;

fn main(bootstrap: Bootstrap) -> i32 {
    let Some(devmgr) = bootstrap else {
        return Step::NoBootstrap as i32;
    };
    let Some(listener) = listen() else {
        return Step::Socket as i32;
    };
    loop {
        let Ok(peer) = linux::accept4(listener, 0) else {
            return Step::Accept as i32;
        };
        let answer = serve(&devmgr, peer);
        let _ = write_all(peer, &answer.encode());
        let _ = linux::close(peer);
    }
}

/// Bind [`SOCKET_NAME`] and listen on it.
fn listen() -> Option<usize> {
    let fd = linux::socket(linux::AF_UNIX, linux::SOCK_STREAM, 0).ok()?;
    let (address, len) = linux::sockaddr_un_abstract(SOCKET_NAME)?;
    let _bound = linux::bind(fd, &address, len).ok()?;
    let _listening = linux::listen(fd, BACKLOG).ok()?;
    Some(fd)
}

/// One request: the peer's uid, the header, the image into a VMO, and
/// `devmgr`'s answer.
fn serve(devmgr: &Channel<Kernel>, peer: usize) -> Answer {
    if linux::peer_uid(peer) != Ok(ROOT) {
        return Answer::of(Outcome::NotAllowed);
    }
    if linux::receive_timeout(peer, READ_PATIENCE).is_err() {
        return Answer::of(Outcome::Malformed);
    }
    let mut header = [0_u8; REQUEST_BYTES];
    if !read_exact(peer, &mut header) {
        return Answer::of(Outcome::Malformed);
    }
    let Ok(request) = Request::decode(&header) else {
        return Answer::of(Outcome::Malformed);
    };
    let Ok(length) = usize::try_from(request.length) else {
        return Answer::of(Outcome::Malformed);
    };
    let Ok(image) = vmo::create(Kernel, length) else {
        return Answer::of(Outcome::Unanswered);
    };
    let mut chunk = [0_u8; CHUNK];
    let mut offset = 0_u64;
    while offset < request.length {
        let left = usize::try_from(request.length - offset).unwrap_or(CHUNK);
        let Some(room) = chunk.get_mut(..left.min(CHUNK)) else {
            return Answer::of(Outcome::Malformed);
        };
        let got = match linux::read(peer, room) {
            Ok(0) | Err(_) => return Answer::of(Outcome::Malformed),
            Ok(got) => got,
        };
        if image
            .write(chunk.get(..got).unwrap_or_default(), offset)
            .is_err()
        {
            return Answer::of(Outcome::Unanswered);
        }
        offset += got as u64;
    }
    ask(devmgr, &header, image)
}

/// Hand the request and its image to `devmgr`, and wait for its answer: an
/// update takes as long as the new driver takes to publish, or the deadline
/// `devmgr` gives it and the old driver's start after it.
fn ask(devmgr: &Channel<Kernel>, header: &[u8; REQUEST_BYTES], image: vmo::Vmo<Kernel>) -> Answer {
    if devmgr.write_with(header, [image.into_owned()]).is_err() {
        return Answer::of(Outcome::Unanswered);
    }
    loop {
        if devmgr
            .wait_one(Signals::READABLE | Signals::PEER_CLOSED, Deadline::Never)
            .is_err()
        {
            return Answer::of(Outcome::Unanswered);
        }
        let mut bytes = [0_u8; ANSWER_BYTES];
        let mut handles = [Handle::INVALID; 0];
        match devmgr.read(&mut bytes, &mut handles) {
            Ok(got) => {
                return Answer::decode(bytes.get(..got.bytes).unwrap_or_default())
                    .unwrap_or(Answer::of(Outcome::Unanswered));
            }
            Err(ferrix_rt::native::channel::ReadError::Failed(
                ferrix_rt::native::Error::ShouldWait,
            )) => {}
            Err(_) => return Answer::of(Outcome::Unanswered),
        }
    }
}

/// Fill `bytes` from `fd`, or say it could not be.
fn read_exact(fd: usize, bytes: &mut [u8]) -> bool {
    let mut at = 0;
    while at < bytes.len() {
        match bytes.get_mut(at..).map(|rest| linux::read(fd, rest)) {
            Some(Ok(got)) if got > 0 => at += got,
            _ => return false,
        }
    }
    true
}

/// Write all of `bytes` to `fd`.
fn write_all(fd: usize, bytes: &[u8]) -> bool {
    let mut at = 0;
    while at < bytes.len() {
        match bytes.get(at..).map(|rest| linux::write(fd, rest)) {
            Some(Ok(sent)) if sent > 0 => at += sent,
            _ => return false,
        }
    }
    true
}
