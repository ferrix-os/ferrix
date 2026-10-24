//! `service-enable-persistent` (B1, B6): init's `enable` or `disable` for a
//! unit on [`ferrix_settings_proto::SWITCHABLE_UNITS`], asked over init's
//! control socket as `svc` asks it. init makes the links; this asks.

use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use ferrix_settings_proto::{Refusal, RefusalKind};
use ferrix_svc_proto::control::{Answer, Call, Framer};

use super::{Action, Plan, System};

/// init, as far as a unit's links go.
pub(crate) trait Init: Send + Sync {
    /// Enable `unit`, or disable it: init's notes on success, its refusal
    /// otherwise.
    ///
    /// # Errors
    ///
    /// What init said, or why it could not be asked.
    fn switch(&self, unit: &str, enabled: bool) -> Result<Vec<String>, String>;
}

/// init's control socket.
#[derive(Debug)]
pub(crate) struct Control {
    socket: PathBuf,
}

impl Control {
    /// init listening at `socket`.
    pub(crate) fn at(socket: PathBuf) -> Control {
        Control { socket }
    }
}

impl Init for Control {
    fn switch(&self, unit: &str, enabled: bool) -> Result<Vec<String>, String> {
        let call = if enabled {
            Call::Enable(unit.to_owned())
        } else {
            Call::Disable(unit.to_owned())
        };
        let mut stream = UnixStream::connect(&self.socket)
            .map_err(|error| format!("init cannot be reached: {error}"))?;
        stream
            .write_all(&call.encode())
            .map_err(|error| format!("writing to init: {error}"))?;
        let mut framer = Framer::new();
        let mut notes = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            while let Some(record) = framer
                .next_record()
                .map_err(|error| format!("init's answer: {error:?}"))?
            {
                match Answer::decode(&record)
                    .map_err(|error| format!("init's answer: {error:?}"))?
                {
                    Answer::Note(note) => notes.push(note),
                    Answer::Done(result) if result == "done" => return Ok(notes),
                    Answer::Done(result) => return Err(result),
                    Answer::Refused(why) => return Err(why),
                    Answer::Units(_) | Answer::Lines(_) => {}
                }
            }
            let count = stream
                .read(&mut buffer)
                .map_err(|error| format!("reading init's answer: {error}"))?;
            if count == 0 {
                return Err("init closed the connection without answering".to_owned());
            }
            framer.push(buffer.get(..count).unwrap_or_default());
        }
    }
}

/// What switching `unit` would do. Whether it is on now is not asked: init
/// makes no link that is there already and removes none that is not.
pub(super) fn plan(unit: &str, enabled: bool) -> Plan {
    let state = if enabled { "on" } else { "off" };
    Plan {
        action: Action::Service {
            unit: unit.to_owned(),
            enabled,
        },
        changed: true,
        would: format!("would turn {unit} {state} for every boot"),
        before: String::new(),
        after: state.to_owned(),
    }
}

/// Ask init to switch `unit`.
///
/// # Errors
///
/// [`RefusalKind::Unavailable`] when init cannot be reached,
/// [`RefusalKind::Failed`] when it refused.
pub(super) fn apply(unit: &str, enabled: bool, system: &System) -> Result<String, Refusal> {
    let state = if enabled { "on" } else { "off" };
    match system.init.switch(unit, enabled) {
        Ok(_) => Ok(format!("{unit} is {state} for every boot")),
        Err(why) if why.starts_with("init cannot be reached") => {
            Err(Refusal::new(RefusalKind::Unavailable, why))
        }
        Err(why) => Err(Refusal::new(
            RefusalKind::Failed,
            format!("init did not turn {unit} {state}: {why}"),
        )),
    }
}
