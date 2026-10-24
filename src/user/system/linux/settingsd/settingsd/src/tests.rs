//! A conversation over a real socket pair, against a scripted authority and
//! a scripted init (`docs/SETTINGS-WRITER.md` §9.1; the certification
//! consultant's ledger line 3482, conditions 2 and 5): only ACCEPTED for
//! exactly the peer writes, a refusal of any kind writes nothing, a dry run
//! asks nobody and writes nothing, an invalid request is refused before
//! anyone is asked, and the password reaches no audit line or answer.

use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ferrix_auth_client::{Person, Verdict, send_packet};
use ferrix_settings_proto::{MAX_RECORD, RefusalKind, Request, ToClient, ToServer};

use crate::authority::Authority;
use crate::conversation::{self, Context, Peer};
use crate::requests::{Init, System};

const PASSWORD: &str = "correct horse settings";
const FERRIX: Peer = Peer { pid: 1, uid: 1000 };
const GUEST: Peer = Peer { pid: 1, uid: 1001 };
const ROOT: Peer = Peer { pid: 1, uid: 0 };

/// What the scripted authority answers, and what it was asked.
#[derive(Default)]
struct Script {
    /// The verdict, or the error reaching `authd` would be.
    verdict: Option<Result<Verdict, String>>,
    /// Whether to put a question to the person first.
    prompt: bool,
    /// Each (service, account) asked about.
    asked: Mutex<Vec<(String, String)>>,
    /// Each answer heard.
    heard: Mutex<Vec<Vec<u8>>>,
}

struct Shared(Arc<Script>);

impl Authority for Shared {
    fn converse(
        &self,
        service: &str,
        account: &str,
        person: &mut dyn Person,
    ) -> Result<Verdict, String> {
        self.0
            .asked
            .lock()
            .unwrap()
            .push((service.to_owned(), account.to_owned()));
        if self.0.prompt {
            match person.ask(false, "Password: ") {
                Some(secret) => self.0.heard.lock().unwrap().push(secret.expose().to_vec()),
                None => return Ok(Verdict::Unavailable("no answer was given".to_owned())),
            }
        }
        self.0
            .verdict
            .clone()
            .unwrap_or_else(|| Ok(Verdict::Unavailable("no script".to_owned())))
    }
}

/// init, recording each switch.
#[derive(Default)]
struct Switches(Arc<Mutex<Vec<(String, bool)>>>);

impl Init for Switches {
    fn switch(&self, unit: &str, enabled: bool) -> Result<Vec<String>, String> {
        self.0.lock().unwrap().push((unit.to_owned(), enabled));
        Ok(vec![format!("linked {unit}")])
    }
}

/// How the scripted client answers a prompt.
#[derive(Clone, Copy)]
enum Client {
    Answers,
    Cancels,
    Leaves,
}

struct Run {
    answer: ToClient,
    script: Arc<Script>,
    switches: Arc<Mutex<Vec<(String, bool)>>>,
    root: PathBuf,
}

impl Drop for Run {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Run {
    fn hostname(&self) -> String {
        std::fs::read_to_string(self.root.join("etc/hostname")).unwrap()
    }

    fn asked(&self) -> usize {
        self.script.asked.lock().unwrap().len()
    }

    fn refused(&self) -> RefusalKind {
        match &self.answer {
            ToClient::Refused(refusal) => refusal.kind,
            other => panic!("not refused: {other:?}"),
        }
    }
}

fn pair() -> (OwnedFd, OwnedFd) {
    let mut fds = [0; 2];
    // SAFETY: `fds` is valid for two descriptors.
    let made = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    };
    assert_eq!(made, 0);
    let [a, b] = fds;
    // SAFETY: `socketpair` just made it, and nothing else owns it.
    let a = unsafe { OwnedFd::from_raw_fd(a) };
    // SAFETY: as above.
    let b = unsafe { OwnedFd::from_raw_fd(b) };
    (a, b)
}

fn send(fd: RawFd, record: &ToServer) {
    send_packet(fd, record.encode().unwrap().bytes()).unwrap();
}

/// The client's side: the request, then each prompt answered as `client`
/// says, until the end.
fn client(fd: OwnedFd, request: Request, dry_run: bool, client: Client) {
    send(fd.as_raw_fd(), &ToServer::Request { dry_run, request });
    let mut buffer = vec![0_u8; MAX_RECORD + 1];
    loop {
        let got = ferrix_auth_client_receive(fd.as_raw_fd(), &mut buffer);
        let Some(packet) = got else { return };
        match ToClient::decode(packet).unwrap() {
            ToClient::Prompt { .. } => match client {
                Client::Answers => send(
                    fd.as_raw_fd(),
                    &ToServer::Respond(
                        ferrix_settings_proto::Secret::from_bytes(PASSWORD.as_bytes()).unwrap(),
                    ),
                ),
                Client::Cancels => send(fd.as_raw_fd(), &ToServer::Cancel),
                Client::Leaves => return,
            },
            ToClient::Info(_) => {}
            ToClient::Done { .. } | ToClient::Refused(_) => return,
        }
    }
}

fn ferrix_auth_client_receive(fd: RawFd, buffer: &mut [u8]) -> Option<&[u8]> {
    // SAFETY: `buffer` is valid for writes of its whole length.
    let got = unsafe { libc::recv(fd, buffer.as_mut_ptr().cast(), buffer.len(), 0) };
    let got = usize::try_from(got).ok().filter(|&got| got > 0)?;
    buffer.get(..got)
}

fn root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("settingsd-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("etc")).unwrap();
    std::fs::write(
        root.join("etc/passwd"),
        "root:x:0:0::/:/bin/sh\nferrix:x:1000:1000::/:/bin/sh\nguest:x:1001:1001::/:/bin/sh\n",
    )
    .unwrap();
    std::fs::write(root.join("etc/hostname"), "ferrix\n").unwrap();
    root
}

fn run(
    name: &str,
    peer: Peer,
    request: Request,
    dry_run: bool,
    script: Script,
    answers: Client,
) -> Run {
    let root = root(name);
    let script = Arc::new(script);
    let switches = Switches::default();
    let recorded = Arc::clone(&switches.0);
    let context = Context {
        system: System {
            root: root.clone(),
            live: false,
            init: Box::new(switches),
        },
        authority: Box::new(Shared(Arc::clone(&script))),
        passwd: root.join("etc/passwd"),
        writing: Mutex::new(()),
    };
    let (ours, theirs) = pair();
    let other = std::thread::spawn(move || client(theirs, request, dry_run, answers));
    let answer = conversation::serve(ours.as_raw_fd(), peer, &context);
    drop(ours);
    other.join().unwrap();
    Run {
        answer,
        script,
        switches: recorded,
        root,
    }
}

fn hostname(name: &str) -> Request {
    Request::SetHostname {
        name: name.to_owned(),
    }
}

fn accepts(uid: u32, account: &str) -> Script {
    Script {
        verdict: Some(Ok(Verdict::Accepted {
            uid,
            account: account.to_owned(),
        })),
        prompt: true,
        ..Script::default()
    }
}

fn root_dir(run: &Run) -> &Path {
    &run.root
}

#[test]
fn an_acceptance_for_the_peer_writes_the_name() {
    let run = run(
        "accepted",
        FERRIX,
        hostname("box"),
        false,
        accepts(1000, "ferrix"),
        Client::Answers,
    );
    assert_eq!(
        run.answer,
        ToClient::Done {
            changed: true,
            text: "the computer is called box (was ferrix)".to_owned()
        }
    );
    assert_eq!(run.hostname(), "box\n");
    assert_eq!(
        *run.script.asked.lock().unwrap(),
        [("settings.hostname".to_owned(), "ferrix".to_owned())],
        "BEGIN names the account of the peer's uid"
    );
    assert_eq!(*run.script.heard.lock().unwrap(), [PASSWORD.as_bytes()]);
    // Nothing is left beside the file.
    let names: Vec<_> = std::fs::read_dir(root_dir(&run).join("etc"))
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name())
        .collect();
    assert_eq!(names.len(), 2, "{names:?}");
}

#[test]
fn every_other_verdict_writes_nothing() {
    let scripts: Vec<(&str, Script, RefusalKind)> = vec![
        (
            "other-uid",
            accepts(1001, "ferrix"),
            RefusalKind::NotPermitted,
        ),
        (
            "other-name",
            accepts(1000, "guest"),
            RefusalKind::NotPermitted,
        ),
        (
            "failed",
            Script {
                verdict: Some(Ok(Verdict::Failed {
                    retry_after_ms: 2000,
                    text: "Authentication failed".to_owned(),
                })),
                prompt: true,
                ..Script::default()
            },
            RefusalKind::AuthenticationFailed,
        ),
        (
            "unavailable",
            Script {
                verdict: Some(Ok(Verdict::Unavailable(
                    "ferrix is not in wheel".to_owned(),
                ))),
                ..Script::default()
            },
            RefusalKind::NotPermitted,
        ),
        (
            "unreachable",
            Script {
                verdict: Some(Err("authd cannot be reached: ENOENT".to_owned())),
                ..Script::default()
            },
            RefusalKind::Unavailable,
        ),
    ];
    for (name, script, kind) in scripts {
        let run = run(
            name,
            FERRIX,
            hostname("box"),
            false,
            script,
            Client::Answers,
        );
        assert_eq!(run.refused(), kind, "{name}");
        assert_eq!(run.hostname(), "ferrix\n", "{name} wrote");
    }
}

#[test]
fn a_failure_carries_its_wait() {
    let run = run(
        "wait",
        FERRIX,
        hostname("box"),
        false,
        Script {
            verdict: Some(Ok(Verdict::Failed {
                retry_after_ms: 16_000,
                text: "wait 16 s".to_owned(),
            })),
            ..Script::default()
        },
        Client::Answers,
    );
    match run.answer {
        ToClient::Refused(ref refusal) => assert_eq!(refusal.retry_after_ms, 16_000),
        ref other => panic!("{other:?}"),
    }
}

#[test]
fn a_client_that_cancels_or_leaves_writes_nothing() {
    for (name, client) in [("cancels", Client::Cancels), ("leaves", Client::Leaves)] {
        let run = run(
            name,
            FERRIX,
            hostname("box"),
            false,
            accepts(1000, "ferrix"),
            client,
        );
        assert_eq!(run.hostname(), "ferrix\n", "{name}");
        assert!(run.script.heard.lock().unwrap().is_empty());
        if let ToClient::Refused(refusal) = &run.answer {
            assert_eq!(refusal.kind, RefusalKind::Cancelled, "{name}");
        }
    }
}

#[test]
fn an_invalid_request_is_refused_before_anyone_is_asked() {
    for bad in ["my pc", "-x", "../../etc/shadow", ""] {
        let run = run(
            "invalid",
            FERRIX,
            hostname(bad),
            false,
            accepts(1000, "ferrix"),
            Client::Answers,
        );
        assert_eq!(run.refused(), RefusalKind::Invalid, "{bad:?}");
        assert_eq!(run.asked(), 0, "{bad:?}");
        assert_eq!(run.hostname(), "ferrix\n");
    }
    let run = run(
        "unit",
        FERRIX,
        Request::ServiceEnablePersistent {
            unit: "auth.socket".to_owned(),
            enabled: true,
        },
        false,
        accepts(1000, "ferrix"),
        Client::Answers,
    );
    assert_eq!(run.refused(), RefusalKind::Invalid);
    assert_eq!(run.asked(), 0);
    assert!(run.switches.lock().unwrap().is_empty());
}

#[test]
fn a_dry_run_asks_nobody_and_writes_nothing() {
    let run = run(
        "dry",
        GUEST,
        hostname("box"),
        true,
        accepts(1001, "guest"),
        Client::Answers,
    );
    assert_eq!(
        run.answer,
        ToClient::Done {
            changed: true,
            text: "would call the computer box; it is called ferrix".to_owned()
        }
    );
    assert_eq!(run.asked(), 0);
    assert_eq!(run.hostname(), "ferrix\n");
    let same = self::run(
        "dry-same",
        GUEST,
        hostname("ferrix"),
        true,
        Script::default(),
        Client::Answers,
    );
    assert!(matches!(same.answer, ToClient::Done { changed: false, .. }));
}

#[test]
fn root_asks_nobody_and_a_uid_with_no_account_is_refused() {
    let run = run(
        "root",
        ROOT,
        hostname("box"),
        false,
        Script::default(),
        Client::Answers,
    );
    assert!(
        matches!(run.answer, ToClient::Done { .. }),
        "{:?}",
        run.answer
    );
    assert_eq!(run.asked(), 0);
    let nobody = self::run(
        "nobody",
        Peer { pid: 1, uid: 4242 },
        hostname("box"),
        false,
        accepts(4242, "x"),
        Client::Answers,
    );
    assert_eq!(nobody.refused(), RefusalKind::NotPermitted);
    assert_eq!(nobody.asked(), 0);
    assert_eq!(nobody.hostname(), "ferrix\n");
}

#[test]
fn a_switch_asks_init_once_it_is_allowed() {
    let request = Request::ServiceEnablePersistent {
        unit: "sshd.service".to_owned(),
        enabled: true,
    };
    let run = run(
        "switch",
        FERRIX,
        request.clone(),
        false,
        accepts(1000, "ferrix"),
        Client::Answers,
    );
    assert_eq!(
        run.answer,
        ToClient::Done {
            changed: true,
            text: "sshd.service is on for every boot".to_owned()
        }
    );
    assert_eq!(
        *run.switches.lock().unwrap(),
        [("sshd.service".to_owned(), true)]
    );
    assert_eq!(
        run.script
            .asked
            .lock()
            .unwrap()
            .first()
            .map(|(s, _)| s.clone()),
        Some("settings.services".to_owned())
    );
    let refused = self::run(
        "switch-refused",
        GUEST,
        request,
        false,
        Script {
            verdict: Some(Ok(Verdict::Unavailable("guest is not in wheel".to_owned()))),
            ..Script::default()
        },
        Client::Answers,
    );
    assert_eq!(refused.refused(), RefusalKind::NotPermitted);
    assert!(refused.switches.lock().unwrap().is_empty());
}

#[test]
fn the_password_reaches_no_audit_line_and_no_answer() {
    let runs = [
        run(
            "secret-ok",
            FERRIX,
            hostname("box"),
            false,
            accepts(1000, "ferrix"),
            Client::Answers,
        ),
        run(
            "secret-failed",
            FERRIX,
            hostname("box"),
            false,
            Script {
                verdict: Some(Ok(Verdict::Failed {
                    retry_after_ms: 0,
                    text: "Authentication failed".to_owned(),
                })),
                prompt: true,
                ..Script::default()
            },
            Client::Answers,
        ),
    ];
    let said = crate::audit::SAID.lock().unwrap().join("\n");
    assert!(said.contains("settings: request=set-hostname"), "{said}");
    assert!(!said.contains(PASSWORD), "{said}");
    for run in &runs {
        assert!(!format!("{:?}", run.answer).contains(PASSWORD));
    }
}
