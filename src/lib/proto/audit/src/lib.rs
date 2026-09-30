//! The audit record: the 64 bytes the kernel keeps for each security
//! decision it makes, and hands a reader as they are
//! (`docs/certification/AUDIT.md` §2).
//!
//! The kernel's store (`src/kernel/src/audit.rs`) keeps records of this layout
//! in its two rings; a reader -- init's `audit.service` -- is handed them
//! byte for byte and writes them to disk. So the layout is here, where both
//! sides and the host tests can reach it, and not in the kernel.
//!
//! A record says when (the kernel's counter), what ([`Class`] and a code
//! within it), how it went ([`Outcome`] and a status), who (the pid and the
//! job, which the kernel attests, and beside them a uid the personality
//! supplied, which it does not), what about, and three words the event
//! defines. The start-up record carries the boot's whole audit id and the
//! two rings' lengths: [`Record::start`] and [`Record::start_fields`].
//!
//! Nothing here allocates, locks or knows the kernel.

#![no_std]
#![forbid(unsafe_code)]

#[cfg(test)]
mod tests;

/// What kind of decision a record is.
#[repr(u16)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// A call refused: rights that did not cover it, rights that would widen
    /// a handle, a limit reached.
    Refused = 1,
    /// Authority handed over: a delegation, a process made, a starter or a
    /// device's control channel given.
    Granted = 2,
    /// A process or job ended from outside.
    Ended = 3,
    /// A device quiesced, or a DMA fault its unit reported.
    Device = 4,
    /// TSF data changed by a call that succeeded: a limit set.
    Changed = 5,
    /// The audit function itself and the boot: start-up, configuration,
    /// the root switch, a power action.
    System = 6,
}

/// One event: its class, and what happened within it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    /// Its class, which decides the ring it is kept in.
    pub class: Class,
    /// What happened, numbered within the class.
    pub code: u16,
}

impl Event {
    /// An event of `class` numbered `code`.
    #[must_use]
    pub const fn new(class: Class, code: u16) -> Event {
        Event { class, code }
    }
}

/// The audit function started: its ring sizes and the boot's audit id.
pub const START: Event = Event::new(Class::System, 1);

/// One item of the boot's configuration, named by a [`Config`] key.
pub const CONFIG: Event = Event::new(Class::System, 2);

/// The boot finished bringing up the kernel: whether its self-checks ran.
pub const BOOTED: Event = Event::new(Class::System, 3);

/// A budget's refusals past the per-second limit, counted and not kept:
/// the count, then the limit, in the detail.
pub const SUPPRESSED: Event = Event::new(Class::Refused, 1);

/// A native call answered `ACCESS_DENIED`: the handle's rights did not
/// cover it. The target is the first handle named, the detail the call's
/// number and the rights asked, where it asks for some.
pub const RIGHTS: Event = Event::new(Class::Refused, 2);

/// A handle's rights asked widened by `handle_duplicate` or
/// `handle_replace`, and refused: as [`RIGHTS`].
pub const WIDEN: Event = Event::new(Class::Refused, 3);

/// A charge a job's limit refused. The target is the resource
/// ([`target::RESOURCE`], its `Resource` number); the detail the amount
/// asked, the limit, and the quota slot charged.
pub const LIMIT: Event = Event::new(Class::Refused, 4);

/// A native process made, with its creator's ids: the target is the new
/// process ([`target::PROCESS`]), the detail the job it was made in.
pub const PROCESS_MADE: Event = Event::new(Class::Granted, 1);

/// A job handle given for a cgroup (`job_for_cgroup`): the target is the
/// handle, the detail the call's number and the rights given.
pub const DELEGATED: Event = Event::new(Class::Granted, 2);

/// A device's control channel or ring made for a driver: the target is the
/// handle, the detail the call's number.
pub const CONTROL: Event = Event::new(Class::Granted, 3);

/// `devmgr` started by pid 1 through the kernel's starter
/// (`devmgr_start`): the target is the new `devmgr`, the detail the job its
/// drivers are made in.
pub const DEVMGR_STARTED: Event = Event::new(Class::Granted, 4);

/// The starter given to pid 1 on its bootstrap channel, with the kernel as
/// subject: the target is pid 1.
pub const STARTER_GIVEN: Event = Event::new(Class::Granted, 5);

/// The audit record's handle given to pid 1 on its bootstrap channel, with
/// the kernel as subject: the target is pid 1.
pub const READER_GIVEN: Event = Event::new(Class::Granted, 6);

/// A job ended by `job_kill`: the target is the job.
pub const JOB_KILLED: Event = Event::new(Class::Ended, 1);

/// A cgroup's processes ended by a write to its `cgroup.kill`: the target
/// is the job.
pub const CGROUP_KILLED: Event = Event::new(Class::Ended, 2);

/// A process ended by the scoped OOM kill: the target is the process, the
/// detail the job whose memory limit asked for it.
pub const OOM_KILLED: Event = Event::new(Class::Ended, 3);

/// A device quiesced before it was handed on: the target is the device.
pub const QUIESCED: Event = Event::new(Class::Device, 1);

/// A DMA fault an IOMMU reported for a device, recorded from the unit's
/// interrupt with the kernel as subject: the target is the requester, the
/// detail the faulting page's number, split in two words.
pub const DMA_FAULT: Event = Event::new(Class::Device, 2);

/// A job's limit set through its handle (`job_set_limit`): the target is
/// the resource, the detail the job's id, split in two words, and the
/// limit, saturated.
pub const LIMIT_SET: Event = Event::new(Class::Changed, 1);

/// A cgroup's limit file written (`pids.max`, `memory.max`, `cpu.weight`):
/// as [`LIMIT_SET`].
pub const CGROUP_LIMIT: Event = Event::new(Class::Changed, 2);

/// The resources a [`LIMIT`] or [`LIMIT_SET`] record names, by number.
pub mod resource {
    /// Memory, in bytes.
    pub const MEMORY: u64 = 1;
    /// Kernel objects.
    pub const OBJECTS: u64 = 2;
    /// Tasks.
    pub const TASKS: u64 = 3;
    /// The kernel heap part of memory, in bytes.
    pub const KERNEL: u64 = 4;
    /// The processor weight (`cpu.weight`).
    pub const CPU_WEIGHT: u64 = 5;
    /// The processor quota per period (`cpu.max`), in microseconds; the
    /// largest number for `max`.
    pub const CPU_MAX: u64 = 6;
}

/// A 64-bit amount in a 32-bit detail word: itself, or `u32::MAX` when it
/// does not fit.
#[must_use]
pub const fn saturated(amount: u64) -> u32 {
    if amount > u32::MAX as u64 {
        u32::MAX
    } else {
        amount as u32
    }
}

/// Where `/` is, as pid 1 is told (`docs/INIT.md` §7.3): the detail's
/// first word 1 when the root volume was switched to and pid 1 moved onto
/// it, 0 when `/` stays in memory.
pub const ROOT_SWITCHED: Event = Event::new(Class::System, 4);

/// A power action, recorded before it is taken: the detail's first word is
/// a [`power`] number. The last record the boot makes.
pub const POWER: Event = Event::new(Class::System, 5);

/// The power actions a [`POWER`] record names.
pub mod power {
    /// Power off.
    pub const OFF: u32 = 1;
    /// Halt.
    pub const HALT: u32 = 2;
    /// Restart.
    pub const RESTART: u32 = 3;
    /// A panic asked for when init exits (`ferrix.onexit=panic`).
    pub const PANIC: u32 = 4;
}

/// Every event this crate names, with its name.
pub const NAMED: [(Event, &str); 22] = [
    (START, "START"),
    (CONFIG, "CONFIG"),
    (BOOTED, "BOOTED"),
    (ROOT_SWITCHED, "ROOT_SWITCHED"),
    (POWER, "POWER"),
    (SUPPRESSED, "SUPPRESSED"),
    (RIGHTS, "RIGHTS"),
    (WIDEN, "WIDEN"),
    (LIMIT, "LIMIT"),
    (PROCESS_MADE, "PROCESS_MADE"),
    (DELEGATED, "DELEGATED"),
    (CONTROL, "CONTROL"),
    (DEVMGR_STARTED, "DEVMGR_STARTED"),
    (STARTER_GIVEN, "STARTER_GIVEN"),
    (READER_GIVEN, "READER_GIVEN"),
    (JOB_KILLED, "JOB_KILLED"),
    (CGROUP_KILLED, "CGROUP_KILLED"),
    (OOM_KILLED, "OOM_KILLED"),
    (QUIESCED, "QUIESCED"),
    (DMA_FAULT, "DMA_FAULT"),
    (LIMIT_SET, "LIMIT_SET"),
    (CGROUP_LIMIT, "CGROUP_LIMIT"),
];

/// What a record's target names, in its `target_kind`.
pub mod target {
    /// Nothing in particular.
    pub const NONE: u32 = 0;
    /// A job, by its id.
    pub const JOB: u32 = 1;
    /// A process, by its pid.
    pub const PROCESS: u32 = 2;
    /// A handle, by its value in the subject's table.
    pub const HANDLE: u32 = 3;
    /// A device: for a quiesce, its place in the kernel's device list; for
    /// a DMA fault, the requester's stream as the unit saw it (a VT-d source
    /// id, an `SMMUv3` stream id).
    pub const DEVICE: u32 = 4;
    /// A resource a job is charged for, by its number.
    pub const RESOURCE: u32 = 5;
}

/// Which item of the boot's configuration a [`CONFIG`] record states, in
/// its first detail word; the value is in the second.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Config {
    /// `ferrix.checks`: 1 when the self-checks run, 0 when `skip` was asked.
    Checks = 1,
    /// `ferrix.devmgr`: 0 when the kernel starts `devmgr`, 1 when pid 1 does.
    Devmgr = 2,
    /// The build's `--mitigations`: 1 when hardened.
    Mitigations = 3,
    /// KASLR: the loader's state word, and in the third detail word 1 when
    /// the layout was randomised from a source an attacker cannot predict.
    Kaslr = 4,
}

/// Whether the decision recorded went for the subject or against it.
#[repr(u16)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Done as asked, or a fact recorded.
    Done = 0,
    /// Refused.
    Refused = 1,
}

/// A record's uid the personality did not give.
pub const NO_UID: u32 = u32::MAX;

/// One record, 64 bytes, as a reader is given it: little-endian fields in
/// this order, with no padding.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
    /// Its place in its ring's sequence, from zero on each boot.
    pub sequence: u64,
    /// Nanoseconds since boot on the kernel's counter.
    pub time: u64,
    /// Its [`Class`].
    pub class: u16,
    /// Its event's code within the class.
    pub code: u16,
    /// Its [`Outcome`].
    pub outcome: u16,
    /// The status or errno answered, as a signed 16-bit number; 0 for none.
    pub status: i16,
    /// Who: the process, 0 for the kernel itself.
    pub pid: u32,
    /// The uid the personality gave for them, or [`NO_UID`]:
    /// personality-supplied data, never the kernel's identity of the
    /// subject.
    pub uid: u32,
    /// Their job, 0 for the kernel itself.
    pub job: u64,
    /// What the decision was about: its kind, numbered by the event.
    pub target_kind: u32,
    /// Which one, as two words so that the record has no padding.
    pub target_id: [u32; 2],
    /// Three words the event defines: the rights asked and held, a limit
    /// and the use, a configuration key and its value.
    pub detail: [u32; 3],
}

/// A record's length.
pub const RECORD_BYTES: usize = 64;

const _: () = assert!(
    size_of::<Record>() == RECORD_BYTES,
    "a record is 64 bytes, with no padding a reader would be handed"
);

/// The largest ring length a start-up record can carry.
pub const MAX_RING: usize = u16::MAX as usize;

impl Record {
    /// A slot no record has been written to.
    pub const EMPTY: Record = Record {
        sequence: 0,
        time: 0,
        class: 0,
        code: 0,
        outcome: 0,
        status: 0,
        pid: 0,
        uid: 0,
        job: 0,
        target_kind: 0,
        target_id: [0; 2],
        detail: [0; 3],
    };

    /// What it is about, whole.
    #[must_use]
    pub const fn target(&self) -> u64 {
        (self.target_id[0] as u64) | ((self.target_id[1] as u64) << 32)
    }

    /// Set what it is about.
    pub const fn set_target(&mut self, id: u64) {
        self.target_id = [id as u32, (id >> 32) as u32];
    }

    /// Whether it is a record of `event`: its class and its code, since
    /// codes are numbered within a class.
    #[must_use]
    pub const fn is(&self, event: Event) -> bool {
        self.class == event.class as u16 && self.code == event.code
    }

    /// The start-up record's fields, before the ring numbers it and the
    /// clock stamps it: the audit id's low half as what it is about, its
    /// high half in the first two detail words, and the two rings' lengths
    /// in the third, the high-value ring's in its upper half. `None` if a
    /// length does not fit its half word.
    #[must_use]
    pub const fn start(id: u128, high: usize, refusals: usize) -> Option<Record> {
        if high > MAX_RING || refusals > MAX_RING {
            return None;
        }
        let mut record = Record::EMPTY;
        record.class = START.class as u16;
        record.code = START.code;
        record.uid = NO_UID;
        record.set_target(id as u64);
        record.detail = [
            (id >> 64) as u32,
            (id >> 96) as u32,
            ((high as u32) << 16) | refusals as u32,
        ];
        Some(record)
    }

    /// What a start-up record says: the audit id and the two rings'
    /// lengths. `None` for a record of any other event.
    #[must_use]
    pub const fn start_fields(&self) -> Option<(u128, usize, usize)> {
        if !self.is(START) {
            return None;
        }
        let id = (self.target() as u128)
            | ((self.detail[0] as u128) << 64)
            | ((self.detail[1] as u128) << 96);
        let high = (self.detail[2] >> 16) as usize;
        let refusals = (self.detail[2] & 0xFFFF) as usize;
        Some((id, high, refusals))
    }

    /// Its event's name, as a reader prints it: the constant's, or
    /// `UNKNOWN` for a class and code this crate does not know.
    #[must_use]
    pub fn event_name(&self) -> &'static str {
        NAMED
            .iter()
            .find(|(event, _)| self.is(*event))
            .map_or("UNKNOWN", |(_, name)| name)
    }

    /// Its bytes as a reader is given them.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; RECORD_BYTES] {
        let mut out = [0_u8; RECORD_BYTES];
        let mut at = 0;
        let mut put = |bytes: &[u8]| {
            if let Some(slot) = out.get_mut(at..at + bytes.len()) {
                slot.copy_from_slice(bytes);
            }
            at += bytes.len();
        };
        put(&self.sequence.to_le_bytes());
        put(&self.time.to_le_bytes());
        put(&self.class.to_le_bytes());
        put(&self.code.to_le_bytes());
        put(&self.outcome.to_le_bytes());
        put(&self.status.to_le_bytes());
        put(&self.pid.to_le_bytes());
        put(&self.uid.to_le_bytes());
        put(&self.job.to_le_bytes());
        put(&self.target_kind.to_le_bytes());
        for word in self.target_id.iter().chain(&self.detail) {
            put(&word.to_le_bytes());
        }
        out
    }

    /// A record from the bytes a reader was given.
    #[must_use]
    pub fn from_bytes(bytes: &[u8; RECORD_BYTES]) -> Record {
        let mut at = 0;
        let mut take = |count: usize| {
            let mut word = [0_u8; 8];
            if let (Some(to), Some(from)) = (word.get_mut(..count), bytes.get(at..at + count)) {
                to.copy_from_slice(from);
            }
            at += count;
            u64::from_le_bytes(word)
        };
        let sequence = take(8);
        let time = take(8);
        let class = take(2) as u16;
        let code = take(2) as u16;
        let outcome = take(2) as u16;
        let status = take(2) as u16 as i16;
        let pid = take(4) as u32;
        let uid = take(4) as u32;
        let job = take(8);
        let target_kind = take(4) as u32;
        let target_id = [take(4) as u32, take(4) as u32];
        let detail = [take(4) as u32, take(4) as u32, take(4) as u32];
        Record {
            sequence,
            time,
            class,
            code,
            outcome,
            status,
            pid,
            uid,
            job,
            target_kind,
            target_id,
            detail,
        }
    }
}
