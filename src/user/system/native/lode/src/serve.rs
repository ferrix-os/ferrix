//! The service: `lode` as init starts it, with a bootstrap channel
//! (`docs/LOGGING.md` §11).
//!
//! It opens what needs root -- `/dev/kmsg`, and the store's directories,
//! which it makes and gives to the `lode` user -- then becomes that user
//! for good, reads its rules and retention classes, takes up the store
//! where its last instance left it (torn tails cut away, the boot's last
//! kernel sequence number found), says READY and reads the kernel log for as
//! long as init keeps its channel open.
//!
//! Every record read passes the rules, the folding and the limiter
//! ([`ferrix_lode_core::limit`]) and goes to its retention class's segment
//! ([`ferrix_lode_core::retention`]): one `write` a class a wakeup, so a
//! crash of lode loses nothing it had stored; `fdatasync` follows every
//! [`SYNC_US`], and at once after a record of priority crit or more urgent.
//! A restart reads the kernel log from its oldest record and skips what was
//! stored, so what the ring still held is not lost, and what it no longer
//! held is said in a record of its own.

use alloc::string::String;
use alloc::vec::Vec;

use ferrix_lode_core::ingest::{self, KernelSequence, Now, Taken};
use ferrix_lode_core::kmsg;
use ferrix_lode_core::limit::{self, Limiter, Rule};
use ferrix_lode_core::record::Record;
use ferrix_lode_core::retention::{self, Class, DAY_US, Kept};
use ferrix_lode_core::segment::{self, Header};
use ferrix_lode_core::time::Zone;
use ferrix_native_abi::directory::{MAX_MESSAGE, Message};
use ferrix_rt::Kernel;
use ferrix_rt::native::channel::Channel;
use ferrix_rt::native::{Deadline, Object, Signals};

use crate::capture::Capture;
use crate::seat::Seat;
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
/// Rules and retention classes.
pub(crate) const CONFIG: &[u8] = b"/etc/ferrix/lode.d";
/// How long a write may wait for `fdatasync`, in microseconds.
pub(crate) const SYNC_US: u64 = 5_000_000;
/// The size a segment is sealed at, and the next begun.
const SEGMENT_MAX: usize = 16 * 1024 * 1024;
/// The most the persistent store may hold.
const STORE_MAX: u64 = 1 << 30;
/// The most the volatile store may hold.
const RUNTIME_MAX: u64 = 64 << 20;
/// How long one wait for the kernel log lasts, in milliseconds.
const WAIT_MS: u64 = 250;
/// What `lode rotate` writes to the kernel log to ask for a rotation.
pub(crate) const ROTATE: &[u8] = b"lode-rotate";

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

/// One class's segment being written.
struct Writer {
    /// The segment, open for appending.
    fd: usize,
    /// Its name.
    name: String,
    /// Its length.
    size: usize,
    /// When it was begun.
    created_us: u64,
    /// Whether a write waits for `fdatasync`.
    dirty: bool,
}

/// The store: its directories, classes and writers.
struct Store {
    /// The persistent directory, if `/var` could be written.
    persistent: Option<usize>,
    /// The volatile directory.
    volatile: usize,
    /// The retention classes.
    classes: Vec<Class>,
    /// A writer per class, made when the class first has a record.
    writers: Vec<Option<Writer>>,
    /// The next record's sequence number.
    next: u64,
    /// The header every new segment gets, but its first sequence number.
    header: Header,
    /// When the last `fdatasync` was.
    synced_at: u64,
}

/// Run the service on init's channel.
pub(crate) fn run(channel: &Channel<Kernel>) -> Result<(), Step> {
    let kmsg = sys::open(b"/dev/kmsg", sys::O_RDONLY | sys::O_NONBLOCK).map_err(|_| Step::Kmsg)?;
    let volatile = store_dir(&[b"/run/log"], store::VOLATILE).ok_or(Step::Store)?;
    let persistent = store_dir(&[b"/var", b"/var/log"], store::PERSISTENT);
    let _ = sys::mkdirat(sys::AT_FDCWD, RUN, 0o755);
    if let Ok(fd) = sys::open(RUN, sys::O_RDONLY) {
        let _ = sys::fchown(fd, UID, GID);
        sys::close(fd);
    }
    let (boot_id, restarted) = boot_id();
    let seat = Seat::open();
    let journal = if persistent.is_some() {
        store::PERSISTENT
    } else {
        store::VOLATILE
    };
    let capture = Capture::open(journal, &boot_id);
    let dropped = sys::become_ids(UID, GID).is_ok();
    let zone = zone::local();
    let now = clocks();
    let mut said: Vec<Record> = Vec::new();
    let (rules, classes) = configuration(&mut said, now);
    let mut store = Store {
        persistent: persistent.map(|dir| dir.fd),
        volatile: volatile.fd,
        writers: classes.iter().map(|_| None).collect(),
        classes,
        next: 1,
        header: new_header(boot_id),
        synced_at: 0,
    };
    let sequence = store.take_up(&mut said);
    let text = if restarted {
        alloc::format!(
            "lode restarted: an earlier instance of this boot ended; last stored kernel seq {}",
            sequence
                .last
                .map_or_else(|| String::from("none"), |seq| alloc::format!("{seq}"))
        )
    } else {
        String::from("lode started for this boot")
    };
    let id = if restarted {
        ingest::MESSAGE_ID_RESTARTED
    } else {
        ingest::MESSAGE_ID_STARTED
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
    let _ = store.append(said, &zone).map_err(|()| Step::Segment)?;
    say_ready(channel)?;
    let limiter = Limiter::new(rules);
    let mut service = Service {
        kmsg,
        store,
        sequence,
        limiter,
        seat,
        capture,
        zone,
    };
    service.serve(channel, restarted)
}

/// What the loop works with.
struct Service {
    /// `/dev/kmsg`, open.
    kmsg: usize,
    /// The store.
    store: Store,
    /// Where the kernel's sequence stands.
    sequence: KernelSequence,
    /// The rules, folding and limiter.
    limiter: Limiter,
    /// The seat's owner's socket and the sessions.
    seat: Seat,
    /// Crash screenshots, when they are on.
    capture: Capture,
    /// The local zone, for the offsets records carry.
    zone: Zone,
}

/// The rules and the retention classes, with a record for each file that
/// did not read.
fn configuration(said: &mut Vec<Record>, now: Now) -> (Vec<Rule>, Vec<Class>) {
    let mut rules = Vec::new();
    let mut classes = retention::defaults();
    let Ok(dir) = Dir::open(CONFIG) else {
        return (rules, classes);
    };
    let mut names = sys::list_dir(dir.fd).unwrap_or_default();
    names.sort();
    for name in names {
        let Ok(bytes) = sys::read_file_from(dir.fd, &name, 0) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        let shown = String::from_utf8_lossy(&name);
        let failed = if name.ends_with(b".rules") {
            limit::parse_rules(&text)
                .map(|more| rules.extend(more))
                .err()
        } else if name == b"retention.conf" {
            retention::parse(&text).map(|read| classes = read).err()
        } else {
            None
        };
        if let Some(error) = failed {
            said.push(ingest::own(
                3,
                ingest::MESSAGE_ID_STARTED,
                &alloc::format!(
                    "{}/{shown}:{}: {}; the file is not used",
                    String::from_utf8_lossy(CONFIG),
                    error.line,
                    error.why
                ),
                now,
            ));
        }
    }
    sys::close(dir.fd);
    (rules, classes)
}

impl Service {
    /// The loop: read the kernel log and the seat's owner, store what they
    /// say, until init lets go.
    fn serve(&mut self, channel: &Channel<Kernel>, restarted: bool) -> Result<(), Step> {
        let mut first_start = !restarted && self.sequence.last.is_none();
        loop {
            let mut batch: Vec<Record> = Vec::new();
            let now = clocks();
            let rotate = self.read_kmsg(now, &mut first_start, &mut batch);
            self.seat.serve(now, &mut batch);
            if rotate {
                self.limiter.flush(now.monotonic_us, &mut batch);
            }
            self.limiter.tick(now.monotonic_us, &mut batch);
            let urgent = batch.iter().any(|record| record.priority <= 2);
            let idle = batch.is_empty();
            for record in &mut batch {
                if record.realtime_us == 0 {
                    record.realtime_us = now.realtime_of(record.monotonic_us);
                }
            }
            let wanted: Vec<(usize, String)> = batch
                .iter()
                .enumerate()
                .filter(|(_, record)| self.capture.wants(record))
                .map(|(index, record)| {
                    let text = String::from_utf8_lossy(record.message());
                    (index, text.chars().take(160).collect())
                })
                .collect();
            let seqnums = self
                .store
                .append(batch, &self.zone)
                .map_err(|()| Step::Segment)?;
            let mut more: Vec<Record> = Vec::new();
            for (index, what) in wanted {
                if let Some(&seqnum) = seqnums.get(index) {
                    self.capture.ask(seqnum, &what, now, &mut more);
                }
            }
            self.capture.serve(now, self.seat.sessions(), &mut more);
            let _ = self
                .store
                .append(more, &self.zone)
                .map_err(|()| Step::Segment)?;
            if rotate {
                self.store.rotate_all();
            }
            self.store.sync_if(urgent, now.monotonic_us);
            if closed(channel) {
                self.store.sync_if(true, now.monotonic_us);
                return Ok(());
            }
            if idle {
                let mut fds = alloc::vec![self.kmsg];
                fds.extend(self.seat.fds());
                fds.extend(self.capture.fds());
                sys::wait_any(&fds, WAIT_MS);
            }
        }
    }

    /// Read what the kernel log has, up to 512 records, into `batch`.
    /// Answers whether one of them asked for a rotation.
    fn read_kmsg(&mut self, now: Now, first_start: &mut bool, batch: &mut Vec<Record>) -> bool {
        let mut buf = alloc::vec![0_u8; 8192];
        let mut rotate = false;
        for _ in 0..512 {
            match sys::read(self.kmsg, &mut buf) {
                Ok(0) => break,
                Ok(got) => {
                    let Some(kernel) = kmsg::parse(buf.get(..got).unwrap_or_default()) else {
                        continue;
                    };
                    let mut record = ingest::from_kernel(&kernel, now);
                    match self.sequence.take(kernel.seq) {
                        Taken::Seen => continue,
                        Taken::Next => {}
                        Taken::After { lost } => {
                            batch.push(ingest::lost_record(
                                lost,
                                kernel.seq,
                                *first_start,
                                &record,
                            ));
                        }
                    }
                    *first_start = false;
                    rotate |= record.get("SYSLOG_IDENTIFIER") == Some(ROTATE)
                        && record.get("_TRANSPORT") == Some(b"console");
                    self.seat.stamp(&mut record);
                    if record.get("_TRANSPORT") == Some(b"kernel")
                        && record.get("CRASH_PID").is_some()
                    {
                        batch.push(with_program(record));
                    } else {
                        self.limiter.admit(record, now.monotonic_us, batch);
                    }
                }
                // Records went past this reader; the next read goes on from
                // the first kept, and the sequence numbers say how many.
                Err(error)
                    if error.0 == sys::EPIPE || error.0 == sys::EINVAL || error.0 == sys::EINTR => {
                }
                Err(_) => break,
            }
        }
        rotate
    }
}

/// A crash record with the program's name and file, read while the ended
/// process may still be there to read them from.
fn with_program(mut record: Record) -> Record {
    let Some(pid) = record.get_str("CRASH_PID").map(String::from) else {
        return record;
    };
    if let Some(comm) = sys::read_small(alloc::format!("/proc/{pid}/comm").as_bytes()) {
        record.push("CRASH_COMM", comm.strip_suffix(b"\n").unwrap_or(&comm));
    }
    if let Some(cmdline) = sys::read_small(alloc::format!("/proc/{pid}/cmdline").as_bytes()) {
        let program = cmdline.split(|&byte| byte == 0).next().unwrap_or_default();
        if !program.is_empty() {
            record.push("CRASH_PROGRAM", program);
        }
    }
    record
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

/// A store directory, made and given to lode's user, if it can be written.
fn store_dir(parents: &[&[u8]], path: &[u8]) -> Option<Dir> {
    for parent in parents {
        let _ = sys::mkdirat(sys::AT_FDCWD, parent, 0o755);
    }
    sys::mkdirat(sys::AT_FDCWD, path, 0o2750).ok()?;
    let dir = Dir::open(path).ok()?;
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
            Some(dir)
        }
        Err(_) => {
            sys::close(dir.fd);
            None
        }
    }
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

/// The header a new segment of `boot_id` gets.
fn new_header(boot_id: [u8; 16]) -> Header {
    let machine_id = sys::read_small(b"/etc/machine-id")
        .and_then(|text| core::str::from_utf8(&text).ok().and_then(segment::parse_id))
        .unwrap_or([0; 16]);
    Header {
        boot_id,
        machine_id,
        first_seqnum: 0,
        created_us: 0,
        host: String::from_utf8_lossy(&sys::hostname()).into_owned(),
    }
}

impl Store {
    /// The directory class `index` is kept in.
    fn dir_of(&self, index: usize) -> usize {
        match (
            self.classes.get(index).is_some_and(|class| class.ram_only),
            self.persistent,
        ) {
            (false, Some(persistent)) => persistent,
            _ => self.volatile,
        }
    }

    /// Every segment in both directories, oldest first.
    fn segments(&self) -> Vec<store::Segment> {
        let mut dirs = alloc::vec![Dir { fd: self.volatile }];
        if let Some(fd) = self.persistent {
            dirs.push(Dir { fd });
        }
        store::all_segments(&dirs)
    }

    /// Take up the store: cut torn tails off each class's newest segment,
    /// find the next sequence number and this boot's last kernel record,
    /// and reopen each class's newest segment that is this boot's. What
    /// lode has to say about it goes into `said`.
    fn take_up(&mut self, said: &mut Vec<Record>) -> KernelSequence {
        let segments = self.segments();
        let mut sequence = KernelSequence::default();
        let mut newest: Vec<&store::Segment> = Vec::new();
        for each in segments.iter().rev() {
            let class = segment::class_of_name(&each.name).unwrap_or_default();
            if !newest
                .iter()
                .any(|seen| segment::class_of_name(&seen.name) == Some(class))
            {
                newest.push(each);
            }
        }
        for each in &newest {
            let Some(bytes) = store::bytes(each, 0) else {
                continue;
            };
            let Some(scanned) = segment::scan(&bytes, 0) else {
                continue;
            };
            if scanned.good < bytes.len() {
                cut(each, scanned.good, bytes.len(), said);
            }
            if let Some(found) = scanned.records.last() {
                self.next = self.next.max(found.record.seqnum + 1);
            }
            self.next = self.next.max(scanned.header.first_seqnum);
            let class = segment::class_of_name(&each.name).unwrap_or_default();
            if scanned.header.boot_id == self.header.boot_id
                && let Some(index) = self.classes.iter().position(|known| known.name == class)
                && let Ok(fd) = sys::openat(
                    each.dir,
                    each.name.as_bytes(),
                    sys::O_WRONLY | sys::O_APPEND | sys::O_CLOEXEC,
                    0,
                )
                && let Some(slot) = self.writers.get_mut(index)
            {
                *slot = Some(Writer {
                    fd,
                    name: each.name.clone(),
                    size: scanned.good,
                    created_us: scanned.header.created_us,
                    dirty: false,
                });
            }
        }
        sequence.last = self.last_kernel_seq(&segments);
        self.unsaid(&segments, said);
        sequence
    }

    /// Kernel records an earlier instance of this boot held back and ended
    /// before saying so -- a fold or a window still open when it was
    /// killed: one record a gap, so `lode verify --kernel` finds none unsaid.
    fn unsaid(&self, segments: &[store::Segment], said: &mut Vec<Record>) {
        let mut records: Vec<Record> = Vec::new();
        for each in segments {
            if let Some(bytes) = store::bytes(each, 0)
                && let Some(scanned) = segment::scan(&bytes, 0)
                && scanned.header.boot_id == self.header.boot_id
            {
                records.extend(scanned.records.into_iter().map(|found| found.record));
            }
        }
        let refs: Vec<&Record> = records.iter().collect();
        for (from, to) in ingest::kernel_gaps(&refs).unsaid {
            let mut record = ingest::own(
                4,
                limit::MESSAGE_ID_SUPPRESSED,
                &alloc::format!(
                    "kernel records {from} to {} were held back by an instance of lode that ended before saying so",
                    to - 1
                ),
                clocks(),
            );
            record.push("N_SUPPRESSED", alloc::format!("{}", to - from).as_bytes());
            record.push("HELD_FIRST_SEQNUM", alloc::format!("{from}").as_bytes());
            record.push("HELD_LAST_SEQNUM", alloc::format!("{}", to - 1).as_bytes());
            said.push(record);
        }
    }

    /// The last kernel sequence number this boot's segments hold.
    fn last_kernel_seq(&self, segments: &[store::Segment]) -> Option<u64> {
        let mut last: Option<u64> = None;
        let mut done: Vec<String> = Vec::new();
        for each in segments.iter().rev() {
            let class = String::from(segment::class_of_name(&each.name).unwrap_or_default());
            if done.contains(&class) {
                continue;
            }
            let Some(bytes) = store::bytes(each, 0) else {
                continue;
            };
            let Some(scanned) = segment::scan(&bytes, 0) else {
                continue;
            };
            if scanned.header.boot_id != self.header.boot_id {
                done.push(class);
                continue;
            }
            let found = scanned
                .records
                .iter()
                .filter_map(|found| ingest::kernel_seq(&found.record))
                .max();
            if found.is_some() {
                last = last.max(found);
                done.push(class);
            }
        }
        last
    }

    /// Number, stamp and write `records`, each to its class's segment.
    /// Answers each record's sequence number, in order.
    fn append(&mut self, records: Vec<Record>, zone: &Zone) -> Result<Vec<u64>, ()> {
        let mut seqnums = Vec::with_capacity(records.len());
        if records.is_empty() {
            return Ok(seqnums);
        }
        let mut bytes: Vec<(u64, Vec<u8>)> = self.classes.iter().map(|_| (0, Vec::new())).collect();
        for mut record in records {
            record.seqnum = self.next;
            seqnums.push(self.next);
            record.utc_offset_s = zone.offset_at(record.realtime_us);
            self.next += 1;
            let class = retention::assign(&self.classes, &record);
            if let Some((first, out)) = bytes.get_mut(class) {
                if out.is_empty() {
                    *first = record.seqnum;
                }
                segment::encode_record(&record, out);
            }
        }
        for (index, (first, out)) in bytes.iter().enumerate() {
            if out.is_empty() {
                continue;
            }
            self.ready(index, *first)?;
            let Some(Some(writer)) = self.writers.get_mut(index) else {
                return Err(());
            };
            sys::write_all(writer.fd, out).map_err(|_| ())?;
            writer.size += out.len();
            writer.dirty = true;
        }
        Ok(seqnums)
    }

    /// Make sure class `index` has a segment with room, begun at `first`
    /// if a new one is needed; a full or day-old one is sealed first.
    fn ready(&mut self, index: usize, first: u64) -> Result<(), ()> {
        let now = sys::clock_us(sys::CLOCK_REALTIME);
        let stale = self
            .writers
            .get(index)
            .and_then(Option::as_ref)
            .is_some_and(|writer| {
                writer.size >= SEGMENT_MAX || now.saturating_sub(writer.created_us) >= DAY_US
            });
        if stale {
            self.seal(index);
        }
        if self.writers.get(index).is_some_and(Option::is_some) {
            return Ok(());
        }
        let class = self
            .classes
            .get(index)
            .map(|class| class.name.clone())
            .ok_or(())?;
        let dir = self.dir_of(index);
        let mut header = self.header.clone();
        header.first_seqnum = first;
        header.created_us = now;
        let name = segment::file_name(&class, &header.boot_id, first);
        let fd = sys::openat(
            dir,
            name.as_bytes(),
            sys::O_WRONLY | sys::O_CREAT | sys::O_APPEND | sys::O_CLOEXEC,
            0o640,
        )
        .map_err(|_| ())?;
        let bytes = header.encode();
        sys::ftruncate(fd, 0).map_err(|_| ())?;
        sys::write_all(fd, &bytes).map_err(|_| ())?;
        if let Some(slot) = self.writers.get_mut(index) {
            *slot = Some(Writer {
                fd,
                name,
                size: bytes.len(),
                created_us: now,
                dirty: true,
            });
        }
        self.vacuum();
        Ok(())
    }

    /// Seal class `index`'s segment: synced and closed, the next record
    /// begins another.
    fn seal(&mut self, index: usize) {
        if let Some(slot) = self.writers.get_mut(index)
            && let Some(writer) = slot.take()
        {
            let _ = sys::fdatasync(writer.fd);
            sys::close(writer.fd);
        }
    }

    /// Seal every class's segment and begin the next at once (`lode
    /// rotate`), so a segment `lode delete` may rewrite is never the one
    /// being written.
    fn rotate_all(&mut self) {
        for index in 0..self.writers.len() {
            if self.writers.get(index).is_some_and(Option::is_some) {
                self.seal(index);
                let _ = self.ready(index, self.next);
            }
        }
    }

    /// `fdatasync` every written segment when `now` is far enough past the
    /// last, or `urgent`.
    fn sync_if(&mut self, urgent: bool, now: u64) {
        if !urgent && now.saturating_sub(self.synced_at) < SYNC_US {
            return;
        }
        for writer in self.writers.iter_mut().flatten() {
            if writer.dirty {
                let _ = sys::fdatasync(writer.fd);
                writer.dirty = false;
            }
        }
        self.synced_at = now;
    }

    /// Remove what the classes and the stores' limits no longer keep.
    fn vacuum(&self) {
        let now = sys::clock_us(sys::CLOCK_REALTIME);
        let active: Vec<&str> = self
            .writers
            .iter()
            .flatten()
            .map(|writer| writer.name.as_str())
            .collect();
        let mut dirs = alloc::vec![(self.volatile, RUNTIME_MAX)];
        if let Some(fd) = self.persistent {
            dirs.push((fd, STORE_MAX));
        }
        for (dir, limit) in dirs {
            let segments = store::all_segments(&[Dir { fd: dir }]);
            let kept: Vec<Kept> = segments
                .iter()
                .map(|each| Kept {
                    class: String::from(segment::class_of_name(&each.name).unwrap_or_default()),
                    name: each.name.clone(),
                    created_us: store::header(each).map_or(0, |header| header.created_us),
                    size: store::size(each),
                    active: active.contains(&each.name.as_str()),
                })
                .collect();
            for name in retention::vacuum(&self.classes, &kept, now, limit) {
                let _ = sys::unlinkat(dir, name.as_bytes());
            }
        }
    }
}

/// Cut a torn tail off `segment` at `good`, and say so.
fn cut(segment: &store::Segment, good: usize, length: usize, said: &mut Vec<Record>) {
    if let Ok(fd) = sys::openat(
        segment.dir,
        segment.name.as_bytes(),
        sys::O_WRONLY | sys::O_CLOEXEC,
        0,
    ) {
        let _ = sys::ftruncate(fd, good);
        let _ = sys::fdatasync(fd);
        sys::close(fd);
    }
    said.push(ingest::own(
        4,
        ingest::MESSAGE_ID_TORN,
        &alloc::format!(
            "segment {}: {} byte(s) of a torn record cut away",
            segment.name,
            length - good
        ),
        clocks(),
    ));
}
