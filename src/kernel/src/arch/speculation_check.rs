//! The boot check for the side-channel defences.
//!
//! Run after stage 7's programs, so that there has been a switch of address
//! space to issue a barrier at. What it can check, it checks:
//!
//! * the clamp: an index inside its bound comes back itself, and -- on the
//!   path a misprediction takes, the clamp run without the check before it --
//!   one outside comes back zero in a hardened build and unchanged in one
//!   built with `--mitigations off`;
//! * every processor that is running recorded what it applied, and nothing it
//!   wrote to the processor failed to read back;
//! * a hardened build applied at least the clamp everywhere, and an unhardened
//!   one applied nothing anywhere;
//! * where any processor's plan has a switch barrier, programs in different
//!   address spaces have run, so at least one was issued -- any processor's,
//!   not the boot processor's, because on `AArch64` each core decides for
//!   itself, and a machine that boots on a core that needs no barrier may
//!   start others that do;
//! * no processor issued a switch barrier its own plan does not have;
//! * whatever the architecture adds: on x86-64, that `VERW`'s operand is one
//!   the instruction takes, on a machine whose exit path may not have run it.
//!
//! What it cannot check is that any of this stops an attack. That is a
//! property of the processor, and `docs/certification/SPECULATION.md` says
//! which vendor statement each defence rests on.

use super::machine_speculation as machine;
use super::speculation::{
    Defences, HARDENED, applied_by, nospec_below, nospec_index, switch_barriers, switch_barriers_on,
};

/// What the check found, for the boot log.
#[derive(Debug, Clone, Copy)]
pub struct Report {
    /// Processors that recorded what they applied.
    pub processors: usize,
    /// Of those, how many applied something other than the boot processor --
    /// the little cores of a big.LITTLE machine, which may need less.
    pub differing: usize,
    /// What the boot processor applied.
    pub boot: Defences,
    /// Switch barriers issued so far.
    pub barriers: u64,
    /// Whether this build carries the defences at all.
    pub hardened: bool,
}

/// Check the defences. See the module.
///
/// # Errors
///
/// What did not hold.
/// Verifies: `L.x86_64.28`, `L.x86_64.29`, `L.x86_64.31`
pub fn check() -> Result<Report, &'static str> {
    check_clamp()?;
    let boot = applied_by(0).ok_or("the boot processor recorded no side-channel defences")?;
    let processors = crate::smp::count();
    let mut differing = 0;
    let mut barrier_planned = false;
    for logical in 0..processors {
        let applied = check_processor(logical)?;
        barrier_planned |= plans_barrier(applied);
        if applied != boot {
            differing += 1;
        }
    }
    let barriers = switch_barriers();
    if HARDENED && barrier_planned && barriers == 0 {
        return Err("programs in different address spaces ran and no switch barrier was issued");
    }
    if !HARDENED && barriers != 0 {
        return Err("a build with the defences off issued a switch barrier");
    }
    machine::check()?;
    Ok(Report {
        processors,
        differing,
        boot,
        barriers,
        hardened: HARDENED,
    })
}

/// What processor `logical` recorded, held to the rules every processor
/// keeps: it recorded, everything it wrote read back, it applied the clamp in
/// a hardened build and nothing in an unhardened one, and it issued no switch
/// barrier its plan does not have.
fn check_processor(logical: usize) -> Result<Defences, &'static str> {
    let applied = applied_by(logical)
        .ok_or("a processor started without applying the side-channel defences")?;
    if applied.contains(Defences::READ_BACK_FAILED) {
        return Err("a side-channel defence written to a processor did not read back");
    }
    if HARDENED && !applied.contains(Defences::CLAMPED_INDICES) {
        return Err("a hardened build has a processor that did not record clamped indices");
    }
    if !HARDENED && applied != Defences::NONE {
        return Err("a build with the defences off has a processor that applied one");
    }
    if !plans_barrier(applied) && switch_barriers_on(logical) != 0 {
        return Err("a processor issued a switch barrier its plan does not have");
    }
    Ok(applied)
}

/// Whether `applied` has something to issue when its processor switches
/// address space.
const fn plans_barrier(applied: Defences) -> bool {
    applied.contains(Defences::SWITCH_BARRIER) || applied.contains(Defences::RSB_FILL)
}

/// The clamp, on both sides of its bound and on the mispredicted path.
/// Verifies: `L.x86_64.30`
fn check_clamp() -> Result<(), &'static str> {
    if nospec_index(3, 4) != Some(3) || nospec_index(0, 1) != Some(0) {
        return Err("an index inside its bound did not come back itself");
    }
    if nospec_index(4, 4).is_some() || nospec_index(usize::MAX, 4).is_some() {
        return Err("an index at or past its bound was not refused");
    }
    if nospec_below(0x1000, 0x2000) != Some(0x1000) || nospec_below(0x2000, 0x2000).is_some() {
        return Err("a 64-bit value was not bounded like an index");
    }
    // The mispredicted path: the check skipped, the clamp run on its own.
    // Through `black_box` so that neither side is worked out at compile time.
    let (outside, bound) = core::hint::black_box((9_usize, 4_usize));
    let clamped = machine::clamp_index(outside, bound);
    let wide = machine::clamp_below(core::hint::black_box(u64::MAX), 0x2000);
    if HARDENED && (clamped != 0 || wide != 0) {
        return Err("a clamped index past its bound came back other than zero");
    }
    if !HARDENED && (clamped != outside || wide != u64::MAX) {
        return Err("with the defences off, the clamp changed an index");
    }
    Ok(())
}
