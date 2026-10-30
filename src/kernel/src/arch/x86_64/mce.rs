//! The machine-check log and recovery from a poisoned user page
//! (`docs/MACHINE-CHECK.md`).
//!
//! Four pieces, each on the processor whose banks it reads:
//!
//! * **Bring-up** ([`bring_up`], `L.x86_64.221`, `222`): the bank count from
//!   `MCG_CAP`, every record the boot before left handed to the poll and
//!   cleared, then every bank enabled -- through the checked MSR accesses, so
//!   a register the processor refuses turns the log off on that processor
//!   and is said, rather than stopping it (`L.x86_64.232`).
//! * **The poll** ([`start`], `L.x86_64.223`): one task pinned to each
//!   processor reads its banks every `ferrix.mce.poll` seconds, prints each
//!   corrected or no-action record as an `mce:` line for lode, counts it, and
//!   clears it; and prints and retires what a machine check handed it.
//! * **The machine check** ([`on_kernel_stack`] from ring 0, on the IST
//!   stack; [`from_user`] from ring 3, on the task's stack): every bank read
//!   and classified (`decode`), then stop, return, hand the record to the
//!   poll, or mark the trap frame for `SIGBUS` (`L.x86_64.224`-`229`).
//! * **The counts** ([`render`], `/proc/ferrix-mce`, `L.x86_64.230`).
//!
//! Nothing here runs on the IST stack but [`on_kernel_stack`] and what it
//! calls, which take no lock, allocate nothing and print nothing (the
//! paranoid entry's rules): they read and write MSRs, atomics and the
//! processor's pending slots.

pub(super) mod check;
pub(super) mod decode;

use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use ferrix_bootinfo::BootView;
use ferrix_sched::{CpuSet, NICE_0_WEIGHT};

use super::cpu;
use super::trap::TrapFrame;
use crate::console::println;
use decode::{Action, Class};

/// `IA32_MCG_CAP`.
const MCG_CAP: u32 = 0x179;
/// `IA32_MCG_STATUS`.
const MCG_STATUS: u32 = 0x17a;
/// `IA32_MCG_CTL`.
const MCG_CTL: u32 = 0x17b;
/// `IA32_MC0_CTL`; bank `b`'s four registers are at `+ 4b`.
const MC0_CTL: u32 = 0x400;

/// The most banks read on one processor (`L.x86_64.232`). `MCG_CAP` allows
/// 255; no part has more than 32, and a count above is clamped and said.
pub(super) const MAX_BANKS: usize = 32;
/// The processors the per-processor state covers: the scheduler's limit.
const PROCESSORS: usize = ferrix_sched::MAX_CPUS;
/// Pending records per processor, handed from a machine check or the
/// bring-up to the poll (`L.x86_64.226`).
pub(super) const SLOTS: usize = 8;

/// The boot word that turns recovery off: every machine check stops, as
/// before this module (`L.x86_64.231`).
const RECOVER_OPTION: &str = "ferrix.mce.recover";
/// The poll period's option, read by `msrlog`'s chooser.
const POLL_OPTION: &str = "ferrix.mce.poll";

/// Bank `bank`'s register `which` (0 `CTL`, 1 `STATUS`, 2 `ADDR`, 3 `MISC`).
const fn bank_msr(bank: usize, which: u32) -> u32 {
    MC0_CTL + 4 * bank as u32 + which
}

/// A processor's record from its bring-up: 0 for none, else [`BROUGHT_UP`]
/// with the bank count in bits 7:0 and [`RECORD_SER`] where the processor
/// reports recovery. A machine check on a processor with none stops.
static RECORDS: [AtomicU32; PROCESSORS] = [const { AtomicU32::new(0) }; PROCESSORS];
/// [`RECORDS`]: brought up.
const BROUGHT_UP: u32 = 1 << 31;
/// [`RECORDS`]: `MCG_SER_P`.
const RECORD_SER: u32 = 1 << 30;
/// [`RECORDS`]: `MCG_CAP` named more banks than [`MAX_BANKS`].
const RECORD_CLAMPED: u32 = 1 << 29;
/// Processors whose bring-up a register refused, said by [`start`].
static REFUSED: [AtomicBool; PROCESSORS] = [const { AtomicBool::new(false) }; PROCESSORS];

/// Whether a recoverable machine check may return (`L.x86_64.231`).
static RECOVER: AtomicBool = AtomicBool::new(true);

/// Per-processor counts, for `/proc/ferrix-mce`.
struct Counts {
    /// Machine checks taken.
    checks: AtomicU64,
    /// Of those, ones that returned.
    recovered: AtomicU64,
    /// Of those, ones with nothing for this processor: a broadcast another
    /// caused, or a spurious check.
    spurious: AtomicU64,
    /// Corrected records the poll found.
    corrected: AtomicU64,
    /// Uncorrected, no-action records the poll found.
    ucna: AtomicU64,
    /// Records the boot before left.
    left: AtomicU64,
    /// Poll rounds run.
    polls: AtomicU64,
}

static COUNTS: [Counts; PROCESSORS] = [const {
    Counts {
        checks: AtomicU64::new(0),
        recovered: AtomicU64::new(0),
        spurious: AtomicU64::new(0),
        corrected: AtomicU64::new(0),
        ucna: AtomicU64::new(0),
        left: AtomicU64::new(0),
        polls: AtomicU64::new(0),
    }
}; PROCESSORS];

/// What a pending record is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Kind {
    /// Read at bring-up: the boot before left it.
    Left = 1,
    /// An action-optional record a machine check handed over.
    Deferred = 2,
}

/// A pending slot's state: written by one processor's machine check or
/// bring-up, read by that processor's poll (`L.x86_64.226`).
///
/// The protocol: a writer moves [`EMPTY`] to [`WRITING`] by compare and
/// exchange, stores the fields, then stores [`FULL`] (release); the reader
/// takes a [`FULL`] slot (acquire), reads the fields, and stores [`EMPTY`].
/// A machine check that interrupts the reader finds the slot [`FULL`] and
/// passes it by; one that interrupts a writer finds it [`WRITING`] and passes
/// it by. So no slot is written while it is read.
pub(super) struct Slot {
    state: AtomicU8,
    kind: AtomicU8,
    bank: AtomicU8,
    status: AtomicU64,
    address: AtomicU64,
    misc: AtomicU64,
}

/// [`Slot`]: free.
pub(super) const EMPTY: u8 = 0;
/// [`Slot`]: a writer holds it.
pub(super) const WRITING: u8 = 1;
/// [`Slot`]: a record waits for the poll.
pub(super) const FULL: u8 = 2;

impl Slot {
    const fn new() -> Self {
        Self {
            state: AtomicU8::new(EMPTY),
            kind: AtomicU8::new(0),
            bank: AtomicU8::new(0),
            status: AtomicU64::new(0),
            address: AtomicU64::new(0),
            misc: AtomicU64::new(0),
        }
    }
}

/// A record taken out of a slot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Record {
    pub(super) kind: Kind,
    pub(super) bank: u8,
    pub(super) status: u64,
    pub(super) address: u64,
    pub(super) misc: u64,
}

static PENDING: [[Slot; SLOTS]; PROCESSORS] =
    [const { [const { Slot::new() }; SLOTS] }; PROCESSORS];

/// Put `record` in a free slot of `slots`; false when every slot is taken.
pub(super) fn put(slots: &[Slot], record: Record) -> bool {
    for slot in slots {
        if slot
            .state
            .compare_exchange(EMPTY, WRITING, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            slot.kind.store(record.kind as u8, Ordering::Relaxed);
            slot.bank.store(record.bank, Ordering::Relaxed);
            slot.status.store(record.status, Ordering::Relaxed);
            slot.address.store(record.address, Ordering::Relaxed);
            slot.misc.store(record.misc, Ordering::Relaxed);
            slot.state.store(FULL, Ordering::Release);
            return true;
        }
    }
    false
}

/// Take the record out of a full slot of `slots`, if there is one.
pub(super) fn take(slots: &[Slot]) -> Option<Record> {
    for slot in slots {
        if slot.state.load(Ordering::Acquire) != FULL {
            continue;
        }
        let kind = if slot.kind.load(Ordering::Relaxed) == Kind::Left as u8 {
            Kind::Left
        } else {
            Kind::Deferred
        };
        let record = Record {
            kind,
            bank: slot.bank.load(Ordering::Relaxed),
            status: slot.status.load(Ordering::Relaxed),
            address: slot.address.load(Ordering::Relaxed),
            misc: slot.misc.load(Ordering::Relaxed),
        };
        slot.state.store(EMPTY, Ordering::Release);
        return Some(record);
    }
    None
}

/// Processor `index`'s pending slots; none for an index past the table.
fn pending(index: usize) -> &'static [Slot] {
    PENDING.get(index).map_or(&[], |slots| slots.as_slice())
}

/// Processor `index`'s counts, for an index in the table.
fn counts(index: usize) -> Option<&'static Counts> {
    COUNTS.get(index)
}

/// This processor's index, if it has a record.
fn here() -> Option<usize> {
    crate::smp::this_cpu()
        .map(|cpu| cpu.logical)
        .filter(|&index| index < PROCESSORS)
}

/// This processor's bring-up record: the bank count and whether it reports
/// recovery, or `None` when it has none.
fn record_of(index: usize) -> Option<(usize, bool)> {
    let record = RECORDS.get(index)?.load(Ordering::Acquire);
    (record & BROUGHT_UP != 0).then_some(((record & 0xff) as usize, record & RECORD_SER != 0))
}

/// Read bank `bank`'s register `which`, which bring-up proved is there.
fn read_bank(bank: usize, which: u32) -> u64 {
    // SAFETY: (SYSREG) bring-up read this bank's registers through the
    // checked read before it recorded the count, so the processor
    // implements them; reading them has no side effect.
    unsafe { cpu::read_msr(bank_msr(bank, which)) }
}

/// Clear bank `bank`'s record.
fn clear_bank(bank: usize) {
    // SAFETY: (SYSREG) writing 0 to an implemented `MCi_STATUS` is the
    // architectural way to clear its record (SDM 16.3.2.2), and changes
    // nothing else.
    unsafe { cpu::write_msr(bank_msr(bank, 1), 0) };
}

/// Clear `MCG_STATUS`: `MCIP` off, so the next machine check is taken
/// rather than shutting the processor down.
fn end_machine_check() {
    // SAFETY: (SYSREG) every processor with a bring-up record has
    // `MCG_STATUS`, read through the checked read there; writing 0 clears
    // `MCIP`, `RIPV` and `EIPV` and nothing else (SDM 16.3.1.2).
    unsafe { cpu::write_msr(MCG_STATUS, 0) };
}

// ---------------------------------------------------------------------------
// Bring-up
// ---------------------------------------------------------------------------

/// `CPUID.1:EDX` bit 14: the machine-check architecture, its banks.
fn has_banks() -> bool {
    core::arch::x86_64::__cpuid(1).edx & (1 << 14) != 0
}

/// `CPUID`'s family and model, as Linux's `x86_family` and `x86_model`.
fn family_model() -> (u32, u32) {
    let eax = core::arch::x86_64::__cpuid(1).eax;
    let base = (eax >> 8) & 0xf;
    let family = if base == 0xf {
        base + ((eax >> 20) & 0xff)
    } else {
        base
    };
    let model = if base >= 6 {
        ((eax >> 4) & 0xf) | (((eax >> 16) & 0xf) << 4)
    } else {
        (eax >> 4) & 0xf
    };
    (family, model)
}

/// Bring this processor's banks up (`L.x86_64.221`, `222`, `232`): called
/// on each processor right after it sets `CR4.MCE`, before it runs a
/// program. Nothing at all where `CPUID` offers no banks.
///
/// Every access is checked: a register the processor refuses leaves the
/// processor without a record -- its machine checks stop as before -- and
/// [`start`] says so.
pub(super) fn bring_up() {
    if !cpu::has_machine_check() || !has_banks() {
        return;
    }
    let Some(index) = here() else {
        return;
    };
    let Some(cap) = cpu::read_msr_checked(MCG_CAP) else {
        return refuse(index);
    };
    let named = (cap & decode::COUNT) as usize;
    let banks = named.min(MAX_BANKS);
    if cpu::read_msr_checked(MCG_STATUS).is_none()
        || !keep_left(index, banks)
        || !enable(cap, banks)
    {
        return refuse(index);
    }
    let record = BROUGHT_UP
        | banks as u32
        | if cap & decode::SER_P != 0 {
            RECORD_SER
        } else {
            0
        }
        | if named > MAX_BANKS { RECORD_CLAMPED } else { 0 };
    if let Some(slot) = RECORDS.get(index) {
        slot.store(record, Ordering::Release);
    }
}

/// Mark processor `index` as one whose bring-up a register refused.
fn refuse(index: usize) {
    if let Some(flag) = REFUSED.get(index) {
        flag.store(true, Ordering::Relaxed);
    }
}

/// Hand every record the boot before left in this processor's first
/// `banks` banks to its poll and clear the bank, before anything is enabled
/// (`L.x86_64.222`); false when a register refused.
fn keep_left(index: usize, banks: usize) -> bool {
    for bank in 0..banks {
        let Some(status) = cpu::read_msr_checked(bank_msr(bank, 1)) else {
            return false;
        };
        if status & decode::VAL == 0 {
            continue;
        }
        let checked = |which, valid| {
            if status & valid != 0 {
                cpu::read_msr_checked(bank_msr(bank, which)).unwrap_or(0)
            } else {
                0
            }
        };
        let record = Record {
            kind: Kind::Left,
            bank: bank as u8,
            status,
            address: checked(2, decode::ADDRV),
            misc: checked(3, decode::MISCV),
        };
        let _ = put(pending(index), record);
        if let Some(counts) = counts(index) {
            let _ = counts.left.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: (SYSREG) clearing an implemented `MCi_STATUS` the checked
        // read just read; refused, the processor keeps its record and the
        // log goes off here.
        if !unsafe { cpu::write_msr_checked(bank_msr(bank, 1), 0) } {
            return false;
        }
    }
    true
}

/// Enable every error the first `banks` banks can report, and `MCG_CTL`
/// where `cap` has it (`L.x86_64.221`); false when a register refused.
fn enable(cap: u64, banks: usize) -> bool {
    // SAFETY: (SYSREG) all ones in `MCG_CTL`, where `MCG_CAP` says it is
    // there, and in each bank's `MCi_CTL`, enable every error the processor
    // can report, as Linux's `__mcheck_cpu_init_generic` and
    // `__mcheck_cpu_init_clear_banks` do; an enabled error is one the
    // processor reports, never one it causes.
    if cap & decode::CTL_P != 0 && !unsafe { cpu::write_msr_checked(MCG_CTL, u64::MAX) } {
        return false;
    }
    // Intel family 6 before model 0x1a: bank 0 is firmware's (Linux's
    // `mce_cpu_quirks`).
    let (family, model) = family_model();
    let skip_bank_0 = intel() && family == 6 && model < 0x1a;
    // SAFETY: (SYSREG) as above.
    (usize::from(skip_bank_0)..banks)
        .all(|bank| unsafe { cpu::write_msr_checked(bank_msr(bank, 0), u64::MAX) })
}

/// Whether `CPUID` names Intel.
fn intel() -> bool {
    let vendor = core::arch::x86_64::__cpuid(0);
    [vendor.ebx, vendor.edx, vendor.ecx]
        .iter()
        .flat_map(|word| word.to_le_bytes())
        .eq(b"GenuineIntel".iter().copied())
}

// ---------------------------------------------------------------------------
// The poll
// ---------------------------------------------------------------------------

/// Start a poll task on every processor with a bring-up record; say which
/// processors have none. Called once from `kmain` after the scheduler.
pub(super) fn start(view: &BootView<'_>) {
    let period = read_options(view);
    // The boot processor took its machine check before it had a processor
    // record (`take_machine_check`, from `init_traps`), so its banks are
    // brought up here, on it, before any program runs.
    if let Some(index) = here()
        && record_of(index).is_none()
        && !refused(index)
    {
        bring_up();
    }
    let Some(topology) = crate::smp::topology() else {
        return;
    };
    let polling = topology
        .cpus()
        .iter()
        .filter(|cpu| start_poll(cpu.logical, period))
        .count();
    println!("mce: polling on {polling} processors");
}

/// `ferrix.mce.recover` and `ferrix.mce.poll`, each said where it is not
/// the default; the poll's period in seconds.
fn read_options(view: &BootView<'_>) -> u64 {
    if view.option(RECOVER_OPTION) == Some("off") {
        RECOVER.store(false, Ordering::Relaxed);
        println!("mce: {RECOVER_OPTION}=off: every machine check stops the kernel");
    }
    let asked = view.option(POLL_OPTION);
    match super::msrlog::choose_period(asked) {
        Some(seconds) if seconds > 0 => seconds,
        _ => {
            println!(
                "mce: {POLL_OPTION}={} is not understood; every {} s",
                asked.unwrap_or(""),
                super::msrlog::DEFAULT_PERIOD
            );
            super::msrlog::DEFAULT_PERIOD
        }
    }
}

/// Whether processor `index`'s bring-up was refused by a register.
fn refused(index: usize) -> bool {
    REFUSED
        .get(index)
        .is_some_and(|flag| flag.load(Ordering::Relaxed))
}

/// Start processor `index`'s poll every `period` seconds, saying why not
/// where it cannot; whether it started.
fn start_poll(index: usize, period: u64) -> bool {
    if refused(index) {
        println!("mce: cpu {index} refused a machine-check register; no log there");
        return false;
    }
    let Some((banks, ser)) = record_of(index) else {
        return false;
    };
    if RECORDS
        .get(index)
        .is_some_and(|record| record.load(Ordering::Relaxed) & RECORD_CLAMPED != 0)
    {
        println!("mce: cpu {index} names more than {MAX_BANKS} banks; the first {MAX_BANKS} read");
    }
    if index == 0 {
        println!(
            "mce: {banks} banks, recovery {}, every {period} s",
            if ser { "reported" } else { "not reported" }
        );
    }
    let mut only = CpuSet::empty();
    // NOALLOC: a CpuSet is a fixed bitmap; insert sets a bit.
    if only.insert(index).is_err() {
        return false;
    }
    let argument = (index << 16) | period as usize;
    match crate::sched::spawn_on("mce", run, argument, NICE_0_WEIGHT, index, only) {
        Ok(_) => true,
        Err(why) => {
            println!("mce: cpu {index}: no poll task: {why}");
            false
        }
    }
}

/// A processor's poll task: `argument` is its index above 16 bits and the
/// period in seconds below.
fn run(argument: usize) {
    let index = argument >> 16;
    let period = (argument & 0xffff) as u64;
    loop {
        poll(index);
        crate::sched::sleep_for(period.saturating_mul(1_000_000_000));
    }
}

/// One round on processor `index`, which this task is pinned to
/// (`L.x86_64.223`): what bring-up and machine checks handed over, then
/// every bank's corrected and no-action records, each printed, counted and
/// cleared. Holds no lock while it reads a bank.
pub(super) fn poll(index: usize) {
    let Some((banks, ser)) = record_of(index) else {
        return;
    };
    let Some(counts) = counts(index) else {
        return;
    };
    let _ = counts.polls.fetch_add(1, Ordering::Relaxed);
    while let Some(record) = take(pending(index)) {
        let class = decode::classify(record.status, ser);
        let what = match record.kind {
            Kind::Left => "left from before this boot",
            Kind::Deferred => class.name(),
        };
        say(
            index,
            record.bank,
            what,
            record.status,
            record.address,
            record.misc,
            None,
        );
        if class != Class::Corrected && decode::usable_address(record.status, record.misc, ser) {
            retire(record.address);
        }
    }
    for bank in 0..banks {
        let status = read_bank(bank, 1);
        let class = decode::classify(status, ser);
        let count = match class {
            Class::Corrected => &counts.corrected,
            Class::Ucna => &counts.ucna,
            // Empty; or an uncorrected record whose machine check this
            // processor has not taken yet, which is the check's.
            _ => continue,
        };
        let address = if status & decode::ADDRV != 0 {
            read_bank(bank, 2)
        } else {
            0
        };
        let misc = if status & decode::MISCV != 0 {
            read_bank(bank, 3)
        } else {
            0
        };
        let total = count.fetch_add(1, Ordering::Relaxed).saturating_add(1);
        say(
            index,
            bank as u8,
            class.name(),
            status,
            address,
            misc,
            Some(total),
        );
        clear_bank(bank);
        if class == Class::Ucna && decode::usable_address(status, misc, ser) {
            retire(address);
        }
    }
}

/// Print one record as an `mce:` line: a warning for a corrected one, an
/// error for the rest.
fn say(
    index: usize,
    bank: u8,
    what: &str,
    status: u64,
    address: u64,
    misc: u64,
    total: Option<u64>,
) {
    let mut line = Line {
        bytes: [0; LINE],
        len: 0,
    };
    let _ = write!(
        line,
        "mce: cpu {index} bank {bank} {what} status={status:#018x}"
    );
    if status & decode::ADDRV != 0 {
        let _ = write!(line, " addr={address:#x}");
    }
    if status & decode::MISCV != 0 {
        let _ = write!(line, " misc={misc:#x}");
    }
    if let Some(total) = total {
        let _ = write!(line, " total={total}");
    }
    let priority = if status & decode::UC == 0 {
        WARNING
    } else {
        ERROR
    };
    crate::console::write_record(priority, line.bytes.get(..line.len).unwrap_or_default());
}

/// The longest `mce:` line: the longest a record makes is under 120 bytes.
const LINE: usize = 160;

/// An `mce:` line formatted on the stack: the item allocates nothing it
/// cannot refuse (F-23), and a line needs no heap. Text past [`LINE`] is
/// cut.
struct Line {
    bytes: [u8; LINE],
    len: usize,
}

impl Write for Line {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let room = LINE - self.len;
        let take = text.len().min(room);
        if let (Some(to), Some(from)) = (
            self.bytes.get_mut(self.len..self.len + take),
            text.as_bytes().get(..take),
        ) {
            to.copy_from_slice(from);
            self.len += take;
        }
        Ok(())
    }
}

/// `LOG_KERN | LOG_WARNING`.
const WARNING: u8 = 4;
/// `LOG_KERN | LOG_ERR`.
const ERROR: u8 = 3;

/// Retire the frame holding `address`, and say what came of it.
fn retire(address: u64) {
    let frame = address / ferrix_bootinfo::PAGE_SIZE;
    let outcome = crate::mm::retire_frame(frame);
    let said = match outcome {
        crate::mm::Retired::Claimed | crate::mm::Retired::Marked => "retired",
        crate::mm::Retired::Already => "already retired",
        crate::mm::Retired::Full | crate::mm::Retired::NotCovered => "not retired",
    };
    println!(
        "mce: frame {:#x} {said}",
        frame * ferrix_bootinfo::PAGE_SIZE
    );
}

// ---------------------------------------------------------------------------
// The machine check
// ---------------------------------------------------------------------------

/// Read and classify every bank of this processor under `mcg_status`, from
/// ring 3 when `user`: the folded action, and the bank a `Signal` names.
/// Reads only.
fn decide(banks: usize, ser: bool, mcg_status: u64, user: bool) -> (Action, Option<usize>) {
    let mut folded = Action::Leave;
    let mut signalled = None;
    for bank in 0..banks {
        let status = read_bank(bank, 1);
        let class = decode::classify(status, ser);
        if class == Class::Empty {
            continue;
        }
        let misc = if status & decode::MISCV != 0 {
            read_bank(bank, 3)
        } else {
            0
        };
        let action = decode::action(class, status, misc, mcg_status, user, ser);
        if matches!(action, Action::Signal { .. }) {
            signalled = Some(bank);
        }
        folded = decode::fold(folded, action);
    }
    (decode::finish(folded, mcg_status), signalled)
}

/// Hand every action-optional bank to the poll and clear it; false when a
/// slot ran out, which stops the kernel (`L.x86_64.226`).
fn defer_banks(index: usize, banks: usize, ser: bool, mcg_status: u64, user: bool) -> bool {
    for bank in 0..banks {
        let status = read_bank(bank, 1);
        let class = decode::classify(status, ser);
        let misc = if status & decode::MISCV != 0 {
            read_bank(bank, 3)
        } else {
            0
        };
        if decode::action(class, status, misc, mcg_status, user, ser) != Action::Defer {
            continue;
        }
        let address = if status & decode::ADDRV != 0 {
            read_bank(bank, 2)
        } else {
            0
        };
        let record = Record {
            kind: Kind::Deferred,
            bank: bank as u8,
            status,
            address,
            misc,
        };
        if !put(pending(index), record) {
            return false;
        }
        clear_bank(bank);
    }
    true
}

/// A machine check taken in ring 0, on its IST stack (`L.x86_64.226`,
/// `227`, `229`): returns when every bank's action lets the kernel go on,
/// with the action-optional ones handed to the poll and `MCIP` clear; stops
/// as FX-9005 with the banks printed otherwise. Takes no lock, allocates
/// nothing, prints nothing before it stops.
pub(super) fn on_kernel_stack(frame: &mut TrapFrame) {
    let Some(index) = here() else {
        stop(frame);
    };
    let Some(counts) = counts(index) else {
        stop(frame);
    };
    let _ = counts.checks.fetch_add(1, Ordering::Relaxed);
    let Some((banks, ser)) = record_of(index) else {
        stop(frame);
    };
    if !RECOVER.load(Ordering::Relaxed) {
        stop(frame);
    }
    // SAFETY: (SYSREG) this processor has `MCG_STATUS`: its bring-up read it.
    let mcg_status = unsafe { cpu::read_msr(MCG_STATUS) };
    match decide(banks, ser, mcg_status, false).0 {
        Action::Leave | Action::Unaffected => {
            let _ = counts.spurious.fetch_add(1, Ordering::Relaxed);
        }
        Action::Defer => {
            if !defer_banks(index, banks, ser, mcg_status, false) {
                stop(frame);
            }
        }
        Action::Signal { .. } | Action::Stop => stop(frame),
    }
    let _ = counts.recovered.fetch_add(1, Ordering::Relaxed);
    end_machine_check();
}

/// [`TrapFrame::error_code`] of a ring-3 machine check marked for `SIGBUS`:
/// the record's physical page, and these two bits below it. A machine
/// check pushes no error code of its own (the stub pushes 0), so the field
/// is free to carry the decision from [`from_user`] to [`signal_for`]
/// across the interrupts the latter runs with.
const MARKED: u64 = 1 << 0;
/// [`MARKED`]'s companion: the instruction cannot be restarted.
const MARKED_KILL: u64 = 1 << 1;

/// A machine check taken in ring 3, moved to the task's stack by the
/// paranoid entry, before anything else of the trap path runs and with
/// interrupts masked (`L.x86_64.228`). Returns `true` when the trap path is
/// to go on with the frame marked for `SIGBUS`, `false` when the program
/// resumes as it was; stops as FX-9005 otherwise.
pub(super) fn from_user(frame: &mut TrapFrame) -> bool {
    let Some(index) = here() else {
        stop(frame);
    };
    let Some(counts) = counts(index) else {
        stop(frame);
    };
    let _ = counts.checks.fetch_add(1, Ordering::Relaxed);
    let Some((banks, ser)) = record_of(index) else {
        stop(frame);
    };
    if !RECOVER.load(Ordering::Relaxed) {
        stop(frame);
    }
    // SAFETY: (SYSREG) as in `on_kernel_stack`.
    let mcg_status = unsafe { cpu::read_msr(MCG_STATUS) };
    let (action, signalled) = decide(banks, ser, mcg_status, true);
    let marked = match (action, signalled) {
        (Action::Stop, _) => stop(frame),
        (Action::Signal { kill }, Some(bank)) => {
            let page = read_bank(bank, 2) & !(ferrix_bootinfo::PAGE_SIZE - 1);
            frame.error_code = page | MARKED | if kill { MARKED_KILL } else { 0 };
            clear_bank(bank);
            true
        }
        (Action::Signal { .. }, None) => stop(frame),
        (Action::Leave | Action::Unaffected, _) => {
            let _ = counts.spurious.fetch_add(1, Ordering::Relaxed);
            false
        }
        (Action::Defer, _) => false,
    };
    if !defer_banks(index, banks, ser, mcg_status, true) {
        stop(frame);
    }
    let _ = counts.recovered.fetch_add(1, Ordering::Relaxed);
    end_machine_check();
    marked
}

/// Stop as FX-9005; the report prints the banks ([`report`]).
fn stop(frame: &TrapFrame) -> ! {
    crate::trap::fatal(
        frame,
        "machine check",
        &crate::panic::catalog::MACHINE_CHECK,
    )
}

/// Whether `frame` is a ring-3 machine check [`from_user`] marked.
pub(super) const fn marked(frame: &TrapFrame) -> bool {
    frame.vector == 18 && frame.error_code & MARKED != 0
}

/// `si_code` `BUS_MCEERR_AR`: a machine check, action required.
const BUS_MCEERR_AR: i32 = 4;

/// The signal for a marked ring-3 machine check, run with interrupts open
/// (`L.x86_64.228`): the mark taken off the frame, the frame retired, then `SIGBUS` with `BUS_MCEERR_AR`
/// at the address the running program maps it at (0 when the walk finds
/// none), and whether it must end the program rather than run a handler.
pub(super) fn signal_for(frame: &mut TrapFrame) -> (u32, i32, u64, bool) {
    let page = frame.error_code & !(ferrix_bootinfo::PAGE_SIZE - 1);
    let kill = frame.error_code & MARKED_KILL != 0;
    // The mark held a physical address, and the signal frame copies the
    // trap's error code into the program's context (`uc_mcontext.err`):
    // cleared before the signal is raised, as the processor's own 0 for a
    // machine check (the consultant's V1, `L.x86_64.228`).
    frame.error_code = 0;
    retire(page);
    let address = crate::sched::current()
        .and_then(|task| task.address_space().cloned())
        .and_then(|space| space.find_frame(page / ferrix_bootinfo::PAGE_SIZE))
        .unwrap_or(0);
    (
        ferrix_linux_abi::types::SIGBUS,
        BUS_MCEERR_AR,
        address,
        kill,
    )
}

// ---------------------------------------------------------------------------
// Reports
// ---------------------------------------------------------------------------

/// The banks, for the report of a machine check that stops
/// (`L.x86_64.227`): each valid one's raw registers and class. Read again
/// here, since a stopping check clears nothing. Unlogged, as the rest of the
/// report.
pub(super) fn report() {
    use crate::console::println_unlogged;

    let Some((banks, ser)) = here().and_then(record_of) else {
        println_unlogged!("  mce      no bank record on this processor");
        return;
    };
    let mcg_status = cpu::read_msr_checked(MCG_STATUS).unwrap_or(0);
    println_unlogged!("  mce      mcg_status {mcg_status:#x}, {banks} banks");
    for bank in 0..banks {
        let status = read_bank(bank, 1);
        if status & decode::VAL == 0 {
            continue;
        }
        let address = if status & decode::ADDRV != 0 {
            read_bank(bank, 2)
        } else {
            0
        };
        let misc = if status & decode::MISCV != 0 {
            read_bank(bank, 3)
        } else {
            0
        };
        println_unlogged!(
            "mce: bank {bank} status={status:#018x} addr={address:#x} misc={misc:#x} {}",
            decode::classify(status, ser).name()
        );
    }
}

/// `/proc/ferrix-mce`'s text (`L.x86_64.230`): the bank count, then one line
/// of counts per processor. No address: the retired frames are in
/// `/proc/ferrix-mce-retired`, readable by root alone.
pub(crate) fn render(out: &mut impl Write) -> core::fmt::Result {
    let Some(topology) = crate::smp::topology() else {
        return Ok(());
    };
    writeln!(
        out,
        "recover {}",
        if RECOVER.load(Ordering::Relaxed) {
            "on"
        } else {
            "off"
        }
    )?;
    for cpu in topology.cpus() {
        let index = cpu.logical;
        let Some(counts) = COUNTS.get(index) else {
            continue;
        };
        let banks = record_of(index).map_or(0, |(banks, _)| banks);
        writeln!(
            out,
            "cpu{index} banks {banks} checks {} recovered {} spurious {} corrected {} ucna {} \
             left {} polls {}",
            counts.checks.load(Ordering::Relaxed),
            counts.recovered.load(Ordering::Relaxed),
            counts.spurious.load(Ordering::Relaxed),
            counts.corrected.load(Ordering::Relaxed),
            counts.ucna.load(Ordering::Relaxed),
            counts.left.load(Ordering::Relaxed),
            counts.polls.load(Ordering::Relaxed),
        )?;
    }
    Ok(())
}
