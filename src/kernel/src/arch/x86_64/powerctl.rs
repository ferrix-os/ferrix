//! The power core: the VR current limit (`MSR_VR_CURRENT_CONFIG`, 0x601)
//! written only through typed calls on a claim the power driver holds;
//! every policy -- the steps, when, how hot is too hot for a raise -- is
//! the driver's (`docs/POWERCTL.md`; the consultant's design review, ledger
//! line 3130; the customer's "keep the kernel slim", 2026-10-22).
//!
//! Without `ferrix.powerctl` (or the test-only `ferrix.powerctl.fake`)
//! nothing here reads, writes, publishes or prints. With it, stage 10's
//! finder asks [`probe`], which publishes a node only on a processor the
//! model table names, in one package, with 0x601 unlocked and `TjMax`
//! believed, and reads firmware's 0x601 once. The node's driver asks
//! `power_control_create` ([`claim`]) and then `power_set` and `power_read`
//! on the claim. The kernel holds what makes a write
//! safe whoever asks: the row's bounds, a raise at most once a second, a
//! raise refused while the backstop reads hot, and firmware's value back
//! when the holder goes, its lease lapses, the backstop trips, a write does
//! not read back, and at a power-off, a reset and both ends of a panic
//! ([`restore`]). A task on the boot processor looks once a second.
//!
//! Every write and every restore compares the register with what the core
//! last wrote (firmware's, before any write) and writes nothing over a
//! value firmware changed. A restore writes firmware's field over the
//! register as it reads, not firmware's whole word.
//!
//! Its limits (§6.8): a scheduler that stops running the task stops the
//! lease, the death watch and the backstop; firmware's PROCHOT and
//! THERMTRIP remain the hardware's protection; an unorderly reset runs no
//! restore.

pub(super) mod check;
pub(super) mod fake_check;
pub(super) mod logic;

use alloc::sync::{Arc, Weak};
use core::arch::x86_64::{__cpuid, __cpuid_count};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ferrix_bootinfo::BootView;
use ferrix_native_abi::types as abi;
use ferrix_sched::{CpuSet, NICE_0_WEIGHT};
use ferrix_sync::Once;

use super::cpu;
use crate::console::println;
use crate::object::channel::Endpoint;
use crate::sync::SpinLock;
use logic::{Row, Trip};

/// The boot word, and its test-only twin over the fake register file.
pub(crate) const OPTION: &str = "ferrix.powerctl";
pub(crate) const FAKE_OPTION: &str = "ferrix.powerctl.fake";

/// `MSR_VR_CURRENT_CONFIG`: the one register the core writes.
const VR_CURRENT_CONFIG: u32 = 0x601;
/// The registers `power_read` reads, read only, in its words' order from
/// `abi::POWER_READ_FIRST`: `TjMax`, the boot processor's core and the
/// package thermal status, `IA32_PERF_STATUS`, the core and graphics limit
/// reasons, the package energy and RAPL units, and the package power
/// limit. 0x601 itself is word `abi::POWER_READ_VR`.
const READINGS: [u32; 9] = [
    0x1a2, 0x19c, 0x1b1, 0x198, 0x64f, 0x6b0, 0x611, 0x606, 0x610,
];
const TEMPERATURE_TARGET: u32 = 0x1a2;
const THERM_STATUS: u32 = 0x19c;
const PACKAGE_THERM_STATUS: u32 = 0x1b1;

// `power_read`'s layout is the driver's (`ferrix_native_abi::types`) too.
const _: () = {
    assert!(
        abi::POWER_READ_WORDS == abi::POWER_READ_FIRST + READINGS.len(),
        "power_read's words are the fixed ones and the readings"
    );
    assert!(
        abi::POWER_READ_FIRST > abi::POWER_READ_VR,
        "the readings follow the register's word"
    );
};

/// The register and the state a write and a restore share. Every method
/// but [`Cells::masked`] is one access, so stage 3 can run a restore's
/// steps between any two of a write's (`check`).
pub(super) trait Cells {
    /// 0x601 as it reads; `None` when the read faulted.
    fn read(&self) -> Option<u64>;
    /// Write 0x601; whether it was taken.
    fn write(&self, value: u64) -> bool;
    /// Firmware's 0x601, read once at the probe.
    fn firmware(&self) -> u64;
    /// The value last written while in force.
    fn last(&self) -> u64;
    fn set_last(&self, value: u64);
    /// Whether a value the core wrote may be in force.
    fn in_force(&self) -> bool;
    fn arm(&self);
    /// Out of force; whether it was in (only one caller sees `true`).
    fn take(&self) -> bool;
    /// Whether a power-off, reset or panic restore has run.
    fn stopped(&self) -> bool;
    fn stop(&self);
    /// A read-only register of [`READINGS`].
    fn reading(&self, msr: u32) -> Option<u64>;
    /// Run `body` with nothing able to stop this processor in it
    /// (interrupts masked on the real register).
    fn masked<T>(&self, body: impl FnOnce() -> T) -> T;
}

/// What a write came to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Wrote {
    /// Written over `was`, read back as `back`.
    Done {
        was: u64,
        value: u64,
        back: Option<u64>,
    },
    /// The register read `reads`, not `expected`: firmware changed it.
    Changed { expected: u64, reads: u64 },
    /// The read faulted; nothing written.
    Absent,
    /// The write faulted; the register still holds `expected`, in force.
    Faulted,
    /// A stop came first or in between; firmware's field is back.
    Stopped,
}

/// Write `eighths` into 0x601's field over the register as it reads now,
/// if it reads what the core last wrote (`L.x86_64.196`, `202`, `204`).
pub(super) fn write(cells: &impl Cells, eighths: u64) -> Wrote {
    if cells.stopped() {
        return Wrote::Stopped;
    }
    let Some(now) = cells.read() else {
        return Wrote::Absent;
    };
    let expected = if cells.in_force() {
        cells.last()
    } else {
        cells.firmware()
    };
    if now != expected {
        return Wrote::Changed {
            expected,
            reads: now,
        };
    }
    let value = logic::compose(now, eighths);
    cells.set_last(value);
    cells.arm();
    if !cells.write(value) {
        cells.set_last(expected);
        return Wrote::Faulted;
    }
    let back = cells.read();
    if cells.stopped() {
        // A stop's restore may have run before this write landed: put
        // firmware's field back here, whichever restore took the register.
        let again = cells.read().unwrap_or(value);
        let _ = cells.write(logic::compose(again, cells.firmware()));
        let _ = cells.take();
        return Wrote::Stopped;
    }
    Wrote::Done {
        was: now,
        value,
        back,
    }
}

/// What a restore came to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Restore {
    /// Nothing in force.
    Idle,
    /// The read faulted; nothing written.
    Absent,
    /// The register read `reads`, not `wrote`: left to firmware.
    Left { wrote: u64, reads: u64 },
    /// Firmware's field written (`taken`), read back as `back`.
    Done {
        value: u64,
        taken: bool,
        back: Option<u64>,
    },
}

impl Restore {
    /// Whether firmware's field is confirmed, or nothing needed it.
    pub(super) fn confirmed(self) -> bool {
        match self {
            Restore::Idle | Restore::Left { .. } => true,
            Restore::Absent => false,
            Restore::Done { value, taken, back } => {
                taken && back.is_some_and(|back| back & logic::VR_FIELD == value & logic::VR_FIELD)
            }
        }
    }
}

/// A restore, one access a step (`L.x86_64.198`, `202`, `206`, `208`); `exit`
/// for a power-off, reset or panic, which stops every later write.
#[derive(Clone, Copy, Debug)]
pub(super) struct Restorer {
    exit: bool,
    step: u8,
    now: u64,
    value: u64,
    taken: bool,
}

impl Restorer {
    pub(super) const fn new(exit: bool) -> Restorer {
        Restorer {
            exit,
            step: 0,
            now: 0,
            value: 0,
            taken: false,
        }
    }

    /// One step; `Some` once it is over.
    pub(super) fn step(&mut self, cells: &impl Cells) -> Option<Restore> {
        self.step += 1;
        match self.step {
            1 if self.exit => cells.stop(),
            1 => {}
            2 if !cells.take() => return Some(Restore::Idle),
            2 => {}
            3 => match cells.read() {
                Some(now) => self.now = now,
                None => return Some(Restore::Absent),
            },
            4 => {
                let wrote = cells.last();
                if self.now != wrote {
                    let reads = self.now;
                    return Some(Restore::Left { wrote, reads });
                }
                self.value = logic::compose(self.now, cells.firmware());
            }
            5 => self.taken = cells.write(self.value),
            _ => {
                let (value, taken) = (self.value, self.taken);
                let back = cells.read();
                return Some(Restore::Done { value, taken, back });
            }
        }
        None
    }

    /// Run to the end.
    pub(super) fn run(mut self, cells: &impl Cells) -> Restore {
        loop {
            if let Some(done) = self.step(cells) {
                return done;
            }
        }
    }
}

/// Why a claim ended, or a raise was taken back.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Cause {
    Gone,
    Lease,
    Backstop(Trip),
    /// A thermal status read faulted.
    Blind,
    /// A write faulted, did not read back, or ran into a stop.
    Verify,
}

impl Cause {
    /// Whether it latches the core off until reboot: evidence about the
    /// hardware, not the driver (§6.3).
    const fn latches(self) -> bool {
        matches!(self, Cause::Backstop(_) | Cause::Blind | Cause::Verify)
    }
}

/// Why a call was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Refused {
    /// Outside the row.
    OutOfRange,
    /// A raise within a second of the last.
    Rate,
    /// A raise while the backstop reads hot.
    Hot,
    /// Latched off, the register left to firmware, or the claim ended.
    Latched,
    /// Not the claim, or no claim.
    NotHeld,
}

/// A line to say, after the lock is let go.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Event {
    Set { was: u64, now: u64 },
    Restored(Restore, Cause),
    Latched(Cause),
}

/// What [`State::admit`] found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Admitted {
    /// The value is in force already: nothing to write.
    InForce(u64),
    /// Write it; whether it raises.
    Write { raise: bool },
}

/// The lines one call says: at most a write or restore and a latch.
pub(super) type Said = [Option<Event>; 2];

/// The most claims one boot may end before the core latches off.
pub(super) const MAX_ENDED: u32 = 8;

/// The core's state across claims (`L.x86_64.198`-`205`).
#[derive(Debug)]
pub(super) struct State {
    /// The holder's end, while a claim stands.
    holder: Option<Weak<Endpoint>>,
    /// The kernel's end of the claim: the holder sees `PEER_CLOSED` when it
    /// is dropped, which the caller does after letting the lock go.
    kernel_end: Option<Arc<Endpoint>>,
    /// The kernel end of a claim just ended, for the caller to drop.
    dropping: Option<Arc<Endpoint>>,
    /// Which claim stands: one more for each, so a watch knows its own.
    generation: u64,
    /// When the claim last made a call.
    renewed: u64,
    /// Latched off until reboot.
    pub(super) latched: bool,
    /// The register is left to firmware until reboot.
    pub(super) forgotten: bool,
    /// When the register was last raised, whichever claim raised it.
    raised: Option<u64>,
    /// Claims ended this boot.
    ended: u32,
}

impl State {
    pub(super) const fn new() -> State {
        State {
            holder: None,
            kernel_end: None,
            dropping: None,
            generation: 0,
            renewed: 0,
            latched: false,
            forgotten: false,
            raised: None,
            ended: 0,
        }
    }

    /// Whether a claim stands.
    pub(super) const fn held(&self) -> bool {
        self.holder.is_some()
    }

    /// Whether the claim of `generation` still stands: a watch whose claim
    /// ended looks no further, and touches no later claim (ledger 3174, B3).
    pub(super) const fn serves(&self, generation: u64) -> bool {
        self.holder.is_some() && self.generation == generation
    }

    /// Whether the holder's end has closed, or there is no kernel end.
    fn gone(&self) -> bool {
        self.kernel_end.as_ref().is_none_or(|end| end.peer_closed())
    }

    /// The kernel end of a claim that ended, to drop after the lock.
    fn release(&mut self) -> Option<Arc<Endpoint>> {
        self.dropping.take()
    }

    /// Whether `end` is the claim's.
    fn holds(&self, end: &Arc<Endpoint>) -> bool {
        self.holder
            .as_ref()
            .is_some_and(|holder| core::ptr::eq(holder.as_ptr(), Arc::as_ptr(end)))
    }

    /// Begin a claim for `holder`, the kernel keeping `kernel_end`, at
    /// `now`; 0x601 compared with firmware's. The claim's generation.
    ///
    /// # Errors
    ///
    /// [`Refused::Latched`] latched off; [`Refused::NotHeld`] while another
    /// claim stands.
    pub(super) fn claim(
        &mut self,
        cells: &impl Cells,
        holder: Weak<Endpoint>,
        kernel_end: Option<Arc<Endpoint>>,
        now: u64,
    ) -> Result<u64, Refused> {
        if self.latched {
            return Err(Refused::Latched);
        }
        if self.held() {
            return Err(Refused::NotHeld);
        }
        if !cells.in_force() && cells.read() != Some(cells.firmware()) {
            self.forgotten = true;
        }
        self.holder = Some(holder);
        self.kernel_end = kernel_end;
        self.generation = self.generation.wrapping_add(1);
        self.renewed = now;
        Ok(self.generation)
    }

    /// The backstop's verdict now.
    fn backstop(cells: &impl Cells, row: &Row) -> Result<(), Cause> {
        let tjmax = (cells.reading(TEMPERATURE_TARGET).ok_or(Cause::Blind)? >> 16) & 0xff;
        let core = cells.reading(THERM_STATUS).ok_or(Cause::Blind)?;
        let package = cells.reading(PACKAGE_THERM_STATUS).ok_or(Cause::Blind)?;
        logic::verdict(tjmax, row.hot, core, package).map_err(Cause::Backstop)
    }

    /// Whether `eighths` may be written now: within the row, not latched,
    /// and a raise due by the floor and with the backstop good.
    fn admit(
        &self,
        cells: &impl Cells,
        row: &Row,
        now: u64,
        eighths: u64,
    ) -> Result<Admitted, Refused> {
        if self.latched || self.forgotten {
            return Err(Refused::Latched);
        }
        if !logic::admitted(row, cells.firmware(), eighths) {
            return Err(Refused::OutOfRange);
        }
        let current = cells.read().ok_or(Refused::Latched)?;
        if current & logic::VR_FIELD == eighths {
            return Ok(Admitted::InForce(current));
        }
        let raise = eighths > current & logic::VR_FIELD;
        if raise && !logic::raise_due(self.raised, now) {
            return Err(Refused::Rate);
        }
        if raise && Self::backstop(cells, row).is_err() {
            return Err(Refused::Hot);
        }
        Ok(Admitted::Write { raise })
    }

    /// `power_set`: `eighths` into 0x601, within the row, a raise at most
    /// once a second and never while the backstop reads hot; the register
    /// as read back (`L.x86_64.195`).
    pub(super) fn set(
        &mut self,
        cells: &impl Cells,
        row: &Row,
        now: u64,
        eighths: u64,
        said: &mut Said,
    ) -> Result<u64, Refused> {
        self.renewed = now;
        let raise = match self.admit(cells, row, now, eighths)? {
            Admitted::InForce(current) => return Ok(current),
            Admitted::Write { raise } => raise,
        };
        match cells.masked(|| write(cells, eighths)) {
            Wrote::Done {
                was,
                value,
                back: Some(back),
            } if back & logic::VR_FIELD == eighths => {
                if raise {
                    self.raised = Some(now);
                }
                said[0] = Some(Event::Set { was, now: value });
                Ok(back)
            }
            Wrote::Changed { .. } => {
                self.forgotten = true;
                Err(Refused::Latched)
            }
            wrote => {
                // The core cannot tell a register that read back otherwise
                // from firmware's change, so it takes the safe side: what
                // it read back is what the restore finds and replaces with
                // firmware's field (ledger 3174, B1).
                if let Wrote::Done {
                    back: Some(back), ..
                } = wrote
                {
                    cells.set_last(back);
                }
                self.end(cells, Cause::Verify, said);
                Err(Refused::Latched)
            }
        }
    }

    /// `power_read` renews the lease.
    pub(super) fn renew(&mut self, now: u64) {
        self.renewed = now;
    }

    /// The task's look, once a second: the holder gone, the lease, and --
    /// while a raise is in force -- the backstop. `true` when the claim
    /// ended (`L.x86_64.200`, `201`).
    pub(super) fn tick(
        &mut self,
        cells: &impl Cells,
        row: &Row,
        now: u64,
        gone: bool,
        said: &mut Said,
    ) -> bool {
        if !self.held() {
            return false;
        }
        let cause = if gone {
            Cause::Gone
        } else if logic::lapsed(self.renewed, now) {
            Cause::Lease
        } else if cells.in_force() {
            match Self::backstop(cells, row) {
                Ok(()) => return false,
                Err(cause) => cause,
            }
        } else {
            return false;
        };
        self.end(cells, cause, said);
        true
    }

    /// End the claim: firmware's value back, latched where the cause says,
    /// a restore not confirmed says, or the claims ended say
    /// (`L.x86_64.201`).
    pub(super) fn end(&mut self, cells: &impl Cells, cause: Cause, said: &mut Said) {
        let confirmed = self.restore_once(cells, cause, said);
        self.holder = None;
        self.dropping = self.kernel_end.take();
        self.ended = self.ended.saturating_add(1);
        if !self.latched && (cause.latches() || !confirmed || self.ended >= MAX_ENDED) {
            self.latched = true;
            said[1] = Some(Event::Latched(cause));
        }
    }

    /// One restore, said; whether confirmed.
    fn restore_once(&mut self, cells: &impl Cells, cause: Cause, said: &mut Said) -> bool {
        // Masked as the write is: a panic's stop on another processor waits
        // until firmware's field is written, so it never finds the register
        // taken out of force and still raised (ledger 3174, B2).
        let restore = cells.masked(|| Restorer::new(false).run(cells));
        if let Restore::Left { .. } = restore {
            self.forgotten = true;
        }
        if restore != Restore::Idle {
            said[0] = Some(Event::Restored(restore, cause));
        }
        restore.confirmed()
    }
}

/// The MSR, and the state a panic's restore reads without a lock.
struct Msr;

static FIRMWARE: AtomicU64 = AtomicU64::new(0);
static LAST: AtomicU64 = AtomicU64::new(0);
static IN_FORCE: AtomicBool = AtomicBool::new(false);
static STOPPED: AtomicBool = AtomicBool::new(false);

impl Cells for Msr {
    fn read(&self) -> Option<u64> {
        cpu::read_msr_checked(VR_CURRENT_CONFIG)
    }
    fn write(&self, value: u64) -> bool {
        // SAFETY: (SYSREG) 0x601 on a processor the model table names,
        // probed unlocked; only bits 12:0 differ from the register as just
        // read, admitted by the row or firmware's own (L.x86_64.192, 196).
        unsafe { cpu::write_msr_checked(VR_CURRENT_CONFIG, value) }
    }
    fn firmware(&self) -> u64 {
        FIRMWARE.load(Ordering::SeqCst)
    }
    fn last(&self) -> u64 {
        LAST.load(Ordering::SeqCst)
    }
    fn set_last(&self, value: u64) {
        LAST.store(value, Ordering::SeqCst);
    }
    fn in_force(&self) -> bool {
        IN_FORCE.load(Ordering::SeqCst)
    }
    fn arm(&self) {
        IN_FORCE.store(true, Ordering::SeqCst);
    }
    fn take(&self) -> bool {
        IN_FORCE.swap(false, Ordering::SeqCst)
    }
    fn stopped(&self) -> bool {
        STOPPED.load(Ordering::SeqCst)
    }
    fn stop(&self) {
        STOPPED.store(true, Ordering::SeqCst);
    }
    fn reading(&self, msr: u32) -> Option<u64> {
        READINGS
            .contains(&msr)
            .then(|| cpu::read_msr_checked(msr))
            .flatten()
    }
    fn masked<T>(&self, body: impl FnOnce() -> T) -> T {
        // A panic's stop is a fixed-delivery IPI: masked, this processor
        // finishes the write's last check before it can be stopped (§6.7).
        let was = <super::Irq as ferrix_sync::IrqControl>::disable();
        let done = body();
        <super::Irq as ferrix_sync::IrqControl>::restore(was);
        done
    }
}

/// What the probe fixed for the boot: the row, the signature, whether the
/// register file is the fake.
#[derive(Clone, Copy, Debug)]
struct Machine {
    row: &'static Row,
    signature: u32,
    fake: bool,
}

static MACHINE: Once<Machine> = Once::new();
static BOOT_CPU: AtomicU64 = AtomicU64::new(0);
static STATE: SpinLock<State> = SpinLock::new(State::new());

/// The shift from an x2APIC id to its package's (CPUID 0x1F, else 0xB).
fn package_shift() -> Option<u32> {
    let max = __cpuid(0).eax;
    let leaf = [0x1f, 0xb].into_iter().find(|&leaf| max >= leaf)?;
    (0..8)
        .map(|sub| __cpuid_count(leaf, sub))
        .take_while(|level| (level.ecx >> 8) & 0xff != 0)
        .last()
        .map(|level| level.eax & 0x1f)
}

/// Whether `CPUID.0` names Intel, and `CPUID.1:EAX`; the fake's own
/// under `ferrix.powerctl.fake`.
fn identity(fake: bool) -> (bool, u32) {
    if fake {
        return (true, fake_check::SIGNATURE);
    }
    let vendor = __cpuid(0);
    let intel = [vendor.ebx, vendor.edx, vendor.ecx]
        .iter()
        .flat_map(|word| word.to_le_bytes())
        .eq(b"GenuineIntel".iter().copied());
    (intel, __cpuid(1).eax)
}

/// Stage 10's question: publish a power node? Without either word nothing
/// at all; otherwise every refusal is one line and no node (`L.device.51`).
pub(crate) fn probe(view: &BootView<'_>) -> bool {
    let fake = view.flag(FAKE_OPTION);
    if (!fake && !view.flag(OPTION)) || MACHINE.get().is_some() {
        return false;
    }
    let tag = if fake { " (fake)" } else { "" };
    let (intel, signature) = identity(fake);
    let cells: &dyn Fn(u32) -> Option<u64> = if fake {
        &|msr| fake_check::FAKE.probe(msr)
    } else {
        &cpu::read_msr_checked
    };
    let ids =
        crate::smp::topology().map(|topology| topology.cpus().iter().map(|cpu| cpu.hardware_id));
    let packages = package_shift()
        .zip(ids)
        .map(|(shift, ids)| logic::packages(shift, ids));
    let vr = cells(VR_CURRENT_CONFIG);
    let tjmax = cells(TEMPERATURE_TARGET).map(|raw| (raw >> 16) & 0xff);
    let here = crate::smp::this_cpu().map(|cpu| cpu.logical);
    let row = logic::row(intel, signature);
    let refusal = logic::refusal(row.is_some(), packages, vr, tjmax)
        .or_else(|| here.is_none().then_some("no processor record"));
    if let Some(why) = refusal {
        println!(
            "power{tag}: refused: {why} (Intel {intel}, signature {signature:#x}, 0x601 {vr:x?}); no node"
        );
        return false;
    }
    let (Some(row), Some(vr), Some(here)) = (row, vr, here) else {
        return false;
    };
    if fake {
        fake_check::FAKE.set_firmware(vr);
    } else {
        FIRMWARE.store(vr, Ordering::SeqCst);
    }
    BOOT_CPU.store(here as u64, Ordering::SeqCst);
    let _ = MACHINE.call_once(|| Machine {
        row,
        signature,
        fake,
    });
    println!(
        "power{tag}: node: signature {signature:#x}, 0x601 {vr:#x}, ceiling {:#x}",
        logic::highest(row, vr & logic::VR_FIELD)
    );
    true
}

/// Run `$body` with `$cells` the boot's register file: the MSR, or the
/// fake under `ferrix.powerctl.fake`.
macro_rules! on_cells {
    ($machine:expr, $cells:ident => $body:expr) => {
        if $machine.fake {
            let $cells = &fake_check::FAKE;
            $body
        } else {
            let $cells = &Msr;
            $body
        }
    };
}

/// Why a claim was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ClaimError {
    InUse,
    Latched,
    NoMemory,
}

/// `power_control_create`: the claim, one at a time; the holder's end of a
/// channel the kernel keeps the other end of (`L.device.52`).
///
/// # Errors
///
/// [`ClaimError`].
pub(crate) fn claim() -> Result<Arc<Endpoint>, ClaimError> {
    let Some(&machine) = MACHINE.get() else {
        return Err(ClaimError::Latched);
    };
    let (kernel_end, holder) = Endpoint::pair().map_err(|_| ClaimError::NoMemory)?;
    let now = crate::timer::now_nanos();
    let weak = Arc::downgrade(&holder);
    let claimed =
        on_cells!(machine, cells => STATE.lock().claim(cells, weak, Some(kernel_end), now));
    let generation = match claimed {
        Ok(generation) => generation,
        Err(Refused::Latched) => return Err(ClaimError::Latched),
        Err(_) => return Err(ClaimError::InUse),
    };
    let here = usize::try_from(BOOT_CPU.load(Ordering::SeqCst)).unwrap_or(0);
    let mut only = CpuSet::empty();
    // NOALLOC: a CpuSet is a fixed bitmap; insert sets a bit.
    let started = only.insert(here).is_ok()
        && usize::try_from(generation).is_ok_and(|argument| {
            crate::sched::spawn_on("powerctl", watch, argument, NICE_0_WEIGHT, here, only).is_ok()
        });
    if !started {
        let mut said = Said::default();
        let end = on_cells!(machine, cells => {
            let mut state = STATE.lock();
            state.end(cells, Cause::Gone, &mut said);
            state.release()
        });
        drop(end);
        return Err(ClaimError::NoMemory);
    }
    Ok(holder)
}

/// Run `body` on the claim's state if `end` holds it, and say its lines
/// after the lock is let go.
fn held<T>(
    end: &Arc<Endpoint>,
    body: impl FnOnce(&mut State, Machine, &mut Said) -> T,
) -> Result<T, Refused> {
    let machine = *MACHINE.get().ok_or(Refused::NotHeld)?;
    let mut said = Said::default();
    let (done, ended) = {
        let mut state = STATE.lock();
        if !state.holds(end) {
            return Err(Refused::NotHeld);
        }
        let done = body(&mut state, machine, &mut said);
        (done, state.release())
    };
    // Outside the lock: the holder sees PEER_CLOSED.
    drop(ended);
    say(&said, machine.fake);
    Ok(done)
}

/// `power_set`.
///
/// # Errors
///
/// [`Refused`].
pub(crate) fn set(end: &Arc<Endpoint>, eighths: u64) -> Result<u64, Refused> {
    let now = crate::timer::now_nanos();
    held(
        end,
        |state, machine, said| on_cells!(machine, cells => state.set(cells, machine.row, now, eighths, said)),
    )?
}

/// `power_read`: `abi::POWER_READ_WORDS` words, a register that faulted
/// marked absent in word `abi::POWER_READ_PRESENT`, never read as 0
/// (`L.x86_64.197`). Renews the lease.
///
/// # Errors
///
/// [`Refused::NotHeld`].
pub(crate) fn read(end: &Arc<Endpoint>) -> Result<[u64; abi::POWER_READ_WORDS], Refused> {
    let now = crate::timer::now_nanos();
    held(end, |state, machine, _| {
        state.renew(now);
        on_cells!(machine, cells => readings(cells, state, machine))
    })
}

/// [`read`]'s words from `cells`.
fn readings(cells: &impl Cells, state: &State, machine: Machine) -> [u64; abi::POWER_READ_WORDS] {
    let mut words = [0u64; abi::POWER_READ_WORDS];
    let mut present = 0u64;
    let values = core::iter::once((abi::POWER_READ_VR, cells.read())).chain(
        READINGS
            .iter()
            .enumerate()
            .map(|(index, &msr)| (abi::POWER_READ_FIRST + index, cells.reading(msr))),
    );
    for (at, value) in values {
        if let (Some(value), Some(slot)) = (value, words.get_mut(at)) {
            *slot = value;
            present |= 1 << at;
        }
    }
    let firmware = cells.firmware();
    words[abi::POWER_READ_PRESENT] = present;
    words[abi::POWER_READ_FIRMWARE] = firmware;
    words[abi::POWER_READ_CEILING] = logic::highest(machine.row, firmware & logic::VR_FIELD);
    words[abi::POWER_READ_SIGNATURE] = u64::from(machine.signature);
    words[abi::POWER_READ_FLAGS] = u64::from(state.latched) | u64::from(state.forgotten) << 1;
    words
}

/// The task of the claim of generation `argument`: once a second, the
/// holder gone, the lease, the backstop; it ends when its claim does, and
/// never looks at a later one.
fn watch(argument: usize) {
    let Some(&machine) = MACHINE.get() else {
        return;
    };
    let generation = argument as u64;
    loop {
        crate::sched::sleep_for(1_000_000_000);
        let now = crate::timer::now_nanos();
        let mut said = Said::default();
        let (ended, end) = on_cells!(machine, cells => {
            let mut state = STATE.lock();
            if state.serves(generation) {
                let gone = state.gone();
                let ended = state.tick(cells, machine.row, now, gone, &mut said);
                (ended, state.release())
            } else {
                (true, None)
            }
        });
        // Dropped outside every lock: the holder sees PEER_CLOSED.
        drop(end);
        say(&said, machine.fake);
        if ended {
            return;
        }
    }
}

/// Whether no live holder has the claim: a quiesce of the power node is
/// refused under one (H.DEV.2).
pub(crate) fn unserved() -> bool {
    let state = STATE.lock();
    !state.held() || state.gone()
}

/// Firmware's value back if a raise may be in force: from an orderly
/// power-off or reset and both ends of a panic, on any processor, no lock,
/// no allocation (`L.x86_64.206`); the fake's under the fake.
pub(crate) fn restore(what: &'static str) {
    let Some(machine) = MACHINE.get() else {
        return;
    };
    let restore = if machine.fake {
        Restorer::new(true).run(&fake_check::FAKE)
    } else {
        Restorer::new(true).run(&Msr)
    };
    if restore != Restore::Idle {
        let tag = if machine.fake { " (fake)" } else { "" };
        println!("power{tag}: restored {restore:x?}: {what}");
    }
}

/// Say `said`'s lines: the raw values, the driver's to decode.
fn say(said: &Said, fake: bool) {
    let tag = if fake { " (fake)" } else { "" };
    for event in said.iter().flatten() {
        match event {
            Event::Set { was, now } => println!("power{tag}: set 0x601 {now:#x} (was {was:#x})"),
            Event::Restored(restore, cause) => {
                println!("power{tag}: restored {restore:x?}: {cause:?}");
            }
            Event::Latched(cause) => println!("power{tag}: latched off until reboot: {cause:?}"),
        }
    }
}
