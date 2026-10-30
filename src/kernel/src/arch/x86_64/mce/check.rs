//! Stage 3's checks of the machine check's pure parts, on every x86-64 boot:
//! the classifier and its actions over a table of records
//! (`L.x86_64.224`, `225`), and the pending slots' protocol on a set of
//! slots of the check's own (`L.x86_64.226`). Nothing here reads or writes a
//! machine-check register.

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
