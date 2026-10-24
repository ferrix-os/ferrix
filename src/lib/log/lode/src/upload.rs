//! Store-and-forward to a collector (`docs/LOGGING.md` §10.2): what
//! `lode upload` does that is a pure function of bytes.
//!
//! The store is the buffer. Each destination has a cursor, the store
//! sequence number of the last record it acknowledged; the uploader reads
//! what follows, sends it, and moves the cursor only on the destination's
//! `2xx`. A destination that is down is tried again after 1 s, then 2, 4,
//! ... up to 5 min, for ever; nothing is lost that vacuum has not removed,
//! and what vacuum (or `lode delete`, or a RAM-only class at a reboot)
//! removed before it was sent is counted and said.
//!
//! `/etc/ferrix/lode-upload.conf`, one destination a line, `#` comments:
//!
//! ```text
//! # NAME     URL                                  FORMAT  [auth] [where EXPR]
//! collector  http://127.0.0.1:19532/upload        export
//! vector     http://localhost:8080/               json    where priority <= warning
//! ```
//!
//! * `export` is the Journal Export Format, posted as `systemd-journal-
//!   upload` posts it (`Content-Type: application/vnd.fdo.journal`), which a
//!   real `systemd-journal-remote` receives; `json` is one journald JSON
//!   object a line (`application/x-ndjson`), as Vector's, Loki's and Fluent
//!   Bit's HTTP sources take it.
//! * Plain HTTP goes only to this machine (`localhost`, `127.0.0.0/8`): a
//!   collector elsewhere is reached through an ssh tunnel until TLS is built.
//! * Records of the auth and authpriv facilities and of `auth.service` are
//!   never sent unless the line says `auth`.
//! * Blobs (`docs/LOGGING.md` §21.4) follow their records on a second,
//!   lower-priority stream, one `POST` a blob to the URL's path with
//!   `/blob` after it, once the records that name them are acknowledged:
//!   `blobs=small` (an `export` line's default) sends the small ones --
//!   backtraces, maps, descriptor lists, at most [`SMALL_BLOB_MAX`] each,
//!   no screenshot and no memory -- `blobs=all` every one, `blobs=none`
//!   (a `json` line's default) none.

use alloc::string::String;
use alloc::vec::Vec;

use crate::filter::{self, Expr};
use crate::format::{self, Output};
use crate::record::{Record, UNSPECIFIED};
use crate::time::Zone;

/// Where the uploader's configuration is.
pub const CONFIG: &str = "/etc/ferrix/lode-upload.conf";

/// Where each destination's cursor is kept, a file a destination.
pub const STATE: &str = "/var/lib/ferrix/lode-upload";

/// The most records one request carries.
pub const BATCH: usize = 1000;

/// The most bytes one request's body carries, a record over it alone.
pub const BATCH_BYTES: usize = 1024 * 1024;

/// The first wait after a failure.
pub const BACKOFF_FIRST_MS: u64 = 1000;

/// The longest wait between tries.
pub const BACKOFF_MAX_MS: u64 = 5 * 60 * 1000;

/// journal-remote's port, the default.
pub const DEFAULT_PORT: u16 = 19532;

/// `MESSAGE_ID` of a record saying records were gone before they were sent.
pub const MESSAGE_ID_UNSENT: &str = "e2a6c4b8d0f24e1a9c3b5d7f9e1a3c5b";

/// What a destination is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// The Journal Export Format, as journal-upload posts it.
    Export,
    /// journald's JSON, one object a line.
    Json,
}

impl Format {
    /// Its `Content-Type`.
    #[must_use]
    pub fn content_type(self) -> &'static str {
        match self {
            Format::Export => "application/vnd.fdo.journal",
            Format::Json => "application/x-ndjson",
        }
    }

    /// The output format that writes it.
    #[must_use]
    pub fn output(self) -> Output {
        match self {
            Format::Export => Output::Export,
            Format::Json => Output::Json,
        }
    }
}

/// Where a destination is: `http://` on this machine, or `https://`
/// anywhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    /// Whether it is `https://`.
    pub tls: bool,
    /// Its IPv4 address, when the host is one (or `localhost`).
    pub address: Option<[u8; 4]>,
    /// Its host as written: for `Host:`, name resolution and the
    /// certificate's name.
    pub host: String,
    /// Its port.
    pub port: u16,
    /// Its path, beginning with `/`.
    pub path: String,
}

impl Url {
    /// Read `http://HOST[:PORT][/PATH]` or `https://...`. A plain `http://`
    /// HOST is `localhost` or an IPv4 address in `127.0.0.0/8`; `path`
    /// stands in for a missing PATH.
    ///
    /// # Errors
    ///
    /// What is wrong with it, a plain-HTTP host off this machine included.
    pub fn parse(text: &str, path: &str) -> Result<Url, String> {
        let (tls, rest) = if let Some(rest) = text.strip_prefix("https://") {
            (true, rest)
        } else if let Some(rest) = text.strip_prefix("http://") {
            (false, rest)
        } else {
            return Err(alloc::format!("{text}: http:// or https://"));
        };
        let (authority, path) = match rest.find('/') {
            Some(at) => (
                rest.get(..at).unwrap_or(rest),
                rest.get(at..).unwrap_or(path),
            ),
            None => (rest, path),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (
                host,
                port.parse::<u16>()
                    .ok()
                    .filter(|&port| port != 0)
                    .ok_or_else(|| alloc::format!("{text}: {port} is not a port"))?,
            ),
            None => (authority, DEFAULT_PORT),
        };
        if host.is_empty()
            || !host
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
        {
            return Err(alloc::format!(
                "{text}: {host:?} is not a host name or IPv4 address"
            ));
        }
        let address = if host == "localhost" {
            Some([127, 0, 0, 1])
        } else {
            ipv4(host)
        };
        if !tls && address.is_none_or(|address| address[0] != 127) {
            return Err(alloc::format!(
                "{text}: plain HTTP goes only to this machine; use https:// with ca= for \
                 {host}, or an ssh tunnel to localhost"
            ));
        }
        Ok(Url {
            tls,
            address,
            host: String::from(host),
            port,
            path: String::from(path),
        })
    }
}

/// The files a `https://` destination's TLS takes, as the line names them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsFiles {
    /// The CA the destination's certificate must chain to (PEM).
    pub ca: String,
    /// This machine's certificate, shown when the destination asks (PEM).
    pub cert: Option<String>,
    /// Its private key (PEM, PKCS#8).
    pub key: Option<String>,
}

/// `a.b.c.d`.
fn ipv4(text: &str) -> Option<[u8; 4]> {
    let mut out = [0_u8; 4];
    let mut parts = text.split('.');
    for byte in &mut out {
        *byte = parts.next()?.parse().ok()?;
    }
    parts.next().is_none().then_some(out)
}

/// One destination.
#[derive(Debug, Clone, PartialEq)]
pub struct Destination {
    /// Its name, which names its cursor's file.
    pub name: String,
    /// Where it is.
    pub url: Url,
    /// What it is sent.
    pub format: Format,
    /// Whether the auth facilities' records go too.
    pub auth: bool,
    /// For `https://`, the files its TLS takes.
    pub tls: Option<TlsFiles>,
    /// What goes; everything when `None`.
    pub filter: Option<Expr>,
    /// Which blobs follow the records.
    pub blobs: Blobs,
}

/// Which blobs a destination is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blobs {
    /// None.
    None,
    /// The small parts of a crash: no screenshot, no memory, at most
    /// [`SMALL_BLOB_MAX`].
    Small,
    /// Every one.
    All,
}

impl Blobs {
    /// `blobs=`'s word.
    fn parse(word: &str) -> Result<Blobs, String> {
        match word {
            "none" => Ok(Blobs::None),
            "small" => Ok(Blobs::Small),
            "all" => Ok(Blobs::All),
            other => Err(alloc::format!("blobs={other}: small, all or none")),
        }
    }
}

/// The largest blob `blobs=small` sends.
pub const SMALL_BLOB_MAX: u64 = 1024 * 1024;

/// The parts `blobs=small` never sends, whatever their size.
const LARGE_PARTS: [&str; 2] = ["SCREENSHOT", "MEMORY"];

impl Destination {
    /// Whether `record` goes to it.
    #[must_use]
    pub fn wants(&self, record: &Record) -> bool {
        (self.auth || !is_auth(record))
            && self.filter.as_ref().is_none_or(|expr| expr.matches(record))
    }

    /// Whether the blob a record names in `field` (`BLOB_<PART>`), as
    /// `reference`, follows it to this destination.
    #[must_use]
    pub fn wants_blob(&self, field: &str, reference: &crate::blob::Reference) -> bool {
        let part = field
            .strip_prefix(crate::blob::FIELD_PREFIX)
            .unwrap_or(field);
        match self.blobs {
            Blobs::None => false,
            Blobs::All => true,
            Blobs::Small => !LARGE_PARTS.contains(&part) && reference.size <= SMALL_BLOB_MAX,
        }
    }
}

/// Whether `record` is the auth facilities' (4 and 10) or `auth.service`'s.
#[must_use]
pub fn is_auth(record: &Record) -> bool {
    matches!(record.get_str("SYSLOG_FACILITY"), Some("4" | "10"))
        || ["UNIT", "_SYSTEMD_UNIT"]
            .iter()
            .any(|field| record.get_str(field) == Some("auth.service"))
}

/// Read the configuration.
///
/// # Errors
///
/// The first line that does not read, with its number.
pub fn parse_config(text: &str) -> Result<Vec<Destination>, String> {
    let mut out: Vec<Destination> = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let error = |why: &str| alloc::format!("line {}: {why}", index + 1);
        let (head, filter) = match line.split_once(" where ") {
            Some((head, expr)) => (
                head,
                Some(filter::parse(expr.trim()).map_err(|why| error(&alloc::format!("{why:?}")))?),
            ),
            None => (line, None),
        };
        let words: Vec<&str> = head.split_whitespace().collect();
        let (Some(name), Some(url), Some(format)) = (words.first(), words.get(1), words.get(2))
        else {
            return Err(error(
                "NAME URL FORMAT [auth] [ca=FILE [cert=FILE key=FILE]] [where EXPR]",
            ));
        };
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        {
            return Err(error("a name is letters, digits, - and _"));
        }
        if out.iter().any(|each| each.name == *name) {
            return Err(error("that name is taken"));
        }
        let format = match *format {
            "export" => Format::Export,
            "json" => Format::Json,
            other => return Err(error(&alloc::format!("{other}: export or json"))),
        };
        let default_path = if format == Format::Export {
            "/upload"
        } else {
            "/"
        };
        let mut auth = false;
        let mut blobs = None;
        let (mut ca, mut cert, mut key) = (None, None, None);
        for word in words.get(3..).unwrap_or_default() {
            match word.split_once('=') {
                None if *word == "auth" && !auth => auth = true,
                Some(("ca", file)) if ca.is_none() && !file.is_empty() => {
                    ca = Some(String::from(file));
                }
                Some(("cert", file)) if cert.is_none() && !file.is_empty() => {
                    cert = Some(String::from(file));
                }
                Some(("key", file)) if key.is_none() && !file.is_empty() => {
                    key = Some(String::from(file));
                }
                Some(("blobs", which)) if blobs.is_none() => {
                    blobs = Some(Blobs::parse(which).map_err(|why| error(&why))?);
                }
                _ => {
                    return Err(error(&alloc::format!(
                        "{word}: after the format, auth, ca=, cert=, key=, blobs= or where EXPR"
                    )));
                }
            }
        }
        let url = Url::parse(url, default_path).map_err(|why| error(&why))?;
        let tls = match (url.tls, ca) {
            (true, Some(ca)) => {
                if cert.is_some() != key.is_some() {
                    return Err(error("cert= and key= go together"));
                }
                Some(TlsFiles { ca, cert, key })
            }
            (true, None) => return Err(error("https:// needs ca=FILE")),
            (false, None) if cert.is_none() && key.is_none() => None,
            (false, _) => return Err(error("ca=, cert= and key= are for https://")),
        };
        out.push(Destination {
            name: String::from(*name),
            url,
            format,
            auth,
            tls,
            filter,
            blobs: blobs.unwrap_or(if format == Format::Export {
                Blobs::Small
            } else {
                Blobs::None
            }),
        });
    }
    Ok(out)
}

/// The body of one request: `records` in `format`, and how many of them
/// it holds -- up to [`BATCH`] and [`BATCH_BYTES`], one at least.
#[must_use]
pub fn body(format: Format, records: &[&Record], zone: &Zone) -> (Vec<u8>, usize) {
    let mut out = Vec::new();
    let mut taken = 0;
    for record in records.iter().take(BATCH) {
        let before = out.len();
        format::write(record, format.output(), zone, &mut out);
        if taken > 0 && out.len() > BATCH_BYTES {
            out.truncate(before);
            break;
        }
        taken += 1;
    }
    (out, taken)
}

/// The headers that say which machine an upload is from.
pub const MACHINE_HEADER: &str = "X-Lode-Machine-Id";

/// The sender's host name.
pub const HOST_HEADER: &str = "X-Lode-Hostname";

/// The header that names a blob's id and type: `<64 hex> <type>`.
pub const BLOB_HEADER: &str = "X-Lode-Blob";

/// A blob's content type.
pub const BLOB_CONTENT_TYPE: &str = "application/vnd.ferrix.blob";

/// One `POST` of `body` to `url`, the connection closed after, saying it
/// is from `machine` (32 hex digits, or empty for none) named `host`.
#[must_use]
pub fn request(url: &Url, format: Format, body: &[u8], machine: &str, host: &str) -> Vec<u8> {
    post(
        &url.path,
        url,
        format.content_type(),
        "",
        body,
        machine,
        host,
    )
}

/// One `POST` of a blob, `bytes` of type `kind` named `id`, to `url`'s
/// path with `/blob` after it.
#[must_use]
pub fn blob_request(
    url: &Url,
    id: &crate::blob::BlobId,
    kind: &str,
    bytes: &[u8],
    machine: &str,
    host: &str,
) -> Vec<u8> {
    let path = alloc::format!("{}/blob", url.path.trim_end_matches('/'));
    let extra = alloc::format!(
        "{BLOB_HEADER}: {} {}\r\n",
        crate::blob::hex(id),
        clean(kind, 16)
    );
    post(&path, url, BLOB_CONTENT_TYPE, &extra, bytes, machine, host)
}

/// `text`'s printable ASCII, at most `most` characters.
fn clean(text: &str, most: usize) -> String {
    text.chars()
        .filter(char::is_ascii_graphic)
        .take(most)
        .collect()
}

/// A `POST` of `body` to `path` on `url`'s host, with `extra` headers
/// (each ending `\r\n`).
fn post(
    path: &str,
    url: &Url,
    content_type: &str,
    extra: &str,
    body: &[u8],
    machine: &str,
    host: &str,
) -> Vec<u8> {
    let mut out = alloc::format!(
        "POST {path} HTTP/1.1\r\nHost: {}:{}\r\nUser-Agent: lode-upload\r\nContent-Type: \
         {content_type}\r\nContent-Length: {}\r\n{MACHINE_HEADER}: {}\r\n{HOST_HEADER}: {}\r\n\
         {extra}Connection: close\r\n\r\n",
        url.host,
        url.port,
        body.len(),
        clean(machine, 64),
        clean(host, 64)
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

/// A blob `POST`'s id and type, from its `head`, if `body` is the blob
/// the head names: its SHA-256 the id.
///
/// # Errors
///
/// Why it is not: no or a malformed header, or bytes that are not the id's.
pub fn parse_blob(head: &[u8], body: &[u8]) -> Result<(crate::blob::BlobId, String), String> {
    let value = header(head, BLOB_HEADER).ok_or_else(|| String::from("no X-Lode-Blob"))?;
    let (id, kind) = value.split_once(' ').unwrap_or((value, ""));
    let id = crate::blob::parse_id(id).ok_or_else(|| String::from("a malformed blob id"))?;
    if kind.is_empty() || kind.len() > 16 || !kind.bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        return Err(String::from("a malformed blob type"));
    }
    if crate::blob::id_of(body) != id {
        return Err(String::from("bytes that are not the blob they name"));
    }
    Ok((id, String::from(kind)))
}

/// The status of an HTTP response that begins `bytes`, once its status
/// line is whole.
#[must_use]
pub fn status(bytes: &[u8]) -> Option<u16> {
    let end = bytes.windows(2).position(|pair| pair == b"\r\n")?;
    let line = core::str::from_utf8(bytes.get(..end)?).ok()?;
    let (version, rest) = line.split_once(' ')?;
    if !version.starts_with("HTTP/1.") {
        return None;
    }
    rest.split(' ').next()?.parse().ok()
}

/// A request read by a receiver: where its body begins and how long it
/// is, once its headers are whole.
#[must_use]
pub fn request_head(bytes: &[u8]) -> Option<(usize, usize)> {
    let end = bytes.windows(4).position(|four| four == b"\r\n\r\n")? + 4;
    let head = core::str::from_utf8(bytes.get(..end)?).ok()?;
    let length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    Some((end, length))
}

/// The wait between tries: doubled at each failure from
/// [`BACKOFF_FIRST_MS`] to [`BACKOFF_MAX_MS`], back to none on a success.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Backoff {
    /// The wait after the last failure; 0 after a success.
    pub wait_ms: u64,
}

impl Backoff {
    /// A failure: the wait before the next try.
    pub fn fail(&mut self) -> u64 {
        self.wait_ms = if self.wait_ms == 0 {
            BACKOFF_FIRST_MS
        } else {
            self.wait_ms.saturating_mul(2).min(BACKOFF_MAX_MS)
        };
        self.wait_ms
    }

    /// A success.
    pub fn succeed(&mut self) {
        self.wait_ms = 0;
    }
}

/// How many sequence numbers after `cursor` are missing from `seqnums`
/// (ascending): records gone from the store before they were sent.
#[must_use]
pub fn unsent(cursor: u64, seqnums: impl IntoIterator<Item = u64>) -> u64 {
    let mut last = cursor;
    let mut missing = 0;
    for seq in seqnums {
        if seq > last {
            missing += seq - last - 1;
            last = seq;
        }
    }
    missing
}

/// A cursor file's sequence number.
#[must_use]
pub fn read_cursor(bytes: &[u8]) -> Option<u64> {
    core::str::from_utf8(bytes).ok()?.trim().parse().ok()
}

/// The value of header `name` in a request's head (any case), trimmed.
#[must_use]
pub fn header<'a>(head: &'a [u8], name: &str) -> Option<&'a str> {
    let text = core::str::from_utf8(head).ok()?;
    text.split("\r\n").skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim()
            .eq_ignore_ascii_case(name)
            .then_some(value.trim())
    })
}

/// The method and path of a request's head.
#[must_use]
pub fn request_line(head: &[u8]) -> Option<(&str, &str)> {
    let text = core::str::from_utf8(head).ok()?;
    let line = text.split("\r\n").next()?;
    let mut words = line.split(' ');
    let method = words.next()?;
    let path = words.next()?;
    let version = words.next()?;
    version.starts_with("HTTP/1.").then_some((method, path))
}

/// The records of a Journal Export Format body, as `-o export` writes them
/// and journal-upload posts them: the store's own fields taken back into
/// the record (`__SEQNUM`, the two timestamps, `_BOOT_ID`, `PRIORITY` and
/// `FERRIX_LEVEL`, `_HOSTNAME` as the host), every other field kept as
/// sent.
///
/// # Errors
///
/// Where the body stops reading, with the record's number.
pub fn parse_export(body: &[u8]) -> Result<Vec<Record>, String> {
    let mut out = Vec::new();
    let mut at = 0;
    let mut record: Option<Record> = None;
    while at < body.len() {
        let rest = body.get(at..).unwrap_or_default();
        let end = rest
            .iter()
            .position(|&byte| byte == b'\n')
            .unwrap_or(rest.len());
        let line = rest.get(..end).unwrap_or_default();
        if line.is_empty() {
            if let Some(done) = record.take() {
                out.push(done);
            }
            at += end + 1;
            continue;
        }
        let current = record.get_or_insert_with(|| Record::new(UNSPECIFIED, 0, 0));
        let bad = || alloc::format!("record {}: a field that does not read", out.len() + 1);
        let (name, value, used) = if let Some(equals) = line.iter().position(|&byte| byte == b'=') {
            (
                line.get(..equals).ok_or_else(bad)?,
                line.get(equals + 1..).ok_or_else(bad)?,
                end + 1,
            )
        } else {
            let length_at = end + 1;
            let length = rest
                .get(length_at..length_at + 8)
                .and_then(|bytes| bytes.try_into().ok())
                .map(u64::from_le_bytes)
                .and_then(|length| usize::try_from(length).ok())
                .ok_or_else(bad)?;
            let value = rest
                .get(length_at + 8..(length_at + 8).checked_add(length).ok_or_else(bad)?)
                .ok_or_else(bad)?;
            if rest.get(length_at + 8 + length) != Some(&b'\n') {
                return Err(bad());
            }
            (line, value, length_at + 8 + length + 1)
        };
        let name = core::str::from_utf8(name).map_err(|_| bad())?;
        take_own(current, name, value).map_err(|()| bad())?;
        at += used;
    }
    if let Some(done) = record.take() {
        out.push(done);
    }
    Ok(out)
}

/// One field of a received record: the store's own taken back into the
/// record, any other kept.
fn take_own(record: &mut Record, name: &str, value: &[u8]) -> Result<(), ()> {
    let text = || core::str::from_utf8(value).map_err(|_| ());
    let number = || text().and_then(|text| text.parse::<u64>().map_err(|_| ()));
    match name {
        "__CURSOR" => {}
        "__SEQNUM" => record.seqnum = number()?,
        "__REALTIME_TIMESTAMP" => record.realtime_us = number()?,
        "__MONOTONIC_TIMESTAMP" => record.monotonic_us = number()?,
        "_BOOT_ID" => record.boot_id = crate::segment::parse_id(text()?).ok_or(())?,
        "PRIORITY" => {
            let level = number()?;
            record.priority = u8::try_from(level)
                .ok()
                .filter(|&level| level <= 7)
                .ok_or(())?;
        }
        "FERRIX_LEVEL" => {
            record.priority = crate::record::level(text()?).ok_or(())?;
        }
        "_HOSTNAME" => record.host = alloc::rc::Rc::from(text()?),
        name if name.starts_with("__") => {}
        name if crate::record::valid_name(name) => record.push(name, value),
        _ => return Err(()),
    }
    Ok(())
}

/// The records of a body of journald JSON, one object a line, as `-o json`
/// writes them: every value a string, or an array of byte values.
///
/// # Errors
///
/// The first line that does not read, with its number.
pub fn parse_json_lines(body: &[u8]) -> Result<Vec<Record>, String> {
    let mut out = Vec::new();
    for (index, line) in body.split(|&byte| byte == b'\n').enumerate() {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let bad = || alloc::format!("line {}: not one object of strings", index + 1);
        let mut record = Record::new(UNSPECIFIED, 0, 0);
        let mut json = Json { bytes: line, at: 0 };
        json.expect(b'{').ok_or_else(bad)?;
        if json.peek() == Some(b'}') {
            json.at += 1;
        } else {
            loop {
                let name = json.string().ok_or_else(bad)?;
                json.expect(b':').ok_or_else(bad)?;
                let value = json.value().ok_or_else(bad)?;
                let name = String::from_utf8(name).map_err(|_| bad())?;
                take_own(&mut record, &name, &value).map_err(|()| bad())?;
                match json.next() {
                    Some(b',') => {}
                    Some(b'}') => break,
                    _ => return Err(bad()),
                }
            }
        }
        if json.peek().is_some() {
            return Err(bad());
        }
        out.push(record);
    }
    Ok(out)
}

/// A reader of the JSON journald writes: objects of strings and byte arrays.
struct Json<'a> {
    /// The line.
    bytes: &'a [u8],
    /// Where it is.
    at: usize,
}

impl Json<'_> {
    /// The next byte that is not white space, not taken.
    fn peek(&mut self) -> Option<u8> {
        while self.bytes.get(self.at).is_some_and(u8::is_ascii_whitespace) {
            self.at += 1;
        }
        self.bytes.get(self.at).copied()
    }

    /// The next byte that is not white space, taken.
    fn next(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.at += 1;
        Some(byte)
    }

    /// Take `byte` or fail.
    fn expect(&mut self, byte: u8) -> Option<()> {
        (self.next()? == byte).then_some(())
    }

    /// A value: a string, or an array of numbers 0 to 255.
    fn value(&mut self) -> Option<Vec<u8>> {
        if self.peek()? != b'[' {
            return self.string();
        }
        self.at += 1;
        let mut out = Vec::new();
        if self.peek()? == b']' {
            self.at += 1;
            return Some(out);
        }
        loop {
            let start = self.at;
            while self.bytes.get(self.at).is_some_and(u8::is_ascii_digit) {
                self.at += 1;
            }
            let digits = core::str::from_utf8(self.bytes.get(start..self.at)?).ok()?;
            out.push(digits.parse::<u8>().ok()?);
            match self.next()? {
                b',' => {
                    let _ = self.peek()?;
                }
                b']' => return Some(out),
                _ => return None,
            }
        }
    }

    /// A string, its escapes undone.
    fn string(&mut self) -> Option<Vec<u8>> {
        self.expect(b'"')?;
        let mut out = Vec::new();
        loop {
            let byte = *self.bytes.get(self.at)?;
            self.at += 1;
            match byte {
                b'"' => return Some(out),
                b'\\' => {
                    let escaped = *self.bytes.get(self.at)?;
                    self.at += 1;
                    match escaped {
                        b'"' | b'\\' | b'/' => out.push(escaped),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'u' => {
                            let hex =
                                core::str::from_utf8(self.bytes.get(self.at..self.at + 4)?).ok()?;
                            self.at += 4;
                            let code = u32::from_str_radix(hex, 16).ok()?;
                            let c = char::from_u32(code)?;
                            let mut buf = [0_u8; 4];
                            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                        }
                        _ => return None,
                    }
                }
                _ => out.push(byte),
            }
        }
    }
}
