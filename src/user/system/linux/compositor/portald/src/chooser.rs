//! `org.freedesktop.portal.FileChooser`: `OpenFile` and `SaveFile` answered
//! by Ferrix's own dialog, the `chooser` app (tole's `FileDialog`, the
//! customer's Key File Dialog design), so GTK's and Chrome's Open and Save
//! on Yoru are that one.
//!
//! A call is answered at once with its request's object path; the dialog
//! runs as a program of its own, and when it ends portald sends the
//! request's `Response` signal: 0 and the file's `file://` URI, or 1 when
//! the person cancelled. What the call's options ask is turned into the
//! dialog's arguments here: its title, the name and the folder to save in,
//! the accept button's words, and the filter -- the portal's globs and
//! MIME types, as the dialog's own patterns (`*.png | [image]`).
//!
//! Not yet: `multiple` is taken and one file is answered (the dialog
//! chooses one); `directory` is answered as an open of a file.

use compositor_dbus::{Malformed, Message, Reader, Writer};

/// The interface.
pub const FILE_CHOOSER: &str = "org.freedesktop.portal.FileChooser";
/// A request's interface.
pub const REQUEST: &str = "org.freedesktop.portal.Request";
/// The version `version` says: what Chrome and GTK ask for.
pub const VERSION: u32 = 3;
/// Where the dialog is.
pub const PROGRAM: &str = "/bin/chooser";

/// One of a filter's rules: a glob (0) or a MIME type (1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rule {
    /// `*.png`.
    Glob(String),
    /// `image/png`.
    Mime(String),
}

/// A filter: its name and its rules.
pub type Filter = (String, Vec<Rule>);

/// What an `OpenFile` or `SaveFile` asks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Asked {
    /// `SaveFile`.
    pub save: bool,
    /// The dialog's title.
    pub title: String,
    /// `handle_token`: the last part of the request's path.
    pub token: String,
    /// `accept_label`.
    pub accept_label: String,
    /// `multiple`.
    pub multiple: bool,
    /// `filters`.
    pub filters: Vec<Filter>,
    /// `current_filter`.
    pub current_filter: Option<Filter>,
    /// `current_name`.
    pub current_name: String,
    /// `current_folder`, without its NUL.
    pub current_folder: String,
}

/// A value of the options this reads.
enum Option_ {
    Str(String),
    Bool(bool),
    Bytes(Vec<u8>),
    Filters(Vec<Filter>),
    Filter(Filter),
    Other,
}

/// `(sa(us))`.
fn filter(reader: &mut Reader<'_>) -> Result<Filter, Malformed> {
    reader.align(8)?;
    let name = reader.string()?.to_owned();
    let length = reader.u32()? as usize;
    reader.align(8)?;
    let mut rules = Vec::new();
    let end = reader.position() + length;
    while reader.position() < end {
        reader.align(8)?;
        let kind = reader.u32()?;
        let text = reader.string()?.to_owned();
        rules.push(if kind == 1 {
            Rule::Mime(text)
        } else {
            Rule::Glob(text)
        });
    }
    Ok((name, rules))
}

/// A variant's value.
fn value(reader: &mut Reader<'_>) -> Result<Option_, Malformed> {
    let signature = reader.signature()?.to_owned();
    Ok(match signature.as_str() {
        "s" | "o" => Option_::Str(reader.string()?.to_owned()),
        "b" => Option_::Bool(reader.u32()? != 0),
        "ay" => {
            let length = reader.u32()? as usize;
            let mut bytes = Vec::with_capacity(length);
            for _ in 0..length {
                bytes.push(reader.byte()?);
            }
            Option_::Bytes(bytes)
        }
        "a(sa(us))" => {
            let length = reader.u32()? as usize;
            reader.align(8)?;
            let end = reader.position() + length;
            let mut filters = Vec::new();
            while reader.position() < end {
                filters.push(filter(reader)?);
            }
            Option_::Filters(filters)
        }
        "(sa(us))" => Option_::Filter(filter(reader)?),
        other => {
            let mut left = other;
            while !left.is_empty() {
                left = reader.skip(left)?;
            }
            Option_::Other
        }
    })
}

/// What `call` -- `OpenFile(s parent, s title, a{sv} options)` or
/// `SaveFile` -- asks.
///
/// # Errors
///
/// A body that is not what the interface says.
pub fn asked(call: &Message, save: bool) -> Result<Asked, Malformed> {
    let mut reader = call.reader();
    let _parent = reader.string()?;
    let mut made = Asked {
        save,
        title: reader.string()?.to_owned(),
        ..Asked::default()
    };
    let length = reader.u32()? as usize;
    reader.align(8)?;
    let end = reader.position() + length;
    while reader.position() < end {
        reader.align(8)?;
        let key = reader.string()?.to_owned();
        match (key.as_str(), value(&mut reader)?) {
            ("handle_token", Option_::Str(text)) => made.token = text,
            ("accept_label", Option_::Str(text)) => made.accept_label = text,
            ("current_name", Option_::Str(text)) => made.current_name = text,
            ("multiple", Option_::Bool(on)) => made.multiple = on,
            ("filters", Option_::Filters(list)) => made.filters = list,
            ("current_filter", Option_::Filter(one)) => made.current_filter = Some(one),
            ("current_folder", Option_::Bytes(bytes)) => {
                let bytes: Vec<u8> = bytes.into_iter().take_while(|b| *b != 0).collect();
                made.current_folder = String::from_utf8_lossy(&bytes).into_owned();
            }
            _ => {}
        }
    }
    Ok(made)
}

/// The dialog's pattern for a MIME type: a kind it knows, or the one
/// extension the type is spelled with; `None` for one it cannot say.
fn mime_pattern(mime: &str) -> Option<String> {
    let known = [
        ("image/*", "[image]"),
        ("text/*", "[text]"),
        ("text/plain", "*.txt"),
        ("text/html", "*.html | *.htm"),
        ("application/pdf", "*.pdf"),
        ("image/png", "*.png"),
        ("image/jpeg", "*.jpg | *.jpeg"),
        ("image/gif", "*.gif"),
        ("image/webp", "*.webp"),
        ("image/svg+xml", "*.svg"),
        ("application/json", "*.json"),
        ("application/zip", "*.zip"),
        ("application/x-mimearchive", "*.mhtml"),
        ("multipart/related", "*.mhtml"),
    ];
    known
        .iter()
        .find(|(m, _)| *m == mime)
        .map(|(_, p)| (*p).to_owned())
}

/// The dialog's pattern for `filter`: its globs and what its MIME types
/// say, joined; empty, every file, for a filter of `*` or one it can say
/// nothing of.
#[must_use]
pub fn pattern(filter: &Filter) -> String {
    let mut parts: Vec<String> = Vec::new();
    for rule in &filter.1 {
        let part = match rule {
            Rule::Glob(glob) if glob == "*" || glob == "*.*" => return String::new(),
            Rule::Glob(glob) => Some(glob.clone()),
            Rule::Mime(mime) if mime == "*/*" || mime == "application/octet-stream" => {
                return String::new();
            }
            Rule::Mime(mime) => mime_pattern(mime),
        };
        if let Some(part) = part
            && !parts.contains(&part)
        {
            parts.push(part);
        }
    }
    parts.join(" | ")
}

/// The dialog's arguments for `asked`.
#[must_use]
pub fn arguments(asked: &Asked) -> Vec<String> {
    let mut words = Vec::new();
    if asked.save {
        words.push("--save".to_owned());
    }
    if !asked.title.is_empty() {
        words.extend(["--title".to_owned(), asked.title.clone()]);
    }
    if !asked.current_name.is_empty() {
        words.extend(["--name".to_owned(), asked.current_name.clone()]);
    }
    if !asked.current_folder.is_empty() {
        words.extend(["--folder".to_owned(), asked.current_folder.clone()]);
    }
    if !asked.accept_label.is_empty() {
        words.extend(["--action".to_owned(), asked.accept_label.replace('_', "")]);
    }
    // Saving takes any name; opening, what the filter in force takes.
    let filter = asked
        .current_filter
        .as_ref()
        .or_else(|| asked.filters.first());
    if !asked.save
        && let Some(filter) = filter
    {
        let accept = pattern(filter);
        if !accept.is_empty() {
            words.extend(["--accept".to_owned(), accept]);
            words.extend(["--accept-label".to_owned(), filter.0.clone()]);
        }
    }
    words
}

/// A request's object path, for the call's sender and its token (the
/// portal's own spelling: the sender's unique name without its colon, its
/// dots as underscores).
#[must_use]
pub fn request_path(sender: &str, token: &str) -> String {
    let caller: String = sender
        .trim_start_matches(':')
        .chars()
        .map(|c| if c == '.' { '_' } else { c })
        .collect();
    let token: String = token
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("/org/freedesktop/portal/desktop/request/{caller}/{token}")
}

/// A path as a `file://` URI: what is not a plain character escaped.
#[must_use]
pub fn uri(path: &str) -> String {
    let mut out = String::from("file://");
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The request's `Response(u response, a{sv} results)`, to `sender`: 0 and
/// the URI for a file chosen, 1 and no URI for a cancel.
#[must_use]
pub fn response(path: &str, sender: &str, chosen: Option<&str>) -> Message {
    let mut body = Writer::new();
    body.u32(u32::from(chosen.is_none()));
    // `uris` is there on a cancel too, empty: GTK 3 reads it whatever the
    // response, and walked an unset list when it was not (both GTK 3's and
    // GTK 4's dialogs ended on Ferrix's first cancel, 2026-10-25), as
    // xdg-desktop-portal always sends it.
    let uris: Vec<String> = chosen.map(uri).into_iter().collect();
    let uris: Vec<&str> = uris.iter().map(String::as_str).collect();
    body.array(8, |out| {
        out.align(8);
        out.string("uris");
        out.signature("as");
        out.strings(&uris);
    });
    let mut signal = Message::signal(path, REQUEST, "Response").with_body("ua{sv}", body);
    signal.destination = Some(sender.to_owned());
    signal
}

#[cfg(test)]
mod tests {
    use super::{Asked, Rule, arguments, asked, pattern, request_path, response, uri};
    use compositor_dbus::{Message, Writer};

    /// What Chrome sends for a Save As: a name, a folder, and its filters.
    fn chrome_save() -> Message {
        let mut body = Writer::new();
        body.string("wayland:abc");
        body.string("Save File");
        body.array(8, |out| {
            out.align(8);
            out.string("handle_token");
            out.signature("s");
            out.string("chrome1");
            out.align(8);
            out.string("current_name");
            out.signature("s");
            out.string("page.html");
            out.align(8);
            out.string("current_folder");
            out.signature("ay");
            out.array(1, |out| {
                for b in b"/dev/shm/Downloads\0" {
                    out.byte(*b);
                }
            });
            out.align(8);
            out.string("filters");
            out.signature("a(sa(us))");
            out.array(8, |out| {
                out.align(8);
                out.string("Webpage, Complete");
                out.array(8, |out| {
                    out.align(8);
                    out.u32(0);
                    out.string("*.html");
                    out.align(8);
                    out.u32(1);
                    out.string("text/html");
                });
            });
            out.align(8);
            out.string("modal");
            out.signature("b");
            out.bool(true);
        });
        let mut call = Message::call(
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            super::FILE_CHOOSER,
            "SaveFile",
        )
        .with_body("ssa{sv}", body);
        call.sender = Some(":1.42".to_owned());
        call
    }

    #[test]
    fn a_save_as_is_read_as_chrome_sends_it() {
        let got = asked(&chrome_save(), true).expect("read");
        assert_eq!(got.title, "Save File");
        assert_eq!(got.token, "chrome1");
        assert_eq!(got.current_name, "page.html");
        assert_eq!(got.current_folder, "/dev/shm/Downloads");
        assert_eq!(
            got.filters,
            vec![(
                "Webpage, Complete".to_owned(),
                vec![
                    Rule::Glob("*.html".to_owned()),
                    Rule::Mime("text/html".to_owned())
                ]
            )]
        );
        let words = arguments(&got);
        assert_eq!(
            words,
            [
                "--save",
                "--title",
                "Save File",
                "--name",
                "page.html",
                "--folder",
                "/dev/shm/Downloads"
            ]
        );
    }

    #[test]
    fn filters_become_the_dialog_s_patterns() {
        let f = |rules: Vec<Rule>| ("F".to_owned(), rules);
        assert_eq!(
            pattern(&f(vec![
                Rule::Glob("*.png".into()),
                Rule::Mime("image/jpeg".into())
            ])),
            "*.png | *.jpg | *.jpeg"
        );
        assert_eq!(pattern(&f(vec![Rule::Mime("image/*".into())])), "[image]");
        assert_eq!(pattern(&f(vec![Rule::Glob("*".into())])), "");
        assert_eq!(
            pattern(&f(vec![Rule::Mime("application/x-unknown".into())])),
            ""
        );
        // An open with a filter in force says it, and its name.
        let open = Asked {
            title: "Open".into(),
            filters: vec![f(vec![Rule::Glob("*.pem".into())])],
            accept_label: "_Open".into(),
            ..Asked::default()
        };
        assert_eq!(
            arguments(&open),
            [
                "--title",
                "Open",
                "--action",
                "Open",
                "--accept",
                "*.pem",
                "--accept-label",
                "F"
            ]
        );
    }

    #[test]
    fn the_request_and_its_answer() {
        assert_eq!(
            request_path(":1.42", "chrome-1"),
            "/org/freedesktop/portal/desktop/request/1_42/chrome_1"
        );
        assert_eq!(
            uri("/home/ada/My File.pdf"),
            "file:///home/ada/My%20File.pdf"
        );
        let done = response("/r", ":1.42", Some("/tmp/a b"));
        assert_eq!(done.signature, "ua{sv}");
        assert_eq!(done.destination.as_deref(), Some(":1.42"));
        let mut reader = done.reader();
        assert_eq!(reader.u32(), Ok(0));
        let cancelled = response("/r", ":1.42", None);
        let mut reader = cancelled.reader();
        assert_eq!(reader.u32(), Ok(1));
        // The results hold `uris`, empty.
        let _ = reader.u32();
        reader.align(8).expect("aligned");
        assert_eq!(reader.string(), Ok("uris"));
        assert_eq!(reader.signature(), Ok("as"));
        assert_eq!(reader.u32(), Ok(0));
    }
}
