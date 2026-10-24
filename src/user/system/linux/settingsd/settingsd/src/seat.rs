//! Asking on the seat (`docs/SETTINGS-WRITER.md` §12.3, §12.5 S4): which
//! accounts the image gives the seat, and the trusted prompt `sessiond`
//! relays to the compositor for them.
//!
//! * **Which accounts.** The image's word, not the socket's (ledger 3636
//!   (1)): the `hyprix.service` unit `sessiond` is started from, in
//!   `/etc/ferrix/units`, else `/lib/ferrix/units`, whose `ExecStart=` runs
//!   `sessiond --user NAME`. No such unit: no seat. A unit there that
//!   cannot be read, or that runs `sessiond` naming no account: nothing can
//!   be said, and every non-root request is refused.
//! * **The prompt.** A `SOCK_SEQPACKET` connection to
//!   `compositor_seat::prompt::SOCKET`, whose listener must be root, then
//!   `hello UID` and `ready`, then the prompt's records; [`OnSeat`] is the
//!   person `authd`'s conversation asks.

use std::io;
#[cfg(test)]
use std::os::fd::FromRawFd as _;
use std::os::fd::{AsRawFd as _, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use compositor_seat::prompt::{FromSeat, MAX_RECORD, ToSeat, zero};
use ferrix_auth_client::{Person, seqpacket_socket, socklen, unix_address};
use ferrix_auth_proto::Secret;

use crate::authority::ANSWER_WITHIN;

/// How long `sessiond` has to say `ready`.
const READY_WITHIN: Duration = Duration::from_secs(5);

/// Where init looks for units, below the root, in its order of precedence
/// (`ferrix_svc::source`'s layers: runtime, administrator, image).
const UNITS: [&str; 3] = ["run/ferrix/units", "etc/ferrix/units", "lib/ferrix/units"];

/// The unit that starts the seat's session.
const SESSION_UNIT: &str = "hyprix.service";

/// The accounts the image gives the seat.
///
/// Read as init would find the unit, and refused where it cannot be read
/// the same simple way (ledger 3636 (1), A5): a drop-in directory
/// (`hyprix.service.d`) in any layer, a unit that is a link (a mask or an
/// alias), or an `ExecStart=` that is neither an absolute `.../sessiond
/// --user NAME` nor an absolute `.../hyprix` (a desktop that is root's).
///
/// # Errors
///
/// Why who has the seat cannot be told; every non-root request is then
/// refused.
pub(crate) fn seat_accounts(root: &Path) -> Result<Vec<String>, String> {
    for dir in UNITS {
        let drop_ins = root.join(dir).join(format!("{SESSION_UNIT}.d"));
        if std::fs::symlink_metadata(&drop_ins).is_ok() {
            return Err(format!(
                "{} changes the session's unit, and settingsd does not read drop-ins",
                drop_ins.display()
            ));
        }
    }
    for dir in UNITS {
        let path = root.join(dir).join(SESSION_UNIT);
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("{} cannot be read: {error}", path.display())),
        };
        if !meta.is_file() {
            return Err(format!(
                "{} is a link or not a file (a mask or an alias)",
                path.display()
            ));
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("{} cannot be read: {error}", path.display()))?;
        return accounts_in(&text).ok_or_else(|| {
            format!(
                "{} starts the session in a way settingsd does not read",
                path.display()
            )
        });
    }
    Ok(Vec::new())
}

/// The accounts a unit's `ExecStart=` lines give `sessiond`: none for a
/// root desktop (an absolute `.../hyprix`), `None` for any other shape --
/// `sessiond` named without a directory or through a wrapper, or with no
/// `--user` before `--`.
fn accounts_in(unit: &str) -> Option<Vec<String>> {
    let mut accounts = Vec::new();
    for line in unit.lines() {
        let Some(command) = line.trim().strip_prefix("ExecStart=") else {
            continue;
        };
        let words: Vec<&str> = command.split_whitespace().collect();
        let program = words.first()?.trim_start_matches(['-', '@', '+', '!']);
        if !program.starts_with('/') {
            return None;
        }
        if program.ends_with("/hyprix") {
            continue;
        }
        if !program.ends_with("/sessiond") {
            return None;
        }
        let user = words
            .windows(2)
            .take_while(|pair| pair.first() != Some(&"--"))
            .find(|pair| pair.first() == Some(&"--user"))
            .and_then(|pair| pair.get(1))?;
        accounts.push((*user).to_owned());
    }
    Some(accounts)
}

/// Why no prompt could be put up.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Unable {
    /// No desktop is running for the account.
    NoDesktop,
    /// The seat is busy, the listener is not root, or it said nothing.
    Unavailable(String),
}

/// A connection to the seat's prompt.
#[derive(Debug)]
pub(crate) struct Channel {
    fd: OwnedFd,
}

impl Channel {
    /// Connect to the prompt at `path` for `uid`, whose listener must be
    /// `listener_uid` (root on the machine).
    ///
    /// # Errors
    ///
    /// Why no prompt can be put up.
    pub(crate) fn open(path: &Path, listener_uid: u32, uid: u32) -> Result<Channel, Unable> {
        let fd = seqpacket_socket().map_err(|error| Unable::Unavailable(error.to_string()))?;
        let address = unix_address(path).map_err(|error| Unable::Unavailable(error.to_string()))?;
        // SAFETY: `address` is an initialised `sockaddr_un` of the length
        // given.
        let connected = unsafe {
            libc::connect(
                fd.as_raw_fd(),
                (&raw const address).cast::<libc::sockaddr>(),
                socklen::<libc::sockaddr_un>(),
            )
        };
        if connected != 0 {
            let error = io::Error::last_os_error();
            return Err(match error.raw_os_error() {
                Some(libc::ENOENT | libc::ECONNREFUSED) => Unable::NoDesktop,
                _ => Unable::Unavailable(format!("the seat's prompt: {error}")),
            });
        }
        let channel = Channel { fd };
        match channel.peer_uid() {
            Some(found) if found == listener_uid => {}
            Some(found) => {
                return Err(Unable::Unavailable(format!(
                    "the seat's prompt is held by uid {found}, not sessiond"
                )));
            }
            None => {
                return Err(Unable::Unavailable(
                    "who holds the seat's prompt cannot be told".to_owned(),
                ));
            }
        }
        channel
            .send(&ToSeat::Hello { uid })
            .map_err(|error| Unable::Unavailable(format!("the seat's prompt: {error}")))?;
        match channel.receive_within(Instant::now() + READY_WITHIN) {
            Some(FromSeat::Ready) => Ok(channel),
            Some(FromSeat::Unable(why)) if why == "not-here" => Err(Unable::NoDesktop),
            Some(FromSeat::Unable(why)) => Err(Unable::Unavailable(format!(
                "the seat cannot ask now ({why}); try again"
            ))),
            _ => Err(Unable::Unavailable(
                "the seat's prompt did not answer".to_owned(),
            )),
        }
    }

    fn peer_uid(&self) -> Option<u32> {
        let mut credentials = libc::ucred {
            pid: 0,
            uid: u32::MAX,
            gid: u32::MAX,
        };
        let mut len = socklen::<libc::ucred>();
        // SAFETY: `credentials` is valid for writes of `len` bytes.
        let status = unsafe {
            libc::getsockopt(
                self.fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&raw mut credentials).cast(),
                &raw mut len,
            )
        };
        (status == 0).then_some(credentials.uid)
    }

    /// Send `record`.
    pub(crate) fn send(&self, record: &ToSeat) -> io::Result<()> {
        let mut bytes = record
            .encode()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "a record too long"))?;
        let sent = ferrix_auth_client::send_packet(self.fd.as_raw_fd(), &bytes);
        zero(&mut bytes);
        sent
    }

    /// The next record before `deadline`, or `None`.
    pub(crate) fn receive_within(&self, deadline: Instant) -> Option<FromSeat> {
        let mut buffer = vec![0_u8; MAX_RECORD + 1];
        let record = crate::wire::receive_within(self.fd.as_raw_fd(), &mut buffer, deadline)
            .and_then(FromSeat::decode);
        zero(&mut buffer);
        record
    }
}

/// What the prompt said of the change at its end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Said {
    /// Nothing yet: no prompt was put up.
    Nothing,
    /// A password typed and Allow.
    Answered,
    /// Allow, with no field.
    Allowed,
    /// Cancel, or the prompt could not stay.
    Denied,
}

/// The person at the seat, as `authd` asks them: each question is a field
/// in the prompt the compositor draws; each `INFO` is a line under it.
#[derive(Debug)]
pub(crate) struct OnSeat {
    channel: Channel,
    id: u64,
    /// The prompt's heading and body: put up on the first question, or by
    /// [`OnSeat::confirm`].
    ask: Option<ToSeat>,
    /// What the person did last.
    pub(crate) said: Said,
    /// Why it was denied, when it was.
    pub(crate) why: String,
}

impl OnSeat {
    /// The prompt `id` on `channel`, showing `ask` once it is put up.
    pub(crate) const fn new(channel: Channel, id: u64, ask: ToSeat) -> OnSeat {
        OnSeat {
            channel,
            id,
            ask: Some(ask),
            said: Said::Nothing,
            why: String::new(),
        }
    }

    /// Put the prompt up, if it is not yet.
    fn put_up(&mut self) -> bool {
        match self.ask.take() {
            Some(ask) => self.channel.send(&ask).is_ok(),
            None => true,
        }
    }

    /// Wait for the person's answer.
    fn answer(&mut self) -> Option<FromSeat> {
        let answer = self.channel.receive_within(Instant::now() + ANSWER_WITHIN);
        match &answer {
            Some(FromSeat::Answer { id, .. }) if *id == self.id => self.said = Said::Answered,
            Some(FromSeat::Allow { id }) if *id == self.id => self.said = Said::Allowed,
            Some(FromSeat::Deny { why, .. }) => {
                self.said = Said::Denied;
                why.clone_into(&mut self.why);
            }
            _ => {
                self.said = Said::Denied;
                "timeout".clone_into(&mut self.why);
            }
        }
        answer
    }

    /// Allow and Cancel, with no field: `authd` accepted with no question.
    /// Whether the person allowed it.
    pub(crate) fn confirm(&mut self) -> bool {
        if !self.put_up() || self.channel.send(&ToSeat::Confirm { id: self.id }).is_err() {
            self.said = Said::Denied;
            "gone".clone_into(&mut self.why);
            return false;
        }
        matches!(self.answer(), Some(FromSeat::Allow { id }) if id == self.id)
    }

    /// The prompt's end, with what came of it.
    pub(crate) fn close(&self, text: &str) {
        if self.ask.is_none() {
            let _ = self.channel.send(&ToSeat::Close {
                id: self.id,
                text: text.to_owned(),
            });
        }
    }
}

impl Person for OnSeat {
    fn ask(&mut self, visible: bool, text: &str) -> Option<Secret> {
        let question = ToSeat::Question {
            id: self.id,
            visible,
            text: text.to_owned(),
        };
        if !self.put_up() || self.channel.send(&question).is_err() {
            self.said = Said::Denied;
            "gone".clone_into(&mut self.why);
            return None;
        }
        match self.answer() {
            Some(FromSeat::Answer { id, secret }) if id == self.id => {
                Secret::from_bytes(secret.expose())
            }
            _ => None,
        }
    }

    fn tell(&mut self, text: &str, _error: bool) {
        if self.put_up() {
            let _ = self.channel.send(&ToSeat::Info {
                id: self.id,
                text: text.to_owned(),
            });
        }
    }
}

/// The prompt's socket on the machine.
pub(crate) fn socket() -> PathBuf {
    PathBuf::from(compositor_seat::prompt::SOCKET)
}

/// Make a socket listening at `path`: for tests, the stand-in `sessiond`.
#[cfg(test)]
pub(crate) fn listen(path: &Path) -> OwnedFd {
    let fd = seqpacket_socket().unwrap();
    let address = unix_address(path).unwrap();
    // SAFETY: as in `Channel::open`.
    let bound = unsafe {
        libc::bind(
            fd.as_raw_fd(),
            (&raw const address).cast::<libc::sockaddr>(),
            socklen::<libc::sockaddr_un>(),
        )
    };
    assert_eq!(bound, 0);
    // SAFETY: a bound socket.
    assert_eq!(unsafe { libc::listen(fd.as_raw_fd(), 4) }, 0);
    fd
}

/// Accept one connection on `listener`: for tests.
#[cfg(test)]
pub(crate) fn accept(listener: &OwnedFd) -> OwnedFd {
    // SAFETY: a null address asks for none.
    let fd = unsafe {
        libc::accept(
            listener.as_raw_fd(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert!(fd >= 0);
    // SAFETY: `accept` just returned it.
    unsafe { OwnedFd::from_raw_fd(fd) }
}

#[cfg(test)]
mod tests {
    use super::{accounts_in, seat_accounts};

    /// A drop-in, a link and an unknown shape refuse; the plain unit is read
    /// from the first layer that has it.
    #[test]
    fn the_session_unit_is_read_as_init_would_or_refused() {
        let root =
            std::env::temp_dir().join(format!("settingsd-unit-layers-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let units = |dir: &str| root.join(dir);
        for dir in ["run/ferrix/units", "etc/ferrix/units", "lib/ferrix/units"] {
            std::fs::create_dir_all(units(dir)).unwrap();
        }
        assert_eq!(seat_accounts(&root), Ok(vec![]));
        std::fs::write(
            units("lib/ferrix/units/hyprix.service"),
            "ExecStart=/bin/sessiond --user ferrix -- /bin/hyprix\n",
        )
        .unwrap();
        assert_eq!(seat_accounts(&root), Ok(vec!["ferrix".to_owned()]));
        // The administrator's copy wins over the image's.
        std::fs::write(
            units("etc/ferrix/units/hyprix.service"),
            "ExecStart=/bin/sessiond --user guest -- /bin/hyprix\n",
        )
        .unwrap();
        assert_eq!(seat_accounts(&root), Ok(vec!["guest".to_owned()]));
        // A drop-in that would change --user: refused.
        std::fs::create_dir_all(units("etc/ferrix/units/hyprix.service.d")).unwrap();
        assert!(seat_accounts(&root).is_err());
        std::fs::remove_dir(units("etc/ferrix/units/hyprix.service.d")).unwrap();
        // A mask in /run: refused.
        std::os::unix::fs::symlink("/dev/null", units("run/ferrix/units/hyprix.service")).unwrap();
        assert!(seat_accounts(&root).is_err());
        std::fs::remove_file(units("run/ferrix/units/hyprix.service")).unwrap();
        // A wrapped sessiond: refused.
        std::fs::write(
            units("etc/ferrix/units/hyprix.service"),
            "ExecStart=/bin/wrap /bin/sessiond --user ferrix -- /bin/hyprix\n",
        )
        .unwrap();
        assert!(seat_accounts(&root).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_seat_s_accounts_are_sessiond_s_user() {
        assert_eq!(
            accounts_in(
                "[Service]\nExecStart=/bin/sessiond --user ferrix --headless 1920x1200 -- /bin/hyprix\n"
            ),
            Some(vec!["ferrix".to_owned()])
        );
        // A root desktop runs hyprix itself: no seat account.
        assert_eq!(
            accounts_in("[Service]\nExecStart=/bin/hyprix\n"),
            Some(vec![])
        );
        // sessiond with no account named, or --user only after `--`.
        assert_eq!(
            accounts_in("ExecStart=/bin/sessiond -- /bin/hyprix\n"),
            None
        );
        assert_eq!(
            accounts_in("ExecStart=/bin/sessiond -- /bin/hyprix --user root\n"),
            None
        );
    }
}
