//! init's socket: units' standard output and error, a line at a time, with
//! who wrote it (`ferrix_lode_core::units`, `docs/LOGGING.md` §4.2).
//!
//! `/run/lode/units`, made by lode while it is still root and left root's,
//! mode 0600; lode also refuses a peer whose uid is not 0, so only init --
//! which made the pipes and knows whose they are -- says what goes into the
//! trusted fields here.

use alloc::vec::Vec;

use ferrix_lode_core::ingest::Now;
use ferrix_lode_core::record::Record;
use ferrix_lode_core::units::{self, Pairing};

use crate::sys;

/// The most a connection may hold of a line before lode drops it.
const LINE_MAX: usize = 64 * 1024;

/// The socket, its connections, and the lines waiting for their console
/// copies.
pub(crate) struct Units {
    /// The listening socket, if it could be made.
    listener: Option<usize>,
    /// Each connection, and what it has sent of a line so far.
    connections: Vec<(usize, Vec<u8>)>,
    /// Trusted lines and console copies, paired.
    pub(crate) pairing: Pairing,
    /// The program each pid init started runs, newest last.
    programs: alloc::collections::VecDeque<(u32, Vec<u8>)>,
}

impl Units {
    /// The program init said `pid` runs.
    pub(crate) fn program(&self, pid: u32) -> Option<&[u8]> {
        self.programs
            .iter()
            .rev()
            .find(|(known, _)| *known == pid)
            .map(|(_, path)| path.as_slice())
    }

    /// Make the socket (as root).
    pub(crate) fn open() -> Units {
        let listener = sys::listen_at(units::SOCKET.as_bytes()).ok();
        if listener.is_some() {
            let _ = sys::chmod(units::SOCKET.as_bytes(), 0o600);
        }
        Units {
            listener,
            connections: Vec::new(),
            pairing: Pairing::default(),
            programs: alloc::collections::VecDeque::new(),
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

    /// Accept, read, and hand every whole line to the pairing; what must be
    /// stored at once goes into `out`.
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
        let mut buf = alloc::vec![0_u8; 16 * 1024];
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
            let mut used = 0;
            while let Some(end) = pending
                .get(used..)
                .and_then(|rest| rest.iter().position(|&byte| byte == b'\n'))
            {
                let line = pending.get(used..used + end).unwrap_or_default();
                if let Some((pid, path)) = units::decode_program(line) {
                    remember(&mut self.programs, pid, path);
                } else if let Some(said) = units::decode(line) {
                    let record = units::record(&said, now);
                    self.pairing.trusted(record, now.monotonic_us, out);
                }
                used += end + 1;
            }
            let _ = pending.drain(..used);
            if open && pending.len() <= LINE_MAX {
                keep.push((fd, pending));
            } else {
                sys::close(fd);
            }
        }
        self.connections = keep;
    }
}

/// Keep `pid`'s program, the oldest forgotten past 1,024.
fn remember(programs: &mut alloc::collections::VecDeque<(u32, Vec<u8>)>, pid: u32, path: Vec<u8>) {
    programs.retain(|(known, _)| *known != pid);
    if programs.len() >= 1024 {
        let _ = programs.pop_front();
    }
    programs.push_back((pid, path));
}
