//! What a machine-check bank's record is, and what the kernel does about it
//! (`docs/MACHINE-CHECK.md` §2, and on AMD `docs/MACHINE-CHECK-AMD.md` §2):
//! pure functions of the raw `MCi_STATUS`, `MCG_STATUS` and, on AMD, the
//! `CPUID` bits and an SMCA bank's `MCA_CONFIG`, checked over tables at boot
//! (`super::check`). Bit names as the SDM's vol. 3B ch. 16 and Linux's
//! `asm/mce.h`.

/// `MCi_STATUS.VAL`: the bank holds a record.
pub(in crate::arch::x86_64) const VAL: u64 = 1 << 63;
/// `MCi_STATUS.OVER`: a record was lost behind this one.
pub(in crate::arch::x86_64) const OVER: u64 = 1 << 62;
/// `MCi_STATUS.UC`: the error was not corrected.
pub(in crate::arch::x86_64) const UC: u64 = 1 << 61;
/// `MCi_STATUS.EN`: the error was enabled for signalling by `MCi_CTL`.
pub(in crate::arch::x86_64) const EN: u64 = 1 << 60;
/// `MCi_STATUS.MISCV`: `MCi_MISC` holds something.
pub(in crate::arch::x86_64) const MISCV: u64 = 1 << 59;
/// `MCi_STATUS.ADDRV`: `MCi_ADDR` holds the address.
pub(in crate::arch::x86_64) const ADDRV: u64 = 1 << 58;
/// `MCi_STATUS.PCC`: the processor's context is corrupt.
pub(in crate::arch::x86_64) const PCC: u64 = 1 << 57;
/// `MCi_STATUS.S`: signalled by a machine check (with `MCG_SER_P`).
pub(in crate::arch::x86_64) const S: u64 = 1 << 56;
/// `MCi_STATUS.AR`: action required before going on (with `MCG_SER_P`).
pub(in crate::arch::x86_64) const AR: u64 = 1 << 55;

/// `MCG_STATUS.RIPV`: the interrupted program can restart where it was.
pub(in crate::arch::x86_64) const RIPV: u64 = 1 << 0;
/// `MCG_STATUS.EIPV`: the saved instruction pointer is the error's.
pub(in crate::arch::x86_64) const EIPV: u64 = 1 << 1;

/// `MCG_CAP.MCG_SER_P`: the processor reports `S` and `AR`.
pub(in crate::arch::x86_64) const SER_P: u64 = 1 << 24;

/// A bank's record, by what it asks of the kernel (§2's table).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::arch::x86_64) enum Class {
    /// No record: `VAL` clear.
    Empty,
    /// Corrected: logged and counted.
    Corrected,
    /// Uncorrected, no action required: logged, counted, the frame retired.
    Ucna,
    /// Software recoverable, action optional: the frame retired.
    Srao,
    /// Software recoverable, action required: the consumer is stopped.
    Srar,
    /// Anything the kernel cannot go on from.
    Fatal,
    /// AMD: uncorrected, nothing has consumed it yet (`Deferred`, bit 44).
    Deferred,
    /// AMD: uncorrected with `SUCCOR`, to be acted on now ([`amd_action`]).
    /// A class of its own so that Intel's bystander arm never sees it.
    AmdAr,
}

impl Class {
    /// The word a log line gives the class.
    pub(in crate::arch::x86_64) const fn name(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Corrected => "corrected",
            Self::Ucna => "uncorrected-no-action",
            Self::Srao => "action-optional",
            Self::Srar | Self::AmdAr => "action-required",
            Self::Fatal => "fatal",
            Self::Deferred => "deferred",
        }
    }
}

/// `MCG_CAP.MCG_CTL_P`: `IA32_MCG_CTL` is there.
pub(in crate::arch::x86_64) const CTL_P: u64 = 1 << 8;
/// `MCG_CAP`'s bank count, bits 7:0.
pub(in crate::arch::x86_64) const COUNT: u64 = 0xff;

/// `MCi_MISC`'s address mode, bits 8:6: 2 is a physical address.
const MISC_MODE_PHYSICAL: u64 = 2;
/// The page's shift: an address whose recoverable bits (`MCi_MISC` 5:0) are
/// wider than a page names no one page.
const PAGE_SHIFT: u64 = 12;

/// The class of a raw `MCi_STATUS`, where `ser` says whether `MCG_CAP`
/// offers software error recovery. Without it `S` and `AR` mean nothing,
/// and every uncorrected record is fatal. Fails closed (`L.x86_64.224`):
/// a lost record behind an uncorrected one, `AR` without `S`, and any
/// combination not named below are fatal.
pub(in crate::arch::x86_64) const fn classify(status: u64, ser: bool) -> Class {
    if status & VAL == 0 {
        return Class::Empty;
    }
    if status & UC == 0 {
        return Class::Corrected;
    }
    if status & PCC != 0 || status & OVER != 0 || !ser {
        return Class::Fatal;
    }
    match (status & S != 0, status & AR != 0, status & EN != 0) {
        (false, false, _) => Class::Ucna,
        // Logged by a bank whose signalling is off: no machine check came of
        // it, so nothing consumed it yet. Uncorrected data all the same.
        (true, _, false) => Class::Ucna,
        (true, false, true) => Class::Srao,
        (true, true, true) => Class::Srar,
        (false, true, _) => Class::Fatal,
    }
}

/// Whether a record's address names one physical page the kernel may retire
/// (`L.x86_64.224`): `ADDRV` and `MISCV`, and, where the processor reports
/// recovery, `MCi_MISC` saying the address is physical and no coarser than a
/// page. As Linux's `mce_usable_address`.
pub(in crate::arch::x86_64) const fn usable_address(status: u64, misc: u64, ser: bool) -> bool {
    if status & ADDRV == 0 || status & MISCV == 0 {
        return false;
    }
    !ser || ((misc >> 6) & 0x7 == MISC_MODE_PHYSICAL && misc & 0x3f <= PAGE_SHIFT)
}

/// What one valid bank asks of a machine check on this processor.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::arch::x86_64) enum Action {
    /// Nothing now: a corrected or no-action record, left for the poll.
    Leave,
    /// Another processor's consumption, seen in a shared bank or by the
    /// broadcast (`EIPV` clear, `RIPV` set): neither cleared nor retired
    /// here; the consumer clears it (`L.x86_64.225`).
    Unaffected,
    /// An action-optional record: handed to the poll, cleared, and the
    /// interrupted code goes on (`L.x86_64.226`).
    Defer,
    /// This processor's ring-3 program consumed it: `SIGBUS`, a kill when
    /// the instruction cannot be restarted (`L.x86_64.228`).
    Signal {
        /// `MCG_STATUS.RIPV` clear.
        kill: bool,
    },
    /// The kernel cannot go on (`L.x86_64.227`, `229`).
    Stop,
}

/// The action for one bank of `class`, its `status` and `misc`, under
/// `mcg_status`, taken from ring 3 when `user`.
pub(in crate::arch::x86_64) const fn action(
    class: Class,
    status: u64,
    misc: u64,
    mcg_status: u64,
    user: bool,
    ser: bool,
) -> Action {
    let ripv = mcg_status & RIPV != 0;
    let eipv = mcg_status & EIPV != 0;
    match class {
        Class::Empty | Class::Corrected | Class::Ucna | Class::Deferred => Action::Leave,
        // AMD's class is [`amd_action`]'s: reaching here is a vendor mix-up,
        // and the kernel stops rather than guess.
        Class::AmdAr => Action::Stop,
        Class::Srao if ripv => Action::Defer,
        Class::Srar if ripv && !eipv => Action::Unaffected,
        Class::Srar if user && eipv && usable_address(status, misc, ser) => {
            Action::Signal { kill: !ripv }
        }
        Class::Srao | Class::Srar | Class::Fatal => Action::Stop,
    }
}

/// Two banks' actions folded: the graver wins, and two consumptions in one
/// machine check, which the kernel cannot tell apart, stop it.
pub(in crate::arch::x86_64) const fn fold(a: Action, b: Action) -> Action {
    match (a, b) {
        (Action::Signal { .. }, Action::Signal { .. }) => Action::Stop,
        _ if rank(a) >= rank(b) => a,
        _ => b,
    }
}

/// The machine check's verdict once every bank is folded into `folded`
/// (`Leave` for none): with nothing to act on, it returns if the interrupted
/// code can restart (a broadcast this processor did not cause, or a spurious
/// check), and stops if not (`L.x86_64.225`).
pub(in crate::arch::x86_64) const fn finish(folded: Action, mcg_status: u64) -> Action {
    match folded {
        Action::Leave | Action::Unaffected if mcg_status & RIPV == 0 => Action::Stop,
        other => other,
    }
}

/// Severity order of the actions.
const fn rank(action: Action) -> u8 {
    match action {
        Action::Leave => 0,
        Action::Unaffected => 1,
        Action::Defer => 2,
        Action::Signal { .. } => 3,
        Action::Stop => 4,
    }
}

// ---------------------------------------------------------------------------
// The vendor, and AMD's rules (`docs/MACHINE-CHECK-AMD.md` §2)
// ---------------------------------------------------------------------------

/// AMD `MCA_STATUS.TCC`: the interrupted task's context is corrupt. The bit
/// Intel calls `AR`.
pub(in crate::arch::x86_64) const TCC: u64 = 1 << 55;
/// AMD SMCA `MCA_STATUS.PhysAddrV`: `MCA_ADDR` holds a system physical
/// address, where the bank's `MCA_CONFIG` bit 11 says the bit is real
/// (Linux 821f5fe4dbcb).
pub(in crate::arch::x86_64) const PADDRV: u64 = 1 << 54;
/// AMD SMCA `MCA_STATUS.SyndV`: `MCA_SYND` holds a syndrome.
pub(in crate::arch::x86_64) const SYNDV: u64 = 1 << 53;
/// AMD `MCA_STATUS.Deferred`.
pub(in crate::arch::x86_64) const DEFERRED: u64 = 1 << 44;
/// AMD `MCA_STATUS.Poison`: the consumed data was poisoned.
pub(in crate::arch::x86_64) const POISON: u64 = 1 << 43;

/// SMCA `MCA_CONFIG` bit 8: the address's lsb is in `MCA_STATUS` 29:24.
pub(in crate::arch::x86_64) const CONFIG_LSB_IN_STATUS: u64 = 1 << 8;
/// SMCA `MCA_CONFIG` bit 11: `MCA_STATUS.PhysAddrV` is supported.
pub(in crate::arch::x86_64) const CONFIG_PADDRV: u64 = 1 << 11;
/// SMCA `MCA_CONFIG.McaX`, bit 32: the OS uses the SMCA registers; without
/// it uncorrectable errors are fatal (Linux 6.18's `smca_configure`).
pub(in crate::arch::x86_64) const CONFIG_MCAX: u64 = 1 << 32;

/// `CPUID 8000_0007h:EBX` bit 0: `OverflowRecov`.
pub(in crate::arch::x86_64) const CPUID_OVERFLOW_RECOV: u32 = 1 << 0;
/// `CPUID 8000_0007h:EBX` bit 1: `SUCCOR`.
pub(in crate::arch::x86_64) const CPUID_SUCCOR: u32 = 1 << 1;
/// `CPUID 8000_0007h:EBX` bit 3: scalable MCA.
pub(in crate::arch::x86_64) const CPUID_SMCA: u32 = 1 << 3;

/// Whose rules a processor's records are read by (`L.x86_64.233`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::arch::x86_64) enum Vendor {
    /// Intel's, and Zhaoxin's: `MCG_SER_P`, `S` and `AR`.
    Intel,
    /// AMD's, and Hygon's, with the `CPUID 8000_0007h` bits.
    Amd {
        /// `SUCCOR`: an uncorrected record may be recovered from.
        succor: bool,
        /// `OverflowRecov`: an overflowed uncorrected record too.
        overflow_recov: bool,
        /// Scalable MCA: the banks' `MCA_CONFIG`, `IPID` and `SYND`.
        smca: bool,
    },
    /// Anyone else: corrected records logged, every uncorrected one fatal.
    NoRecovery,
}

impl Vendor {
    /// The word the boot line gives the vendor.
    pub(in crate::arch::x86_64) const fn name(self) -> &'static str {
        match self {
            Self::Intel => "intel",
            Self::Amd { .. } => "amd",
            Self::NoRecovery => "other, no recovery",
        }
    }
}

/// The vendor for a `CPUID 0` vendor string and `8000_0007h:EBX`
/// (`L.x86_64.233`, the consultant's ruling (a), ledger 4820): AMD's rules
/// for `AuthenticAMD` and `HygonGenuine`, Intel's for `GenuineIntel` and
/// Zhaoxin's `"  Shanghai  "`, no recovery for every other string.
pub(in crate::arch::x86_64) fn vendor_of(name: &[u8; 12], ebx: u32) -> Vendor {
    match name {
        b"GenuineIntel" | b"  Shanghai  " => Vendor::Intel,
        b"AuthenticAMD" | b"HygonGenuine" => Vendor::Amd {
            succor: ebx & CPUID_SUCCOR != 0,
            overflow_recov: ebx & CPUID_OVERFLOW_RECOV != 0,
            smca: ebx & CPUID_SMCA != 0,
        },
        _ => Vendor::NoRecovery,
    }
}

/// The class of a raw `MCi_STATUS` under `vendor`, with `ser` the
/// `MCG_SER_P` Intel's rules read: Intel's [`classify`], AMD's
/// [`classify_amd`], or every uncorrected record fatal.
pub(in crate::arch::x86_64) const fn classify_by(vendor: Vendor, status: u64, ser: bool) -> Class {
    match vendor {
        Vendor::Intel => classify(status, ser),
        Vendor::Amd {
            succor,
            overflow_recov,
            ..
        } => classify_amd(status, succor, overflow_recov),
        Vendor::NoRecovery => classify(status, false),
    }
}

/// The class of an AMD record (`L.x86_64.234`), as Linux 6.18's
/// `mce_severity_amd`: `PCC` fatal, `Deferred` deferred, corrected, an
/// overflowed uncorrected record fatal without `OverflowRecov`, every
/// uncorrected record fatal without `SUCCOR`, the rest action required.
pub(in crate::arch::x86_64) const fn classify_amd(
    status: u64,
    succor: bool,
    overflow_recov: bool,
) -> Class {
    if status & VAL == 0 {
        Class::Empty
    } else if status & PCC != 0 {
        Class::Fatal
    } else if status & DEFERRED != 0 {
        Class::Deferred
    } else if status & UC == 0 {
        Class::Corrected
    } else if (status & OVER != 0 && !overflow_recov) || !succor {
        Class::Fatal
    } else {
        Class::AmdAr
    }
}

/// An SMCA bank's `MCA_CONFIG` as bring-up kept it: [`BANK_CONFIG_READ`]
/// and the two bits read from it.
pub(in crate::arch::x86_64) type BankConfig = u8;
/// [`BankConfig`]: bring-up's checked reads of `MCA_CONFIG` and `MCA_IPID`
/// and its `McaX` write all succeeded.
pub(in crate::arch::x86_64) const BANK_CONFIG_READ: BankConfig = 1 << 0;
/// [`BankConfig`]: `MCA_CONFIG` bit 8.
pub(in crate::arch::x86_64) const BANK_LSB_IN_STATUS: BankConfig = 1 << 1;
/// [`BankConfig`]: `MCA_CONFIG` bit 11.
pub(in crate::arch::x86_64) const BANK_PADDRV: BankConfig = 1 << 2;

/// What bring-up keeps of a raw `MCA_CONFIG`.
pub(in crate::arch::x86_64) const fn bank_config(raw: u64) -> BankConfig {
    BANK_CONFIG_READ
        | if raw & CONFIG_LSB_IN_STATUS != 0 {
            BANK_LSB_IN_STATUS
        } else {
            0
        }
        | if raw & CONFIG_PADDRV != 0 {
            BANK_PADDRV
        } else {
            0
        }
}

/// `MCA_CONFIG` with `McaX` set and nothing else changed (`L.x86_64.240`).
pub(in crate::arch::x86_64) const fn with_mcax(raw: u64) -> u64 {
    raw | CONFIG_MCAX
}

/// Whether `PhysAddrV` is to be read for this bank: SMCA, its `CONFIG` read
/// at bring-up, and bit 11 set there.
const fn paddrv_supported(smca: bool, config: BankConfig) -> bool {
    smca && config & BANK_CONFIG_READ != 0 && config & BANK_PADDRV != 0
}

/// Whether an AMD record's address names a frame (`L.x86_64.235`): `ADDRV`
/// first, always; on SMCA, only on a bank whose bring-up reads and `McaX`
/// write succeeded, since its cut and `PhysAddrV`'s meaning are otherwise
/// unknown (the consultant's B2, ledger 4839); then `PhysAddrV` where the
/// bank supports it (Linux 821f5fe4dbcb), else `Poison` (Linux 6.18's
/// `amd_mce_usable_address`).
pub(in crate::arch::x86_64) const fn usable_address_amd(
    status: u64,
    smca: bool,
    config: BankConfig,
) -> bool {
    if status & ADDRV == 0 || (smca && config & BANK_CONFIG_READ == 0) {
        return false;
    }
    if paddrv_supported(smca, config) {
        status & PADDRV != 0
    } else {
        status & POISON != 0
    }
}

/// Whether a deferred record's frame may be retired (`L.x86_64.242`): only
/// where `PhysAddrV` makes its address physical. `Poison` does not cover a
/// deferred record (Linux 6.18's comment), and a part without SMCA never.
pub(in crate::arch::x86_64) const fn deferred_retirable(
    status: u64,
    smca: bool,
    config: BankConfig,
) -> bool {
    status & ADDRV != 0 && paddrv_supported(smca, config) && status & PADDRV != 0
}

/// The page an AMD record's `MCA_ADDR` names, cut to its valid bits as
/// Linux 6.18's `smca_extract_err_addr` (`L.x86_64.235`): on SMCA, lsb from
/// `MCA_STATUS` 29:24 and bits 56:lsb where `CONFIG` bit 8 says so, else lsb
/// from `MCA_ADDR` 61:56 and bits 55:lsb; without SMCA, the address as it
/// is. `None` where the lsb is above a page: no one frame.
pub(in crate::arch::x86_64) const fn cut_address(
    address: u64,
    status: u64,
    smca: bool,
    config: BankConfig,
) -> Option<u64> {
    if !smca {
        return Some(address & !((1 << PAGE_SHIFT) - 1));
    }
    // A bank whose configuration bring-up could not read has no known cut.
    if config & BANK_CONFIG_READ == 0 {
        return None;
    }
    let (lsb, top) = if config & BANK_LSB_IN_STATUS != 0 {
        ((status >> 24) & 0x3f, 56)
    } else {
        ((address >> 56) & 0x3f, 55)
    };
    if lsb > PAGE_SHIFT {
        return None;
    }
    let mask = (u64::MAX >> (63 - top)) & !((1 << lsb) - 1);
    Some(address & mask & !((1 << PAGE_SHIFT) - 1))
}

/// The action for one AMD bank of `class` (`L.x86_64.236`-`238`): corrected
/// and deferred left for the poll; an action-required record stops in ring
/// 0, and in ring 3 stops with `EIPV` clear or no usable address, kills on
/// `RIPV` clear or `TCC` (the consultant's ruling (b)), and otherwise
/// signals. Never `Unaffected`: AMD's machine check is not broadcast.
pub(in crate::arch::x86_64) const fn amd_action(
    class: Class,
    status: u64,
    mcg_status: u64,
    user: bool,
    usable: bool,
) -> Action {
    let ripv = mcg_status & RIPV != 0;
    let eipv = mcg_status & EIPV != 0;
    match class {
        Class::Empty | Class::Corrected | Class::Deferred => Action::Leave,
        Class::AmdAr if !user || !eipv || !usable => Action::Stop,
        Class::AmdAr => Action::Signal {
            kill: !ripv || status & TCC != 0,
        },
        _ => Action::Stop,
    }
}
