//! Stage 3's checks of the power core, every x86-64 boot, none of which
//! writes a register the processor has: the pure decisions on fixed values,
//! the claim's state driven over a fake register file through every way a
//! claim ends, and every interleaving of a write with a restore.

use alloc::sync::Weak;
use core::sync::atomic::Ordering;

use super::fake_check::Fake;
use super::logic::{self, Trip};
use super::{Cells, Refused, Restore, Restorer, Said, State, Wrote, write};

/// A second, in nanoseconds.
const SECOND: u64 = 1_000_000_000;

/// Stage 3's checks.
///
/// # Errors
///
/// The first that failed.
pub(in crate::arch::x86_64) fn run() -> Result<(), &'static str> {
    decided()?;
    guarded()?;
    claimed()?;
    leased()?;
    backstopped()?;
    latched()?;
    compared()?;
    rated()?;
    verified()?;
    exits()?;
    lifecycle()?;
    restores_interleaved()?;
    measured()?;
    interleaved()
}

/// The row.
fn row() -> &'static logic::Row {
    &logic::TABLE[0]
}

/// A claim on `fake`, `state` as earlier claims left it.
fn claim(state: &mut State, fake: &Fake, now: u64) -> Result<(), &'static str> {
    state
        .claim(fake, Weak::new(), None, now)
        .map(|_| ())
        .map_err(|_| "a power claim on the fake was refused")
}

/// Set `amps` A at `now`.
fn set(state: &mut State, fake: &Fake, amps: u64, now: u64) -> Result<u64, Refused> {
    state.set(fake, row(), now, amps * 8, &mut Said::default())
}

/// The fake's field, in eighths.
fn vr(fake: &Fake) -> u64 {
    fake.peek() & logic::VR_FIELD
}

/// The gate, the packages, the bounds and the composition.
///
/// Verifies: `L.x86_64.194`
/// Verifies: `L.x86_64.195`
/// Verifies: `L.x86_64.196`
fn decided() -> Result<(), &'static str> {
    let n150 = 0x000b_06e0;
    if logic::row(true, n150).is_none()
        || logic::row(false, n150).is_some()
        || logic::row(true, 0x000b_06f0).is_some()
        || logic::row(true, 0x000a_06e0).is_some()
        || logic::row(true, 0x000b_0fe0).is_some()
        || logic::row(false, 0x0060_0fb1).is_some()
    {
        return Err("the power model gate let another processor through");
    }
    let one = [0u64, 1, 2, 3];
    if logic::packages(4, one.iter().copied()) != 1
        || logic::packages(4, [0u64, 0x10].iter().copied()) != 2
        || logic::packages(4, [].iter().copied()) != 0
    {
        return Err("the power package count was wrong");
    }
    let refusal = |row, packages, vr, tjmax| logic::refusal(row, packages, vr, tjmax);
    if refusal(true, Some(1), Some(0xc8), Some(105)).is_some()
        || refusal(false, Some(1), Some(0xc8), Some(105)).is_none()
        || refusal(true, Some(2), Some(0xc8), Some(105)).is_none()
        || refusal(true, None, Some(0xc8), Some(105)).is_none()
        || refusal(true, Some(1), None, Some(105)).is_none()
        || refusal(true, Some(1), Some(0x8000_00c8), Some(105)).is_none()
        || refusal(true, Some(1), Some(0), Some(105)).is_none()
        || refusal(true, Some(1), Some(0xc8), Some(79)).is_none()
        || refusal(true, Some(1), Some(0xc8), None).is_none()
    {
        return Err("the power probe published where it should have refused");
    }
    if !logic::admitted(row(), 0xc8, 200)
        || !logic::admitted(row(), 0xc8, 384)
        || logic::admitted(row(), 0xc8, 385)
        || logic::admitted(row(), 0xc8, 199)
        || logic::highest(row(), 250) != 384
        || logic::highest(row(), 100) != 284
    {
        return Err("a power request outside the row was admitted");
    }
    if logic::compose(0xffff_ffff_7fff_e0c8, 216) != 0xffff_ffff_7fff_e0d8
        || logic::compose(0, 0x2000) != 0
    {
        return Err("a power value changed bits other than 12:0");
    }
    Ok(())
}

/// A thermal status word: `below` degrees under `TjMax`, valid, `bits`.
const fn status(below: u64, bits: u64) -> u64 {
    (1 << 31) | (below << 16) | bits
}

/// The backstop's verdict: 89 C passes, 90 C trips on either sensor, each
/// of bits 0 to 5 trips on either, 6 to 15 do not, an invalid readout
/// trips.
///
/// Verifies: `L.x86_64.200`
fn guarded() -> Result<(), &'static str> {
    if logic::verdict(100, 90, status(11, 0), status(11, 0)).is_err()
        || logic::verdict(100, 90, status(10, 0), status(11, 0)) != Err(Trip::Hot(90))
        || logic::verdict(100, 90, status(11, 0), status(10, 0)) != Err(Trip::Hot(90))
    {
        return Err("the power backstop was wrong about 89 or 90 C");
    }
    for bit in 0..6 {
        if logic::verdict(100, 90, status(65, 1 << bit), status(65, 0)) != Err(Trip::CoreBit(bit))
            || logic::verdict(100, 90, status(65, 0), status(65, 1 << bit))
                != Err(Trip::PackageBit(bit))
        {
            return Err("the power backstop did not trip on a thermal or PROCHOT bit");
        }
    }
    if logic::verdict(100, 90, status(65, 0xffc0), status(65, 0xffc0)).is_err()
        || logic::verdict(100, 90, status(65, 0) & !(1 << 31), status(65, 0)) != Err(Trip::Invalid)
    {
        return Err("the power backstop judged an unguarded bit or an invalid readout wrongly");
    }
    Ok(())
}

/// One claim at a time; a raise, firmware's value set again, the holder
/// gone.
///
/// Verifies: `L.x86_64.198`
fn claimed() -> Result<(), &'static str> {
    let fake = Fake::new();
    let mut state = State::new();
    claim(&mut state, &fake, 0)?;
    if state.claim(&fake, Weak::new(), None, 0) != Err(Refused::NotHeld) {
        return Err("a second power claim was made while one stood");
    }
    if set(&mut state, &fake, 27, 0) != Ok(216) || vr(&fake) != 216 || !fake.in_force() {
        return Err("a power raise on the fake was not written");
    }
    if set(&mut state, &fake, 25, 1) != Ok(200) || vr(&fake) != 200 || !state.held() {
        return Err("a power set of firmware's value did not write it");
    }
    let _ = set(&mut state, &fake, 27, 2 * SECOND);
    if !state.tick(&fake, row(), 2 * SECOND, true, &mut Said::default()) || vr(&fake) != 200 {
        return Err("a power holder's death did not restore firmware's value");
    }
    if Restorer::new(false).run(&fake) != Restore::Idle || state.latched {
        return Err("a power restore was not single-shot, or a death latched");
    }
    Ok(())
}

/// No call for five seconds ends the claim and restores; a call renews.
///
/// Verifies: `L.x86_64.199`
fn leased() -> Result<(), &'static str> {
    let fake = Fake::new();
    let mut state = State::new();
    claim(&mut state, &fake, 0)?;
    let _ = set(&mut state, &fake, 28, 0);
    state.renew(4 * SECOND);
    if state.tick(&fake, row(), 8 * SECOND, false, &mut Said::default()) || vr(&fake) != 224 {
        return Err("a renewed power lease lapsed");
    }
    if !state.tick(&fake, row(), 9 * SECOND, false, &mut Said::default()) || vr(&fake) != 200 {
        return Err("a lapsed power lease did not restore firmware's value");
    }
    if state.latched || claim(&mut state, &fake, 10 * SECOND).is_err() {
        return Err("a lapsed power lease latched the core");
    }
    Ok(())
}

/// The backstop restores; a raise is refused while it reads hot, a
/// lowering is not; a backstop that cannot read restores.
///
/// Verifies: `L.x86_64.200`
fn backstopped() -> Result<(), &'static str> {
    let fake = Fake::new();
    let mut state = State::new();
    claim(&mut state, &fake, 0)?;
    let _ = set(&mut state, &fake, 30, 0);
    fake.heat(16, 15, 0);
    if !state.tick(&fake, row(), SECOND, false, &mut Said::default()) || vr(&fake) != 200 {
        return Err("the power backstop did not restore at 90 C");
    }
    let hot = Fake::new();
    let mut cautious = State::new();
    claim(&mut cautious, &hot, 0)?;
    let _ = set(&mut cautious, &hot, 30, 0);
    hot.heat(65, 65, 1 << 2);
    if set(&mut cautious, &hot, 31, 2 * SECOND) != Err(Refused::Hot)
        || set(&mut cautious, &hot, 27, 2 * SECOND).is_err()
    {
        return Err("a power raise was written while the backstop read hot, or a lowering refused");
    }
    let blind = Fake::new();
    let mut unseeing = State::new();
    claim(&mut unseeing, &blind, 0)?;
    let _ = set(&mut unseeing, &blind, 27, 0);
    blind.blind.store(true, Ordering::SeqCst);
    if !unseeing.tick(&blind, row(), SECOND, false, &mut Said::default()) || vr(&blind) != 200 {
        return Err("a power backstop that could not read did not restore");
    }
    Ok(())
}

/// A backstop trip, a restore not confirmed and eight ended claims latch
/// the core until reboot; a latched core refuses claims.
///
/// Verifies: `L.x86_64.201`
fn latched() -> Result<(), &'static str> {
    let fake = Fake::new();
    let mut state = State::new();
    claim(&mut state, &fake, 0)?;
    let _ = set(&mut state, &fake, 30, 0);
    fake.heat(10, 60, 0);
    let _ = state.tick(&fake, row(), SECOND, false, &mut Said::default());
    if !state.latched || state.claim(&fake, Weak::new(), None, 2 * SECOND) != Err(Refused::Latched)
    {
        return Err("a power backstop trip did not latch the core");
    }
    let shy = Fake::new();
    let mut unconfirmed = State::new();
    claim(&mut unconfirmed, &shy, 0)?;
    let _ = set(&mut unconfirmed, &shy, 30, 0);
    shy.faults.store(1, Ordering::SeqCst);
    let _ = unconfirmed.tick(&shy, row(), SECOND, true, &mut Said::default());
    if !unconfirmed.latched {
        return Err("a power restore not confirmed did not latch the core");
    }
    let tired = Fake::new();
    let mut many = State::new();
    for at in 0..u64::from(super::MAX_ENDED) {
        claim(&mut many, &tired, at)?;
        let _ = many.tick(&tired, row(), at, true, &mut Said::default());
    }
    if !many.latched {
        return Err("eight ended power claims did not latch the core");
    }
    Ok(())
}

/// A register firmware changed -- before the claim, before a write, while
/// a raise was in force -- is written no more; a restore writes firmware's
/// field over the register as it reads.
///
/// Verifies: `L.x86_64.202`
fn compared() -> Result<(), &'static str> {
    let fake = Fake::new();
    let mut state = State::new();
    claim(&mut state, &fake, 0)?;
    fake.poke(0xc0);
    if set(&mut state, &fake, 27, 0) != Err(Refused::Latched)
        || fake.peek() != 0xc0
        || !state.forgotten
    {
        return Err("a power write went over a register firmware changed");
    }
    let watched = Fake::new();
    let mut state = State::new();
    claim(&mut state, &watched, 0)?;
    let _ = set(&mut state, &watched, 30, 0);
    watched.poke(0xd0);
    let _ = state.tick(&watched, row(), SECOND, true, &mut Said::default());
    if watched.peek() != 0xd0 {
        return Err("a power restore wrote over a register firmware changed");
    }
    let before = Fake::new();
    before.poke(0xd8);
    let mut late = State::new();
    claim(&mut late, &before, 0)?;
    if set(&mut late, &before, 30, 0) != Err(Refused::Latched) {
        return Err("a power register firmware changed before the claim was written");
    }
    let outside = Fake::new();
    let mut state = State::new();
    claim(&mut state, &outside, 0)?;
    let _ = set(&mut state, &outside, 30, 0);
    let flipped = outside.peek() | 1 << 20;
    outside.poke(flipped);
    outside.set_last(flipped);
    let _ = state.tick(&outside, row(), SECOND, true, &mut Said::default());
    if outside.peek() != (1 << 20 | 0xc8) {
        return Err("a power restore wrote firmware's whole word, not its field");
    }
    Ok(())
}

/// A raise at most once a second, whichever claim raised; a lowering at
/// once.
///
/// Verifies: `L.x86_64.203`
fn rated() -> Result<(), &'static str> {
    let fake = Fake::new();
    let mut state = State::new();
    claim(&mut state, &fake, 0)?;
    let _ = set(&mut state, &fake, 27, 0);
    if set(&mut state, &fake, 28, SECOND / 2) != Err(Refused::Rate)
        || set(&mut state, &fake, 26, SECOND / 2).is_err()
        || set(&mut state, &fake, 28, SECOND).is_err()
    {
        return Err("the power raise floor was not held, or a lowering waited");
    }
    let _ = state.tick(&fake, row(), SECOND, true, &mut Said::default());
    claim(&mut state, &fake, SECOND + 1)?;
    if set(&mut state, &fake, 27, SECOND + 2) != Err(Refused::Rate) {
        return Err("a new power claim reset the raise floor");
    }
    Ok(())
}

/// A write that faults or reads back otherwise ends the claim, restores
/// and latches.
///
/// Verifies: `L.x86_64.205`
fn verified() -> Result<(), &'static str> {
    let skewed = Fake::new();
    let mut state = State::new();
    claim(&mut state, &skewed, 0)?;
    skewed.skew.store(1, Ordering::SeqCst);
    if set(&mut state, &skewed, 27, 0) != Err(Refused::Latched)
        || vr(&skewed) != 200
        || !state.latched
        || state.held()
    {
        return Err("a power write that read back otherwise did not end, restore and latch");
    }
    let faulting = Fake::new();
    let mut state = State::new();
    claim(&mut state, &faulting, 0)?;
    faulting.faults.store(1, Ordering::SeqCst);
    if set(&mut state, &faulting, 27, 0) != Err(Refused::Latched)
        || vr(&faulting) != 200
        || !state.latched
    {
        return Err("a power write that faulted did not end, restore and latch");
    }
    Ok(())
}

/// An exit restore puts firmware's field back and stops every later write.
///
/// Verifies: `L.x86_64.206`
fn exits() -> Result<(), &'static str> {
    let fake = Fake::new();
    let mut state = State::new();
    claim(&mut state, &fake, 0)?;
    let _ = set(&mut state, &fake, 30, 0);
    if !Restorer::new(true).run(&fake).confirmed() || vr(&fake) != 200 {
        return Err("a power exit restore did not put firmware's value back");
    }
    let writes = fake.writes.load(Ordering::SeqCst);
    if write(&fake, 216) != Wrote::Stopped || fake.writes.load(Ordering::SeqCst) != writes {
        return Err("a power write ran after an exit restore");
    }
    Ok(())
}

/// `power_read`'s words: firmware's value, the ceiling, the register and
/// every reading present; one that faulted marked absent, not 0.
///
/// Verifies: `L.x86_64.197`
fn measured() -> Result<(), &'static str> {
    use ferrix_native_abi::types as abi;
    let fake = Fake::new();
    let state = State::new();
    let machine = super::Machine {
        row: row(),
        signature: super::fake_check::SIGNATURE,
        fake: true,
    };
    let words = super::readings(&fake, &state, machine);
    let all = (1 << abi::POWER_READ_VR) | (((1 << 9) - 1) << abi::POWER_READ_FIRST);
    if words[abi::POWER_READ_PRESENT] != all
        || words[abi::POWER_READ_FIRMWARE] != 0xc8
        || words[abi::POWER_READ_CEILING] != 384
        || words[abi::POWER_READ_VR] != 0xc8
        || words[abi::POWER_READ_FIRST] != 105 << 16
    {
        return Err("power_read's words were not the readings");
    }
    fake.blind.store(true, Ordering::SeqCst);
    let blind = super::readings(&fake, &state, machine);
    let thermal = 0b110 << abi::POWER_READ_FIRST;
    if blind[abi::POWER_READ_PRESENT] & thermal != 0 {
        return Err("power_read marked a reading that faulted as present");
    }
    Ok(())
}

/// The most accesses a write makes, and steps a restore takes.
const WRITE_ACCESSES: u32 = 13;
const RESTORE_STEPS: u32 = 6;

/// Every interleaving of a raise's accesses with an exit restore's steps,
/// from firmware's value and from a raise in force: none leaves anything
/// but firmware's field, nothing in force. Each interleaving is the set of
/// slots, among the two counts' sum, that are the restore's, walked in
/// order (Gosper's successor).
///
/// Verifies: `L.x86_64.204`
fn interleaved() -> Result<(), &'static str> {
    let total = WRITE_ACCESSES + RESTORE_STEPS;
    for raised in [false, true] {
        let mut positions: u32 = (1 << RESTORE_STEPS) - 1;
        while positions < 1 << total {
            let fake = Fake::new();
            if raised {
                let _ = write(&fake, 0xd8);
            }
            fake.interleave(Restorer::new(true), positions);
            // Masked, as `State::set` runs it.
            let _ = fake.masked(|| write(&fake, 0x100));
            let _ = fake.finish();
            if vr(&fake) != 0xc8 || fake.in_force() {
                return Err("an interleaving of a power write and a restore left a raised value");
            }
            let lowest = positions & positions.wrapping_neg();
            let ripple = positions + lowest;
            positions = ripple | (((positions ^ ripple) >> 2) / lowest);
        }
    }
    Ok(())
}

/// The most steps a task restore takes inside [`State::end`].
const TASK_RESTORE_STEPS: u32 = 6;

/// Every interleaving of a task restore -- the holder gone, ending the
/// claim -- with an exit restore's steps, from a raise in force: the exit
/// restore's stop halts a processor that is not masked, and none leaves a
/// raised value with nothing in force (ledger 3174, B2); the lease, backstop and Verify endings reach the same restore.
///
/// Verifies: `L.x86_64.208`
fn restores_interleaved() -> Result<(), &'static str> {
    let total = TASK_RESTORE_STEPS + RESTORE_STEPS;
    let mut positions: u32 = (1 << RESTORE_STEPS) - 1;
    while positions < 1 << total {
        let fake = Fake::new();
        let mut state = State::new();
        claim(&mut state, &fake, 0)?;
        let _ = set(&mut state, &fake, 30, 0);
        fake.interleave(Restorer::new(true), positions);
        state.end(&fake, super::Cause::Gone, &mut Said::default());
        let _ = fake.finish();
        if vr(&fake) != 0xc8 || fake.in_force() {
            return Err("an interleaving of two power restores left a raised value");
        }
        let lowest = positions & positions.wrapping_neg();
        let ripple = positions + lowest;
        positions = ripple | (((positions ^ ripple) >> 2) / lowest);
    }
    Ok(())
}

/// A claim's kernel end is the claim's own: ended, it is handed to the
/// caller to drop; the next claim's is untouched by the first claim's
/// watch, which no longer serves (ledger 3174, B3).
///
/// Verifies: `L.x86_64.198`
fn lifecycle() -> Result<(), &'static str> {
    let fake = Fake::new();
    let mut state = State::new();
    let (first_kernel, first_holder) =
        crate::object::channel::Endpoint::pair().map_err(|_| "no memory for a power claim")?;
    let first = state
        .claim(
            &fake,
            alloc::sync::Arc::downgrade(&first_holder),
            Some(first_kernel),
            0,
        )
        .map_err(|_| "a first power claim was refused")?;
    state.end(&fake, super::Cause::Gone, &mut Said::default());
    let ended = state.release();
    let (second_kernel, second_holder) =
        crate::object::channel::Endpoint::pair().map_err(|_| "no memory for a power claim")?;
    let second = state
        .claim(
            &fake,
            alloc::sync::Arc::downgrade(&second_holder),
            Some(second_kernel),
            0,
        )
        .map_err(|_| "a second power claim was refused")?;
    if ended.is_none()
        || state.serves(first)
        || !state.serves(second)
        || state.release().is_some()
        || state.gone()
    {
        return Err("a stale power watch reached the next claim");
    }
    drop(ended);
    state.end(&fake, super::Cause::Gone, &mut Said::default());
    drop(state.release());
    drop((first_holder, second_holder));
    Ok(())
}
