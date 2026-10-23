//! One connection, one request (`docs/SETTINGS-WRITER.md` §1, §5, §6):
//! read it, check it against its own rules, read the system, and then
//! either say what a dry run would do, or have `authd` decide, make the
//! change under the one lock, record it, and answer.

use std::os::fd::RawFd;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ferrix_settings_proto::{
    DecodeError, MAX_RECORD, Refusal, RefusalKind, Request, ToClient, ToServer,
};

use crate::audit;
use crate::authority::{Authority, Relay, decide};
use crate::requests::{self, System};
use crate::wire;

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
}

/// Serve one connection: read its request and answer it. Returns the final
/// answer sent, for tests.
pub(crate) fn serve(fd: RawFd, peer: Peer, context: &Context) -> ToClient {
    let answer = answer(fd, peer, context);
    let _ = wire::send(fd, &answer);
    answer
}

fn answer(fd: RawFd, peer: Peer, context: &Context) -> ToClient {
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
    let told = match authorise(fd, peer, &request, context) {
        Ok(told) => told,
        Err((refusal, told)) => {
            record(peer, &request, &told, refusal.kind.name(), "", "");
            return ToClient::Refused(refusal);
        }
    };
    // Under the lock, planned again from what is there now, so the record's
    // "before" is what was replaced, then made.
    let Ok(_held) = context.writing.lock() else {
        return refused(RefusalKind::Unavailable, "settingsd's lock is broken");
    };
    let result = requests::plan(&request, &context.system)
        .and_then(|plan| requests::apply(&plan, &context.system).map(|text| (plan, text)));
    match result {
        Ok((plan, text)) => {
            record(peer, &request, &told, "done", &plan.before, &plan.after);
            ToClient::Done {
                changed: true,
                text,
            }
        }
        Err(refusal) => {
            record(
                peer,
                &request,
                &told,
                refusal.kind.name(),
                &plan.before,
                &plan.after,
            );
            ToClient::Refused(refusal)
        }
    }
}

/// The request's own rules, as the client checks them too.
fn validate(request: &Request) -> Result<(), String> {
    request.validate().map_err(|invalid| invalid.0)
}

/// Whether `peer` may make `request`: root may; anyone else as `authd`
/// says for the request's area, about the account of the peer's uid.
fn authorise(
    fd: RawFd,
    peer: Peer,
    request: &Request,
    context: &Context,
) -> Result<Told, (Refusal, Told)> {
    if peer.uid == 0 {
        return Ok(Told {
            auth: "root",
            account: "root".to_owned(),
        });
    }
    let Some(account) = account_of(&context.passwd, peer.uid) else {
        return Err((
            Refusal::new(
                RefusalKind::NotPermitted,
                format!("uid {} has no account", peer.uid),
            ),
            Told {
                auth: "-",
                account: String::new(),
            },
        ));
    };
    let mut relay = Relay::new(fd);
    let verdict = context
        .authority
        .converse(request.area().service(), &account, &mut relay);
    // `none` is said only of an acceptance no question came before:
    // `Methods=none`.
    let auth = if relay.prompted { "password" } else { "-" };
    let mut told = Told { auth, account };
    let verdict = match verdict {
        Ok(verdict) => verdict,
        Err(why) => return Err((Refusal::new(RefusalKind::Unavailable, why), told)),
    };
    match decide(&verdict, peer.uid, &told.account, relay.cancelled) {
        Ok(()) => {
            if !relay.prompted {
                told.auth = "none";
            }
            Ok(told)
        }
        Err(refusal) => Err((refusal, told)),
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
