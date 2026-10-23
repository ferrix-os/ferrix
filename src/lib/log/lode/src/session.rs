//! Sessions: what the seat's owner tells lode, and the session id records
//! carry (`docs/LOGGING.md` §19.1, step 2a).
//!
//! The program that owns a seat -- `sessiond` for seat0's graphical
//! session -- tells lode, on the socket only root can reach
//! (`/run/lode/seat`), when a session begins and ends, one line each:
//!
//! ```text
//! SESSION 3 seat=seat0 uid=1000 pid=812 scope=session-3.scope unit=hyprix.service token=<32 hex>
//! END 3
//! ```
//!
//! The id is the session's number, the one its scope `session-<n>.scope`
//! has, which `login` and `sessiond` draw from one count a boot. lode stamps
//! `SESSION_ID` on the records of the session's unit while it lasts, so
//! `lode -s 3` is that session's lines. The token is the session's secret:
//! the compositor is handed it beside the card, and it is how a screenshot
//! provider proves it holds the seat (step 2b). lode never shows it.

use alloc::string::String;
use alloc::vec::Vec;

use crate::record::Record;

/// What a seat's owner said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Said {
    /// A session began.
    Began(Session),
    /// Session `id` ended.
    Ended(u32),
}

/// One session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// Its number in this boot.
    pub id: u32,
    /// Its seat.
    pub seat: String,
    /// Its account.
    pub uid: u32,
    /// Its compositor's (or shell's) pid.
    pub pid: u32,
    /// Its scope.
    pub scope: String,
    /// The unit whose output its programs' lines come through.
    pub unit: String,
    /// Its secret, 16 bytes.
    pub token: [u8; 16],
}

/// Read one line a seat's owner wrote.
#[must_use]
pub fn parse(line: &str) -> Option<Said> {
    let mut words = line.split_whitespace();
    match words.next()? {
        "END" => Some(Said::Ended(words.next()?.parse().ok()?)),
        "SESSION" => {
            let id: u32 = words.next()?.parse().ok()?;
            let mut session = Session {
                id,
                seat: String::new(),
                uid: u32::MAX,
                pid: 0,
                scope: String::new(),
                unit: String::new(),
                token: [0; 16],
            };
            for word in words {
                let (key, value) = word.split_once('=')?;
                match key {
                    "seat" => session.seat = String::from(value),
                    "uid" => session.uid = value.parse().ok()?,
                    "pid" => session.pid = value.parse().ok()?,
                    "scope" => session.scope = String::from(value),
                    "unit" => session.unit = String::from(value),
                    "token" => session.token = crate::segment::parse_id(value)?,
                    _ => {}
                }
            }
            let named = |text: &str| {
                !text.is_empty()
                    && text.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'@')
                    })
            };
            (named(&session.seat)
                && named(&session.scope)
                && named(&session.unit)
                && session.uid != u32::MAX)
                .then_some(Said::Began(session))
        }
        _ => None,
    }
}

/// The line a seat's owner writes for `session`.
#[must_use]
pub fn line(session: &Session) -> String {
    let mut token = String::new();
    crate::segment::push_hex(&mut token, &session.token);
    alloc::format!(
        "SESSION {} seat={} uid={} pid={} scope={} unit={} token={token}\n",
        session.id,
        session.seat,
        session.uid,
        session.pid,
        session.scope,
        session.unit
    )
}

/// The sessions open now.
#[derive(Debug, Default)]
pub struct Sessions {
    /// Each, by id.
    open: Vec<Session>,
}

impl Sessions {
    /// Take what a seat's owner said: the record that says it, to store.
    pub fn take(&mut self, said: Said, record: &mut Record) {
        match said {
            Said::Began(session) => {
                self.open
                    .retain(|open| open.id != session.id && open.seat != session.seat);
                record.push(
                    "MESSAGE",
                    alloc::format!(
                        "session {} began on {} for uid {} ({}, its programs' lines through {})",
                        session.id,
                        session.seat,
                        session.uid,
                        session.scope,
                        session.unit
                    )
                    .as_bytes(),
                );
                record.push("SESSION_ID", alloc::format!("{}", session.id).as_bytes());
                record.push("SESSION_SEAT", session.seat.as_bytes());
                record.push("SESSION_UID", alloc::format!("{}", session.uid).as_bytes());
                record.push("SESSION_SCOPE", session.scope.as_bytes());
                record.push("SESSION_UNIT", session.unit.as_bytes());
                self.open.push(session);
            }
            Said::Ended(id) => {
                record.push("MESSAGE", alloc::format!("session {id} ended").as_bytes());
                record.push("SESSION_ID", alloc::format!("{id}").as_bytes());
                self.open.retain(|open| open.id != id);
            }
        }
    }

    /// Stamp `record` with the session whose unit or pid it came from.
    pub fn stamp(&self, record: &mut Record) {
        if record.get("SESSION_ID").is_some() {
            return;
        }
        let unit = record.get("_SYSTEMD_UNIT").or_else(|| record.get("UNIT"));
        let pid = record
            .get_str("_PID")
            .or_else(|| record.get_str("SYSLOG_PID"))
            .and_then(|pid| pid.parse::<u32>().ok());
        let found = self.open.iter().find(|session| {
            unit == Some(session.unit.as_bytes()) || pid.is_some_and(|pid| pid == session.pid)
        });
        if let Some(session) = found {
            record.push("SESSION_ID", alloc::format!("{}", session.id).as_bytes());
        }
    }

    /// Every session open now.
    #[must_use]
    pub fn open(&self) -> &[Session] {
        &self.open
    }

    /// The session holding `seat`, if any.
    #[must_use]
    pub fn on_seat(&self, seat: &str) -> Option<&Session> {
        self.open.iter().find(|session| session.seat == seat)
    }
}
