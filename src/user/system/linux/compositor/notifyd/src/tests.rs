//! The interface's answers as libnotify would get them, and the banners'
//! drawing and clicks.

use compositor_dbus::{Kind, Message, Writer};

use crate::bus::{CAPABILITIES, NAME, PATH, handle};
use crate::paint::{self, Hit};
use crate::store::{Store, Urgency};

fn call(member: &str, signature: &str, body: Writer) -> Message {
    let mut message = Message::call(NAME, PATH, NAME, member).with_body(signature, body);
    message.serial = 9;
    message.sender = Some(":1.4".to_owned());
    message
}

fn notify(summary: &str, actions: &[&str], urgency: Option<u8>) -> Message {
    let mut body = Writer::new();
    body.string("Files");
    body.u32(0);
    body.string("folder");
    body.string(summary);
    body.string("3 files copied to Documents");
    body.strings(actions);
    body.array(8, |dict| {
        dict.align(8);
        dict.string("desktop-entry");
        dict.signature("s");
        dict.string("files");
        if let Some(urgency) = urgency {
            dict.align(8);
            dict.string("urgency");
            dict.signature("y");
            dict.byte(urgency);
        }
    });
    body.i32(-1);
    call("Notify", "susssasa{sv}i", body)
}

/// `Notify` answers the id and shows it; the urgency hint is read past the
/// hints before it; `CloseNotification` takes it off with reason 3.
#[test]
fn notify_and_close_answer_as_the_specification_says() {
    let mut store = Store::default();
    let outcome = handle(
        &mut store,
        &notify("Copy done", &["default", "Open"], Some(2)),
        0,
    );
    assert!(outcome.changed);
    let reply = outcome.send.first().expect("a reply");
    assert_eq!(reply.kind, Some(Kind::Return));
    assert_eq!(reply.reply_serial, Some(9));
    assert_eq!(reply.destination.as_deref(), Some(":1.4"));
    let id = reply.reader().u32().expect("the id");
    assert_eq!(store.shown()[0].urgency, Urgency::Critical);
    assert_eq!(store.shown()[0].until_ms, None, "critical stays");

    let mut body = Writer::new();
    body.u32(id);
    let outcome = handle(&mut store, &call("CloseNotification", "u", body), 10);
    assert_eq!(outcome.send.len(), 2);
    let signal = &outcome.send[1];
    assert_eq!(signal.member.as_deref(), Some("NotificationClosed"));
    let mut reader = signal.reader();
    assert_eq!((reader.u32(), reader.u32()), (Ok(id), Ok(3)));
    assert!(store.shown().is_empty());
}

/// The capabilities and the server's name, and an unknown method's error.
#[test]
fn the_server_says_what_it_is() {
    let mut store = Store::default();
    let outcome = handle(&mut store, &call("GetCapabilities", "", Writer::new()), 0);
    let caps = outcome.send[0].reader().strings().expect("as");
    assert_eq!(caps, CAPABILITIES);
    let outcome = handle(
        &mut store,
        &call("GetServerInformation", "", Writer::new()),
        0,
    );
    assert_eq!(outcome.send[0].signature, "ssss");
    assert_eq!(outcome.send[0].reader().string(), Ok("notifyd"));
    let outcome = handle(&mut store, &call("Frobnicate", "", Writer::new()), 0);
    assert_eq!(outcome.send[0].kind, Some(Kind::Error));
    assert!(!outcome.changed);
}

/// A banner is drawn as tall as its text and buttons, and a click lands on
/// the action, the cross or the banner, innermost first.
#[test]
fn banners_are_drawn_and_clicked() {
    let mut fonts = compositor_text::Fonts::system();
    let mut store = Store::default();
    let _ = handle(
        &mut store,
        &notify("Copy done", &["default", "Open", "undo", "Undo"], None),
        0,
    );
    let (pixmap, height, areas) = paint::banners(&mut fonts, store.shown(), 2).expect("a banner");
    assert_eq!(pixmap.width(), paint::WIDTH * 2);
    assert!(height > 80, "text and buttons: {height}");
    let id = store.shown()[0].id;
    let action = areas
        .iter()
        .find(|area| area.hit == Hit::Action(id, "undo".to_owned()))
        .expect("the Undo button");
    let (x, y, w, h) = action.rect;
    assert_eq!(
        paint::hit(&areas, f64::from(x + w / 2.0), f64::from(y + h / 2.0)),
        Some(Hit::Action(id, "undo".to_owned()))
    );
    assert_eq!(paint::hit(&areas, 30.0, 30.0), Some(Hit::Banner(id)));
    assert_eq!(paint::hit(&areas, 2.0, 2.0), None, "the gap round it");
    assert!(paint::banners(&mut fonts, &[], 1).is_none());
    let (_, _, areas) = paint::list(&mut fonts, store.history(), 1).expect("the list");
    assert!(
        areas
            .iter()
            .any(|area| area.hit == Hit::Action(0, "clear".to_owned()))
    );
}

/// A line of lode's alerts is a banner keyed by the alert and its source.
#[test]
fn an_alert_line_is_a_notification() {
    use crate::store::from_alert_line;
    let (key, summary, body, urgency) =
        from_alert_line("alert flood chrome chrome is flooding the log (4 in a second)").unwrap();
    assert_eq!(key, "lode:flood:chrome");
    assert_eq!(summary, "Log alert: flood");
    assert_eq!(body, "chrome is flooding the log (4 in a second)");
    assert_eq!(urgency, Urgency::Normal);
    assert_eq!(from_alert_line("died gpu 0x10 13"), None);
    assert_eq!(from_alert_line("alert flood"), None);
}
