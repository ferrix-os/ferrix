//! `lode-upload`: the log sent on to collectors (`docs/LOGGING.md` §10.2),
//! as `lode-upload.service`.
//!
//! The store is the buffer: for each destination it reads what follows that
//! destination's cursor, sends it, and moves the cursor only on the
//! destination's `2xx`. A destination that is down is tried again after
//! 1 s, 2, 4, ... up to 5 min, for ever. What the bytes are -- the
//! configuration, the request, the answer, the back-off, what was gone
//! before it was sent -- is `ferrix_lode_core::upload`'s; this is the
//! reading, the sockets, TLS and the waiting.
//!
//! Blobs (`docs/LOGGING.md` §21.4) follow on a second stream a
//! destination at a time, behind the records: only when its records are
//! caught up, only blobs of records it has acknowledged, one `POST` each,
//! with a cursor of its own (`<name>.blobs`). A destination that answers
//! a blob 404, 405 or 415 takes none, and is sent records alone.
//!
//! It starts as root, reads its keys and makes its state directory, then
//! becomes uid [`UID`] in `adm`, which may read the log and nothing else of
//! it.

use std::net::{TcpStream, ToSocketAddrs as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use std::collections::VecDeque;

use ferrix_lode_core::blob::{self, BlobId, Reference};
use ferrix_lode_core::record::Record;
use ferrix_lode_core::segment;
use ferrix_lode_core::time::Zone;
use ferrix_lode_core::upload::{self, Backoff, Destination};
use rustls::pki_types::ServerName;

use crate::store::{self, Segment};

/// The uploader's own user.
pub const UID: u32 = 92;

/// `adm`, whose members read the log.
const ADM: u32 = 4;

/// The store's directories.
const STORES: [&str; 2] = ["/var/log/journal", "/run/log/journal"];

/// The most records read ahead of a destination's cursor.
const WINDOW: u64 = 5000;

/// How long a connection or an answer may take.
const PATIENCE: Duration = Duration::from_secs(10);

/// How long an idle pass waits before looking again.
const IDLE: Duration = Duration::from_millis(500);

/// The least time between two busy passes: a backlog is sent at most a
/// batch per destination this often, and a log that grows without pause
/// is not read in a loop that never sleeps (ef-load, 2026-11-07).
const BUSY_FLOOR: Duration = Duration::from_millis(50);

/// Where a reader has got to in the store.
#[derive(Debug, Default)]
struct Reader {
    /// Each segment being written, and the offset its next entry is at.
    places: Vec<(String, usize)>,
    /// The highest sequence number read.
    high: u64,
    /// Every segment's name and size when the reader last caught up, so an
    /// idle pass reads nothing.
    seen: Vec<(String, u64)>,
    /// Each sealed segment's name and the last sequence number its summary
    /// gives: a sealed segment never changes, so its summary is read once
    /// and not at every pass.
    sealed: Vec<(String, u64)>,
    /// The store's segments, listed again only when a directory changed.
    listing: store::Listing,
}

impl Reader {
    /// Append to `out`, in order, the records after `high` and at most
    /// [`WINDOW`] past it.
    fn more(&mut self, dirs: &[PathBuf], out: &mut Vec<Record>) {
        let segments = self.listing.segments(dirs).to_vec();
        // A sealed segment's records were read when it was first seen
        // sealed, and it gains none: it is not looked at again, its size
        // given as `u64::MAX` (none a file has) so that the sealing of a
        // segment still counts as a change.
        let sizes: Vec<(String, u64)> = segments
            .iter()
            .map(|each| {
                let sealed = self.sealed.iter().any(|(name, _)| *name == each.name);
                (
                    each.name.clone(),
                    if sealed { u64::MAX } else { each.size() },
                )
            })
            .collect();
        if sizes == self.seen {
            return;
        }
        let top = self.high.saturating_add(WINDOW);
        let mut fresh: Vec<Record> = Vec::new();
        let mut cut = false;
        self.sealed
            .retain(|(name, _)| sizes.iter().any(|(each, _)| each == name));
        for each in &segments {
            let last = self.sealed_last(each);
            if last.is_some_and(|last| last <= self.high) {
                continue;
            }
            let (records, place) = self.read(each, last.is_some());
            let mut over = false;
            for record in records {
                if record.seqnum > top {
                    over = true;
                } else if record.seqnum > self.high {
                    fresh.push(record);
                }
            }
            if let Some(place) = place
                && !over
            {
                self.places.retain(|(name, _)| *name != each.name);
                self.places.push((each.name.clone(), place));
            }
            cut |= over;
        }
        self.places
            .retain(|(name, _)| sizes.iter().any(|(each, _)| each == name));
        fresh.sort_by_key(|record| record.seqnum);
        fresh.dedup_by_key(|record| record.seqnum);
        if let Some(last) = fresh.last() {
            self.high = last.seqnum;
        }
        // What this pass found sealed is seen as sealed, so that the next
        // pass with nothing new reads nothing.
        let mut sizes = sizes;
        for (name, size) in &mut sizes {
            if self.sealed.iter().any(|(each, _)| each == name) {
                *size = u64::MAX;
            }
        }
        self.seen = if cut { Vec::new() } else { sizes };
        out.extend(fresh);
    }

    /// The last sequence number of `each` if it is sealed, from its
    /// summary, which is read only the first time.
    fn sealed_last(&mut self, each: &Segment) -> Option<u64> {
        if let Some((_, last)) = self.sealed.iter().find(|(name, _)| *name == each.name) {
            return Some(*last);
        }
        let last = each.summary()?.seqs.1;
        self.sealed.push((each.name.clone(), last));
        Some(last)
    }

    /// A segment's records: a sealed one's whole, a written one's from
    /// where the last read stopped, with the place after them.
    fn read(&self, each: &Segment, sealed: bool) -> (Vec<Record>, Option<usize>) {
        if sealed {
            return (each.records(), None);
        }
        let Some(header) = each.header() else {
            return (Vec::new(), None);
        };
        let from = self
            .places
            .iter()
            .find(|(name, _)| *name == each.name)
            .map_or_else(|| header.encode().len(), |(_, place)| *place);
        let Some(bytes) = each.bytes_from(from) else {
            return (Vec::new(), None);
        };
        let (records, used) = segment::entries(&bytes);
        let host: std::rc::Rc<str> = std::rc::Rc::from(header.host.as_str());
        let records = records
            .into_iter()
            .map(|mut record| {
                record.boot_id = header.boot_id;
                record.host = std::rc::Rc::clone(&host);
                record
            })
            .collect();
        (records, Some(from + used))
    }
}

/// One destination and where it stands.
#[derive(Debug)]
struct Upload {
    /// It.
    destination: Destination,
    /// Its TLS, for `https://`.
    tls: Option<Arc<rustls::ClientConfig>>,
    /// The last record it acknowledged.
    cursor: Option<u64>,
    /// Where its cursor is kept.
    path: PathBuf,
    /// Its reader.
    reader: Reader,
    /// What was read and is not yet acknowledged, in order.
    pending: Vec<Record>,
    /// The wait after failures.
    backoff: Backoff,
    /// No try before this.
    next_try: Instant,
    /// Its blob stream.
    blobs: BlobStream,
}

/// The blobs that follow one destination's records.
#[derive(Debug)]
struct BlobStream {
    /// The last record whose blobs it was sent.
    cursor: u64,
    /// Where that is kept.
    path: PathBuf,
    /// Its reader, behind the records'.
    reader: Reader,
    /// The blobs to send, in order, each with its record's sequence number.
    queue: VecDeque<(u64, Reference)>,
    /// Records its reader read past what the records' stream had sent, kept
    /// for when that stream has sent them.
    held: Vec<Record>,
    /// The last record whose blobs are queued.
    through: u64,
    /// Blobs it was sent, the newest [`SENT_MAX`], not sent again.
    sent: VecDeque<BlobId>,
    /// Set when it answered that it takes no blobs.
    off: bool,
    /// The wait after failures.
    backoff: Backoff,
    /// No try before this.
    next_try: Instant,
}

impl BlobStream {
    /// The records through `acknowledged` its reader has not given before:
    /// those it holds from an earlier read first, then, unless those already
    /// reach past `acknowledged`, more from the store. What is read past
    /// `acknowledged` is held for a later call. The reader goes on from
    /// where it stopped: it was set back to `acknowledged`, and so read the
    /// segment being written from its start, at every call while the log
    /// grew (ef-load, 2026-11-07).
    fn through_acknowledged(&mut self, dirs: &[PathBuf], acknowledged: u64) -> Vec<Record> {
        let mut records = core::mem::take(&mut self.held);
        if records
            .last()
            .is_none_or(|record| record.seqnum <= acknowledged)
        {
            self.reader.more(dirs, &mut records);
        }
        let (now, later) = records
            .into_iter()
            .partition(|record| record.seqnum <= acknowledged);
        self.held = later;
        now
    }
}

/// How many blob ids a destination remembers as sent.
const SENT_MAX: usize = 4096;

/// This machine, as uploads name it.
#[derive(Debug, Clone, Default)]
struct Identity {
    /// `/etc/machine-id`, 32 hex digits, or empty.
    machine: String,
    /// Its host name.
    host: String,
}

/// `lode-upload [--config FILE]`.
#[must_use]
pub fn run(args: &[String]) -> i32 {
    let config = match args {
        [] => PathBuf::from(upload::CONFIG),
        [flag, path] if flag == "--config" => PathBuf::from(path),
        _ => {
            say("usage: lode-upload [--config FILE]");
            return 2;
        }
    };
    let Ok(text) = std::fs::read_to_string(&config) else {
        say(&format!("no {}: nothing to send", config.display()));
        return 0;
    };
    let destinations = match upload::parse_config(&text) {
        Ok(destinations) if destinations.is_empty() => {
            say(&format!(
                "{} names no destination: nothing to send",
                config.display()
            ));
            return 0;
        }
        Ok(destinations) => destinations,
        Err(why) => {
            say(&format!("{}: {why}", config.display()));
            return 1;
        }
    };
    // Keys are read as root, before the uploader becomes its own user.
    let mut uploads = Vec::new();
    for destination in destinations {
        let tls = match &destination.tls {
            Some(files) => {
                let identity = files
                    .cert
                    .as_deref()
                    .zip(files.key.as_deref())
                    .map(|(cert, key)| (Path::new(cert), Path::new(key)));
                match crate::tls::client(Path::new(&files.ca), identity) {
                    Ok(config) => Some(config),
                    Err(why) => {
                        say(&format!("{}: {why}", destination.name));
                        return 1;
                    }
                }
            }
            None => None,
        };
        let path = Path::new(upload::STATE).join(&destination.name);
        uploads.push((destination, tls, path));
    }
    if let Err(why) = become_uploader() {
        say(&why);
        return 1;
    }
    let identity = Identity {
        machine: std::fs::read_to_string("/etc/machine-id")
            .map(|text| text.trim().to_owned())
            .ok()
            .filter(|id| id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .unwrap_or_default(),
        host: std::fs::read_to_string("/proc/sys/kernel/hostname")
            .map(|text| text.trim().to_owned())
            .unwrap_or_default(),
    };
    let dirs: Vec<PathBuf> = STORES.iter().map(PathBuf::from).collect();
    let mut uploads: Vec<Upload> = uploads
        .into_iter()
        .map(|(destination, tls, path)| {
            let cursor = std::fs::read(&path)
                .ok()
                .and_then(|bytes| upload::read_cursor(&bytes));
            let blob_path = Path::new(upload::STATE).join(format!("{}.blobs", destination.name));
            let blob_cursor = std::fs::read(&blob_path)
                .ok()
                .and_then(|bytes| upload::read_cursor(&bytes))
                .or(cursor)
                .unwrap_or(0);
            say(&format!(
                "sending to {} ({}://{}:{}{}), {}",
                destination.name,
                if destination.url.tls { "https" } else { "http" },
                destination.url.host,
                destination.url.port,
                destination.url.path,
                cursor.map_or_else(
                    || String::from("from the oldest record held"),
                    |seq| format!("after record {seq}")
                )
            ));
            Upload {
                reader: Reader {
                    high: cursor.unwrap_or(0),
                    ..Reader::default()
                },
                destination,
                tls,
                cursor,
                path,
                pending: Vec::new(),
                backoff: Backoff::default(),
                next_try: Instant::now(),
                blobs: BlobStream {
                    cursor: blob_cursor,
                    path: blob_path,
                    reader: Reader {
                        high: blob_cursor,
                        ..Reader::default()
                    },
                    queue: VecDeque::new(),
                    held: Vec::new(),
                    through: blob_cursor,
                    sent: VecDeque::new(),
                    off: false,
                    backoff: Backoff::default(),
                    next_try: Instant::now(),
                },
            }
        })
        .collect();
    let zone = Zone::utc();
    loop {
        let mut busy = false;
        for each in &mut uploads {
            let more = pass(each, &dirs, &zone, &identity);
            busy |= more;
            // Blobs only behind caught-up records: a screenshot never
            // delays a line.
            if !more && each.pending.is_empty() {
                busy |= pass_blobs(each, &dirs, &identity);
            }
        }
        std::thread::sleep(if busy { BUSY_FLOOR } else { IDLE });
    }
}

/// Make the state directory, then become [`UID`] in `adm`.
fn become_uploader() -> Result<(), String> {
    // SAFETY: no arguments; it reads the process's own uid.
    if unsafe { libc::geteuid() } != 0 {
        return Ok(());
    }
    std::fs::create_dir_all(upload::STATE)
        .map_err(|error| format!("{}: {error}", upload::STATE))?;
    std::os::unix::fs::chown(upload::STATE, Some(UID), Some(ADM))
        .map_err(|error| format!("{}: {error}", upload::STATE))?;
    // SAFETY: an empty group list, then ids; no pointers but the empty list.
    let dropped = unsafe {
        libc::setgroups(0, std::ptr::null()) == 0
            && libc::setresgid(ADM, ADM, ADM) == 0
            && libc::setresuid(UID, UID, UID) == 0
    };
    if dropped {
        Ok(())
    } else {
        Err(format!(
            "becoming uid {UID}: {}",
            std::io::Error::last_os_error()
        ))
    }
}

/// One destination's turn: read, send what is due, move its cursor.
/// Whether there is more to do at once.
fn pass(each: &mut Upload, dirs: &[PathBuf], zone: &Zone, identity: &Identity) -> bool {
    let now = Instant::now();
    if now < each.next_try {
        return false;
    }
    if each.pending.is_empty() {
        let before = each.reader.high;
        each.reader.more(dirs, &mut each.pending);
        let base = match each.cursor {
            Some(_) => before,
            None => each
                .pending
                .first()
                .map_or(0, |first| first.seqnum.saturating_sub(1)),
        };
        let missing = upload::unsent(base, each.pending.iter().map(|record| record.seqnum));
        if missing > 0 {
            tell_unsent(&each.destination.name, missing, base, each.pending.first());
        }
    }
    let Some(last) = each.pending.last().map(|record| record.seqnum) else {
        return false;
    };
    let slice = each
        .pending
        .get(..upload::BATCH.min(each.pending.len()))
        .unwrap_or_default();
    let wanted: Vec<&Record> = slice
        .iter()
        .filter(|record| each.destination.wants(record))
        .collect();
    let (end, sent) = if wanted.is_empty() {
        (slice.len(), Ok(()))
    } else {
        let (body, taken) = upload::body(each.destination.format, &wanted, zone);
        let end = if taken == wanted.len() {
            slice.len()
        } else {
            let through = wanted
                .get(taken.saturating_sub(1))
                .map_or(0, |record| record.seqnum);
            slice
                .iter()
                .take_while(|record| record.seqnum <= through)
                .count()
        };
        let request = upload::request(
            &each.destination.url,
            each.destination.format,
            &body,
            &identity.machine,
            &identity.host,
        );
        let sent = send(&each.destination, each.tls.as_ref(), &request);
        (end, sent)
    };
    match sent {
        Ok(()) => {
            let through = each
                .pending
                .get(end.saturating_sub(1))
                .map_or(last, |record| record.seqnum);
            if store::replace(&each.path, format!("{through}\n").as_bytes()).is_err() {
                say(&format!(
                    "{}: the cursor could not be kept; sent again after a restart",
                    each.destination.name
                ));
            }
            if each.backoff.wait_ms > 0 {
                say(&format!("{}: sending again", each.destination.name));
            }
            each.backoff.succeed();
            each.cursor = Some(through);
            let _ = each.pending.drain(..end);
            !each.pending.is_empty()
        }
        Err(why) => {
            let wait = each.backoff.fail();
            each.next_try = now + Duration::from_millis(wait);
            say(&format!(
                "{}: {why}; trying again in {} s",
                each.destination.name,
                wait / 1000
            ));
            false
        }
    }
}

/// One turn of a destination's blob stream: queue the blobs of the
/// records it has acknowledged, or send the next one. Whether there is more
/// to do at once.
fn pass_blobs(each: &mut Upload, dirs: &[PathBuf], identity: &Identity) -> bool {
    let stream = &mut each.blobs;
    if each.destination.blobs == upload::Blobs::None || stream.off {
        return false;
    }
    let now = Instant::now();
    if now < stream.next_try {
        return false;
    }
    let acknowledged = each.cursor.unwrap_or(0);
    if stream.queue.is_empty() {
        if stream.through >= acknowledged {
            return false;
        }
        let records = stream.through_acknowledged(dirs, acknowledged);
        let mut through = stream.through;
        for record in records
            .iter()
            .filter(|record| record.seqnum > stream.through && record.seqnum <= acknowledged)
        {
            through = through.max(record.seqnum);
            if !each.destination.wants(record) {
                continue;
            }
            for (field, reference) in blob::references(record) {
                if each.destination.wants_blob(&field, &reference) {
                    stream.queue.push_back((record.seqnum, reference));
                }
            }
        }
        stream.through = through;
        if stream.queue.is_empty() {
            keep_blob_cursor(&each.destination.name, stream, through);
            return false;
        }
    }
    let Some((seqnum, reference)) = stream.queue.front().cloned() else {
        return false;
    };
    if !stream.sent.contains(&reference.id) {
        let Some((kind, bytes)) = store::blob(dirs, &reference.id) else {
            say(&format!(
                "{}: blob {} of record {seqnum} is not in the store (expired or missing); not sent",
                each.destination.name,
                blob::hex(&reference.id)
            ));
            let _ = stream.queue.pop_front();
            return true;
        };
        let request = upload::blob_request(
            &each.destination.url,
            &reference.id,
            &kind,
            &bytes,
            &identity.machine,
            &identity.host,
        );
        match send(&each.destination, each.tls.as_ref(), &request) {
            Ok(()) => {
                stream.backoff.succeed();
                if stream.sent.len() >= SENT_MAX {
                    let _ = stream.sent.pop_front();
                }
                stream.sent.push_back(reference.id);
            }
            Err(why)
                if ["answered 404", "answered 405", "answered 415"].contains(&why.as_str()) =>
            {
                say(&format!(
                    "{}: {why} to a blob: it takes no blobs; records are sent alone",
                    each.destination.name
                ));
                stream.off = true;
                return false;
            }
            Err(why) => {
                let wait = stream.backoff.fail();
                stream.next_try = now + Duration::from_millis(wait);
                say(&format!(
                    "{}: a blob: {why}; trying again in {} s",
                    each.destination.name,
                    wait / 1000
                ));
                return false;
            }
        }
    }
    let _ = stream.queue.pop_front();
    if stream.queue.is_empty() {
        let through = stream.through;
        keep_blob_cursor(&each.destination.name, stream, through);
    }
    true
}

/// The blob stream sent everything of the records through `through`.
fn keep_blob_cursor(name: &str, stream: &mut BlobStream, through: u64) {
    stream.cursor = through;
    if store::replace(&stream.path, format!("{through}\n").as_bytes()).is_err() {
        say(&format!(
            "{name}: the blob cursor could not be kept; blobs sent again after a restart"
        ));
    }
}

/// One request to `destination`, over `tls` for `https://`: `Ok` on a
/// `2xx`.
///
/// # Errors
///
/// A connection, handshake, write or read that failed, or another status.
pub fn send(
    destination: &Destination,
    tls: Option<&Arc<rustls::ClientConfig>>,
    request: &[u8],
) -> Result<(), String> {
    let url = &destination.url;
    let stream = connect(&url.host, url.port)?;
    stream
        .set_read_timeout(Some(PATIENCE))
        .and_then(|()| stream.set_write_timeout(Some(PATIENCE)))
        .map_err(|error| format!("connecting: {error}"))?;
    match tls {
        Some(config) => {
            let name = ServerName::try_from(url.host.clone())
                .map_err(|error| format!("{}: {error}", url.host))?;
            let connection = rustls::ClientConnection::new(Arc::clone(config), name)
                .map_err(|error| format!("TLS: {error}"))?;
            exchange(rustls::StreamOwned::new(connection, stream), request)
        }
        None => exchange(stream, request),
    }
}

/// A connection to `host`'s first address that takes one, each tried in
/// the order the resolver gave them: `localhost` is `::1` before
/// `127.0.0.1` on many machines, and a collector may listen on only one.
fn connect(host: &str, port: u16) -> Result<TcpStream, String> {
    let addresses = (host, port)
        .to_socket_addrs()
        .map_err(|error| format!("{host}: {error}"))?;
    connect_any(host, addresses)
}

/// [`connect`], to `addresses` in turn.
fn connect_any(
    host: &str,
    addresses: impl IntoIterator<Item = std::net::SocketAddr>,
) -> Result<TcpStream, String> {
    let mut last = None;
    for address in addresses {
        match TcpStream::connect_timeout(&address, PATIENCE) {
            Ok(stream) => return Ok(stream),
            Err(error) => last = Some(error),
        }
    }
    Err(last.map_or_else(
        || format!("{host}: no address"),
        |error| format!("connecting: {error}"),
    ))
}

/// Write `request` on `stream` and read the answer's status: `Ok` on a
/// `2xx`.
fn exchange(mut stream: impl std::io::Read + std::io::Write, request: &[u8]) -> Result<(), String> {
    stream
        .write_all(request)
        .and_then(|()| stream.flush())
        .map_err(|error| format!("sending: {error}"))?;
    let mut answer = Vec::new();
    let mut chunk = [0_u8; 512];
    loop {
        if let Some(code) = upload::status(&answer) {
            return if (200..300).contains(&code) {
                Ok(())
            } else {
                Err(format!("answered {code}"))
            };
        }
        match stream.read(&mut chunk) {
            Ok(0) => return Err(String::from("closed before it answered")),
            Ok(got) => answer.extend_from_slice(chunk.get(..got).unwrap_or_default()),
            Err(error) => return Err(format!("no answer: {error}")),
        }
        if answer.len() > 64 * 1024 {
            return Err(String::from("an answer that is not HTTP"));
        }
    }
}

/// Say in the log, as a record of its own (`MESSAGE_ID` and `N_UNSENT`,
/// through journald's native socket), that `missing` records after `base`
/// were gone from the store before they were sent to `name`.
fn tell_unsent(name: &str, missing: u64, base: u64, first: Option<&Record>) {
    let message = format!(
        "{missing} record(s) after record {base} were gone from the store before they were \
         sent to {name} (vacuum, lode delete, or a RAM-only class at a reboot)"
    );
    say(&message);
    let datagram = format!(
        "PRIORITY=4\nSYSLOG_IDENTIFIER=lode-upload\nMESSAGE_ID={}\nN_UNSENT={missing}\n\
         UPLOAD_DESTINATION={name}\nUNSENT_AFTER_SEQNUM={base}\nUNSENT_BEFORE_SEQNUM={}\n\
         MESSAGE={message}\n",
        upload::MESSAGE_ID_UNSENT,
        first.map_or(0, |record| record.seqnum)
    );
    if let Ok(socket) = std::os::unix::net::UnixDatagram::unbound() {
        let _ = socket.send_to(datagram.as_bytes(), "/run/systemd/journal/socket");
    }
}

/// A line on standard output, which init's log pipe brings to the log.
fn say(text: &str) {
    println!("lode upload: {text}");
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::*;

    /// A record of the boot the tests' segments are of.
    fn record(seqnum: u64) -> Record {
        let mut record = Record::new(6, seqnum, seqnum);
        record.seqnum = seqnum;
        record.boot_id = [1; 16];
        record
    }

    /// A store read as it is written, sealed and followed by a new segment
    /// gives every record once, in order; a written segment is read from
    /// where the last read stopped; a sealed one's summary is kept, its
    /// size no longer looked at.
    #[test]
    fn a_store_read_as_it_grows_and_seals_gives_each_record_once() {
        let dir = std::env::temp_dir().join(format!("lode-upload-reader-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let dirs = [dir.clone()];
        let mut writer = store::Writer::open(&dir, [7; 16]).unwrap();
        let mut reader = Reader::default();
        let mut got = Vec::new();
        writer.append(&[record(1), record(2)], "box").unwrap();
        reader.more(&dirs, &mut got);
        writer.append(&[record(3)], "box").unwrap();
        reader.more(&dirs, &mut got);
        reader.more(&dirs, &mut got);
        writer.seal().unwrap();
        reader.more(&dirs, &mut got);
        let first = store::segments(&dirs).pop().unwrap();
        assert_eq!(reader.sealed, [(first.name.clone(), 3)]);
        assert_eq!(reader.seen, [(first.name.clone(), u64::MAX)]);
        writer.append(&[record(4), record(5)], "box").unwrap();
        reader.more(&dirs, &mut got);
        let seqs: Vec<u64> = got.iter().map(|record| record.seqnum).collect();
        assert_eq!(seqs, [1, 2, 3, 4, 5]);
        assert_eq!(reader.sealed.len(), 1);
        // The directory was read again only when a segment (or a summary)
        // was added: at the first read, the seal, and the second segment.
        let reads = reader.listing.reads;
        reader.more(&dirs, &mut got);
        reader.more(&dirs, &mut got);
        assert_eq!(reader.listing.reads, reads);
        writer.append(&[record(6)], "box").unwrap();
        reader.more(&dirs, &mut got);
        assert_eq!(reader.listing.reads, reads);
        assert_eq!(got.last().map(|record| record.seqnum), Some(6));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The blob stream's reader, behind the records': each call gives the
    /// records through what was acknowledged, once each, holds what it read
    /// past, and goes on from where it stopped rather than reading the
    /// written segment again from its start.
    #[test]
    fn the_blob_reader_holds_what_it_read_past_and_reads_on() {
        let dir = std::env::temp_dir().join(format!("lode-upload-blobs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let dirs = [dir.clone()];
        let mut writer = store::Writer::open(&dir, [7; 16]).unwrap();
        let mut stream = BlobStream {
            cursor: 0,
            path: dir.join("cursor"),
            reader: Reader::default(),
            queue: VecDeque::new(),
            held: Vec::new(),
            through: 0,
            sent: VecDeque::new(),
            off: false,
            backoff: Backoff::default(),
            next_try: Instant::now(),
        };
        let seqs = |records: &[Record]| records.iter().map(|r| r.seqnum).collect::<Vec<u64>>();
        writer
            .append(&(1..=6).map(record).collect::<Vec<_>>(), "box")
            .unwrap();
        assert_eq!(seqs(&stream.through_acknowledged(&dirs, 3)), [1, 2, 3]);
        assert_eq!(seqs(&stream.held), [4, 5, 6]);
        let place = stream.reader.places.clone();
        assert!(!place.is_empty());
        // Nothing new acknowledged past the held ones: no read at all.
        assert_eq!(seqs(&stream.through_acknowledged(&dirs, 5)), [4, 5]);
        assert_eq!(stream.reader.places, place);
        writer.append(&[record(7), record(8)], "box").unwrap();
        assert_eq!(seqs(&stream.through_acknowledged(&dirs, 8)), [6, 7, 8]);
        assert!(stream.held.is_empty());
        assert_eq!(stream.reader.high, 8);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A written segment's tail read from an offset is the same bytes as
    /// the whole file read and cut there.
    #[test]
    fn a_tail_read_from_an_offset_is_the_whole_file_cut_there() {
        let dir = std::env::temp_dir().join(format!("lode-upload-tail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut writer = store::Writer::open(&dir, [7; 16]).unwrap();
        writer
            .append(&[record(1), record(2), record(3)], "box")
            .unwrap();
        let segment = store::segments(std::slice::from_ref(&dir)).pop().unwrap();
        let whole = segment.bytes(0).unwrap();
        for offset in [0, 1, whole.len() / 2, whole.len(), whole.len() + 10] {
            assert_eq!(
                segment.bytes_from(offset),
                segment.bytes(offset),
                "at {offset}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `localhost` as many machines resolve it, `::1` first, to a collector
    /// listening on `127.0.0.1` alone: the second address takes it.
    #[test]
    fn an_address_that_refuses_is_passed_for_the_next() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let addresses = [
            std::net::SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port)),
            std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port)),
        ];
        let stream = connect_any("localhost", addresses).unwrap();
        assert_eq!(stream.peer_addr().unwrap(), addresses[1]);
        assert!(connect_any("localhost", addresses[..1].to_vec()).is_err());
        assert_eq!(
            connect_any("nowhere", []).unwrap_err(),
            "nowhere: no address"
        );
    }
}
