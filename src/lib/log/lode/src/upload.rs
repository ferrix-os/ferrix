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

use alloc::string::String;
use alloc::vec::Vec;

use crate::filter::{self, Expr};
use crate::format::{self, Output};
use crate::record::Record;
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

/// An `http://` address on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    /// Its IPv4 address.
    pub address: [u8; 4],
    /// Its host as written, for `Host:`.
    pub host: String,
    /// Its port.
    pub port: u16,
    /// Its path, beginning with `/`.
    pub path: String,
}

impl Url {
    /// Read `http://HOST[:PORT][/PATH]`. HOST is `localhost` or an IPv4
    /// address in `127.0.0.0/8`; `path` stands in for a missing PATH.
    ///
    /// # Errors
    ///
    /// What is wrong with it, a host off this machine included.
    pub fn parse(text: &str, path: &str) -> Result<Url, String> {
        let rest = text.strip_prefix("http://").ok_or_else(|| {
            alloc::format!(
                "{text}: only http:// is built (TLS is not yet); reach a collector \
                 elsewhere through an ssh tunnel to localhost"
            )
        })?;
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
        let address = if host == "localhost" {
            [127, 0, 0, 1]
        } else {
            ipv4(host).ok_or_else(|| alloc::format!("{text}: {host} is not an IPv4 address"))?
        };
        if address[0] != 127 {
            return Err(alloc::format!(
                "{text}: plain HTTP goes only to this machine; reach {host} through an ssh \
                 tunnel to localhost (TLS is not built yet)"
            ));
        }
        Ok(Url {
            address,
            host: String::from(host),
            port,
            path: String::from(path),
        })
    }
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
    /// What goes; everything when `None`.
    pub filter: Option<Expr>,
}

impl Destination {
    /// Whether `record` goes to it.
    #[must_use]
    pub fn wants(&self, record: &Record) -> bool {
        (self.auth || !is_auth(record))
            && self.filter.as_ref().is_none_or(|expr| expr.matches(record))
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
            return Err(error("NAME URL FORMAT [auth] [where EXPR]"));
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
        let auth = match words.get(3..).unwrap_or_default() {
            [] => false,
            ["auth"] => true,
            _ => return Err(error("after the format: auth, or where EXPR")),
        };
        out.push(Destination {
            name: String::from(*name),
            url: Url::parse(url, default_path).map_err(|why| error(&why))?,
            format,
            auth,
            filter,
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

/// One `POST` of `body` to `url`, the connection closed after.
#[must_use]
pub fn request(url: &Url, format: Format, body: &[u8]) -> Vec<u8> {
    let mut out = alloc::format!(
        "POST {} HTTP/1.1\r\nHost: {}:{}\r\nUser-Agent: lode-upload\r\nContent-Type: {}\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        url.path,
        url.host,
        url.port,
        format.content_type(),
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
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
