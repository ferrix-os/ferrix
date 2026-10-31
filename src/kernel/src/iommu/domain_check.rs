//! The IOMMU's stage 10 check, [`check_iommu`]: a device's domain pins and
//! unpins as the rules say, the quarantine holds a dead driver's pins, the
//! queued invalidations wait as they must, and the interrupt remapping holds
//! (the console line, routes, and the isolated-interrupts mark a native
//! process sets), as stage 10 requires.
//!
//! Split from `check.rs` on 2026-10-15 (`docs/KERNEL-CRATES.md` §3 (3)): this
//! half drives native processes (`object::check::Side`, the pin budget's
//! checks), so it is raised above the core when the kernel is split into
//! crates; `check.rs` keeps what a bring-up calls and what needs nothing
//! above the core.

#![allow(
    clippy::disallowed_methods,
    reason = "a raised check: it calls methods made pub for it alone"
)]

use alloc::sync::Arc;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_paging::MapFlags;

use crate::device::{self, DeviceNode, Location};
use crate::panic::{catalog, fatal};
use crate::{mm, object, println};

use super::check::{FIRMWARE_QUEUES_STOPPED, MASKED_BYTE_SERVED};
use super::gate;
use super::{Domain, DomainError};

/// Stage 10: the ceiling on raised pin budgets, counted; then a device's
/// domain, and the budget and quarantine a driver's pins count against,
/// unless the boot was told to skip its checks.
///
/// Halts rather than returning, as every other stage's check does.
pub fn check_iommu() {
    if !crate::boot_checks::run() {
        return;
    }
    // First, while no function's domain is made but by bring-up's own.
    say_line(check_isolation_before_pin());
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
    say_line(check_cleaning(super::vtd::cleaning()));
    say_line(check_runs(device::devices()));
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
    say_line(check_exempt_pin());
    say_line(super::amdvi_check::check_tables(failed_kept));
    say_line(check_queue(super::invalidations(), failed_kept, planted));
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
    report_intel_gpu_budget();
    // Last, since it asks every node and so makes every domain not made
    // yet, as devmgr's admission of each device does next.
    say_line(check_no_dma().map(Some));
}

/// Say a stage 10 check's line, if it has one, or halt on its problem.
fn say_line(result: Result<Option<alloc::string::String>, impl core::fmt::Display>) {
    match result {
        Ok(Some(line)) => println!("  iommu    {line}"),
        Ok(None) => {}
        Err(problem) => fatal!(
            catalog::STAGE10_IOMMU,
            "stage 10 self-check failed: {problem}"
        ),
    }
}

/// The Intel integrated GPU's pin budget, checked and said
/// (`object::pin::check::check_intel_gpu_budget`).
fn report_intel_gpu_budget() {
    match object::pin::check::check_intel_gpu_budget() {
        Ok(done) => println!(
            "  iommu    Intel GPU pin budget: {} pages, a quarter of the {} usable frames above \
             the watermark of {} and at most {} (4 GiB), its pins held to twice it; a pin past \
             it refused ENOSPC and past twice it EDQUOT, a raise past it refused, {} refusals \
             in all; {} identities told apart",
            done.budget,
            done.usable,
            done.watermark,
            object::pin::INTEL_GPU_CAP_PAGES,
            done.refusals,
            done.identities,
        ),
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

/// D10's exempt functions -- an AMD-Vi unit's own, the root complex --
/// are refused a pin and bus mastering, and the refused pin leaves bus
/// mastering as enumeration found it: the domain is taken before bus
/// mastering goes on (ledger 4108 (1)). `None`
/// with no exempt function, as on every machine but an AMD-Vi one.
///
/// # Errors
///
/// What did not hold, as a sentence.
///
/// Verifies: L.iommu.99, L.iommu.100
fn check_exempt_pin() -> Result<Option<alloc::string::String>, &'static str> {
    use ferrix_native_abi::nr;
    use ferrix_native_abi::rights::Rights;

    let Some(node) = device::devices().iter().find(|node| {
        matches!(node.location(), Location::Pci(function) if super::amdvi_check::exempt(function))
    }) else {
        return Ok(None);
    };
    let side = object::check::Side::new()?;
    let outcome = (|| {
        let reg = object::check::reg;
        let manager = side
            .process
            .with_handles(|table| {
                table.insert(
                    object::Object::Device(Arc::clone(node)),
                    Rights(Rights::DEVICE.0 | Rights::MANAGE.0),
                )
            })
            .map_err(|_| "no room for a device handle")?;
        // The unit's own function may be a bus master as firmware found it
        // -- it fetches its tables -- and the kernel leaves it so: what it
        // must not do is turn it on.
        let before = super::amdvi_check::bus_master(node);
        let vmo = side.handle(
            nr::VMO_CREATE,
            &[PAGE_SIZE],
            "vmo_create for the exempt pin failed",
        )?;
        if side
            .call(nr::VMO_PIN, &[reg(manager), reg(vmo), 0, PAGE_SIZE, 0])
            .is_ok()
        {
            return Err("an exempt function was given a pin");
        }
        if super::amdvi_check::bus_master(node) != before || node.enable_dma().is_ok() {
            return Err("an exempt function was made a bus master");
        }
        Ok(alloc::format!(
            "{}, exempt under D10, was refused a pin and bus mastering, which stayed as \
             enumeration found it",
            node.location()
        ))
    })();
    side.close_everything();
    outcome.map(Some)
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

/// The 2 MiB run cases (`super::blocks_check`) on the first PCI function's
/// domain, as [`check_domains`] finds it.
fn check_runs(nodes: &[Arc<DeviceNode>]) -> Result<Option<alloc::string::String>, &'static str> {
    let Some(node) = nodes
        .iter()
        .find(|node| matches!(node.location(), Location::Pci(_)))
    else {
        return Ok(None);
    };
    let domain = node
        .domain()
        .map_err(|_| "no memory for a device's domain")?;
    super::blocks_check::check_runs(&domain)
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

/// The byte check R5 loops back once the line is converted.
const CONVERTED_BYTE: u8 = 0xF6;

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
/// Verifies: `L.x86_64.130`, H.DMA.9, L.iommu.93
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

/// `L.device.55`: `device_isolation` asked of a PCI function whose domain
/// no pin has made yet answers bit 0 for the domain the function is given,
/// as devmgr asks it of a GPU before its driver starts: the domain is made
/// by the answer, and bit 0 is set exactly when that domain is translated.
/// `None` where every function's domain is made already.
///
/// # Errors
///
/// The answer did not make the domain, or did not say how it is translated.
///
/// Verifies: L.device.55
fn check_isolation_before_pin() -> Result<Option<alloc::string::String>, &'static str> {
    use ferrix_native_abi::nr;
    use ferrix_native_abi::rights::Rights;
    use ferrix_native_abi::types::DEVICE_ISOLATION_DMA_TRANSLATED;

    let Some(node) = device::devices().iter().find(|node| {
        node.pci_function().is_some() && node.reaches_memory() && node.domain_made().is_none()
    }) else {
        return Ok(None);
    };
    let side = object::check::Side::new()?;
    let outcome = (|| {
        let handle = side
            .process
            .with_handles(|table| {
                table.insert(object::Object::Device(Arc::clone(node)), Rights::NONE)
            })
            .map_err(|_| "no room for a device handle")?;
        let bits = side
            .call(nr::DEVICE_ISOLATION, &[object::check::reg(handle)])
            .map_err(|_| "device_isolation refused")?;
        let said = bits & DEVICE_ISOLATION_DMA_TRANSLATED as usize != 0;
        let Some(domain) = node.domain_made() else {
            // No domain after the answer: either the node can be given none
            // -- aliased, unresolved, its identity domain held -- and the
            // answer must say untranslated, or the answer did not make it.
            if node.domain().is_ok() {
                return Err("device_isolation asked before any pin made no domain");
            }
            if said {
                return Err("device_isolation said DMA translated for a node given no domain");
            }
            return Ok(alloc::format!(
                "device_isolation asked of {} before any pin: {bits:#x}, a node that can be \
                 given no domain",
                node.location()
            ));
        };
        if said != domain.translated() {
            return Err(
                "device_isolation asked before any pin did not answer for the domain the \
                 device is given",
            );
        }
        Ok(alloc::format!(
            "device_isolation asked of {} before any pin: {bits:#x}, its domain made {}",
            node.location(),
            if said { "translated" } else { "untranslated" }
        ))
    })();
    side.close_everything();
    outcome.map(Some)
}

/// `L.device.57`: `device_isolation` answers `NO_DMA` (bit 2) exactly for
/// the nodes that reach no memory -- the firmware framebuffer's, the x86-64
/// power node -- asked of every node; and each such node has no domain and
/// is refused `vmo_pin` `WRONG_TYPE`, so devmgr's admission of it on bit 2
/// admits a node that can take no DMA at all.
///
/// # Errors
///
/// A node's answer had bit 2 where it reaches memory, or lacked it where it
/// does not; or such a node had a domain or was not refused its pin.
///
/// Verifies: L.device.57
fn check_no_dma() -> Result<alloc::string::String, &'static str> {
    use ferrix_native_abi::nr;
    use ferrix_native_abi::rights::Rights;
    use ferrix_native_abi::status;
    use ferrix_native_abi::types::DEVICE_ISOLATION_NO_DMA;

    let side = object::check::Side::new()?;
    let outcome = (|| {
        let (mut without, mut with) = (0_usize, 0_usize);
        for node in device::devices() {
            let handle = side
                .process
                .with_handles(|table| {
                    table.insert(object::Object::Device(Arc::clone(node)), Rights::MANAGE)
                })
                .map_err(|_| "no room for a device handle")?;
            let bits = side
                .call(nr::DEVICE_ISOLATION, &[object::check::reg(handle)])
                .map_err(|_| "device_isolation refused")?;
            let no_dma = bits & DEVICE_ISOLATION_NO_DMA as usize != 0;
            if no_dma == node.reaches_memory() {
                return Err(if no_dma {
                    "device_isolation said NO_DMA of a node that reaches memory"
                } else {
                    "device_isolation did not say NO_DMA of a node that reaches no memory"
                });
            }
            if node.reaches_memory() {
                with += 1;
                continue;
            }
            if node.domain_made().is_some() {
                return Err("a node that reaches no memory was given a domain");
            }
            // The pin is refused on the node before its VMO is looked at.
            let pin = side.call(
                nr::VMO_PIN,
                &[object::check::reg(handle), 0, 0, PAGE_SIZE, 0],
            );
            if pin != Err(status::WRONG_TYPE) {
                return Err("a node that reaches no memory was not refused vmo_pin WRONG_TYPE");
            }
            without += 1;
        }
        Ok(alloc::format!(
            "device_isolation's NO_DMA: {without} nodes reaching no memory answered bit 2, each \
             with no domain and refused vmo_pin WRONG_TYPE; {with} nodes that reach memory \
             answered without it"
        ))
    })();
    side.close_everything();
    outcome
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

/// The region [`check_rmrr_identity`] honoured on QEMU's unit, for
/// [`check_rmrr_frames`] to read again after the reclaim.
static CHECKED_RMRR: ferrix_sync::Once<ferrix_paging::vtd::rmrr::Identity> =
    ferrix_sync::Once::new();

/// Stage 10: an RMRR's identity domain, on the first translating VT-d unit,
/// unless the boot skips its checks (`vtd-design.md` §3.9(b)).
///
/// QEMU's DMAR has no RMRR, and the QEMU patch that would give it one is
/// deferred (a BACKLOG row), not unbuildable. So this makes the same
/// thing bring-up makes for a real RMRR, through the same rules and the
/// same `attach_identity` and claim, over a synthetic region -- the two
/// pages below 2^39, which no QEMU memory map reaches -- for a requester
/// no device has (ff:1f.7), on a unit already translating. It shows the
/// kernel's own tables and rules; it is not device DMA into an RMRR, and
/// identity-before-TE on a real one is the N150 box's row (`L.iommu.73`).
///
/// Halts on a failure, as every other stage's check does.
///
/// Verifies: L.iommu.73
/// Verifies: L.iommu.74
/// Verifies: L.iommu.75
/// Verifies: L.iommu.76
/// Verifies: L.iommu.77
/// Verifies: L.iommu.78
pub fn check_rmrr_identity(memory: &[ferrix_bootinfo::MemRegion]) {
    if !crate::boot_checks::run() {
        return;
    }
    match rmrr_identity(memory) {
        Ok(Some(line)) => println!("  iommu    {line}"),
        Ok(None) => {}
        Err(problem) => fatal!(
            catalog::STAGE10_IOMMU,
            "stage 10 self-check failed: {problem}"
        ),
    }
}

/// [`check_rmrr_identity`]'s steps; `None` on a machine with no translating
/// VT-d unit.
fn rmrr_identity(
    memory: &[ferrix_bootinfo::MemRegion],
) -> Result<Option<alloc::string::String>, &'static str> {
    use ferrix_paging::vtd::rmrr;

    let Some(programmed) = super::PROGRAMMED.get() else {
        return Ok(None);
    };
    let Some(unit) = programmed.vtd.iter().find(|unit| unit.translating()) else {
        return Ok(None);
    };
    rmrr_over_the_allocator_refused(memory)?;

    // The synthetic RMRR, honoured and attached as bring-up does.
    let reach = 1_u64 << ferrix_paging::vtd::unit::DOMAIN_BITS;
    let region = rmrr::identity(reach - 2 * rmrr::PAGE, reach - 1, ram(memory))
        .map_err(|_| "the check's RMRR, the two pages below 2^39, was refused")?;
    if super::frames_outside_allocator(region).is_err() {
        return Err("the check's RMRR holds a frame the allocator holds");
    }
    let function = ferrix_pci::Address::new(0, 0xFF, 0x1F, 7).ok_or("no address ff:1f.7")?;
    let attached = unit.attach_identity(function, &[region])?;
    let _ = CHECKED_RMRR.call_once(|| region);
    // FATAL-ALLOC: boot only: the check's identity domain is made once and, as every identity domain, kept for the boot.
    let regions = alloc::vec![region];
    // FATAL-ALLOC: boot only: the check's identity domain is made once and, as every identity domain, kept for the boot.
    let identity: &'static super::IdentityDomain = alloc::boxed::Box::leak(alloc::boxed::Box::new(
        super::IdentityDomain::new(function, attached, regions),
    ));
    let domain = identity
        .claim(unit)
        .ok_or("the check's identity domain could not be claimed")?;

    // Exactly the RMRR's pages, each at its own address (`L.iommu.73`).
    if !exact(&domain, region) {
        return Err("an RMRR identity domain does not map exactly the RMRR's pages");
    }
    // Never handed out twice while it lives (`L.iommu.78`).
    if !identity.claimed() || identity.claim(unit).is_some() {
        return Err("an RMRR identity domain was handed out twice");
    }
    rmrr_pins_refused(&domain, region)?;
    rmrr_unpin_refused(&domain, region)?;
    rmrr_never_detached(identity, unit, domain, region)?;

    // Its frames are still outside the allocator (`L.iommu.75`).
    if super::frames_outside_allocator(region).is_err() {
        return Err("an RMRR frame became the allocator's");
    }
    Ok(Some(alloc::format!(
        "RMRR identity: {} pages mapped exactly, at their own addresses, for an absent \
         requester on QEMU's unit; pins over them and an unpin naming them refused, a page \
         beside them pinned and unpinned, never detached, never claimed twice, and RMRRs over \
         RAM or an allocator's frame refused",
        region.pages
    )))
}

/// Every region the memory map calls RAM, as `(base, len)`.
fn ram(memory: &[ferrix_bootinfo::MemRegion]) -> impl Iterator<Item = (u64, u64)> + '_ {
    memory
        .iter()
        .filter(|region| region.kind.is_ram())
        .map(|region| (region.base, region.len))
}

/// Whether `domain` maps exactly `region`'s pages, each at its own address:
/// the page below it unmapped (the one above is past 2^39).
fn exact(domain: &Domain, region: ferrix_paging::vtd::rmrr::Identity) -> bool {
    region
        .page_addresses()
        .all(|page| domain.resolve(page) == Some(page))
        && domain
            .resolve(region.base - ferrix_paging::vtd::rmrr::PAGE)
            .is_none()
}

/// The rules refuse an RMRR over a frame the allocator holds: by the
/// memory map, and by the frame's own state (`L.iommu.74`, `L.iommu.75`).
fn rmrr_over_the_allocator_refused(
    memory: &[ferrix_bootinfo::MemRegion],
) -> Result<(), &'static str> {
    use ferrix_paging::vtd::rmrr::{self, Identity};

    let taken = mm::allocate_frames(0).ok_or("no frame for the RMRR check")?;
    let over = Identity {
        base: taken * PAGE_SIZE,
        pages: 1,
    };
    let by_state = super::frames_outside_allocator(over);
    let by_map = rmrr::identity(over.base, over.end() - 1, ram(memory));
    mm::deallocate_frames(taken, 0);
    if by_state != Err(rmrr::Refusal::AllocatorFrame { page: over.base }) {
        return Err("an RMRR over a frame the allocator holds was not refused");
    }
    if !matches!(by_map, Err(rmrr::Refusal::OverRam { .. })) {
        return Err("an RMRR over RAM the memory map gives was not refused");
    }
    Ok(())
}

/// A pin over an RMRR page is refused, with nothing mapped; any other page
/// pins and unpins as into any domain, a driver's (`L.iommu.76`).
fn rmrr_pins_refused(
    domain: &Domain,
    region: ferrix_paging::vtd::rmrr::Identity,
) -> Result<(), &'static str> {
    let rmrr_frame = region.base / PAGE_SIZE;
    if !matches!(
        domain.pin(&[rmrr_frame], MapFlags::DMA),
        Err(DomainError::Reserved)
    ) {
        return Err("a pin over an RMRR page was not refused");
    }
    let spare = mm::allocate_frames(0).ok_or("no frame for the RMRR check")?;
    let beside = spare * PAGE_SIZE;
    match domain.pin(&[spare, rmrr_frame], MapFlags::DMA) {
        Err(DomainError::Reserved) => {}
        Ok(pinned) => {
            pinned.leak();
            return Err("a pin with an RMRR page in it was not refused");
        }
        Err(_) => return Err("a pin with an RMRR page in it was refused for another reason"),
    }
    if domain.resolve(beside).is_some() || domain.pinned_pages() != 0 {
        return Err("a refused pin over an RMRR mapped a page beside it");
    }
    let pinned = domain
        .pin(&[spare], MapFlags::DMA)
        .map_err(|_| "a page beside the RMRR could not be pinned into its domain")?;
    if pinned.addresses() != [beside] || domain.resolve(beside) != Some(beside) {
        pinned.leak();
        return Err("a page pinned beside the RMRR is not at its own address");
    }
    if let Err((_, pinned)) = domain.unpin(pinned) {
        pinned.leak();
        return Err("a page pinned beside the RMRR could not be unpinned");
    }
    mm::deallocate_frames(spare, 0);
    Ok(())
}

/// An unpin naming an RMRR page is refused, and takes nothing out
/// (`L.iommu.77`).
fn rmrr_unpin_refused(
    domain: &Domain,
    region: ferrix_paging::vtd::rmrr::Identity,
) -> Result<(), &'static str> {
    let forged = super::Pinned {
        domain: domain.id(),
        // FATAL-ALLOC: boot only: one address, for the check's forged pin.
        addresses: alloc::vec![region.base],
    };
    match domain.unpin(forged) {
        Err((DomainError::Reserved, back)) => back.leak(),
        Err((_, back)) => {
            back.leak();
            return Err("an unpin naming an RMRR page was refused for another reason");
        }
        Ok(()) => return Err("an unpin naming an RMRR page was not refused"),
    }
    if !exact(domain, region) {
        return Err("an unpin took an RMRR page out of its identity domain");
    }
    Ok(())
}

/// Never detached: not when asked, not when dropped; the claim comes back,
/// and the same tables with it (`L.iommu.78`).
fn rmrr_never_detached(
    identity: &'static super::IdentityDomain,
    unit: &'static super::vtd::Unit,
    domain: Domain,
    region: ferrix_paging::vtd::rmrr::Identity,
) -> Result<(), &'static str> {
    if domain.detach_translation().is_ok() {
        return Err("an RMRR identity domain was detached");
    }
    drop(domain);
    let again = identity
        .claim(unit)
        .ok_or("a dropped RMRR identity domain did not give its claim back")?;
    if !exact(&again, region) {
        return Err("a dropped RMRR identity domain was detached");
    }
    Ok(())
}

/// After the reclaim: every frame an RMRR identity domain holds -- bring-up's,
/// and [`check_rmrr_identity`]'s -- is still outside the frame allocator, so
/// the reclaim gave none of them to it (`L.iommu.75`). Unless the boot skips
/// its checks.
///
/// Verifies: L.iommu.75
pub fn check_rmrr_frames() {
    if !crate::boot_checks::run() {
        return;
    }
    let mut pages = 0;
    for region in super::identity_regions().chain(CHECKED_RMRR.get().copied()) {
        if let Err(refusal) = super::frames_outside_allocator(region) {
            fatal!(
                catalog::STAGE10_IOMMU,
                "stage 10 self-check failed: an RMRR identity domain's frame is the \
                 allocator's after the reclaim: {refusal}"
            );
        }
        pages += region.pages;
    }
    if pages != 0 {
        println!("  iommu    RMRR frames after the reclaim: {pages} pages, none the allocator's");
    }
}
