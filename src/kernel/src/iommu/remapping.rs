//! Interrupt remapping on VT-d (`docs/NVIDIA.md` §12.3, N0g): which units
//! remap, the conversion of the one interrupt line that is live before they
//! do, how a function's messages are routed, and when a machine's
//! interrupts are isolated.
//!
//! # Why the architecture registers hooks here
//!
//! `iommu` is shared by every architecture and names none of them
//! (`arch`'s facade rule), and remapping is x86-64's alone, with nothing for
//! Arm to provide (condition G7). So x86-64 registers what bring-up needs of
//! it -- the vectors it has handed out, and its console's I/O APIC line --
//! through [`note_hooks`], from the console's interrupt set-up at stage 3,
//! and the functions an x86-64 caller alone reaches carry an `expect`
//! of dead code elsewhere.
//!
//! # Bring-up, per unit that can remap
//!
//! After the unit's queue and table are up ([`vtd::Unit::enable`]):
//!
//! 1. **The console's line, if this unit's DMAR scope names its I/O APIC.**
//!    It has been live in compatibility format since stage 3. It is masked;
//!    a fresh vector `V1` is taken; an entry is written for it, with the
//!    destination the line is routed to now and the I/O APIC's source ID,
//!    and invalidated. The old vector `V0` stays taken for good, so its
//!    entry index is never present.
//! 2. **No compatibility-format vector exists** (check R9): every vector
//!    handed out is the console's `V0` or `V1`. Message vectors are minted
//!    at stage 10, after this; one minted before would pass through, so the
//!    boot stops by name.
//! 3. `IRE`, and `CFIS` read back clear ([`vtd::Unit::start_remapping`]).
//! 4. The line's handler moves to `V1` through the generic interrupt layer,
//!    its entry is rewritten in remappable format naming `V1`'s entry with
//!    `V1` as its vector, it is unmasked, and the port is serviced once.
//!
//! The line is masked before it is rewritten, which under KVM's split
//! irqchip is required: QEMU recomputes a KVM route only on an entry or
//! message write or an invalidation, never on `IRE`, and a refused
//! recomputation leaves the old route delivering. A delivery on `V0` after
//! this is counted, and fails the boot.
//!
//! When no unit's scope names the console's I/O APIC, the line cannot be
//! converted and console input cannot go back to polling, so no unit
//! remaps: the line stays as it is and the machine's interrupts are not
//! isolated.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use ferrix_acpi::dmar::{self, Structure};
use ferrix_paging::vtd::remap::{self, Remap, UnitState};
use ferrix_pci::Address;
use ferrix_sync::Once;

use super::vtd;
use crate::panic::{catalog, fatal};
use crate::println;

/// The first vector the architecture hands out, whose entry index is 0: an
/// entry's index is its vector's slot among the sixty-four.
const FIRST_VECTOR: u8 = 0x40;

/// An interrupt line routed before remapping came up, masked for its
/// conversion: what it is routed to now.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Masked {
    /// The vector, `V0`.
    pub(crate) vector: u8,
    /// The local APIC ID it is delivered to.
    pub(crate) apic_id: u32,
    /// Whether it is level-triggered.
    pub(crate) level: bool,
}

/// x86-64's console line, as bring-up needs it.
#[derive(Clone, Copy, Debug)]
pub struct LiveLine {
    /// Its I/O APIC's MADT identifier.
    pub(crate) io_apic: u8,
    /// The vector it is routed to now, unmasked or not.
    pub(crate) routed: fn() -> Option<u8>,
    /// Mask it, and say what it is routed to.
    pub(crate) mask: fn() -> Option<Masked>,
    /// Unmask it as it was, in compatibility format.
    pub(crate) unmask: fn(),
    /// Move its handler from `old` to `new` through the generic interrupt
    /// layer, leaving `old` one that counts what still arrives there.
    pub(crate) retire: fn(old: u8, new: u8) -> Result<(), &'static str>,
    /// Undo `retire`, for a conversion given up.
    pub(crate) restore: fn(old: u8, new: u8),
    /// Write its entry in remappable format naming entry `handle` with
    /// vector `new`, unmask it, and service the port once.
    pub(crate) convert: fn(new: u8, handle: u16) -> Result<(), &'static str>,
    /// Loop `byte` back through the port to its own receiver, the port's
    /// transmit side quiet: for checks R5 and G3's service once.
    pub loop_back: fn(byte: u8),
}

/// What x86-64 registers for bring-up.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Hooks {
    /// Take a vector from the sixty-four the architecture hands out.
    pub(crate) vector: fn() -> Option<u8>,
    /// Which of the sixty-four are handed out, one bit each from 0x40.
    pub(crate) taken: fn() -> u64,
    /// NMIs taken since boot, for check R2.
    pub(crate) nmis: fn() -> u64,
    /// The console's line, when it is routed through an I/O APIC.
    pub(crate) line: Option<LiveLine>,
}

/// What x86-64 registered.
static HOOKS: Once<Hooks> = Once::new();

/// The console line's two vectors once converted, `V0` in bits 7:0 and
/// `V1` in bits 15:8; 0 before.
static CONSOLE_VECTORS: AtomicU64 = AtomicU64::new(0);

/// Whether a unit is remapping, so a function's messages are routed through
/// one.
static ANY_REMAPPING: AtomicBool = AtomicBool::new(false);

/// What each unit the DMAR lists ended in, refused ones included (G4).
static STATES: Once<Vec<UnitState>> = Once::new();

/// Set only by check R10 (`iommu/check.rs`), for the length of the check:
/// the machine's interrupts read as not isolated, so the isolated-
/// interrupts mark's refusal can be shown on a machine whose interrupts
/// are. Never set at run time.
pub static FORCED_UNISOLATED: AtomicBool = AtomicBool::new(false);

/// PCI functions placed behind no unit, once stage 10's discovery has
/// counted them; `usize::MAX` before.
static BYPASSING: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Register what bring-up needs of the architecture. Only the first call
/// counts.
#[cfg_attr(
    not(target_arch = "x86_64"),
    expect(dead_code, reason = "only x86-64 has interrupts to remap")
)]
pub(crate) fn note_hooks(hooks: Hooks) {
    let _ = HOOKS.call_once(|| hooks);
}

/// The console line's MADT I/O APIC identifier and the source ID the DMAR
/// gives it on some unit: `None` with no line; `Some((id, None))` when no
/// unit's scope names its I/O APIC.
fn console_source(table: &dmar::Dmar<'_>) -> Option<(u8, Option<u16>)> {
    let line = HOOKS.get()?.line?;
    let source = table.structures().find_map(|structure| match structure {
        Structure::Drhd(unit) => unit.ioapic_source(line.io_apic),
        _ => None,
    });
    Some((line.io_apic, source))
}

/// Bring remapping up on `unit`, which `drhd` describes and which has its
/// queue and table, and say what it ended in. Translation is not on yet.
pub(super) fn bring_up(
    unit: &vtd::Unit,
    drhd: &dmar::Drhd<'_>,
    table: &dmar::Dmar<'_>,
) -> UnitState {
    let off = UnitState {
        remapping: false,
        compatibility_blocked: false,
    };
    if !unit.can_remap() {
        return off;
    }
    let console = console_source(table);
    if let Some((io_apic, None)) = console {
        println!(
            "  iommu    vt-d unit {:#x}: remapping not enabled: no DMAR scope names I/O APIC \
             {io_apic}, which carries the console",
            unit.phys()
        );
        return off;
    }
    let carried = console
        .and_then(|(io_apic, source)| source.filter(|_| drhd.ioapic_source(io_apic).is_some()));
    let converting = match carried {
        Some(source) => match prepare_console(unit, source) {
            Ok(converting) => Some(converting),
            Err(why) => {
                println!(
                    "  iommu    vt-d unit {:#x}: remapping not enabled: the console's line \
                     could not be converted: {why}",
                    unit.phys()
                );
                return off;
            }
        },
        None => None,
    };
    require_no_compatibility_vector();
    let blocked = match unit.start_remapping() {
        Ok(blocked) => blocked,
        Err(why) => {
            unmask_console(converting);
            println!(
                "  iommu    vt-d unit {:#x}: remapping not enabled: {why}",
                unit.phys()
            );
            return off;
        }
    };
    if !blocked {
        unmask_console(converting);
        println!(
            "  iommu    vt-d unit {:#x}: remapping not enabled: compatibility format still \
             accepted (CFIS read back set)",
            unit.phys()
        );
        return off;
    }
    ANY_REMAPPING.store(true, Ordering::Release);
    if let Some((old, new, handle)) = converting {
        finish_console(unit, old, new, handle);
    }
    UnitState {
        remapping: true,
        compatibility_blocked: true,
    }
}

/// Steps 1 of the conversion: mask the console's line, take `V1`, and
/// write and invalidate `V1`'s entry for the line's destination and
/// `source`. `(V0, V1, V1's entry)`.
fn prepare_console(unit: &vtd::Unit, source: u16) -> Result<(u8, u8, u16), &'static str> {
    let hooks = HOOKS.get().ok_or("no interrupt hooks")?;
    let line = hooks.line.ok_or("no console line")?;
    let masked = (line.mask)().ok_or("the console's line is not routed")?;
    // A byte received while the line is masked raises an edge the I/O APIC
    // drops; the service after the conversion must read it (G3).
    super::check::plant_masked_byte(&line);
    let result = (|| {
        let destination =
            remap::destination(masked.apic_id).ok_or("a destination above APIC ID 255")?;
        let new = (hooks.vector)().ok_or("no vector left for the console")?;
        let handle = u16::from(new.wrapping_sub(FIRST_VECTOR));
        CONSOLE_VECTORS.store(
            u64::from(masked.vector) | u64::from(new) << 8,
            Ordering::Release,
        );
        unit.remap(
            handle,
            Remap {
                vector: new,
                destination,
                level: masked.level,
                source,
            },
        )?;
        // The handler to `V1` now, while the line is masked and before
        // `IRE`: from here a delivery on `V0` -- a compatibility route KVM
        // kept, on a line rewritten unmasked -- is counted, and fails the
        // boot.
        (line.retire)(masked.vector, new)?;
        Ok((masked.vector, new, handle))
    })();
    if result.is_err() {
        (line.unmask)();
    }
    result
}

/// Give the console's line back its handler on `V0` and unmask it as it
/// was, if a conversion masked it and moved the handler.
fn unmask_console(converting: Option<(u8, u8, u16)>) {
    if let Some((old, new, _)) = converting
        && let Some(line) = HOOKS.get().and_then(|hooks| hooks.line)
    {
        (line.restore)(old, new);
        (line.unmask)();
    }
}

/// Step 4: the console's line delivered through its entry from here.
fn finish_console(unit: &vtd::Unit, old: u8, new: u8, handle: u16) {
    let Some(line) = HOOKS.get().and_then(|hooks| hooks.line) else {
        return;
    };
    let converted = (line.convert)(new, handle);
    super::check::require_masked_byte_served();
    match converted {
        Ok(()) => println!(
            "  iommu    vt-d unit {:#x}: the console's I/O APIC line converted: vector {old:#x} \
             retired, {new:#x} through interrupt entry {handle}",
            unit.phys()
        ),
        Err(why) => fatal!(
            catalog::STAGE10_REMAP,
            "the console's I/O APIC line could not be converted to remappable format: {why}"
        ),
    }
}

/// Check R9: every vector handed out before remapping goes on is the
/// console line's, `V0` or `V1`. One minted in compatibility format before
/// it would pass through every unit; the boot stops.
fn require_no_compatibility_vector() {
    let Some(hooks) = HOOKS.get() else {
        return;
    };
    let taken = (hooks.taken)();
    let console = CONSOLE_VECTORS.load(Ordering::Acquire);
    let routed = hooks
        .line
        .and_then(|line| (line.routed)())
        .map_or(0, u64::from);
    let allowed = [console & 0xFF, console >> 8 & 0xFF, routed]
        .into_iter()
        .filter(|&vector| vector >= u64::from(FIRST_VECTOR))
        .fold(0_u64, |bits, vector| {
            bits | 1 << (vector - u64::from(FIRST_VECTOR))
        });
    if taken & !allowed != 0 {
        fatal!(
            catalog::STAGE10_REMAP,
            "a vector was minted in compatibility format before interrupt remapping: \
             vectors {taken:#x} handed out, of which only {allowed:#x} are the console's"
        );
    }
    let converted = u64::from(console != 0);
    println!(
        "  iommu    no vector minted in compatibility format before remapping: {} handed out, \
         all the console's; {converted} I/O APIC inputs converted",
        taken.count_ones()
    );
}

/// Record what every unit the DMAR lists ended in.
pub(super) fn record(states: Vec<UnitState>) {
    let _ = STATES.call_once(|| states);
}

/// Record how many PCI functions discovery placed behind no unit.
pub(super) fn note_bypassing(bypassing: usize) {
    BYPASSING.store(bypassing, Ordering::Release);
}

/// The console line's retired vector and the one it is delivered on now,
/// once converted.
pub fn console_vectors() -> Option<(u8, u8)> {
    let console = CONSOLE_VECTORS.load(Ordering::Acquire);
    (console != 0 && ANY_REMAPPING.load(Ordering::Acquire))
        .then_some(((console & 0xFF) as u8, (console >> 8 & 0xFF) as u8))
}

/// The console's line, where x86-64 registered one.
pub fn live_line() -> Option<LiveLine> {
    HOOKS.get().and_then(|hooks| hooks.line)
}

/// Whether any unit remaps interrupts.
pub fn any_remapping() -> bool {
    ANY_REMAPPING.load(Ordering::Acquire)
}

/// NMIs taken since boot, where the architecture counts them: for check
/// R2, that a forged NMI-mode message arrived nowhere.
pub(crate) fn nmis() -> Option<u64> {
    HOOKS.get().map(|hooks| (hooks.nmis)())
}

/// Whether the machine's interrupts are isolated (`INTERRUPTS_ISOLATED`,
/// condition G4): every unit the DMAR lists remaps with compatibility
/// format blocked, and no PCI function bypasses every unit. False before
/// stage 10's discovery has counted the functions.
pub(crate) fn interrupts_isolated() -> bool {
    let bypassing = BYPASSING.load(Ordering::Acquire);
    !FORCED_UNISOLATED.load(Ordering::Acquire)
        && bypassing != usize::MAX
        && STATES
            .get()
            .is_some_and(|states| remap::interrupts_isolated(states, bypassing))
}

/// How a function's messages reach the processors.
#[derive(Clone, Copy, Debug)]
#[cfg_attr(
    not(target_arch = "x86_64"),
    expect(dead_code, reason = "only x86-64 programs a remapped message")
)]
pub enum Route {
    /// No unit remaps: a compatibility-format message.
    Compatibility,
    /// Through this unit's interrupt remapping table.
    Remapped(&'static vtd::Unit),
}

/// How the function whose requester ID is `requester`, on segment 0 -- the
/// only one x86-64 enumerates -- has its messages routed.
///
/// # Errors
///
/// While a unit remaps: the function is placed behind no unit that remaps,
/// or under another's source ID (`topology::Behind::Aliased`), so it gets no
/// vector (`L.device.28`).
pub fn route(requester: u16) -> Result<Route, &'static str> {
    if !ANY_REMAPPING.load(Ordering::Acquire) {
        return Ok(Route::Compatibility);
    }
    let [bus, devfn] = requester.to_be_bytes();
    let function = Address::new(0, bus, devfn >> 3, devfn & 7)
        .ok_or("a requester ID that names no function")?;
    match super::programmed_unit_for(function) {
        Some(unit) if unit.remapping() => Ok(Route::Remapped(unit)),
        Some(_) => Ok(Route::Compatibility),
        None => Err(
            "the function's messages arrive under another's source ID, or reach no unit \
                     that remaps",
        ),
    }
}

/// Write `vector`'s entry on `unit` for the function `requester`, aimed at
/// `apic_id`, and answer the remappable-format message's address that
/// names it; its data is 0.
///
/// # Errors
///
/// A destination past APIC ID 255, or what [`vtd::Unit::remap`] refused.
#[cfg_attr(
    not(target_arch = "x86_64"),
    expect(dead_code, reason = "only x86-64 routes messages through VT-d")
)]
pub(crate) fn remap_message(
    unit: &vtd::Unit,
    requester: u16,
    vector: u8,
    apic_id: u32,
) -> Result<u64, &'static str> {
    let destination = remap::destination(apic_id).ok_or("a destination above APIC ID 255")?;
    let handle = u16::from(vector.wrapping_sub(FIRST_VECTOR));
    unit.remap(
        handle,
        Remap {
            vector,
            destination,
            level: false,
            source: requester,
        },
    )?;
    Ok(remap::message_address(handle))
}

/// Whether the function at `function` has its interrupts isolated: the
/// machine's are, and the unit it is placed behind remaps.
pub(crate) fn function_isolated(function: Address) -> bool {
    interrupts_isolated() && super::programmed_unit_for(function).is_some_and(vtd::Unit::remapping)
}

/// The vector check R1 aims its forged compatibility-format message at
/// (ruling 3): outside the sixty-four the architecture hands out, so no
/// entry is indexed by it, and below the kernel's own IPI, timer and
/// spurious vectors, so a forged one cannot be told for theirs.
pub const CHECK_VECTOR: u8 = 0xFC;

/// Whether check R1 or R2 is watching for [`CHECK_VECTOR`].
static CHECK_WINDOW: AtomicBool = AtomicBool::new(false);
/// Deliveries of [`CHECK_VECTOR`] while a check watched.
static CHECK_SEEN: AtomicU64 = AtomicU64::new(0);
/// Deliveries of [`CHECK_VECTOR`] while none did: stray, and the boot fails.
static CHECK_STRAY: AtomicU64 = AtomicU64::new(0);
/// Deliveries on the console line's retired vector after its conversion.
static RETIRED_DELIVERIES: AtomicU64 = AtomicU64::new(0);

/// Count a delivery of [`CHECK_VECTOR`], from its handler, which does
/// nothing else but acknowledge it.
#[cfg_attr(
    not(target_arch = "x86_64"),
    expect(dead_code, reason = "only x86-64 has a check vector")
)]
pub(crate) fn note_check_vector() {
    if CHECK_WINDOW.load(Ordering::Acquire) {
        let _ = CHECK_SEEN.fetch_add(1, Ordering::Relaxed);
    } else {
        let _ = CHECK_STRAY.fetch_add(1, Ordering::Relaxed);
    }
}

/// Open check R1's or R2's window on [`CHECK_VECTOR`], answering the
/// deliveries seen so far.
pub(crate) fn open_check_window() -> u64 {
    CHECK_WINDOW.store(true, Ordering::Release);
    CHECK_SEEN.load(Ordering::Acquire)
}

/// Close it, answering the deliveries seen so far.
pub(crate) fn close_check_window() -> u64 {
    CHECK_WINDOW.store(false, Ordering::Release);
    CHECK_SEEN.load(Ordering::Acquire)
}

/// Count a delivery on the console line's retired vector, from the handler
/// the conversion left there.
#[cfg_attr(
    not(target_arch = "x86_64"),
    expect(dead_code, reason = "only x86-64 converts a line")
)]
pub(crate) fn note_retired_delivery(_irq: u32) {
    let _ = RETIRED_DELIVERIES.fetch_add(1, Ordering::Relaxed);
}

/// Deliveries of [`CHECK_VECTOR`] outside a check's window, and on the
/// console's retired vector: each fails the boot.
pub fn stray_deliveries() -> (u64, u64) {
    (
        CHECK_STRAY.load(Ordering::Relaxed),
        RETIRED_DELIVERIES.load(Ordering::Relaxed),
    )
}
