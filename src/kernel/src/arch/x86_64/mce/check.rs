//! Stage 3's checks of the machine check's pure parts, on every x86-64 boot:
//! the classifier and its actions over a table of records
//! (`L.x86_64.224`, `225`), and the pending slots' protocol on a set of
//! slots of the check's own (`L.x86_64.226`); and AMD's rules
//! (`docs/MACHINE-CHECK-AMD.md`): the vendor table, AMD's classifier, its
//! actions, its usable address and the SMCA cut, `McaX` and the line's
//! fields (`L.x86_64.233`-`236`, `240`-`242`). Nothing here reads or writes
//! a machine-check register.

use super::decode::{self, ADDRV, AR, Action, Class, EIPV, EN, MISCV, OVER, PCC, RIPV, S, UC, VAL};
use super::{EMPTY, FULL, Kind, Record, SLOTS, Slot, WRITING, put, take};
use crate::console::println;

/// `MCi_MISC` naming a physical address recoverable to the byte.
const MISC_PHYSICAL: u64 = 2 << 6;
/// `MCi_MISC` naming a physical address recoverable to a page.
const MISC_PAGE: u64 = (2 << 6) | 12;
/// `MCi_MISC` naming a physical address recoverable only to 2 MiB.
const MISC_COARSE: u64 = (2 << 6) | 21;
/// `MCi_MISC` naming a linear address.
const MISC_LINEAR: u64 = 1 << 6;

/// The record SRAR makes: a consumption, with its address.
const SRAR: u64 = VAL | UC | EN | S | AR | ADDRV | MISCV;
/// The record SRAO makes: a patrol scrub's find.
const SRAO: u64 = VAL | UC | EN | S | ADDRV | MISCV;

/// Stage 3's checks.
///
/// # Errors
///
/// The first that failed.
pub(in crate::arch::x86_64) fn run() -> Result<(), &'static str> {
    classes()?;
    addresses()?;
    actions()?;
    slots()?;
    println!(
        "  mce      classifier, actions and pending slots as the table says; no register read"
    );
    vendors()?;
    amd_classes()?;
    amd_addresses()?;
    amd_actions()?;
    smca_values()?;
    println!(
        "  mce      vendor table, AMD's classifier, actions, address cut, McaX and line fields \
         as the table says; no register read"
    );
    Ok(())
}

/// Every class, and every bit that moves a record from one to another.
///
/// Verifies: `L.x86_64.224`
fn classes() -> Result<(), &'static str> {
    let table: &[(u64, bool, Class)] = &[
        (0, true, Class::Empty),
        (UC | PCC, true, Class::Empty),
        (VAL, true, Class::Corrected),
        (VAL | OVER | ADDRV, true, Class::Corrected),
        (VAL | UC | EN, true, Class::Ucna),
        (VAL | UC, true, Class::Ucna),
        (VAL | UC | AR | S, true, Class::Ucna),
        (SRAO, true, Class::Srao),
        (SRAR, true, Class::Srar),
        (SRAR | PCC, true, Class::Fatal),
        (SRAR | OVER, true, Class::Fatal),
        (SRAO | OVER, true, Class::Fatal),
        (VAL | UC | EN | AR, true, Class::Fatal),
        (SRAR, false, Class::Fatal),
        (SRAO, false, Class::Fatal),
        (VAL | UC | EN, false, Class::Fatal),
        (VAL | EN, false, Class::Corrected),
    ];
    for &(status, ser, class) in table {
        if decode::classify(status, ser) != class {
            return Err("the machine-check classifier put a record in the wrong class");
        }
    }
    Ok(())
}

/// An address is usable only with `ADDRV` and `MISCV`, and, with recovery
/// reported, physical and no coarser than a page.
///
/// Verifies: `L.x86_64.224`
fn addresses() -> Result<(), &'static str> {
    let table: &[(u64, u64, bool, bool)] = &[
        (SRAR, MISC_PAGE, true, true),
        (SRAR, MISC_PHYSICAL, true, true),
        (SRAR, MISC_COARSE, true, false),
        (SRAR, MISC_LINEAR, true, false),
        (SRAR & !ADDRV, MISC_PAGE, true, false),
        (SRAR & !MISCV, MISC_PAGE, true, false),
        (SRAR, MISC_LINEAR, false, true),
        (SRAR & !MISCV, 0, false, false),
    ];
    for &(status, misc, ser, usable) in table {
        if decode::usable_address(status, misc, ser) != usable {
            return Err("the machine check took an address it should not, or refused one");
        }
    }
    Ok(())
}

/// Each class's action from either ring under each `RIPV` and `EIPV`; the
/// fold; and the finish for a check with nothing to act on.
///
/// Verifies: `L.x86_64.225`
/// Verifies: `L.x86_64.229`
fn actions() -> Result<(), &'static str> {
    let both = RIPV | EIPV;
    let table: &[(Class, u64, u64, bool, Action)] = &[
        (Class::Corrected, SRAR, both, false, Action::Leave),
        (Class::Ucna, SRAR, 0, false, Action::Leave),
        (Class::Srao, SRAO, RIPV, false, Action::Defer),
        (Class::Srao, SRAO, RIPV, true, Action::Defer),
        (Class::Srao, SRAO, EIPV, true, Action::Stop),
        // A shared bank or a broadcast: another processor's consumption.
        (Class::Srar, SRAR, RIPV, false, Action::Unaffected),
        (Class::Srar, SRAR, RIPV, true, Action::Unaffected),
        (
            Class::Srar,
            SRAR,
            both,
            true,
            Action::Signal { kill: false },
        ),
        (Class::Srar, SRAR, EIPV, true, Action::Signal { kill: true }),
        // Consumed in the kernel.
        (Class::Srar, SRAR, both, false, Action::Stop),
        (Class::Srar, SRAR, EIPV, false, Action::Stop),
        (Class::Srar, SRAR, 0, true, Action::Stop),
        // Consumed in a program, with no page to name.
        (Class::Srar, SRAR & !ADDRV, both, true, Action::Stop),
        (Class::Fatal, SRAR | PCC, both, true, Action::Stop),
    ];
    for &(class, status, mcg_status, user, action) in table {
        if decode::action(class, status, MISC_PAGE, mcg_status, user, true) != action {
            return Err("the machine check chose the wrong action for a bank");
        }
    }
    let kill = Action::Signal { kill: false };
    let folds: &[(Action, Action, Action)] = &[
        (Action::Leave, Action::Defer, Action::Defer),
        (Action::Unaffected, Action::Leave, Action::Unaffected),
        (Action::Defer, kill, kill),
        (kill, Action::Stop, Action::Stop),
        (kill, kill, Action::Stop),
    ];
    for &(a, b, folded) in folds {
        if decode::fold(a, b) != folded || decode::fold(b, a) != folded {
            return Err("the machine check folded two banks' actions wrongly");
        }
    }
    let finishes: &[(Action, u64, Action)] = &[
        (Action::Leave, RIPV, Action::Leave),
        (Action::Leave, 0, Action::Stop),
        (Action::Unaffected, RIPV, Action::Unaffected),
        (Action::Unaffected, EIPV, Action::Stop),
        (Action::Defer, RIPV, Action::Defer),
    ];
    for &(folded, mcg_status, finished) in finishes {
        if decode::finish(folded, mcg_status) != finished {
            return Err("a machine check with nothing to act on returned where it should stop");
        }
    }
    Ok(())
}

/// The pending slots' protocol on slots of the check's own: they fill to
/// [`SLOTS`] and refuse the next; a slot a writer holds is passed by both
/// ways; records come out whole and the slot is free again.
///
/// Verifies: `L.x86_64.226`
fn slots() -> Result<(), &'static str> {
    let slots: [Slot; SLOTS] = [const { Slot::new() }; SLOTS];
    let record = |bank: u8| Record {
        kind: Kind::Deferred,
        bank,
        status: SRAO,
        address: 0x1000 * u64::from(bank),
        misc: MISC_PAGE,
    };
    if take(&slots).is_some() {
        return Err("a pending slot gave a record before any was put");
    }
    // A writer interrupted half way: the slot is neither written nor read.
    slots[0]
        .state
        .store(WRITING, core::sync::atomic::Ordering::Relaxed);
    for bank in 1..SLOTS as u8 {
        if !put(&slots, record(bank)) {
            return Err("a pending slot refused a record with room left");
        }
    }
    if put(&slots, record(99)) {
        return Err("the pending slots took a record past their room");
    }
    for bank in 1..SLOTS as u8 {
        if take(&slots) != Some(record(bank)) {
            return Err("a pending slot gave back a record other than the one put");
        }
    }
    if take(&slots).is_some() {
        return Err("a slot a writer holds was read");
    }
    slots[0]
        .state
        .store(EMPTY, core::sync::atomic::Ordering::Relaxed);
    let left = Record {
        kind: Kind::Left,
        ..record(3)
    };
    if !put(&slots, left)
        || slots[0].state.load(core::sync::atomic::Ordering::Relaxed) != FULL
        || take(&slots) != Some(left)
    {
        return Err("a freed pending slot did not carry a record again");
    }
    Ok(())
}

/// An AMD consumption: uncorrected, enabled, its address valid and poisoned.
const AMD_AR: u64 = VAL | UC | EN | ADDRV | MISCV | decode::POISON;

/// The vendor string decides whose rules, and `8000_0007h:EBX` AMD's bits.
///
/// Verifies: `L.x86_64.233`
fn vendors() -> Result<(), &'static str> {
    use decode::Vendor;
    let bits = decode::CPUID_SUCCOR | decode::CPUID_OVERFLOW_RECOV | decode::CPUID_SMCA;
    let amd = |ebx| decode::vendor_of(b"AuthenticAMD", ebx);
    let table: &[(&[u8; 12], u32, Vendor)] = &[
        (b"GenuineIntel", bits, Vendor::Intel),
        (b"  Shanghai  ", 0, Vendor::Intel),
        (
            b"AuthenticAMD",
            bits,
            Vendor::Amd {
                succor: true,
                overflow_recov: true,
                smca: true,
            },
        ),
        (
            b"HygonGenuine",
            decode::CPUID_SUCCOR,
            Vendor::Amd {
                succor: true,
                overflow_recov: false,
                smca: false,
            },
        ),
        (b"CentaurHauls", bits, Vendor::NoRecovery),
        (b"KVMKVMKVM\0\0\0", bits, Vendor::NoRecovery),
        (b"genuineintel", bits, Vendor::NoRecovery),
    ];
    for &(name, ebx, vendor) in table {
        if decode::vendor_of(name, ebx) != vendor {
            return Err("the machine check chose the wrong vendor's rules");
        }
    }
    if amd(0)
        != (Vendor::Amd {
            succor: false,
            overflow_recov: false,
            smca: false,
        })
    {
        return Err("an AMD processor without CPUID 8000_0007h bits was given them");
    }
    // A vendor without recovery reads every uncorrected record as fatal,
    // whatever MCG_SER_P says.
    if decode::classify_by(Vendor::NoRecovery, VAL | UC | EN | S | AR | ADDRV, true) != Class::Fatal
    {
        return Err("a vendor without recovery was given Intel's rules");
    }
    Ok(())
}

/// AMD's classes, as Linux 6.18's `mce_severity_amd`.
///
/// Verifies: `L.x86_64.234`
fn amd_classes() -> Result<(), &'static str> {
    let table: &[(u64, bool, bool, Class)] = &[
        (0, true, true, Class::Empty),
        (VAL, true, true, Class::Corrected),
        (VAL | decode::DEFERRED, true, true, Class::Deferred),
        (VAL | UC | decode::DEFERRED, false, false, Class::Deferred),
        (AMD_AR, true, true, Class::AmdAr),
        (AMD_AR | decode::TCC, true, true, Class::AmdAr),
        (AMD_AR | PCC, true, true, Class::Fatal),
        (VAL | decode::DEFERRED | PCC, true, true, Class::Fatal),
        (AMD_AR, false, true, Class::Fatal),
        (AMD_AR | OVER, true, false, Class::Fatal),
        (AMD_AR | OVER, true, true, Class::AmdAr),
        (VAL | OVER, false, false, Class::Corrected),
    ];
    for &(status, succor, overflow_recov, class) in table {
        if decode::classify_amd(status, succor, overflow_recov) != class {
            return Err("AMD's classifier put a record in the wrong class");
        }
    }
    Ok(())
}

/// An AMD address is usable with `ADDRV` first, then `PhysAddrV` where the
/// bank supports it, else `Poison`; a deferred frame only by `PhysAddrV`;
/// the SMCA cut to the lsb, refusing one above a page.
///
/// Verifies: `L.x86_64.235`
/// Verifies: `L.x86_64.242`
fn amd_addresses() -> Result<(), &'static str> {
    let read = decode::BANK_CONFIG_READ;
    let paddrv = read | decode::BANK_PADDRV;
    let usable: &[(u64, bool, decode::BankConfig, bool)] = &[
        (AMD_AR, false, 0, true),
        (AMD_AR & !ADDRV, false, 0, false),
        (AMD_AR & !decode::POISON, false, 0, false),
        (AMD_AR, true, read, true),
        (AMD_AR, true, paddrv, false),
        (AMD_AR | decode::PADDRV, true, paddrv, true),
        ((AMD_AR | decode::PADDRV) & !ADDRV, true, paddrv, false),
        // PADDRV is read only where the bank's CONFIG read succeeded.
        (AMD_AR, true, decode::BANK_PADDRV, true),
    ];
    for &(status, smca, config, want) in usable {
        if decode::usable_address_amd(status, smca, config) != want {
            return Err("AMD's usable address took one it should not, or refused one");
        }
    }
    let deferred = VAL | decode::DEFERRED | ADDRV | decode::POISON;
    let retire: &[(u64, bool, decode::BankConfig, bool)] = &[
        (deferred, false, 0, false),
        (deferred, true, read, false),
        (deferred | decode::PADDRV, true, paddrv, true),
        (deferred | decode::PADDRV, true, read, false),
        ((deferred | decode::PADDRV) & !ADDRV, true, paddrv, false),
    ];
    for &(status, smca, config, want) in retire {
        if decode::deferred_retirable(status, smca, config) != want {
            return Err("a deferred record's frame was retired without PhysAddrV, or not with it");
        }
    }
    let in_status = read | decode::BANK_LSB_IN_STATUS;
    let address = 0x0012_3456_7890_1abc_u64;
    let cuts: &[(u64, u64, bool, decode::BankConfig, Option<u64>)] = &[
        // Without SMCA, the frame containing the address.
        (address, 0, false, 0, Some(0x0012_3456_7890_1000)),
        // lsb 12 in ADDR 61:56: bits 55:12.
        (
            (12 << 56) | address,
            0,
            true,
            read,
            Some(0x0012_3456_7890_1000),
        ),
        // lsb 6: bits 55:6, the frame of it.
        (
            (6 << 56) | address,
            0,
            true,
            read,
            Some(0x0012_3456_7890_1000),
        ),
        // Bits above 55 dropped.
        (
            (12 << 56) | (1 << 55) | address,
            0,
            true,
            read,
            Some(0x0092_3456_7890_1000),
        ),
        // lsb 13: no one frame.
        ((13 << 56) | address, 0, true, read, None),
        ((21 << 56) | address, 0, true, read, None),
        // The lsb held in STATUS 29:24, bits 56:lsb.
        (
            (1 << 56) | address,
            12 << 24,
            true,
            in_status,
            Some(0x0112_3456_7890_1000),
        ),
        (address, 13 << 24, true, in_status, None),
    ];
    for &(raw, status, smca, config, want) in cuts {
        if decode::cut_address(raw, status, smca, config) != want {
            return Err("the SMCA address cut named the wrong frame");
        }
    }
    Ok(())
}

/// AMD's actions: never the bystander arm; ring 0 stops; `EIPV` clear or
/// no usable address stops; `RIPV` clear or `TCC` kills; otherwise signals.
///
/// Verifies: `L.x86_64.236`
/// Verifies: `L.x86_64.237`
/// Verifies: `L.x86_64.238`
fn amd_actions() -> Result<(), &'static str> {
    let both = RIPV | EIPV;
    let table: &[(Class, u64, u64, bool, bool, Action)] = &[
        (Class::Corrected, VAL, both, true, true, Action::Leave),
        (
            Class::Deferred,
            VAL | decode::DEFERRED,
            0,
            false,
            false,
            Action::Leave,
        ),
        (
            Class::AmdAr,
            AMD_AR,
            both,
            true,
            true,
            Action::Signal { kill: false },
        ),
        (
            Class::AmdAr,
            AMD_AR,
            EIPV,
            true,
            true,
            Action::Signal { kill: true },
        ),
        (
            Class::AmdAr,
            AMD_AR | decode::TCC,
            both,
            true,
            true,
            Action::Signal { kill: true },
        ),
        // The bystander's flags (RIPV set, EIPV clear) stop on AMD.
        (Class::AmdAr, AMD_AR, RIPV, true, true, Action::Stop),
        (Class::AmdAr, AMD_AR, both, true, false, Action::Stop),
        (Class::AmdAr, AMD_AR, both, false, true, Action::Stop),
        (
            Class::AmdAr,
            AMD_AR | decode::TCC,
            both,
            false,
            true,
            Action::Stop,
        ),
        (Class::Fatal, AMD_AR | PCC, both, true, true, Action::Stop),
    ];
    for &(class, status, mcg_status, user, usable, action) in table {
        let got = decode::amd_action(class, status, mcg_status, user, usable);
        if got == Action::Unaffected || got != action {
            return Err("AMD's machine check chose the wrong action for a bank");
        }
    }
    // Intel's action never acts on AMD's class.
    if decode::action(Class::AmdAr, AMD_AR, 0, RIPV, true, true) != Action::Stop {
        return Err("Intel's rules acted on AMD's action-required class");
    }
    Ok(())
}

/// `McaX` set and nothing else changed; the bits bring-up keeps of
/// `MCA_CONFIG`; the line's `ipid=` and `synd=`.
///
/// Verifies: `L.x86_64.240`
/// Verifies: `L.x86_64.241`
fn smca_values() -> Result<(), &'static str> {
    for raw in [0, 0x0000_0020_0000_0900, u64::MAX & !decode::CONFIG_MCAX] {
        let set = decode::with_mcax(raw);
        if set != raw | decode::CONFIG_MCAX {
            return Err("McaX was not set, or another MCA_CONFIG bit changed");
        }
    }
    if decode::bank_config(0x900)
        != decode::BANK_CONFIG_READ | decode::BANK_LSB_IN_STATUS | decode::BANK_PADDRV
        || decode::bank_config(0) != decode::BANK_CONFIG_READ
    {
        return Err("bring-up kept the wrong MCA_CONFIG bits");
    }
    let mut line = Fields::default();
    let fields = super::write_fields(
        &mut line,
        VAL | ADDRV,
        0x1000,
        7,
        Some(0xb0_0000_0002),
        Some(0x55),
    );
    if fields.is_err() || line.text() != " addr=0x1000 ipid=0xb000000002 synd=0x55" {
        return Err("an SMCA bank's line lost its ipid= or synd=, or misc= appeared without MISCV");
    }
    let mut line = Fields::default();
    let _ = super::write_fields(&mut line, VAL | MISCV, 0x1000, 7, None, None);
    if line.text() != " misc=0x7" {
        return Err("a line without SMCA registers carried ipid= or synd=");
    }
    Ok(())
}

/// A line's fields, written on the stack for [`smca_values`].
struct Fields {
    bytes: [u8; 96],
    len: usize,
}

impl Default for Fields {
    fn default() -> Self {
        Self {
            bytes: [0; 96],
            len: 0,
        }
    }
}

impl Fields {
    fn text(&self) -> &str {
        core::str::from_utf8(self.bytes.get(..self.len).unwrap_or_default()).unwrap_or("")
    }
}

impl core::fmt::Write for Fields {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let end = self.len + text.len();
        let slot = self.bytes.get_mut(self.len..end).ok_or(core::fmt::Error)?;
        slot.copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}
