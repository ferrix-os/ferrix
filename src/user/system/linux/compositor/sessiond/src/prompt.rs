//! The trusted prompt's relay (`docs/SETTINGS-WRITER.md` §12.3): what
//! `sessiond` passes between `settingsd` and the compositor, and what it
//! refuses to.
//!
//! * Only uid 0 is served on [`SOCKET`], which is `0600 root` besides.
//! * A prompt is for the session's account alone, and one is up at a time;
//!   any other is answered `unable not-here` or `unable busy` and closed.
//! * Every record of `settingsd`'s after its `hello` names the open
//!   prompt's number, the first of them an `ask`; anything else ends it.
//! * From the compositor only an `answer`, `allow` or `deny` for the open
//!   prompt is passed on; anything else ends the prompt as `deny gone`, and
//!   nothing answer-shaped is made up here.
//! * A secret is passed on and its buffers zeroed; it is never said.
//!
//! [`Relay`] is the rules with no descriptor in sight; [`Prompts`] is the
//! socket around them.

use std::io;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt as _;
use std::time::Duration;

use compositor_seat::prompt::{FromSeat, MAX_RECORD, SOCKET, ToSeat, zero};

/// How long a new connection has to say `hello`.
const HELLO_WITHIN: Duration = Duration::from_secs(2);

/// What a step asks of `sessiond`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Out {
    /// A record to the compositor.
    ToCompositor(ToSeat),
    /// A record to `settingsd`.
    ToSettingsd(FromSeat),
    /// The prompt is over: close `settingsd`'s connection.
    Over,
}

/// The relay's state: the prompt up now, if any.
#[derive(Debug, Default)]
pub(crate) struct Relay {
    /// A connection is being served.
    open: bool,
    /// Its prompt's number, once its `ask` came.
    id: Option<u64>,
}

impl Relay {
    /// Whether a prompt's connection is being served.
    pub(crate) const fn busy(&self) -> bool {
        self.open
    }

    /// A new connection said `hello` for `uid`; the session's is `session`.
    pub(crate) fn hello(&mut self, uid: u32, session: u32, compositor: bool) -> FromSeat {
        if uid != session {
            FromSeat::Unable("not-here".to_owned())
        } else if !compositor {
            FromSeat::Unable("no-screen".to_owned())
        } else if self.open {
            FromSeat::Unable("busy".to_owned())
        } else {
            self.open = true;
            self.id = None;
            FromSeat::Ready
        }
    }

    /// A record from `settingsd`, or `None` for a packet that was none.
    pub(crate) fn settingsd_said(&mut self, record: Option<ToSeat>) -> Vec<Out> {
        let Some(record) = record else {
            return self.end("cancelled");
        };
        match (&record, self.id) {
            (ToSeat::Ask { id, .. }, None) => {
                self.id = Some(*id);
                vec![Out::ToCompositor(record)]
            }
            (ToSeat::Close { id, .. }, Some(open)) if *id == open => {
                self.open = false;
                self.id = None;
                vec![Out::ToCompositor(record), Out::Over]
            }
            (
                ToSeat::Question { id, .. } | ToSeat::Info { id, .. } | ToSeat::Confirm { id },
                Some(open),
            ) if *id == open => vec![Out::ToCompositor(record)],
            _ => self.end("cancelled"),
        }
    }

    /// `settingsd`'s connection closed.
    pub(crate) fn settingsd_gone(&mut self) -> Vec<Out> {
        if self.open {
            self.end("cancelled")
        } else {
            Vec::new()
        }
    }

    /// A record from the compositor, or `None` for a packet that was none
    /// or the channel's end.
    pub(crate) fn compositor_said(&mut self, record: Option<FromSeat>) -> Vec<Out> {
        let open = self.id;
        match record {
            Some(answer @ (FromSeat::Answer { .. } | FromSeat::Allow { .. }))
                if answer.id().is_some() && answer.id() == open =>
            {
                vec![Out::ToSettingsd(answer)]
            }
            Some(FromSeat::Deny { id, why }) if Some(id) == open => {
                vec![Out::ToSettingsd(FromSeat::Deny { id, why })]
            }
            // A denial for a prompt already gone: nothing to do.
            Some(FromSeat::Deny { .. }) if open.is_none() => Vec::new(),
            _ => match open {
                Some(id) => {
                    self.open = false;
                    self.id = None;
                    vec![
                        Out::ToSettingsd(FromSeat::Deny {
                            id,
                            why: "gone".to_owned(),
                        }),
                        Out::ToCompositor(ToSeat::Close {
                            id,
                            text: "this prompt could not go on".to_owned(),
                        }),
                        Out::Over,
                    ]
                }
                None => Vec::new(),
            },
        }
    }

    /// End the open prompt: the compositor takes it down, and `settingsd`
    /// is closed.
    fn end(&mut self, why: &str) -> Vec<Out> {
        let id = self.id.take();
        self.open = false;
        let mut out = Vec::new();
        if let Some(id) = id {
            out.push(Out::ToCompositor(ToSeat::Close {
                id,
                text: format!("this prompt was {why}"),
            }));
        }
        out.push(Out::Over);
        out
    }
}

/// A `SOCK_SEQPACKET` socket pair: the compositor's prompt channel.
///
/// # Errors
///
/// `socketpair`'s.
pub(crate) fn pair() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: `fds` is valid for writes of two descriptors.
    let made = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    };
    if made != 0 {
        return Err(io::Error::last_os_error());
    }
    let [a, b] = fds;
    // SAFETY: `socketpair` just made it, and nothing else owns it.
    let a = unsafe { OwnedFd::from_raw_fd(a) };
    // SAFETY: as above.
    let b = unsafe { OwnedFd::from_raw_fd(b) };
    Ok((a, b))
}

/// Send `packet` whole on `fd`.
pub(crate) fn send(fd: RawFd, packet: &[u8]) -> io::Result<()> {
    // SAFETY: `packet` is valid for reads of its length.
    let sent = unsafe { libc::send(fd, packet.as_ptr().cast(), packet.len(), libc::MSG_NOSIGNAL) };
    if usize::try_from(sent).ok() == Some(packet.len()) {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// One packet from `fd` into `buffer`, without waiting: `Ok(None)` for none
/// waiting, `Ok(Some(0))` for the end.
pub(crate) fn receive(fd: RawFd, buffer: &mut [u8]) -> io::Result<Option<usize>> {
    // SAFETY: `buffer` is valid for writes of its length.
    let got = unsafe {
        libc::recv(
            fd,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            libc::MSG_DONTWAIT,
        )
    };
    match usize::try_from(got) {
        Ok(got) => Ok(Some(got)),
        Err(_) => {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                Ok(None)
            } else {
                Err(error)
            }
        }
    }
}

/// The uid `SO_PEERCRED` recorded for `fd`'s other end.
fn peer_uid(fd: RawFd) -> io::Result<u32> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: u32::MAX,
        gid: u32::MAX,
    };
    let mut len = libc::socklen_t::try_from(size_of::<libc::ucred>()).unwrap_or(0);
    // SAFETY: `credentials` is valid for writes of `len` bytes, its own
    // size.
    let status = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut credentials).cast(),
            &raw mut len,
        )
    };
    if status == 0 {
        Ok(credentials.uid)
    } else {
        Err(io::Error::last_os_error())
    }
}

/// The prompt's listener and the connection served now.
#[derive(Debug)]
pub(crate) struct Prompts {
    listener: OwnedFd,
    serving: Option<OwnedFd>,
    relay: Relay,
    session: u32,
}

impl Prompts {
    /// Listen at [`SOCKET`] for `settingsd`, for the session of `session`.
    ///
    /// # Errors
    ///
    /// The socket could not be made.
    pub(crate) fn bind(session: u32) -> io::Result<Prompts> {
        let path = std::path::Path::new(SOCKET);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let _ = std::fs::remove_file(path);
        // SAFETY: plain constants.
        let fd = unsafe {
            libc::socket(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                0,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `socket` just made it.
        let listener = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: a zeroed `sockaddr_un` is a valid one to fill.
        let mut address: libc::sockaddr_un = unsafe { core::mem::zeroed() };
        address.sun_family = libc::sa_family_t::try_from(libc::AF_UNIX).unwrap_or(1);
        for (slot, byte) in address.sun_path.iter_mut().zip(SOCKET.bytes()) {
            // `c_char` is `i8` on x86-64 and `u8` on AArch64: the bits, either way.
            *slot = libc::c_char::from_ne_bytes([byte]);
        }
        // The socket is made 0600 before anyone can connect: the umask.
        // SAFETY: `umask` takes and returns a mode.
        let old = unsafe { libc::umask(0o177) };
        // SAFETY: `address` is an initialised `sockaddr_un` of the length
        // given.
        let bound = unsafe {
            libc::bind(
                listener.as_raw_fd(),
                (&raw const address).cast::<libc::sockaddr>(),
                libc::socklen_t::try_from(size_of::<libc::sockaddr_un>()).unwrap_or(0),
            )
        };
        // SAFETY: as above.
        let _ = unsafe { libc::umask(old) };
        if bound != 0 {
            return Err(io::Error::last_os_error());
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        // SAFETY: `listener` is a bound socket this owns.
        if unsafe { libc::listen(listener.as_raw_fd(), 4) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Prompts {
            listener,
            serving: None,
            relay: Relay::default(),
            session,
        })
    }

    /// The listener's descriptor and the served connection's, for `poll`.
    pub(crate) fn fds(&self) -> [Option<RawFd>; 2] {
        [
            Some(self.listener.as_raw_fd()),
            self.serving.as_ref().map(OwnedFd::as_raw_fd),
        ]
    }

    /// Take a new connection off the listener and answer its `hello`.
    pub(crate) fn accept(&mut self, compositor: bool, say: &dyn Fn(&str)) {
        // SAFETY: a null address asks for none.
        let fd = unsafe {
            libc::accept4(
                self.listener.as_raw_fd(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                libc::SOCK_CLOEXEC,
            )
        };
        if fd < 0 {
            return;
        }
        // SAFETY: `accept4` just returned it.
        let connection = unsafe { OwnedFd::from_raw_fd(fd) };
        match peer_uid(connection.as_raw_fd()) {
            Ok(0) => {}
            Ok(uid) => {
                say(&format!(
                    "the prompt socket refused uid {uid}: only root may ask"
                ));
                return;
            }
            Err(_) => return,
        }
        let mut buffer = [0_u8; MAX_RECORD + 1];
        let timeout = libc::timeval {
            tv_sec: libc::time_t::try_from(HELLO_WITHIN.as_secs()).unwrap_or(2),
            tv_usec: 0,
        };
        // SAFETY: `timeout` is a valid timeval of the length given.
        let _ = unsafe {
            libc::setsockopt(
                connection.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                (&raw const timeout).cast(),
                libc::socklen_t::try_from(size_of::<libc::timeval>()).unwrap_or(0),
            )
        };
        // SAFETY: `buffer` is valid for writes of its length.
        let got = unsafe {
            libc::recv(
                connection.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                0,
            )
        };
        let hello = usize::try_from(got)
            .ok()
            .and_then(|got| buffer.get(..got))
            .and_then(ToSeat::decode);
        let Some(ToSeat::Hello { uid }) = hello else {
            return;
        };
        let answer = self.relay.hello(uid, self.session, compositor);
        if let Some(bytes) = answer.encode() {
            let _ = send(connection.as_raw_fd(), &bytes);
        }
        if answer == FromSeat::Ready {
            self.serving = Some(connection);
        }
    }

    /// Read what the served connection sent; what the relay makes of it.
    pub(crate) fn settingsd_said(&mut self) -> Vec<Out> {
        let Some(fd) = self.serving.as_ref().map(OwnedFd::as_raw_fd) else {
            return Vec::new();
        };
        let mut buffer = [0_u8; MAX_RECORD + 1];
        let out = match receive(fd, &mut buffer) {
            Ok(None) => Vec::new(),
            Ok(Some(0)) | Err(_) => self.relay.settingsd_gone(),
            Ok(Some(got)) => {
                let record = buffer
                    .get(..got)
                    .filter(|_| got <= MAX_RECORD)
                    .and_then(ToSeat::decode);
                self.relay.settingsd_said(record)
            }
        };
        zero(&mut buffer);
        out
    }

    /// A record from the compositor: what the relay makes of it.
    pub(crate) fn compositor_said(&mut self, record: Option<FromSeat>) -> Vec<Out> {
        self.relay.compositor_said(record)
    }

    /// Carry out `out`: records to the compositor on `compositor`, to
    /// `settingsd` on the served connection, and its close.
    pub(crate) fn act(&mut self, out: Vec<Out>, compositor: Option<RawFd>) {
        for step in out {
            match step {
                Out::ToCompositor(record) => {
                    if let (Some(fd), Some(mut bytes)) = (compositor, record.encode()) {
                        let _ = send(fd, &bytes);
                        zero(&mut bytes);
                    }
                }
                Out::ToSettingsd(record) => {
                    if let (Some(fd), Some(mut bytes)) = (
                        self.serving.as_ref().map(OwnedFd::as_raw_fd),
                        record.encode(),
                    ) {
                        let _ = send(fd, &bytes);
                        zero(&mut bytes);
                    }
                }
                Out::Over => self.serving = None,
            }
        }
    }

    /// Whether a prompt is being served.
    pub(crate) const fn busy(&self) -> bool {
        self.relay.busy()
    }
}

impl Drop for Prompts {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(SOCKET);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use compositor_seat::prompt::Secret;

    fn ask(id: u64) -> ToSeat {
        ToSeat::Ask {
            id,
            area: "hostname".to_owned(),
            program: "/bin/settings".to_owned(),
            parent: "/bin/hyprix".to_owned(),
            account: "ferrix".to_owned(),
            what: "would call the computer box".to_owned(),
        }
    }

    fn opened() -> Relay {
        let mut relay = Relay::default();
        assert_eq!(relay.hello(1000, 1000, true), FromSeat::Ready);
        assert_eq!(
            relay.settingsd_said(Some(ask(1))),
            [Out::ToCompositor(ask(1))]
        );
        relay
    }

    #[test]
    fn only_the_session_s_account_and_one_at_a_time() {
        let mut relay = Relay::default();
        assert_eq!(
            relay.hello(1001, 1000, true),
            FromSeat::Unable("not-here".to_owned())
        );
        assert_eq!(
            relay.hello(1000, 1000, false),
            FromSeat::Unable("no-screen".to_owned())
        );
        assert_eq!(relay.hello(1000, 1000, true), FromSeat::Ready);
        assert_eq!(
            relay.hello(1000, 1000, true),
            FromSeat::Unable("busy".to_owned())
        );
    }

    #[test]
    fn an_answer_for_the_open_prompt_is_passed_on_and_nothing_else() {
        let mut relay = opened();
        let answer = FromSeat::Answer {
            id: 1,
            secret: Secret::new(b"pw").unwrap(),
        };
        assert_eq!(
            relay.compositor_said(Some(answer)),
            [Out::ToSettingsd(FromSeat::Answer {
                id: 1,
                secret: Secret::new(b"pw").unwrap()
            })]
        );
        // An answer for another prompt, or a record that is no answer, ends
        // this one denied: nothing answer-shaped is made up.
        for bad in [
            Some(FromSeat::Allow { id: 2 }),
            Some(FromSeat::Ready),
            Some(FromSeat::Unable("busy".to_owned())),
            None,
        ] {
            let mut relay = opened();
            let out = relay.compositor_said(bad);
            assert_eq!(
                out.first(),
                Some(&Out::ToSettingsd(FromSeat::Deny {
                    id: 1,
                    why: "gone".to_owned()
                }))
            );
            assert_eq!(out.last(), Some(&Out::Over));
            assert!(!relay.busy());
        }
        // From a compositor with no prompt up, nothing goes anywhere.
        assert!(
            Relay::default()
                .compositor_said(Some(FromSeat::Allow { id: 1 }))
                .is_empty()
        );
    }

    #[test]
    fn settingsd_speaks_only_of_its_own_prompt() {
        let mut relay = opened();
        let confirm = ToSeat::Confirm { id: 1 };
        assert_eq!(
            relay.settingsd_said(Some(ToSeat::Confirm { id: 1 })),
            [Out::ToCompositor(confirm)]
        );
        for bad in [
            Some(ToSeat::Confirm { id: 2 }),
            Some(ask(1)),
            Some(ToSeat::Hello { uid: 0 }),
            None,
        ] {
            let mut relay = opened();
            let out = relay.settingsd_said(bad);
            assert_eq!(
                out,
                [
                    Out::ToCompositor(ToSeat::Close {
                        id: 1,
                        text: "this prompt was cancelled".to_owned()
                    }),
                    Out::Over
                ]
            );
            assert!(!relay.busy());
        }
        // A first record that is not an ask.
        let mut relay = Relay::default();
        let _ = relay.hello(1000, 1000, true);
        assert_eq!(
            relay.settingsd_said(Some(ToSeat::Confirm { id: 1 })),
            [Out::Over]
        );
        let mut relay = opened();
        let close = ToSeat::Close {
            id: 1,
            text: "done".to_owned(),
        };
        assert_eq!(
            relay.settingsd_said(Some(ToSeat::Close {
                id: 1,
                text: "done".to_owned()
            })),
            [Out::ToCompositor(close), Out::Over]
        );
        assert!(!relay.busy());
        assert!(relay.settingsd_gone().is_empty());
    }
}
