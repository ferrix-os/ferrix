//! A fake 0x601 and its readings: what stage 3 drives the power core over
//! (`super::check`), and what the node stands on under the test-only
//! `ferrix.powerctl.fake` (`L.x86_64.207`), so `cargo xtask test-power`
//! shows a lease lapse and a holder's death end to end through devmgr and
//! the real driver. Nothing here reaches an MSR, and every kernel line
//! under it says `(fake)` (ledger 3130, condition 9). Its values are the
//! N150's as `ferrix.msrlog` read them: 25 A, `TjMax` 105 C.
//!
//! For the interleaving checks a fake can carry an exit restore to run
//! between the accesses of a write or of a task restore
//! ([`Fake::interleave`]): before each such access it runs the exit
//! restore's steps the schedule puts there. A stop that lands while the
//! other processor is not masked halts it, as the stop IPI does: from then
//! on its accesses change nothing (the halted processor does no further
//! step). Masked, it runs on, as interrupts masked make the real one.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use super::{Cells, READINGS, Restore, Restorer};
use crate::sync::SpinLock;

/// The signature the fake answers as: Intel family 6 model 0xBE.
pub(in crate::arch::x86_64) const SIGNATURE: u32 = 0x000b_06e0;
/// The N150's firmware 0x601: 25 A, lock clear.
pub(in crate::arch::x86_64) const VR: u64 = 0xc8;
/// A thermal status `below` degrees under `TjMax`, valid.
pub(in crate::arch::x86_64) const fn therm(below: u64) -> u64 {
    (1 << 31) | (below << 16)
}

/// The file under `ferrix.powerctl.fake`.
pub(in crate::arch::x86_64) static FAKE: Fake = Fake::new();

/// A restore scheduled between a write's accesses.
#[derive(Clone, Copy)]
struct Schedule {
    restorer: Restorer,
    /// Bit k: slot k of the interleaving is the restore's.
    positions: u32,
    slot: u32,
    done: Option<Restore>,
}

/// A fake 0x601, the state a real one keeps in `super`'s statics, the
/// readings, and the faults a check asks of it.
pub(in crate::arch::x86_64) struct Fake {
    register: AtomicU64,
    firmware: AtomicU64,
    last: AtomicU64,
    in_force: AtomicBool,
    stopped: AtomicBool,
    core: AtomicU64,
    package: AtomicU64,
    /// Writes still to fault.
    pub(in crate::arch::x86_64) faults: AtomicU32,
    /// Flipped into the next taken write, once, as an exclusive or.
    pub(in crate::arch::x86_64) skew: AtomicU64,
    /// The thermal readings fault.
    pub(in crate::arch::x86_64) blind: AtomicBool,
    /// Writes taken.
    pub(in crate::arch::x86_64) writes: AtomicU32,
    schedule: SpinLock<Option<Schedule>>,
    inside: AtomicBool,
    /// The processor the schedule interrupts is masked.
    masked: AtomicBool,
    /// It was stopped while not masked: its accesses do nothing.
    halted: AtomicBool,
}

impl Fake {
    pub(in crate::arch::x86_64) const fn new() -> Fake {
        Fake {
            register: AtomicU64::new(VR),
            firmware: AtomicU64::new(VR),
            last: AtomicU64::new(0),
            in_force: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            core: AtomicU64::new(therm(65)),
            package: AtomicU64::new(therm(63)),
            faults: AtomicU32::new(0),
            skew: AtomicU64::new(0),
            blind: AtomicBool::new(false),
            writes: AtomicU32::new(0),
            schedule: SpinLock::new(None),
            inside: AtomicBool::new(false),
            masked: AtomicBool::new(false),
            halted: AtomicBool::new(false),
        }
    }

    /// What the probe reads of `msr`.
    pub(in crate::arch::x86_64) fn probe(&self, msr: u32) -> Option<u64> {
        match msr {
            0x601 => Some(self.register.load(Ordering::SeqCst)),
            _ => self.reading(msr),
        }
    }

    /// Firmware's value, as the probe stores it.
    pub(in crate::arch::x86_64) fn set_firmware(&self, value: u64) {
        self.firmware.store(value, Ordering::SeqCst);
    }

    /// Firmware writing the register behind the core's back.
    pub(in crate::arch::x86_64) fn poke(&self, value: u64) {
        self.register.store(value, Ordering::SeqCst);
    }

    /// The register as it holds now.
    pub(in crate::arch::x86_64) fn peek(&self) -> u64 {
        self.register.load(Ordering::SeqCst)
    }

    /// The core and the package `core` and `package` degrees under 105,
    /// `bits` set in the core's register.
    pub(in crate::arch::x86_64) fn heat(&self, core: u64, package: u64, bits: u64) {
        self.core.store(therm(core) | bits, Ordering::SeqCst);
        self.package.store(therm(package), Ordering::SeqCst);
    }

    /// Run `restorer` in the slots `positions` names, between the accesses
    /// of what runs next; [`Fake::finish`] runs what is left of it.
    pub(in crate::arch::x86_64) fn interleave(&self, restorer: Restorer, positions: u32) {
        *self.schedule.lock() = Some(Schedule {
            restorer,
            positions,
            slot: 0,
            done: None,
        });
    }

    /// Run the scheduled restore to its end; what it came to.
    pub(in crate::arch::x86_64) fn finish(&self) -> Option<Restore> {
        let schedule = self.schedule.lock().take()?;
        self.inside.store(true, Ordering::SeqCst);
        let done = schedule.done.unwrap_or_else(|| schedule.restorer.run(self));
        self.inside.store(false, Ordering::SeqCst);
        Some(done)
    }

    /// Before one of the write's accesses: the restore's steps due here.
    fn turn(&self) {
        if self.inside.load(Ordering::SeqCst) {
            return;
        }
        let Some(mut schedule) = self.schedule.lock().take() else {
            return;
        };
        self.inside.store(true, Ordering::SeqCst);
        while schedule.slot < 32 && schedule.positions >> schedule.slot & 1 == 1 {
            schedule.slot += 1;
            if schedule.done.is_none() {
                schedule.done = schedule.restorer.step(self);
            }
        }
        schedule.slot += 1;
        if self.stopped.load(Ordering::SeqCst) && !self.masked.load(Ordering::SeqCst) {
            self.halted.store(true, Ordering::SeqCst);
        }
        self.inside.store(false, Ordering::SeqCst);
        *self.schedule.lock() = Some(schedule);
    }

    /// Before one of the interrupted processor's accesses that changes
    /// something: the restore's steps due here, then whether that processor
    /// still runs to make it.
    fn runs(&self) -> bool {
        self.turn();
        self.inside.load(Ordering::SeqCst) || !self.halted.load(Ordering::SeqCst)
    }
}

impl Cells for Fake {
    fn read(&self) -> Option<u64> {
        self.turn();
        Some(self.register.load(Ordering::SeqCst))
    }
    fn write(&self, value: u64) -> bool {
        if !self.runs() {
            return true;
        }
        let faults = self.faults.load(Ordering::SeqCst);
        if faults > 0 {
            self.faults.store(faults - 1, Ordering::SeqCst);
            return false;
        }
        let _ = self.writes.fetch_add(1, Ordering::SeqCst);
        let skew = self.skew.swap(0, Ordering::SeqCst);
        self.register.store(value ^ skew, Ordering::SeqCst);
        true
    }
    fn firmware(&self) -> u64 {
        self.turn();
        self.firmware.load(Ordering::SeqCst)
    }
    fn last(&self) -> u64 {
        self.turn();
        self.last.load(Ordering::SeqCst)
    }
    fn set_last(&self, value: u64) {
        if !self.runs() {
            return;
        }
        self.last.store(value, Ordering::SeqCst);
    }
    fn in_force(&self) -> bool {
        self.turn();
        self.in_force.load(Ordering::SeqCst)
    }
    fn arm(&self) {
        if !self.runs() {
            return;
        }
        self.in_force.store(true, Ordering::SeqCst);
    }
    fn take(&self) -> bool {
        if !self.runs() {
            return false;
        }
        self.in_force.swap(false, Ordering::SeqCst)
    }
    fn stopped(&self) -> bool {
        self.turn();
        self.stopped.load(Ordering::SeqCst)
    }
    fn stop(&self) {
        if !self.runs() {
            return;
        }
        self.stopped.store(true, Ordering::SeqCst);
    }
    fn reading(&self, msr: u32) -> Option<u64> {
        let blind = self.blind.load(Ordering::SeqCst);
        match msr {
            0x1a2 => Some(105 << 16),
            0x19c => (!blind).then(|| self.core.load(Ordering::SeqCst)),
            0x1b1 => (!blind).then(|| self.package.load(Ordering::SeqCst)),
            0x198 => Some(0x1a00),
            0x64f => Some(0x0100),
            0x6b0 => Some(0x0100_0000),
            0x611 => Some(0x1000),
            0x606 => Some(0x000a_0e03),
            0x610 => Some(0x0042_80f0_0023_80c8),
            _ => None,
        }
        .filter(|_| READINGS.contains(&msr))
    }
    fn masked<T>(&self, body: impl FnOnce() -> T) -> T {
        let was = self.masked.swap(true, Ordering::SeqCst);
        let done = body();
        self.masked.store(was, Ordering::SeqCst);
        done
    }
}
