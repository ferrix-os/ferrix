//! `org.freedesktop.Notifications` (the Desktop Notifications
//! Specification 1.2) as a pure function of the store and a message: what
//! to answer and which signals to send. No socket here; `main.rs` reads
//! and writes them.

use compositor_dbus::message::NO_REPLY_EXPECTED;
use compositor_dbus::{Kind, Malformed, Message, Reader, Writer};

use crate::store::{Asked, Reason, Store};

/// The name owned on the session bus.
pub const NAME: &str = "org.freedesktop.Notifications";
/// The object served.
pub const PATH: &str = "/org/freedesktop/Notifications";

/// What the server can do: `GetCapabilities`.
pub const CAPABILITIES: [&str; 3] = ["body", "actions", "persistence"];

const INTROSPECTION: &str = r#"<!DOCTYPE node PUBLIC "-//freedesktop//DTD D-BUS Object Introspection 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd">
<node>
 <interface name="org.freedesktop.Notifications">
  <method name="Notify"><arg type="s" direction="in"/><arg type="u" direction="in"/><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="as" direction="in"/><arg type="a{sv}" direction="in"/><arg type="i" direction="in"/><arg type="u" direction="out"/></method>
  <method name="CloseNotification"><arg type="u" direction="in"/></method>
  <method name="GetCapabilities"><arg type="as" direction="out"/></method>
  <method name="GetServerInformation"><arg type="s" direction="out"/><arg type="s" direction="out"/><arg type="s" direction="out"/><arg type="s" direction="out"/></method>
  <signal name="NotificationClosed"><arg type="u"/><arg type="u"/></signal>
  <signal name="ActionInvoked"><arg type="u"/><arg type="s"/></signal>
 </interface>
 <interface name="org.freedesktop.DBus.Introspectable">
  <method name="Introspect"><arg type="s" direction="out"/></method>
 </interface>
</node>
"#;

/// What a message comes to.
#[derive(Debug, Default)]
pub struct Outcome {
    /// Messages to send: the reply first, then signals.
    pub send: Vec<Message>,
    /// Whether what is on screen changed.
    pub changed: bool,
}

/// `NotificationClosed(id, reason)`.
#[must_use]
pub fn closed(id: u32, reason: Reason) -> Message {
    let mut body = Writer::new();
    body.u32(id);
    body.u32(reason.code());
    Message::signal(PATH, NAME, "NotificationClosed").with_body("uu", body)
}

/// `ActionInvoked(id, key)`.
#[must_use]
pub fn invoked(id: u32, key: &str) -> Message {
    let mut body = Writer::new();
    body.u32(id);
    body.string(key);
    Message::signal(PATH, NAME, "ActionInvoked").with_body("us", body)
}

/// `Notify`'s arguments read from its body.
///
/// # Errors
///
/// [`Malformed`] for a body that is not `susssasa{sv}i`.
pub fn read_notify(message: &Message) -> Result<Asked, Malformed> {
    if message.signature != "susssasa{sv}i" {
        return Err(Malformed::Signature);
    }
    let mut reader = message.reader();
    let app = reader.string()?.to_owned();
    let replaces = reader.u32()?;
    let _icon = reader.string()?;
    let summary = reader.string()?.to_owned();
    let body = reader.string()?.to_owned();
    let actions = reader.strings()?.into_iter().map(str::to_owned).collect();
    let urgency = read_urgency(&mut reader)?;
    let timeout_ms = reader.i32()?;
    Ok(Asked {
        app,
        replaces,
        summary,
        body,
        actions,
        urgency,
        timeout_ms,
    })
}

/// The `a{sv}` hints, of which only `urgency` (a byte) is taken.
fn read_urgency(reader: &mut Reader<'_>) -> Result<Option<u8>, Malformed> {
    let length = usize::try_from(reader.u32()?).map_err(|_| Malformed::Short)?;
    reader.align(8)?;
    let end = reader.position() + length;
    let mut urgency = None;
    while reader.position() < end {
        reader.align(8)?;
        let key = reader.string()?;
        let signature = reader.signature()?;
        if key == "urgency" && signature == "y" {
            urgency = Some(reader.byte()?);
        } else {
            let mut left = signature;
            while !left.is_empty() {
                left = reader.skip(left)?;
            }
        }
    }
    Ok(urgency)
}

/// Answer `message` against `store` at `now_ms`.
pub fn handle(store: &mut Store, message: &Message, now_ms: u64) -> Outcome {
    let mut outcome = Outcome::default();
    if message.kind != Some(Kind::Call) {
        return outcome;
    }
    let mut reply = Message::reply_to(message);
    if message.is_call(NAME, "Notify") {
        match read_notify(message) {
            Ok(asked) => {
                let (id, pushed) = store.notify(asked, now_ms);
                let mut body = Writer::new();
                body.u32(id);
                reply = reply.with_body("u", body);
                outcome.send.push(reply);
                outcome
                    .send
                    .extend(pushed.into_iter().map(|gone| closed(gone, Reason::Expired)));
                outcome.changed = true;
                return outcome;
            }
            Err(why) => {
                reply = Message::error_to(
                    message,
                    "org.freedesktop.DBus.Error.InvalidArgs",
                    &format!("Notify: {why}"),
                );
            }
        }
    } else if message.is_call(NAME, "CloseNotification") {
        let id = message.reader().u32().unwrap_or(0);
        outcome.send.push(reply);
        if store.close(id).is_some() {
            outcome.send.push(closed(id, Reason::Closed));
            outcome.changed = true;
        }
        return outcome;
    } else if message.is_call(NAME, "GetCapabilities") {
        let mut body = Writer::new();
        body.strings(&CAPABILITIES);
        reply = reply.with_body("as", body);
    } else if message.is_call(NAME, "GetServerInformation") {
        let mut body = Writer::new();
        body.string("notifyd");
        body.string("Ferrix");
        body.string(env!("CARGO_PKG_VERSION"));
        body.string("1.2");
        reply = reply.with_body("ssss", body);
    } else if message.is_call("org.freedesktop.DBus.Introspectable", "Introspect") {
        let mut body = Writer::new();
        body.string(INTROSPECTION);
        reply = reply.with_body("s", body);
    } else if message.is_call("org.freedesktop.DBus.Peer", "Ping") {
        // An empty return.
    } else {
        reply = Message::error_to(
            message,
            "org.freedesktop.DBus.Error.UnknownMethod",
            &format!(
                "no method {} here",
                message.member.as_deref().unwrap_or_default()
            ),
        );
    }
    if message.flags & NO_REPLY_EXPECTED == 0 {
        outcome.send.push(reply);
    }
    outcome
}
