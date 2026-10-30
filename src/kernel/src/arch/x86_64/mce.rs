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

use super::trap::TrapFrame;
use super::{copy_mc, cpu};
use crate::console::println;
use decode::{Action, BankConfig, Class, Context, Vendor};

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

/// AMD's `MCA_CTL` of bank 0 in the SMCA range; bank `b`'s registers are at
/// `+ 0x10b`.
const SMCA_MC0: u32 = 0xc000_2000;
/// [`smca_msr`]: `MCA_CONFIG`.
const SMCA_CONFIG: u32 = 4;
/// [`smca_msr`]: `MCA_IPID`.
const SMCA_IPID: u32 = 5;
/// [`smca_msr`]: `MCA_SYND`.
const SMCA_SYND: u32 = 6;

/// SMCA bank `bank`'s register `which` ([`SMCA_CONFIG`], [`SMCA_IPID`],
/// [`SMCA_SYND`]).
const fn smca_msr(bank: usize, which: u32) -> u32 {
    SMCA_MC0 + 0x10 * bank as u32 + which
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
/// [`RECORDS`]: the vendor's rules, bits 9:8 -- [`RECORD_INTEL`],
/// [`RECORD_AMD`] or [`RECORD_NONE`] (`L.x86_64.233`).
const RECORD_VENDOR: u32 = 3 << 8;
/// [`RECORD_VENDOR`]: Intel's rules.
const RECORD_INTEL: u32 = 1 << 8;
/// [`RECORD_VENDOR`]: AMD's rules.
const RECORD_AMD: u32 = 2 << 8;
/// [`RECORD_VENDOR`]: no recovery.
const RECORD_NONE: u32 = 3 << 8;
/// [`RECORDS`], AMD: `SUCCOR`.
const RECORD_SUCCOR: u32 = 1 << 10;
/// [`RECORDS`], AMD: `OverflowRecov`.
const RECORD_OVERFLOW_RECOV: u32 = 1 << 11;
/// [`RECORDS`], AMD: scalable MCA.
const RECORD_SMCA: u32 = 1 << 12;

/// Each processor's SMCA banks' `MCA_CONFIG` as bring-up kept it
/// (`decode::bank_config`); 0 where it was not read.
static BANK_CONFIG: [[AtomicU8; MAX_BANKS]; PROCESSORS] =
    [const { [const { AtomicU8::new(0) }; MAX_BANKS] }; PROCESSORS];
/// Each processor's SMCA banks' `MCA_IPID`, read once at bring-up (the
/// consultant's A6); 0 where it was not read.
static BANK_IPID: [[AtomicU64; MAX_BANKS]; PROCESSORS] =
    [const { [const { AtomicU64::new(0) }; MAX_BANKS] }; PROCESSORS];
/// SMCA register accesses each processor's bring-up was refused, said by
/// [`start`] (`L.x86_64.239`).
static SMCA_REFUSED: [AtomicU32; PROCESSORS] = [const { AtomicU32::new(0) }; PROCESSORS];
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
    /// AMD's deferred records the poll found.
    deferred: AtomicU64,
    /// Records the boot before left.
    left: AtomicU64,
    /// Poll rounds run.
    polls: AtomicU64,
    /// Of the recovered, ones at a system call's copy
    /// (`docs/MACHINE-CHECK-COPY.md` §3.4).
    copies: AtomicU64,
}

static COUNTS: [Counts; PROCESSORS] = [const {
    Counts {
        checks: AtomicU64::new(0),
        recovered: AtomicU64::new(0),
        spurious: AtomicU64::new(0),
        corrected: AtomicU64::new(0),
        ucna: AtomicU64::new(0),
        deferred: AtomicU64::new(0),
        left: AtomicU64::new(0),
        polls: AtomicU64::new(0),
        copies: AtomicU64::new(0),
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

/// What bring-up recorded of one processor.
#[derive(Clone, Copy, Debug)]
struct Proc {
    /// Its index.
    index: usize,
    /// Its banks, at most [`MAX_BANKS`].
    banks: usize,
    /// `MCG_SER_P`, which Intel's rules read.
    ser: bool,
    /// Whose rules its records are read by.
    vendor: Vendor,
}

impl Proc {
    /// Whether the processor has SMCA's per-bank registers.
    const fn smca(self) -> bool {
        matches!(self.vendor, Vendor::Amd { smca: true, .. })
    }

    /// Whether the processor reports recovery under its vendor's rules.
    const fn recovers(self) -> bool {
        match self.vendor {
            Vendor::Intel => self.ser,
            Vendor::Amd { succor, .. } => succor,
            Vendor::NoRecovery => false,
        }
    }

    /// Bank `bank`'s `MCA_CONFIG` as bring-up kept it.
    fn config(self, bank: usize) -> BankConfig {
        BANK_CONFIG
            .get(self.index)
            .and_then(|banks| banks.get(bank))
            .map_or(0, |config| config.load(Ordering::Relaxed))
    }

    /// Bank `bank`'s `MCA_IPID` as bring-up read it, on an SMCA part.
    fn ipid(self, bank: usize) -> Option<u64> {
        if !self.smca() || self.config(bank) & decode::BANK_CONFIG_READ == 0 {
            return None;
        }
        BANK_IPID
            .get(self.index)
            .and_then(|banks| banks.get(bank))
            .map(|ipid| ipid.load(Ordering::Relaxed))
    }

    /// The class of a raw `MCi_STATUS` under the processor's vendor.
    const fn class(self, status: u64) -> Class {
        decode::classify_by(self.vendor, status, self.ser)
    }
}

/// Processor `index`'s bring-up record, or `None` when it has none.
fn record_of(index: usize) -> Option<Proc> {
    let record = RECORDS.get(index)?.load(Ordering::Acquire);
    if record & BROUGHT_UP == 0 {
        return None;
    }
    let vendor = match record & RECORD_VENDOR {
        RECORD_INTEL => Vendor::Intel,
        RECORD_AMD => Vendor::Amd {
            succor: record & RECORD_SUCCOR != 0,
            overflow_recov: record & RECORD_OVERFLOW_RECOV != 0,
            smca: record & RECORD_SMCA != 0,
        },
        _ => Vendor::NoRecovery,
    };
    Some(Proc {
        index,
        banks: (record & 0xff) as usize,
        ser: record & RECORD_SER != 0,
        vendor,
    })
}

/// The vendor's rules for this processor and `CPUID 8000_0007h:EBX`, the
/// latter 0 where the leaf is not offered (`L.x86_64.233`).
fn this_vendor() -> (Vendor, u32) {
    let id = core::arch::x86_64::__cpuid(0);
    let mut name = [0_u8; 12];
    for (chunk, word) in name.chunks_exact_mut(4).zip([id.ebx, id.edx, id.ecx]) {
        chunk.copy_from_slice(&word.to_le_bytes());
    }
    let ebx = if core::arch::x86_64::__cpuid(0x8000_0000).eax >= 0x8000_0007 {
        core::arch::x86_64::__cpuid(0x8000_0007).ebx
    } else {
        0
    };
    (decode::vendor_of(&name, ebx), ebx)
}

/// The record bits for a vendor.
const fn vendor_bits(vendor: Vendor) -> u32 {
    match vendor {
        Vendor::Intel => RECORD_INTEL,
        Vendor::Amd {
            succor,
            overflow_recov,
            smca,
        } => {
            RECORD_AMD
                | if succor { RECORD_SUCCOR } else { 0 }
                | if overflow_recov {
                    RECORD_OVERFLOW_RECOV
                } else {
                    0
                }
                | if smca { RECORD_SMCA } else { 0 }
        }
        Vendor::NoRecovery => RECORD_NONE,
    }
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
    let (vendor, _) = this_vendor();
    if matches!(vendor, Vendor::Amd { smca: true, .. }) {
        smca_bank_up(index, banks);
    }
    let record = BROUGHT_UP
        | vendor_bits(vendor)
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

/// SMCA's per-bank registers on processor `index` (`L.x86_64.239`,
/// `240`): each bank's `MCA_CONFIG` and `MCA_IPID` through the checked read,
/// the `CONFIG` bits kept and the IPID cached, and `CONFIG` written back
/// with `McaX` set and nothing else changed, through the checked write. A
/// refusal leaves that bank's address unusable and its IPID unread, and is
/// counted for [`start`] to say; it does not refuse the processor. The one
/// function that touches `MCA_CONFIG` and `MCA_IPID` (check-device-access.py).
fn smca_bank_up(index: usize, banks: usize) {
    let mut refused = 0_u32;
    for bank in 0..banks {
        let config = smca_msr(bank, SMCA_CONFIG);
        let Some(raw) = cpu::read_msr_checked(config) else {
            refused += 1;
            continue;
        };
        // SAFETY: (SYSREG) `McaX` is the OS's acknowledgement that it uses
        // the SMCA registers, without which uncorrectable errors are fatal
        // (Linux 6.18's `smca_configure`); the value is the one just read
        // with that bit set, so nothing else in the bank's configuration
        // changes. Refused, the bank keeps its configuration, and its
        // address stays unusable: nothing is kept for it (B2).
        if !unsafe { cpu::write_msr_checked(config, decode::with_mcax(raw)) } {
            refused += 1;
            continue;
        }
        let Some(ipid) = cpu::read_msr_checked(smca_msr(bank, SMCA_IPID)) else {
            refused += 1;
            continue;
        };
        if let (Some(kept), Some(cached)) = (
            BANK_CONFIG.get(index).and_then(|banks| banks.get(bank)),
            BANK_IPID.get(index).and_then(|banks| banks.get(bank)),
        ) {
            cached.store(ipid, Ordering::Relaxed);
            kept.store(decode::bank_config(raw), Ordering::Relaxed);
        }
    }
    if let Some(count) = SMCA_REFUSED.get(index) {
        count.store(refused, Ordering::Relaxed);
    }
}

/// SMCA bank `bank`'s `MCA_SYND`, where its status says it holds one and
/// bring-up read the bank's registers (`L.x86_64.239`): through the checked
/// read, so a refusal is a missing syndrome, never a fault. The one place
/// that reads `MCA_SYND` (check-device-access.py).
fn read_synd(proc: Proc, bank: usize, status: u64) -> Option<u64> {
    if status & decode::SYNDV == 0 || proc.ipid(bank).is_none() {
        return None;
    }
    cpu::read_msr_checked(smca_msr(bank, SMCA_SYND))
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
    let Some(proc) = record_of(index) else {
        return false;
    };
    let refused_smca = SMCA_REFUSED
        .get(index)
        .map_or(0, |count| count.load(Ordering::Relaxed));
    if refused_smca > 0 {
        println!(
            "mce: cpu {index} refused {refused_smca} SMCA register accesses; those banks' \
             addresses are unusable"
        );
    }
    if RECORDS
        .get(index)
        .is_some_and(|record| record.load(Ordering::Relaxed) & RECORD_CLAMPED != 0)
    {
        println!("mce: cpu {index} names more than {MAX_BANKS} banks; the first {MAX_BANKS} read");
    }
    if index == 0 {
        let (_, ebx) = this_vendor();
        println!(
            "mce: {} banks, {} rules, cpuid 80000007 ebx={ebx:#x}, recovery {}, every {period} s",
            proc.banks,
            proc.vendor.name(),
            if proc.recovers() {
                "reported"
            } else {
                "not reported"
            }
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
    let Some(proc) = record_of(index) else {
        return;
    };
    let Some(counts) = counts(index) else {
        return;
    };
    let _ = counts.polls.fetch_add(1, Ordering::Relaxed);
    while let Some(record) = take(pending(index)) {
        let bank = usize::from(record.bank);
        let what = match record.kind {
            Kind::Left => "left from before this boot",
            Kind::Deferred => proc.class(record.status).name(),
        };
        say(
            proc,
            bank,
            what,
            record.status,
            record.address,
            record.misc,
            None,
        );
        if let Some(page) = retirable(proc, bank, record.status, record.address, record.misc) {
            retire(page);
        }
    }
    for bank in 0..proc.banks {
        let status = read_bank(bank, 1);
        let count = match proc.class(status) {
            Class::Corrected => &counts.corrected,
            Class::Ucna => &counts.ucna,
            Class::Deferred => &counts.deferred,
            // Empty; or an uncorrected record whose machine check this
            // processor has not taken yet, which is the check's.
            _ => continue,
        };
        let address = address_of(bank, status);
        let misc = misc_of(bank, status);
        let total = count.fetch_add(1, Ordering::Relaxed).saturating_add(1);
        say(
            proc,
            bank,
            proc.class(status).name(),
            status,
            address,
            misc,
            Some(total),
        );
        clear_bank(bank);
        if let Some(page) = retirable(proc, bank, status, address, misc) {
            retire(page);
        }
    }
}

/// Bank `bank`'s `MCi_ADDR`, read only where `status` says it holds one.
fn address_of(bank: usize, status: u64) -> u64 {
    if status & decode::ADDRV != 0 {
        read_bank(bank, 2)
    } else {
        0
    }
}

/// Bank `bank`'s `MCi_MISC`, read only where `status` says it holds one.
fn misc_of(bank: usize, status: u64) -> u64 {
    if status & decode::MISCV != 0 {
        read_bank(bank, 3)
    } else {
        0
    }
}

/// The frame a record names, where the poll may retire it: Intel's
/// uncorrected records with a usable address; AMD's action-required and
/// fatal ones with a usable address cut to its frame; AMD's deferred ones
/// only by `PhysAddrV` (`L.x86_64.242`); never a corrected record.
fn retirable(proc: Proc, bank: usize, status: u64, address: u64, misc: u64) -> Option<u64> {
    let (smca, config) = (proc.smca(), proc.config(bank));
    match (proc.class(status), proc.vendor) {
        (Class::Empty | Class::Corrected, _) => None,
        (Class::Deferred, _) => decode::deferred_retirable(status, smca, config)
            .then(|| decode::cut_address(address, status, smca, config))
            .flatten(),
        (_, Vendor::Amd { .. }) => decode::usable_address_amd(status, smca, config)
            .then(|| decode::cut_address(address, status, smca, config))
            .flatten(),
        (_, Vendor::Intel | Vendor::NoRecovery) => decode::usable_address(status, misc, proc.ser)
            .then_some(address & !(ferrix_bootinfo::PAGE_SIZE - 1)),
    }
}

/// Print one record as an `mce:` line: a warning for a corrected one, an
/// error for the rest. An SMCA bank adds its `ipid=` and, with `SyndV`, its
/// `synd=` (`L.x86_64.241`).
fn say(
    proc: Proc,
    bank: usize,
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
    let index = proc.index;
    let _ = write!(
        line,
        "mce: cpu {index} bank {bank} {what} status={status:#018x}"
    );
    let _ = write_fields(
        &mut line,
        status,
        address,
        misc,
        proc.ipid(bank),
        read_synd(proc, bank, status),
    );
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

/// A record's optional fields as an `mce:` line carries them: `addr=` and
/// `misc=` where `status` says they are valid, and an SMCA bank's `ipid=`
/// and `synd=` (`L.x86_64.241`). Pure, for stage 3's table.
pub(super) fn write_fields(
    out: &mut impl Write,
    status: u64,
    address: u64,
    misc: u64,
    ipid: Option<u64>,
    synd: Option<u64>,
) -> core::fmt::Result {
    if status & decode::ADDRV != 0 {
        write!(out, " addr={address:#x}")?;
    }
    if status & decode::MISCV != 0 {
        write!(out, " misc={misc:#x}")?;
    }
    if let Some(ipid) = ipid {
        write!(out, " ipid={ipid:#x}")?;
    }
    if let Some(synd) = synd {
        write!(out, " synd={synd:#x}")?;
    }
    Ok(())
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
pub(super) fn retire(address: u64) {
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

/// The action one valid bank asks for under the processor's vendor: AMD's
/// [`decode::amd_action`] with its usable, cut address, else Intel's
/// [`decode::action`] (no-recovery vendors' uncorrected records are fatal
/// by then). Reads only.
fn bank_action(proc: Proc, bank: usize, status: u64, mcg_status: u64, context: Context) -> Action {
    let class = proc.class(status);
    match proc.vendor {
        Vendor::Amd { smca, .. } => {
            let config = proc.config(bank);
            let usable = decode::usable_address_amd(status, smca, config)
                && decode::cut_address(address_of(bank, status), status, smca, config).is_some();
            decode::amd_action(class, status, mcg_status, context, usable)
        }
        Vendor::Intel | Vendor::NoRecovery => decode::action(
            class,
            status,
            misc_of(bank, status),
            mcg_status,
            context,
            proc.ser,
        ),
    }
}

/// The frame a signalled bank's record names: AMD's cut address, or
/// Intel's address's page.
fn signal_page(proc: Proc, bank: usize, status: u64) -> Option<u64> {
    let address = address_of(bank, status);
    match proc.vendor {
        Vendor::Amd { smca, .. } => decode::cut_address(address, status, smca, proc.config(bank)),
        Vendor::Intel | Vendor::NoRecovery => Some(address & !(ferrix_bootinfo::PAGE_SIZE - 1)),
    }
}

/// Read and classify every bank of this processor under `mcg_status`, in
/// `context`: the folded action, and the bank a `Signal` or `Recover` names.
/// Reads only.
fn decide(proc: Proc, mcg_status: u64, context: Context) -> (Action, Option<usize>) {
    let mut folded = Action::Leave;
    let mut signalled = None;
    for bank in 0..proc.banks {
        let status = read_bank(bank, 1);
        if proc.class(status) == Class::Empty {
            continue;
        }
        let action = bank_action(proc, bank, status, mcg_status, context);
        if matches!(action, Action::Signal { .. } | Action::Recover) {
            signalled = Some(bank);
        }
        folded = decode::fold(folded, action);
    }
    (decode::finish(folded, mcg_status), signalled)
}

/// Hand every action-optional bank to the poll and clear it; false when a
/// slot ran out, which stops the kernel (`L.x86_64.226`). Only Intel's rules
/// defer: AMD's never ask for it.
fn defer_banks(proc: Proc, mcg_status: u64, context: Context) -> bool {
    for bank in 0..proc.banks {
        let status = read_bank(bank, 1);
        if bank_action(proc, bank, status, mcg_status, context) != Action::Defer {
            continue;
        }
        let record = Record {
            kind: Kind::Deferred,
            bank: bank as u8,
            status,
            address: address_of(bank, status),
            misc: misc_of(bank, status),
        };
        if !put(pending(proc.index), record) {
            return false;
        }
        clear_bank(bank);
    }
    true
}

/// A machine check taken in ring 0, on its IST stack (`L.x86_64.226`,
/// `227`, `229`, `246`): returns when every bank's action lets the kernel go
/// on, with the action-optional ones handed to the poll and `MCIP` clear --
/// a consumption at the system call's copy sent to the copy's exit with the
/// page in `rdx`; stops as FX-9005 with the banks printed otherwise. Takes no
/// lock, allocates nothing, prints nothing before it stops.
pub(super) fn on_kernel_stack(frame: &mut TrapFrame) {
    let Some(index) = here() else {
        stop(frame);
    };
    let Some(counts) = counts(index) else {
        stop(frame);
    };
    let _ = counts.checks.fetch_add(1, Ordering::Relaxed);
    let Some(proc) = record_of(index) else {
        stop(frame);
    };
    if !RECOVER.load(Ordering::Relaxed) {
        stop(frame);
    }
    // SAFETY: (SYSREG) this processor has `MCG_STATUS`: its bring-up read it.
    let mcg_status = unsafe { cpu::read_msr(MCG_STATUS) };
    let context = decode::context(false, frame.rip, frame.rcx, copy_mc::movs_address());
    match decide(proc, mcg_status, context) {
        (Action::Leave | Action::Unaffected, _) => {
            let _ = counts.spurious.fetch_add(1, Ordering::Relaxed);
        }
        (Action::Defer, _) => {
            if !defer_banks(proc, mcg_status, context) {
                stop(frame);
            }
        }
        (Action::Recover, Some(bank)) => {
            let page = signal_page(proc, bank, read_bank(bank, 1));
            match page {
                Some(page) if decode::same_page(page, crate::mm::ram_behind(frame.rsi)) => {
                    recover_copy(frame, page);
                }
                _ => stop(frame),
            }
            clear_bank(bank);
            if !defer_banks(proc, mcg_status, context) {
                stop(frame);
            }
            let _ = counts.copies.fetch_add(1, Ordering::Relaxed);
        }
        (Action::Recover, None) | (Action::Signal { .. } | Action::Stop, _) => stop(frame),
    }
    let _ = counts.recovered.fetch_add(1, Ordering::Relaxed);
    end_machine_check();
}

/// Send a system call's copy stopped at its `rep movsb` to its exit, with
/// the poisoned `page` and bit 0 in `rdx` (`L.x86_64.246`): `rcx`, the bytes
/// left, is the processor's own and stays.
pub(super) fn recover_copy(frame: &mut TrapFrame, page: u64) {
    frame.rip = copy_mc::out_address();
    frame.rdx = page | 1;
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
    let Some(proc) = record_of(index) else {
        stop(frame);
    };
    if !RECOVER.load(Ordering::Relaxed) {
        stop(frame);
    }
    // SAFETY: (SYSREG) as in `on_kernel_stack`.
    let mcg_status = unsafe { cpu::read_msr(MCG_STATUS) };
    let (action, signalled) = decide(proc, mcg_status, Context::Ring3);
    let marked = match (action, signalled) {
        (Action::Stop, _) => stop(frame),
        (Action::Signal { kill }, Some(bank)) => {
            let Some(page) = signal_page(proc, bank, read_bank(bank, 1)) else {
                stop(frame);
            };
            frame.error_code = page | MARKED | if kill { MARKED_KILL } else { 0 };
            clear_bank(bank);
            true
        }
        (Action::Signal { .. } | Action::Recover, None) | (Action::Recover, Some(_)) => stop(frame),
        (Action::Leave | Action::Unaffected, _) => {
            let _ = counts.spurious.fetch_add(1, Ordering::Relaxed);
            false
        }
        (Action::Defer, _) => false,
    };
    if !defer_banks(proc, mcg_status, Context::Ring3) {
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

    let Some(proc) = here().and_then(record_of) else {
        println_unlogged!("  mce      no bank record on this processor");
        return;
    };
    let mcg_status = cpu::read_msr_checked(MCG_STATUS).unwrap_or(0);
    println_unlogged!(
        "  mce      mcg_status {mcg_status:#x}, {} banks, {} rules",
        proc.banks,
        proc.vendor.name()
    );
    for bank in 0..proc.banks {
        let status = read_bank(bank, 1);
        if status & decode::VAL == 0 {
            continue;
        }
        let mut line = Line {
            bytes: [0; LINE],
            len: 0,
        };
        let _ = write!(line, "mce: bank {bank} status={status:#018x}");
        let _ = write_fields(
            &mut line,
            status | decode::ADDRV | decode::MISCV,
            address_of(bank, status),
            misc_of(bank, status),
            proc.ipid(bank),
            read_synd(proc, bank, status),
        );
        let _ = write!(line, " {}", proc.class(status).name());
        println_unlogged!(
            "{}",
            core::str::from_utf8(line.bytes.get(..line.len).unwrap_or_default()).unwrap_or("")
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
        let banks = record_of(index).map_or(0, |proc| proc.banks);
        writeln!(
            out,
            "cpu{index} banks {banks} checks {} recovered {} spurious {} corrected {} ucna {} \
             deferred {} left {} polls {} copies {}",
            counts.checks.load(Ordering::Relaxed),
            counts.recovered.load(Ordering::Relaxed),
            counts.spurious.load(Ordering::Relaxed),
            counts.corrected.load(Ordering::Relaxed),
            counts.ucna.load(Ordering::Relaxed),
            counts.deferred.load(Ordering::Relaxed),
            counts.left.load(Ordering::Relaxed),
            counts.polls.load(Ordering::Relaxed),
            counts.copies.load(Ordering::Relaxed),
        )?;
    }
    Ok(())
}
