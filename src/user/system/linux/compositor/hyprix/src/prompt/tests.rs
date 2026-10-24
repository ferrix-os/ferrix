//! The prompt's rules on its state, with a socket pair for `sessiond`'s end
//! (`docs/SETTINGS-WRITER.md` §12.4, §12.9).

use std::os::unix::net::UnixDatagram;
use std::time::Instant;

use compositor_seat::prompt::{FromSeat, ToSeat};

use super::{ARMING, Prompts, shown};

fn pair() -> (Prompts, UnixDatagram) {
    let (ours, theirs) = UnixDatagram::pair().unwrap();
    ours.set_nonblocking(true).unwrap();
    theirs.set_nonblocking(true).unwrap();
    (Prompts::with(Some(ours)), theirs)
}

fn say(sessiond: &UnixDatagram, record: &ToSeat) {
    let _ = sessiond.send(&record.encode().unwrap()).unwrap();
}

fn heard(sessiond: &UnixDatagram) -> Vec<FromSeat> {
    let mut out = Vec::new();
    let mut buffer = [0_u8; 4096];
    while let Ok(got) = sessiond.recv(&mut buffer) {
        out.push(FromSeat::decode(&buffer[..got]).unwrap());
    }
    out
}

fn ask(id: u64) -> ToSeat {
    ToSeat::Ask {
        id,
        area: "hostname".to_owned(),
        program: "/bin/settingsctl".to_owned(),
        parent: "/bin/zinc\u{202e}gnp.exe\u{2028}next".to_owned(),
        account: "ferrix".to_owned(),
        what: "would call the computer box; it is called ferrix".to_owned(),
    }
}

/// Up, with a field, and armed.
fn asking(question: bool) -> (Prompts, UnixDatagram) {
    let (mut prompts, sessiond) = pair();
    say(&sessiond, &ask(1));
    if question {
        say(
            &sessiond,
            &ToSeat::Question {
                id: 1,
                visible: false,
                text: "Password: ".to_owned(),
            },
        );
    } else {
        say(&sessiond, &ToSeat::Confirm { id: 1 });
    }
    assert!(prompts.read(false, true));
    assert!(prompts.asking());
    std::thread::sleep(ARMING);
    (prompts, sessiond)
}

/// What the seat's US layout makes of the letters typed.
fn keysym(code: u16) -> Option<&'static str> {
    compositor_xkb::key(code).and_then(|key| key.plain())
}

fn type_in(prompts: &mut Prompts, codes: &[u16]) {
    for &code in codes {
        let _ = prompts.key(code, true, keysym(code));
        let _ = prompts.key(code, false, keysym(code));
    }
}

#[test]
fn a_password_typed_and_return_is_the_answer() {
    let (mut prompts, sessiond) = asking(true);
    // h, o, r, s, e
    type_in(&mut prompts, &[35, 24, 19, 31, 18]);
    type_in(&mut prompts, &[28]);
    match heard(&sessiond).as_slice() {
        [FromSeat::Answer { id: 1, secret }] => assert_eq!(secret.expose(), b"horse"),
        other => panic!("{other:?}"),
    }
    // Backspace takes one character off; Escape cancels.
    let (mut prompts, sessiond) = asking(true);
    type_in(&mut prompts, &[35, 24, 14, 28]);
    match heard(&sessiond).as_slice() {
        [FromSeat::Answer { secret, .. }] => assert_eq!(secret.expose(), b"h"),
        other => panic!("{other:?}"),
    }
    let (mut prompts, sessiond) = asking(true);
    type_in(&mut prompts, &[35, 1]);
    assert_eq!(
        heard(&sessiond),
        [FromSeat::Deny {
            id: 1,
            why: "cancelled".to_owned()
        }]
    );
    assert!(!prompts.asking());
}

#[test]
fn a_confirm_is_allowed_with_return() {
    let (mut prompts, sessiond) = asking(false);
    // A letter types nothing into a prompt with no field.
    type_in(&mut prompts, &[35, 28]);
    assert_eq!(heard(&sessiond), [FromSeat::Allow { id: 1 }]);
}

/// P5: nothing in the first second, and a key held as the prompt came up
/// answers nothing when it is let go or repeats.
#[test]
fn input_before_it_is_armed_or_held_from_before_answers_nothing() {
    let (mut prompts, sessiond) = pair();
    say(&sessiond, &ask(1));
    say(&sessiond, &ToSeat::Confirm { id: 1 });
    assert!(prompts.read(false, true));
    // Nothing held: only the first second keeps this Return out.
    let shown_at = Instant::now();
    type_in(&mut prompts, &[28]);
    assert!(shown_at.elapsed() < ARMING);
    assert!(heard(&sessiond).is_empty());
    std::thread::sleep(ARMING);
    // Return was held at the opening: its repeat and its release are not
    // a press.
    let (mut prompts, sessiond) = pair();
    say(&sessiond, &ask(2));
    say(&sessiond, &ToSeat::Confirm { id: 2 });
    let _ = prompts.read(false, true);
    prompts.note_held([28]);
    std::thread::sleep(ARMING);
    let _ = prompts.key(28, false, None);
    assert!(heard(&sessiond).is_empty());
    // A fresh press after that is.
    type_in(&mut prompts, &[28]);
    assert_eq!(heard(&sessiond), [FromSeat::Allow { id: 2 }]);
}

/// P7 and P9.
#[test]
fn none_over_a_lock_and_one_at_a_time() {
    let (mut prompts, sessiond) = pair();
    say(&sessiond, &ask(1));
    assert!(!prompts.read(true, true));
    assert!(!prompts.up());
    assert_eq!(
        heard(&sessiond),
        [FromSeat::Deny {
            id: 1,
            why: "locked".to_owned()
        }]
    );
    say(&sessiond, &ask(2));
    assert!(!prompts.read(false, false));
    assert_eq!(
        heard(&sessiond),
        [FromSeat::Deny {
            id: 2,
            why: "no-screen".to_owned()
        }]
    );
    let (mut prompts, sessiond) = asking(true);
    say(&sessiond, &ask(9));
    let _ = prompts.read(false, true);
    assert_eq!(
        heard(&sessiond),
        [FromSeat::Deny {
            id: 9,
            why: "busy".to_owned()
        }]
    );
    // A lock taken while it is up denies it.
    prompts.lock_taken();
    assert_eq!(
        heard(&sessiond),
        [FromSeat::Deny {
            id: 1,
            why: "locked".to_owned()
        }]
    );
    // A record for another prompt changes nothing.
    let (mut prompts, sessiond) = asking(true);
    say(&sessiond, &ToSeat::Confirm { id: 5 });
    assert!(!prompts.read(false, true));
    // Its close takes it down after its last words.
    say(
        &sessiond,
        &ToSeat::Close {
            id: 1,
            text: "done".to_owned(),
        },
    );
    assert!(prompts.read(false, true));
    assert!(!prompts.asking());
    assert!(prompts.up());
    assert!(prompts.tick(Instant::now() + std::time::Duration::from_secs(5)));
    assert!(!prompts.up());
}

/// P8: Ctrl+Alt+Delete is the compositor's, and with no prompt up says so.
#[test]
fn the_trusted_key_is_the_compositor_s() {
    let (mut prompts, _sessiond) = pair();
    assert!(!prompts.trusted_key(29, true, false));
    assert!(!prompts.trusted_key(56, true, false));
    assert!(prompts.trusted_key(111, true, false));
    assert!(prompts.up());
    assert!(prompts.trusted_key(111, false, false));
    let _ = prompts.trusted_key(29, false, false);
    // Delete alone is anyone's.
    assert!(!prompts.trusted_key(111, true, false));
    assert!(prompts.tick(Instant::now() + super::NOTICE_FOR));
    assert!(!prompts.up());
}

/// P2: what the asker could put in a field is shown escaped and cut.
#[test]
fn every_field_is_shown_escaped_and_cut() {
    assert_eq!(shown("a\u{202e}b\nc"), "a\\u{202e}b\\u{a}c");
    for c in [
        '\u{2028}',
        '\u{2029}',
        '\u{ad}',
        '\u{fe0f}',
        '\u{e0041}',
        '\u{2066}',
    ] {
        assert!(shown(&format!("a{c}b")).contains("\\u{"), "{c:?}");
    }
    assert_eq!(super::shown_ascii("/bin/zä"), "/bin/z\\u{e4}");
    assert_eq!(super::shown_ascii("/bin/a\u{2028}b"), "/bin/a\\u{2028}b");
    assert_eq!(shown("\u{200f}x\u{2066}"), "\\u{200f}x\\u{2066}");
    let long = shown(&"a".repeat(500));
    assert_eq!(long.chars().count(), 121);
    let (prompts, _sessiond) = asking(true);
    let open = prompts.open.as_ref().unwrap();
    assert!(open.lines[0].contains("\\u{202e}"), "{:?}", open.lines);
    assert!(!open.lines.iter().any(|line| line.contains('\n')));
}

/// The secret is in no `Debug`, and the field is zeroed once sent.
#[test]
fn the_field_is_in_no_debug_and_gone_once_sent() {
    let (mut prompts, sessiond) = asking(true);
    type_in(&mut prompts, &[35, 24]);
    let said = format!("{prompts:?}");
    assert!(!said.contains("ho") && !said.contains('2'), "{said}");
    type_in(&mut prompts, &[28]);
    assert!(prompts.open.as_ref().unwrap().typed.is_empty());
    let _ = heard(&sessiond);
}

/// P6 and ledger 3636 (7): every way a frame's pixels leave the compositor
/// is one of these, each guarded while a prompt is up -- screencopy and
/// image-copy-capture through `copy_screen`, which blacks out the prompt's
/// whole screen (`screen.hidden`); lode's screenshots through
/// `screen_pixels`, refused while it is up; the frame dumps through
/// `write_ppm`, skipped while it is up. A new reader of a screen's pixels
/// fails this until it is guarded and listed.
#[test]
fn every_pixel_path_is_one_the_prompt_guards() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut counts = std::collections::BTreeMap::new();
    let mut stack = vec![dir];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "rs") || path.ends_with("prompt/tests.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            for pattern in [
                "screen_pixels(",
                "copy_screen(",
                "write_ppm(",
                ".drawn()",
                "canvas.data()",
            ] {
                *counts.entry(pattern).or_insert(0) += text.matches(pattern).count();
            }
        }
    }
    let listed = std::collections::BTreeMap::from([
        // its definition and lode's screenshot
        ("screen_pixels(", 2),
        // its definition and the three screenshot halves
        ("copy_screen(", 4),
        // its definition and the frame dump
        ("write_ppm(", 2),
        // write_ppm's, screen_pixels's, copy_screen's
        (".drawn()", 3),
        // screen_pixels's and copy_screen's
        ("canvas.data()", 2),
    ]);
    assert_eq!(
        counts, listed,
        "a reader of a screen's pixels changed: guard it while a prompt is up (P6) and list it here"
    );
}

/// Ledger 3636 (8), A3: the field is one block, made as the prompt comes up
/// and never grown, so typing leaves no copy of a prefix in a freed one.
#[test]
fn the_field_never_moves_as_it_fills() {
    let (mut prompts, _sessiond) = asking(true);
    let (at, room) = {
        let open = prompts.open.as_ref().unwrap();
        (open.typed.as_ptr(), open.typed.capacity())
    };
    assert_eq!(room, compositor_seat::prompt::MAX_SECRET);
    for _ in 0..compositor_seat::prompt::MAX_SECRET + 10 {
        let _ = prompts.key(30, true, keysym(30));
        let _ = prompts.key(30, false, keysym(30));
    }
    let open = prompts.open.as_ref().unwrap();
    assert_eq!(open.typed.len(), compositor_seat::prompt::MAX_SECRET);
    assert_eq!((open.typed.as_ptr(), open.typed.capacity()), (at, room));
}

/// Ledger 3636 (6): while a prompt is up the control socket changes
/// nothing: a dispatcher, a keyword and a reload are each refused and
/// nothing is handed back to be done; a question is still answered.
#[test]
fn the_control_socket_changes_nothing_while_a_prompt_is_up() {
    use std::io::{Read as _, Write as _};
    for (request, refused) in [
        ("dispatch workspace 2", true),
        ("keyword general:gaps_in 9", true),
        ("reload", true),
        ("version", false),
    ] {
        let (mut client, server) = std::os::unix::net::UnixStream::pair().unwrap();
        client.write_all(format!("{request}\n").as_bytes()).unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        let mut pending = crate::control::Pending::new(server).unwrap();
        assert_eq!(pending.read(Instant::now()), Some(true));
        let mut plugins = crate::plugins::Plugins::default();
        let todo = crate::control::serve(
            pending,
            &compositor_ipc::Snapshot::default(),
            &mut plugins,
            None,
            true,
        )
        .unwrap();
        let mut answer = String::new();
        let _ = client.read_to_string(&mut answer).unwrap();
        assert!(todo.is_empty(), "{request}: {todo:?}");
        assert_eq!(
            answer.contains("refused: a system prompt is up"),
            refused,
            "{request}: {answer}"
        );
    }
}
