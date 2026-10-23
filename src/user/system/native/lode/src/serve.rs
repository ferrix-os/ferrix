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

use ferrix_lode_core::compress::Compressor;
use ferrix_lode_core::ingest::{self, KernelSequence, Now, Taken};
use ferrix_lode_core::kmsg;
use ferrix_lode_core::limit::{self, Limiter, Rule};
use ferrix_lode_core::record::Record;
use ferrix_lode_core::retention::{self, Class, DAY_US, Kept};
use ferrix_lode_core::segment::{self, Header};
use ferrix_lode_core::time::Zone;
use ferrix_lode_core::{access, units};
use ferrix_native_abi::directory::{MAX_MESSAGE, Message};
use ferrix_rt::Kernel;
use ferrix_rt::native::channel::Channel;
use ferrix_rt::native::{Deadline, Object, Signals};

use crate::capture::Capture;
use crate::front::Front;
use crate::handout::Handout;
use crate::seat::Seat;
use crate::store::{self, Dir};
use crate::units::Units;
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

/// A segment being written, and whose: a class's, the system's or a
/// user's (`ferrix_lode_core::access`).
struct Slot {
    /// The class, by its index.
    class: usize,
    /// The user whose segments these are; `None` for the system's.
    owner: Option<u32>,
    /// The writer.
    writer: Writer,
}

/// A writer's class, by index, and owner.
type Key = (usize, Option<u32>);

/// The store: its directories, classes and writers.
struct Store {
    /// The persistent directory, if `/var` could be written.
    persistent: Option<usize>,
    /// The volatile directory.
    volatile: usize,
    /// The retention classes.
    classes: Vec<Class>,
    /// A writer per class and owner, made when it first has a record.
    writers: Vec<Slot>,
    /// The next record's sequence number.
    next: u64,
    /// The header every new segment gets, but its first sequence number.
    header: Header,
    /// When the last `fdatasync` was.
    synced_at: u64,
    /// Sealed segments waiting to be compressed: directory and name.
    to_compress: Vec<(usize, String)>,
    /// The one being compressed.
    compressing: Option<Sealing>,
}

/// A sealed segment being compressed, a few frames each turn of the loop.
struct Sealing {
    /// Its directory.
    dir: usize,
    /// Its name.
    name: String,
    /// The work.
    compressor: Compressor,
    /// Its size when begun: one that changed (`lode delete` rewrote it) is
    /// left as it is.
    size: u64,
}

/// Frames compressed a turn: 256 KiB of records, so the kernel log is read
/// again soon whatever a segment's size.
const FRAMES_A_TURN: usize = 4;

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
    let front = Front::open();
    let units = Units::open();
    let handout = Handout::open();
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
        writers: Vec::new(),
        classes,
        next: 1,
        header: new_header(boot_id),
        synced_at: 0,
        to_compress: Vec::new(),
        compressing: None,
    };
    let sequence = store.take_up(&mut said);
    store.summarise_old();
    store.compress_old();
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
        front,
        capture,
        zone,
        units,
        handout,
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
    /// The Linux front: `/dev/log`, the journal's sockets.
    front: Front,
    /// Crash screenshots, when they are on.
    capture: Capture,
    /// The local zone, for the offsets records carry.
    zone: Zone,
    /// init's socket: units' output with who wrote it.
    units: Units,
    /// Users' own segments, handed over.
    handout: Handout,
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
            // The seat's owner first: a session it announced is open before
            // the lines its programs wrote after the announcement are read.
            self.seat.serve(now, &mut batch);
            // init's lines before the kernel log, so a console copy finds
            // the trusted line it copies waiting.
            let mut trusted = Vec::new();
            self.units.serve(now, &mut trusted);
            self.admit_all(trusted, now, &mut batch);
            let (rotate, drained) = self.read_kmsg(now, &mut first_start, &mut batch);
            // A trusted line is stored without its copy only once the
            // kernel log has been read to its end: a copy still waiting
            // behind a flood is not a copy lost.
            if drained {
                let mut expired = Vec::new();
                self.units.pairing.expire(now.monotonic_us, &mut expired);
                self.admit_all(expired, now, &mut batch);
            }
            let mut heard = Vec::new();
            self.front.serve(now, &mut heard);
            for mut record in heard {
                self.seat.stamp(&mut record);
                self.limiter.admit(record, now.monotonic_us, &mut batch);
            }
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
            let compressing = self.store.compress_some();
            if closed(channel) {
                self.store.sync_if(true, now.monotonic_us);
                return Ok(());
            }
            let mut stores = alloc::vec![self.store.volatile];
            stores.extend(self.store.persistent);
            self.handout.serve(now.monotonic_us, &stores);
            if idle && !compressing && !self.handout.busy() {
                let mut fds = alloc::vec![self.kmsg];
                fds.extend(self.seat.fds());
                fds.extend(self.units.fds());
                fds.extend(self.handout.fds());
                fds.extend(self.front.fds());
                fds.extend(self.capture.fds());
                sys::wait_any(&fds, WAIT_MS);
            }
        }
    }

    /// Stamp and admit records that did not come from the kernel log.
    fn admit_all(&mut self, records: Vec<Record>, now: Now, batch: &mut Vec<Record>) {
        for mut record in records {
            self.seat.stamp(&mut record);
            self.limiter.admit(record, now.monotonic_us, batch);
        }
    }

    /// A unit's line init wrote to the console: the trusted line init
    /// handed over for it, with the copy's kernel sequence number, or the
    /// copy itself when init handed none over. init writes the socket
    /// before the console, so a copy whose line has not been read yet finds
    /// it on the socket now.
    fn pair(&mut self, copy: Record, now: Now, batch: &mut Vec<Record>) -> Record {
        let copy = match self.units.pairing.console(copy) {
            Ok(paired) => return paired,
            Err(copy) => copy,
        };
        let mut more = Vec::new();
        self.units.serve(now, &mut more);
        self.admit_all(more, now, batch);
        match self.units.pairing.console(copy) {
            Ok(paired) => paired,
            Err(copy) => {
                self.units.pairing.stored_alone(&copy, now.monotonic_us);
                copy
            }
        }
    }

    /// Read what the kernel log has, up to 512 records, into `batch`.
    /// Answers whether one of them asked for a rotation, and whether the
    /// log was read to its end.
    fn read_kmsg(
        &mut self,
        now: Now,
        first_start: &mut bool,
        batch: &mut Vec<Record>,
    ) -> (bool, bool) {
        let mut buf = alloc::vec![0_u8; 8192];
        let mut rotate = false;
        let mut drained = false;
        for _ in 0..512 {
            match sys::read(self.kmsg, &mut buf) {
                Ok(0) => {
                    drained = true;
                    break;
                }
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
                    if units::is_unit_copy(&record) {
                        let mut paired = self.pair(record, now, batch);
                        self.seat.stamp(&mut paired);
                        self.limiter.admit(paired, now.monotonic_us, batch);
                        continue;
                    }
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
                Err(_) => {
                    drained = true;
                    break;
                }
            }
        }
        (rotate, drained)
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
        // Newest per class and owner: `class_of_name` of a user's path is
        // `user-<uid>/<class>`.
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
            // A compressed segment is sealed: never cut, never written to.
            let sealed = store::is_compressed(each.dir, &each.name);
            if scanned.good < bytes.len() && !sealed {
                cut(each, scanned.good, bytes.len(), said);
            }
            if let Some(found) = scanned.records.last() {
                self.next = self.next.max(found.record.seqnum + 1);
            }
            self.next = self.next.max(scanned.header.first_seqnum);
            let class = segment::class_of_name(access::base_name(&each.name)).unwrap_or_default();
            let owner = access::owner_of_path(&each.name);
            if scanned.header.boot_id == self.header.boot_id
                && !sealed
                && let Some(index) = self.classes.iter().position(|known| known.name == class)
                && let Ok(fd) = sys::openat(
                    each.dir,
                    each.name.as_bytes(),
                    sys::O_WRONLY | sys::O_APPEND | sys::O_CLOEXEC,
                    0,
                )
            {
                self.writers.push(Slot {
                    class: index,
                    owner,
                    writer: Writer {
                        fd,
                        name: each.name.clone(),
                        size: scanned.good,
                        created_us: scanned.header.created_us,
                        dirty: false,
                    },
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
        // Bytes per class and owner, in the order first seen.
        let mut bytes: Vec<(Key, u64, Vec<u8>)> = Vec::new();
        for mut record in records {
            // Every stored record says where its level came from.
            record.stamp_level_source();
            record.seqnum = self.next;
            seqnums.push(self.next);
            record.utc_offset_s = zone.offset_at(record.realtime_us);
            self.next += 1;
            let key = (
                retention::assign(&self.classes, &record),
                access::owner(&record),
            );
            match bytes.iter_mut().find(|(each, _, _)| *each == key) {
                Some((_, _, out)) => segment::encode_record(&record, out),
                None => {
                    let mut out = Vec::new();
                    segment::encode_record(&record, &mut out);
                    bytes.push((key, record.seqnum, out));
                }
            }
        }
        for (key, first, out) in &bytes {
            let at = self.ready(*key, *first)?;
            let Some(slot) = self.writers.get_mut(at) else {
                return Err(());
            };
            sys::write_all(slot.writer.fd, out).map_err(|_| ())?;
            slot.writer.size += out.len();
            slot.writer.dirty = true;
        }
        Ok(seqnums)
    }

    /// The writer of class and owner `key`, if there is one.
    fn slot_of(&self, key: Key) -> Option<usize> {
        self.writers
            .iter()
            .position(|slot| (slot.class, slot.owner) == key)
    }

    /// Make sure class and owner `key` has a segment with room, begun at
    /// `first` if a new one is needed; a full or day-old one is sealed
    /// first. Answers its writer's place.
    fn ready(&mut self, key: Key, first: u64) -> Result<usize, ()> {
        let now = sys::clock_us(sys::CLOCK_REALTIME);
        if let Some(at) = self.slot_of(key) {
            let stale = self.writers.get(at).is_some_and(|slot| {
                slot.writer.size >= SEGMENT_MAX
                    || now.saturating_sub(slot.writer.created_us) >= DAY_US
            });
            if !stale {
                return Ok(at);
            }
            self.seal(at);
        }
        let (index, owner) = key;
        let class = self
            .classes
            .get(index)
            .map(|class| class.name.clone())
            .ok_or(())?;
        let dir = self.dir_of(index);
        let mut header = self.header.clone();
        header.first_seqnum = first;
        header.created_us = now;
        let file = segment::file_name(&class, &header.boot_id, first);
        let name = match owner {
            Some(uid) => {
                // lode's, group adm (the store's set-group-id bit), 2750:
                // the user reads theirs through lode (`access`).
                let user = access::user_dir(uid);
                let _ = sys::mkdirat(dir, user.as_bytes(), 0o2750);
                alloc::format!("{user}/{file}")
            }
            None => file,
        };
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
        self.writers.push(Slot {
            class: index,
            owner,
            writer: Writer {
                fd,
                name,
                size: bytes.len(),
                created_us: now,
                dirty: true,
            },
        });
        self.vacuum();
        Ok(self.writers.len() - 1)
    }

    /// Seal the segment of writer `at`: synced and closed, the next record
    /// of its class and owner begins another.
    fn seal(&mut self, at: usize) {
        if at >= self.writers.len() {
            return;
        }
        let slot = self.writers.remove(at);
        let dir = self.dir_of(slot.class);
        let _ = sys::fdatasync(slot.writer.fd);
        sys::close(slot.writer.fd);
        store::write_summary(dir, &slot.writer.name);
        self.to_compress.push((dir, slot.writer.name));
    }

    /// Queue every sealed segment not yet compressed -- sealed by an older
    /// lode, or by one stopped before it compressed it.
    fn compress_old(&mut self) {
        let active: Vec<String> = self
            .writers
            .iter()
            .map(|slot| slot.writer.name.clone())
            .collect();
        for each in self.segments() {
            if !active.contains(&each.name) && !store::is_compressed(each.dir, &each.name) {
                self.to_compress.push((each.dir, each.name.clone()));
            }
        }
    }

    /// Compress a few frames of the segment being compressed, beginning the
    /// next queued one when there is none. Answers whether work is left.
    fn compress_some(&mut self) -> bool {
        while self.compressing.is_none() && !self.to_compress.is_empty() {
            let (dir, name) = self.to_compress.remove(0);
            let Ok(raw) = sys::read_file_from(dir, name.as_bytes(), 0) else {
                continue;
            };
            let size = raw.len() as u64;
            if let Some(compressor) = Compressor::new(raw) {
                self.compressing = Some(Sealing {
                    dir,
                    name,
                    compressor,
                    size,
                });
            }
        }
        let Some(job) = self.compressing.as_mut() else {
            return false;
        };
        for _ in 0..FRAMES_A_TURN {
            if job.compressor.step() {
                if let Some(job) = self.compressing.take() {
                    finish_compressing(job);
                }
                return !self.to_compress.is_empty();
            }
        }
        true
    }

    /// Summarise sealed segments that have no summary -- sealed by an older
    /// lode, or by one killed before it wrote it -- at most 64 a start.
    fn summarise_old(&self) {
        let active: Vec<&str> = self
            .writers
            .iter()
            .map(|slot| slot.writer.name.as_str())
            .collect();
        let mut done = 0;
        for each in self.segments().iter().rev() {
            if done >= 64 {
                break;
            }
            if active.contains(&each.name.as_str()) || store::summary(each).is_some() {
                continue;
            }
            store::write_summary(each.dir, &each.name);
            done += 1;
        }
    }

    /// Seal every class's segment and begin the next at once (`lode
    /// rotate`), so a segment `lode delete` may rewrite is never the one
    /// being written.
    fn rotate_all(&mut self) {
        let keys: Vec<Key> = self
            .writers
            .iter()
            .map(|slot| (slot.class, slot.owner))
            .collect();
        for key in keys {
            if let Some(at) = self.slot_of(key) {
                self.seal(at);
            }
            let _ = self.ready(key, self.next);
        }
    }

    /// `fdatasync` every written segment when `now` is far enough past the
    /// last, or `urgent`.
    fn sync_if(&mut self, urgent: bool, now: u64) {
        if !urgent && now.saturating_sub(self.synced_at) < SYNC_US {
            return;
        }
        for slot in &mut self.writers {
            if slot.writer.dirty {
                let _ = sys::fdatasync(slot.writer.fd);
                slot.writer.dirty = false;
            }
        }
        self.synced_at = now;
    }

    /// Remove what the classes and the stores' limits no longer keep: the
    /// system's segments in each directory against its limit, and each
    /// user's against a quarter of it.
    fn vacuum(&self) {
        let now = sys::clock_us(sys::CLOCK_REALTIME);
        let active: Vec<&str> = self
            .writers
            .iter()
            .map(|slot| slot.writer.name.as_str())
            .collect();
        let mut dirs = alloc::vec![(self.volatile, RUNTIME_MAX)];
        if let Some(fd) = self.persistent {
            dirs.push((fd, STORE_MAX));
        }
        for (dir, limit) in dirs {
            let segments = store::all_segments(&[Dir { fd: dir }]);
            let mut owners: Vec<Option<u32>> = segments
                .iter()
                .map(|each| access::owner_of_path(&each.name))
                .collect();
            owners.sort_unstable();
            owners.dedup();
            for owner in owners {
                let kept: Vec<Kept> = segments
                    .iter()
                    .filter(|each| access::owner_of_path(&each.name) == owner)
                    .map(|each| Kept {
                        class: String::from(
                            segment::class_of_name(access::base_name(&each.name))
                                .unwrap_or_default(),
                        ),
                        name: each.name.clone(),
                        created_us: store::header(each).map_or(0, |header| header.created_us),
                        size: store::size(each),
                        active: active.contains(&each.name.as_str()),
                    })
                    .collect();
                let limit = if owner.is_some() { limit / 4 } else { limit };
                for name in retention::vacuum(&self.classes, &kept, now, limit) {
                    let _ = sys::unlinkat(dir, name.as_bytes());
                    let _ = sys::unlinkat(dir, alloc::format!("{name}.sum").as_bytes());
                }
            }
        }
    }
}

/// Put a compressed segment in place of the sealed one it was made from:
/// a new file, synced, renamed over it, so a crash leaves one or the
/// other whole. Not when the segment changed meanwhile, nor when
/// compressing did not make it smaller.
fn finish_compressing(job: Sealing) {
    let segment = store::Segment {
        dir: job.dir,
        name: job.name.clone(),
        first: 0,
        passed: None,
    };
    if store::size(&segment) != job.size {
        return;
    }
    let packed = job.compressor.finish();
    if packed.len() as u64 >= job.size {
        return;
    }
    let fresh = access::temporary(&job.name, ".z.new");
    let Ok(fd) = sys::openat(
        job.dir,
        fresh.as_bytes(),
        sys::O_WRONLY | sys::O_CREAT | sys::O_TRUNC | sys::O_CLOEXEC,
        0o640,
    ) else {
        return;
    };
    let written = sys::write_all(fd, &packed).and_then(|()| sys::fdatasync(fd));
    sys::close(fd);
    if written.is_err() || store::size(&segment) != job.size {
        let _ = sys::unlinkat(job.dir, fresh.as_bytes());
        return;
    }
    let _ = sys::renameat(job.dir, fresh.as_bytes(), job.name.as_bytes());
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
