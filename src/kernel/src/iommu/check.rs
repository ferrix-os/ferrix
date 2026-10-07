//! The IOMMU's boot checks.
//!
//! * [`check_iommu`]: a device's domain pins and unpins as the rules say, and
//!   the quarantine holds a dead driver's pins, as stage 10 requires;
//! * [`check_dma_faults`]: last in the boot, no unit recorded a fault that no
//!   check provoked;
//! * [`run`]: the unit gate's waits, in the shapes a unit that answers at
//!   once -- every unit QEMU presents -- never puts them in: a look that
//!   outlasts its deadline, one that takes long enough for a waiter to start
//!   giving up its processor, and an operation that stays inside a gate past
//!   the next one's patience.

use alloc::sync::Arc;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_paging::MapFlags;

use crate::device::{self, DeviceNode, Location};
use crate::panic::{catalog, fatal};
use crate::{mm, object, println};

use super::gate::{self, Gate};
use super::{Domain, DomainError};

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

/// Stage 10: the ceiling on raised pin budgets, counted; then a device's
/// domain, and the budget and quarantine a driver's pins count against,
/// unless the boot was told to skip its checks.
///
/// Halts rather than returning, as every other stage's check does.
pub(crate) fn check_iommu() {
    // The ceiling on raised pin budgets, a quarter of RAM, counted here at
    // stage 10, checks or not: before devmgr can set a budget, and before
    // the budget's checks below set one (`object::pin`).
    object::pin::count_ceiling();
    if !crate::checks::run() {
        return;
    }
    let domains = match check_domains(device::devices()) {
        Ok(report) => report,
        // A change refused for a write not cleaned fails the domains'
        // check too; the cause is named first.
        Err(problem) => match check_cleaning(super::vtd::cleaning()) {
            Err(cause) => fatal!(
                catalog::STAGE10_IOMMU,
                "stage 10 self-check failed: {cause} ({problem})"
            ),
            Ok(_) => fatal!(
                catalog::STAGE10_IOMMU,
                "stage 10 self-check failed: {problem}"
            ),
        },
    };
    println!(
        "  iommu    {} pages pinned and unpinned through a device's {} domain, {} refusals \
         as specified, {} waits on a unit with interrupts on",
        domains.pinned,
        if domains.translated {
            "translated"
        } else {
            "untranslated"
        },
        domains.refusals,
        domains.waits,
    );
    match check_cleaning(super::vtd::cleaning()) {
        Ok(Some(line)) => println!("  iommu    {line}"),
        Ok(None) => {}
        Err(problem) => fatal!(
            catalog::STAGE10_IOMMU,
            "stage 10 self-check failed: {problem}"
        ),
    }
    let failed_kept = match object::pin::check::check_budget(device::devices()) {
        Ok(Some(report)) => {
            println!(
                "  iommu    pin budget: {} pins refused at a budget of 2 pages and at twice it, a \
                 dead driver's pins quarantined and released, {} page kept and still counted, \
                 and device_set_limit refused {} times -- without SET_LIMIT, under a live pin \
                 and past the ceiling of {} pages -- each audited",
                report.refusals,
                report.kept,
                report.set_refused,
                object::pin::ceiling(),
            );
            report.failed_kept
        }
        Ok(None) => 0,
        Err(problem) => fatal!(
            catalog::STAGE10_IOMMU,
            "stage 10 self-check failed: {problem}"
        ),
    };
    let planted = match check_completion_errors() {
        Ok(planted) => planted,
        Err(problem) => fatal!(
            catalog::STAGE10_IOMMU,
            "stage 10 self-check failed: {problem}"
        ),
    };
    check_remapping();
    match check_queue(super::invalidations(), failed_kept, planted) {
        Ok(Some(line)) => println!("  iommu    {line}"),
        Ok(None) => {}
        Err(problem) => fatal!(
            catalog::STAGE10_IOMMU,
            "stage 10 self-check failed: {problem}"
        ),
    }
    match object::pin::check::check_untranslated(device::devices()) {
        Ok(Some(done)) => println!(
            "  iommu    untranslated pins: {} a live driver closed given back at once, past twice \
             a budget of 2 pages; {} pages a dead driver left quarantined and freed at the next \
             HELLO",
            done.given_back, done.released,
        ),
        Ok(None) => {}
        Err(problem) => fatal!(
            catalog::STAGE10_IOMMU,
            "stage 10 self-check failed: {problem}"
        ),
    }
}

/// Every VT-d change on a unit whose walk does not snoop found its table
/// writes cleaned to memory before its own publish point (finding F-58): no
/// attach, detach, map or unmap was refused for a write noted and not
/// cleaned, and the boot's domains made some to clean. QEMU's unit reports
/// `ECAP.C` clear, so this runs on every x86-64 boot test. That every write
/// is noted rests on construction (`vtd.rs`'s module documentation), not on
/// this check.
///
/// `None` on a machine with no such unit.
///
/// # Errors
///
/// A change refused for a write not cleaned, or a unit that cleaned nothing
/// over a boot that attached and pinned through it.
///
/// Verifies: L.iommu.56
/// Verifies: L.iommu.57
fn check_cleaning(
    cleaning: super::vtd::Cleaning,
) -> Result<Option<alloc::string::String>, alloc::string::String> {
    if cleaning.uncleaned != 0 {
        return Err(alloc::format!(
            "{} of {} VT-d changes were refused for a table write not cleaned to memory",
            cleaning.uncleaned,
            cleaning.checked
        ));
    }
    if cleaning.units == 0 {
        return Ok(None);
    }
    if cleaning.entries == 0 || cleaning.tables == 0 || cleaning.checked == 0 {
        return Err(alloc::format!(
            "a VT-d unit that does not snoop cleaned {} entries and {} tables over {} changes",
            cleaning.entries,
            cleaning.tables,
            cleaning.checked
        ));
    }
    Ok(Some(alloc::format!(
        "{} entry writes and {} fresh tables noted and cleaned to memory on {} VT-d units that \
         do not snoop; {} changes each found them cleaned before its own publish point",
        cleaning.entries,
        cleaning.tables,
        cleaning.units,
        cleaning.checked
    )))
}

/// Interrupt remapping's stage 10 checks, each printing its line where a
/// unit remaps (`docs/NVIDIA.md` §12.3): R5 first, while the console's
/// entry is as bring-up wrote it, then `L.device.28` and `L.iommu.52`, then
/// R10.
///
/// Halts rather than returning, as every other stage's check does.
fn check_remapping() {
    match check_console_line() {
        Ok(Some(line)) => println!("  remap    {line}"),
        Ok(None) => {}
        Err(problem) => fatal!(
            catalog::STAGE10_IOMMU,
            "stage 10 self-check failed: {problem}"
        ),
    }
    match check_no_route_unplaced() {
        Ok(Some(line)) => println!("  remap    {line}"),
        Ok(None) => {}
        Err(problem) => fatal!(
            catalog::STAGE10_IOMMU,
            "stage 10 self-check failed: {problem}"
        ),
    }
    match check_isolation_mark() {
        Ok(Some(line)) => println!("  remap    {line}"),
        Ok(None) => {}
        Err(problem) => fatal!(
            catalog::STAGE10_IOMMU,
            "stage 10 self-check failed: {problem}"
        ),
    }
}

/// VT-d units on which [`check_firmware_left_on`] left the queue on as
/// firmware would and saw `open`'s path turn it off.
static FIRMWARE_QUEUES_STOPPED: core::sync::atomic::AtomicU64 =
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
    if !crate::checks::run() {
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

/// R6 and R7 (`docs/NVIDIA.md` §12.3): every VT-d invalidation of the boot
/// went through a unit's queue and was waited for, and none by register --
/// no unit reads back a register-based invalidation pending, which QEMU's
/// leaves set for good once the queue is on -- and the one invalidation check
/// R7 made fail, and nothing else, failed: its pin's `failed_kept` pages were
/// kept. No unit was marked failed.
///
/// `None` on a machine with no VT-d unit translating.
///
/// # Errors
///
/// A register-based invalidation pending, no invalidation of a kind the
/// boot makes, a failure R7 did not make, or a unit marked failed.
///
/// Verifies: L.iommu.47
/// Verifies: L.iommu.48
fn check_queue(
    queued: super::Invalidations,
    failed_kept: usize,
    planted: u64,
) -> Result<Option<alloc::string::String>, alloc::string::String> {
    let (units, pending) = super::register_invalidations_pending();
    if units == 0 {
        return Ok(None);
    }
    if pending != 0 {
        return Err(alloc::format!(
            "{pending} of {units} VT-d units read back a register-based invalidation pending"
        ));
    }
    if queued.context == 0 || queued.iotlb == 0 {
        return Err(alloc::format!(
            "the boot queued {} context-cache and {} IOTLB invalidations, where it makes both",
            queued.context,
            queued.iotlb
        ));
    }
    if queued.units_failed != 0 {
        return Err(alloc::format!(
            "{} VT-d units' invalidation queues stopped",
            queued.units_failed
        ));
    }
    if queued.completion_errors != planted || planted != units as u64 {
        return Err(alloc::format!(
            "{} completion errors were cleared where the check planted {planted} on {units} \
             VT-d units",
            queued.completion_errors
        ));
    }
    let provoked = u64::from(failed_kept != 0);
    let unplanted = queued.failed.saturating_sub(planted);
    if unplanted != provoked || queued.failed < planted {
        return Err(alloc::format!(
            "{} VT-d invalidations failed where check R7 made {provoked} fail and the planted \
             completion errors {planted}",
            queued.failed
        ));
    }
    let stopped = FIRMWARE_QUEUES_STOPPED.load(core::sync::atomic::Ordering::Relaxed);
    if stopped != units as u64 {
        return Err(alloc::format!(
            "a queue left on as firmware leaves it was turned off on {stopped} of {units} VT-d \
             units"
        ));
    }
    if super::remapping::any_remapping() && queued.interrupt_entry == 0 {
        return Err(
            "a unit remaps interrupts and the boot queued no interrupt entry cache invalidation"
                .into(),
        );
    }
    Ok(Some(alloc::format!(
        "{} context-cache, {} IOTLB and {} interrupt entry invalidations queued and each waited \
         for, none by \
         register on {units} VT-d units; {unplanted} failed as check R7 made it, its \
         {failed_kept} pages kept; {planted} completion errors planted, cleared, and the next \
         invalidation completed; a queue left on as firmware leaves it turned off on {stopped}",
        queued.context,
        queued.iotlb,
        queued.interrupt_entry,
    )))
}

/// The consultant's condition 1 on N0g's slice 1: on every translating
/// VT-d unit, an invalidation completion error (`ICE`) the check plants
/// fails that invalidation, is cleared and counted, and the next
/// invalidation completes on a unit not marked failed -- so a completion
/// error is a failed invalidation, never a sticky bit that fails every
/// later one. Answers how many were planted.
///
/// # Errors
///
/// What did not hold, as a sentence.
///
/// Verifies: L.iommu.48
fn check_completion_errors() -> Result<u64, &'static str> {
    let Some(programmed) = super::PROGRAMMED.get() else {
        return Ok(0);
    };
    for unit in &programmed.vtd {
        super::vtd::check_planted_completion_error(unit)?;
    }
    Ok(programmed.vtd.len() as u64)
}

/// What the domain check found.
#[derive(Clone, Copy, Debug, Default)]
struct DomainReport {
    /// Pages pinned and unpinned.
    pinned: u64,
    /// Requests refused, each exactly as the rule requires.
    refusals: usize,
    /// Whether the domain checked was translated.
    translated: bool,
    /// Waits on a unit made with interrupts on, since boot.
    waits: u64,
}

/// Pin two frames through the first PCI node's domain, and require the node to
/// hand out one domain, the pin to give each frame an address, the domain to
/// count what it holds, and a pin to be refused by any domain but its own.
///
/// # Errors
///
/// The first thing that is not so. The frames are then kept out of the
/// allocator, since a pin that was not given back may still be reachable.
///
/// Verifies: L.device.8
fn check_domains(nodes: &[Arc<DeviceNode>]) -> Result<DomainReport, &'static str> {
    let mut report = DomainReport::default();
    let Some(node) = nodes
        .iter()
        .find(|node| matches!(node.location(), Location::Pci(_)))
    else {
        return Ok(report);
    };
    let domain = node
        .domain()
        .map_err(|_| "no memory for a device's domain")?;
    let again = node
        .domain()
        .map_err(|_| "no memory for a device's domain")?;
    if !Arc::ptr_eq(&domain, &again) {
        return Err("a device node handed out two domains");
    }
    let Some(first) = mm::allocate_frames(0) else {
        return Err("no frame to pin");
    };
    let Some(second) = mm::allocate_frames(0) else {
        mm::deallocate_frames(first, 0);
        return Err("no frame to pin");
    };
    pin_and_unpin(&domain, [first, second], &mut report)?;
    mm::deallocate_frames(first, 0);
    mm::deallocate_frames(second, 0);
    Ok(report)
}

/// The body of [`check_domains`], once it has its frames.
///
/// Verifies: L.iommu.19, L.iommu.20, L.iommu.21, L.iommu.22, L.iommu.29, H.DMA.3
fn pin_and_unpin(
    domain: &Domain,
    frames: [u64; 2],
    report: &mut DomainReport,
) -> Result<(), &'static str> {
    let before = domain.pinned_pages();
    let pinned = domain
        .pin(&frames, MapFlags::DMA)
        .map_err(|_| "a domain refused to pin two frames")?;
    let expected = frames.map(|frame| frame * PAGE_SIZE);
    // Every domain gives a page its physical address as its device address;
    // a translated one must also send the device there and nowhere else.
    let addressed = pinned.addresses() == expected.as_slice()
        && expected
            .iter()
            .all(|&phys| domain.resolve(phys) == Some(phys));
    let counted = domain.pinned_pages() == before + 2;

    let Err((DomainError::Foreign, pinned)) = Domain::untranslated().unpin(pinned) else {
        return Err("a domain unpinned a pin another domain took");
    };
    report.refusals += 1;
    if !addressed || !counted {
        pinned.leak();
        return Err(if addressed {
            "a domain miscounted the pages pinned into it"
        } else {
            "an untranslated domain gave a device address other than the frame's"
        });
    }
    if !matches!(domain.pin(&[], MapFlags::DMA), Err(DomainError::Empty)) {
        pinned.leak();
        return Err("a domain pinned nothing");
    }
    report.refusals += 1;
    let waits = gate::waits_with_interrupts_on();
    if domain.unpin(pinned).is_err() {
        return Err("a domain refused its own pin");
    }
    if domain.pinned_pages() != before {
        return Err("a domain still counted pages it had unpinned");
    }
    if domain.translated() && expected.iter().any(|&phys| domain.resolve(phys).is_some()) {
        return Err("a translated domain still reached a page it had unpinned");
    }
    // The boot task may block, so the unpin's wait for its unit must have
    // been made with interrupts on.
    if domain.translated() && gate::waits_with_interrupts_on() == waits {
        return Err("a translated domain's unpin waited on its unit with interrupts masked");
    }
    report.waits = gate::waits_with_interrupts_on();
    report.translated = domain.translated();
    report.pinned += 2;
    Ok(())
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
         than a refused access, across {} translating units; {} late faults from the \
         out-of-domain probe",
        audit.stray, audit.stray_events, audit.units, audit.provoked,
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

/// The byte check R5 loops back once the line is converted.
const CONVERTED_BYTE: u8 = 0xF6;

/// Whether [`plant_masked_byte`] planted one.
static PLANTED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// G3, at bring-up, while the console's line is masked for its conversion,
/// unless the boot was told to skip its checks: loop a byte back into the
/// port, whose edge the I/O APIC drops, so that only the service once after
/// the conversion can read it ([`require_masked_byte_served`]). The receive
/// path is armed for it first, so that it keeps the byte for the check
/// rather than handing it to the console's reader.
pub(crate) fn plant_masked_byte(line: &super::LiveLine) {
    if !crate::checks::run() {
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
static MASKED_BYTE_SERVED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Check R5 (`docs/NVIDIA.md` §12.3), at stage 10 once the console's I/O
/// APIC line is converted: a byte looped back through the port is read by
/// the console's receive path through the line's interrupt, now delivered on its
/// new vector through its interrupt remapping entry -- its handler is there
/// alone -- and nothing arrives on the retired vector, which a compatibility
/// or stale KVM route would deliver on. `None` where no line was converted.
/// Only a delivery on the retired vector fails: a UART that raises no
/// interrupt in loopback (a PC's 16550 loops OUT2 internally) leaves nothing
/// to observe, and the line says so (condition 2 of the C6 review).
///
/// # Errors
///
/// A delivery came on the retired vector.
///
/// Verifies: `L.x86_64.130`, H.DMA.9
fn check_console_line() -> Result<Option<alloc::string::String>, &'static str> {
    let (Some((old, new)), Some(line)) = (super::console_vectors(), super::remapping::live_line())
    else {
        return Ok(None);
    };
    let retired = super::stray_deliveries().1;
    // The receive path keeps the byte for this check rather than putting it
    // in the ring, where the `console` thread could take and echo it first.
    crate::console::input::arm_check_byte(CONVERTED_BYTE);
    (line.loop_back)(CONVERTED_BYTE);
    let deadline = crate::timer::now_nanos().saturating_add(50_000_000);
    while crate::timer::now_nanos() <= deadline
        && !crate::console::input::check_byte_taken(CONVERTED_BYTE)
    {
        core::hint::spin_loop();
    }
    let arrived = crate::console::input::disarm_check_byte(CONVERTED_BYTE);
    if super::stray_deliveries().1 != retired {
        return Err("the console's line was delivered on its retired vector");
    }
    if !arrived {
        // A PC's 16550 loops OUT2 back internally in loopback mode, so its
        // interrupt line to the I/O APIC is not driven and nothing can
        // arrive on either vector (the consultant's condition 2): not a
        // failure, only not observable. The byte is taken out of the port,
        // or out of the ring if a late delivery put it there, so no reader
        // sees it. test-boot still requires the positive line on the
        // reference machine, whose UART raises its interrupt in loopback.
        let mut taken = crate::console::input::take_check_byte(CONVERTED_BYTE);
        for _ in 0..16 {
            if taken {
                break;
            }
            taken = crate::arch::take_console_byte().is_none_or(|byte| byte == CONVERTED_BYTE);
        }
        return Ok(Some(alloc::format!(
            "R5: the UART raised no interrupt in loopback; not observable here (none on the \
             retired vector {old:#x} either); {} byte received while the line was masked read \
             by the service after its conversion",
            u8::from(MASKED_BYTE_SERVED.load(core::sync::atomic::Ordering::Relaxed))
        )));
    }
    Ok(Some(alloc::format!(
        "the console's I/O APIC line delivered a looped-back byte on vector {new:#x} through its \
         interrupt entry and none on its retired {old:#x}; {} byte received while it was masked \
         read by the service after its conversion",
        u8::from(MASKED_BYTE_SERVED.load(core::sync::atomic::Ordering::Relaxed))
    )))
}

/// Check R10 (`docs/NVIDIA.md` §12.3, condition G5), through the system
/// call a driver and `devmgr` make, on QEMU's `edu`, which no driver takes,
/// once its interrupts are isolated: the isolated-interrupts mark set through a handle with
/// `SET_LIMIT`; `interrupt_create` allowed while the machine's interrupts
/// are isolated, and refused `ACCESS_DENIED` while the check forces them
/// not to be (`FORCED_UNISOLATED`, check-only); `device_isolation` saying
/// so each time; and a `device_set_limit` that would clear the mark
/// refused `ACCESS_DENIED`, the mark still set. `None` where no node's
/// interrupts are isolated. The node stays marked, as `devmgr` would leave
/// it.
///
/// # Errors
///
/// What did not hold, as a sentence.
///
/// Verifies: L.device.27, L.iommu.55, H.DMA.9
fn check_isolation_mark() -> Result<Option<alloc::string::String>, &'static str> {
    use ferrix_native_abi::nr;
    use ferrix_native_abi::rights::Rights;
    use ferrix_native_abi::status;
    use ferrix_native_abi::types::{
        DEVICE_ISOLATION_DMA_TRANSLATED, DEVICE_ISOLATION_INTERRUPTS,
        DEVICE_LIMIT_ISOLATED_INTERRUPTS,
    };

    let Some(node) = device::devices().iter().find(|node| {
        node.pci_function()
            .is_some_and(|function| (function.vendor, function.device) == (0x1234, 0x11E8))
            && node.interrupts_isolated()
            && node.vector_count() > 0
            && !node.isolated_marked()
    }) else {
        return Ok(None);
    };
    let side = object::check::Side::new()?;
    let outcome = (|| {
        let manager = side
            .process
            .with_handles(|table| {
                table.insert(
                    object::Object::Device(Arc::clone(node)),
                    Rights(Rights::DEVICE.0 | Rights::SET_LIMIT.0),
                )
            })
            .map_err(|_| "no room for a device handle")?;
        let reg = object::check::reg;
        let mark = |value: u64| {
            side.call(
                nr::DEVICE_SET_LIMIT,
                &[reg(manager), DEVICE_LIMIT_ISOLATED_INTERRUPTS, value],
            )
        };
        let isolation = || side.call(nr::DEVICE_ISOLATION, &[reg(manager)]);
        let take = || side.call(nr::INTERRUPT_CREATE, &[reg(manager), 0]);
        if mark(1) != Ok(0) {
            return Err("the isolated-interrupts mark could not be set");
        }
        let isolated = isolation().map_err(|_| "device_isolation refused")?;
        let interrupts = DEVICE_ISOLATION_INTERRUPTS as usize;
        if isolated & interrupts == 0 {
            return Err("device_isolation did not say the node's interrupts are isolated");
        }
        let interrupt = take().map_err(|_| "a marked node was refused a vector while isolated")?;
        let _ = side.call(nr::HANDLE_CLOSE, &[interrupt as u64]);
        super::remapping::FORCED_UNISOLATED.store(true, core::sync::atomic::Ordering::Release);
        let refused = take();
        let forced = isolation();
        super::remapping::FORCED_UNISOLATED.store(false, core::sync::atomic::Ordering::Release);
        if let Ok(interrupt) = refused {
            let _ = side.call(nr::HANDLE_CLOSE, &[interrupt as u64]);
            return Err(
                "a marked node was given a vector while the machine's interrupts were not isolated",
            );
        }
        if refused != Err(status::ACCESS_DENIED) {
            return Err("a marked node's vector was refused other than ACCESS_DENIED");
        }
        if forced.map(|bits| bits & interrupts) != Ok(0) {
            return Err(
                "device_isolation said a node's interrupts were isolated when they were not",
            );
        }
        if mark(0) != Err(status::ACCESS_DENIED) {
            return Err("the isolated-interrupts mark was cleared");
        }
        if side.call(
            nr::DEVICE_GET_LIMIT,
            &[reg(manager), DEVICE_LIMIT_ISOLATED_INTERRUPTS],
        ) != Ok(1)
        {
            return Err("the isolated-interrupts mark did not stay set");
        }
        Ok(alloc::format!(
            "the isolated-interrupts mark set on {}: device_isolation {isolated:#x} (DMA \
             translated {}, interrupts isolated), interrupt_create allowed while isolated and \
             refused ACCESS_DENIED with them forced not to be, its clearing refused \
             ACCESS_DENIED",
            node.location(),
            isolated & DEVICE_ISOLATION_DMA_TRANSLATED as usize != 0
        ))
    })();
    side.close_everything();
    outcome.map(Some)
}

/// `L.device.28`: while a unit remaps, a requester ID that no unit places as
/// its own -- here one on bus 0xFE, where nothing is enumerated -- is given
/// no route, so the function gets no vector. Every function QEMU's machines
/// have is placed, and none under a bridge's alias, so the refusal is
/// shown on a requester that cannot be; the aliased case takes the same
/// path (`vtd_unit_for` answers none). `None` where no unit remaps.
///
/// # Errors
///
/// The unplaced requester was given a route.
///
/// Verifies: L.device.28
fn check_no_route_unplaced() -> Result<Option<alloc::string::String>, &'static str> {
    if !super::remapping::any_remapping() {
        return Ok(None);
    }
    /// Bus 0xFE, device 31, function 7.
    const UNPLACED: u16 = 0xFEFF;
    if super::route(UNPLACED).is_ok() {
        return Err("a requester no unit places was given a message route");
    }
    let rewrites = check_entry_never_rewritten()?;
    Ok(Some(alloc::format!(
        "a requester no unit places ({UNPLACED:#x}) is given no message route, so no vector; \
         {rewrites} rewrite of a present interrupt entry refused"
    )))
}

/// `L.iommu.52`: an interrupt remapping entry in use is never rewritten.
/// The console line's entry, present since bring-up and shown delivering by
/// check R5 just before, is offered a rewrite on every unit that remaps,
/// aimed elsewhere, and each refuses it. Answers how many refused.
///
/// # Errors
///
/// A unit rewrote a present entry.
///
/// Verifies: L.iommu.52
fn check_entry_never_rewritten() -> Result<usize, &'static str> {
    let (Some((_, new)), Some(programmed)) = (super::console_vectors(), super::PROGRAMMED.get())
    else {
        return Ok(0);
    };
    let handle = u16::from(new.wrapping_sub(0x40));
    let elsewhere = ferrix_paging::vtd::remap::Remap {
        vector: super::CHECK_VECTOR,
        destination: 0,
        level: false,
        source: 0,
    };
    let mut refused = 0;
    for unit in programmed.vtd.iter().filter(|unit| unit.remapping()) {
        if unit.remap(handle, elsewhere).is_ok() {
            return Err("a present interrupt entry was rewritten");
        }
        refused += 1;
    }
    Ok(refused)
}
