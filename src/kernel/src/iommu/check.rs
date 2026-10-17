//! The IOMMU's boot checks.
//!
//! * a VT-d unit firmware left on, seen as `open` turns its queue off
//!   ([`check_firmware_left_on`]), and the console line's byte received
//!   while masked ([`plant_masked_byte`], [`require_masked_byte_served`]):
//!   the checks a bring-up runs inside itself;
//! * [`check_dma_faults`]: last in the boot, no unit recorded a fault that no
//!   check provoked;
//! * [`run`]: the unit gate's waits, in the shapes a unit that answers at
//!   once -- every unit QEMU presents -- never puts them in: a look that
//!   outlasts its deadline, one that takes long enough for a waiter to start
//!   giving up its processor, and an operation that stays inside a gate past
//!   the next one's patience.
//!
//! Stage 10's check of domains, pins and remapping, which drives native
//! processes, is `domain_check.rs` (`docs/KERNEL-CRATES.md` §3 (3)).

use crate::panic::{catalog, fatal};
use crate::println;

use super::gate::{self, Gate};

/// Run the gate's.
///
/// # Errors
///
/// The first property that did not hold, as a sentence.
///
/// Verifies: L.iommu.30, L.iommu.31
pub(crate) fn run() -> Result<(), &'static str> {
    // A deadline already past: one last look, and its answer.
    if gate::poll(|| false, 0) {
        return Err("a unit wait past its deadline said the unit had answered");
    }

    // A unit that answers on the hundredth look, to a waiter that may block:
    // it gives up its processor between looks and still sees the answer.
    let mut looks = 0_u32;
    let far = crate::timer::now_nanos().saturating_add(5_000_000_000);
    if !gate::poll(
        || {
            looks += 1;
            looks > 100
        },
        far,
    ) {
        return Err("a unit wait that yielded between looks missed the unit's answer");
    }

    // An operation inside the gate for longer than the next one's patience:
    // the next is refused rather than left waiting for good.
    let unit = Gate::new();
    let inside = unit
        .enter()
        .map_err(|_| "an empty gate could not be entered")?;
    // Through the same wait `enter` makes, with a patience of 10 ms, not
    // the unit's second.
    if unit.enter_within(10_000_000).is_ok() {
        return Err("a gate was entered twice at once");
    }
    drop(inside);
    if unit.enter().is_err() {
        return Err("a gate left by its holder could not be entered");
    }
    // As a translated domain's failure report prints it.
    if !alloc::format!("{unit:?}").starts_with("Gate { held: false") {
        return Err("a gate does not print whether it is held");
    }
    Ok(())
}

/// VT-d units on which [`check_firmware_left_on`] left the queue on as
/// firmware would and saw `open`'s path turn it off.
pub(super) static FIRMWARE_QUEUES_STOPPED: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// Stage 10, at bring-up, between a VT-d unit's `open` and its `enable`,
/// unless the boot was told to skip its checks:
/// the unit's invalidation queue turned on as firmware that used it would
/// leave it, and then turned off by the path `open` takes when it finds a
/// queue firmware left on, and read back off. No firmware QEMU boots leaves
/// one on, so without this the path would never run.
///
/// Halts rather than returning, as every other stage's check does.
///
/// Verifies: L.iommu.51
pub(crate) fn check_firmware_left_on(unit: &super::vtd::Unit) {
    if !crate::boot_checks::run() {
        return;
    }
    if let Err(problem) = super::vtd::leave_queue_on_and_stop(unit) {
        fatal!(
            catalog::STAGE10_IOMMU,
            "stage 10 self-check failed: {problem}"
        );
    }
    let _ = FIRMWARE_QUEUES_STOPPED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

/// Stage 10, at bring-up, after a VT-d unit's `open`, checks or not: its
/// fault event reads back masked, as `open` left it. A unit's own fault
/// messages are never remapped, so one firmware left armed would deliver
/// wherever firmware aimed it.
///
/// Halts rather than returning, as every other stage's check does.
///
/// Verifies: L.iommu.70
pub(crate) fn check_fault_events_masked(unit: &super::vtd::Unit) {
    if !unit.fault_events_masked() {
        fatal!(
            catalog::STAGE10_IOMMU,
            "stage 10 self-check failed: VT-d unit {:#x}'s fault event did not read back masked",
            unit.phys()
        );
    }
}

/// Stage 10: no IOMMU recorded a fault that no check provoked. Answers how
/// many DMA faults were read from a unit, for the audit record's end-of-boot
/// check, which runs after this since reading the faults is what records them.
///
/// Halts rather than returning, as every other stage's check does.
///
/// Verifies: H.DMA.6
pub(crate) fn check_dma_faults() -> u64 {
    let audit = super::audit_faults();
    println!(
        "  iommu    {} DMA faults recorded that no check provoked, {} of them unit events other \
         than a refused access, across {} units that translate or remap ({} translating); {} \
         late faults from the out-of-domain probe",
        audit.stray, audit.stray_events, audit.units, audit.translating, audit.provoked,
    );
    check_stray_deliveries();
    if audit.stray == 0 {
        return super::dma_faults_read();
    }
    if let Some(fault) = audit.first {
        println!("  iommu    the first read here: {fault}");
    }
    fatal!(
        catalog::STAGE10_DMA_FAULT,
        "stage 10 self-check failed: {} DMA faults no check provoked",
        audit.stray
    );
}

/// Interrupt remapping's two stray-delivery counts, read last in the boot
/// beside the stray faults: the check vector 0xFC delivered outside checks
/// R1 and R2's window (ruling 3), and the console line's retired vector
/// delivered after its conversion. Either is a message that should have
/// been refused or a route that should have gone, and fails the boot. Said
/// only where a unit remaps.
///
/// Halts rather than returning, as every other stage's check does.
///
/// Verifies: `L.x86_64.130`, H.DMA.9
fn check_stray_deliveries() {
    if !super::remapping::any_remapping() {
        return;
    }
    let (check, retired) = super::stray_deliveries();
    let old = super::console_vectors().map_or(0, |(old, _)| old);
    println!(
        "  remap    {check} deliveries of the check vector {:#x} outside its window, {retired} on \
         the console's retired vector {old:#x} after its conversion",
        super::CHECK_VECTOR
    );
    if check != 0 || retired != 0 {
        fatal!(
            catalog::STAGE10_DMA_FAULT,
            "stage 10 self-check failed: {check} stray deliveries of the check vector and \
             {retired} on the console's retired vector"
        );
    }
}

/// The byte looped back while the console's line is masked for its
/// conversion, which only the service after it can read (G3).
const MASKED_BYTE: u8 = 0xF5;

/// Whether [`plant_masked_byte`] planted one.
static PLANTED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// G3, at bring-up, while the console's line is masked for its conversion,
/// unless the boot was told to skip its checks: loop a byte back into the
/// port, whose edge the I/O APIC drops, so that only the service once after
/// the conversion can read it ([`require_masked_byte_served`]). The receive
/// path is armed for it first, so that it keeps the byte for the check
/// rather than handing it to the console's reader.
pub(crate) fn plant_masked_byte(line: &super::LiveLine) {
    if !crate::boot_checks::run() {
        return;
    }
    crate::console::input::arm_check_byte(MASKED_BYTE);
    (line.loop_back)(MASKED_BYTE);
    PLANTED.store(true, core::sync::atomic::Ordering::Relaxed);
}

/// G3, at bring-up, after the console's line is converted: the byte
/// [`plant_masked_byte`] planted was read from the port by the service once,
/// as the receive path, armed for it, says; it never reached the console's
/// ring, so the `console` thread cannot take it before this looks (FX-1012).
/// The service reads the port whether or not the loopback raised an
/// interrupt, so this holds on a UART that raises none in loopback too.
///
/// Halts rather than returning, as every other stage's check does.
///
/// Verifies: `L.x86_64.130`
pub(crate) fn require_masked_byte_served() {
    if !PLANTED.swap(false, core::sync::atomic::Ordering::Relaxed) {
        return;
    }
    if !crate::console::input::disarm_check_byte(MASKED_BYTE) {
        fatal!(
            catalog::STAGE10_REMAP,
            "a byte the port received while its line was masked was not read by the service \
             after the conversion"
        );
    }
    MASKED_BYTE_SERVED.store(true, core::sync::atomic::Ordering::Relaxed);
}

/// Whether [`require_masked_byte_served`] found its byte read.
pub(super) static MASKED_BYTE_SERVED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
