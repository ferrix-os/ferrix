//! The control socket (§10): `/run/ferrix/control`, where `svc` asks.
//!
//! A stream socket anyone may connect to. `SO_PEERCRED` says who connected:
//! anyone may read (`status`, `list`, `log`); only root may change state,
//! except that a user may group their own processes in a scope under their
//! own `user-<uid>.slice`. Each connection carries one call and init's
//! answers to it, the last one final, and then init closes it.
//!
//! Nothing here blocks. A connection is read as bytes arrive and written as
//! the socket takes them; a client that stops reading is dropped once its
//! unsent answers pass [`MAX_UNSENT`], so no client can hold pid 1.

use std::collections::BTreeMap;
use std::io::{self, Read as _, Write as _};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};

use ferrix_svc_proto::control::{Answer, Call, Framer, SOCKET};

use crate::sys;

/// The most init holds unsent for one client.
const MAX_UNSENT: usize = 4 << 20;

/// One connection.
#[derive(Debug)]
struct Client {
    stream: UnixStream,
    uid: u32,
    /// The process that connected, as `SO_PEERCRED` says: whose seat it
    /// speaks for (`crate::seat`). 0 when the kernel names none.
    pid: u32,
    framer: Framer,
    /// Bytes waiting for the socket to take them.
    unsent: Vec<u8>,
    /// Whether the call has been read, so more bytes are not a second one.
    asked: bool,
    /// Whether the final answer is queued, so the connection closes once
    /// it is sent.
    answered: bool,
}

/// What a connection did, for the event loop.
#[derive(Debug)]
pub(crate) enum Heard {
    /// A call, with the uid and the pid of whoever made it.
    Call {
        client: u64,
        uid: u32,
        pid: u32,
        call: Call,
    },
    /// The connection went, or spoke something else; it is closed.
    Gone,
    /// Nothing whole yet.
    Nothing,
}

/// The listening socket and its connections.
#[derive(Debug)]
pub(crate) struct Control {
    listener: UnixListener,
    clients: BTreeMap<u64, Client>,
    next: u64,
}

impl Control {
    /// Listen at [`SOCKET`], writable by everyone: `SO_PEERCRED` and the
    /// calls decide what each may do.
    pub(crate) fn listen() -> io::Result<Control> {
        let _ = std::fs::remove_file(SOCKET);
        let listener = UnixListener::bind(SOCKET)?;
        listener.set_nonblocking(true)?;
        std::fs::set_permissions(SOCKET, std::fs::Permissions::from_mode(0o666))?;
        Ok(Control {
            listener,
            clients: BTreeMap::new(),
            // 0 is a signal to pid 1, which is answered by nobody.
            next: 1,
        })
    }

    /// The listener's descriptor, to watch.
    pub(crate) fn fd(&self) -> RawFd {
        self.listener.as_raw_fd()
    }

    /// Accept every waiting connection; return each one's number and
    /// descriptor, for the caller to watch.
    pub(crate) fn accept(&mut self) -> Vec<(u64, RawFd)> {
        let mut accepted = Vec::new();
        loop {
            let stream = match self.listener.accept() {
                Ok((stream, _)) => stream,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            if stream.set_nonblocking(true).is_err() {
                continue;
            }
            let Ok((pid, uid)) = sys::peer_cred(stream.as_raw_fd()) else {
                continue;
            };
            let id = self.next;
            self.next += 1;
            accepted.push((id, stream.as_raw_fd()));
            let _ = self.clients.insert(
                id,
                Client {
                    stream,
                    uid,
                    pid,
                    framer: Framer::new(),
                    unsent: Vec::new(),
                    asked: false,
                    answered: false,
                },
            );
        }
        accepted
    }

    /// Read what client `id` sent.
    pub(crate) fn read(&mut self, id: u64) -> Heard {
        let Some(client) = self.clients.get_mut(&id) else {
            return Heard::Gone;
        };
        let mut buffer = [0_u8; 4096];
        loop {
            match client.stream.read(&mut buffer) {
                Ok(0) => {
                    // The client has finished writing. It may still be
                    // waiting for its answer, so it stays until that is
                    // sent -- unless it never asked.
                    if client.asked {
                        return Heard::Nothing;
                    }
                    let _ = self.clients.remove(&id);
                    return Heard::Gone;
                }
                Ok(count) => client.framer.push(buffer.get(..count).unwrap_or_default()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    let _ = self.clients.remove(&id);
                    return Heard::Gone;
                }
            }
        }
        if client.asked {
            return Heard::Nothing;
        }
        match client.framer.next_record() {
            Ok(Some(record)) => match Call::decode(&record) {
                Ok(call) => {
                    client.asked = true;
                    Heard::Call {
                        client: id,
                        uid: client.uid,
                        pid: client.pid,
                        call,
                    }
                }
                Err(_) => {
                    let _ = self.clients.remove(&id);
                    Heard::Gone
                }
            },
            Ok(None) => Heard::Nothing,
            Err(_) => {
                let _ = self.clients.remove(&id);
                Heard::Gone
            }
        }
    }

    /// Queue `answer` for client `id` and send what the socket takes.
    /// Returns whether bytes are still waiting, for the caller to watch
    /// the socket for room; `None` once the connection is done with.
    pub(crate) fn answer(&mut self, id: u64, answer: &Answer) -> Option<bool> {
        let client = self.clients.get_mut(&id)?;
        if client.answered {
            return Some(!client.unsent.is_empty());
        }
        client.unsent.extend_from_slice(&answer.encode());
        client.answered = answer.is_final();
        if client.unsent.len() > MAX_UNSENT {
            let _ = self.clients.remove(&id);
            return None;
        }
        self.flush(id)
    }

    /// Send what client `id` has waiting. Returns whether bytes are still
    /// waiting; `None` once the connection is done with and closed.
    pub(crate) fn flush(&mut self, id: u64) -> Option<bool> {
        let client = self.clients.get_mut(&id)?;
        while !client.unsent.is_empty() {
            match client.stream.write(&client.unsent) {
                Ok(0) => break,
                Ok(count) => {
                    let _ = client.unsent.drain(..count);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Some(true),
                Err(_) => {
                    let _ = self.clients.remove(&id);
                    return None;
                }
            }
        }
        if client.answered && client.unsent.is_empty() {
            let _ = self.clients.remove(&id);
            return None;
        }
        Some(!client.unsent.is_empty())
    }

    /// The descriptor of client `id`, while it is connected.
    pub(crate) fn client_fd(&self, id: u64) -> Option<RawFd> {
        self.clients
            .get(&id)
            .map(|client| client.stream.as_raw_fd())
    }
}
