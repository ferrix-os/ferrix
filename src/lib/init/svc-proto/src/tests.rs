//! Every record through its encoder and back, the refusals of bytes that
//! are not a record, and readiness lines in pieces.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::control::{Answer, Call, DecodeError, Framer, MAX_RECORD, UnitStatus};
use crate::notify::{self, Lines, Notice};

fn s(text: &str) -> String {
    String::from(text)
}

fn calls() -> Vec<Call> {
    vec![
        Call::Status(None),
        Call::Status(Some(s("a.service"))),
        Call::List { failed: true },
        Call::Start(s("a.service")),
        Call::Stop(s("a.service")),
        Call::Restart(s("a.service")),
        Call::Reload(s("a.service")),
        Call::Isolate(s("rescue.target")),
        Call::ResetFailed(None),
        Call::Poweroff,
        Call::Reboot,
        Call::Log {
            unit: s("a.service"),
            lines: 40,
        },
        Call::DaemonReload,
        Call::Enable(s("a.service")),
        Call::Disable(s("a.service")),
        Call::Mask(s("a.service")),
        Call::Unmask(s("a.service")),
        Call::SetProperty {
            unit: s("a.service"),
            assignments: vec![s("TasksMax=16"), s("MemoryMax=64M")],
            persistent: true,
        },
        Call::Scope {
            unit: s("session-1.scope"),
            slice: Some(s("user-1000.slice")),
            pids: vec![41, 42],
        },
    ]
}

fn answers() -> Vec<Answer> {
    vec![
        Answer::Done(s("done")),
        Answer::Refused(s("no")),
        Answer::Units(vec![
            UnitStatus::default(),
            UnitStatus {
                name: s("a.service"),
                description: Some(s("The A")),
                load: s("loaded"),
                active: s("active"),
                sub: s("running"),
                main: Some(7),
                result: Some(s("success")),
                status: Some(s("serving")),
                cgroup: Some(s("system.slice/a.service")),
                invocation: Some(s("5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a")),
            },
        ]),
        Answer::Lines(vec![s("one"), s("")]),
        Answer::Note(s("made a link")),
    ]
}

/// A record's body: the length prefix off, and checked.
fn body(record: &[u8]) -> &[u8] {
    let (header, body) = record.split_at(4);
    let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    assert_eq!(len as usize, body.len());
    body
}

#[test]
fn every_call_and_answer_comes_back_as_it_went() {
    for call in calls() {
        assert_eq!(
            Call::decode(body(&call.encode())),
            Ok(call.clone()),
            "{call:?}"
        );
    }
    for answer in answers() {
        assert_eq!(
            Answer::decode(body(&answer.encode())),
            Ok(answer.clone()),
            "{answer:?}"
        );
    }
}

#[test]
fn only_reading_calls_are_open_to_everyone() {
    let open: Vec<Call> = calls().into_iter().filter(|c| !c.changes_state()).collect();
    assert_eq!(
        open,
        [
            Call::Status(None),
            Call::Status(Some(s("a.service"))),
            Call::List { failed: true },
            Call::Log {
                unit: s("a.service"),
                lines: 40
            },
        ]
    );
    assert!(!Answer::Note(s("x")).is_final());
    assert!(Answer::Done(s("done")).is_final());
}

#[test]
fn bytes_that_are_not_a_record_are_refused() {
    assert_eq!(Call::decode(&[]), Err(DecodeError::Short));
    assert_eq!(Call::decode(&[200]), Err(DecodeError::Tag(200)));
    // A string whose length runs past the record.
    assert_eq!(
        Call::decode(&[3, 9, 0, 0, 0, b'a']),
        Err(DecodeError::Short)
    );
    // A count past what is left: refused, not allocated.
    assert_eq!(
        Call::decode(&[18, 0, 0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff]),
        Err(DecodeError::Short)
    );
    assert_eq!(Call::decode(&[3, 1, 0, 0, 0, 0xff]), Err(DecodeError::Utf8));
    assert_eq!(Call::decode(&[9, 0]), Err(DecodeError::Trailing));
    // A flag is 0 or 1, as the writer writes one: CI's fuzzer found
    // `[2, 0xaf]` decoding as `List { failed: true }` and encoding back as
    // `[2, 1]` (run 36303812173).
    assert_eq!(Call::decode(&[2, 0xaf]), Err(DecodeError::Flag(0xaf)));
    assert_eq!(Call::decode(&[1, 2]), Err(DecodeError::Flag(2)));
    assert_eq!(Call::decode(&[2, 1]), Ok(Call::List { failed: true }));
    assert_eq!(Answer::decode(&[9]), Err(DecodeError::Tag(9)));
}

#[test]
fn the_framer_gives_whole_records_whatever_the_pieces() {
    let first = Call::Start(s("a.service")).encode();
    let second = Call::Poweroff.encode();
    let mut stream = first;
    stream.extend_from_slice(&second);
    for split in 0..=stream.len() {
        let mut framer = Framer::new();
        let mut got = Vec::new();
        for piece in [&stream[..split], &stream[split..]] {
            framer.push(piece);
            while let Some(record) = framer.next_record().unwrap() {
                got.push(Call::decode(&record).unwrap());
            }
        }
        assert_eq!(
            got,
            [Call::Start(s("a.service")), Call::Poweroff],
            "split at {split}"
        );
        assert!(!framer.is_partial());
    }
}

#[test]
fn the_framer_refuses_a_length_past_the_limit() {
    let mut framer = Framer::new();
    framer.push(&u32::try_from(MAX_RECORD + 1).unwrap().to_le_bytes());
    assert_eq!(framer.next_record(), Err(DecodeError::TooLong));
    let mut framer = Framer::new();
    framer.push(&[1, 0]);
    assert_eq!(framer.next_record(), Ok(None));
    assert!(framer.is_partial());
}

#[test]
fn readiness_lines_say_what_sd_notify_says() {
    assert_eq!(notify::parse("READY=1"), Notice::Ready);
    assert_eq!(notify::parse("READY=0"), Notice::Other);
    assert_eq!(
        notify::parse("STATUS=up, 3 clients"),
        Notice::Status(s("up, 3 clients"))
    );
    assert_eq!(notify::parse("MAINPID=42"), Notice::MainPid(42));
    assert_eq!(notify::parse("MAINPID=x"), Notice::Other);
    assert_eq!(notify::parse("STOPPING=1"), Notice::Stopping);
    assert_eq!(notify::parse("RELOADING=1"), Notice::Reloading);
    assert_eq!(notify::parse("WATCHDOG=1"), Notice::Other);
    assert_eq!(notify::parse("garbage"), Notice::Other);
}

#[test]
fn lines_arrive_in_pieces_and_an_overlong_one_is_dropped() {
    let mut lines = Lines::new();
    assert!(lines.push(b"STATUS=a").is_empty());
    assert_eq!(
        lines.push(b"b\nREADY=1\nMAIN"),
        [s("STATUS=ab"), s("READY=1")]
    );
    assert_eq!(lines.finish(), Some(s("MAIN")));
    assert_eq!(lines.finish(), None);

    let mut long = vec![b'x'; notify::MAX_LINE + 10];
    long.extend_from_slice(b"\nREADY=1\n");
    assert_eq!(Lines::new().push(&long), [s("READY=1")]);
}
