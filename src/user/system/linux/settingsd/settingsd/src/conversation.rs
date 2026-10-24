//! One connection, one request (`docs/SETTINGS-WRITER.md` §1, §5, §6,
//! §12.5): read it, check it against its own rules, read the system, and
//! then either say what a dry run would do, or check who asked (S3), have
//! `authd` decide with the person at the seat or at the client's terminal
//! answering (S4, S5), make the change under the one lock, record it, and
//! answer.

use std::collections::HashMap;
use std::os::fd::RawFd;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use compositor_seat::prompt::ToSeat;

use ferrix_settings_proto::{
    DecodeError, MAX_RECORD, Refusal, RefusalKind, Request, ToClient, ToServer,
};

use crate::audit;
use crate::authority::{Authority, Relay, decide};
use crate::identity::{Identify, Program, bound};
use crate::requests::{self, Plan, System};
use crate::seat::{Channel, OnSeat, Said, Unable, seat_accounts};
use crate::{apps, wire};

/// How long a person's Cancel keeps a program from asking for the same
/// area again (S6).
const PAUSE_AFTER_CANCEL: Duration = Duration::from_secs(30);

/// How long a client has to send its request after connecting.
const REQUEST_WITHIN: Duration = Duration::from_secs(10);

/// Everything a conversation reaches.
pub(crate) struct Context {
    /// What requests read and change.
    pub(crate) system: System,
    /// Who decides.
    pub(crate) authority: Box<dyn Authority>,
    /// The accounts, for the name of the uid that asked.
    pub(crate) passwd: PathBuf,
    /// Held while a change is planned again and made: one writer at a time.
    pub(crate) writing: Mutex<()>,
    /// Who a pid is.
    pub(crate) identify: Box<dyn Identify>,
    /// Where the seat's prompt listens.
    pub(crate) prompt: PathBuf,
    /// The uid the allow-lists' files must belong to: root on the machine.
    pub(crate) trusted_uid: u32,
    /// The uid the seat's prompt listener must be: root on the machine.
    pub(crate) listener_uid: u32,
    /// When a person last said no, by uid, program and area (S6).
    pub(crate) cancelled: Mutex<HashMap<(u32, String, &'static str), Instant>>,
    /// The next prompt's number.
    pub(crate) prompts: AtomicU64,
}

/// Who is at the other end, as `SO_PEERCRED` recorded at `connect`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Peer {
    /// Its pid: for the audit record only, never evidence.
    pub(crate) pid: i32,
    /// Its effective uid: what decides whose account `authd` is asked about.
    pub(crate) uid: u32,
}

/// What a request came to, for the audit record.
struct Told {
    auth: &'static str,
    account: String,
    program: String,
}

impl Told {
    fn nobody(program: &str) -> Told {
        Told {
            auth: "-",
            account: String::new(),
            program: program.to_owned(),
        }
    }
}

/// Serve one connection: read its request and answer it. Returns the final
/// answer sent, for tests.
pub(crate) fn serve(fd: RawFd, peer: Peer, context: &Context) -> ToClient {
    let answer = answer(fd, peer, context);
    let _ = wire::send(fd, &answer);
    answer
}

fn answer(fd: RawFd, peer: Peer, context: &Context) -> ToClient {
    // S3: who asked, as the connection was taken; read again below.
    let at_accept = (peer.uid != 0)
        .then(|| context.identify.of(peer.pid))
        .flatten();
    let mut buffer = vec![0_u8; MAX_RECORD + 1];
    let Some(packet) = wire::receive_within(fd, &mut buffer, Instant::now() + REQUEST_WITHIN)
    else {
        return refused(RefusalKind::Cancelled, "no request came");
    };
    let decoded = ToServer::decode(packet);
    buffer.fill(0);
    let (dry_run, request) = match decoded {
        Ok(ToServer::Request { dry_run, request }) => (dry_run, request),
        Ok(_) => return refused(RefusalKind::Invalid, "a conversation starts with a request"),
        Err(DecodeError::Request) => {
            return refused(
                RefusalKind::Unsupported,
                format!("this settingsd knows only {}", Request::NAMES.join(", ")),
            );
        }
        Err(DecodeError::Version) => {
            return refused(
                RefusalKind::Unsupported,
                "the client speaks a version of the protocol this settingsd does not",
            );
        }
        Err(why) => return refused(RefusalKind::Invalid, format!("not a request: {why:?}")),
    };
    if let Err(invalid) = validate(&request) {
        return refused(RefusalKind::Invalid, invalid);
    }
    let plan = match requests::plan(&request, &context.system) {
        Ok(plan) => plan,
        Err(refusal) => return ToClient::Refused(refusal),
    };
    if dry_run {
        return ToClient::Done {
            changed: plan.changed,
            text: plan.would,
        };
    }
    let (told, on_seat) = match authorise(fd, peer, &request, &plan, at_accept.as_ref(), context) {
        Ok(told) => told,
        Err((refusal, told, result)) => {
            record(
                peer,
                &request,
                &told,
                result.unwrap_or(refusal.kind.name()),
                "",
                "",
            );
            return ToClient::Refused(refusal);
        }
    };
    let answer = apply(peer, &request, &plan, &told, context);
    if let Some(on_seat) = on_seat {
        on_seat.close(match &answer {
            ToClient::Done { text, .. } => text,
            ToClient::Refused(refusal) => &refusal.text,
            _ => "",
        });
    }
    answer
}

/// Make the change, authorised: under the lock, planned again from what is
/// there now, then recorded.
fn apply(peer: Peer, request: &Request, plan: &Plan, told: &Told, context: &Context) -> ToClient {
    // Under the lock, planned again from what is there now, so the record's
    // "before" is what was replaced, then made.
    let Ok(_held) = context.writing.lock() else {
        return refused(RefusalKind::Unavailable, "settingsd's lock is broken");
    };
    let result = requests::plan(request, &context.system)
        .and_then(|plan| requests::apply(&plan, &context.system).map(|text| (plan, text)));
    match result {
        Ok((plan, text)) => {
            record(peer, request, told, "done", &plan.before, &plan.after);
            ToClient::Done {
                changed: true,
                text,
            }
        }
        Err(refusal) => {
            // `now` whose links were made and whose start or stop was not:
            // the boot's setting did change, and the record says that.
            let partial = match &plan.action {
                requests::Action::Service {
                    enabled, now: true, ..
                } if refusal.text.contains(requests::service::PARTIAL) => Some(if *enabled {
                    ("start-failed", "on")
                } else {
                    ("stop-failed", "off")
                }),
                _ => None,
            };
            match partial {
                Some((result, after)) => record(peer, request, told, result, &plan.before, after),
                None => record(
                    peer,
                    request,
                    told,
                    refusal.kind.name(),
                    &plan.before,
                    &plan.after,
                ),
            }
            ToClient::Refused(refusal)
        }
    }
}

/// The request's own rules, as the client checks them too.
fn validate(request: &Request) -> Result<(), String> {
    request.validate().map_err(|invalid| invalid.0)
}

/// A refusal of [`authorise`]'s: the answer, what the record says, and
/// the record's own result word when it is not the refusal's kind.
type Refused = (Refusal, Told, Option<&'static str>);

/// Whether `peer` may make `request`: root may; anyone else only from a
/// program on the area's list (S3), and as `authd` says for the area about
/// the account of the peer's uid, with the person at the seat answering
/// for a seat account (S4), or at the client's terminal for another.
fn authorise(
    fd: RawFd,
    peer: Peer,
    request: &Request,
    plan: &Plan,
    at_accept: Option<&Program>,
    context: &Context,
) -> Result<(Told, Option<OnSeat>), Refused> {
    if peer.uid == 0 {
        return Ok((
            Told {
                auth: "root",
                account: "root".to_owned(),
                program: String::new(),
            },
            None,
        ));
    }
    let area = request.area();
    // S3: the program bound to the connection, and on the area's list.
    let now = context.identify.of(peer.pid);
    let program = now
        .as_ref()
        .map(|program| program.exe.clone())
        .unwrap_or_default();
    if !bound(at_accept, now.as_ref()) {
        return Err((
            Refusal::new(
                RefusalKind::NotPermitted,
                "the program that asked is not the one that connected",
            ),
            Told::nobody(&program),
            Some("not-bound"),
        ));
    }
    let parent = now
        .as_ref()
        .map(|program| program.parent.clone())
        .unwrap_or_default();
    if !apps::allowed(
        &context.system.root,
        context.trusted_uid,
        area.name(),
        &program,
        &mut |w| {
            audit::say(w);
        },
    ) {
        return Err((
            Refusal::new(
                RefusalKind::NotPermitted,
                format!(
                    "{program} may not ask to change {}",
                    area_words(area.name())
                ),
            ),
            Told::nobody(&program),
            Some("not-listed"),
        ));
    }
    let Some(account) = account_of(&context.passwd, peer.uid) else {
        return Err((
            Refusal::new(
                RefusalKind::NotPermitted,
                format!("uid {} has no account", peer.uid),
            ),
            Told::nobody(&program),
            None,
        ));
    };
    // S6: a person said no a moment ago.
    let key = (peer.uid, program.clone(), area.name());
    let paused = context
        .cancelled
        .lock()
        .ok()
        .and_then(|cancelled| cancelled.get(&key).copied())
        .is_some_and(|at| at.elapsed() < PAUSE_AFTER_CANCEL);
    let told = |auth| Told {
        auth,
        account: account.clone(),
        program: program.clone(),
    };
    if paused {
        return Err((
            Refusal::new(
                RefusalKind::Cancelled,
                "you said no to this a moment ago; try again later",
            ),
            told("-"),
            Some("paused"),
        ));
    }
    // S4: the image's word on who has the seat.
    let seats = seat_accounts(&context.system.root).map_err(|why| {
        (
            Refusal::new(
                RefusalKind::Unavailable,
                format!("who has the desktop cannot be told: {why}"),
            ),
            told("-"),
            None,
        )
    })?;
    if seats.contains(&account) {
        on_seat(
            fd,
            peer,
            request,
            plan,
            (&account, &program, &parent),
            context,
        )
        .map(|(auth, seat)| (told(auth), Some(seat)))
        .map_err(|(refusal, auth, result)| {
            if refusal.kind == RefusalKind::Cancelled
                && let Ok(mut cancelled) = context.cancelled.lock()
            {
                let _ = cancelled.insert(key.clone(), Instant::now());
            }
            (refusal, told(auth), result)
        })
    } else {
        relayed(fd, peer, request, &account, context)
            .map(|auth| (told(auth), None))
            .map_err(|(refusal, auth, result)| (refusal, told(auth), result))
    }
}

/// A refusal of [`on_seat`]'s or [`relayed`]'s: the answer, the audit's
/// `auth`, and its own result word.
type Turned = (Refusal, &'static str, Option<&'static str>);

/// The conversation for a seat account: the person at the seat answers in
/// the compositor's prompt, and the client hears only that it is asked
/// and how it ended.
fn on_seat(
    fd: RawFd,
    peer: Peer,
    request: &Request,
    plan: &Plan,
    (account, program, parent): (&str, &str, &str),
    context: &Context,
) -> Result<(&'static str, OnSeat), Turned> {
    let channel =
        Channel::open(&context.prompt, context.listener_uid, peer.uid).map_err(|unable| {
            let text = match unable {
                Unable::NoDesktop => {
                    "confirm on the desktop; no desktop is running for you".to_owned()
                }
                Unable::Unavailable(why) => why,
            };
            (
                Refusal::new(RefusalKind::Unavailable, text),
                "-",
                Some("no-seat"),
            )
        })?;
    let _ = wire::send(
        fd,
        &ToClient::Info("confirm the change on the screen".to_owned()),
    );
    let id = context.prompts.fetch_add(1, Ordering::Relaxed);
    let ask = ToSeat::Ask {
        id,
        area: request.area().name().to_owned(),
        program: program.to_owned(),
        parent: parent.to_owned(),
        account: account.to_owned(),
        what: plan.would.clone(),
    };
    let mut seat = OnSeat::new(channel, id, ask);
    let verdict = context
        .authority
        .converse(request.area().service(), account, &mut seat);
    let asked = seat.said == Said::Answered;
    let auth = if asked { "seat+password" } else { "seat" };
    let verdict = match verdict {
        Ok(verdict) => verdict,
        Err(why) => {
            seat.close(&why);
            return Err((Refusal::new(RefusalKind::Unavailable, why), auth, None));
        }
    };
    let cancelled = seat.said == Said::Denied;
    if let Err(refusal) = decide(&verdict, peer.uid, account, cancelled) {
        seat.close(&refusal.text);
        let result = (refusal.kind == RefusalKind::Cancelled).then_some("cancelled");
        return Err((refusal, auth, result));
    }
    // ACCEPTED with no question (`Methods=none`): the person still says
    // yes on the seat before anything is made.
    if !asked && !seat.confirm() {
        let why = if seat.why.is_empty() {
            "cancelled"
        } else {
            seat.why.as_str()
        };
        let text = format!("nothing was changed: the prompt was {why}");
        seat.close(&text);
        return Err((
            Refusal::new(RefusalKind::Cancelled, text),
            auth,
            Some("cancelled"),
        ));
    }
    Ok((auth, seat))
}

/// The conversation for an account the image gives no seat: relayed to
/// the client, as B1 does -- but an acceptance no one typed anything for
/// is refused (ledger 3636 (2)).
fn relayed(
    fd: RawFd,
    peer: Peer,
    request: &Request,
    account: &str,
    context: &Context,
) -> Result<&'static str, Turned> {
    let mut relay = Relay::new(fd);
    let verdict = context
        .authority
        .converse(request.area().service(), account, &mut relay);
    let auth = if relay.prompted { "password" } else { "-" };
    let verdict =
        verdict.map_err(|why| (Refusal::new(RefusalKind::Unavailable, why), auth, None))?;
    decide(&verdict, peer.uid, account, relay.cancelled)
        .map_err(|refusal| (refusal, auth, None))?;
    if !relay.prompted {
        return Err((
            Refusal::new(
                RefusalKind::NotPermitted,
                "needs the desktop: this area asks no password, so it must be confirmed on the seat",
            ),
            "none",
            Some("needs-seat"),
        ));
    }
    Ok(auth)
}

/// The words for an area, in a refusal.
fn area_words(area: &str) -> &'static str {
    match area {
        "hostname" => "the computer's name",
        "time" => "the clock or the time zone",
        "services" => "which services run",
        _ => "this setting",
    }
}

/// The name `/etc/passwd` gives `uid`.
pub(crate) fn account_of(passwd: &std::path::Path, uid: u32) -> Option<String> {
    let text = std::fs::read_to_string(passwd).ok()?;
    text.lines().find_map(|line| {
        let mut fields = line.split(':');
        let name = fields.next()?;
        let _password = fields.next()?;
        let found: u32 = fields.next()?.parse().ok()?;
        (found == uid && !name.is_empty()).then(|| name.to_owned())
    })
}

/// The audit record for a request that reached authorisation.
fn record(peer: Peer, request: &Request, told: &Told, result: &str, before: &str, after: &str) {
    let pid = peer.pid.to_string();
    let uid = peer.uid.to_string();
    let comm = std::fs::read_to_string(format!("/proc/{}/comm", peer.pid))
        .map(|text| text.trim_end().to_owned())
        .unwrap_or_default();
    audit::say(&audit::record(&[
        ("request", request.name()),
        ("peer-pid", &pid),
        ("peer-uid", &uid),
        ("comm", &comm),
        ("program", &told.program),
        ("account", &told.account),
        ("auth", told.auth),
        ("result", result),
        ("before", before),
        ("after", after),
    ]));
}

fn refused(kind: RefusalKind, text: impl Into<String>) -> ToClient {
    ToClient::Refused(Refusal::new(kind, text))
}
