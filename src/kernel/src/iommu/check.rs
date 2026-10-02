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

/// Stage 10: a device's domain, and the quarantine a dead driver's pins go to,
/// unless the boot was told to skip its checks.
///
/// Halts rather than returning, as every other stage's check does.
pub(crate) fn check_iommu() {
    if !crate::checks::run() {
        return;
    }
    let domains = match check_domains(device::devices()) {
        Ok(report) => report,
        Err(problem) => fatal!(
            catalog::STAGE10_IOMMU,
            "stage 10 self-check failed: {problem}"
        ),
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
    match object::pin::check::check_quarantine(device::devices()) {
        Ok(true) => println!(
            "  iommu    a dead driver's pin was quarantined, a pin past the quarantine's cap \
             refused and one taken again once it was released, a live driver's given back"
        ),
        Ok(false) => {}
        Err(problem) => fatal!(
            catalog::STAGE10_IOMMU,
            "stage 10 self-check failed: {problem}"
        ),
    }
}

/// Every table write on a VT-d unit whose walk does not snoop was cleaned to
/// memory before it was published (finding F-58): no publish point --
/// invalidation, or the end of a change -- found a write noted and not
/// cleaned, and the boot's domains made some to clean. QEMU's unit reports
/// `ECAP.C` clear, so this runs on every x86-64 boot test.
///
/// `None` on a machine with no such unit.
///
/// # Errors
///
/// A publish point that found a write not cleaned, or a unit that cleaned
/// nothing over a boot that attached and pinned through it.
///
/// Verifies: L.iommu.56
/// Verifies: L.iommu.57
fn check_cleaning(
    cleaning: super::vtd::Cleaning,
) -> Result<Option<alloc::string::String>, alloc::string::String> {
    if cleaning.uncleaned != 0 {
        return Err(alloc::format!(
            "{} of {} VT-d publish points found a table write not cleaned to memory",
            cleaning.uncleaned,
            cleaning.checked
        ));
    }
    if cleaning.units == 0 {
        return Ok(None);
    }
    if cleaning.entries == 0 || cleaning.tables == 0 || cleaning.checked == 0 {
        return Err(alloc::format!(
            "a VT-d unit that does not snoop cleaned {} entries and {} tables over {} publish \
             points",
            cleaning.entries,
            cleaning.tables,
            cleaning.checked
        ));
    }
    Ok(Some(alloc::format!(
        "{} entry writes and {} fresh tables cleaned to memory on {} VT-d units that do not \
         snoop, {} publish points found none left uncleaned",
        cleaning.entries,
        cleaning.tables,
        cleaning.units,
        cleaning.checked
    )))
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

/// Stage 10: no IOMMU recorded a fault that no check provoked. Answers
/// whether any unit translates, for the audit record's end-of-boot check,
/// which runs after this since reading the faults is what records them.
///
/// Halts rather than returning, as every other stage's check does.
///
/// Verifies: H.DMA.6
pub(crate) fn check_dma_faults() -> bool {
    let audit = super::audit_faults();
    println!(
        "  iommu    {} DMA faults recorded that no check provoked, {} of them unit events other \
         than a refused access, across {} translating units; {} late faults from the \
         out-of-domain probe",
        audit.stray, audit.stray_events, audit.units, audit.provoked,
    );
    if audit.stray == 0 {
        return audit.units > 0;
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
