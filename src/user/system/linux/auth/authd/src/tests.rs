//! The engine's rules, driven with no socket: each test builds a root of
//! its own with `/etc/passwd`, the shipped policies, a gate policy and the
//! credentials it needs, and plays one peer or another.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ferrix_auth_proto::{Record, Response, method};

use crate::engine::{Armed, Conversation, Engine, FAILED, Out, Peer, Reply};
use crate::password::{FLOOR, Hasher};
use crate::paths::Paths;
use crate::store::{Credential, Store};

const ROOT: Peer = Peer { pid: 1, uid: 0 };
const FERRIX: Peer = Peer { pid: 2, uid: 1000 };
const OTHER: Peer = Peer { pid: 3, uid: 1001 };

/// A root under the temporary directory, with accounts and policies.
struct Machine {
    root: PathBuf,
    engine: Engine,
    now_ms: u64,
}

impl Machine {
    fn new(name: &str) -> Machine {
        let root = std::env::temp_dir().join(format!("authd-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        write(
            &root.join("etc/passwd"),
            "root:x:0:0:root:/:/bin/sh\nferrix:x:1000:1000:ferrix:/home/ferrix:/bin/sh\nother:x:1001:1001::/:/bin/sh\nauth:x:90:90::/:/sbin/nologin\n",
        );
        let services = root.join("lib/ferrix/auth/services");
        write(
            &root.join("etc/group"),
            "root:x:0:\nwheel:x:10:ferrix\nferrix:x:1000:\nother:x:1001:\n",
        );
        for service in ["hyprlock", "passwd", "login", "su"] {
            let shipped = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../services")
                .join(service);
            write(
                &services.join(service),
                &std::fs::read_to_string(shipped).unwrap(),
            );
        }
        write(
            &services.join("gate"),
            "[Service]\nAccount=any\nMethods=password\nFailDelaySec=2\n",
        );
        let paths = Paths::under(&root);
        let engine = Engine::new(paths, Hasher::fixed(FLOOR), [9; 32]);
        engine.store().prepare().unwrap();
        Machine {
            root,
            engine,
            now_ms: 1_790_000_000_000,
        }
    }

    /// Give `account` (uid `uid`) the password `password`.
    fn set(&self, account: &str, uid: u32, password: &str) {
        let mut hasher = Hasher::fixed(FLOOR);
        let secret = ferrix_auth_proto::Secret::from_bytes(password.as_bytes()).unwrap();
        let hash = hasher.make(&secret, &mut |_| {}).unwrap();
        Store::new(Paths::under(&self.root))
            .set_credential(&Credential {
                account: account.to_owned(),
                uid,
                password: Some(hash),
                locked: None,
                changed: 0,
            })
            .unwrap();
    }

    /// Run a conversation for `service` and `account` as `peer`, answering
    /// each prompt with the next of `answers`; the replies in order.
    fn converse(&mut self, peer: Peer, service: &str, account: &str, answers: &[&str]) -> Vec<Out> {
        let mut conversation = Conversation::new(peer);
        let mut all = self.engine.handle(
            &mut conversation,
            &Record::Begin {
                service,
                account,
                method: "",
            },
            self.now_ms,
        );
        let mut answers = answers.iter();
        while matches!(
            all.last(),
            Some(Out {
                reply: Reply::Prompt { .. },
                ..
            })
        ) {
            let answer = answers.next().expect("a prompt the test did not expect");
            let more = self.engine.handle(
                &mut conversation,
                &Record::Respond(Response(answer.as_bytes())),
                self.now_ms,
            );
            all.extend(more);
        }
        // A held reply is sent only when its delay has passed, and a client
        // asks again only after it has its answer.
        let held = all.iter().map(|out| out.after).max().unwrap_or_default();
        self.now_ms += u64::try_from(held.as_millis()).unwrap();
        all
    }

    fn verdict(&mut self, peer: Peer, service: &str, account: &str, answers: &[&str]) -> Out {
        self.converse(peer, service, account, answers)
            .pop()
            .unwrap()
    }

    fn one(&mut self, peer: Peer, record: &Record<'_>) -> Reply {
        let mut conversation = Conversation::new(peer);
        let mut outs = self.engine.handle(&mut conversation, record, self.now_ms);
        assert_eq!(outs.len(), 1, "{outs:?}");
        outs.pop().unwrap().reply
    }

    fn audit(&self) -> String {
        std::fs::read_to_string(self.root.join("var/log/ferrix/auth.log")).unwrap_or_default()
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn accepted(account: &str, uid: u32) -> Out {
    Out {
        reply: Reply::Accepted {
            uid,
            account: account.to_owned(),
        },
        after: Duration::ZERO,
    }
}

fn failed(retry_after_ms: u32) -> Out {
    Out {
        reply: Reply::Failed {
            retry_after_ms,
            text: FAILED.to_owned(),
        },
        after: Duration::from_secs(2),
    }
}

fn unavailable(text: &str) -> Out {
    Out {
        reply: Reply::Unavailable(text.to_owned()),
        after: Duration::ZERO,
    }
}

#[test]
fn the_right_password_is_accepted_and_a_wrong_one_fails_after_the_delay() {
    let mut m = Machine::new("right-wrong");
    m.set("ferrix", 1000, "correct horse");
    let outs = m.converse(FERRIX, "hyprlock", "", &["correct horse"]);
    assert_eq!(
        outs.first().map(|out| &out.reply),
        Some(&Reply::Prompt {
            visible: false,
            text: "Password: ".to_owned()
        })
    );
    assert_eq!(outs.last(), Some(&accepted("ferrix", 1000)));
    assert_eq!(
        m.verdict(FERRIX, "hyprlock", "", &["correct hors"]),
        failed(0)
    );
    let log = m.audit();
    assert!(log.contains("service=hyprlock account=ferrix peer-pid=2 peer-uid=1000 method=password result=accepted"), "{log}");
    assert!(log.contains("result=failed why=failures:1"), "{log}");
    assert!(
        !log.contains("correct"),
        "the audit log holds no password: {log}"
    );
}

#[test]
fn no_credential_is_unavailable_never_an_empty_password() {
    let mut m = Machine::new("none");
    assert_eq!(
        m.verdict(FERRIX, "hyprlock", "", &[]),
        unavailable("no password is set for ferrix")
    );
    assert_eq!(
        m.one(FERRIX, &Record::Status { account: "" }),
        Reply::State {
            credential: false,
            methods: 0,
            throttled_ms: 0
        }
    );
    m.set("ferrix", 1000, "x");
    assert_eq!(
        m.one(FERRIX, &Record::Status { account: "" }),
        Reply::State {
            credential: true,
            methods: method::PASSWORD,
            throttled_ms: 0
        }
    );
}

#[test]
fn only_root_names_another_account_and_never_skips_the_password() {
    let mut m = Machine::new("naming");
    m.set("ferrix", 1000, "correct horse");
    assert_eq!(
        m.verdict(OTHER, "gate", "ferrix", &[]),
        unavailable("only root may name another account")
    );
    assert_eq!(
        m.verdict(OTHER, "hyprlock", "ferrix", &[]),
        unavailable("this service checks only the caller's own account")
    );
    assert_eq!(
        m.one(OTHER, &Record::Status { account: "ferrix" }),
        Reply::Unavailable("only root may name another account".to_owned())
    );
    // Root names ferrix, and is still asked for ferrix's password.
    assert_eq!(m.verdict(ROOT, "gate", "ferrix", &["wrong"]), failed(0));
    assert_eq!(
        m.verdict(ROOT, "gate", "ferrix", &["correct horse"]),
        accepted("ferrix", 1000)
    );
    // login is root's alone.
    assert_eq!(
        m.verdict(FERRIX, "login", "", &[]),
        unavailable("only root may use the login service")
    );
    assert_eq!(
        m.verdict(FERRIX, "nosuch", "", &[]),
        unavailable("there is no nosuch service")
    );
}

#[test]
fn an_unknown_account_answers_as_a_real_one_does_over_six_failures() {
    let mut m = Machine::new("unknown");
    m.set("ferrix", 1000, "correct horse");
    let start = m.now_ms;
    let real: Vec<Vec<Out>> = (0..6)
        .map(|_| m.converse(ROOT, "gate", "ferrix", &["guess"]))
        .collect();
    m.now_ms = start;
    let unknown: Vec<Vec<Out>> = (0..6)
        .map(|_| m.converse(ROOT, "gate", "nobody", &["guess"]))
        .collect();
    assert_eq!(
        real, unknown,
        "the same prompts, texts, delays and throttles, answer by answer"
    );
    assert!(
        matches!(real.last().and_then(|outs| outs.last()), Some(Out { reply: Reply::Failed { text, .. }, .. }) if text.starts_with("wait ")),
        "six in a row reach the throttle: {real:?}"
    );
    assert!(
        !m.root.join("var/lib/ferrix/auth/state/nobody").exists(),
        "an unknown account leaves no file behind"
    );
}

#[test]
fn the_fourth_failure_throttles_and_the_throttle_is_kept_in_the_store() {
    let mut m = Machine::new("throttle");
    m.set("ferrix", 1000, "correct horse");
    for (attempt, wait_ms) in [(1, 0), (2, 0), (3, 0), (4, 2000)] {
        assert_eq!(
            m.verdict(ROOT, "gate", "ferrix", &["wrong"]),
            failed(wait_ms),
            "attempt {attempt}"
        );
    }
    // The fourth FAILED went out two seconds after its guess, and the
    // throttle runs two more from then. Inside them, an attempt is refused
    // unlooked-at, with the right password too, and with no prompt.
    m.now_ms += 500;
    let outs = m.converse(ROOT, "gate", "ferrix", &[]);
    assert_eq!(
        outs,
        vec![Out {
            reply: Reply::Failed {
                retry_after_ms: 1500,
                text: "wait 2 s".to_owned()
            },
            after: Duration::ZERO
        }]
    );
    // A fresh engine over the same store keeps the throttle.
    let paths = Paths::under(&m.root);
    m.engine = Engine::new(paths, Hasher::fixed(FLOOR), [9; 32]);
    assert!(matches!(
        m.one(ROOT, &Record::Status { account: "ferrix" }),
        Reply::State {
            throttled_ms: 1500,
            ..
        }
    ));
    // Once it has passed, the right password opens and resets the count.
    m.now_ms += 1500;
    assert_eq!(
        m.verdict(ROOT, "gate", "ferrix", &["correct horse"]),
        accepted("ferrix", 1000)
    );
    assert_eq!(m.verdict(ROOT, "gate", "ferrix", &["wrong"]), failed(0));
    // Only root resets a throttle.
    assert!(matches!(
        m.one(FERRIX, &Record::Reset { account: "ferrix" }),
        Reply::Unavailable(_)
    ));
    assert!(matches!(
        m.one(ROOT, &Record::Reset { account: "ferrix" }),
        Reply::State {
            throttled_ms: 0,
            ..
        }
    ));
}

#[test]
fn passwd_asks_for_the_current_password_unless_root_asks() {
    let mut m = Machine::new("passwd");
    m.set("ferrix", 1000, "old one");
    let outs = m.converse(FERRIX, "passwd", "", &["old one", "new one", "new one"]);
    let prompts: Vec<&Reply> = outs.iter().map(|out| &out.reply).collect();
    assert_eq!(prompts.len(), 4, "{prompts:?}");
    assert_eq!(outs.last(), Some(&accepted("ferrix", 1000)));
    assert_eq!(
        m.verdict(FERRIX, "hyprlock", "", &["new one"]),
        accepted("ferrix", 1000)
    );
    assert_eq!(m.verdict(FERRIX, "hyprlock", "", &["old one"]), failed(0));
    // A wrong current password changes nothing.
    assert_eq!(m.verdict(FERRIX, "passwd", "", &["nope"]), failed(0));
    // Mismatched and empty new passwords are refused.
    assert!(matches!(
        m.verdict(FERRIX, "passwd", "", &["new one", "a", "b"]).reply,
        Reply::Failed { ref text, .. } if text == "the passwords did not match"
    ));
    assert!(matches!(
        m.verdict(FERRIX, "passwd", "", &["new one", ""]).reply,
        Reply::Failed { ref text, .. } if text == "a password may not be empty"
    ));
    // Root sets another account's first password with no current one.
    let outs = m.converse(ROOT, "passwd", "other", &["theirs", "theirs"]);
    assert_eq!(outs.len(), 3);
    assert_eq!(outs.last(), Some(&accepted("other", 1001)));
    // Nobody else may.
    assert_eq!(
        m.verdict(FERRIX, "passwd", "other", &[]),
        unavailable("only root may name another account")
    );
    assert!(m.audit().contains(
        "service=passwd account=other peer-pid=1 peer-uid=0 method=password result=changed"
    ));
}

#[test]
fn a_seed_is_imported_once_and_a_changed_password_survives_it() {
    let mut m = Machine::new("seed");
    let mut hasher = Hasher::fixed(FLOOR);
    let secret = ferrix_auth_proto::Secret::from_bytes(b"seeded").unwrap();
    let hash = hasher.make(&secret, &mut |_| {}).unwrap();
    write(
        &m.root.join("lib/ferrix/auth/seed/ferrix"),
        &format!("{hash}\n"),
    );
    write(
        &m.root.join("lib/ferrix/auth/seed/ghost"),
        &format!("{hash}\n"),
    );
    m.engine.import_seeds(m.now_ms);
    assert_eq!(
        m.verdict(FERRIX, "hyprlock", "", &["seeded"]),
        accepted("ferrix", 1000)
    );
    assert!(
        !m.root.join("var/lib/ferrix/auth/users/ghost").exists(),
        "no account, no import"
    );
    assert_eq!(
        m.verdict(FERRIX, "passwd", "", &["seeded", "mine", "mine"]),
        accepted("ferrix", 1000)
    );
    m.engine.import_seeds(m.now_ms);
    assert_eq!(
        m.verdict(FERRIX, "hyprlock", "", &["mine"]),
        accepted("ferrix", 1000)
    );
}

#[test]
fn a_sha512_crypt_seed_is_rehashed_as_argon2id_at_its_first_success() {
    let mut m = Machine::new("rehash");
    write(
        &m.root.join("lib/ferrix/auth/seed/ferrix"),
        "$6$saltstring$svn8UoSVapNtMuq1ukKS4tPQd8iKwSMHWjl/O817G3uBnIFNjnQJuesI68u4OTLiBFdcbYEdFCoEOfaS35inz1\n",
    );
    m.engine.import_seeds(m.now_ms);
    assert_eq!(
        m.verdict(FERRIX, "hyprlock", "", &["Hello world!"]),
        accepted("ferrix", 1000)
    );
    let record = std::fs::read_to_string(m.root.join("var/lib/ferrix/auth/users/ferrix")).unwrap();
    assert!(record.contains("password $argon2id$v=19$"), "{record}");
    assert_eq!(
        m.verdict(FERRIX, "hyprlock", "", &["Hello world!"]),
        accepted("ferrix", 1000)
    );
}

/// The lock `sessiond` armed for ferrix's session, as [`Armed`].
const LOCK: Armed = Armed {
    uid: 1000,
    epoch: 5,
};

/// A machine where ferrix's password is `mine` and ferrix's lock 5 is up.
fn armed(name: &str) -> Machine {
    let mut m = Machine::new(name);
    m.set("ferrix", 1000, "mine");
    m.engine.arm(LOCK.uid, LOCK.epoch);
    m
}

#[test]
fn hyprlocks_acceptance_grants_the_armed_lock_once() {
    let mut m = armed("grant");
    assert_eq!(
        m.verdict(FERRIX, "hyprlock", "", &["mine"]),
        accepted("ferrix", 1000)
    );
    assert_eq!(m.engine.take_grants(), [LOCK]);
    assert!(m.audit().contains("result=granted"));
    assert!(m.audit().contains("seat-epoch=5"));
    // Used up: a second acceptance grants nothing until the next lock.
    assert_eq!(
        m.verdict(FERRIX, "hyprlock", "", &["mine"]),
        accepted("ferrix", 1000)
    );
    assert_eq!(m.engine.take_grants(), []);
}

#[test]
fn a_wrong_password_grants_nothing() {
    let mut m = armed("grant-wrong");
    let _ = m.verdict(FERRIX, "hyprlock", "", &["yours"]);
    assert_eq!(m.engine.take_grants(), []);
}

#[test]
fn only_the_armed_uid_is_granted() {
    let mut m = Machine::new("grant-uid");
    m.set("other", 1001, "theirs");
    m.engine.arm(LOCK.uid, LOCK.epoch);
    assert_eq!(
        m.verdict(OTHER, "hyprlock", "", &["theirs"]),
        accepted("other", 1001)
    );
    assert_eq!(
        m.engine.take_grants(),
        [],
        "another user's password opened ferrix's lock"
    );
}

#[test]
fn nothing_armed_means_nothing_granted() {
    let mut m = Machine::new("grant-unarmed");
    m.set("ferrix", 1000, "mine");
    // Accepted before any lock was armed: no grant is kept for a later one.
    let _ = m.verdict(FERRIX, "hyprlock", "", &["mine"]);
    m.engine.arm(LOCK.uid, LOCK.epoch);
    assert_eq!(m.engine.take_grants(), []);
}

#[test]
fn a_service_without_grant_seat_grants_nothing() {
    let mut m = armed("grant-service");
    assert_eq!(
        m.verdict(FERRIX, "gate", "", &["mine"]),
        accepted("ferrix", 1000)
    );
    assert_eq!(m.engine.take_grants(), []);
}

#[test]
fn a_new_arm_replaces_the_old_and_disarm_names_its_own() {
    let mut m = armed("grant-rearm");
    m.engine.arm(1000, 6);
    // A disarm for the lock before does not touch this one.
    m.engine.disarm(5);
    let _ = m.verdict(FERRIX, "hyprlock", "", &["mine"]);
    assert_eq!(
        m.engine.take_grants(),
        [Armed {
            uid: 1000,
            epoch: 6
        }]
    );
    m.engine.arm(1000, 7);
    m.engine.disarm(7);
    let _ = m.verdict(FERRIX, "hyprlock", "", &["mine"]);
    assert_eq!(m.engine.take_grants(), []);
}

#[test]
fn the_seat_going_drops_what_was_armed_and_granted() {
    let mut m = armed("grant-gone");
    m.engine.seat_gone();
    let _ = m.verdict(FERRIX, "hyprlock", "", &["mine"]);
    assert_eq!(m.engine.take_grants(), []);
}

/// ARM is the seat channel's alone: from the socket it is a record out of
/// turn, and arms nothing.
#[test]
fn arm_from_the_socket_arms_nothing() {
    let mut m = Machine::new("grant-socket");
    m.set("ferrix", 1000, "mine");
    for record in [
        Record::Arm {
            uid: 1000,
            epoch: 1,
        },
        Record::Disarm { epoch: 1 },
        Record::Grant {
            uid: 1000,
            epoch: 1,
        },
    ] {
        assert!(matches!(m.one(ROOT, &record), Reply::Unavailable(_)));
    }
    let _ = m.verdict(FERRIX, "hyprlock", "", &["mine"]);
    assert_eq!(m.engine.take_grants(), []);
}

#[test]
fn unlock_seat_is_roots_audited_grant() {
    let mut m = Machine::new("unlock-seat");
    assert_eq!(
        m.one(FERRIX, &Record::UnlockSeat),
        Reply::Unavailable("only root may let the seat's lock go".to_owned())
    );
    assert!(
        matches!(m.one(ROOT, &Record::UnlockSeat), Reply::Unavailable(ref t) if t.contains("no lock"))
    );
    m.engine.arm(LOCK.uid, LOCK.epoch);
    assert_eq!(
        m.one(FERRIX, &Record::UnlockSeat),
        Reply::Unavailable("only root may let the seat's lock go".to_owned())
    );
    assert_eq!(m.engine.take_grants(), [], "a user's unlock-seat granted");
    assert_eq!(m.one(ROOT, &Record::UnlockSeat), accepted("root", 0).reply);
    assert_eq!(m.engine.take_grants(), [LOCK]);
    let audit = m.audit();
    assert!(audit.contains("service=unlock-seat"), "{audit}");
    assert!(audit.contains("peer-uid=0"), "{audit}");
    assert!(audit.contains("seat-epoch=5"), "{audit}");
}

#[test]
fn records_out_of_turn_end_the_conversation() {
    let mut m = Machine::new("turns");
    let mut conversation = Conversation::new(FERRIX);
    let outs = m.engine.handle(
        &mut conversation,
        &Record::Respond(Response(b"x")),
        m.now_ms,
    );
    assert!(matches!(
        outs.as_slice(),
        [Out {
            reply: Reply::Unavailable(_),
            ..
        }]
    ));
    let after = m
        .engine
        .handle(&mut conversation, &Record::Status { account: "" }, m.now_ms);
    assert!(after.is_empty(), "nothing after the end");
}

/// A machine whose engine takes every caller to be at the console, or none.
fn at_console(name: &str, console: bool) -> Machine {
    let mut m = Machine::new(name);
    let engine = std::mem::replace(
        &mut m.engine,
        Engine::new(Paths::under(&m.root), Hasher::fixed(FLOOR), [9; 32]),
    );
    m.engine = engine.with_local(if console {
        |_| Some(crate::local::CONSOLE)
    } else {
        |_| Some(0)
    });
    m
}

/// `login`'s caller: getty's, root, on the console.
const GETTY: Peer = Peer { pid: 7, uid: 0 };

#[test]
fn login_on_the_console_offers_a_first_password_and_lets_them_in() {
    let mut m = at_console("first-password", true);
    let outs = m.converse(GETTY, "login", "ferrix", &["chosen", "chosen"]);
    assert!(
        outs.iter()
            .any(|out| out.reply
                == Reply::Info("ferrix has no password. Choose one now:".to_owned())),
        "{outs:?}"
    );
    assert_eq!(outs.last().cloned(), Some(accepted("ferrix", 1000)));
    assert!(m.audit().contains("result=first-password"));
    // The password is the account's now: it is asked for, and taken.
    assert_eq!(
        m.verdict(GETTY, "login", "ferrix", &["chosen"]),
        accepted("ferrix", 1000)
    );
    assert!(matches!(
        m.verdict(GETTY, "login", "ferrix", &["guess"]).reply,
        Reply::Failed { .. }
    ));
}

#[test]
fn a_first_password_is_typed_twice_and_never_empty() {
    let mut m = at_console("first-password-twice", true);
    assert!(matches!(
        m.verdict(GETTY, "login", "ferrix", &["one", "two"]).reply,
        Reply::Failed { ref text, .. } if text.contains("did not match")
    ));
    assert!(matches!(
        m.verdict(GETTY, "login", "ferrix", &[""]).reply,
        Reply::Failed { ref text, .. } if text.contains("empty")
    ));
    // Neither was kept: the offer stands.
    assert_eq!(
        m.verdict(GETTY, "login", "ferrix", &["chosen", "chosen"]),
        accepted("ferrix", 1000)
    );
}

#[test]
fn off_the_console_no_password_is_still_no_password() {
    let mut m = at_console("first-password-pty", false);
    assert_eq!(
        m.verdict(GETTY, "login", "ferrix", &[]).reply,
        Reply::Unavailable("no password is set for ferrix".to_owned())
    );
    assert!(m.audit().contains("why=no-credential,tty_nr=0"));
}

/// F1: root, `auth`, and a person's account whose shell refuses logins are
/// never offered a first password, even at the console.
#[test]
fn root_and_the_systems_accounts_get_no_first_password() {
    let mut m = at_console("first-password-who", true);
    std::fs::write(
        m.root.join("etc/passwd"),
        "root:x:0:0:root:/:/bin/sh\nferrix:x:1000:1000:ferrix:/home/ferrix:/bin/sh\n\
         auth:x:90:90::/:/sbin/nologin\nshut:x:1002:1002::/:/sbin/nologin\n",
    )
    .unwrap();
    for account in ["root", "auth", "shut"] {
        assert_eq!(
            m.verdict(GETTY, "login", account, &[]).reply,
            Reply::Unavailable(format!("no password is set for {account}")),
            "{account}"
        );
    }
    assert!(m.audit().contains("not-a-persons-account"));
    assert_eq!(
        m.verdict(GETTY, "login", "ferrix", &["chosen", "chosen"]),
        accepted("ferrix", 1000)
    );
}

#[test]
fn only_a_first_password_service_and_only_root_get_the_offer() {
    let mut m = at_console("first-password-service", true);
    // `gate` is `Account=any` with no `FirstPassword=`.
    assert_eq!(
        m.verdict(GETTY, "gate", "ferrix", &[]).reply,
        Reply::Unavailable("no password is set for ferrix".to_owned())
    );
    // A user at the console cannot use `login` at all (`Callers=root`), and
    // hyprlock, whose caller is the user, is never offered one.
    assert!(matches!(
        m.verdict(FERRIX, "login", "ferrix", &[]).reply,
        Reply::Unavailable(_)
    ));
    assert_eq!(
        m.verdict(FERRIX, "hyprlock", "", &[]).reply,
        Reply::Unavailable("no password is set for ferrix".to_owned())
    );
}

/// `su`'s conversation (decision 5): the caller's own password, for a member
/// of wheel, asked of the person -- `su` connects with their uid.
#[test]
fn su_takes_a_wheel_members_own_password() {
    let mut m = Machine::new("su");
    m.set("ferrix", 1000, "mine");
    m.set("other", 1001, "theirs");
    assert_eq!(
        m.verdict(FERRIX, "su", "", &["mine"]),
        accepted("ferrix", 1000)
    );
    assert!(matches!(
        m.verdict(FERRIX, "su", "", &["guess"]).reply,
        Reply::Failed { .. }
    ));
    // Not in wheel: refused before any prompt.
    let outs = m.converse(OTHER, "su", "", &[]);
    assert_eq!(
        outs.last().map(|out| out.reply.clone()),
        Some(Reply::Unavailable("other is not in wheel".to_owned()))
    );
    assert!(
        !outs
            .iter()
            .any(|out| matches!(out.reply, Reply::Prompt { .. })),
        "a non-member was asked for a password"
    );
    assert!(m.audit().contains("why=not-in-group"));
    // Only the caller's own account: naming another is refused.
    assert!(matches!(
        m.verdict(OTHER, "su", "ferrix", &[]).reply,
        Reply::Unavailable(_)
    ));
}
