//! A user's own segments, handed over (`ferrix_lode_core::access`,
//! `docs/LOGGING.md` §12.1).
//!
//! `/run/lode/read`, a `SOCK_SEQPACKET` socket made by lode while it is
//! still root, mode 0666: anyone may ask, and is handed only the files of
//! the uid `SO_PEERCRED` gives at `accept` -- each segment and summary in
//! `user-<uid>/` of both stores, opened by lode for reading and passed with
//! `SCM_RIGHTS`. lode reads nothing for anyone; a reader that is slow to
//! take its files keeps a connection, never lode's loop.

use alloc::string::String;
use alloc::vec::Vec;

use ferrix_lode_core::access::{self, Packet};

use crate::sys;

/// How long a connection may take to ask and be answered, in microseconds.
const DEADLINE_US: u64 = 5_000_000;

/// A connection being answered.
struct Asking {
    /// The connection.
    fd: usize,
    /// Its peer's uid.
    uid: u32,
    /// When it was accepted, on the monotonic clock.
    since_us: u64,
    /// Whether its request has come.
    asked: bool,
    /// What is left to send: each file's store directory and path.
    left: Option<Vec<(usize, String)>>,
    /// Files sent.
    sent: u64,
}

/// The socket and the connections being answered.
pub(crate) struct Handout {
    /// The listening socket, if it could be made.
    listener: Option<usize>,
    /// Connections being answered.
    asking: Vec<Asking>,
}

impl Handout {
    /// Make the socket (as root).
    pub(crate) fn open() -> Handout {
        let listener = sys::listen_kind(access::SOCKET.as_bytes(), sys::SOCK_SEQPACKET).ok();
        if listener.is_some() {
            let _ = sys::chmod(access::SOCKET.as_bytes(), 0o666);
        }
        Handout {
            listener,
            asking: Vec::new(),
        }
    }

    /// Every descriptor to wait on.
    pub(crate) fn fds(&self) -> Vec<usize> {
        self.listener
            .iter()
            .copied()
            .chain(self.asking.iter().map(|asking| asking.fd))
            .collect()
    }

    /// Whether an answer is part sent: the loop comes back soon.
    pub(crate) fn busy(&self) -> bool {
        self.asking.iter().any(|asking| asking.left.is_some())
    }

    /// Accept, read requests, and send what can be sent now. `stores` are
    /// the store directories, open.
    pub(crate) fn serve(&mut self, now_us: u64, stores: &[usize]) {
        if let Some(listener) = self.listener {
            while let Some(fd) = sys::accept(listener) {
                match sys::peer_uid(fd) {
                    Some(uid) => self.asking.push(Asking {
                        fd,
                        uid,
                        since_us: now_us,
                        asked: false,
                        left: None,
                        sent: 0,
                    }),
                    None => sys::close(fd),
                }
            }
        }
        let mut keep = Vec::new();
        for mut asking in core::mem::take(&mut self.asking) {
            if !asking.asked {
                let mut buf = [0_u8; 64];
                match sys::read(asking.fd, &mut buf) {
                    Ok(got) if buf.get(..got) == Some(access::REQUEST) => {
                        asking.asked = true;
                        asking.left = Some(files_of(asking.uid, stores));
                    }
                    Ok(_) => {
                        sys::close(asking.fd);
                        continue;
                    }
                    Err(_) => {}
                }
            }
            let done = asking.asked && send_some(&mut asking);
            if done || now_us.saturating_sub(asking.since_us) > DEADLINE_US {
                sys::close(asking.fd);
            } else {
                keep.push(asking);
            }
        }
        self.asking = keep;
    }
}

/// Every segment and summary of `uid`'s in `stores`.
fn files_of(uid: u32, stores: &[usize]) -> Vec<(usize, String)> {
    let user = access::user_dir(uid);
    let mut files = Vec::new();
    for &store in stores {
        let Ok(fd) = sys::openat(store, user.as_bytes(), sys::O_RDONLY | sys::O_CLOEXEC, 0) else {
            continue;
        };
        for name in sys::list_dir(fd).unwrap_or_default() {
            let Ok(name) = String::from_utf8(name) else {
                continue;
            };
            let segment = name.strip_suffix(".sum").unwrap_or(&name);
            if !name.starts_with('.') && ferrix_lode_core::segment::is_segment_name(segment) {
                files.push((store, alloc::format!("{user}/{name}")));
            }
        }
        sys::close(fd);
    }
    files
}

/// Send what the connection can take now; answers whether all was sent.
fn send_some(asking: &mut Asking) -> bool {
    let Some(left) = asking.left.as_mut() else {
        return false;
    };
    while let Some((store, name)) = left.first() {
        let Ok(file) = sys::openat(*store, name.as_bytes(), sys::O_RDONLY | sys::O_CLOEXEC, 0)
        else {
            // Vacuumed since it was listed: nothing to send.
            let _ = left.remove(0);
            continue;
        };
        let packet = Packet::File(name.clone()).encode();
        let sent = sys::send_with(asking.fd, &packet, Some(file));
        sys::close(file);
        match sent {
            Ok(_) => {
                let _ = left.remove(0);
                asking.sent += 1;
            }
            Err(error) if error.0 == 11 || error.0 == sys::EINTR => return false,
            Err(_) => return true,
        }
    }
    let end = Packet::End(asking.sent).encode();
    match sys::send_with(asking.fd, &end, None) {
        Ok(_) => true,
        Err(error) => error.0 != 11 && error.0 != sys::EINTR,
    }
}
