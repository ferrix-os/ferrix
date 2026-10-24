//! A conversation over a real socket pair, against a scripted authority and
//! a scripted init (`docs/SETTINGS-WRITER.md` §9.1; the certification
//! consultant's ledger line 3482, conditions 2 and 5): only ACCEPTED for
//! exactly the peer writes, a refusal of any kind writes nothing, a dry run
//! asks nobody and writes nothing, an invalid request is refused before
//! anyone is asked, and the password reaches no audit line or answer.

use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ferrix_auth_client::{Person, Verdict, send_packet};
use ferrix_settings_proto::{MAX_RECORD, RefusalKind, Request, ToClient, ToServer};

use compositor_seat::prompt::{FromSeat, Secret as SeatSecret, ToSeat};

use crate::authority::Authority;
use crate::conversation::{self, Context, Peer};
use crate::identity::{Identify, Program};
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
    // The allow-lists, naming settingsctl, a root-owned -- here the test's
    // own uid's -- file nobody else may write (S3).
    for dir in ["", "etc", "bin", "lib", "lib/ferrix", "lib/ferrix/settings"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
        std::fs::set_permissions(root.join(dir), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    for program in ["bin/settingsctl", "bin/zinc"] {
        std::fs::write(root.join(program), "").unwrap();
        std::fs::set_permissions(root.join(program), std::fs::Permissions::from_mode(0o755))
            .unwrap();
    }
    std::fs::write(
        root.join(crate::apps::FILE),
        "hostname = /bin/settingsctl\nservices = /bin/settingsctl\n",
    )
    .unwrap();
    root
}

/// The test's uid: what "root-owned" means under a test's root.
fn me() -> u32 {
    // SAFETY: getuid cannot fail.
    unsafe { libc::getuid() }
}

/// Who asked: `/bin/settingsctl` started at tick 7, the same each time it
/// is looked up -- or, scripted, a list of answers in turn.
#[derive(Default)]
struct Asker(Mutex<Vec<Option<Program>>>);

fn program(exe: &str, start: u64) -> Option<Program> {
    Some(Program {
        exe: exe.to_owned(),
        start,
        parent: "/bin/zinc".to_owned(),
    })
}

impl Identify for Asker {
    fn of(&self, _pid: i32) -> Option<Program> {
        let mut turns = self.0.lock().unwrap();
        if turns.len() > 1 {
            return turns.remove(0);
        }
        turns
            .first()
            .cloned()
            .unwrap_or_else(|| program("/bin/settingsctl", 7))
    }
}

/// What the stand-in for the seat does.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SeatDoes {
    /// Types this test's password and allows.
    Types,
    /// Allows a prompt with no field.
    Allows,
    /// Cancels.
    Cancels,
    /// Says `busy` to the hello.
    Busy,
}

/// The seat: `sessiond` and the compositor in one, on the prompt socket
/// under the test's root; and every record it heard, by its first word.
struct Seat {
    heard: Arc<Mutex<Vec<String>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

fn seat(root: &Path, does: SeatDoes) -> Seat {
    std::fs::create_dir_all(root.join("lib/ferrix/units")).unwrap();
    std::fs::write(
        root.join("lib/ferrix/units/hyprix.service"),
        "[Service]\nExecStart=/bin/sessiond --user ferrix -- /bin/hyprix\n",
    )
    .unwrap();
    let listener = crate::seat::listen(&root.join("prompt"));
    let heard = Arc::new(Mutex::new(Vec::new()));
    let kept = Arc::clone(&heard);
    let thread = std::thread::spawn(move || {
        let connection = crate::seat::accept(&listener);
        let fd = connection.as_raw_fd();
        let mut buffer = vec![0_u8; 4096];
        let send = |record: &FromSeat| send_packet(fd, &record.encode().unwrap()).unwrap();
        while let Some(packet) = ferrix_auth_client_receive(fd, &mut buffer) {
            let record = ToSeat::decode(packet).unwrap();
            let word = format!("{record:?}");
            kept.lock()
                .unwrap()
                .push(word.split([' ', '{']).next().unwrap().to_owned());
            match (record, does) {
                (ToSeat::Hello { .. }, SeatDoes::Busy) => {
                    send(&FromSeat::Unable("busy".to_owned()));
                }
                (ToSeat::Hello { uid: 1000 }, _) => send(&FromSeat::Ready),
                (ToSeat::Hello { .. }, _) => send(&FromSeat::Unable("not-here".to_owned())),
                (ToSeat::Question { id, .. }, SeatDoes::Types) => send(&FromSeat::Answer {
                    id,
                    secret: SeatSecret::new(PASSWORD.as_bytes()).unwrap(),
                }),
                (ToSeat::Confirm { id }, SeatDoes::Allows | SeatDoes::Types) => {
                    send(&FromSeat::Allow { id });
                }
                (ToSeat::Question { id, .. } | ToSeat::Confirm { id }, _) => {
                    send(&FromSeat::Deny {
                        id,
                        why: "cancelled".to_owned(),
                    });
                }
                (ToSeat::Close { .. }, _) => return,
                _ => {}
            }
        }
    });
    Seat {
        heard,
        thread: Some(thread),
    }
}

impl Seat {
    fn heard(mut self) -> Vec<String> {
        // The seat's thread ends at the close, or when settingsd hangs up.
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
        self.heard.lock().unwrap().clone()
    }
}

/// The context a test's settingsd runs with.
fn context(root: &Path, script: &Arc<Script>, switches: Switches, asker: Asker) -> Context {
    Context {
        system: System {
            root: root.to_path_buf(),
            live: false,
            init: Box::new(switches),
        },
        authority: Box::new(Shared(Arc::clone(script))),
        passwd: root.join("etc/passwd"),
        writing: Mutex::new(()),
        identify: Box::new(asker),
        prompt: root.join("prompt"),
        trusted_uid: me(),
        listener_uid: me(),
        cancelled: Mutex::new(std::collections::HashMap::new()),
        prompts: std::sync::atomic::AtomicU64::new(1),
    }
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
    let context = context(&root, &script, switches, Asker::default());
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

/// One request through the seat: the seat doing `does`, `authd` scripted
/// by `script`, `asker` saying who asked. The answer, what the seat heard
/// and what `authd` was asked.
fn on_seat(
    name: &str,
    script: Script,
    does: Option<SeatDoes>,
    asker: Asker,
) -> (ToClient, Vec<String>, usize, String) {
    let root = root(name);
    let seat = does.map(|does| seat(&root, does));
    if does.is_none() {
        // A seat account whose desktop is not running: the unit, no socket.
        std::fs::create_dir_all(root.join("lib/ferrix/units")).unwrap();
        std::fs::write(
            root.join("lib/ferrix/units/hyprix.service"),
            "ExecStart=/bin/sessiond --user ferrix -- /bin/hyprix\n",
        )
        .unwrap();
    }
    let script = Arc::new(script);
    let context = context(&root, &script, Switches::default(), asker);
    let (ours, theirs) = pair();
    let other = std::thread::spawn(move || client(theirs, hostname("box"), false, Client::Answers));
    let answer = conversation::serve(ours.as_raw_fd(), FERRIX, &context);
    drop(ours);
    other.join().unwrap();
    let heard = seat.map(Seat::heard).unwrap_or_default();
    let asked = script.asked.lock().unwrap().len();
    let name_now = std::fs::read_to_string(root.join("etc/hostname")).unwrap();
    let _ = std::fs::remove_dir_all(&root);
    (answer, heard, asked, name_now)
}

/// S4, S5: the seat's account is asked in the prompt, and the password the
/// person typed there is what reaches authd; the client hears only how it
/// ended.
#[test]
fn a_seat_account_is_asked_on_the_seat() {
    let (answer, heard, asked, name) = on_seat(
        "seat",
        accepts(1000, "ferrix"),
        Some(SeatDoes::Types),
        Asker::default(),
    );
    assert_eq!(
        answer,
        ToClient::Done {
            changed: true,
            text: "the computer is called box (was ferrix)".to_owned()
        }
    );
    assert_eq!(heard, ["Hello", "Ask", "Question", "Close"]);
    assert_eq!(asked, 1);
    assert_eq!(name, "box\n");
    let said = crate::audit::SAID.lock().unwrap().clone();
    assert!(
        said.iter().any(|line| line
            .contains("program=/bin/settingsctl account=ferrix auth=seat+password result=done")),
        "{said:?}"
    );
}

/// `Methods=none` on the seat: authd asks nothing, and the person still
/// says yes before anything is made; a Cancel makes nothing and pauses that
/// program's asking (S6).
#[test]
fn an_acceptance_with_no_question_waits_for_allow() {
    let none = || Script {
        verdict: Some(Ok(Verdict::Accepted {
            uid: 1000,
            account: "ferrix".to_owned(),
        })),
        prompt: false,
        ..Script::default()
    };
    let (answer, heard, _, name) = on_seat(
        "seat-allow",
        none(),
        Some(SeatDoes::Allows),
        Asker::default(),
    );
    assert!(matches!(answer, ToClient::Done { .. }), "{answer:?}");
    assert_eq!(heard, ["Hello", "Ask", "Confirm", "Close"]);
    assert_eq!(name, "box\n");
    let (answer, heard, _, name) = on_seat(
        "seat-cancel",
        none(),
        Some(SeatDoes::Cancels),
        Asker::default(),
    );
    match answer {
        ToClient::Refused(refusal) => assert_eq!(refusal.kind, RefusalKind::Cancelled),
        other => panic!("{other:?}"),
    }
    assert_eq!(heard, ["Hello", "Ask", "Confirm", "Close"]);
    assert_eq!(name, "ferrix\n");
}

/// T10's host half: a denial of the password field makes nothing.
#[test]
fn a_denied_field_makes_nothing() {
    let (answer, _, _, name) = on_seat(
        "seat-deny",
        accepts(1000, "ferrix"),
        Some(SeatDoes::Cancels),
        Asker::default(),
    );
    assert!(matches!(answer, ToClient::Refused(_)), "{answer:?}");
    assert_eq!(name, "ferrix\n");
}

/// T5's host half: a seat account with no desktop, or a busy seat, is
/// refused before authd is asked -- never relayed to the client.
#[test]
fn a_seat_account_with_no_seat_to_ask_is_refused() {
    let (answer, _, asked, name) =
        on_seat("seat-none", accepts(1000, "ferrix"), None, Asker::default());
    match answer {
        ToClient::Refused(refusal) => {
            assert_eq!(refusal.kind, RefusalKind::Unavailable);
            assert!(
                refusal.text.contains("no desktop is running"),
                "{}",
                refusal.text
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(asked, 0);
    assert_eq!(name, "ferrix\n");
    let (answer, _, asked, _) = on_seat(
        "seat-busy",
        accepts(1000, "ferrix"),
        Some(SeatDoes::Busy),
        Asker::default(),
    );
    assert!(
        matches!(answer, ToClient::Refused(ref r) if r.kind == RefusalKind::Unavailable),
        "{answer:?}"
    );
    assert_eq!(asked, 0);
}

/// S3: a program off the list, or one that is not the program that
/// connected, is refused before anyone is asked.
#[test]
fn only_a_listed_program_bound_to_its_connection_asks() {
    let zinc = Asker(Mutex::new(vec![program("/bin/zinc", 7)]));
    let (answer, heard, asked, name) = on_seat("seat-zinc", accepts(1000, "ferrix"), None, zinc);
    assert!(
        matches!(answer, ToClient::Refused(ref r) if r.kind == RefusalKind::NotPermitted),
        "{answer:?}"
    );
    assert!(heard.is_empty(), "{heard:?}");
    assert_eq!(asked, 0);
    assert_eq!(name, "ferrix\n");
    let changed = Asker(Mutex::new(vec![
        program("/bin/settingsctl", 7),
        program("/bin/settingsctl", 8),
    ]));
    let (answer, _, asked, _) = on_seat("seat-changed", accepts(1000, "ferrix"), None, changed);
    match answer {
        ToClient::Refused(refusal) => assert!(
            refusal.text.contains("not the one that connected"),
            "{}",
            refusal.text
        ),
        other => panic!("{other:?}"),
    }
    assert_eq!(asked, 0);
}

/// Ledger 3636 (2): an account with no seat is relayed to its client, but
/// an acceptance nobody typed anything for is refused and makes nothing.
#[test]
fn a_relayed_acceptance_with_no_question_is_refused() {
    let run = run(
        "relay-none",
        FERRIX,
        hostname("box"),
        false,
        Script {
            verdict: Some(Ok(Verdict::Accepted {
                uid: 1000,
                account: "ferrix".to_owned(),
            })),
            prompt: false,
            ..Script::default()
        },
        Client::Answers,
    );
    assert_eq!(run.refused(), RefusalKind::NotPermitted);
    assert_eq!(run.hostname(), "ferrix\n");
    let said = crate::audit::SAID.lock().unwrap().clone();
    assert!(
        said.iter().any(|line| line.contains("result=needs-seat")),
        "{said:?}"
    );
}

/// Ledger 3636 (1), A6: a prompt listener that is not root is refused before
/// a byte goes to it, and a session unit settingsd cannot read refuses
/// every request before anyone is asked -- each end to end through
/// `serve()`.
#[test]
fn a_seat_settingsd_cannot_trust_or_cannot_read_refuses() {
    for (name, wrong_listener) in [("seat-listener", true), ("seat-unreadable", false)] {
        let root = root(name);
        let seat = wrong_listener.then(|| seat(&root, SeatDoes::Types));
        if !wrong_listener {
            std::fs::create_dir_all(root.join("lib/ferrix/units/hyprix.service.d")).unwrap();
        }
        let script = Arc::new(accepts(1000, "ferrix"));
        let mut context = context(&root, &script, Switches::default(), Asker::default());
        if wrong_listener {
            // The listener is the test's uid: say root is someone else.
            context.listener_uid = me() + 1;
        }
        let (ours, theirs) = pair();
        let other =
            std::thread::spawn(move || client(theirs, hostname("box"), false, Client::Answers));
        let answer = conversation::serve(ours.as_raw_fd(), FERRIX, &context);
        drop(ours);
        other.join().unwrap();
        let heard = seat.map(Seat::heard).unwrap_or_default();
        match &answer {
            ToClient::Refused(refusal) => {
                assert!(
                    matches!(
                        refusal.kind,
                        RefusalKind::Unavailable | RefusalKind::NotPermitted
                    ),
                    "{name}: {refusal:?}"
                );
            }
            other => panic!("{name}: {other:?}"),
        }
        assert!(heard.is_empty(), "{name}: the listener heard {heard:?}");
        assert_eq!(script.asked.lock().unwrap().len(), 0, "{name}");
        assert_eq!(
            std::fs::read_to_string(root.join("etc/hostname")).unwrap(),
            "ferrix\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// Ledger 3636 (16): a request this settingsd does not know is answered
/// `unsupported` with words, never silently.
#[test]
fn an_unknown_request_is_unsupported_and_said_so() {
    let root = root("unknown-request");
    let script = Arc::new(Script::default());
    let context = context(&root, &script, Switches::default(), Asker::default());
    let (ours, theirs) = pair();
    // A REQUEST record naming a request this build has not got.
    let mut packet = vec![1_u8, ferrix_settings_proto::VERSION, 0];
    let name = b"set-moon-phase";
    packet.extend_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
    packet.extend_from_slice(name);
    send_packet(theirs.as_raw_fd(), &packet).unwrap();
    let answer = conversation::serve(ours.as_raw_fd(), FERRIX, &context);
    match answer {
        ToClient::Refused(refusal) => {
            assert_eq!(refusal.kind, RefusalKind::Unsupported);
            assert!(
                refusal.text.starts_with("this settingsd knows only"),
                "{}",
                refusal.text
            );
        }
        other => panic!("{other:?}"),
    }
    let _ = std::fs::remove_dir_all(&root);
}
