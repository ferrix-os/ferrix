//! `lode-receive`: the uploads of Ferrix machines taken in over TLS and
//! kept, a lode store for each machine (`docs/LOGGING.md` §10.3).
//!
//! ```text
//! <root>/<machine-id>/      one machine's store, as lode lays out its own:
//!                           system@<boot>-<seq>.fxj and .sum beside them
//! <root>/host-<hostname>/   a machine with no machine id
//! <root>/<dir>/machine      hostname=, last_seen=, paired=
//! <root>/.pki/              the CA and certificates (crate::pki)
//! <root>/.receive.lock      held while `lode-receive run` is
//! ```
//!
//! A blob (`docs/LOGGING.md` §21.4) comes as a `POST` of its own, its
//! SHA-256 checked against the id it is sent as, and is kept in that
//! machine's blob container, `<root>/<dir>/blobs/`, once.
//!
//! A record is kept once. The store's highest sequence number and its
//! boots say which records it has, so an upload sent again after a lost
//! answer stores nothing twice; a machine whose store began again (a lower
//! number in a boot never seen) is taken as new. The answer is `200` only
//! after the records are synced.

use std::collections::HashMap;
use std::fs;
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::os::fd::AsRawFd as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferrix_lode_core::record::Record;
use ferrix_lode_core::upload;

use crate::pki::Pki;
use crate::store::{BlobWriter, Writer};

/// The default port, journal-remote's.
pub const PORT: u16 = upload::DEFAULT_PORT;

/// The largest body taken.
const BODY_MAX: usize = 64 * 1024 * 1024;

/// How long a connection may be silent.
const PATIENCE: Duration = Duration::from_secs(30);

/// One machine's store, open.
#[derive(Debug)]
struct Machine {
    /// Its writer.
    writer: Writer,
    /// The highest sequence number it holds.
    highest: u64,
    /// The boots it holds.
    boots: Vec<[u8; 16]>,
}

/// The receiver's state across connections.
#[derive(Debug)]
pub struct Receiver {
    /// The root.
    root: PathBuf,
    /// Its certificates.
    pki: Pki,
    /// The machines open, by directory name.
    machines: Mutex<HashMap<String, Machine>>,
    /// Their blob containers, by directory name.
    blobs: Mutex<HashMap<String, BlobWriter>>,
}

impl Receiver {
    /// A receiver keeping its stores under `root`.
    #[must_use]
    pub fn new(root: &Path) -> Receiver {
        Receiver {
            root: root.to_owned(),
            pki: Pki::at(root),
            machines: Mutex::new(HashMap::new()),
            blobs: Mutex::new(HashMap::new()),
        }
    }

    /// Take one upload: `head` and `body` of a request, from a client whose
    /// certificate's SHA-256 is `client` (`None` for one without). The
    /// status to answer and a line for the log.
    pub fn take(&self, head: &[u8], body: &[u8], client: Option<&str>) -> (u16, String) {
        let Some(("POST", _)) = upload::request_line(head) else {
            return (405, String::from("not a POST"));
        };
        let paired = match client {
            Some(fingerprint) => match self
                .pki
                .clients()
                .into_iter()
                .find(|(_, each)| each == fingerprint)
            {
                Some((name, _)) => Some(name),
                None => {
                    return (
                        403,
                        String::from("a certificate no machine was paired with"),
                    );
                }
            },
            None => None,
        };
        let kind = upload::header(head, "Content-Type").unwrap_or_default();
        if kind.starts_with(upload::BLOB_CONTENT_TYPE) {
            return self.take_blob(head, body, paired.as_deref());
        }
        let records = match kind {
            kind if kind.starts_with("application/vnd.fdo.journal") => upload::parse_export(body),
            kind if kind.starts_with("application/x-ndjson")
                || kind.starts_with("application/json") =>
            {
                upload::parse_json_lines(body)
            }
            other => return (415, format!("content type {other:?}")),
        };
        let records = match records {
            Ok(records) => records,
            Err(why) => return (400, why),
        };
        let (dir, machine_id, host, label) = match self.machine(head, paired.as_deref()) {
            Ok(found) => found,
            Err(answer) => return answer,
        };
        let Ok(mut machines) = self.machines.lock() else {
            return (500, String::from("the receiver's state is poisoned"));
        };
        let machine = match machines.entry(dir.clone()) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                match Writer::open(&self.root.join(&dir), machine_id) {
                    Ok(writer) => {
                        let (highest, boots) = writer.known();
                        entry.insert(Machine {
                            writer,
                            highest,
                            boots,
                        })
                    }
                    Err(error) => return (503, format!("{dir}: {error}")),
                }
            }
        };
        let fresh: Vec<Record> = records
            .into_iter()
            .filter(|record| {
                record.seqnum > machine.highest || !machine.boots.contains(&record.boot_id)
            })
            .collect();
        let count = fresh.len();
        let host_name = if host.is_empty() {
            dir.clone()
        } else {
            host.clone()
        };
        if let Err(error) = machine.writer.append(&fresh, &host_name) {
            return (503, format!("{dir}: {error}"));
        }
        for record in &fresh {
            machine.highest = machine.highest.max(record.seqnum);
            if !machine.boots.contains(&record.boot_id) {
                machine.boots.push(record.boot_id);
            }
        }
        let text = format!(
            "hostname={host_name}\nlast_seen={}\npaired={}\n",
            rfc3339(crate::store::now_us()),
            paired.as_deref().unwrap_or_default()
        );
        let _ = crate::store::replace(&label, text.as_bytes());
        (200, format!("{dir}: {count} record(s) kept"))
    }

    /// Which machine a request is from: its directory, its machine id, its
    /// host name and its label's path; the answer to give when it may not
    /// send (a machine paired under another name) or names none.
    #[allow(
        clippy::type_complexity,
        reason = "four parts of one answer, used once each"
    )]
    fn machine(
        &self,
        head: &[u8],
        paired: Option<&str>,
    ) -> Result<(String, [u8; 16], String, PathBuf), (u16, String)> {
        let host = clean(upload::header(head, upload::HOST_HEADER).unwrap_or_default());
        let id = upload::header(head, upload::MACHINE_HEADER).unwrap_or_default();
        let (dir, machine_id) = match ferrix_lode_core::segment::parse_id(id) {
            Some(machine_id) if id.len() == 32 => (id.to_ascii_lowercase(), machine_id),
            _ if !host.is_empty() => (format!("host-{host}"), [0; 16]),
            _ => match paired {
                Some(name) => (format!("host-{name}"), [0; 16]),
                None => return Err((400, String::from("no machine id and no host name"))),
            },
        };
        let label = self.root.join(&dir).join("machine");
        if let Ok(text) = fs::read_to_string(&label)
            && let Some(was) = text.lines().find_map(|line| line.strip_prefix("paired="))
            && Some(was) != paired
            && !was.is_empty()
        {
            return Err((403, format!("{dir} was paired as {was}")));
        }
        Ok((dir, machine_id, host, label))
    }

    /// Take one blob: kept in its machine's container once its bytes are
    /// the id's, and synced before the `200`.
    fn take_blob(&self, head: &[u8], body: &[u8], paired: Option<&str>) -> (u16, String) {
        let (id, kind) = match upload::parse_blob(head, body) {
            Ok(found) => found,
            Err(why) => return (400, why),
        };
        let (dir, _, _, _) = match self.machine(head, paired) {
            Ok(found) => found,
            Err(answer) => return answer,
        };
        let Ok(mut blobs) = self.blobs.lock() else {
            return (500, String::from("the receiver's state is poisoned"));
        };
        let writer = match blobs.entry(dir.clone()) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                match BlobWriter::open(&self.root.join(&dir)) {
                    Ok(writer) => entry.insert(writer),
                    Err(error) => return (503, format!("{dir}: {error}")),
                }
            }
        };
        let had = writer.has(&id);
        if let Err(error) = writer.keep(&id, &kind, body) {
            return (503, format!("{dir}: {error}"));
        }
        let id = ferrix_lode_core::blob::hex(&id);
        if had {
            (200, format!("{dir}: blob {id} held already"))
        } else {
            (200, format!("{dir}: blob {id} kept, {} bytes", body.len()))
        }
    }
}

/// A host name as a directory name: letters, digits, `-` and `.`, at most
/// 64; anything else dropped.
fn clean(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.'))
        .take(64)
        .collect::<String>()
        .trim_start_matches('.')
        .to_owned()
}

/// Microseconds since 1970 as RFC 3339, UTC.
#[must_use]
pub fn rfc3339(us: u64) -> String {
    let at = ferrix_lode_core::time::Civil::at(us, 0);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        at.year, at.month, at.day, at.hour, at.minute, at.second
    )
}

/// Serve on `listener` for ever: each connection on a thread of its own.
pub fn serve(receiver: &Arc<Receiver>, listener: &TcpListener, tls: &Arc<rustls::ServerConfig>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            continue;
        };
        let receiver = Arc::clone(receiver);
        let tls = Arc::clone(tls);
        let _ = std::thread::Builder::new()
            .name(String::from("lode-receive"))
            .spawn(move || {
                let peer = stream
                    .peer_addr()
                    .map_or_else(|_| String::from("?"), |address| address.to_string());
                match connection(&receiver, stream, tls) {
                    Ok(line) => println!("lode-receive: {peer}: {line}"),
                    Err(why) => println!("lode-receive: {peer}: {why}"),
                }
            });
    }
}

/// One connection: TLS, one request, one answer.
fn connection(
    receiver: &Receiver,
    stream: TcpStream,
    tls: Arc<rustls::ServerConfig>,
) -> Result<String, String> {
    stream
        .set_read_timeout(Some(PATIENCE))
        .and_then(|()| stream.set_write_timeout(Some(PATIENCE)))
        .map_err(|error| error.to_string())?;
    let connection = rustls::ServerConnection::new(tls).map_err(|error| error.to_string())?;
    let mut stream = rustls::StreamOwned::new(connection, stream);
    let mut request = Vec::new();
    let mut chunk = [0_u8; 16 * 1024];
    let (start, length) = loop {
        if let Some((start, length)) = upload::request_head(&request) {
            if length > BODY_MAX {
                let _ = answer(&mut stream, 413);
                return Err(format!("a body of {length} bytes"));
            }
            if request.len() >= start + length {
                break (start, length);
            }
        } else if request.len() > 64 * 1024 {
            return Err(String::from("a head that does not end"));
        }
        match stream.read(&mut chunk) {
            Ok(0) => return Err(String::from("closed mid-request")),
            Ok(got) => request.extend_from_slice(chunk.get(..got).unwrap_or_default()),
            Err(error) => return Err(format!("TLS: {error}")),
        }
    };
    let client = stream
        .conn
        .peer_certificates()
        .and_then(<[_]>::first)
        .map(|cert| crate::tls::fingerprint(cert));
    let head = request.get(..start).unwrap_or_default();
    let body = request.get(start..start + length).unwrap_or_default();
    let (status, line) = receiver.take(head, body, client.as_deref());
    answer(&mut stream, status).map_err(|error| format!("answering: {error}"))?;
    stream.conn.send_close_notify();
    let _ = std::io::Write::flush(&mut stream);
    Ok(format!("{status} {line}"))
}

/// Answer `status`, and close.
fn answer(stream: &mut impl std::io::Write, status: u16) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        405 => "Method Not Allowed",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        _ => "Service Unavailable",
    };
    stream.write_all(
        format!("HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .as_bytes(),
    )?;
    stream.flush()
}

/// The lock `run` holds while it runs, with what it says of itself.
#[derive(Debug)]
pub struct Lock {
    /// The file, locked.
    _file: fs::File,
}

/// Take `<root>/.receive.lock`, saying `pid` and `listen` in it; `None` if
/// another receiver holds it.
///
/// # Errors
///
/// A lock file that cannot be made.
pub fn lock(root: &Path, listen: &str) -> std::io::Result<Option<Lock>> {
    fs::create_dir_all(root)?;
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(".receive.lock"))?;
    // SAFETY: a descriptor this function owns; no pointers.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Ok(None);
    }
    file.set_len(0)?;
    write!(file, "pid={}\nlisten={listen}\n", std::process::id())?;
    file.sync_all()?;
    Ok(Some(Lock { _file: file }))
}

/// Whether a receiver holds `<root>/.receive.lock`, and what it says.
#[must_use]
pub fn holder(root: &Path) -> Option<(u32, String)> {
    let path = root.join(".receive.lock");
    let file = fs::File::open(&path).ok()?;
    // SAFETY: a descriptor this function owns; no pointers.
    let free = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0;
    if free {
        return None;
    }
    let text = fs::read_to_string(&path).ok()?;
    let field = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(name))
            .map(str::to_owned)
    };
    Some((field("pid=")?.parse().ok()?, field("listen=")?))
}
