//! What an alert does (`ferrix_lode_core::alert`, `docs/LOGGING.md`
//! §7.4), without lode doing anything to the system itself (§11.2):
//!
//! * `notify`: a line on `/run/lode/alerts` to every reader -- the
//!   desktop's notifyd shows it as a banner, a box run follows it with
//!   `lode alerts`. Made by lode as root, mode 0666: anyone may listen,
//!   and what is said is the alert's own text, which root wrote.
//! * `start`: `svc start` on init's control socket, as lode's own user;
//!   init starts the unit only if its file says `StartableBy=lode`. init's
//!   answer is read when it comes and said in a record.
//!
//! Every write is non-blocking, so a reader that does not read, or an init
//! that is slow to answer, costs a connection and never lode's loop.

use alloc::string::String;
use alloc::vec::Vec;

use ferrix_lode_core::alert::{Action, Fired, MESSAGE_ID_ALERT};
use ferrix_lode_core::ingest::{self, Now};
use ferrix_lode_core::record::Record;
use ferrix_svc_proto::control::{Answer, Call, Framer};

use crate::sys;

/// Where alerts are said.
pub(crate) const SOCKET: &[u8] = b"/run/lode/alerts";

/// init's control socket.
const CONTROL: &[u8] = b"/run/ferrix/control";

/// How long init may take to answer a start, in microseconds.
const ANSWER_US: u64 = 30_000_000;

/// A start asked of init, waiting for its answer.
struct Start {
    /// The connection.
    fd: usize,
    /// The unit.
    unit: String,
    /// The alert that asked.
    alert: String,
    /// When it was asked.
    since: u64,
    /// What init has said so far.
    framer: Framer,
}

/// The alerts' socket, its readers, and the starts waiting for init.
pub(crate) struct Alerting {
    /// The listening socket, if it could be made.
    listener: Option<usize>,
    /// Who listens.
    readers: Vec<usize>,
    /// Starts asked of init.
    starts: Vec<Start>,
}

impl Alerting {
    /// Make the socket (as root).
    pub(crate) fn open() -> Alerting {
        let listener = sys::listen_at(SOCKET).ok();
        if listener.is_some() {
            let _ = sys::chmod(SOCKET, 0o666);
        }
        Alerting {
            listener,
            readers: Vec::new(),
            starts: Vec::new(),
        }
    }

    /// Every descriptor to wait on.
    pub(crate) fn fds(&self) -> Vec<usize> {
        self.listener
            .iter()
            .copied()
            .chain(self.readers.iter().copied())
            .chain(self.starts.iter().map(|start| start.fd))
            .collect()
    }

    /// Accept readers, notice the ones gone, and read init's answers;
    /// what init said goes into `out` as records.
    pub(crate) fn serve(&mut self, now: Now, out: &mut Vec<Record>) {
        if let Some(listener) = self.listener {
            while let Some(fd) = sys::accept(listener) {
                self.readers.push(fd);
            }
        }
        let mut buf = [0_u8; 512];
        self.readers.retain(|&fd| match sys::read(fd, &mut buf) {
            // A reader says nothing; one that closed is gone.
            Ok(0) => {
                sys::close(fd);
                false
            }
            Ok(_) => true,
            Err(error) => error.0 == 11 || error.0 == sys::EINTR,
        });
        let mut waiting = Vec::new();
        for mut start in core::mem::take(&mut self.starts) {
            let mut closed = false;
            loop {
                match sys::read(start.fd, &mut buf) {
                    Ok(0) => {
                        closed = true;
                        break;
                    }
                    Ok(got) => start.framer.push(buf.get(..got).unwrap_or_default()),
                    Err(_) => break,
                }
            }
            let mut said = None;
            while let Ok(Some(body)) = start.framer.next_record() {
                if let Ok(answer @ (Answer::Done(_) | Answer::Refused(_))) = Answer::decode(&body) {
                    said = Some(answer);
                }
            }
            let text = match said {
                Some(Answer::Done(result)) => {
                    alloc::format!(
                        "alert {}: init started {}: {result}",
                        start.alert,
                        start.unit
                    )
                }
                Some(Answer::Refused(why)) => alloc::format!(
                    "alert {}: init refused to start {}: {why}",
                    start.alert,
                    start.unit
                ),
                _ if closed || now.monotonic_us.saturating_sub(start.since) > ANSWER_US => {
                    alloc::format!(
                        "alert {}: init did not answer the start of {}",
                        start.alert,
                        start.unit
                    )
                }
                _ => {
                    waiting.push(start);
                    continue;
                }
            };
            sys::close(start.fd);
            out.push(ingest::own(4, MESSAGE_ID_ALERT, &text, now));
        }
        self.starts = waiting;
    }

    /// Carry out what `fired` says; what cannot be done is said in `out`.
    pub(crate) fn act(&mut self, fired: &Fired, now: Now, out: &mut Vec<Record>) {
        match &fired.action {
            Action::Notify(_) => {
                let Some(line) = fired.line() else {
                    return;
                };
                self.readers.retain(|&fd| {
                    let kept = sys::send_with(fd, line.as_bytes(), None).is_ok();
                    if !kept {
                        sys::close(fd);
                    }
                    kept
                });
            }
            Action::Start(unit) => {
                let asked = crate::cli::connect(CONTROL).and_then(|fd| {
                    let request = Call::Start(unit.clone()).encode();
                    match sys::send_with(fd, &request, None) {
                        Ok(_) => Ok(fd),
                        Err(error) => {
                            sys::close(fd);
                            Err(error)
                        }
                    }
                });
                match asked {
                    Ok(fd) => {
                        let _ = sys::set_nonblocking(fd);
                        self.starts.push(Start {
                            fd,
                            unit: unit.clone(),
                            alert: fired.name.clone(),
                            since: now.monotonic_us,
                            framer: Framer::new(),
                        });
                    }
                    Err(error) => out.push(ingest::own(
                        4,
                        MESSAGE_ID_ALERT,
                        &alloc::format!(
                            "alert {}: init's control socket: errno {}",
                            fired.name,
                            error.0
                        ),
                        now,
                    )),
                }
            }
        }
    }
}
