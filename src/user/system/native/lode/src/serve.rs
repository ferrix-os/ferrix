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

use ferrix_lode_core::alert::{self, Alert, Alerts};
use ferrix_lode_core::compress::{self, Compressor};
use ferrix_lode_core::events::Events;
use ferrix_lode_core::ingest::{self, KernelSequence, Now, Taken};
use ferrix_lode_core::limit::{self, Limiter, Rule};
use ferrix_lode_core::record::Record;
use ferrix_lode_core::retention::{self, Class, DAY_US, Kept};
use ferrix_lode_core::segment::{self, Header};
use ferrix_lode_core::time::Zone;
use ferrix_lode_core::{access, units};
use ferrix_lode_core::{console, kmsg};
use ferrix_native_abi::directory::{MAX_MESSAGE, Message};
use ferrix_rt::Kernel;
use ferrix_rt::native::channel::Channel;
use ferrix_rt::native::{Deadline, Object, Signals};

use crate::alerting::Alerting;
use crate::capture::Capture;
use crate::control::{self, Control, Request};
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
    /// The store directory it is in.
    dir: usize,
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
    let control = Control::open();
    let alerting = Alerting::open();
    let capture = Capture::open();
    let dropped = sys::become_ids(UID, GID).is_ok();
    let zone = zone::local();
    let now = clocks();
    let mut said: Vec<Record> = Vec::new();
    let Configuration {
        rules,
        classes,
        alerts,
        events,
        blob_quota,
    } = configuration(&mut said, now);
    let tombstones = ferrix_lode_core::tombstone::Tombstones::new(u64::from(
        events.count("tombstone").unwrap_or(5),
    ));
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
    let mut first = ingest::own(if restarted { 4 } else { 6 }, id, &text, now);
    boot_facts(&mut first);
    said.insert(0, first);
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
        control,
        alerts: Alerts::new(alerts),
        events,
        echoes: units::Echoes::default(),
        blobs: {
            let mut blobs = crate::blobs::Blobs::new();
            blobs.quota = blob_quota;
            blobs
        },
        blobs_swept: 0,
        tombstones,
        alerting,
        identities: alloc::collections::VecDeque::new(),
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
    /// What root asks: sync, flush, rotate.
    control: Control,
    /// The alerts, and what each has counted.
    alerts: Alerts,
    /// Which kinds of event are on (`events.conf`).
    events: Events,
    /// Echoed stream lines waiting for their copies as units' lines.
    echoes: units::Echoes,
    /// The blob container.
    blobs: crate::blobs::Blobs,
    /// When it was last swept, monotonic microseconds.
    blobs_swept: u64,
    /// This boot's tombstones.
    tombstones: ferrix_lode_core::tombstone::Tombstones,
    /// Their actions.
    alerting: Alerting,
    /// Who the processes a fault ended were, as the kernel said, until
    /// their crash records come.
    identities: alloc::collections::VecDeque<console::Identity>,
}

/// What `/etc/ferrix/lode.d` says.
struct Configuration {
    /// The limiter's rules.
    rules: Vec<Rule>,
    /// The retention classes.
    classes: Vec<Class>,
    /// The alerts.
    alerts: Vec<Alert>,
    /// Which kinds of event are on.
    events: Events,
    /// The blob container's quota.
    blob_quota: u64,
}

/// The rules, the retention classes, the alerts and the events, with a
/// record for each file that did not read.
fn configuration(said: &mut Vec<Record>, now: Now) -> Configuration {
    let mut rules = Vec::new();
    let mut alerts = Vec::new();
    let mut classes = retention::defaults();
    let mut events = Events::default();
    let mut blob_quota = retention::BLOB_QUOTA;
    let Ok(dir) = Dir::open(CONFIG) else {
        return Configuration {
            rules,
            classes,
            alerts,
            events,
            blob_quota,
        };
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
                .and_then(|more| alert::parse(&text).map(|found| (more, found)))
                .map(|(more, found)| {
                    rules.extend(more);
                    alerts.extend(found);
                })
                .err()
        } else if name == b"retention.conf" {
            blob_quota = retention::blob_quota(&text);
            retention::parse(&text).map(|read| classes = read).err()
        } else if name == b"events.conf" {
            events.apply(&text, "/etc/ferrix/lode.d/events.conf").err()
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
    Configuration {
        rules,
        classes,
        alerts,
        events,
        blob_quota,
    }
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
            // The sockets before init's lines and the kernel log: an echoed
            // stream line waits there for its copy as a unit's line.
            let mut heard = Vec::new();
            self.front.serve(now, &mut heard);
            for mut record in heard {
                self.seat.stamp(&mut record);
                if record.get("FERRIX_ECHOED").is_some() {
                    let mut out = Vec::new();
                    self.echoes.echoed(record, now.monotonic_us, &mut out);
                    self.admit_all(out, now, &mut batch);
                } else {
                    self.admit(record, now, &mut batch);
                }
            }
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
                let mut echoes = Vec::new();
                self.echoes.expire(now.monotonic_us, &mut echoes);
                for mut record in echoes {
                    self.seat.stamp(&mut record);
                    self.admit_now(record, now, &mut batch);
                }
                self.admit_all(expired, now, &mut batch);
            }
            if rotate {
                self.limiter.flush(now.monotonic_us, &mut batch);
            }
            self.limiter.tick(now.monotonic_us, &mut batch);
            let urgent = batch.iter().any(|record| record.priority <= 2);
            let idle = batch.is_empty();
            self.store_batch(batch, now)?;
            if rotate {
                self.store.rotate_all();
            }
            self.store.sync_if(urgent, now.monotonic_us);
            self.requests(now)?;
            let mut answered = Vec::new();
            self.alerting.serve(now, &mut answered);
            let _ = self
                .store
                .append(answered, &self.zone)
                .map_err(|()| Step::Segment)?;
            let compressing = self.store.compress_some();
            self.sweep_blobs(now);
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
                fds.extend(self.control.fds());
                fds.extend(self.alerting.fds());
                fds.extend(self.front.fds());
                fds.extend(self.capture.fds());
                sys::wait_any(&fds, WAIT_MS);
            }
        }
    }

    /// Carry out what root asked on the control socket, each after what
    /// was read before it is stored.
    fn requests(&mut self, now: Now) -> Result<(), Step> {
        for (fd, request) in self.control.poll(now.monotonic_us) {
            let result = match request {
                Request::Sync => {
                    self.store.sync_if(true, now.monotonic_us);
                    Ok(String::new())
                }
                Request::Rotate => {
                    let mut held = Vec::new();
                    self.limiter.flush(now.monotonic_us, &mut held);
                    let _ = self
                        .store
                        .append(held, &self.zone)
                        .map_err(|()| Step::Segment)?;
                    self.store.rotate_all();
                    Ok(String::new())
                }
                Request::Flush => self.store.flush().map(|()| String::new()),
                Request::Reload => Ok(self.reload(now)),
            };
            control::answer(fd, result);
        }
        Ok(())
    }

    /// A record that arrived: counted by the alerts before the limiter, so
    /// a flood it holds back is still seen; then the limiter's. An alert
    /// that fires is said in a record of its own and carried out.
    fn admit(&mut self, record: Record, now: Now, batch: &mut Vec<Record>) {
        // A unit's line a program also sent on its own stream is kept once,
        // as the stream's.
        let echo_open = self.front.echo_open();
        let Some(record) = self.echoes.unit_line(record, now.monotonic_us, echo_open) else {
            // Held for its echo, which [`units::Echoes::expire`] gives up on.
            return;
        };
        self.admit_now(record, now, batch);
    }

    /// [`admit`] past the echoes: names, alerts, the limiter.
    fn admit_now(&mut self, mut record: Record, now: Now, batch: &mut Vec<Record>) {
        // Every record that names a process names it: its `_COMM`, `_EXE`
        // and `_CMDLINE` (`events.conf`'s process-names).
        if self.events.on("process-names")
            && let Some(pid) = record
                .get_str("_PID")
                .and_then(|pid| pid.parse::<u32>().ok())
        {
            self.front.name(&mut record, pid, now, false);
            if record.get("_EXE").is_none()
                && let Some(path) = self.units.program(pid)
            {
                record.push("_EXE", path);
            }
        }
        if !self.alerts.is_empty() {
            let mut fired = Vec::new();
            self.alerts.observe(&record, now.monotonic_us, &mut fired);
            for each in fired {
                batch.push(each.record(now.realtime_us, now.monotonic_us));
                self.alerting.act(&each, now, batch);
            }
        }
        self.limiter.admit(record, now.monotonic_us, batch);
    }

    /// Store a batch: stamped, its blobs written first, then the records;
    /// then what follows from them -- tombstones for its crashes and
    /// screenshots asked for -- stored after.
    fn store_batch(&mut self, mut batch: Vec<Record>, now: Now) -> Result<(), Step> {
        for record in &mut batch {
            if record.realtime_us == 0 {
                record.realtime_us = now.realtime_of(record.monotonic_us);
            }
        }
        let wanted: Vec<(usize, String)> = batch
            .iter()
            .enumerate()
            .filter(|(_, record)| self.events.on("capture") && self.capture.wants(record))
            .map(|(index, record)| {
                let text = String::from_utf8_lossy(record.message());
                (index, text.chars().take(160).collect())
            })
            .collect();
        // Which records name a process, and which are crashes, for the
        // tombstones made once their sequence numbers are known.
        let pids: Vec<Option<u32>> = batch
            .iter()
            .map(|record| record.get_str("_PID").and_then(|pid| pid.parse().ok()))
            .collect();
        let crashes: Vec<(usize, Record)> = batch
            .iter()
            .enumerate()
            .filter(|(_, record)| record.get_str("MESSAGE_ID") == Some(ingest::MESSAGE_ID_CRASH))
            .map(|(index, record)| (index, record.clone()))
            .collect();
        // A batch's blobs are on disk before the records that name them.
        self.flush_blobs(&mut batch, now);
        let seqnums = self
            .store
            .append(batch, &self.zone)
            .map_err(|()| Step::Segment)?;
        let mut more: Vec<Record> = Vec::new();
        for (pid, seqnum) in pids.iter().zip(&seqnums) {
            if let Some(pid) = pid {
                self.tombstones.stored(*pid, *seqnum);
            }
        }
        if self.events.on("tombstone") {
            self.tombstones_for(crashes, &seqnums, now, &mut more);
        }
        for (index, what) in wanted {
            if let Some(&seqnum) = seqnums.get(index) {
                self.capture.ask(seqnum, &what, now, &mut more);
            }
        }
        self.capture.serve(now, self.seat.sessions(), &mut more);
        self.flush_blobs(&mut more, now);
        let _ = self
            .store
            .append(more, &self.zone)
            .map_err(|()| Step::Segment)?;
        Ok(())
    }

    /// The tombstones for the `crashes` of a batch stored as `seqnums`.
    fn tombstones_for(
        &mut self,
        crashes: Vec<(usize, Record)>,
        seqnums: &[u64],
        now: Now,
        out: &mut Vec<Record>,
    ) {
        let boot = self.store.header.boot_id;
        for (index, crash) in crashes {
            let Some(&seqnum) = seqnums.get(index) else {
                continue;
            };
            out.extend(self.tombstones.for_crash(&crash, seqnum, &boot, now));
        }
    }

    /// Mark and sweep the blob container, at most once an hour: every blob
    /// the segments' summaries list, and the records of the segments being
    /// written, is live; packs with nothing live go, packs half dead are
    /// rewritten (`docs/LOGGING.md` §21.4).
    fn sweep_blobs(&mut self, now: Now) {
        if now.monotonic_us.saturating_sub(self.blobs_swept) < 3_600_000_000 {
            return;
        }
        self.blobs_swept = now.monotonic_us;
        let mut dirs = alloc::vec![Dir {
            fd: self.store.volatile
        }];
        dirs.extend(self.store.persistent.map(|fd| Dir { fd }));
        let mut live = alloc::collections::BTreeSet::new();
        for segment in store::all_segments(&dirs) {
            match store::summary(&segment) {
                Some(summary) => live.extend(summary.blobs),
                None => {
                    let bytes = store::bytes(&segment, 0).unwrap_or_default();
                    live.extend(
                        segment::entries(&bytes)
                            .0
                            .iter()
                            .flat_map(ferrix_lode_core::blob::references)
                            .map(|(_, reference)| reference.id),
                    );
                }
            }
        }
        let _ = self.blobs.sweep(&live, now.realtime_us / 1_000_000);
    }

    /// Write the queued blobs, the capture's included, to the pack in the
    /// directory the store writes to; a record whose blob could not be
    /// written says so (`BLOB_DROPPED`), never names what is not there.
    fn flush_blobs(&mut self, batch: &mut [Record], _now: Now) {
        let mut failed = Vec::new();
        for (id, kind, bytes) in self.capture.take_blobs() {
            if self.blobs.submit(id, &kind, bytes).is_err() {
                failed.push(id);
            }
        }
        let store = self.store.persistent.unwrap_or(self.store.volatile);
        failed.extend(self.blobs.flush(store));
        if failed.is_empty() {
            return;
        }
        for record in batch.iter_mut() {
            let names: Vec<String> = ferrix_lode_core::blob::references(record)
                .into_iter()
                .filter(|(_, reference)| failed.contains(&reference.id))
                .map(|(name, _)| name)
                .collect();
            for name in names {
                record.fields.retain(|field| field.name != name);
                record.push(
                    "BLOB_DROPPED",
                    alloc::format!("{name}: not written").as_bytes(),
                );
            }
        }
    }

    /// `lode reload`: the rules, the alerts and the events read again; what
    /// the old rules held is said first. Retention classes take a restart,
    /// since segments are being written per class.
    fn reload(&mut self, now: Now) -> String {
        let mut said = Vec::new();
        self.limiter.flush(now.monotonic_us, &mut said);
        let configuration = configuration(&mut said, now);
        let rules = configuration.rules.len();
        self.blobs.quota = configuration.blob_quota;
        // A new quota is kept at the next turn of the loop, not in an hour.
        self.blobs_swept = 0;
        let alerts = configuration.alerts.len();
        self.limiter = Limiter::new(configuration.rules);
        self.alerts = Alerts::new(configuration.alerts);
        // The kinds whose switch, level or count changed.
        let changed: Vec<String> = configuration
            .events
            .settings
            .iter()
            .filter(|new| !self.events.settings.contains(new))
            .map(|new| alloc::format!("{} {}", new.name, if new.on { "on" } else { "off" }))
            .collect();
        self.events = configuration.events;
        // Retention classes: what is being written is sealed under the old
        // classes, and new segments begin under the new.
        // Compared as written out: a class's filter has no equality of its own.
        let classes_changed = alloc::format!("{:?}", self.store.classes)
            != alloc::format!("{:?}", configuration.classes);
        if classes_changed {
            self.store.replace_classes(configuration.classes);
        }
        let capture = self.capture.reload();
        let text = alloc::format!(
            "reloaded: {rules} rule(s), {alerts} alert(s); retention {}; capture {capture}; events {}",
            if classes_changed {
                "changed, segments sealed"
            } else {
                "unchanged"
            },
            if changed.is_empty() {
                String::from("unchanged")
            } else {
                changed.join(", ")
            }
        );
        said.push(ingest::own(
            5,
            ingest::MESSAGE_ID_STARTED,
            &alloc::format!("lode {text}"),
            now,
        ));
        let _ = self.store.append(said, &self.zone);
        text
    }

    /// Keep the kernel's identity line in `text`, if it is one, for the
    /// crash record that follows it.
    fn remember_identity(&mut self, text: &[u8]) {
        let Some(who) = console::identity(text) else {
            return;
        };
        if self.identities.len() >= 64 {
            let _ = self.identities.pop_front();
        }
        self.identities.push_back(who);
    }

    /// The identity kept for the crash record `record`, taken.
    fn identity_of(&mut self, record: &Record) -> Option<console::Identity> {
        let pid: u32 = record.get_str("CRASH_PID")?.parse().ok()?;
        let at = self.identities.iter().rposition(|who| who.pid == pid)?;
        self.identities.remove(at)
    }

    /// Stamp and admit records that did not come from the kernel log.
    fn admit_all(&mut self, records: Vec<Record>, now: Now, batch: &mut Vec<Record>) {
        for mut record in records {
            self.seat.stamp(&mut record);
            self.admit(record, now, batch);
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
                    // Who a fault ended, said just before its end: kept for
                    // the crash record that follows.
                    if record.get("_TRANSPORT") == Some(b"kernel") {
                        self.remember_identity(&kernel.text);
                    }
                    if units::is_unit_copy(&record) {
                        let mut paired = self.pair(record, now, batch);
                        self.seat.stamp(&mut paired);
                        self.admit(paired, now, batch);
                        continue;
                    }
                    self.seat.stamp(&mut record);
                    if record.get("_TRANSPORT") == Some(b"kernel")
                        && record.get("CRASH_PID").is_some()
                    {
                        let known = self
                            .identity_of(&record)
                            .filter(|_| self.events.on("crash-names"));
                        batch.push(with_program(record, known));
                    } else {
                        self.admit(record, now, batch);
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

/// A crash record with who the process was: from the kernel's identity
/// line, read while the process still ran (its name, the path it was
/// started from, the file and offset of the program counter, its command
/// line), else from `/proc` if the process is still there to read.
fn with_program(mut record: Record, known: Option<console::Identity>) -> Record {
    if let Some(who) = known {
        record.push("CRASH_COMM", who.comm.as_bytes());
        record.push("CRASH_EXE", who.exe.as_bytes());
        if let Some(module) = &who.module {
            record.push("CRASH_MODULE", module.as_bytes());
        }
        if !who.cmdline.is_empty() {
            record.push("CRASH_CMDLINE", who.cmdline.as_bytes());
        }
        let program = who.exe.split(" (deleted)").next().unwrap_or_default();
        record.push("CRASH_PROGRAM", program.as_bytes());
        return record;
    }
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

/// What the boot record says of the boot (`docs/LOGGING.md` §18 "The boot
/// record"): the kernel's command line, a deploy's nonce on it
/// (`n150.deploy=`), and the image's commit from `/etc/ferrix/image`.
fn boot_facts(record: &mut Record) {
    if let Some(line) = sys::read_small(b"/proc/cmdline") {
        let line = line.strip_suffix(b"\n").unwrap_or(&line);
        record.push("BOOT_CMDLINE", line);
        for word in line.split(|&byte| byte == b' ') {
            if let Some(nonce) = word.strip_prefix(b"n150.deploy=") {
                record.push("BOOT_DEPLOY", nonce);
            }
        }
    }
    if let Some(image) = sys::read_small(b"/etc/ferrix/image") {
        for line in image.split(|&byte| byte == b'\n') {
            if let Some(commit) = line.strip_prefix(b"commit=") {
                record.push("BOOT_IMAGE", commit);
            }
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
                    dir: each.dir,
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
            dir,
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
        let dir = slot.dir;
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

    /// `journalctl --flush`: take `/var/log/journal` (made by the caller,
    /// root, when it was missing) if lode is still in `/run`, seal what is
    /// being written there, and move every sealed segment of a class that
    /// is not RAM-only over, a copy synced before the original goes.
    fn flush(&mut self) -> Result<(), String> {
        let persistent = match self.persistent {
            Some(fd) => fd,
            None => {
                let dir = Dir::open(store::PERSISTENT)
                    .map_err(|error| alloc::format!("/var/log/journal: errno {}", error.0))?;
                if !writable(dir.fd) {
                    sys::close(dir.fd);
                    return Err(String::from("/var/log/journal cannot be written by lode"));
                }
                self.persistent = Some(dir.fd);
                dir.fd
            }
        };
        let moving: Vec<usize> = (0..self.writers.len())
            .rev()
            .filter(|&at| {
                self.writers.get(at).is_some_and(|slot| {
                    slot.dir == self.volatile
                        && !self
                            .classes
                            .get(slot.class)
                            .is_some_and(|class| class.ram_only)
                })
            })
            .collect();
        for at in moving {
            self.seal(at);
        }
        if self
            .compressing
            .as_ref()
            .is_some_and(|job| job.dir == self.volatile)
        {
            self.compressing = None;
        }
        let active: Vec<String> = self
            .writers
            .iter()
            .map(|slot| slot.writer.name.clone())
            .collect();
        for each in store::all_segments(&[Dir { fd: self.volatile }]) {
            let class = segment::class_of_name(access::base_name(&each.name)).unwrap_or_default();
            let ram_only = self
                .classes
                .iter()
                .any(|known| known.name == class && known.ram_only);
            if ram_only || active.contains(&each.name) {
                continue;
            }
            move_segment(self.volatile, persistent, &each.name)?;
            self.to_compress
                .retain(|(dir, name)| !(*dir == self.volatile && *name == each.name));
        }
        self.vacuum();
        Ok(())
    }

    /// Seal every class's segment and begin the next at once (`lode
    /// rotate`), so a segment `lode delete` may rewrite is never the one
    /// being written.
    /// Take new retention classes: every segment being written is sealed
    /// first, so no segment holds records of two sets of classes.
    fn replace_classes(&mut self, classes: Vec<Class>) {
        while !self.writers.is_empty() {
            self.seal(0);
        }
        self.classes = classes;
    }

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

/// Whether lode can make a file in the directory `dir`.
fn writable(dir: usize) -> bool {
    let probe = b".lode-probe";
    match sys::openat(
        dir,
        probe,
        sys::O_WRONLY | sys::O_CREAT | sys::O_CLOEXEC,
        0o600,
    ) {
        Ok(fd) => {
            sys::close(fd);
            let _ = sys::unlinkat(dir, probe);
            true
        }
        Err(_) => false,
    }
}

/// Move the segment `name` and its summary from the store `from` to the
/// store `to`: written there compressed and synced, then removed here. It
/// is compressed on the way, at once, so the disk is written once: a RAM
/// tier is at most 64 MiB, and `--flush` is asked for once a boot.
fn move_segment(from: usize, to: usize, name: &str) -> Result<(), String> {
    if let Some((user, _)) = name.split_once('/') {
        let _ = sys::mkdirat(to, user.as_bytes(), 0o2750);
    }
    for each in [String::from(name), alloc::format!("{name}.sum")] {
        let Ok(mut bytes) = sys::read_file_from(from, each.as_bytes(), 0) else {
            continue;
        };
        if each == name && !compress::is_compressed(bytes.get(..8).unwrap_or_default()) {
            let raw = bytes.len();
            bytes = compress::compress(bytes.clone())
                .filter(|packed| packed.len() < raw)
                .unwrap_or(bytes);
        }
        let fresh = access::temporary(&each, ".move");
        let fd = sys::openat(
            to,
            fresh.as_bytes(),
            sys::O_WRONLY | sys::O_CREAT | sys::O_TRUNC | sys::O_CLOEXEC,
            0o640,
        )
        .map_err(|error| alloc::format!("{each}: errno {}", error.0))?;
        let written = sys::write_all(fd, &bytes).and_then(|()| sys::fdatasync(fd));
        sys::close(fd);
        written.map_err(|error| alloc::format!("{each}: errno {}", error.0))?;
        sys::renameat(to, fresh.as_bytes(), each.as_bytes())
            .map_err(|error| alloc::format!("{each}: errno {}", error.0))?;
    }
    let _ = sys::unlinkat(from, name.as_bytes());
    let _ = sys::unlinkat(from, alloc::format!("{name}.sum").as_bytes());
    Ok(())
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
