//! Checks of the small services the rest of the item leans on, once the
//! scheduler runs: a registration list, a device's claim and the numbers its
//! node is published under, the boot-mode word a `reboot(2)` passes on, and
//! the sentences the interrupt table, init and the user copy layer report
//! failures with.
//!
//! Each is a handful of lines that the boot reaches only in its passing
//! shape: every hook list has room, no device is quiesced under a live
//! driver, the machines QEMU presents keep no boot mode, and nothing fails to
//! register. What they do in the other shape is what a caller relies on when
//! it happens, so it is driven here, with values of the check's own that
//! touch nothing the running machine uses -- a local list, a local set of
//! claims, a channel made for the purpose.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;

use ferrix_linux_abi::errno::Errno;
use ferrix_sched_fair::NICE_0_WEIGHT;

use crate::claim::{Claims, Numbers, StillServed};
use crate::device::{DeviceNode, Location};
use crate::discovery::devmgr::{self, Request};
use crate::hooks::{Full, Hooks};
use crate::init::Failure;
use crate::iommu::{Cause, Domain, Fault};
use crate::irq::IrqError;
use crate::object::channel::Endpoint;
use crate::sched::{self, Task};
use crate::sync::{SchedParker, SpinLock};
use crate::syscall::uaccess::UserError;

/// What the checks did, for the boot log.
#[derive(Debug, Default)]
pub struct Report {
    /// Registrations a list took, and the one it refused.
    pub registered: usize,
    /// Refusals asserted: a full list, a claim held twice, a quiesce under a
    /// live driver and one cancelled, a boot mode where none is kept.
    pub refusals: usize,
    /// Failure sentences read back word for word.
    pub sentences: usize,
}

/// Run every check.
///
/// # Errors
///
/// The first property that did not hold, as a sentence.
///
/// Verifies: H.DEV.1
pub fn run() -> Result<Report, &'static str> {
    let mut report = Report::default();
    a_list_keeps_its_order_and_its_bound(&mut report)?;
    a_claim_is_refused_while_its_driver_lives(&mut report)?;
    a_shared_node_waits_for_every_claim(&mut report)?;
    a_wait_ends_with_its_release_or_its_patience(&mut report)?;
    a_claim_or_a_number_without_memory_is_refused(&mut report)?;
    a_device_failure_names_its_node(&mut report)?;
    a_number_given_back_is_the_next_taken()?;
    a_boot_mode_nobody_keeps_is_refused(&mut report)?;
    failures_read_as_they_are_documented(&mut report)?;
    faults_and_domains_read_as_recorded(&mut report)?;
    devmgr_refuses_what_a_sysfs_write_is_refused(&mut report)?;
    a_write_that_may_not_wait_is_queued();
    an_unanswered_call_is_reported_by_name();
    the_last_line_is_kept_for_a_failure_report()?;
    crate::iommu::check_gate()?;
    crate::syscall::program_check::run()?;
    report.refusals += 2;
    Ok(report)
}

/// A [`Hooks`] list gives its registrations back in the order they were
/// made, counts them, and refuses one past its bound with [`Full`] rather
/// than dropping it or overwriting the first.
fn a_list_keeps_its_order_and_its_bound(report: &mut Report) -> Result<(), &'static str> {
    static LIST: Hooks<u32, 2> = Hooks::new();
    static FIRST: u32 = 1;
    static SECOND: u32 = 2;
    static THIRD: u32 = 3;

    if LIST.register(&FIRST).is_err() || LIST.register(&SECOND).is_err() {
        return Err("a registration list with room refused a registration");
    }
    if LIST.register(&THIRD) != Err(Full) {
        return Err("a full registration list took one more");
    }
    let mut order = LIST.iter();
    if order.next() != Some(&FIRST) || order.next() != Some(&SECOND) || order.next().is_some() {
        return Err("a registration list did not give back what it took, in order");
    }
    if LIST.len() != 2 {
        return Err("a registration list miscounted what it holds");
    }
    report.registered += LIST.len();
    report.refusals += 1;
    Ok(())
}

/// A device claimed through a channel whose driver end is open refuses a
/// quiesce outright; once that end has closed, a quiesce waits for the claim
/// to go, and a caller cancelled meanwhile is told it is still waiting. A
/// second claim of the same device is refused while the first stands.
///
/// On a node the boot published and a set of claims of this check's own, so
/// the display and render cores' claims are untouched.
///
/// Verifies: L.claim.1, L.claim.5
fn a_claim_is_refused_while_its_driver_lives(report: &mut Report) -> Result<(), &'static str> {
    let Some(node) = crate::device::devices().first() else {
        // No device to claim: nothing a quiesce could be asked about either.
        return Ok(());
    };
    let claims = Claims::new();
    let (core, driver) = Endpoint::pair().map_err(|_| "no memory for a claim's channel")?;

    if !claims.claim(node, &core) {
        return Err("a device nobody holds could not be claimed");
    }
    if claims.claim(node, &core) {
        return Err("a device was claimed twice");
    }
    if claims.wait_until_released(node, &|| false) != Err(StillServed::ByADriver) {
        return Err("a device was quiesced under a driver that still held it");
    }

    // The driver goes: its end closes, and the core's end hears it.
    drop(driver);
    if claims.wait_until_released(node, &|| true) != Err(StillServed::Waiting) {
        return Err("a cancelled quiesce was not told the device is still claimed");
    }

    claims.release(node);
    if claims.wait_until_released(node, &|| false) != Ok(()) {
        return Err("a released device was still waited for");
    }
    report.refusals += 3;
    Ok(())
}

/// A node claimed through several channels -- a USB host's keyboard and
/// mouse -- takes no more than its limit, refuses a quiesce while any one
/// driver end is open, and is free only once every claim has gone; letting
/// one claim go leaves the other standing.
///
/// Verifies: L.claim.2, L.claim.3, L.claim.7
fn a_shared_node_waits_for_every_claim(report: &mut Report) -> Result<(), &'static str> {
    let Some(node) = crate::device::devices().first() else {
        return Ok(());
    };
    let claims = Claims::new();
    let (first, first_driver) = Endpoint::pair().map_err(|_| "no memory for a claim's channel")?;
    let (second, second_driver) =
        Endpoint::pair().map_err(|_| "no memory for a claim's channel")?;

    if !claims.claim_up_to(node, &first, 2) || !claims.claim_up_to(node, &second, 2) {
        return Err("a device could not be claimed up to its limit");
    }
    if claims.claim_up_to(node, &first, 2) {
        return Err("a device was claimed past its limit");
    }

    // One driver goes; the other still serves the node.
    drop(first_driver);
    if claims.wait_until_released(node, &|| false) != Err(StillServed::ByADriver) {
        return Err("a device was quiesced under one of its drivers");
    }
    claims.release_one(node, &first);
    if claims.wait_until_released(node, &|| true) != Err(StillServed::ByADriver) {
        return Err("letting one claim go let the node's other go too");
    }

    drop(second_driver);
    claims.release_one(node, &second);
    if claims.wait_until_released(node, &|| false) != Ok(()) {
        return Err("a node was still waited for after its last claim went");
    }
    report.refusals += 3;
    Ok(())
}

/// The claims the parked-wait check waits on, and the node and task it
/// waits with: statics, since the task that releases is started with a word.
static PARKED_CLAIMS: Claims = Claims::new();
/// The node claimed and the task waiting for it, for [`release_when_parked`].
static PARKED: SpinLock<Option<(Arc<DeviceNode>, Arc<Task>)>> = SpinLock::new(None);

/// Wait until the waiter [`PARKED`] names is blocked -- parked in its wait,
/// not merely descheduled -- and then release the node it waits for.
fn release_when_parked(_argument: usize) {
    loop {
        let parked = PARKED.lock().clone();
        let Some((node, waiter)) = parked else {
            return;
        };
        if waiter.is_blocked() {
            PARKED_CLAIMS.release(&node);
            return;
        }
        sched::yield_now();
    }
}

/// A wait for a claim whose driver has gone ends `Ok` once the claim is let
/// go, whichever comes first -- the release before the wait begins, or while
/// the wait is parked -- and answers `Waiting` when its patience runs out
/// first.
///
/// The parked case is a handshake, not a sleep: a task on this processor
/// releases only once the waiter is blocked in its wait. The patience is the
/// check's own, ten milliseconds, where the product waits five seconds.
///
/// Verifies: L.claim.4, L.claim.6
fn a_wait_ends_with_its_release_or_its_patience(report: &mut Report) -> Result<(), &'static str> {
    const SHORT_NANOS: u64 = 10_000_000;
    const LONG_NANOS: u64 = 10_000_000_000;
    let Some(node) = crate::device::devices().first() else {
        return Ok(());
    };
    let claim = |claims: &Claims| -> Result<(), &'static str> {
        let (core, driver) = Endpoint::pair().map_err(|_| "no memory for a claim's channel")?;
        if !claims.claim(node, &core) {
            return Err("a device nobody holds could not be claimed");
        }
        // The driver goes: what is left is waiting for the core to let go.
        drop(driver);
        Ok(())
    };

    // Its patience runs out first.
    let claims = Claims::new();
    claim(&claims)?;
    if claims.wait_within(node, &|| false, SHORT_NANOS) != Err(StillServed::Waiting) {
        return Err("a wait past its patience was not told the device is still claimed");
    }
    // The release comes before the wait begins.
    claims.release(node);
    if claims.wait_within(node, &|| false, SHORT_NANOS) != Ok(()) {
        return Err("a wait begun after its claim's release did not end at once");
    }

    // The release comes while the wait is parked.
    claim(&PARKED_CLAIMS)?;
    let waiter = sched::current().ok_or("the checking task is not running")?;
    *PARKED.lock() = Some((Arc::clone(node), waiter));
    let releaser = sched::spawn("parked", release_when_parked, 0, NICE_0_WEIGHT)?;
    let waited = PARKED_CLAIMS.wait_within(node, &|| false, LONG_NANOS);
    *PARKED.lock() = None;
    sched::wait_until_gone(&releaser, LONG_NANOS)?;
    drop(releaser);
    PARKED_CLAIMS.release(node);
    if waited != Ok(()) {
        return Err("a wait parked for its claim did not end when the claim was released");
    }
    report.refusals += 1;
    Ok(())
}

/// A claim, or a node's number, with no memory to record it is refused --
/// the claim as a device in use, the number as none -- and records nothing,
/// so the next asked with memory is given.
///
/// Verifies: L.claim.8
fn a_claim_or_a_number_without_memory_is_refused(report: &mut Report) -> Result<(), &'static str> {
    let me = sched::current_id().ok_or("the checking task is not running")?;
    if let Some(node) = crate::device::devices().first() {
        let claims = Claims::new();
        let (core, _driver) = Endpoint::pair().map_err(|_| "no memory for a claim's channel")?;
        crate::fallible::inject_once(me, 1, false);
        let claimed = claims.claim(node, &core);
        let refused = crate::fallible::stop_injecting();
        if claimed || refused != 1 {
            return Err("a claim with no memory to record it was not refused");
        }
        if !claims.claim(node, &core) {
            return Err("a claim refused for memory was recorded");
        }
        claims.release(node);
        report.refusals += 1;
    }
    let numbers = Numbers::new(3);
    crate::fallible::inject_once(me, 1, false);
    let taken = numbers.take();
    let refused = crate::fallible::stop_injecting();
    if taken.is_some() || refused != 1 {
        return Err("a node's number with no memory to hold it was not refused");
    }
    if numbers.take() != Some(3) {
        return Err("a number refused for memory was held");
    }
    report.refusals += 1;
    Ok(())
}

/// A device node that broke its rule is named in the sentence the boot
/// stops with, whatever kind of node it is: a PCI function by its address,
/// a device tree node by its registers.
///
/// Verifies: L.device.11
fn a_device_failure_names_its_node(report: &mut Report) -> Result<(), &'static str> {
    let say = |location| {
        format!(
            "{}",
            crate::device::Failure {
                location,
                what: "it broke its rule",
            }
        )
    };
    let pci = crate::device::devices()
        .iter()
        .map(|node| node.location())
        .find(|location| matches!(location, Location::Pci(_)));
    if let Some(location @ Location::Pci(address)) = pci
        && say(location) != format!("pci {address}: it broke its rule")
    {
        return Err("a PCI function's failure did not name its address");
    }
    if say(Location::VirtioMmio(0xa00_0200)) != "virtio,mmio@0xa000200: it broke its rule"
        || say(Location::Tree(0x5a00_1000)) != "tree@0x5a001000: it broke its rule"
    {
        return Err("a device tree node's failure did not name its registers");
    }
    report.sentences += 1;
    Ok(())
}

/// A number given back is the lowest free one, and so the next taken: the
/// property that makes a driver started again `card0` rather than `card1`.
///
/// Verifies: L.claim.9
fn a_number_given_back_is_the_next_taken() -> Result<(), &'static str> {
    let numbers = Numbers::new(7);
    let (Some(first), Some(second)) = (numbers.take(), numbers.take()) else {
        return Err("no memory to hold a node's number");
    };
    if (first, second) != (7, 8) {
        return Err("a node's number was not the lowest free one");
    }
    numbers.give_back(first);
    if numbers.take() != Some(first) {
        return Err("a number given back was not the next one taken");
    }
    Ok(())
}

/// A `reboot(2)` word no firmware keeps is refused, and nothing is written:
/// on a machine without a boot context -- every one QEMU presents -- because
/// it keeps none, and on an STM32MP15 board because U-Boot has no mode of
/// that name.
fn a_boot_mode_nobody_keeps_is_refused(report: &mut Report) -> Result<(), &'static str> {
    match crate::power::request_boot_mode("no-such-mode") {
        Err("this machine keeps no boot mode" | "U-Boot has no boot mode of that name") => {}
        _ => return Err("a boot mode no firmware keeps was not refused as documented"),
    }
    report.refusals += 1;
    Ok(())
}

/// The failures the interrupt table, init and the copy layer report read as
/// their documentation says, and the parker a sleeping lock is lent prints
/// and defaults as the lock's own report expects.
fn failures_read_as_they_are_documented(report: &mut Report) -> Result<(), &'static str> {
    let expected: [(String, &str); 7] = [
        (
            format!("{}", IrqError::OutOfRange(4096)),
            "interrupt 4096 is outside the table",
        ),
        (
            format!("{}", IrqError::AlreadyTaken(33)),
            "interrupt 33 already has a handler",
        ),
        (
            format!("{}", Failure::Linker(Errno::ENOENT)),
            "its linker: errno 2",
        ),
        (
            format!("{}", Failure::Exec(String::from("not an executable"))),
            "not an executable",
        ),
        (format!("{:?}", UserError::NotUserRange), "NotUserRange"),
        (format!("{:?}", UserError::NoMemory), "NoMemory"),
        (format!("{SchedParker:?}"), "SchedParker"),
    ];
    for (said, wanted) in &expected {
        if said != wanted {
            return Err("a failure does not read as its documentation says");
        }
    }
    report.sentences += expected.len();
    Ok(())
}

/// A DMA fault reads as its unit recorded it, whichever of the five causes it
/// was, and a domain prints as what translates it: the words the boot's fault
/// audit and a failed domain check put in the log.
///
/// Verifies: L.iommu.13, L.iommu.34
fn faults_and_domains_read_as_recorded(report: &mut Report) -> Result<(), &'static str> {
    let fault = |write, cause| Fault {
        stream: 0x10,
        page: 0x1000,
        write,
        cause,
    };
    let expected: [(String, &str); 6] = [
        (
            format!("{}", fault(false, Cause::Access)),
            "stream 0x10, page 0x1000, a read",
        ),
        (
            format!("{}", fault(true, Cause::Access)),
            "stream 0x10, page 0x1000, a write",
        ),
        (
            format!("{}", fault(false, Cause::Lost)),
            "an SMMUv3 event queue overflowed: events were lost",
        ),
        (
            format!("{}", fault(false, Cause::Overflow)),
            "a VT-d fault was lost to a full record, which held stream 0x10, page 0x1000",
        ),
        (
            format!("{}", fault(false, Cause::Queue)),
            "a VT-d unit's invalidation queue stopped, and the unit was marked failed",
        ),
        (format!("{:?}", Cause::Event(0x10)), "Event(16)"),
    ];
    for (said, wanted) in &expected {
        if said != wanted {
            return Err("a DMA fault does not read as its unit recorded it");
        }
    }
    let event = format!("{}", fault(false, Cause::Event(0xFF)));
    let known = format!("{}", fault(false, Cause::Event(0x10)));
    if event != "stream 0x10, SMMUv3 event 0xff (reserved)"
        || known != "stream 0x10, SMMUv3 event 0x10 (F_TRANSLATION)"
    {
        return Err("an SMMUv3 event does not read as its type");
    }

    let untranslated = Domain::untranslated();
    if !format!("{untranslated:?}").starts_with("Domain {") {
        return Err("an untranslated domain does not print as one");
    }
    if untranslated.take_fault().is_some() {
        return Err("a domain no unit translates reported a fault");
    }
    // The domain the stage 10 check pinned through, translated where a unit
    // is: its print names the unit's kind.
    if let Some(node) = crate::device::devices()
        .iter()
        .find(|node| matches!(node.location(), Location::Pci(_)))
    {
        let domain = node
            .domain()
            .map_err(|_| "no memory for a device's domain")?;
        let printed = format!("{domain:?}");
        let names = [
            "translation: None",
            "translation: VtD",
            "translation: SmmuV3",
            "translation: AmdVi",
        ];
        if !names.iter().any(|name| printed.contains(name)) {
            return Err("a device's domain does not print what translates it");
        }
        // A function whose unit already gives it a domain is refused a second
        // one there, and is told so with an untranslated domain rather than
        // two domains translating one device.
        if domain.translated()
            && let Location::Pci(function) = node.location()
        {
            if crate::iommu::domain_for(function).is_some_and(|domain| domain.translated()) {
                return Err("a function was given a second translated domain");
            }
            report.refusals += 1;
        }
    }
    a_dropped_domain_gives_its_stream_back()?;
    report.sentences += expected.len() + 1;
    Ok(())
}

/// `devmgr` refuses a request naming a device that does not exist with
/// `ENODEV`, and one to bind a device to the driver that already drives it
/// with `EBUSY`: what a write to sysfs's `bind` reports to a program. Neither
/// changes which driver drives what.
fn devmgr_refuses_what_a_sysfs_write_is_refused(report: &mut Report) -> Result<(), &'static str> {
    let devices = crate::device::devices();
    if devmgr::request(Request::Bind, devices.len().saturating_add(1), 0) != Err(Errno::ENODEV) {
        return Err("devmgr was asked to bind a device that does not exist, and did not refuse");
    }
    report.refusals += 1;
    let bound = (0..devices.len()).find_map(|device| Some((device, devmgr::driver_of(device)?.0)));
    if let Some((device, driver)) = bound {
        if devmgr::request(Request::Bind, device, driver) != Err(Errno::EBUSY) {
            return Err("devmgr bound a device to the driver that already drives it");
        }
        report.refusals += 1;
        // Its driver is running, so nothing says how it ended.
        let ended = devices
            .get(device)
            .and_then(|node| devmgr::location_of(node))
            .and_then(devmgr::driver_ending);
        if ended.is_some() {
            return Err("a running driver was reported as having ended");
        }
    }
    Ok(())
}

/// A program's bytes written where the writer may not wait -- here with
/// preemption held off by a spin lock, as an interrupt handler's would be --
/// are queued for the port without waiting for room. The line is the proof
/// that they went out.
fn a_write_that_may_not_wait_is_queued() {
    static HELD: SpinLock<()> = SpinLock::new(());
    let _held = HELD.lock();
    crate::console::write_bytes(
        b"  console  a program's bytes written where the writer may not wait\n",
    );
}

/// A call a program makes that nothing answers is named on the console while
/// init has asked for the report, and not once the report is turned off: the
/// line after this check's own says which call and number it was.
fn an_unanswered_call_is_reported_by_name() {
    crate::console::println!(
        "  services the next line is an ENOSYS report asked for by the check, not a program's"
    );
    let call = ferrix_linux_abi::nr::Syscall::Acct;
    let number = (0..=600)
        .find(|&number| crate::arch::decode_syscall(number) == Some(call))
        .unwrap_or_default();
    crate::syscall::report_unanswered(1);
    // The second is past the one line asked for, and is not printed.
    crate::syscall::unanswered(Some(call), number);
    crate::syscall::unanswered(Some(call), number);
    crate::syscall::report_unanswered(0);
    // And a tab, which the screen console, when it is drawn, advances to the
    // next stop of eight rather than drawing.
    crate::console::println!("  services\ta tab in a console line");
}

/// The recent-output ring a failure report and the screen console read back
/// holds the last line printed, as it was printed.
///
/// Verifies: L.console.17
fn the_last_line_is_kept_for_a_failure_report() -> Result<(), &'static str> {
    const LINE: &str = "  services the console keeps this line for a failure report\n";
    crate::console::println!("{}", LINE.trim_end());
    // A kilobyte back, since another processor may print meanwhile.
    let mut kept = [0_u8; 1024];
    let count = crate::console::recent(&mut kept);
    let found = kept.get(..count).is_some_and(|kept| {
        kept.windows(LINE.len())
            .any(|window| window == LINE.as_bytes())
    });
    if !found {
        return Err("the console did not keep the last line it printed");
    }
    Ok(())
}

/// A translated domain made for a function that has none, and dropped, gives
/// its stream back to its unit: a second one for the same function is then
/// granted rather than refused as already attached. On a PCI function no
/// driver has given DMA, so nothing is reaching through the stream meanwhile;
/// a machine whose functions all have domains, or none translated, has none
/// to try.
///
/// Verifies: L.iommu.14
fn a_dropped_domain_gives_its_stream_back() -> Result<(), &'static str> {
    for node in crate::device::devices() {
        let Location::Pci(function) = node.location() else {
            continue;
        };
        let Some(first) = crate::iommu::domain_for(function) else {
            continue;
        };
        if !first.translated() {
            continue;
        }
        drop(first);
        if !crate::iommu::domain_for(function).is_some_and(|domain| domain.translated()) {
            return Err("a dropped domain did not give its stream back to its unit");
        }
        return Ok(());
    }
    Ok(())
}
