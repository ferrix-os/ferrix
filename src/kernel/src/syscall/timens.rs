//! Time namespaces: `CLOCK_MONOTONIC` and `CLOCK_BOOTTIME` as a namespace
//! shows them (`docs/NAMESPACES.md` §12.1, `time_namespaces(7)`).
//!
//! A [`TimeNamespace`] is two signed offsets, in nanoseconds, from the host's
//! counter: one for the monotonic clocks, one for the boot-time clock. A
//! process in the namespace reads the host's time plus the offset, and when it
//! gives the kernel an absolute time on one of those clocks the kernel takes
//! the offset off again ([`TimeNamespace::host`]). Nothing else moves:
//! `CLOCK_REALTIME`, `CLOCK_TAI` and the CPU-time clocks are the same in every
//! namespace, and every deadline the kernel keeps for itself stays on the
//! host's counter.
//!
//! # Two references, as Linux's `nsproxy` has them
//!
//! A process holds [`Held`]: the namespace it is in, which its clock reads
//! use, and the one it makes children in. They differ after
//! `unshare(CLONE_NEWTIME)`, which moves the second only, because a process's
//! clocks must not jump under it (`time_namespaces(7)`). A child takes its
//! parent's second for both and freezes it: from then on the offsets can no
//! longer be written, so a process never sees them change.
//!
//! # Locks
//!
//! The offsets, and whether they are frozen, are behind one spin lock taken
//! for a read or a write and never held across anything else; a process's
//! [`Held`] is behind a lock of its own, taken alone. Nothing is allocated
//! under either, and what a replaced reference drops is dropped after the
//! lock.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use ferrix_kmem::Charge;
use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::types::{
    CLOCK_BOOTTIME, CLOCK_MONOTONIC, CLOCK_MONOTONIC_COARSE, CLOCK_MONOTONIC_RAW,
};
use ferrix_sync::Once;

use crate::sync::SpinLock;
use crate::syscall::credentials::Credentials;
use crate::syscall::time::CLOCK_BOOTTIME_ALARM;
use crate::syscall::userns::{self, CAP_SYS_ADMIN, CAP_SYS_TIME, UserNamespace};

/// `CLONE_NEWTIME`: a time namespace for the caller's children. Inside
/// `CSIGNAL`, so only `unshare` and `setns` can take it.
pub(crate) const CLONE_NEWTIME: u64 = 0x0000_0080;

/// Linux's `KTIME_SEC_MAX`: the most seconds a clock can hold.
const KTIME_SEC_MAX: i64 = 9_223_372_036;

/// Nanoseconds in a second.
const NANOS: i128 = 1_000_000_000;

/// The most a write to `timens_offsets` takes: a page.
const WRITE_MAX: usize = 4096;

/// `/proc/<pid>/ns/time`'s number for the first namespace: Linux's own.
const FIRST_ID: u64 = 0xEFFF_FFFA;
/// And for the ones made after it.
static NEXT_ID: AtomicU64 = AtomicU64::new(0xF400_0000);

/// Which of the two shifted clocks a call is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shift {
    /// `CLOCK_MONOTONIC`, its raw and coarse forms.
    Monotonic,
    /// `CLOCK_BOOTTIME`.
    Boottime,
}

/// The shift `clock` (a `CLOCK_*` number) is read with, or `None` for a clock
/// every namespace reads alike.
pub(crate) const fn shift_of(clock: u32) -> Option<Shift> {
    match clock {
        CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW | CLOCK_MONOTONIC_COARSE => Some(Shift::Monotonic),
        CLOCK_BOOTTIME | CLOCK_BOOTTIME_ALARM => Some(Shift::Boottime),
        _ => None,
    }
}

/// What a namespace's lock guards.
#[derive(Debug, Clone, Copy)]
struct Offsets {
    /// Nanoseconds added to the monotonic clocks.
    monotonic: i64,
    /// Nanoseconds added to the boot-time clock.
    boottime: i64,
    /// A process has been made in it: the offsets are written no more.
    frozen: bool,
}

/// A time namespace.
#[derive(Debug)]
pub(crate) struct TimeNamespace {
    /// The user namespace it was made in: whoever holds `CAP_SYS_TIME` over
    /// it may write its offsets.
    owner: Arc<UserNamespace>,
    /// What `/proc/<pid>/ns/time` names.
    id: u64,
    /// The offsets, and whether they are still writable.
    offsets: SpinLock<Offsets>,
    /// The kernel heap this is, charged to the job that made it (F-37).
    _charge: Option<Charge>,
}

/// The first namespace, made on first use: no offsets, and never writable.
pub(crate) fn first() -> &'static Arc<TimeNamespace> {
    static FIRST: Once<Arc<TimeNamespace>> = Once::new();
    FIRST.call_once(|| {
        Arc::new(TimeNamespace {
            owner: Arc::clone(userns::first()),
            id: FIRST_ID,
            offsets: SpinLock::new(Offsets {
                monotonic: 0,
                boottime: 0,
                frozen: true,
            }),
            _charge: None,
        })
    })
}

/// Make the namespace `unshare(CLONE_NEWTIME)` asks for, owned by `fresh`
/// (the user namespace made in the same call) or else by the creator's.
///
/// # Errors
///
/// `EPERM` without `CAP_SYS_ADMIN` in the owning user namespace; `ENOMEM`
/// past the job's memory.
pub(crate) fn create(
    creator: &Credentials,
    fresh: Option<&Arc<UserNamespace>>,
) -> Result<Arc<TimeNamespace>, Errno> {
    let owner = match fresh {
        Some(fresh) => Arc::clone(fresh),
        None if creator.holds(CAP_SYS_ADMIN) => Arc::clone(&creator.user_ns),
        None => return Err(Errno::EPERM),
    };
    let charge = Charge::arc::<TimeNamespace>().map_err(|_| Errno::ENOMEM)?;
    let namespace = TimeNamespace {
        owner,
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        offsets: SpinLock::new(Offsets {
            monotonic: 0,
            boottime: 0,
            frozen: false,
        }),
        _charge: Some(charge),
    };
    crate::fallible::try_arc(namespace).map_err(|_| Errno::ENOMEM)
}

impl TimeNamespace {
    /// Whether this is the first namespace.
    pub(crate) fn is_first(&self) -> bool {
        self.id == FIRST_ID
    }

    /// What `/proc/<pid>/ns/time` names.
    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    /// The user namespace that owns it.
    pub(crate) fn owner(&self) -> &Arc<UserNamespace> {
        &self.owner
    }

    /// Whether the offsets can no longer be written.
    pub(crate) fn frozen(&self) -> bool {
        self.offsets.lock().frozen
    }

    /// The offset of `which`, in nanoseconds.
    pub(crate) fn offset(&self, which: Shift) -> i64 {
        let offsets = self.offsets.lock();
        match which {
            Shift::Monotonic => offsets.monotonic,
            Shift::Boottime => offsets.boottime,
        }
    }

    /// `host` nanoseconds on the counter, as a process in this namespace
    /// reads them on `which`: Linux's `timens_add_monotonic` and
    /// `timens_add_boottime`. Never below zero.
    pub(crate) fn shown(&self, which: Shift, host: u64) -> u64 {
        let shifted = i128::from(host) + i128::from(self.offset(which));
        u64::try_from(shifted.max(0)).unwrap_or(u64::MAX)
    }

    /// An absolute time a process in this namespace gave on `which`, as the
    /// host's counter reads it: Linux's `timens_ktime_to_host`. Never below
    /// zero, which is a time already past.
    pub(crate) fn host(&self, which: Shift, shown: u64) -> u64 {
        let host = i128::from(shown) - i128::from(self.offset(which));
        u64::try_from(host.max(0)).unwrap_or(u64::MAX)
    }

    /// Stop the offsets being written: a process is to be made in it.
    pub(crate) fn freeze(&self) {
        self.offsets.lock().frozen = true;
    }

    /// `/proc/<pid>/timens_offsets` as a reader sees it: Linux's
    /// `"%-10s %10lld %9ld\n"`, the nanoseconds a whole number of seconds
    /// and a fraction that is never negative.
    pub(crate) fn render_offsets(&self) -> Vec<u8> {
        let offsets = *self.offsets.lock();
        let mut out = Vec::new();
        for (name, nanos) in [
            ("monotonic", offsets.monotonic),
            ("boottime", offsets.boottime),
        ] {
            let nanos = i128::from(nanos);
            let line = alloc::format!(
                "{name:<10} {:>10} {:>9}\n",
                nanos.div_euclid(NANOS),
                nanos.rem_euclid(NANOS)
            );
            out.extend_from_slice(line.as_bytes());
        }
        out
    }
}

/// What a process holds: the namespace it is in, and the one its children
/// are made in.
#[derive(Debug, Clone)]
pub(crate) struct Held {
    /// The namespace its clock reads use.
    pub(crate) own: Arc<TimeNamespace>,
    /// The namespace a child of it is made in.
    pub(crate) children: Arc<TimeNamespace>,
}

impl Held {
    /// A process the kernel starts: in the first namespace, making its
    /// children there.
    pub(crate) fn initial() -> Held {
        Held {
            own: Arc::clone(first()),
            children: Arc::clone(first()),
        }
    }

    /// What a child of a process holding `self` holds: its parent's
    /// namespace for children, for both, frozen (Linux's `timens_on_fork`).
    pub(crate) fn for_fork(&self) -> Held {
        self.children.freeze();
        Held {
            own: Arc::clone(&self.children),
            children: Arc::clone(&self.children),
        }
    }
}

/// The offset of `which` for the process making the call, or zero.
pub(crate) fn reader_offset(which: Shift) -> i64 {
    userns::acting().map_or(0, |process| process.time_namespace().offset(which))
}

/// `host` nanoseconds on the counter as the process making the call reads
/// `which` (a boot check's, when no task is running), and as they are for the
/// kernel's own reads: what `/proc/uptime` and `/proc/stat` show a reader.
pub(crate) fn shown_to_reader(which: Shift, host: u64) -> u64 {
    userns::acting().map_or(host, |process| process.time_namespace().shown(which, host))
}

/// Parse a write to `timens_offsets`: up to two lines of
/// `monotonic|boottime <secs> <nsecs>`, as Linux reads them. `None` for any
/// malformed text.
fn parse(data: &[u8]) -> Option<[Option<i128>; 2]> {
    let text = core::str::from_utf8(data).ok()?;
    let body = text.strip_suffix('\n').unwrap_or(text);
    let mut found = [None, None];
    let mut lines = 0_usize;
    for line in body.split('\n') {
        lines += 1;
        if lines > 2 {
            return None;
        }
        let mut words = line.split_ascii_whitespace();
        let (name, secs, nsecs) = (words.next()?, words.next()?, words.next()?);
        if words.next().is_some() || name.len() > 9 {
            return None;
        }
        let secs = secs.parse::<i64>().ok()?;
        let nsecs = nsecs
            .parse::<u64>()
            .ok()
            .filter(|&n| i128::from(n) < NANOS)?;
        let nanos = i128::from(secs) * NANOS + i128::from(nsecs);
        let slot = match name {
            "monotonic" => 0,
            "boottime" => 1,
            _ => return None,
        };
        *found.get_mut(slot)? = Some(nanos);
    }
    Some(found)
}

/// A write to `timens_offsets` of the namespace `ns`: Linux's order. The text
/// is read (`EINVAL`), `opener` and `writer` must each hold `CAP_SYS_TIME`
/// over the namespace's owner (`EPERM`), each clock plus its offset must lie
/// within `0` and half of [`KTIME_SEC_MAX`] seconds (`ERANGE`), and the
/// offsets must not be frozen (`EACCES`).
///
/// # Errors
///
/// As above.
pub(crate) fn write_offsets(
    ns: &TimeNamespace,
    opener: &Credentials,
    writer: &Credentials,
    data: &[u8],
) -> Result<usize, Errno> {
    if data.len() >= WRITE_MAX {
        return Err(Errno::EINVAL);
    }
    let given = parse(data).ok_or(Errno::EINVAL)?;
    for credentials in [opener, writer] {
        if !userns::capable_over(credentials, &ns.owner, CAP_SYS_TIME) {
            return Err(Errno::EPERM);
        }
    }
    let now = i128::from(crate::timer::now_nanos());
    let limit = i128::from(KTIME_SEC_MAX / 2) * NANOS;
    for nanos in given.iter().flatten() {
        if !(-i128::from(KTIME_SEC_MAX)..=i128::from(KTIME_SEC_MAX))
            .contains(&nanos.div_euclid(NANOS))
        {
            return Err(Errno::ERANGE);
        }
        let sum = now + nanos;
        if !(0..=limit).contains(&sum) {
            return Err(Errno::ERANGE);
        }
    }
    let mut offsets = ns.offsets.lock();
    if offsets.frozen {
        return Err(Errno::EACCES);
    }
    // Both fit an `i64`: the range above leaves a sum below half the
    // maximum, so the offset is that less the clock.
    if let Some(nanos) = given[0] {
        offsets.monotonic = i64::try_from(nanos).map_err(|_| Errno::ERANGE)?;
    }
    if let Some(nanos) = given[1] {
        offsets.boottime = i64::try_from(nanos).map_err(|_| Errno::ERANGE)?;
    }
    Ok(data.len())
}
