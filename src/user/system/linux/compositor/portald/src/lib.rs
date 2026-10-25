//! `portald`: the one XDG desktop portal interface a desktop's look
//! needs, `org.freedesktop.portal.Settings`, on the session bus as
//! `org.freedesktop.portal.Desktop` (Ferrix's `docs/DESKTOP.md` §6, Yoru's
//! "Chrome and GTK apps in the tokens"). GTK 4 and libadwaita read
//! `org.freedesktop.appearance` from it for dark or light and the accent,
//! GTK 3 the GNOME keys for the theme's name, and Chrome its colour scheme
//! for its own dark mode -- each through `ReadOne` (version 2) or the older
//! `Read`, whose answer is a variant in a variant.
//!
//! It also answers `org.freedesktop.portal.FileChooser` ([`chooser`]):
//! Open and Save are Ferrix's own dialog, tole's `FileDialog`, so GTK's and
//! Chrome's dialogs on Yoru are that one.
//!
//! What it says is fixed at its start (`--light` for a light desktop):
//! nothing is read from the person, nothing is written, and no other
//! portal is offered -- a program that asks for a screenshot gets the
//! bus's "no such interface" and falls back as it does where no portal
//! runs.

pub mod chooser;

use compositor_dbus::{Message, Writer};

/// The name on the bus.
pub const NAME: &str = "org.freedesktop.portal.Desktop";
/// The object.
pub const PATH: &str = "/org/freedesktop/portal/desktop";
/// The interface.
pub const SETTINGS: &str = "org.freedesktop.portal.Settings";
/// The interface's version, as `version` says it.
pub const VERSION: u32 = 2;

/// A setting's value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// `u`.
    U32(u32),
    /// `s`.
    Str(&'static str),
    /// `(ddd)`: a colour, each part 0 to 1.
    Colour(f64, f64, f64),
}

impl Value {
    /// Its D-Bus signature.
    #[must_use]
    pub fn signature(&self) -> &'static str {
        match self {
            Value::U32(_) => "u",
            Value::Str(_) => "s",
            Value::Colour(..) => "(ddd)",
        }
    }

    /// Write it as the content of a variant: its signature, then it.
    pub fn write_variant(&self, out: &mut Writer) {
        out.signature(self.signature());
        match self {
            Value::U32(value) => out.u32(*value),
            Value::Str(value) => out.string(value),
            Value::Colour(r, g, b) => {
                out.align(8);
                out.f64(*r);
                out.f64(*g);
                out.f64(*b);
            }
        }
    }
}

/// Every setting: `(namespace, key, value)`.
#[must_use]
pub fn settings(dark: bool) -> Vec<(&'static str, &'static str, Value)> {
    // Nocturne's accent, #9184d9, and Ferrix Light's (the same hue).
    let accent = Value::Colour(
        f64::from(0x91_u8) / 255.0,
        f64::from(0x84_u8) / 255.0,
        f64::from(0xd9_u8) / 255.0,
    );
    vec![
        // 1 prefers dark, 2 prefers light (the portal's own numbers).
        (
            "org.freedesktop.appearance",
            "color-scheme",
            Value::U32(if dark { 1 } else { 2 }),
        ),
        ("org.freedesktop.appearance", "accent-color", accent),
        ("org.freedesktop.appearance", "contrast", Value::U32(0)),
        (
            "org.freedesktop.appearance",
            "reduced-motion",
            Value::U32(0),
        ),
        (
            "org.gnome.desktop.interface",
            "color-scheme",
            Value::Str(if dark { "prefer-dark" } else { "default" }),
        ),
        (
            "org.gnome.desktop.interface",
            "gtk-theme",
            Value::Str(if dark { "Nocturne" } else { "Adwaita" }),
        ),
        (
            "org.gnome.desktop.interface",
            "font-name",
            Value::Str("Inter 11"),
        ),
        (
            "org.gnome.desktop.interface",
            "cursor-theme",
            Value::Str("Adwaita"),
        ),
    ]
}

/// Whether `namespace` is one of `patterns`: a name, or a prefix before a
/// `*`; no patterns match every namespace.
#[must_use]
pub fn wanted(patterns: &[&str], namespace: &str) -> bool {
    patterns.is_empty()
        || patterns
            .iter()
            .any(|pattern| match pattern.strip_suffix('*') {
                Some(prefix) => namespace.starts_with(prefix),
                None => *pattern == namespace,
            })
}

/// The `a{sv}` of `namespace`'s keys in `all`, written into an array.
fn write_keys(out: &mut Writer, all: &[(&str, &str, Value)], namespace: &str) {
    for (_, key, value) in all.iter().filter(|(n, _, _)| *n == namespace) {
        out.align(8);
        out.string(key);
        value.write_variant(out);
    }
}

/// A file chooser's call: the reply naming its request, the request's
/// path, the caller, and what the dialog is asked; `None` for any other
/// message.
#[must_use]
pub fn dialog(call: &Message, serial: u64) -> Option<(Message, String, String, chooser::Asked)> {
    let save = if call.is_call(chooser::FILE_CHOOSER, "SaveFile") {
        true
    } else if call.is_call(chooser::FILE_CHOOSER, "OpenFile") {
        false
    } else {
        return None;
    };
    let sender = call.sender.clone().unwrap_or_default();
    let Ok(mut asked) = chooser::asked(call, save) else {
        return Some((
            Message::error_to(
                call,
                "org.freedesktop.DBus.Error.InvalidArgs",
                "OpenFile and SaveFile take (s parent_window, s title, a{sv} options)",
            ),
            String::new(),
            sender,
            chooser::Asked::default(),
        ));
    };
    if asked.token.is_empty() {
        asked.token = format!("portald{serial}");
    }
    let path = chooser::request_path(&sender, &asked.token);
    let mut body = Writer::new();
    body.string(&path);
    let reply = Message::reply_to(call).with_body("o", body);
    Some((reply, path, sender, asked))
}

/// The answer to `call`, or `None` for a message that wants none.
#[must_use]
pub fn answer(call: &Message, dark: bool) -> Option<Message> {
    if call.kind != Some(compositor_dbus::Kind::Call) {
        return None;
    }
    let all = settings(dark);
    let mut reader = call.reader();
    if call.is_call(SETTINGS, "ReadAll") {
        let patterns = reader.strings().unwrap_or_default();
        let mut namespaces: Vec<&str> = Vec::new();
        for (namespace, _, _) in &all {
            if !namespaces.contains(namespace) && wanted(&patterns, namespace) {
                namespaces.push(namespace);
            }
        }
        let mut body = Writer::new();
        body.array(8, |out| {
            for namespace in &namespaces {
                out.align(8);
                out.string(namespace);
                out.array(8, |out| write_keys(out, &all, namespace));
            }
        });
        return Some(Message::reply_to(call).with_body("a{sa{sv}}", body));
    }
    let read_one = call.is_call(SETTINGS, "ReadOne");
    if read_one || call.is_call(SETTINGS, "Read") {
        let namespace = reader.string().unwrap_or_default().to_owned();
        let key = reader.string().unwrap_or_default().to_owned();
        let Some((_, _, value)) = all.iter().find(|(n, k, _)| *n == namespace && *k == key) else {
            return Some(Message::error_to(
                call,
                "org.freedesktop.portal.Error.NotFound",
                &format!("Requested setting {key} in {namespace} not found"),
            ));
        };
        let mut body = Writer::new();
        if read_one {
            value.write_variant(&mut body);
        } else {
            // `Read` answers a variant holding the variant.
            body.signature("v");
            value.write_variant(&mut body);
        }
        return Some(Message::reply_to(call).with_body("v", body));
    }
    if call.is_call("org.freedesktop.DBus.Properties", "Get") {
        let interface = reader.string().unwrap_or_default();
        let property = reader.string().unwrap_or_default();
        let version = match interface {
            SETTINGS => Some(VERSION),
            chooser::FILE_CHOOSER => Some(chooser::VERSION),
            _ => None,
        };
        if let Some(version) = version
            && property == "version"
        {
            let mut body = Writer::new();
            Value::U32(version).write_variant(&mut body);
            return Some(Message::reply_to(call).with_body("v", body));
        }
        return Some(Message::error_to(
            call,
            "org.freedesktop.DBus.Error.UnknownProperty",
            "no such property",
        ));
    }
    if call.is_call("org.freedesktop.DBus.Properties", "GetAll") {
        let interface = reader.string().unwrap_or_default();
        let mut body = Writer::new();
        body.array(8, |out| {
            let version = match interface {
                SETTINGS => Some(VERSION),
                chooser::FILE_CHOOSER => Some(chooser::VERSION),
                _ => None,
            };
            if let Some(version) = version {
                out.align(8);
                out.string("version");
                Value::U32(version).write_variant(out);
            }
        });
        return Some(Message::reply_to(call).with_body("a{sv}", body));
    }
    if call.is_call("org.freedesktop.DBus.Introspectable", "Introspect") {
        let mut body = Writer::new();
        body.string(INTROSPECTION);
        return Some(Message::reply_to(call).with_body("s", body));
    }
    if call.is_call("org.freedesktop.DBus.Peer", "Ping") || call.is_call(chooser::REQUEST, "Close")
    {
        return Some(Message::reply_to(call));
    }
    Some(Message::error_to(
        call,
        "org.freedesktop.DBus.Error.UnknownMethod",
        "portald offers org.freedesktop.portal.Settings and FileChooser alone",
    ))
}

/// What `Introspect` says.
pub const INTROSPECTION: &str = r#"<!DOCTYPE node PUBLIC "-//freedesktop//DTD D-BUS Object Introspection 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd">
<node>
 <interface name="org.freedesktop.portal.Settings">
  <method name="ReadAll"><arg type="as" name="namespaces" direction="in"/><arg type="a{sa{sv}}" name="value" direction="out"/></method>
  <method name="Read"><arg type="s" name="namespace" direction="in"/><arg type="s" name="key" direction="in"/><arg type="v" name="value" direction="out"/></method>
  <method name="ReadOne"><arg type="s" name="namespace" direction="in"/><arg type="s" name="key" direction="in"/><arg type="v" name="value" direction="out"/></method>
  <signal name="SettingChanged"><arg type="s" name="namespace"/><arg type="s" name="key"/><arg type="v" name="value"/></signal>
  <property name="version" type="u" access="read"/>
 </interface>
 <interface name="org.freedesktop.portal.FileChooser">
  <method name="OpenFile"><arg type="s" name="parent_window" direction="in"/><arg type="s" name="title" direction="in"/><arg type="a{sv}" name="options" direction="in"/><arg type="o" name="handle" direction="out"/></method>
  <method name="SaveFile"><arg type="s" name="parent_window" direction="in"/><arg type="s" name="title" direction="in"/><arg type="a{sv}" name="options" direction="in"/><arg type="o" name="handle" direction="out"/></method>
  <property name="version" type="u" access="read"/>
 </interface>
</node>
"#;

#[cfg(test)]
mod tests {
    use super::{PATH, SETTINGS, answer, wanted};
    use compositor_dbus::{Kind, Message, Writer};

    fn call(member: &str, signature: &str, body: Writer) -> Message {
        let mut made =
            Message::call(super::NAME, PATH, SETTINGS, member).with_body(signature, body);
        made.serial = 7;
        made.sender = Some(":1.9".to_owned());
        made
    }

    fn two(a: &str, b: &str) -> Writer {
        let mut body = Writer::new();
        body.string(a);
        body.string(b);
        body
    }

    #[test]
    fn read_one_says_dark_and_read_wraps_it_again() {
        let reply = answer(
            &call(
                "ReadOne",
                "ss",
                two("org.freedesktop.appearance", "color-scheme"),
            ),
            true,
        )
        .expect("answered");
        assert_eq!(reply.kind, Some(Kind::Return));
        assert_eq!(reply.signature, "v");
        let mut reader = reply.reader();
        assert_eq!(reader.signature(), Ok("u"));
        assert_eq!(reader.u32(), Ok(1));
        let reply = answer(
            &call(
                "Read",
                "ss",
                two("org.gnome.desktop.interface", "gtk-theme"),
            ),
            true,
        )
        .expect("answered");
        let mut reader = reply.reader();
        assert_eq!(reader.signature(), Ok("v"));
        assert_eq!(reader.signature(), Ok("s"));
        assert_eq!(reader.string(), Ok("Nocturne"));
        let light = answer(
            &call(
                "ReadOne",
                "ss",
                two("org.freedesktop.appearance", "color-scheme"),
            ),
            false,
        )
        .expect("answered");
        let mut reader = light.reader();
        let _ = reader.signature();
        assert_eq!(reader.u32(), Ok(2));
    }

    #[test]
    fn the_accent_is_three_doubles() {
        let reply = answer(
            &call(
                "ReadOne",
                "ss",
                two("org.freedesktop.appearance", "accent-color"),
            ),
            true,
        )
        .expect("answered");
        let mut reader = reply.reader();
        assert_eq!(reader.signature(), Ok("(ddd)"));
        let red = reader.f64().expect("red");
        assert!((red - 145.0 / 255.0).abs() < 1e-9);
    }

    #[test]
    fn an_unknown_key_is_not_found_and_read_all_filters() {
        let reply = answer(
            &call("ReadOne", "ss", two("org.freedesktop.appearance", "nope")),
            true,
        )
        .expect("answered");
        assert_eq!(reply.kind, Some(Kind::Error));
        assert_eq!(
            reply.error_name.as_deref(),
            Some("org.freedesktop.portal.Error.NotFound")
        );
        let mut body = Writer::new();
        body.strings(&["org.freedesktop.*"]);
        let all = answer(&call("ReadAll", "as", body), true).expect("answered");
        assert_eq!(all.signature, "a{sa{sv}}");
        let mut reader = all.reader();
        // The array's length, then the first entry's namespace.
        let _ = reader.u32();
        reader.align(8).expect("aligned");
        assert_eq!(reader.string(), Ok("org.freedesktop.appearance"));
        assert!(wanted(&[], "anything"));
        assert!(!wanted(
            &["org.gnome.desktop.interface"],
            "org.freedesktop.appearance"
        ));
    }

    #[test]
    fn other_portals_are_refused_and_signals_unanswered() {
        let mut other = Message::call(
            super::NAME,
            PATH,
            "org.freedesktop.portal.FileChooser",
            "OpenFile",
        );
        other.serial = 3;
        let reply = answer(&other, true).expect("answered");
        assert_eq!(reply.kind, Some(Kind::Error));
        let signal = Message::signal(PATH, SETTINGS, "SettingChanged");
        assert!(answer(&signal, true).is_none());
    }
}
