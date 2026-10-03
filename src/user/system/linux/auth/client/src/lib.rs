//! Talking to `authd` (`docs/AUTH.md` §3.3): the client half every program
//! that authenticates someone shares.
//!
//! [`Connection`] is one `SOCK_SEQPACKET` connection to
//! [`ferrix_auth_proto::SOCKET`], which carries one conversation.
//! [`converse`] runs it: it sends BEGIN, asks a [`Person`] each PROMPT and
//! shows them each INFO and ERROR, and returns the [`Verdict`]. A client
//! never sees a hash and never decides anything itself; it carries a secret
//! from a person to `authd` and a verdict back.
//!
//! [`Terminal`] is the [`Person`] a command-line program has: the
//! controlling terminal with its echo off for a secret when standard input
//! is that terminal, or else one line of standard input per prompt, which is
//! how a script or a gate answers.

use std::ffi::CString;
use std::io::{self, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};
use std::path::Path;

use ferrix_auth_proto::{MAX_RECORD, Record, Response, SOCKET, Secret};

/// One connection to `authd`.
#[derive(Debug)]
pub struct Connection {
    fd: OwnedFd,
}

/// How a conversation ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The person is who they said.
    Accepted {
        /// Their uid.
        uid: u32,
        /// Their account.
        account: String,
    },
    /// They are not, or must wait.
    Failed {
        /// When the next attempt will be looked at.
        retry_after_ms: u32,
        /// What to show them.
        text: String,
    },
    /// No verdict could be had.
    Unavailable(String),
}

/// What an account has, from STATUS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct State {
    /// Whether any credential is set.
    pub credential: bool,
    /// The methods it has, as `ferrix_auth_proto::method` bits.
    pub methods: u32,
    /// How long it is throttled for.
    pub throttled_ms: u64,
}

/// What an answer to a one-record request was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// STATE.
    State(State),
    /// Any verdict.
    Verdict(Verdict),
}

/// Whoever the conversation is with.
pub trait Person {
    /// Ask them `text`; `visible` is false for a secret, whose typing must
    /// not be shown. `None` when they will not answer.
    fn ask(&mut self, visible: bool, text: &str) -> Option<Secret>;
    /// Show them `text`; `error` for a problem rather than news.
    fn tell(&mut self, text: &str, error: bool);
}

impl Connection {
    /// Connect to `authd` where it listens.
    ///
    /// # Errors
    ///
    /// The socket's error: `authd` not running is `ENOENT` or
    /// `ECONNREFUSED`.
    pub fn open() -> io::Result<Connection> {
        Connection::open_at(Path::new(SOCKET))
    }

    /// Connect to an `authd` listening at `path`.
    ///
    /// # Errors
    ///
    /// As [`Connection::open`], and `ENAMETOOLONG` for a path that does
    /// not fit a `sockaddr_un`.
    pub fn open_at(path: &Path) -> io::Result<Connection> {
        let fd = seqpacket_socket()?;
        let address = unix_address(path)?;
        // SAFETY: `address` is a valid, initialised `sockaddr_un` and the
        // length given is its size; `fd` is an open socket this owns.
        let status = unsafe {
            libc::connect(
                fd.as_raw_fd(),
                (&raw const address).cast::<libc::sockaddr>(),
                socklen::<libc::sockaddr_un>(),
            )
        };
        if status != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Connection { fd })
    }

    /// The uid of whoever listens at the other end, as `SO_PEERCRED` recorded
    /// it when the socket was set to listen: the init's (0) for a socket it
    /// activates, or `authd`'s own. A set-uid program checks it before it
    /// sends a byte, so a server a user started cannot answer it.
    ///
    /// # Errors
    ///
    /// The socket's error.
    pub fn peer_uid(&self) -> io::Result<u32> {
        let mut credentials = libc::ucred {
            pid: 0,
            uid: u32::MAX,
            gid: u32::MAX,
        };
        let mut len = socklen::<libc::ucred>();
        // SAFETY: `credentials` is valid for writes of `len` bytes, its own
        // size, and `len` for a write of its own.
        let status = unsafe {
            libc::getsockopt(
                self.fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&raw mut credentials).cast(),
                &raw mut len,
            )
        };
        if status != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(credentials.uid)
    }

    /// A connection over `fd`, already connected: for `authd`'s own tests.
    #[must_use]
    pub fn from_fd(fd: OwnedFd) -> Connection {
        Connection { fd }
    }

    /// Send one record. A RESPOND's bytes are zeroed in the buffer after.
    ///
    /// # Errors
    ///
    /// `EINVAL` for a record that will not encode, else the socket's error.
    pub fn send(&self, record: &Record<'_>) -> io::Result<()> {
        let mut packet = [0_u8; MAX_RECORD];
        let sent = record
            .encode(&mut packet)
            .map_err(|why| io::Error::new(io::ErrorKind::InvalidInput, format!("{why:?}")))
            .and_then(|len| send_packet(self.fd.as_raw_fd(), packet.get(..len).unwrap_or(&[])));
        packet.fill(0);
        let _ = std::hint::black_box(&packet);
        sent
    }

    /// Receive one record into `buffer`, which it borrows from.
    ///
    /// # Errors
    ///
    /// `UnexpectedEof` when `authd` closed the connection, `InvalidData` for
    /// a packet that is not a record, else the socket's error.
    pub fn receive<'b>(&self, buffer: &'b mut [u8; MAX_RECORD + 1]) -> io::Result<Record<'b>> {
        // SAFETY: `buffer` is valid for writes of its whole length, which is
        // what is passed; `fd` is an open socket this owns.
        let got = unsafe {
            libc::recv(
                self.fd.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                0,
            )
        };
        let got = usize::try_from(got).map_err(|_| io::Error::last_os_error())?;
        if got == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "authd closed the connection",
            ));
        }
        let packet = buffer.get(..got).unwrap_or(&[]);
        Record::decode(packet)
            .map_err(|why| io::Error::new(io::ErrorKind::InvalidData, format!("{why:?}")))
    }

    /// Send a one-record request and read its answer.
    ///
    /// # Errors
    ///
    /// As [`Connection::send`] and [`Connection::receive`], and
    /// `InvalidData` for an answer that ends nothing.
    pub fn request(&self, record: &Record<'_>) -> io::Result<Answer> {
        self.send(record)?;
        let mut buffer = [0_u8; MAX_RECORD + 1];
        match self.receive(&mut buffer)? {
            Record::State {
                credential,
                methods,
                throttled_ms,
            } => Ok(Answer::State(State {
                credential,
                methods,
                throttled_ms,
            })),
            other => verdict(&other).map(Answer::Verdict).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "authd answered out of turn")
            }),
        }
    }
}

/// The verdict a record is, if it is one.
fn verdict(record: &Record<'_>) -> Option<Verdict> {
    match *record {
        Record::Accepted { uid, account } => Some(Verdict::Accepted {
            uid,
            account: account.to_owned(),
        }),
        Record::Failed {
            retry_after_ms,
            text,
        } => Some(Verdict::Failed {
            retry_after_ms,
            text: text.to_owned(),
        }),
        Record::Unavailable(text) => Some(Verdict::Unavailable(text.to_owned())),
        _ => None,
    }
}

/// Run one conversation on `connection` for `service` and `account` (empty
/// for the caller's own), with `person` answering.
///
/// # Errors
///
/// The connection's errors. A person who will not answer ends the
/// conversation with CANCEL and the verdict UNAVAILABLE.
pub fn converse(
    connection: &Connection,
    service: &str,
    account: &str,
    person: &mut dyn Person,
) -> io::Result<Verdict> {
    connection.send(&Record::Begin {
        service,
        account,
        method: "",
    })?;
    loop {
        let mut buffer = [0_u8; MAX_RECORD + 1];
        let record = connection.receive(&mut buffer)?;
        if let Some(verdict) = verdict(&record) {
            return Ok(verdict);
        }
        match record {
            Record::Prompt { visible, text } => {
                let Some(answer) = person.ask(visible, text) else {
                    connection.send(&Record::Cancel)?;
                    return Ok(Verdict::Unavailable("no answer was given".to_owned()));
                };
                connection.send(&Record::Respond(Response(answer.expose())))?;
            }
            Record::Info(text) => person.tell(text, false),
            Record::Error(text) => person.tell(text, true),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "authd sent a record a conversation does not have",
                ));
            }
        }
    }
}

/// A person at a command line.
#[derive(Debug, Default)]
pub struct Terminal {
    /// Whether standard input's end has been reached.
    ended: bool,
    /// Whether only the controlling terminal may answer: a set-uid program's
    /// secret is never read from a pipe.
    tty_only: bool,
}

impl Terminal {
    /// The person at this program's terminal, or at its standard input.
    #[must_use]
    pub fn new() -> Terminal {
        Terminal::default()
    }

    /// The person at the terminal that is this program's standard input, and
    /// nowhere else: a secret is never read from a pipe or a file, and with
    /// no terminal the answer is none. `/dev/tty` is not opened: on Ferrix it
    /// is the console whatever the caller's controlling terminal, so a
    /// program in a pty would ask on the wrong screen.
    #[must_use]
    pub fn tty_only() -> Terminal {
        Terminal {
            ended: false,
            tty_only: true,
        }
    }
}

impl Person for Terminal {
    fn ask(&mut self, visible: bool, text: &str) -> Option<Secret> {
        let mut err = io::stderr().lock();
        let _ = write!(err, "{text}");
        let _ = err.flush();
        drop(err);
        // The terminal only when standard input is one: a secret piped in
        // by a script is read from the pipe, not waited for on the console.
        let tty = if self.tty_only {
            is_terminal(libc::STDIN_FILENO)
                .then(|| {
                    std::os::fd::AsFd::as_fd(&io::stdin())
                        .try_clone_to_owned()
                        .ok()
                        .map(std::fs::File::from)
                })
                .flatten()
        } else {
            is_terminal(libc::STDIN_FILENO)
                .then(|| {
                    std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open("/dev/tty")
                        .ok()
                })
                .flatten()
        };
        let answer = match tty {
            Some(file) => read_from_terminal(&file, visible),
            None if self.ended || self.tty_only => None,
            None => {
                let answer = read_line(&mut io::stdin().lock());
                self.ended = answer.is_none();
                answer
            }
        };
        if !visible {
            let _ = writeln!(io::stderr());
        }
        answer
    }

    fn tell(&mut self, text: &str, _error: bool) {
        let _ = writeln!(io::stderr(), "{text}");
    }
}

/// Whether `fd` is a terminal.
fn is_terminal(fd: RawFd) -> bool {
    // SAFETY: `isatty` reads nothing through its argument; any number is
    // safe to ask about.
    unsafe { libc::isatty(fd) == 1 }
}

/// One line from a terminal, its echo off for a secret and restored after.
fn read_from_terminal(file: &std::fs::File, visible: bool) -> Option<Secret> {
    let fd = file.as_raw_fd();
    let mut saved = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: `saved` is valid for a `termios` write; `fd` is open.
    let got = unsafe { libc::tcgetattr(fd, saved.as_mut_ptr()) } == 0;
    // SAFETY: `tcgetattr` succeeded, so it filled `saved` in.
    let saved = got.then(|| unsafe { saved.assume_init() });
    if let (Some(original), false) = (saved, visible) {
        let mut quiet = original;
        quiet.c_lflag &= !(libc::ECHO | libc::ECHONL);
        quiet.c_lflag |= libc::ICANON;
        // SAFETY: `quiet` is a valid `termios` copied from the terminal's own.
        let _ = unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &raw const quiet) };
    }
    let mut reader = io::BufReader::new(file);
    let answer = read_line(&mut reader);
    if let (Some(original), false) = (saved, visible) {
        // SAFETY: `original` is what `tcgetattr` gave.
        let _ = unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &raw const original) };
    }
    answer
}

/// One line, its newline taken off, as a secret; `None` at end of input.
/// The line's buffer is zeroed after.
fn read_line(reader: &mut dyn io::BufRead) -> Option<Secret> {
    let mut line = Vec::with_capacity(ferrix_auth_proto::MAX_SECRET + 2);
    let read = reader.read_until(b'\n', &mut line).ok()?;
    let secret = (read > 0).then(|| {
        let text = line.strip_suffix(b"\n").unwrap_or(&line);
        let text = text.strip_suffix(b"\r").unwrap_or(text);
        Secret::from_bytes(text)
    });
    line.fill(0);
    let _ = std::hint::black_box(&line);
    secret.flatten()
}

/// A new `SOCK_SEQPACKET` Unix socket, closed on exec.
///
/// # Errors
///
/// `socket`'s error.
pub fn seqpacket_socket() -> io::Result<OwnedFd> {
    // SAFETY: plain constants; the call allocates a descriptor or fails.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `socket` just returned `fd`, and nothing else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// The `sockaddr_un` for `path`.
///
/// # Errors
///
/// `ENAMETOOLONG` when it does not fit, `EINVAL` for a NUL in it.
pub fn unix_address(path: &Path) -> io::Result<libc::sockaddr_un> {
    let bytes = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: an all-zero `sockaddr_un` is a valid value of the type.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let path = bytes.as_bytes_with_nul();
    if path.len() > address.sun_path.len() {
        return Err(io::Error::from_raw_os_error(libc::ENAMETOOLONG));
    }
    for (slot, &byte) in address.sun_path.iter_mut().zip(path) {
        *slot = byte as libc::c_char;
    }
    Ok(address)
}

/// The size of `T` as a `socklen_t`.
#[must_use]
pub fn socklen<T>() -> libc::socklen_t {
    libc::socklen_t::try_from(size_of::<T>()).unwrap_or(0)
}

/// Send one packet whole.
///
/// # Errors
///
/// `send`'s error, or `EMSGSIZE` for a packet sent in part.
pub fn send_packet(fd: RawFd, packet: &[u8]) -> io::Result<()> {
    // SAFETY: `packet` is valid for reads of its length; `MSG_NOSIGNAL`
    // makes a closed peer an `EPIPE` rather than a signal.
    let sent = unsafe { libc::send(fd, packet.as_ptr().cast(), packet.len(), libc::MSG_NOSIGNAL) };
    let sent = usize::try_from(sent).map_err(|_| io::Error::last_os_error())?;
    if sent == packet.len() {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(libc::EMSGSIZE))
    }
}
