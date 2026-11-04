//! What each first process is handed, one const table per role
//! (`docs/microkernel/BOOT-AUTHORITY.md` §3, BA.5, L.object.321).
//!
//! The kernel takes every right it gives a first process from here, in a const
//! expression, so that a kind of handle not in a role's row does not compile, and
//! `check-first-rows.py` reads these tables against the design's and these call sites
//! against these tables. No row holds the root job: there is no such kind.

use ferrix_native_abi::rights::{IMAGE_RIGHTS, Rights};
use ferrix_native_abi::types::CHANNEL_MAX_HANDLES;

use crate::discovery::devmgr::{GIVEN_DEVICE_RIGHTS, KEPT_DEVICE_RIGHTS};
#[cfg(ferrix_personality = "user")]
use crate::object::Object;

/// A kind of handle a first process may hold.
#[derive(Clone, Copy, Debug)]
pub enum Held {
    /// The job its children go in: never the root's.
    Job,
    /// The debug log's write handle.
    DebugLog,
    /// The partitions description, sealed.
    Description,
    /// A program's image, sealed.
    Image,
    /// A device as a driver is given it.
    DeviceGiven,
    /// A device as its keeper holds it.
    DeviceKept,
    /// One end of a channel the kernel made.
    Channel,
    /// Its restricted region.
    Region,
    /// The initramfs, read-only.
    Archive,
    /// The console, read-only.
    Console,
}

/// One kind of handle and what it may do.
#[derive(Debug)]
pub struct Row {
    /// The kind.
    pub held: Held,
    /// Its rights.
    pub rights: Rights,
    /// How many of it this role may hold: what bounds a role's count.
    pub most: usize,
}

/// Whether `object` is of the kind `held` names, and not the root job.
#[cfg(ferrix_personality = "user")]
fn is_of(object: &Object, held: Held) -> bool {
    match (object, held) {
        (Object::Job(job), Held::Job) => !job.is_root(),
        (Object::DebugLog, Held::DebugLog)
        | (Object::Channel(_), Held::Channel)
        | (Object::Device(_), Held::DeviceGiven | Held::DeviceKept)
        | (Object::Vmo(_), Held::Description | Held::Image | Held::Archive)
        | (Object::Console, Held::Console)
        | (Object::Region(_), Held::Region) => true,
        _ => false,
    }
}

/// A role's whole table.
#[derive(Debug)]
pub struct Rows(&'static [Row]);

impl Rows {
    /// The rights of `held` in this row: not in it, and this does not compile, since
    /// every use is in a const expression.
    #[must_use]
    pub const fn of(&self, held: Held) -> Rights {
        let mut at = 0;
        while at < self.0.len() {
            if self.0[at].held as u8 == held as u8 {
                return self.0[at].rights;
            }
            at += 1;
        }
        panic!("a handle outside its first process's row")
    }

    /// Hold what a first process is about to be given to this row, at the kernel's hand,
    /// from the objects themselves rather than the calls that made them (L.object.321):
    /// every handle of a kind and rights the row names, none the root job, and no kind past
    /// its number. Refused, the boot stops, in every image.
    ///
    /// # Errors
    ///
    /// What was out of its row.
    #[cfg(ferrix_personality = "user")]
    pub fn judge<'a>(
        &self,
        given: impl Iterator<Item = (&'a Object, Rights)>,
    ) -> Result<(), &'static str> {
        let mut counts = [0_usize; MOST_ROWS];
        for (object, rights) in given {
            let found = self
                .0
                .iter()
                .position(|row| row.rights.0 == rights.0 && is_of(object, row.held));
            let Some(at) = found else {
                return Err("a first process was given a handle outside its row");
            };
            let count = counts.get_mut(at).ok_or("a row longer than its count")?;
            *count += 1;
            if *count > self.0.get(at).map_or(0, |row| row.most) {
                return Err("a first process was given more of a handle than its row says");
            }
        }
        Ok(())
    }
}

/// The most kinds a row has.
#[cfg(ferrix_personality = "user")]
const MOST_ROWS: usize = 8;
/// As a channel message holds.
const MANY: usize = CHANNEL_MAX_HANDLES;

const LOG_TO_PASS_ON: Rights = Rights(Rights::WRITE.0 | Rights::DUPLICATE.0 | Rights::TRANSFER.0);
const ARCHIVE: Rights = Rights(Rights::VMO.0 & !Rights::WRITE.0);

/// The partition manager's.
pub const MANAGER: Rows = Rows(&[
    Row {
        held: Held::Job,
        rights: Rights::JOB,
        most: 1,
    },
    Row {
        held: Held::DebugLog,
        rights: LOG_TO_PASS_ON,
        most: 1,
    },
    Row {
        held: Held::Description,
        rights: Rights(Rights::READ.0 | Rights::MAP.0 | Rights::TRANSFER.0),
        most: 1,
    },
    Row {
        held: Held::Image,
        rights: IMAGE_RIGHTS,
        most: MANY,
    },
    Row {
        held: Held::DeviceKept,
        rights: KEPT_DEVICE_RIGHTS,
        most: MANY,
    },
]);

/// `devmgr`'s.
pub const DEVMGR: Rows = Rows(&[
    Row {
        held: Held::Job,
        rights: Rights::JOB,
        most: 1,
    },
    Row {
        held: Held::DebugLog,
        rights: LOG_TO_PASS_ON,
        most: 1,
    },
    Row {
        held: Held::Image,
        rights: IMAGE_RIGHTS,
        most: MANY,
    },
    Row {
        held: Held::DeviceGiven,
        rights: GIVEN_DEVICE_RIGHTS,
        most: MANY,
    },
    Row {
        held: Held::DeviceKept,
        rights: KEPT_DEVICE_RIGHTS,
        most: MANY,
    },
    Row {
        held: Held::Channel,
        rights: Rights::CHANNEL,
        most: 3,
    },
]);

/// The first `linuxd`'s.
pub const LINUXD: Rows = Rows(&[
    Row {
        held: Held::Job,
        rights: Rights::JOB,
        most: 1,
    },
    Row {
        held: Held::DebugLog,
        rights: Rights::WRITE,
        most: 1,
    },
    Row {
        held: Held::Region,
        rights: Rights::REGION,
        most: 1,
    },
    Row {
        held: Held::Archive,
        rights: ARCHIVE,
        most: 1,
    },
    Row {
        held: Held::Console,
        rights: Rights::READ,
        most: 1,
    },
    Row {
        held: Held::Channel,
        rights: Rights::CHANNEL,
        most: 4,
    },
]);
