//! lode's control socket, `/run/lode/control` (`docs/LOGGING.md` §9.6):
//! what journalctl's `--sync`, `--flush` and `--rotate` ask journald.
//!
//! Made by lode while it is still root and left root's, mode 0600, so only
//! root asks; lode also refuses a peer whose uid is not 0. A request is one
//! line, `SYNC`, `FLUSH` or `ROTATE`; lode answers `OK` or `ERR <why>` once
//! it is done, and closes. A request that does not come within a few
//! seconds of the connection is dropped, so a client cannot hold lode.

use alloc::string::String;
use alloc::vec::Vec;

use crate::sys;

/// The socket.
pub(crate) const SOCKET: &[u8] = b"/run/lode/control";

/// How long a connection may take to ask, in microseconds.
const DEADLINE_US: u64 = 5_000_000;

/// What a client asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Request {
    /// Write everything held to the disk, and wait for it.
    Sync,
    /// Move the RAM tier to the disk, and keep writing there.
    Flush,
    /// Seal every segment and begin new ones.
    Rotate,
}

impl Request {
    /// The line a client sends.
    pub(crate) fn line(self) -> &'static [u8] {
        match self {
            Request::Sync => b"SYNC\n",
            Request::Flush => b"FLUSH\n",
            Request::Rotate => b"ROTATE\n",
        }
    }

    /// A line read, without its newline.
    fn read(line: &[u8]) -> Option<Request> {
        Some(match line {
            b"SYNC" => Request::Sync,
            b"FLUSH" => Request::Flush,
            b"ROTATE" => Request::Rotate,
            _ => return None,
        })
    }
}

/// The socket and the connections that have not asked yet.
pub(crate) struct Control {
    /// The listening socket, if it could be made.
    listener: Option<usize>,
    /// Each connection, when it was accepted, and what it sent so far.
    waiting: Vec<(usize, u64, Vec<u8>)>,
}

impl Control {
    /// Make the socket (as root).
    pub(crate) fn open() -> Control {
        let listener = sys::listen_at(SOCKET).ok();
        if listener.is_some() {
            let _ = sys::chmod(SOCKET, 0o600);
        }
        Control {
            listener,
            waiting: Vec::new(),
        }
    }

    /// Every descriptor to wait on.
    pub(crate) fn fds(&self) -> Vec<usize> {
        self.listener
            .iter()
            .copied()
            .chain(self.waiting.iter().map(|(fd, _, _)| *fd))
            .collect()
    }

    /// Accept and read: the requests whole now, each with its connection,
    /// which [`answer`] closes.
    pub(crate) fn poll(&mut self, now_us: u64) -> Vec<(usize, Request)> {
        if let Some(listener) = self.listener {
            while let Some(fd) = sys::accept(listener) {
                if sys::peer_uid(fd) == Some(0) {
                    self.waiting.push((fd, now_us, Vec::new()));
                } else {
                    let _ = sys::write_all(fd, b"ERR root's\n");
                    sys::close(fd);
                }
            }
        }
        let mut asked = Vec::new();
        let mut keep = Vec::new();
        let mut buf = [0_u8; 64];
        for (fd, since, mut pending) in core::mem::take(&mut self.waiting) {
            if let Ok(got) = sys::read(fd, &mut buf) {
                pending.extend_from_slice(buf.get(..got).unwrap_or_default());
            }
            if let Some(end) = pending.iter().position(|&byte| byte == b'\n') {
                match Request::read(pending.get(..end).unwrap_or_default()) {
                    Some(request) => asked.push((fd, request)),
                    None => answer(fd, Err(String::from("not a request lode knows"))),
                }
            } else if pending.len() > 32 || now_us.saturating_sub(since) > DEADLINE_US {
                sys::close(fd);
            } else {
                keep.push((fd, since, pending));
            }
        }
        self.waiting = keep;
        asked
    }
}

/// Say how a request ended, and close its connection.
pub(crate) fn answer(fd: usize, result: Result<(), String>) {
    let line = match result {
        Ok(()) => String::from("OK\n"),
        Err(why) => alloc::format!("ERR {why}\n"),
    };
    let _ = sys::write_all(fd, line.as_bytes());
    sys::close(fd);
}
