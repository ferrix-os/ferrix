//! Asking `settingsd` for a change (`docs/SETTINGS-WRITER.md` §7): the
//! half the Settings app and `settingsctl` share.
//!
//! ```no_run
//! use ferrix_settings_client::{Client, Request, Step};
//!
//! let request = Request::SetHostname { name: "box".into() };
//! let mut talk = Client::new().begin(&request, false).unwrap();
//! loop {
//!     match talk.step() {
//!         Step::Prompt { visible, text } => {
//!             # let secret = ferrix_settings_client::Secret::new();
//!             // ask the person, then:
//!             talk.answer(secret).unwrap();
//!         }
//!         Step::Info(text) => { /* show it */ }
//!         Step::Done(outcome) => break,
//!         Step::Refused(refusal) => break,
//!     }
//! }
//! ```
//!
//! [`Conversation`] is step-wise and [`AsRawFd`], so an app's event loop
//! can poll it and draw while the person types. [`Client::apply`] is the
//! blocking form, answered by authd's own [`Person`]; [`Client::dry_run`]
//! asks nobody and writes nothing.
//!
//! Before a password can be sent, the client checks that whoever listens
//! at the socket is root (`settingsd`, or the init that activated it), as
//! `su` checks `authd` (U1): a server a user started is never told one.

pub mod manifest;

use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};

pub use ferrix_auth_client::Person;
pub use ferrix_settings_proto::{
    Area, Invalid, MAX_RECORD, Refusal, RefusalKind, Request, SOCKET, SWITCHABLE_UNITS, Secret,
    ToClient, ToServer, valid_hostname,
};

use ferrix_auth_client::{send_packet, seqpacket_socket, socklen, unix_address};

/// What a finished request came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Whether anything changed, or for a dry run would.
    pub changed: bool,
    /// What was done, or would be, for a person.
    pub text: String,
}

/// One step of a conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Ask the person, then [`Conversation::answer`].
    Prompt {
        /// Whether what they type may be shown.
        visible: bool,
        /// The question.
        text: String,
    },
    /// Something to show them.
    Info(String),
    /// Over: done, or what a dry run would do.
    Done(Outcome),
    /// Over: nothing was written.
    Refused(Refusal),
}

/// Where `settingsd` is, and who must be listening there.
#[derive(Debug, Clone)]
pub struct Client {
    socket: PathBuf,
    listener_uid: u32,
}

impl Default for Client {
    fn default() -> Client {
        Client::new()
    }
}

impl Client {
    /// The machine's `settingsd`, listened for by root.
    #[must_use]
    pub fn new() -> Client {
        Client {
            socket: PathBuf::from(SOCKET),
            listener_uid: 0,
        }
    }

    /// A `settingsd` at `socket` that `listener_uid` listens for: for tests.
    #[must_use]
    pub fn at(socket: &Path, listener_uid: u32) -> Client {
        Client {
            socket: socket.to_owned(),
            listener_uid,
        }
    }

    /// Start a conversation for `request`; a dry run when `dry_run`.
    ///
    /// # Errors
    ///
    /// [`RefusalKind::Invalid`] for a request that breaks its own rules,
    /// checked here before anything is sent; [`RefusalKind::Unavailable`]
    /// when `settingsd` cannot be reached or someone else listens.
    pub fn begin(&self, request: &Request, dry_run: bool) -> Result<Conversation, Refusal> {
        request
            .validate()
            .map_err(|invalid| Refusal::new(RefusalKind::Invalid, invalid.0))?;
        self.begin_as_given(request, dry_run)
    }

    /// [`Client::begin`] without checking the request's rules here first,
    /// so that `settingsd`'s own refusal answers: for gates and tests.
    ///
    /// # Errors
    ///
    /// [`RefusalKind::Unavailable`] when `settingsd` cannot be reached or
    /// someone else listens.
    pub fn begin_as_given(
        &self,
        request: &Request,
        dry_run: bool,
    ) -> Result<Conversation, Refusal> {
        let unavailable = |what: String| Refusal::new(RefusalKind::Unavailable, what);
        let fd = connect(&self.socket)
            .map_err(|error| missing_writer(&error, &manifest::Manifest::read(), &self.socket))?;
        let listener = peer_uid(fd.as_raw_fd())
            .map_err(|error| unavailable(format!("cannot tell who listens: {error}")))?;
        if listener != self.listener_uid {
            return Err(unavailable(format!(
                "{} is held by uid {listener}, not settingsd",
                self.socket.display()
            )));
        }
        let conversation = Conversation { fd, over: false };
        let packet = ToServer::Request {
            dry_run,
            request: request.clone(),
        }
        .encode()
        .map_err(|_| Refusal::new(RefusalKind::Invalid, "the request does not fit a record"))?;
        send_packet(conversation.fd.as_raw_fd(), packet.bytes())
            .map_err(|error| unavailable(format!("writing to settingsd: {error}")))?;
        Ok(conversation)
    }

    /// Carry `request` out, asking `person` what `authd` asks.
    ///
    /// # Errors
    ///
    /// Why it was not carried out.
    pub fn apply(&self, request: &Request, person: &mut dyn Person) -> Result<Outcome, Refusal> {
        let mut talk = self.begin(request, false)?;
        loop {
            match talk.step() {
                Step::Prompt { visible, text } => match person.ask(visible, &text) {
                    Some(secret) => talk.answer(secret)?,
                    None => {
                        talk.cancel();
                        return Err(Refusal::new(RefusalKind::Cancelled, "no answer was given"));
                    }
                },
                Step::Info(text) => person.tell(&text, false),
                Step::Done(outcome) => return Ok(outcome),
                Step::Refused(refusal) => return Err(refusal),
            }
        }
    }

    /// What `request` would do, asking no one and writing nothing.
    ///
    /// # Errors
    ///
    /// Why it would not be carried out.
    pub fn dry_run(&self, request: &Request) -> Result<Outcome, Refusal> {
        let mut talk = self.begin(request, true)?;
        loop {
            match talk.step() {
                Step::Done(outcome) => return Ok(outcome),
                Step::Refused(refusal) => return Err(refusal),
                Step::Prompt { .. } => {
                    talk.cancel();
                    return Err(Refusal::new(
                        RefusalKind::Unavailable,
                        "settingsd asked a question during a dry run",
                    ));
                }
                Step::Info(_) => {}
            }
        }
    }
}

/// One request's conversation with `settingsd`.
#[derive(Debug)]
pub struct Conversation {
    fd: OwnedFd,
    over: bool,
}

impl AsRawFd for Conversation {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

impl Conversation {
    /// Wait for the next step. After [`Step::Done`] or [`Step::Refused`],
    /// every call answers [`RefusalKind::Cancelled`].
    pub fn step(&mut self) -> Step {
        if self.over {
            return Step::Refused(Refusal::new(
                RefusalKind::Cancelled,
                "the conversation is over",
            ));
        }
        let mut buffer = vec![0_u8; MAX_RECORD + 1];
        let step = match receive(self.fd.as_raw_fd(), &mut buffer) {
            Ok(packet) => match ToClient::decode(packet) {
                Ok(ToClient::Prompt { visible, text }) => Step::Prompt { visible, text },
                Ok(ToClient::Info(text)) => Step::Info(text),
                Ok(ToClient::Done { changed, text }) => Step::Done(Outcome { changed, text }),
                Ok(ToClient::Refused(refusal)) => Step::Refused(refusal),
                Err(why) => Step::Refused(Refusal::new(
                    RefusalKind::Unavailable,
                    format!("settingsd sent something that is not a record: {why:?}"),
                )),
            },
            Err(error) => Step::Refused(Refusal::new(
                RefusalKind::Unavailable,
                format!("settingsd went away: {error}"),
            )),
        };
        self.over = matches!(step, Step::Done(_) | Step::Refused(_));
        step
    }

    /// Answer the last [`Step::Prompt`]. The secret is zeroed as it goes.
    ///
    /// # Errors
    ///
    /// [`RefusalKind::Unavailable`] when it could not be sent.
    pub fn answer(&mut self, secret: Secret) -> Result<(), Refusal> {
        let packet = ToServer::Respond(secret)
            .encode()
            .map_err(|_| Refusal::new(RefusalKind::Invalid, "the answer is too long"))?;
        send_packet(self.fd.as_raw_fd(), packet.bytes()).map_err(|error| {
            Refusal::new(
                RefusalKind::Unavailable,
                format!("writing to settingsd: {error}"),
            )
        })
    }

    /// Give up: `settingsd` writes nothing.
    pub fn cancel(mut self) {
        if !self.over
            && let Ok(packet) = ToServer::Cancel.encode()
        {
            let _ = send_packet(self.fd.as_raw_fd(), packet.bytes());
        }
        self.over = true;
    }
}

/// A connected `SOCK_SEQPACKET` socket to `path`.
/// What a client says when `settingsd` cannot be reached: as the build says
/// it when there is none (ledger 3636 (16)), never a silent no-op.
fn missing_writer(error: &io::Error, manifest: &manifest::Manifest, socket: &Path) -> Refusal {
    let missing = matches!(
        error.raw_os_error(),
        Some(libc::ENOENT | libc::ECONNREFUSED)
    );
    let text = if missing && manifest.writer_absent() {
        manifest.fixed_text()
    } else if missing {
        "the settings service is not running".to_owned()
    } else {
        format!(
            "settingsd cannot be reached at {}: {error}",
            socket.display()
        )
    };
    Refusal::new(RefusalKind::Unavailable, text)
}

fn connect(path: &Path) -> io::Result<OwnedFd> {
    let fd = seqpacket_socket()?;
    let address = unix_address(path)?;
    // SAFETY: `address` is an initialised `sockaddr_un` of the length
    // given; `fd` is an open socket this owns.
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
    Ok(fd)
}

/// The uid `SO_PEERCRED` recorded for the other end of `fd`.
///
/// # Errors
///
/// `getsockopt`'s error.
pub fn peer_uid(fd: RawFd) -> io::Result<u32> {
    peer(fd).map(|credentials| credentials.uid)
}

/// The pid, uid and gid `SO_PEERCRED` recorded for the other end of `fd`.
///
/// # Errors
///
/// `getsockopt`'s error.
pub fn peer(fd: RawFd) -> io::Result<libc::ucred> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: u32::MAX,
        gid: u32::MAX,
    };
    let mut len = socklen::<libc::ucred>();
    // SAFETY: `credentials` is valid for writes of `len` bytes, its own
    // size; `len` for a write of its own.
    let status = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut credentials).cast(),
            &raw mut len,
        )
    };
    if status != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(credentials)
}

/// One packet from `fd` into `buffer`, the part that was filled.
///
/// # Errors
///
/// `UnexpectedEof` when the other end closed, else `recv`'s error.
pub fn receive(fd: RawFd, buffer: &mut [u8]) -> io::Result<&[u8]> {
    // SAFETY: `buffer` is valid for writes of its whole length, which is
    // what is passed.
    let got = unsafe { libc::recv(fd, buffer.as_mut_ptr().cast(), buffer.len(), 0) };
    let got = usize::try_from(got).map_err(|_| io::Error::last_os_error())?;
    if got == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the other end closed the connection",
        ));
    }
    if got > MAX_RECORD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "a packet longer than a record",
        ));
    }
    buffer
        .get(..got)
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))
}

#[cfg(test)]
mod missing_tests {
    use super::{RefusalKind, manifest::Manifest, missing_writer};
    use std::path::Path;

    /// Ledger 3636 (16): no settingsd is said as the build says it.
    #[test]
    fn no_settingsd_is_said_as_the_build_says() {
        let gone = std::io::Error::from_raw_os_error(libc::ENOENT);
        let absent = Manifest::parse(
            "format=1\nprofile=max\nruntime_settings=fixed\nsettings_writer=absent\n",
        );
        let present = Manifest::parse(
            "format=1\nprofile=present\nruntime_settings=live\nsettings_writer=present\n",
        );
        let socket = Path::new("/run/ferrix/settings");
        let said = missing_writer(&gone, &absent, socket);
        assert_eq!(said.kind, RefusalKind::Unavailable);
        assert_eq!(said.text, "Fixed by this build (profile max)");
        assert_eq!(
            missing_writer(&gone, &present, socket).text,
            "the settings service is not running"
        );
        let other = std::io::Error::from_raw_os_error(libc::EACCES);
        assert!(
            missing_writer(&other, &present, socket)
                .text
                .starts_with("settingsd cannot be reached")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A listener of `uid`'s at a fresh path, answering one record with
    /// `reply` after reading one.
    fn server(name: &str, reply: ToClient) -> (PathBuf, std::thread::JoinHandle<Vec<u8>>) {
        let dir =
            std::env::temp_dir().join(format!("settings-client-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s");
        let fd = seqpacket_socket().unwrap();
        let address = unix_address(&path).unwrap();
        // SAFETY: as in `connect`.
        let bound = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                (&raw const address).cast::<libc::sockaddr>(),
                socklen::<libc::sockaddr_un>(),
            )
        };
        assert_eq!(bound, 0);
        // SAFETY: a bound socket this owns.
        assert_eq!(unsafe { libc::listen(fd.as_raw_fd(), 1) }, 0);
        let handle = std::thread::spawn(move || {
            // SAFETY: null addresses ask for none.
            let raw =
                unsafe { libc::accept(fd.as_raw_fd(), std::ptr::null_mut(), std::ptr::null_mut()) };
            assert!(raw >= 0);
            // SAFETY: just accepted.
            let conn = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(raw) };
            let mut buffer = vec![0_u8; MAX_RECORD + 1];
            let got = receive(conn.as_raw_fd(), &mut buffer)
                .map(<[u8]>::to_vec)
                .unwrap_or_default();
            let _ = send_packet(conn.as_raw_fd(), reply.encode().unwrap().bytes());
            got
        });
        (path, handle)
    }

    fn own_uid() -> u32 {
        // SAFETY: getuid cannot fail.
        unsafe { libc::getuid() }
    }

    #[test]
    fn a_dry_run_sends_the_flag_and_reads_the_answer() {
        let (path, handle) = server(
            "dry",
            ToClient::Done {
                changed: true,
                text: "would call the computer box".to_owned(),
            },
        );
        let request = Request::SetHostname {
            name: "box".to_owned(),
        };
        let outcome = Client::at(&path, own_uid()).dry_run(&request).unwrap();
        assert_eq!(outcome.text, "would call the computer box");
        let sent = handle.join().unwrap();
        match ToServer::decode(&sent).unwrap() {
            ToServer::Request {
                dry_run,
                request: r,
            } => assert!(dry_run && r == request),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_listener_that_is_not_root_is_never_told_anything() {
        let (path, handle) = server("spoof", ToClient::Info(String::new()));
        let request = Request::SetHostname {
            name: "box".to_owned(),
        };
        let refused = Client::at(&path, own_uid().wrapping_add(1))
            .begin(&request, false)
            .unwrap_err();
        assert_eq!(refused.kind, RefusalKind::Unavailable);
        assert!(refused.text.contains("not settingsd"), "{}", refused.text);
        // The server read nothing: the client hung up before sending.
        assert!(handle.join().unwrap().is_empty());
    }

    #[test]
    fn an_invalid_request_is_refused_before_connecting() {
        let request = Request::SetHostname {
            name: "my pc".to_owned(),
        };
        let refused = Client::at(Path::new("/nonexistent/s"), 0)
            .begin(&request, false)
            .unwrap_err();
        assert_eq!(refused.kind, RefusalKind::Invalid);
    }
}
