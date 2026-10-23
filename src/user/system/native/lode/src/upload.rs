//! `lode upload`: the log sent on to collectors (`docs/LOGGING.md` §10.2),
//! as `lode-upload.service`, and `lode upload-stub`, a collector for tests.
//!
//! The store is the buffer: for each destination it reads what follows
//! that destination's cursor, sends it, and moves the cursor only on the
//! destination's `2xx`. What the bytes are -- the configuration, the
//! request, the answer, the back-off, what was gone before it was sent --
//! is `ferrix_lode_core::upload`'s; this is the reading, the sockets and
//! the waiting. It starts as root to make its state directory, then is
//! uid [`UID`] in `adm`, which may read the log and nothing else of it.

use alloc::string::String;
use alloc::vec::Vec;

use ferrix_lode_core::record::Record;
use ferrix_lode_core::segment;
use ferrix_lode_core::time::Zone;
use ferrix_lode_core::upload::{self, Backoff, Destination};

use crate::cli::{self, Known};
use crate::store::{self, Dir};
use crate::{sys, zone};

/// The uploader's own user.
pub(crate) const UID: u32 = 92;

/// `adm`, whose members read the log.
const ADM: u32 = 4;

/// The most records read ahead of a destination's cursor.
const WINDOW: u64 = 5000;

/// How long an answer may take.
const ANSWER_SECONDS: usize = 10;

/// How long an idle pass waits before looking again.
const IDLE_MS: u64 = 500;

/// Where a reader has got to in the store.
#[derive(Default)]
struct Reader {
    /// Each segment being written, and the offset its next entry is at.
    places: Vec<(String, usize)>,
    /// The highest sequence number read.
    high: u64,
    /// Every segment's name and size when the reader last caught up, so an
    /// idle pass reads nothing.
    seen: Vec<(String, u64)>,
}

impl Reader {
    /// Append to `out`, in order, the records after `high` and at most
    /// [`WINDOW`] past it.
    fn more(&mut self, dirs: &[Dir], out: &mut Vec<Record>) {
        let segments = cli::known(dirs);
        let sizes: Vec<(String, u64)> = segments
            .iter()
            .map(|each| (each.segment.name.clone(), store::size(&each.segment)))
            .collect();
        if sizes == self.seen {
            return;
        }
        let top = self.high.saturating_add(WINDOW);
        let mut fresh: Vec<Record> = Vec::new();
        let mut cut = false;
        for each in &segments {
            if each.last_seq() <= self.high {
                continue;
            }
            let (records, place) = self.read(each);
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
                self.places.retain(|(name, _)| *name != each.segment.name);
                self.places.push((each.segment.name.clone(), place));
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
        self.seen = if cut { Vec::new() } else { sizes };
        out.extend(fresh);
    }

    /// A segment's records: a sealed one's whole, a written one's from
    /// where the last read stopped, with the place after them.
    fn read(&self, each: &Known) -> (Vec<Record>, Option<usize>) {
        if each.summary.is_some() {
            return (cli::records_of(each), None);
        }
        let from = self
            .places
            .iter()
            .find(|(name, _)| *name == each.segment.name)
            .map_or_else(|| each.header.encode().len(), |(_, place)| *place);
        let Some(bytes) = store::bytes(&each.segment, from) else {
            return (Vec::new(), None);
        };
        let (records, used) = segment::entries(&bytes);
        let host: alloc::rc::Rc<str> = alloc::rc::Rc::from(each.header.host.as_str());
        let records = records
            .into_iter()
            .map(|mut record| {
                record.boot_id = each.header.boot_id;
                record.host = alloc::rc::Rc::clone(&host);
                record
            })
            .collect();
        (records, Some(from + used))
    }
}

/// One destination and where it stands.
struct Upload {
    /// It.
    destination: Destination,
    /// The last record it acknowledged.
    cursor: Option<u64>,
    /// Where its cursor is kept.
    path: Vec<u8>,
    /// Its reader.
    reader: Reader,
    /// What was read and is not yet acknowledged, in order.
    pending: Vec<Record>,
    /// The wait after failures.
    backoff: Backoff,
    /// No try before this (monotonic microseconds).
    next_try_us: u64,
}

/// `lode upload [--config FILE]`.
pub(crate) fn run(words: &[String]) -> i32 {
    let config = match words {
        [] => String::from(upload::CONFIG),
        [flag, path] if flag == "--config" => path.clone(),
        _ => return cli::usage("upload [--config FILE]"),
    };
    let Some(text) = sys::read_small(config.as_bytes()) else {
        say(&alloc::format!("no {config}: nothing to send"));
        return 0;
    };
    let destinations = match upload::parse_config(&String::from_utf8_lossy(&text)) {
        Ok(destinations) if destinations.is_empty() => {
            say(&alloc::format!(
                "{config} names no destination: nothing to send"
            ));
            return 0;
        }
        Ok(destinations) => destinations,
        Err(why) => {
            say(&alloc::format!("{config}: {why}"));
            return 1;
        }
    };
    if let Err(why) = become_uploader() {
        say(&why);
        return 1;
    }
    let dirs: Vec<Dir> = [store::PERSISTENT, store::VOLATILE]
        .iter()
        .filter_map(|path| Dir::open(path).ok())
        .collect();
    if dirs.is_empty() {
        say("the log cannot be read: neither /var/log/journal nor /run/log/journal");
        return 1;
    }
    let mut uploads: Vec<Upload> = destinations
        .into_iter()
        .map(|destination| {
            let path = alloc::format!("{}/{}", upload::STATE, destination.name).into_bytes();
            let cursor = sys::read_small(&path).and_then(|bytes| upload::read_cursor(&bytes));
            say(&alloc::format!(
                "sending to {} ({}:{}{}), {}",
                destination.name,
                destination.url.host,
                destination.url.port,
                destination.url.path,
                cursor.map_or_else(
                    || String::from("from the oldest record held"),
                    |seq| alloc::format!("after record {seq}")
                )
            ));
            Upload {
                reader: Reader {
                    high: cursor.unwrap_or(0),
                    ..Reader::default()
                },
                destination,
                cursor,
                path,
                pending: Vec::new(),
                backoff: Backoff::default(),
                next_try_us: 0,
            }
        })
        .collect();
    let zone = zone::reader();
    loop {
        let mut busy = false;
        for each in &mut uploads {
            busy |= pass(each, &dirs, &zone);
        }
        if !busy {
            sys::sleep_ms(IDLE_MS);
        }
    }
}

/// Make the state directory, then become [`UID`] in `adm`.
fn become_uploader() -> Result<(), String> {
    if sys::getuid() != 0 {
        return Ok(());
    }
    for path in [
        &b"/var/lib"[..],
        b"/var/lib/ferrix",
        upload::STATE.as_bytes(),
    ] {
        sys::mkdirat(sys::AT_FDCWD, path, 0o755).map_err(|error| {
            alloc::format!("{}: errno {}", String::from_utf8_lossy(path), error.0)
        })?;
    }
    sys::chown(upload::STATE.as_bytes(), UID, ADM)
        .and_then(|()| sys::become_ids(UID, ADM))
        .map_err(|error| alloc::format!("becoming uid {UID}: errno {}", error.0))
}

/// One destination's turn: read, send what is due, move its cursor.
/// Whether there is more to do at once.
fn pass(each: &mut Upload, dirs: &[Dir], zone: &Zone) -> bool {
    let now = sys::clock_us(sys::CLOCK_MONOTONIC);
    if now < each.next_try_us {
        return false;
    }
    if each.pending.is_empty() {
        let before = each.reader.high;
        let start = each.pending.len();
        each.reader.more(dirs, &mut each.pending);
        let read = each.pending.get(start..).unwrap_or_default();
        let base = match each.cursor {
            Some(_) => before,
            None => read
                .first()
                .map_or(0, |first| first.seqnum.saturating_sub(1)),
        };
        let missing = upload::unsent(base, read.iter().map(|record| record.seqnum));
        if missing > 0 {
            tell_unsent(&each.destination.name, missing, base, read.first());
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
            let through = wanted.get(taken - 1).map_or(0, |record| record.seqnum);
            slice
                .iter()
                .take_while(|record| record.seqnum <= through)
                .count()
        };
        (end, send(&each.destination, &body))
    };
    match sent {
        Ok(()) => {
            let through = each
                .pending
                .get(end.saturating_sub(1))
                .map_or(last, |record| record.seqnum);
            if sys::replace_file(&each.path, alloc::format!("{through}\n").as_bytes()).is_err() {
                say(&alloc::format!(
                    "{}: the cursor could not be kept; sent again after a restart",
                    each.destination.name
                ));
            }
            if each.backoff.wait_ms > 0 {
                say(&alloc::format!("{}: sending again", each.destination.name));
            }
            each.backoff.succeed();
            each.cursor = Some(through);
            let _ = each.pending.drain(..end);
            !each.pending.is_empty()
        }
        Err(why) => {
            let wait = each.backoff.fail();
            each.next_try_us = now + wait * 1000;
            say(&alloc::format!(
                "{}: {why}; trying again in {} s",
                each.destination.name,
                wait / 1000
            ));
            false
        }
    }
}

/// One request: `Ok` on a `2xx`.
fn send(destination: &Destination, body: &[u8]) -> Result<(), String> {
    let url = &destination.url;
    let fd = sys::tcp_connect(url.address, url.port, ANSWER_SECONDS)
        .map_err(|error| alloc::format!("connecting: errno {}", error.0))?;
    let request = upload::request(url, destination.format, body);
    let result = sys::write_all(fd, &request)
        .map_err(|error| alloc::format!("sending: errno {}", error.0))
        .and_then(|()| {
            let mut answer = Vec::new();
            let mut chunk = [0_u8; 512];
            loop {
                if let Some(code) = upload::status(&answer) {
                    return if (200..300).contains(&code) {
                        Ok(())
                    } else {
                        Err(alloc::format!("answered {code}"))
                    };
                }
                match sys::read(fd, &mut chunk) {
                    Ok(0) => return Err(String::from("closed before it answered")),
                    Ok(got) => answer.extend_from_slice(chunk.get(..got).unwrap_or_default()),
                    Err(error) => return Err(alloc::format!("no answer: errno {}", error.0)),
                }
                if answer.len() > 64 * 1024 {
                    return Err(String::from("an answer that is not HTTP"));
                }
            }
        });
    sys::close(fd);
    result
}

/// Say in the log, as a record of its own, that `missing` records after
/// `base` were gone before they were sent.
fn tell_unsent(name: &str, missing: u64, base: u64, first: Option<&Record>) {
    let message = alloc::format!(
        "{missing} record(s) after record {base} were gone from the store before they were \
         sent to {name} (vacuum, lode delete, or a RAM-only class at a reboot)"
    );
    say(&message);
    let Ok(fd) = cli::connect_datagram(b"/run/systemd/journal/socket") else {
        return;
    };
    let datagram = alloc::format!(
        "PRIORITY=4\nSYSLOG_IDENTIFIER=lode-upload\nMESSAGE_ID={}\nN_UNSENT={missing}\n\
         UPLOAD_DESTINATION={name}\nUNSENT_AFTER_SEQNUM={base}\nUNSENT_BEFORE_SEQNUM={}\n\
         MESSAGE={message}\n",
        upload::MESSAGE_ID_UNSENT,
        first.map_or(0, |record| record.seqnum)
    );
    let _ = sys::write(fd, datagram.as_bytes());
    sys::close(fd);
}

/// A line on standard output, which init's log pipe brings to the log.
fn say(text: &str) {
    cli::out(alloc::format!("lode upload: {text}\n").as_bytes());
}

/// `lode upload-stub PORT FILE`: a collector on `127.0.0.1:PORT` that
/// appends each request's body to FILE and answers 200 -- or 503 while
/// `FILE.refuse` exists, keeping nothing. A test peer.
pub(crate) fn stub(words: &[String]) -> i32 {
    let [port, file] = words else {
        return cli::usage("upload-stub PORT FILE");
    };
    let Ok(port) = port.parse::<u16>() else {
        return cli::usage("upload-stub PORT FILE");
    };
    let listener = match sys::tcp_listen(port) {
        Ok(fd) => fd,
        Err(error) => {
            cli::complain(&alloc::format!(
                "lode: upload-stub: port {port}: errno {}",
                error.0
            ));
            return 1;
        }
    };
    cli::out(alloc::format!("upload-stub: listening on 127.0.0.1:{port}\n").as_bytes());
    let refuse = alloc::format!("{file}.refuse").into_bytes();
    loop {
        let Ok(fd) = sys::accept_blocking(listener) else {
            continue;
        };
        let mut request = Vec::new();
        let mut chunk = [0_u8; 8192];
        let whole = loop {
            if let Some((start, length)) = upload::request_head(&request)
                && request.len() >= start + length
            {
                break request.get(start..start + length).map(Vec::from);
            }
            match sys::read(fd, &mut chunk) {
                Ok(0) | Err(_) => break None,
                Ok(got) => request.extend_from_slice(chunk.get(..got).unwrap_or_default()),
            }
        };
        let answer: &[u8] = match whole {
            Some(_) if sys::read_small(&refuse).is_some() => {
                b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n"
            }
            Some(body) => {
                let kept = sys::openat(
                    sys::AT_FDCWD,
                    file.as_bytes(),
                    sys::O_WRONLY | sys::O_CREAT | sys::O_APPEND | sys::O_CLOEXEC,
                    0o644,
                )
                .and_then(|out| {
                    let written = sys::write_all(out, &body);
                    sys::close(out);
                    written
                });
                if kept.is_ok() {
                    b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"
                } else {
                    b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n"
                }
            }
            None => b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n",
        };
        let _ = sys::write_all(fd, answer);
        sys::close(fd);
    }
}
