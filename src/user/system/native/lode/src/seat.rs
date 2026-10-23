//! The seat's owner's socket (`docs/LOGGING.md` §19.1, step 2a).
//!
//! `/run/lode/seat`, made by lode while it is still root and left root's,
//! mode 0600, so only root can connect; lode also refuses a peer whose uid is
//! not 0. A seat's owner -- `sessiond` -- writes one line when a session
//! begins and one when it ends ([`ferrix_lode_core::session`]). lode keeps
//! the open sessions in `/run/lode/sessions`, its own file, so a restarted
//! lode takes them up again.

use alloc::string::String;
use alloc::vec::Vec;

use ferrix_lode_core::ingest::{self, Now};
use ferrix_lode_core::record::Record;
use ferrix_lode_core::session::{self, Sessions};

use crate::sys;

/// The socket.
pub(crate) const SOCKET: &[u8] = b"/run/lode/seat";
/// The sessions open, for a restarted lode.
const KEPT: &[u8] = b"/run/lode/sessions";
/// The most a connection may hold unread before lode drops it.
const LINE_MAX: usize = 4096;
/// `MESSAGE_ID` of a record saying a session began or ended.
pub(crate) const MESSAGE_ID_SESSION: &str = "5e55f0a1b2c34d5e6f708192a3b4c5d6";

/// The socket, its connections and the sessions.
pub(crate) struct Seat {
    /// The listening socket, if it could be made.
    listener: Option<usize>,
    /// Each connection, and what it has sent of a line so far.
    connections: Vec<(usize, Vec<u8>)>,
    /// The sessions open now.
    sessions: Sessions,
}

impl Seat {
    /// Make the socket (as root) and take up the sessions kept.
    pub(crate) fn open() -> Seat {
        let listener = sys::listen_at(SOCKET).ok();
        if listener.is_some() {
            let _ = sys::chmod(SOCKET, 0o600);
        }
        let mut sessions = Sessions::default();
        if let Some(bytes) = sys::read_small(KEPT) {
            for line in String::from_utf8_lossy(&bytes).lines() {
                if let Some(said) = session::parse(line) {
                    sessions.take(said, &mut Record::new(6, 0, 0));
                }
            }
        }
        Seat {
            listener,
            connections: Vec::new(),
            sessions,
        }
    }

    /// Every descriptor to wait on.
    pub(crate) fn fds(&self) -> Vec<usize> {
        self.listener
            .iter()
            .copied()
            .chain(self.connections.iter().map(|(fd, _)| *fd))
            .collect()
    }

    /// Accept, read, and turn every whole line into a record in `out`.
    pub(crate) fn serve(&mut self, now: Now, out: &mut Vec<Record>) {
        if let Some(listener) = self.listener {
            while let Some(fd) = sys::accept(listener) {
                if sys::peer_uid(fd) == Some(0) {
                    self.connections.push((fd, Vec::new()));
                } else {
                    sys::close(fd);
                }
            }
        }
        let mut changed = false;
        let mut buf = [0_u8; 1024];
        let mut keep = Vec::new();
        for (fd, mut pending) in core::mem::take(&mut self.connections) {
            let mut open = true;
            loop {
                match sys::read(fd, &mut buf) {
                    Ok(0) => {
                        open = false;
                        break;
                    }
                    Ok(got) => pending.extend_from_slice(buf.get(..got).unwrap_or_default()),
                    Err(error) if error.0 == sys::EINTR => {}
                    Err(_) => break,
                }
            }
            while let Some(end) = pending.iter().position(|&byte| byte == b'\n') {
                let line: Vec<u8> = pending.drain(..=end).collect();
                if let Some(said) = core::str::from_utf8(&line).ok().and_then(session::parse) {
                    let mut record = ingest::own(5, MESSAGE_ID_SESSION, "", now);
                    record.fields.retain(|field| field.name != "MESSAGE");
                    self.sessions.take(said, &mut record);
                    out.push(record);
                    changed = true;
                }
            }
            if open && pending.len() <= LINE_MAX {
                keep.push((fd, pending));
            } else {
                sys::close(fd);
            }
        }
        self.connections = keep;
        if changed {
            self.keep();
        }
    }

    /// The sessions open now.
    pub(crate) fn sessions(&self) -> &Sessions {
        &self.sessions
    }

    /// Stamp `record` with the session it came from.
    pub(crate) fn stamp(&self, record: &mut Record) {
        self.sessions.stamp(record);
    }

    /// Write the open sessions to [`KEPT`], whole or not at all.
    fn keep(&self) {
        let mut text = String::new();
        for open in self.sessions.open() {
            text.push_str(&session::line(open));
        }
        let fresh = b"/run/lode/sessions.new";
        if let Ok(fd) = sys::open(fresh, sys::O_WRONLY | sys::O_CREAT | sys::O_TRUNC) {
            let written = sys::write_all(fd, text.as_bytes());
            sys::close(fd);
            if written.is_ok() {
                let _ = sys::rename(fresh, KEPT);
            }
        }
    }
}
