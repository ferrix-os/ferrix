//! One conversation's rules (`docs/AUTH.md` §3.3 to §3.5): what a record in
//! means, and what goes back.
//!
//! [`Engine::handle`] takes one record from a peer and gives the replies,
//! each with how long to hold it before it is sent. It does all of its
//! reading and writing through the [`Store`] and the files under
//! [`Paths`], and takes the time as an argument, so a test drives it with
//! no socket and no clock.
//!
//! # Who may ask what
//!
//! A caller is known by `SO_PEERCRED`'s effective uid. Any caller may be
//! authenticated as its own account. Only root may name another account,
//! and only for a service whose policy allows any account. Only root may
//! reset a throttle or ask about another account. Being root lets a caller
//! *ask*; it never lets it skip the password.
//!
//! # Failures
//!
//! A failure is counted against the account, whoever asked, and written to
//! the store before the answer goes: restarting `authd` resets nothing.
//! FAILED is held for the policy's `FailDelaySec=`, and until it has gone
//! out no attempt on that account is looked at, from any connection: a
//! guesser with a hundred connections gets one guess a delay, as one with
//! one does. From the fourth failure in a row, the next attempt is refused
//! unlooked-at until a further `2^(n-3)` seconds after that delay, capped at
//! five minutes, and a success resets the count.
//!
//! A name that is no account is checked against a decoy hash, fails with
//! the same text after the same delay, and is counted and throttled the same
//! way, in memory only (`phantom`). Its answers say nothing of whether it
//! exists, and naming accounts makes no files.

use std::time::Duration;

use ferrix_auth_proto::{Record, Secret, method};

use crate::accounts::{self, Account};
use crate::audit::{Audit, say};
use crate::local;
use crate::password::{Checked, Hasher};
use crate::paths::Paths;
use crate::phantom::Phantoms;
use crate::policy::{self, AccountRule, Callers, Policy, Purpose};
use crate::sabotage;
use crate::store::{Credential, Store, Tally};

/// The text of a wrong password, upstream hyprlock's and `pam_unix`'s.
pub(crate) const FAILED: &str = "Authentication failed";

/// Failures in a row that are free of the throttle.
const FREE_FAILURES: u32 = 3;

/// The longest throttle.
const MAX_THROTTLE: Duration = Duration::from_secs(300);

/// Who is asking, from `SO_PEERCRED`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Peer {
    /// Its pid, for the audit line only.
    pub(crate) pid: i32,
    /// Its effective uid, which is what decides.
    pub(crate) uid: u32,
}

/// What `authd` says, owned, so it can be held for a delay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Reply {
    Prompt {
        visible: bool,
        text: String,
    },
    Accepted {
        uid: u32,
        account: String,
    },
    Failed {
        retry_after_ms: u32,
        text: String,
    },
    Unavailable(String),
    /// Something to show before the next prompt.
    Info(String),
    State {
        credential: bool,
        methods: u32,
        throttled_ms: u64,
    },
}

impl Reply {
    /// The record it is sent as.
    pub(crate) fn record(&self) -> Record<'_> {
        match self {
            Reply::Prompt { visible, text } => Record::Prompt {
                visible: *visible,
                text,
            },
            Reply::Accepted { uid, account } => Record::Accepted { uid: *uid, account },
            Reply::Failed {
                retry_after_ms,
                text,
            } => Record::Failed {
                retry_after_ms: *retry_after_ms,
                text,
            },
            Reply::Unavailable(text) => Record::Unavailable(text),
            Reply::Info(text) => Record::Info(text),
            Reply::State {
                credential,
                methods,
                throttled_ms,
            } => Record::State {
                credential: *credential,
                methods: *methods,
                throttled_ms: *throttled_ms,
            },
        }
    }

    /// Whether it ends the conversation.
    pub(crate) fn is_final(&self) -> bool {
        self.record().is_final()
    }
}

/// A reply and how long to hold it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Out {
    pub(crate) reply: Reply,
    pub(crate) after: Duration,
}

impl Out {
    fn now(reply: Reply) -> Vec<Out> {
        vec![Out {
            reply,
            after: Duration::ZERO,
        }]
    }
}

/// Who a conversation is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    /// The name asked for.
    name: String,
    /// Its account, when `/etc/passwd` has one.
    account: Option<Account>,
}

/// Where one conversation is.
#[derive(Debug, Default)]
pub(crate) enum Step {
    /// Nothing has been asked yet.
    #[default]
    Start,
    /// Waiting for the password that proves who they are.
    Password { policy: Policy, target: Target },
    /// Waiting for the current password, before a change.
    Current { policy: Policy, target: Target },
    /// Waiting for the new password.
    New { policy: Policy, target: Target },
    /// Waiting for it again.
    Again {
        policy: Policy,
        target: Target,
        first: Box<Secret>,
    },
    /// Ended; anything more is ignored.
    Over,
}

/// One connection's conversation.
#[derive(Debug)]
pub(crate) struct Conversation {
    pub(crate) peer: Peer,
    pub(crate) step: Step,
}

impl Conversation {
    pub(crate) fn new(peer: Peer) -> Conversation {
        Conversation {
            peer,
            step: Step::Start,
        }
    }
}

/// Everything conversations share: the files, the hash, the log.
#[derive(Debug)]
pub(crate) struct Engine {
    paths: Paths,
    store: Store,
    hasher: Hasher,
    audit: Audit,
    phantoms: Phantoms,
    /// The seat's current lock, as `sessiond` armed it on the seat channel:
    /// the session's uid and the lock's epoch (`docs/AUTH.md` §3.7). One at
    /// a time; a new one replaces it.
    armed: Option<Armed>,
    /// Grants made and not yet sent down the seat channel.
    grants: Vec<Armed>,
    /// The controlling terminal's `tty_nr` of the root process `pid`
    /// ([`local::root_terminal`]); a test gives its own.
    local: fn(i32) -> Option<u32>,
}

/// A lock that may be granted, or the grant for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Armed {
    /// The session's user.
    pub(crate) uid: u32,
    /// The lock's number.
    pub(crate) epoch: u64,
}

/// The throttle after `failures` in a row.
pub(crate) fn throttle(failures: u32) -> Duration {
    if failures <= FREE_FAILURES {
        return Duration::ZERO;
    }
    let exponent = (failures - FREE_FAILURES).min(16);
    Duration::from_secs(1_u64 << exponent).min(MAX_THROTTLE)
}

/// `tally` after one more failure under `policy` at `now_ms`, and the
/// throttle it now carries past the policy's delay, in milliseconds.
fn failure(mut tally: Tally, policy: &Policy, now_ms: u64) -> (Tally, u64) {
    tally.failures = tally.failures.saturating_add(1);
    let delay = millis(policy.fail_delay);
    let wait = millis(throttle(tally.failures));
    tally.not_before_ms = now_ms.saturating_add(delay).saturating_add(wait);
    (tally, wait)
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

impl Engine {
    pub(crate) fn new(paths: Paths, hasher: Hasher, phantom_key: [u8; 32]) -> Engine {
        let store = Store::new(paths.clone());
        let audit = Audit::at(&paths.audit());
        Engine {
            paths,
            store,
            hasher,
            audit,
            phantoms: Phantoms::new(phantom_key),
            armed: None,
            grants: Vec::new(),
            local: local::root_terminal,
        }
    }

    /// The same, with `local` reading a pid's terminal.
    #[cfg(test)]
    pub(crate) fn with_local(mut self, local: fn(i32) -> Option<u32>) -> Engine {
        self.local = local;
        self
    }

    /// `sessiond`'s ARM, from the seat channel only: the lock `epoch` of
    /// `uid`'s session is up. It replaces any lock armed before it.
    pub(crate) fn arm(&mut self, uid: u32, epoch: u64) {
        self.armed = Some(Armed { uid, epoch });
    }

    /// `sessiond`'s DISARM: the lock `epoch` went without a grant.
    pub(crate) fn disarm(&mut self, epoch: u64) {
        if self.armed.is_some_and(|armed| armed.epoch == epoch) {
            self.armed = None;
        }
    }

    /// The seat channel closed: whatever was armed has nobody to go to.
    pub(crate) fn seat_gone(&mut self) {
        self.armed = None;
        self.grants.clear();
    }

    /// The grants to send down the seat channel, oldest first.
    pub(crate) fn take_grants(&mut self) -> Vec<Armed> {
        std::mem::take(&mut self.grants)
    }

    /// Grant the armed lock, if it is `uid`'s, and disarm it: one grant a
    /// lock. Audited with who asked and why.
    fn grant(&mut self, peer: Peer, uid: Option<u32>, service: &str, account: &str) -> bool {
        let Some(armed) = self.armed else {
            return false;
        };
        if uid.is_some_and(|uid| uid != armed.uid) {
            return false;
        }
        self.armed = None;
        self.grants.push(armed);
        let epoch = armed.epoch.to_string();
        self.log(
            peer,
            service,
            account,
            "granted",
            &format!("seat-epoch={epoch}"),
        );
        true
    }

    /// `target`'s tally: the store's for an account, the phantom table's
    /// for a name that is none.
    fn tally_of(&self, target: &Target) -> Tally {
        if target.account.is_some() {
            self.store.tally(&target.name)
        } else {
            self.phantoms.tally(&target.name)
        }
    }

    /// The store, for `main`'s seeding.
    pub(crate) fn store(&self) -> &Store {
        &self.store
    }

    /// Import the image's seeds: each account that `/etc/passwd` knows and
    /// the store has no record for gets the seed's hash (`docs/AUTH.md`
    /// §5.3). One that has a record keeps it, so a password changed since
    /// survives the next image.
    pub(crate) fn import_seeds(&mut self, now_ms: u64) {
        for (name, hash) in self.store.seeds() {
            let Some(account) = accounts::by_name(&self.paths.passwd(), &name) else {
                say(&format!(
                    "authd: the seed for {name} names no account; skipped"
                ));
                continue;
            };
            if !matches!(self.store.credential(&name), Ok(None)) {
                continue;
            }
            let usable = ferrix_argon2::phc::Encoded::parse(&hash).is_ok()
                || hash.starts_with("$6$")
                || hash.starts_with("$5$");
            if !usable {
                say(&format!(
                    "authd: the seed for {name} is not a hash authd reads; skipped"
                ));
                continue;
            }
            let credential = Credential {
                account: name.clone(),
                uid: account.uid,
                password: Some(hash),
                locked: None,
                changed: now_ms / 1000,
            };
            match self.store.set_credential(&credential) {
                Ok(()) => self.audit.line(&[("seed", &name), ("result", "imported")]),
                Err(error) => say(&format!(
                    "authd: the seed for {name} could not be stored: {error}"
                )),
            }
        }
    }

    /// Take one record from `conversation`'s peer.
    pub(crate) fn handle(
        &mut self,
        conversation: &mut Conversation,
        record: &Record<'_>,
        now_ms: u64,
    ) -> Vec<Out> {
        let peer = conversation.peer;
        let step = std::mem::replace(&mut conversation.step, Step::Over);
        let (next, outs) = match (step, record) {
            (Step::Over, _) | (_, Record::Cancel) => (Step::Over, Vec::new()),
            (
                Step::Start,
                &Record::Begin {
                    service, account, ..
                },
            ) => self.begin(peer, service, account, now_ms),
            (Step::Start, &Record::Status { account }) => {
                (Step::Over, self.status(peer, account, now_ms))
            }
            (Step::Start, &Record::Reset { account }) => {
                (Step::Over, self.reset(peer, account, now_ms))
            }
            (Step::Start, Record::UnlockSeat) => (Step::Over, self.unlock_seat(peer)),
            (step, Record::Respond(response)) => {
                let secret = Secret::from_bytes(response.bytes()).unwrap_or_default();
                self.respond(peer, step, secret, now_ms)
            }
            _ => (
                Step::Over,
                Out::now(Reply::Unavailable(
                    "authd did not expect that record here".to_owned(),
                )),
            ),
        };
        conversation.step = next;
        outs
    }

    fn begin(&mut self, peer: Peer, service: &str, account: &str, now_ms: u64) -> (Step, Vec<Out>) {
        let over = |text: String| (Step::Over, Out::now(Reply::Unavailable(text)));
        let Some(policy) = policy::load(&self.paths.service_layers(), service, &mut |warning| {
            say(&format!("authd: {warning}"));
        }) else {
            self.log(peer, service, account, "unavailable", "no-such-service");
            return over(format!("there is no {service} service"));
        };
        if policy.callers == Callers::Root && peer.uid != 0 {
            self.log(peer, service, account, "unavailable", "not-root");
            return over(format!("only root may use the {service} service"));
        }
        let target = match self.target(peer, account, policy.account) {
            Ok(target) => target,
            Err(why) => {
                self.log(peer, service, account, "unavailable", "named-another");
                return over(why);
            }
        };
        // `TargetGroup=` (`su`'s `wheel`): an account outside it is refused
        // before any password is asked, so a non-member gets no more of an
        // oracle from it than from any service of its own account.
        if let Some(group) = &policy.target_group {
            let member = target
                .account
                .as_ref()
                .is_some_and(|account| accounts::in_group(&self.paths.group(), account, group));
            if !member {
                self.log(peer, service, &target.name, "unavailable", "not-in-group");
                return over(format!("{} is not in {group}", target.name));
            }
        }
        if let Some(wait) = self.throttled(&target, now_ms) {
            self.log(peer, service, &target.name, "throttled", "");
            return (Step::Over, Out::now(wait));
        }
        let root_changes = policy.purpose == Purpose::ChangePassword && peer.uid == 0;
        if target.account.is_some() && !root_changes {
            match self.store.credential(&target.name) {
                Err(_) => {
                    self.log(peer, service, &target.name, "unavailable", "damaged-record");
                    return over(format!(
                        "the store's record for {} cannot be read",
                        target.name
                    ));
                }
                Ok(None) => return self.first_password(peer, policy, target),
                Ok(Some(credential)) if credential.locked.is_some() => {
                    self.log(peer, service, &target.name, "unavailable", "locked");
                    return over(format!("{} is locked", target.name));
                }
                Ok(Some(credential)) if credential.password.is_none() => {
                    return self.first_password(peer, policy, target);
                }
                Ok(Some(_)) => {}
            }
        }
        let prompt = |text: &str| {
            Out::now(Reply::Prompt {
                visible: false,
                text: text.to_owned(),
            })
        };
        match policy.purpose {
            Purpose::Authenticate => (Step::Password { policy, target }, prompt("Password: ")),
            Purpose::ChangePassword if root_changes => {
                if target.account.is_none() {
                    return over(format!("there is no account called {}", target.name));
                }
                (Step::New { policy, target }, prompt("New password: "))
            }
            Purpose::ChangePassword => (
                Step::Current { policy, target },
                prompt("Current password: "),
            ),
        }
    }

    /// An account with no credential (`docs/AUTH.md` §5.4): no password is
    /// never "any password". Under a `FirstPassword=local` policy, asked by
    /// root from a process whose controlling terminal is the console --
    /// read here from `/proc`, never taken from the caller -- the person may
    /// choose one now, typed twice, and is then let in with it. Anything
    /// else, a pty and ssh among it, is refused as before.
    ///
    /// Only a person's account is offered one (the certification
    /// consultant's F1): never root, whose record is absent so that nobody
    /// logs in as root by password, and never a system account such as
    /// `auth`, nor one whose shell is `nologin` or `false`. The audit line
    /// says which terminal was seen.
    fn first_password(&mut self, peer: Peer, policy: Policy, target: Target) -> (Step, Vec<Out>) {
        let terminal = (peer.uid == 0 && policy.first_password_local)
            .then(|| (self.local)(peer.pid))
            .flatten();
        let persons = target.account.as_ref().is_some_and(Account::is_a_persons);
        let offered = policy.first_password_local
            && peer.uid == 0
            && persons
            && terminal == Some(local::CONSOLE);
        let seen = terminal.map_or_else(|| "none".to_owned(), |nr| nr.to_string());
        if !offered {
            let why = if persons {
                format!("no-credential,tty_nr={seen}")
            } else {
                "no-credential,not-a-persons-account".to_owned()
            };
            self.log(peer, &policy.service, &target.name, "unavailable", &why);
            return (
                Step::Over,
                Out::now(Reply::Unavailable(format!(
                    "no password is set for {}",
                    target.name
                ))),
            );
        }
        self.log(
            peer,
            &policy.service,
            &target.name,
            "offered",
            &format!("first-password,tty_nr={seen}"),
        );
        let mut outs = Out::now(Reply::Info(format!(
            "{} has no password. Choose one now:",
            target.name
        )));
        outs.extend(Out::now(Reply::Prompt {
            visible: false,
            text: "New password: ".to_owned(),
        }));
        (Step::New { policy, target }, outs)
    }

    /// Who `account` names, under `rule`, for `peer`.
    fn target(&self, peer: Peer, account: &str, rule: AccountRule) -> Result<Target, String> {
        let passwd = self.paths.passwd();
        let own = accounts::by_uid(&passwd, peer.uid);
        if account.is_empty() || own.as_ref().is_some_and(|own| own.name == account) {
            let own =
                own.ok_or_else(|| format!("uid {} has no account in /etc/passwd", peer.uid))?;
            return Ok(Target {
                name: own.name.clone(),
                account: Some(own),
            });
        }
        if rule == AccountRule::Caller {
            return Err("this service checks only the caller's own account".to_owned());
        }
        if peer.uid != 0 && !sabotage::is("let-anyone-name") {
            return Err("only root may name another account".to_owned());
        }
        Ok(Target {
            name: account.to_owned(),
            account: accounts::by_name(&passwd, account),
        })
    }

    /// FAILED with the wait, if the account is throttled now.
    fn throttled(&self, target: &Target, now_ms: u64) -> Option<Reply> {
        if sabotage::is("no-throttle") {
            return None;
        }
        let tally = self.tally_of(target);
        let left = tally
            .not_before_ms
            .checked_sub(now_ms)
            .filter(|&left| left > 0)?;
        Some(Reply::Failed {
            retry_after_ms: u32::try_from(left).unwrap_or(u32::MAX),
            text: format!("wait {} s", left.div_ceil(1000)),
        })
    }

    fn respond(&mut self, peer: Peer, step: Step, secret: Secret, now_ms: u64) -> (Step, Vec<Out>) {
        match step {
            Step::Password { policy, target } => {
                let outs = match self.verify(peer, &policy, &target, &secret, now_ms) {
                    Ok(account) => {
                        self.log(peer, &policy.service, &target.name, "accepted", "");
                        // A `Grant=seat` service's acceptance is the seat's
                        // grant, for the armed lock of this account alone.
                        if policy.grant_seat {
                            let _ =
                                self.grant(peer, Some(account.uid), &policy.service, &target.name);
                        }
                        Out::now(Reply::Accepted {
                            uid: account.uid,
                            account: account.name,
                        })
                    }
                    Err(outs) => outs,
                };
                (Step::Over, outs)
            }
            Step::Current { policy, target } => {
                match self.verify(peer, &policy, &target, &secret, now_ms) {
                    Ok(_) => (
                        Step::New { policy, target },
                        Out::now(Reply::Prompt {
                            visible: false,
                            text: "New password: ".to_owned(),
                        }),
                    ),
                    Err(outs) => (Step::Over, outs),
                }
            }
            Step::New { policy, target } => {
                if secret.is_empty() {
                    return (
                        Step::Over,
                        Out::now(Reply::Failed {
                            retry_after_ms: 0,
                            text: "a password may not be empty".to_owned(),
                        }),
                    );
                }
                (
                    Step::Again {
                        policy,
                        target,
                        first: Box::new(secret),
                    },
                    Out::now(Reply::Prompt {
                        visible: false,
                        text: "Retype new password: ".to_owned(),
                    }),
                )
            }
            Step::Again {
                policy,
                target,
                first,
            } => {
                if !ferrix_argon2::equal(first.expose(), secret.expose()) {
                    return (
                        Step::Over,
                        Out::now(Reply::Failed {
                            retry_after_ms: 0,
                            text: "the passwords did not match".to_owned(),
                        }),
                    );
                }
                (
                    Step::Over,
                    self.change(peer, &policy, &target, &secret, now_ms),
                )
            }
            Step::Start | Step::Over => (
                Step::Over,
                Out::now(Reply::Unavailable("nothing was asked".to_owned())),
            ),
        }
    }

    /// Check `secret` for `target`: its account on a match, else the
    /// replies that end the conversation.
    fn verify(
        &mut self,
        peer: Peer,
        policy: &Policy,
        target: &Target,
        secret: &Secret,
        now_ms: u64,
    ) -> Result<Account, Vec<Out>> {
        let mut say_line = |line: String| say(&line);
        let failed = |retry_after_ms: u32| {
            vec![Out {
                reply: Reply::Failed {
                    retry_after_ms,
                    text: FAILED.to_owned(),
                },
                after: policy.fail_delay,
            }]
        };
        let Some(account) = target.account.clone() else {
            if sabotage::is("tell-unknown") {
                return Err(Out::now(Reply::Unavailable(
                    "there is no such account".to_owned(),
                )));
            }
            self.hasher.decoy(secret, &mut say_line);
            // Counted as an account's failure is, in memory, so the answers
            // that follow are the ones an account's would be.
            let (tally, wait) = failure(self.phantoms.tally(&target.name), policy, now_ms);
            self.phantoms.set(&target.name, tally);
            self.log(
                peer,
                &policy.service,
                &target.name,
                "failed",
                "unknown-account",
            );
            return Err(failed(u32::try_from(wait).unwrap_or(u32::MAX)));
        };
        let credential = match self.store.credential(&account.name) {
            Ok(Some(credential)) if credential.uid == account.uid => credential,
            _ => {
                self.log(
                    peer,
                    &policy.service,
                    &account.name,
                    "unavailable",
                    "no-credential",
                );
                return Err(Out::now(Reply::Unavailable(format!(
                    "no password is set for {}",
                    account.name
                ))));
            }
        };
        let stored = credential.password.clone().unwrap_or_default();
        let checked = match self.hasher.check(secret, &stored, &mut say_line) {
            Checked::Mismatch if sabotage::is("accept-any") => Checked::Match { rehash: false },
            other => other,
        };
        match checked {
            Checked::Match { rehash } => {
                if self.store.tally(&account.name).failures > 0 {
                    let _ = self.store.set_tally(&account.name, Tally::default());
                }
                if rehash && let Some(fresh) = self.hasher.make(secret, &mut say_line) {
                    let updated = Credential {
                        password: Some(fresh),
                        changed: now_ms / 1000,
                        ..credential
                    };
                    if self.store.set_credential(&updated).is_ok() {
                        self.log(peer, &policy.service, &account.name, "rehashed", "");
                    }
                }
                Ok(account)
            }
            Checked::Mismatch => {
                let (tally, wait) = failure(self.store.tally(&account.name), policy, now_ms);
                let failures = tally.failures.to_string();
                if let Err(error) = self.store.set_tally(&account.name, tally) {
                    say(&format!(
                        "authd: the tally for {} could not be stored: {error}",
                        account.name
                    ));
                }
                self.log(
                    peer,
                    &policy.service,
                    &account.name,
                    "failed",
                    &format!("failures:{failures}"),
                );
                Err(failed(u32::try_from(wait).unwrap_or(u32::MAX)))
            }
            Checked::Unusable => {
                self.log(
                    peer,
                    &policy.service,
                    &account.name,
                    "unavailable",
                    "unusable-hash",
                );
                Err(Out::now(Reply::Unavailable(
                    "the stored password cannot be checked here".to_owned(),
                )))
            }
        }
    }

    /// Set `target`'s password to `secret`.
    fn change(
        &mut self,
        peer: Peer,
        policy: &Policy,
        target: &Target,
        secret: &Secret,
        now_ms: u64,
    ) -> Vec<Out> {
        let Some(account) = target.account.clone() else {
            return Out::now(Reply::Unavailable(format!(
                "there is no account called {}",
                target.name
            )));
        };
        let mut say_line = |line: String| say(&line);
        let Some(hash) = self.hasher.make(secret, &mut say_line) else {
            return Out::now(Reply::Unavailable(
                "the new password could not be hashed".to_owned(),
            ));
        };
        let credential = Credential {
            account: account.name.clone(),
            uid: account.uid,
            password: Some(hash),
            locked: None,
            changed: now_ms / 1000,
        };
        if let Err(error) = self.store.set_credential(&credential) {
            say(&format!(
                "authd: {}'s record could not be written: {error}",
                account.name
            ));
            return Out::now(Reply::Unavailable(
                "the store could not be written".to_owned(),
            ));
        }
        let _ = self.store.set_tally(&account.name, Tally::default());
        // A password chosen where none was is the first; through `passwd`
        // it is a change.
        let result = if policy.purpose == Purpose::Authenticate {
            "first-password"
        } else {
            "changed"
        };
        self.log(peer, &policy.service, &account.name, result, "");
        Out::now(Reply::Accepted {
            uid: account.uid,
            account: account.name,
        })
    }

    fn status(&mut self, peer: Peer, account: &str, now_ms: u64) -> Vec<Out> {
        let target = match self.target(peer, account, AccountRule::Any) {
            Ok(target) => target,
            Err(why) => return Out::now(Reply::Unavailable(why)),
        };
        let credential = self
            .store
            .credential(&target.name)
            .ok()
            .flatten()
            .filter(|credential| {
                credential.password.is_some()
                    && credential.locked.is_none()
                    && target
                        .account
                        .as_ref()
                        .is_some_and(|account| account.uid == credential.uid)
            });
        let throttled_ms = self.tally_of(&target).not_before_ms.saturating_sub(now_ms);
        Out::now(Reply::State {
            credential: credential.is_some(),
            methods: if credential.is_some() {
                method::PASSWORD
            } else {
                0
            },
            throttled_ms,
        })
    }

    fn reset(&mut self, peer: Peer, account: &str, now_ms: u64) -> Vec<Out> {
        if peer.uid != 0 {
            self.log(peer, "reset", account, "unavailable", "not-root");
            return Out::now(Reply::Unavailable(
                "only root may reset a throttle".to_owned(),
            ));
        }
        if accounts::by_name(&self.paths.passwd(), account).is_none() {
            return Out::now(Reply::Unavailable(format!(
                "there is no account called {account}"
            )));
        }
        if let Err(error) = self.store.set_tally(account, Tally::default()) {
            return Out::now(Reply::Unavailable(format!(
                "the tally could not be written: {error}"
            )));
        }
        self.log(peer, "reset", account, "reset", "");
        self.status(peer, account, now_ms)
    }

    /// `authctl unlock-seat`: root's audited override (decision 11). It
    /// grants the armed lock whoever's it is; the audit line says root asked,
    /// through which process, and for which lock.
    fn unlock_seat(&mut self, peer: Peer) -> Vec<Out> {
        if peer.uid != 0 {
            self.log(peer, "unlock-seat", "", "refused", "not-root");
            return Out::now(Reply::Unavailable(
                "only root may let the seat's lock go".to_owned(),
            ));
        }
        if self.grant(peer, None, "unlock-seat", "root") {
            return Out::now(Reply::Accepted {
                uid: 0,
                account: "root".to_owned(),
            });
        }
        self.log(peer, "unlock-seat", "", "unavailable", "no-lock-armed");
        Out::now(Reply::Unavailable(
            "no lock is up on the seat, or no session's compositor is listening".to_owned(),
        ))
    }

    fn log(&mut self, peer: Peer, service: &str, account: &str, result: &str, why: &str) {
        let pid = peer.pid.to_string();
        let uid = peer.uid.to_string();
        let mut fields = vec![
            ("service", service),
            ("account", account),
            ("peer-pid", pid.as_str()),
            ("peer-uid", uid.as_str()),
            ("method", "password"),
            ("result", result),
        ];
        if !why.is_empty() {
            fields.push(("why", why));
        }
        self.audit.line(&fields);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_throttle_starts_at_the_fourth_failure_and_stops_growing() {
        for (failures, seconds) in [
            (0, 0),
            (3, 0),
            (4, 2),
            (5, 4),
            (7, 16),
            (11, 256),
            (12, 300),
            (40, 300),
        ] {
            assert_eq!(
                throttle(failures),
                Duration::from_secs(seconds),
                "{failures}"
            );
        }
    }
}
