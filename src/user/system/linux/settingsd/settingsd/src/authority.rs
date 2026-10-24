//! Who may make a change is `authd`'s to say (`docs/SETTINGS-WRITER.md`
//! §5): `settingsd` runs one conversation for the request's area, naming
//! the account of the uid that asked, relays every question to that peer
//! and every answer back, and acts on ACCEPTED for exactly that peer.
//!
//! The duties of a root client that sees a password, as `login` and `su`
//! have them (the certification consultant's ledger line 3482, condition 2):
//! the listener at `authd`'s socket is checked before a byte is sent (U1,
//! [`ferrix_auth_client::talks_to_authd`]); the verdict must name the peer's
//! uid and account (U3, [`decide`]); the secret is held in a
//! [`ferrix_auth_proto::Secret`], which zeroes itself, and the packet it
//! came in is zeroed after; no secret reaches a log, an audit line or a
//! refusal's text.

use std::os::fd::RawFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ferrix_auth_client::{Connection, Person, Verdict, converse, talks_to_authd};
use ferrix_auth_proto::Secret;
use ferrix_settings_proto::{MAX_RECORD, Refusal, RefusalKind, ToClient, ToServer};

use crate::wire;

/// How long a person has to answer a question.
pub(crate) const ANSWER_WITHIN: Duration = Duration::from_secs(120);

/// Whoever decides: `authd`, or a test's script.
pub(crate) trait Authority: Send + Sync {
    /// Run one conversation for `service` about `account`, with `person`
    /// answering.
    ///
    /// # Errors
    ///
    /// `authd` could not be reached, or is not who listens.
    fn converse(
        &self,
        service: &str,
        account: &str,
        person: &mut dyn Person,
    ) -> Result<Verdict, String>;
}

/// The machine's `authd`.
#[derive(Debug)]
pub(crate) struct Authd {
    socket: PathBuf,
}

impl Authd {
    /// The `authd` listening at `socket`.
    pub(crate) fn at(socket: PathBuf) -> Authd {
        Authd { socket }
    }
}

impl Authority for Authd {
    fn converse(
        &self,
        service: &str,
        account: &str,
        person: &mut dyn Person,
    ) -> Result<Verdict, String> {
        let connection = Connection::open_at(&self.socket)
            .map_err(|error| format!("authd cannot be reached: {error}"))?;
        talks_to_authd(&connection)?;
        converse(&connection, service, account, person)
            .map_err(|error| format!("the conversation with authd broke off: {error}"))
    }
}

/// The person at the other end of the client's connection, as `authd`
/// sees them: each PROMPT goes to the client, each RESPOND comes back.
#[derive(Debug)]
pub(crate) struct Relay {
    client: RawFd,
    /// The client gave no answer, cancelled or went away.
    pub(crate) cancelled: bool,
    /// A question was put to the person.
    pub(crate) prompted: bool,
}

impl Relay {
    /// The person at the other end of `client`.
    pub(crate) fn new(client: RawFd) -> Relay {
        Relay {
            client,
            cancelled: false,
            prompted: false,
        }
    }
}

impl Person for Relay {
    fn ask(&mut self, visible: bool, text: &str) -> Option<Secret> {
        self.prompted = true;
        let prompt = ToClient::Prompt {
            visible,
            text: text.to_owned(),
        };
        if wire::send(self.client, &prompt).is_err() {
            self.cancelled = true;
            return None;
        }
        let mut buffer = vec![0_u8; MAX_RECORD + 1];
        let answer =
            match wire::receive_within(self.client, &mut buffer, Instant::now() + ANSWER_WITHIN) {
                Some(packet) => match ToServer::decode(packet) {
                    Ok(ToServer::Respond(secret)) => Some(secret),
                    _ => None,
                },
                None => None,
            };
        buffer.fill(0);
        let _ = std::hint::black_box(&buffer);
        self.cancelled |= answer.is_none();
        answer
    }

    fn tell(&mut self, text: &str, _error: bool) {
        let _ = wire::send(self.client, &ToClient::Info(text.to_owned()));
    }
}

/// U3: whether `verdict` lets the peer `uid`, whose account is `account`,
/// make the change, and if not, the refusal to answer with.
///
/// # Errors
///
/// The refusal: anything but ACCEPTED for exactly `uid` and `account`.
pub(crate) fn decide(
    verdict: &Verdict,
    uid: u32,
    account: &str,
    cancelled: bool,
) -> Result<(), Refusal> {
    match verdict {
        Verdict::Accepted {
            uid: accepted,
            account: name,
        } if *accepted == uid && name == account => Ok(()),
        Verdict::Accepted { .. } => Err(Refusal::new(
            RefusalKind::NotPermitted,
            "authd accepted someone other than the person who asked",
        )),
        Verdict::Failed {
            retry_after_ms,
            text,
        } => Err(Refusal {
            kind: RefusalKind::AuthenticationFailed,
            retry_after_ms: *retry_after_ms,
            text: text.clone(),
        }),
        Verdict::Unavailable(_) if cancelled => Err(Refusal::new(
            RefusalKind::Cancelled,
            "no answer was given; nothing was changed",
        )),
        Verdict::Unavailable(text) => Err(Refusal::new(RefusalKind::NotPermitted, text.clone())),
    }
}
