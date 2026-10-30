//! What a machine-check bank's record is, and what the kernel does about it
//! (`docs/MACHINE-CHECK.md` §2): pure functions of the raw `MCi_STATUS` and
//! `MCG_STATUS`, checked over a table at boot (`super::check`). Bit names as
//! the SDM's vol. 3B ch. 16 and Linux's `asm/mce.h`.

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
}

impl Class {
    /// The word a log line gives the class.
    pub(in crate::arch::x86_64) const fn name(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Corrected => "corrected",
            Self::Ucna => "uncorrected-no-action",
            Self::Srao => "action-optional",
            Self::Srar => "action-required",
            Self::Fatal => "fatal",
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
        Class::Empty | Class::Corrected | Class::Ucna => Action::Leave,
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
