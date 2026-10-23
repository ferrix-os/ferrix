//! The service: `lode` as init starts it, with a bootstrap channel
//! (`docs/LOGGING.md` §11).
//!
//! It opens what needs root -- `/dev/kmsg`, and the store's directories,
//! which it makes and gives to the `lode` user -- then becomes that user
//! for good, takes up the store where its last instance left it (a torn
//! tail cut away, the boot's last kernel sequence number found), says READY
//! and reads the kernel log for as long as init keeps its channel open.
//!
//! Every record read is written at once, in one `write` a wakeup, so a
//! crash of lode loses nothing the kernel was given; `fdatasync` follows
//! every [`SYNC_US`], and at once after a record of priority crit or more
//! urgent. A restart reads the kernel log from its oldest record and skips
//! what was stored, so what the ring still held is not lost, and what it
//! no longer held is said in a record of its own.

use alloc::string::String;
use alloc::vec::Vec;

use ferrix_lode_core::ingest::{self, KernelSequence, Now, Taken};
use ferrix_lode_core::kmsg;
use ferrix_lode_core::record::Record;
use ferrix_lode_core::segment::{self, Header};
use ferrix_lode_core::time::Zone;
use ferrix_native_abi::directory::{MAX_MESSAGE, Message};
use ferrix_rt::Kernel;
use ferrix_rt::native::channel::Channel;
use ferrix_rt::native::{Deadline, Object, Signals};

use crate::store::{self, Dir};
use crate::{sys, zone};

/// The user lode runs as once it has opened what needs root.
pub(crate) const UID: u32 = 91;
/// The group the store is readable by: `adm`.
pub(crate) const GID: u32 = 4;
/// Where lode keeps what one boot's instances share.
const RUN: &[u8] = b"/run/lode";
/// The boot's id, which a restarted lode takes up.
const BOOT_ID: &[u8] = b"/run/lode/boot_id";
/// How long a write may wait for `fdatasync`, in microseconds.
pub(crate) const SYNC_US: u64 = 5_000_000;
/// The size a segment is sealed at, and the next begun.
const SEGMENT_MAX: usize = 16 * 1024 * 1024;
/// The most the store may hold; the oldest segments go first.
const STORE_MAX: usize = 1024 * 1024 * 1024;
/// How long one wait for the kernel log lasts, in milliseconds.
const WAIT_MS: u64 = 250;

/// Why the service stopped, as its exit status.
#[derive(Debug, Clone, Copy)]
#[repr(i32)]
pub(crate) enum Step {
    /// `/dev/kmsg` would not open.
    Kmsg = 2,
    /// No store directory could be made.
    Store = 3,
    /// No segment could be written.
    Segment = 4,
    /// init's channel failed.
    Channel = 5,
}

/// The segment being written.
struct Writer {
    /// The store's directory.
    dir: usize,
    /// The segment, open for appending.
    fd: usize,
    /// Its length.
    size: usize,
    /// Its header.
    header: Header,
    /// The next record's sequence number.
    next: u64,
    /// When the last `fdatasync` was, and whether anything was written since.
    synced_at: u64,
    /// Whether a write waits for `fdatasync`.
    dirty: bool,
}

/// Run the service on init's channel.
pub(crate) fn run(channel: &Channel<Kernel>) -> Result<(), Step> {
    let kmsg = sys::open(b"/dev/kmsg", sys::O_RDONLY | sys::O_NONBLOCK).map_err(|_| Step::Kmsg)?;
    let dir = store_dir().ok_or(Step::Store)?;
    let _ = sys::mkdirat(sys::AT_FDCWD, RUN, 0o755);
    if let Ok(fd) = sys::open(RUN, sys::O_RDONLY) {
        let _ = sys::fchown(fd, UID, GID);
        sys::close(fd);
    }
    let (boot_id, restarted) = boot_id();
    let dropped = sys::become_ids(UID, GID).is_ok();
    let zone = zone::local();
    let mut said: Vec<Record> = Vec::new();
    let (mut writer, mut sequence) = take_up(dir.fd, boot_id, &mut said).ok_or(Step::Segment)?;
    let now = clocks();
    let (id, text) = if restarted {
        (
            ingest::MESSAGE_ID_RESTARTED,
            alloc::format!(
                "lode restarted: an earlier instance of this boot ended; last stored kernel seq {}",
                sequence
                    .last
                    .map_or_else(|| String::from("none"), |seq| alloc::format!("{seq}"))
            ),
        )
    } else {
        (
            ingest::MESSAGE_ID_STARTED,
            String::from("lode started for this boot"),
        )
    };
    said.insert(
        0,
        ingest::own(if restarted { 4 } else { 6 }, id, &text, now),
    );
    if !dropped {
        said.push(ingest::own(
            4,
            ingest::MESSAGE_ID_STARTED,
            "lode could not become its own user, and runs as root",
            now,
        ));
    }
    writer
        .append(&mut said, &zone)
        .map_err(|()| Step::Segment)?;
    say_ready(channel)?;
    serve(channel, kmsg, &mut writer, &mut sequence, &zone, restarted)
}

/// The loop: read the kernel log, store it, until init lets go.
fn serve(
    channel: &Channel<Kernel>,
    kmsg: usize,
    writer: &mut Writer,
    sequence: &mut KernelSequence,
    zone: &Zone,
    restarted: bool,
) -> Result<(), Step> {
    let mut first_start = !restarted && sequence.last.is_none();
    let mut buf = alloc::vec![0_u8; 8192];
    loop {
        let mut batch: Vec<Record> = Vec::new();
        let now = clocks();
        loop {
            match sys::read(kmsg, &mut buf) {
                Ok(0) => break,
                Ok(got) => {
                    let Some(kernel) = kmsg::parse(buf.get(..got).unwrap_or_default()) else {
                        continue;
                    };
                    let record = ingest::from_kernel(&kernel, now);
                    match sequence.take(kernel.seq) {
                        Taken::Seen => continue,
                        Taken::Next => {}
                        Taken::After { lost } => {
                            batch.push(ingest::lost_record(lost, kernel.seq, first_start, &record));
                        }
                    }
                    first_start = false;
                    batch.push(record);
                    if batch.len() >= 512 {
                        break;
                    }
                }
                // Records went past this reader; the next read goes on from
                // the first kept, and the sequence numbers say how many.
                Err(error) if error.0 == sys::EPIPE || error.0 == sys::EINVAL => {}
                Err(error) if error.0 == sys::EINTR => {}
                Err(_) => break,
            }
        }
        let urgent = batch.iter().any(|record| record.priority <= 2);
        if !batch.is_empty() {
            writer
                .append(&mut batch, zone)
                .map_err(|()| Step::Segment)?;
        }
        writer.sync_if(urgent, now.monotonic_us);
        if closed(channel) {
            writer.sync_if(true, now.monotonic_us);
            return Ok(());
        }
        if batch.is_empty() {
            let _ = sys::wait_readable(kmsg, WAIT_MS);
        }
    }
}

/// Whether init has closed the channel: the service is being stopped.
fn closed(channel: &Channel<Kernel>) -> bool {
    matches!(
        channel.wait_one(Signals::PEER_CLOSED, Deadline::At(0)),
        Ok(signals) if signals.intersects(Signals::PEER_CLOSED)
    )
}

/// READY, on init's channel (`Type=native`).
fn say_ready(channel: &Channel<Kernel>) -> Result<(), Step> {
    let mut out = [0_u8; MAX_MESSAGE];
    let length = Message::ready().encode(&mut out).ok_or(Step::Channel)?;
    channel
        .write(out.get(..length).unwrap_or_default())
        .map_err(|_| Step::Channel)
}

/// The two clocks, read together.
pub(crate) fn clocks() -> Now {
    Now {
        realtime_us: sys::clock_us(sys::CLOCK_REALTIME),
        monotonic_us: sys::clock_us(sys::CLOCK_MONOTONIC),
    }
}

/// The persistent store, made and given to lode's user, or the volatile one.
fn store_dir() -> Option<Dir> {
    for (parents, path) in [
        (&[&b"/var"[..], b"/var/log"][..], store::PERSISTENT),
        (&[&b"/run/log"[..]][..], store::VOLATILE),
    ] {
        for parent in parents {
            let _ = sys::mkdirat(sys::AT_FDCWD, parent, 0o755);
        }
        if sys::mkdirat(sys::AT_FDCWD, path, 0o2750).is_err() {
            continue;
        }
        let Ok(dir) = Dir::open(path) else {
            continue;
        };
        let _ = sys::fchown(dir.fd, UID, GID);
        let _ = sys::fchmod(dir.fd, 0o2750);
        // A store that cannot be written to is no store: try a file.
        let probe = b".lode-probe";
        match sys::openat(
            dir.fd,
            probe,
            sys::O_WRONLY | sys::O_CREAT | sys::O_CLOEXEC,
            0o600,
        ) {
            Ok(fd) => {
                sys::close(fd);
                let _ = sys::unlinkat(dir.fd, probe);
                return Some(dir);
            }
            Err(_) => sys::close(dir.fd),
        }
    }
    None
}

/// This boot's id, and whether an earlier lode of this boot drew it.
fn boot_id() -> ([u8; 16], bool) {
    if let Some(text) = sys::read_small(BOOT_ID)
        && let Some(id) = core::str::from_utf8(&text).ok().and_then(segment::parse_id)
    {
        return (id, true);
    }
    let mut id = [0_u8; 16];
    if sys::random(&mut id).is_err() {
        // No randomness: the clocks are at least different every boot.
        let now = clocks();
        let mut mixed = [0_u8; 16];
        let (low, high) = mixed.split_at_mut(8);
        low.copy_from_slice(&now.realtime_us.to_le_bytes());
        high.copy_from_slice(&now.monotonic_us.to_le_bytes());
        id = mixed;
    }
    let mut text = String::new();
    segment::push_hex(&mut text, &id);
    text.push('\n');
    if let Ok(fd) = sys::open(BOOT_ID, sys::O_WRONLY | sys::O_CREAT | sys::O_EXCL) {
        let _ = sys::write_all(fd, text.as_bytes());
        let _ = sys::fchown(fd, UID, GID);
        sys::close(fd);
    }
    (id, false)
}

/// Take up the store: cut a torn tail off the newest segment, find the
/// next sequence number and this boot's last kernel record, and open the
/// segment to write -- the newest, if it is this boot's, else a new one.
/// What lode has to say about it goes into `said`.
fn take_up(
    dir: usize,
    boot_id: [u8; 16],
    said: &mut Vec<Record>,
) -> Option<(Writer, KernelSequence)> {
    let dirs = [Dir { fd: dir }];
    let segments = store::all_segments(&dirs);
    let mut next = 1;
    let mut sequence = KernelSequence::default();
    let mut current: Option<(String, Header, usize)> = None;
    if let Some(newest) = segments.last()
        && let Some(bytes) = store::bytes(newest, 0)
        && let Some(scanned) = segment::scan(&bytes, 0)
    {
        if scanned.good < bytes.len() {
            if let Ok(fd) = sys::openat(
                dir,
                newest.name.as_bytes(),
                sys::O_WRONLY | sys::O_CLOEXEC,
                0,
            ) {
                let _ = sys::ftruncate(fd, scanned.good);
                let _ = sys::fdatasync(fd);
                sys::close(fd);
            }
            said.push(ingest::own(
                4,
                ingest::MESSAGE_ID_TORN,
                &alloc::format!(
                    "segment {}: {} byte(s) of a torn record cut away",
                    newest.name,
                    bytes.len() - scanned.good
                ),
                clocks(),
            ));
        }
        next = scanned
            .records
            .last()
            .map_or(scanned.header.first_seqnum, |found| found.record.seqnum + 1);
        if scanned.header.boot_id == boot_id {
            current = Some((newest.name.clone(), scanned.header.clone(), scanned.good));
        }
    }
    for each in segments.iter().rev() {
        let Some(bytes) = store::bytes(each, 0) else {
            continue;
        };
        let Some(scanned) = segment::scan(&bytes, 0) else {
            continue;
        };
        if scanned.header.boot_id != boot_id {
            break;
        }
        sequence.last = scanned
            .records
            .iter()
            .filter_map(|found| ingest::kernel_seq(&found.record))
            .max();
        if sequence.last.is_some() {
            break;
        }
    }
    let writer = match current {
        Some((name, header, size)) => {
            let fd = sys::openat(
                dir,
                name.as_bytes(),
                sys::O_WRONLY | sys::O_APPEND | sys::O_CLOEXEC,
                0,
            )
            .ok()?;
            Writer::with(dir, fd, size, header, next)
        }
        None => Writer::create(dir, new_header(boot_id, next))?,
    };
    Some((writer, sequence))
}

/// A header for a new segment of `boot_id` beginning at `first`.
fn new_header(boot_id: [u8; 16], first: u64) -> Header {
    let machine_id = sys::read_small(b"/etc/machine-id")
        .and_then(|text| core::str::from_utf8(&text).ok().and_then(segment::parse_id))
        .unwrap_or([0; 16]);
    Header {
        boot_id,
        machine_id,
        first_seqnum: first,
        created_us: sys::clock_us(sys::CLOCK_REALTIME),
        host: String::from_utf8_lossy(&sys::hostname()).into_owned(),
    }
}

impl Writer {
    /// A writer for a segment already open.
    fn with(dir: usize, fd: usize, size: usize, header: Header, next: u64) -> Writer {
        Writer {
            dir,
            fd,
            size,
            header,
            next,
            synced_at: 0,
            dirty: false,
        }
    }

    /// A new segment with `header`, made and its header written.
    fn create(dir: usize, header: Header) -> Option<Writer> {
        let name = segment::file_name(&header.boot_id, header.first_seqnum);
        let fd = sys::openat(
            dir,
            name.as_bytes(),
            sys::O_WRONLY | sys::O_CREAT | sys::O_APPEND | sys::O_CLOEXEC,
            0o640,
        )
        .ok()?;
        let bytes = header.encode();
        sys::ftruncate(fd, 0).ok()?;
        sys::write_all(fd, &bytes).ok()?;
        let next = header.first_seqnum;
        Some(Writer::with(dir, fd, bytes.len(), header, next))
    }

    /// Number, stamp and write `records`, and empty the list.
    fn append(&mut self, records: &mut Vec<Record>, zone: &Zone) -> Result<(), ()> {
        if self.size >= SEGMENT_MAX {
            self.rotate()?;
        }
        let mut bytes = Vec::new();
        for record in records.iter_mut() {
            record.seqnum = self.next;
            record.utc_offset_s = zone.offset_at(record.realtime_us);
            self.next += 1;
            segment::encode_record(record, &mut bytes);
        }
        records.clear();
        sys::write_all(self.fd, &bytes).map_err(|_| ())?;
        self.size += bytes.len();
        self.dirty = true;
        Ok(())
    }

    /// `fdatasync` when `now` is far enough past the last, or `urgent`.
    fn sync_if(&mut self, urgent: bool, now: u64) {
        if self.dirty && (urgent || now.saturating_sub(self.synced_at) >= SYNC_US) {
            let _ = sys::fdatasync(self.fd);
            self.synced_at = now;
            self.dirty = false;
        }
    }

    /// Seal this segment and begin the next, then trim the store.
    fn rotate(&mut self) -> Result<(), ()> {
        let _ = sys::fdatasync(self.fd);
        sys::close(self.fd);
        let mut header = self.header.clone();
        header.first_seqnum = self.next;
        header.created_us = sys::clock_us(sys::CLOCK_REALTIME);
        let next = Writer::create(self.dir, header).ok_or(())?;
        *self = next;
        vacuum(
            self.dir,
            &segment::file_name(&self.header.boot_id, self.header.first_seqnum),
        );
        Ok(())
    }
}

/// Remove the oldest segments while the store holds more than [`STORE_MAX`],
/// never `keep`, the one being written.
fn vacuum(dir: usize, keep: &str) {
    let dirs = [Dir { fd: dir }];
    let segments = store::all_segments(&dirs);
    let sizes: Vec<usize> = segments
        .iter()
        .map(|each| {
            sys::openat(dir, each.name.as_bytes(), sys::O_RDONLY | sys::O_CLOEXEC, 0).map_or(
                0,
                |fd| {
                    let size = sys::lseek(fd, 0, sys::SEEK_END).unwrap_or(0);
                    sys::close(fd);
                    size
                },
            )
        })
        .collect();
    let mut total: usize = sizes.iter().sum();
    for (each, size) in segments.iter().zip(sizes) {
        if total <= STORE_MAX {
            break;
        }
        if each.name == keep {
            continue;
        }
        if sys::unlinkat(dir, each.name.as_bytes()).is_ok() {
            total -= size;
        }
    }
}
