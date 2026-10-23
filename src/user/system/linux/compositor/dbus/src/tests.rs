//! The wire format against bytes the D-Bus Specification and dbus-daemon
//! give, and a message's round trip.

use super::message::{Kind, Message};
use super::wire::{Reader, Writer, split_one};
use super::{auth_line, session_path};

/// `AUTH EXTERNAL` carries the uid's decimal digits in hex: uid 1000 is
/// "31303030".
#[test]
fn the_auth_line_hexes_the_uids_digits() {
    assert_eq!(auth_line(1000), "AUTH EXTERNAL 31303030\r\n");
    assert_eq!(auth_line(0), "AUTH EXTERNAL 30\r\n");
}

/// The session bus's path comes out of its address.
#[test]
fn the_session_path_is_read_from_the_address() {
    assert_eq!(
        session_path("unix:path=/run/user/1000/bus,guid=abc"),
        Some("/run/user/1000/bus".to_owned())
    );
    assert_eq!(
        session_path("tcp:host=x;unix:path=/tmp/b"),
        Some("/tmp/b".to_owned())
    );
    assert_eq!(session_path("unix:abstract=/x"), None);
}

/// `Hello` as dbus-send writes it: the header the specification spells
/// out byte for byte, then a body-less call.
#[test]
fn hello_is_the_bytes_the_specification_gives() {
    let hello = Message::call(
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "Hello",
    );
    let bytes = hello.encode(1);
    assert_eq!(&bytes[..12], &[b'l', 1, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0]);
    // The field array: PATH first, at offset 16, code 1, signature "o".
    assert_eq!(&bytes[16..20], &[1, 1, b'o', 0]);
    assert_eq!(bytes.len() % 8, 0, "header padded to 8, no body");
    let back = Message::decode(&bytes).expect("a message");
    assert_eq!(back.kind, Some(Kind::Call));
    assert_eq!(back.member.as_deref(), Some("Hello"));
    assert_eq!(back.path.as_deref(), Some("/org/freedesktop/DBus"));
    assert_eq!(Message::length(&bytes), Some(bytes.len()));
}

/// `Notify(susssasa{sv}i)` as libnotify sends it: every argument read back,
/// the hints dictionary skipped over by its signature.
#[test]
fn a_notify_call_reads_back() {
    let mut body = Writer::new();
    body.string("Files");
    body.u32(0);
    body.string("folder");
    body.string("Copy done");
    body.string("3 files copied");
    body.strings(&["default", "Open"]);
    body.array(8, |dict| {
        dict.align(8);
        dict.string("urgency");
        dict.signature("y");
        dict.byte(2);
        dict.align(8);
        dict.string("transient");
        dict.signature("b");
        dict.bool(true);
    });
    body.i32(-1);
    let call = Message::call(
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
        "Notify",
    )
    .with_body("susssasa{sv}i", body);
    let back = Message::decode(&call.encode(7)).expect("a message");
    assert_eq!(back.signature, "susssasa{sv}i");
    let mut reader = back.reader();
    assert_eq!(reader.string(), Ok("Files"));
    assert_eq!(reader.u32(), Ok(0));
    assert_eq!(reader.string(), Ok("folder"));
    assert_eq!(reader.string(), Ok("Copy done"));
    assert_eq!(reader.string(), Ok("3 files copied"));
    assert_eq!(reader.strings(), Ok(vec!["default", "Open"]));
    assert_eq!(reader.skip("a{sv}i"), Ok("i"));
    assert_eq!(reader.i32(), Ok(-1));
}

/// A signature splits into its complete types.
#[test]
fn signatures_split_into_complete_types() {
    assert_eq!(split_one("susssasa{sv}i"), Ok(("s", "usssasa{sv}i")));
    assert_eq!(split_one("asa{sv}i"), Ok(("as", "a{sv}i")));
    assert_eq!(split_one("a{sv}i"), Ok(("a{sv}", "i")));
    assert_eq!(split_one("(ii)s"), Ok(("(ii)", "s")));
    assert!(split_one("(i").is_err());
}

/// Reading past the end is an error, not a panic.
#[test]
fn a_short_body_is_an_error() {
    let mut reader = Reader::new(&[1, 0]);
    assert!(reader.u32().is_err());
    assert!(Message::decode(&[b'l', 1]).is_err());
}
