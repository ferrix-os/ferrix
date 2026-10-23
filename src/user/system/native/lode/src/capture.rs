//! Crash screenshots (`docs/LOGGING.md` §19, step 2b): lode's side of the
//! provider protocol ([`ferrix_lode_capture`]).
//!
//! Off unless `/etc/ferrix/lode.d/capture.conf` says `Capture=yes`. When on,
//! lode listens on `/run/lode/capture` (anyone may connect; only a provider
//! that is the session's announced pid and holds its token is accepted),
//! and for each crash record -- a kernel `signal` line that ended a
//! process, a `devmgr` line saying a driver died, or a record a `capture
//! where EXPR` line of the file matches -- asks the seat's provider for a
//! picture, at most one each [`SPACING_US`]. The record is stored first and
//! never waits: the answer, or its absence, is a record of its own, linked
//! by `CAPTURE_OF`, and an image is a file beside the journal, readable by
//! exactly those who read the journal.

use alloc::string::String;
use alloc::vec::Vec;

use ferrix_lode_capture::{FORMAT_QOI, Message};
use ferrix_lode_core::filter::{self, Expr};
use ferrix_lode_core::ingest::{self, Now};
use ferrix_lode_core::record::Record;
use ferrix_lode_core::session::Sessions;

use crate::store::Dir;
use crate::sys;

/// The provider socket.
const SOCKET: &[u8] = b"/run/lode/capture";
/// The switch and the rules.
const CONFIG: &[u8] = b"/etc/ferrix/lode.d/capture.conf";
/// At most one capture each 10 s.
const SPACING_US: u64 = 10_000_000;
/// How long a provider has to answer.
const PATIENCE_US: u64 = 2_000_000;
/// Time-outs in a row before a provider is dropped.
const TIMEOUTS_MAX: u32 = 3;
/// The most the captures may take on disk.
const KEPT_MAX: u64 = 256 << 20;
/// How long a capture is kept, in seconds.
const KEPT_S: u64 = 7 * 86_400;
/// `MESSAGE_ID` of a record about a capture.
pub(crate) const MESSAGE_ID_CAPTURE: &str = "6c0a8e4f2d1b4a39b7e5c3a1f9d7b5e3";

/// A capture asked for and not answered.
struct Pending {
    /// Its request number.
    request: u32,
    /// The crash record's sequence number.
    seqnum: u64,
    /// When it is given up on, monotonic microseconds.
    deadline: u64,
}

/// One connection.
struct Provider {
    /// Its socket.
    fd: usize,
    /// What it has sent of a frame so far.
    buf: Vec<u8>,
    /// What it called itself, once registered.
    name: Option<String>,
    /// The seat it was accepted for.
    seat: Option<String>,
    /// The capture it owes.
    pending: Option<Pending>,
    /// Time-outs in a row.
    timeouts: u32,
}

/// The switch, the socket, the providers.
pub(crate) struct Capture {
    /// Whether captures are on.
    on: bool,
    /// The listening socket.
    listener: Option<usize>,
    /// The captures' directory.
    dir: Option<usize>,
    /// The boot, for file names.
    boot: String,
    /// The connections.
    providers: Vec<Provider>,
    /// `capture where EXPR` lines.
    rules: Vec<Expr>,
    /// When the last capture was asked for.
    last: Option<u64>,
    /// The next request's number.
    next: u32,
}

impl Capture {
    /// Read the switch, and when on make the socket and the directory, as
    /// root, beside `journal` (the store's own directory's path).
    pub(crate) fn open(journal: &[u8], boot_id: &[u8; 16]) -> Capture {
        let mut on = false;
        let mut rules = Vec::new();
        if let Some(bytes) = sys::read_small(CONFIG) {
            for line in String::from_utf8_lossy(&bytes).lines() {
                let line = line
                    .split_once('#')
                    .map_or(line, |(before, _)| before)
                    .trim();
                if let Some(value) = line.strip_prefix("Capture=") {
                    on = matches!(value.trim(), "yes" | "true" | "on" | "1");
                } else if let Some(expr) = line.strip_prefix("capture where ")
                    && let Ok(expr) = filter::parse(expr)
                {
                    rules.push(expr);
                }
            }
        }
        let mut boot = String::new();
        ferrix_lode_core::segment::push_hex(&mut boot, boot_id);
        let mut capture = Capture {
            on,
            listener: None,
            dir: None,
            boot,
            providers: Vec::new(),
            rules,
            last: None,
            next: 1,
        };
        if !on {
            return capture;
        }
        capture.listener = sys::listen_at(SOCKET).ok();
        if capture.listener.is_some() {
            let _ = sys::chmod(SOCKET, 0o666);
        }
        let mut path = Vec::from(journal);
        path.extend_from_slice(b"/captures");
        if sys::mkdirat(sys::AT_FDCWD, &path, 0o2750).is_ok()
            && let Ok(dir) = Dir::open(&path)
        {
            let _ = sys::fchown(dir.fd, crate::serve::UID, crate::serve::GID);
            let _ = sys::fchmod(dir.fd, 0o2750);
            capture.dir = Some(dir.fd);
        }
        capture
    }

    /// Every descriptor to wait on.
    pub(crate) fn fds(&self) -> Vec<usize> {
        self.listener
            .iter()
            .copied()
            .chain(self.providers.iter().map(|provider| provider.fd))
            .collect()
    }

    /// Whether `record` asks for a picture.
    pub(crate) fn wants(&self, record: &Record) -> bool {
        self.on
            && (record.get_str("MESSAGE_ID") == Some(ingest::MESSAGE_ID_CRASH)
                || (record.get("FERRIX_SUBSYSTEM") == Some(b"devmgr")
                    && filter::contains(b"died", record.message()))
                || self.rules.iter().any(|rule| rule.matches(record)))
    }

    /// Ask for a picture of the screen at record `seqnum`, `what` being
    /// one line of it; what is to be said goes into `out`.
    pub(crate) fn ask(&mut self, seqnum: u64, what: &str, now: Now, out: &mut Vec<Record>) {
        if self
            .last
            .is_some_and(|last| now.monotonic_us.saturating_sub(last) < SPACING_US)
        {
            out.push(said(
                now,
                seqnum,
                "skipped",
                "another capture was taken less than 10 s before",
                None,
            ));
            return;
        }
        let request = self.next;
        let Some(provider) = self
            .providers
            .iter_mut()
            .find(|provider| provider.seat.is_some() && provider.pending.is_none())
        else {
            out.push(said(
                now,
                seqnum,
                "none",
                "no screenshot provider is registered",
                None,
            ));
            return;
        };
        self.next = self.next.wrapping_add(1);
        self.last = Some(now.monotonic_us);
        let frame = Message::Capture {
            request,
            seqnum,
            what: String::from(what),
        }
        .encode();
        if sys::write_some(provider.fd, &frame) != frame.len() {
            out.push(said(
                now,
                seqnum,
                "timed-out",
                "the provider would not take the request",
                provider.name.as_deref(),
            ));
            return;
        }
        provider.pending = Some(Pending {
            request,
            seqnum,
            deadline: now.monotonic_us + PATIENCE_US,
        });
    }

    /// Accept, read, answer; give up on late answers. What is to be said
    /// goes into `out`.
    pub(crate) fn serve(&mut self, now: Now, sessions: &Sessions, out: &mut Vec<Record>) {
        if let Some(listener) = self.listener {
            while let Some(fd) = sys::accept(listener) {
                self.providers.push(Provider {
                    fd,
                    buf: Vec::new(),
                    name: None,
                    seat: None,
                    pending: None,
                    timeouts: 0,
                });
            }
        }
        let mut kept = Vec::new();
        for mut provider in core::mem::take(&mut self.providers) {
            let open = self.read(&mut provider, now, sessions, out);
            if let Some(pending) = &provider.pending
                && now.monotonic_us > pending.deadline
            {
                out.push(said(
                    now,
                    pending.seqnum,
                    "timed-out",
                    "the provider did not answer within 2 s",
                    provider.name.as_deref(),
                ));
                provider.pending = None;
                provider.timeouts += 1;
            }
            if open && provider.timeouts < TIMEOUTS_MAX {
                kept.push(provider);
            } else {
                sys::close(provider.fd);
            }
        }
        self.providers = kept;
    }

    /// Read what `provider` sent and act on each frame; whether it stays.
    fn read(
        &mut self,
        provider: &mut Provider,
        now: Now,
        sessions: &Sessions,
        out: &mut Vec<Record>,
    ) -> bool {
        let mut chunk = alloc::vec![0_u8; 64 * 1024];
        loop {
            match sys::read(provider.fd, &mut chunk) {
                Ok(0) => return false,
                Ok(got) => provider
                    .buf
                    .extend_from_slice(chunk.get(..got).unwrap_or_default()),
                Err(error) if error.0 == sys::EINTR => {}
                Err(_) => break,
            }
        }
        loop {
            match Message::decode(&provider.buf) {
                Ok(None) => return true,
                Err(_) => return false,
                Ok(Some((message, used))) => {
                    let _ = provider.buf.drain(..used);
                    if !self.take(provider, message, now, sessions, out) {
                        return false;
                    }
                }
            }
        }
    }

    /// One frame from `provider`; whether it stays.
    fn take(
        &mut self,
        provider: &mut Provider,
        message: Message,
        now: Now,
        sessions: &Sessions,
        out: &mut Vec<Record>,
    ) -> bool {
        match message {
            Message::Register {
                name, seat, token, ..
            } => {
                let peer = sys::peer(provider.fd);
                let holder = sessions.on_seat(&seat);
                let proven = matches!((peer, holder), (Some((pid, _)), Some(session)) if session.pid == pid && session.token == token);
                let answer = if proven {
                    provider.name = Some(name);
                    provider.seat = Some(seat);
                    Message::Accepted {
                        formats: FORMAT_QOI,
                    }
                } else {
                    Message::Refused {
                        why: String::from("not the session's compositor, or not its token"),
                    }
                };
                let frame = answer.encode();
                let _ = sys::write_some(provider.fd, &frame);
                proven
            }
            Message::Image {
                request,
                format,
                width,
                height,
                bytes,
            } => {
                let Some(pending) = provider
                    .pending
                    .take_if(|pending| pending.request == request)
                else {
                    return false;
                };
                out.push(self.keep(
                    now,
                    &pending,
                    provider.name.as_deref(),
                    (format, width, height),
                    &bytes,
                ));
                provider.timeouts = 0;
                true
            }
            Message::Declined { request, why } => {
                if let Some(pending) = provider
                    .pending
                    .take_if(|pending| pending.request == request)
                {
                    out.push(said(
                        now,
                        pending.seqnum,
                        "declined",
                        &why,
                        provider.name.as_deref(),
                    ));
                }
                provider.timeouts = 0;
                true
            }
            Message::Unregister => false,
            Message::Accepted { .. } | Message::Refused { .. } | Message::Capture { .. } => false,
        }
    }

    /// Write an image beside the journal, and the record that links it.
    fn keep(
        &self,
        now: Now,
        pending: &Pending,
        provider: Option<&str>,
        (format, width, height): (u32, u32, u32),
        bytes: &[u8],
    ) -> Record {
        let Some(dir) = self.dir else {
            return said(
                now,
                pending.seqnum,
                "none",
                "there is no captures directory",
                provider,
            );
        };
        let extension = if format == FORMAT_QOI { "qoi" } else { "png" };
        let name = alloc::format!(
            "{}-{:016x}-{}.{extension}",
            self.boot,
            pending.seqnum,
            now.realtime_us / 1_000_000
        );
        let written = sys::openat(
            dir,
            name.as_bytes(),
            sys::O_WRONLY | sys::O_CREAT | sys::O_EXCL | sys::O_CLOEXEC,
            0o640,
        )
        .and_then(|fd| {
            let result = sys::write_all(fd, bytes);
            sys::close(fd);
            result
        });
        if written.is_err() {
            return said(
                now,
                pending.seqnum,
                "none",
                "the image could not be written",
                provider,
            );
        }
        vacuum(dir, now.realtime_us / 1_000_000);
        let mut record = said(
            now,
            pending.seqnum,
            "stored",
            &alloc::format!("{width}x{height}"),
            provider,
        );
        record.fields.retain(|field| field.name != "MESSAGE");
        record.push(
            "MESSAGE",
            alloc::format!(
                "screenshot of the screen at record {}: captures/{name} ({width}x{height})",
                pending.seqnum
            )
            .as_bytes(),
        );
        record.push("CAPTURE_FILE", alloc::format!("captures/{name}").as_bytes());
        record
    }
}

/// A record about the capture for record `seqnum`.
fn said(now: Now, seqnum: u64, outcome: &str, why: &str, provider: Option<&str>) -> Record {
    let mut record = ingest::own(
        6,
        MESSAGE_ID_CAPTURE,
        &alloc::format!("screenshot for record {seqnum}: {outcome} ({why})"),
        now,
    );
    record.push("CAPTURE_OF", alloc::format!("{seqnum}").as_bytes());
    record.push("CAPTURE", outcome.as_bytes());
    record.push("CAPTURE_WHY", why.as_bytes());
    if let Some(provider) = provider {
        record.push("CAPTURE_PROVIDER", provider.as_bytes());
    }
    record
}

/// Remove captures older than [`KEPT_S`], then the oldest past [`KEPT_MAX`].
fn vacuum(dir: usize, now_s: u64) {
    let _ = sys::lseek(dir, 0, sys::SEEK_SET);
    let mut files: Vec<(u64, Vec<u8>, u64)> = sys::list_dir(dir)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|name| {
            let text = core::str::from_utf8(&name).ok()?;
            let stem = text.rsplit_once('.')?.0;
            let when: u64 = stem.rsplit_once('-')?.1.parse().ok()?;
            let size = sys::openat(dir, &name, sys::O_RDONLY | sys::O_CLOEXEC, 0).map_or(0, |fd| {
                let size = sys::lseek(fd, 0, sys::SEEK_END).unwrap_or(0);
                sys::close(fd);
                size as u64
            });
            Some((when, name, size))
        })
        .collect();
    files.sort();
    let mut total: u64 = files.iter().map(|(_, _, size)| size).sum();
    for (when, name, size) in files {
        if (now_s.saturating_sub(when) > KEPT_S || total > KEPT_MAX)
            && sys::unlinkat(dir, &name).is_ok()
        {
            total = total.saturating_sub(size);
        }
    }
}
