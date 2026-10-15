//! The pin budget's and the quarantine's boot checks, beside the pins they
//! drive: they read what `pin.rs` keeps to itself -- a domain's counts, the
//! release, a pin under a budget of its own -- as a child module may.
//!
//! P1 to P5 of `docs/NVIDIA.md` §12.2, each at a budget of two pages, which a
//! boot can afford, on the first translated PCI domain. With no translated
//! domain there is no quarantine, and nothing is checked (ARMv7-A).

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::rights::Rights;
use ferrix_native_abi::status;
use ferrix_native_abi::types::{
    DEVICE_LIMIT_PIN_CEILING, DEVICE_LIMIT_PIN_PAGES, DEVICE_LIMIT_PIN_ROOM,
};
use ferrix_paging::MapFlags;

use super::{Counts, DEFAULT_PIN_BUDGET_PAGES, Pin, PinError, ceiling, counts, release};
use crate::audit::{self, DEVICE_LIMIT, DEVICE_LIMIT_SET};
use crate::device::DeviceNode;
use crate::iommu::{Domain, DomainError, Wait};
use crate::object::Object;
use crate::object::check::{Side, reg};
use crate::object::process::Exit;

/// The budget the checks drive.
const BUDGET: usize = 2;

/// What the checks did, for the boot log.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Report {
    /// Pins refused as the budget's rules say.
    pub(crate) refusals: u32,
    /// `device_set_limit` calls refused as P5 provokes them.
    pub(crate) set_refused: u32,
    /// Pages a release could not unpin, moved to `kept` (one per boot).
    pub(crate) kept: usize,
    /// Pages of a pin whose unpin's invalidation was made to fail, kept and
    /// held (check R7; two per boot on a VT-d domain, else none).
    pub(crate) failed_kept: usize,
}

/// The two ends a check pins for: a process that has ended, whose pins go
/// to the quarantine as they close, and one that has not.
struct Owners {
    /// Ended.
    dead: Arc<Exit>,
    /// Not ended.
    live: Arc<Exit>,
}

/// P1 to P5 on the first translated PCI domain, in order; `None` when there
/// is none.
///
/// # Errors
///
/// The first thing that is not so.
pub(crate) fn check_budget(nodes: &[Arc<DeviceNode>]) -> Result<Option<Report>, &'static str> {
    let Some((node, domain)) = translated_pci_domain(nodes)? else {
        return Ok(None);
    };
    let before = counts(&domain);
    if before != idle(before.budget) {
        return Err("the checked device had pins counted before its checks");
    }
    let owners = Owners {
        dead: Exit::for_check(true).map_err(|_| "no memory for a check's end")?,
        live: Exit::for_check(false).map_err(|_| "no memory for a check's end")?,
    };
    let mut report = Report::default();
    check_limit(&domain, &owners, &mut report)?;
    check_twice(&domain, &owners, &mut report)?;
    check_give_back(&domain, &owners)?;
    check_release(&domain, &owners, &mut report)?;
    check_set_limit(&node, &domain, &owners, &mut report)?;
    check_failed_invalidation(&domain, &owners, &mut report)?;
    Ok(Some(report))
}

/// R7 (`docs/NVIDIA.md` §12.3): a pin whose unpin's invalidation fails is
/// kept. Its unpin's wait descriptor is made to write no status, through
/// the check-only [`Wait::Unwritten`], so the unit's queue never says it
/// finished: the unpin answers an error after a short patience of its own
/// (2 ms), the pin's two pages move from `live` to `kept`, and its frames
/// stay held -- the check's own reference on each is not the last. Only on
/// a domain a VT-d unit translates; elsewhere nothing is checked.
///
/// Verifies: L.iommu.48
fn check_failed_invalidation(
    domain: &Arc<Domain>,
    owners: &Owners,
    report: &mut Report,
) -> Result<(), &'static str> {
    if !domain.queued() {
        return Ok(());
    }
    let before = counts(domain);
    let (mut pin, frames) = watched_pin(domain, &owners.live)?;
    pin.wait = Wait::Unwritten;
    let failed = crate::iommu::invalidations().failed;
    drop(pin);
    let now = counts(domain);
    let mut released = false;
    for &frame in &frames {
        released |= crate::mm::release_frame(frame);
    }
    if released {
        return Err("frames released after a failed invalidation");
    }
    if crate::iommu::invalidations().failed == failed {
        return Err("an unpin whose wait wrote no status did not fail its invalidation");
    }
    if now.kept != before.kept + frames.len() || now.live != before.live {
        return Err("a pin whose invalidation failed was not moved from live to kept");
    }
    report.failed_kept = frames.len();
    Ok(())
}

/// P1: with a budget of two pages, a two-page pin is taken and a one-page
/// pin after it is refused `LimitReached`, leaving `live` as it was.
///
/// Verifies: L.object.119
fn check_limit(
    domain: &Arc<Domain>,
    owners: &Owners,
    report: &mut Report,
) -> Result<(), &'static str> {
    let taken =
        check_pin(domain, &owners.live, 2).map_err(|_| "a pin within the budget was refused")?;
    if counts(domain).live != 2 {
        return Err("a pin taken was not counted live");
    }
    if !matches!(
        check_pin(domain, &owners.live, 1),
        Err(PinError::LimitReached)
    ) {
        return Err("a pin past the device's budget was not refused LimitReached");
    }
    report.refusals += 1;
    if counts(domain).live != 2 {
        return Err("a pin refused at the budget changed the live count");
    }
    drop(taken);
    if counts(domain) != idle(counts(domain).budget) {
        return Err("a pin closed by a live process was not given back whole");
    }
    Ok(())
}

/// P2: pins closed by a dead owner are quarantined, moved from `live`, and a
/// pin is refused `QuarantineFull` exactly when quarantined, live and new
/// pages would pass twice the budget: taken at 2 + 0 + 2, refused at
/// 4 + 0 + 1, and -- what the first design's `quarantined >= 2B` would have
/// taken -- refused at 3 + 0 + 2. A pin closed by a live owner is given
/// back whole, and not quarantined.
///
/// Verifies: L.object.47, L.object.48, H.DMA.4
fn check_twice(
    domain: &Arc<Domain>,
    owners: &Owners,
    report: &mut Report,
) -> Result<(), &'static str> {
    drop(check_pin(domain, &owners.dead, 2).map_err(|_| "a pin within the budget was refused")?);
    let now = counts(domain);
    if (now.quarantined, now.live) != (2, 0) {
        return Err("a pin closed by a dead process was not moved from live to the quarantine");
    }
    let second = check_pin(domain, &owners.dead, 2)
        .map_err(|_| "a pin within twice the budget, its quarantine counted, was refused")?;
    drop(second);
    if counts(domain).quarantined != 4 {
        return Err("a second dead driver's pin was not quarantined");
    }
    if !matches!(
        check_pin(domain, &owners.live, 1),
        Err(PinError::QuarantineFull)
    ) {
        return Err("a pin was taken on a device whose quarantine held twice its budget");
    }
    report.refusals += 1;
    if release(domain, 0).pages != 4 || counts(domain) != idle(BUDGET_UNCHANGED) {
        return Err("a release left pages counted in the quarantine");
    }
    drop(check_pin(domain, &owners.dead, 2).map_err(|_| "a pin after a release was refused")?);
    drop(check_pin(domain, &owners.dead, 1).map_err(|_| "a pin after a release was refused")?);
    if counts(domain).quarantined != 3 {
        return Err("three dead pages were not counted in the quarantine");
    }
    if !matches!(
        check_pin(domain, &owners.live, 2),
        Err(PinError::QuarantineFull)
    ) {
        return Err("a pin past twice the budget was taken: the quarantine's old rule");
    }
    report.refusals += 1;
    if release(domain, 0).pages != 3 || counts(domain) != idle(BUDGET_UNCHANGED) {
        return Err("a release left pages counted in the quarantine");
    }
    drop(check_pin(domain, &owners.live, 1).map_err(|_| "a pin after a release was refused")?);
    if counts(domain) != idle(BUDGET_UNCHANGED) {
        return Err("a pin closed by a live process was quarantined or kept counted");
    }
    Ok(())
}

/// P3: a pin the domain refuses -- a page pinned twice -- gives back the
/// pages it reserved, so `live` is as before.
///
/// Verifies: L.object.120
fn check_give_back(domain: &Arc<Domain>, owners: &Owners) -> Result<(), &'static str> {
    let vmo = crate::user::vmo::Vmo::new_anonymous(1).map_err(|_| "no memory for a check's VMO")?;
    let first = vmo
        .hold(0, 1)
        .map_err(|_| "a check's page could not be held")?;
    let again = vmo
        .hold(0, 1)
        .map_err(|_| "a check's page could not be held twice")?;
    let pin = Pin::with_budget(
        Arc::clone(domain),
        first,
        MapFlags::DMA,
        Arc::clone(&owners.live),
        Some(BUDGET),
        true,
    )
    .map_err(|_| "a pin within the budget was refused")?;
    let refused = Pin::with_budget(
        Arc::clone(domain),
        again,
        MapFlags::DMA,
        Arc::clone(&owners.live),
        Some(BUDGET),
        true,
    );
    if refused.as_ref().err() != Some(&PinError::Domain(DomainError::AlreadyPinned)) {
        return Err("a page pinned twice into one domain was not refused by the domain");
    }
    if counts(domain).live != 1 {
        return Err("a pin the domain refused kept the pages it reserved");
    }
    drop(pin);
    if counts(domain) != idle(BUDGET_UNCHANGED) {
        return Err("a pin closed by a live process was not given back whole");
    }
    Ok(())
}

/// P4: a release takes off `quarantined` only what it gave back. A pin its
/// unpin is refused -- here by the check, as a unit that never answers would
/// refuse it -- moves to `kept`, and still counts against twice the budget:
/// with one page kept and two quarantined, a two-page pin is refused. A pin
/// up to the budget is taken after the release.
///
/// Verifies: L.object.49
fn check_release(
    domain: &Arc<Domain>,
    owners: &Owners,
    report: &mut Report,
) -> Result<(), &'static str> {
    drop(check_pin(domain, &owners.dead, 1).map_err(|_| "a pin within the budget was refused")?);
    drop(check_pin(domain, &owners.dead, 1).map_err(|_| "a pin within the budget was refused")?);
    let released = release(domain, 1);
    let now = counts(domain);
    if released.pages != 1 || released.kept != 1 || (now.quarantined, now.kept) != (0, 1) {
        return Err("a release did not move a pin it could not unpin to kept, and only that one");
    }
    report.kept = now.kept;
    drop(
        check_pin(domain, &owners.live, 2)
            .map_err(|_| "a pin up to the budget after a release was refused")?,
    );
    drop(check_pin(domain, &owners.dead, 2).map_err(|_| "a pin within the budget was refused")?);
    if !matches!(
        check_pin(domain, &owners.live, 2),
        Err(PinError::QuarantineFull)
    ) {
        return Err("a pin was taken past twice the budget with kept pages not counted");
    }
    report.refusals += 1;
    if release(domain, 0).pages != 2 {
        return Err("a release did not give back the quarantined pin");
    }
    let now = counts(domain);
    if (now.live, now.quarantined, now.kept) != (0, 0, 1) {
        return Err("the counts after a release are not what was kept");
    }
    Ok(())
}

/// P5: `device_set_limit` is refused `ACCESS_DENIED` without `SET_LIMIT`,
/// `BAD_STATE` while the device has a live pin, and `NO_MEMORY` past the
/// ceiling, with nothing changed; `device_get_limit` answers the budget, the
/// ceiling and the room. Through the native calls, from a process of the
/// check's own.
///
/// Verifies: L.object.118
fn check_set_limit(
    node: &Arc<DeviceNode>,
    domain: &Arc<Domain>,
    owners: &Owners,
    report: &mut Report,
) -> Result<(), &'static str> {
    let side = Side::new()?;
    let outcome = set_limit_through(&side, node, domain, owners, report);
    side.close_everything();
    outcome
}

/// [`check_set_limit`]'s calls, in `side`.
fn set_limit_through(
    side: &Side,
    node: &Arc<DeviceNode>,
    domain: &Arc<Domain>,
    owners: &Owners,
    report: &mut Report,
) -> Result<(), &'static str> {
    let insert = |rights: Rights| {
        side.process
            .with_handles(|table| table.insert(Object::Device(Arc::clone(node)), rights))
            .map_err(|_| "no room for a device handle")
    };
    let driver = insert(Rights::DEVICE)?;
    let manager = insert(Rights(Rights::DEVICE.0 | Rights::SET_LIMIT.0))?;
    let set = |device: Handle, pages: usize| {
        side.call(
            nr::DEVICE_SET_LIMIT,
            &[reg(device), DEVICE_LIMIT_PIN_PAGES, pages as u64],
        )
    };
    let get = |which: u64| side.call(nr::DEVICE_GET_LIMIT, &[reg(driver), which]);
    let before = counts(domain);

    if set(driver, DEFAULT_PIN_BUDGET_PAGES) != Err(status::ACCESS_DENIED) {
        return Err("a pin budget was set through a handle without SET_LIMIT");
    }
    report.set_refused += 1;
    let live = check_pin_under(domain, &owners.live, 1, None)
        .map_err(|_| "a pin within the device's own budget was refused")?;
    if set(manager, DEFAULT_PIN_BUDGET_PAGES) != Err(status::BAD_STATE) {
        return Err("a pin budget was changed under a live pin");
    }
    report.set_refused += 1;
    drop(live);

    let room = get(DEVICE_LIMIT_PIN_ROOM).map_err(|_| "device_get_limit refused the room")?;
    if get(DEVICE_LIMIT_PIN_CEILING) != Ok(ceiling())
        || ceiling() == 0
        || room > ceiling()
        || get(DEVICE_LIMIT_PIN_PAGES) != Ok(before.budget)
    {
        return Err("device_get_limit did not answer the budget, the ceiling and the room");
    }
    let past = (room / 2 + 1).max(DEFAULT_PIN_BUDGET_PAGES + 1);
    if set(manager, past) != Err(status::NO_MEMORY) {
        return Err("a pin budget past the kernel's ceiling was set");
    }
    report.set_refused += 1;
    if get(DEVICE_LIMIT_PIN_PAGES) != Ok(before.budget) || get(DEVICE_LIMIT_PIN_ROOM) != Ok(room) {
        return Err("a pin budget refused past the ceiling changed something");
    }
    if set(manager, before.budget) != Ok(0) {
        return Err("a pin budget set again to its own value was refused");
    }
    let budget = before.budget as u64;
    let default = DEFAULT_PIN_BUDGET_PAGES as u64;
    audited(
        side,
        node,
        &[
            (DEVICE_LIMIT, status::ACCESS_DENIED.0, [budget, default]),
            (DEVICE_LIMIT, status::BAD_STATE.0, [budget, default]),
            (DEVICE_LIMIT, status::NO_MEMORY.0, [budget, past as u64]),
            (DEVICE_LIMIT_SET, 0, [budget, budget]),
        ],
    )?;
    // Raised within the room, where the machine has room for a raise, and
    // put back.
    let within = room / 2;
    if within > DEFAULT_PIN_BUDGET_PAGES {
        if set(manager, within) != Ok(0) || get(DEVICE_LIMIT_PIN_PAGES) != Ok(within) {
            return Err("a pin budget within the room was not set");
        }
        if set(manager, before.budget) != Ok(0) {
            return Err("a raised pin budget could not be put back");
        }
    }
    if counts(domain) != before {
        return Err("device_set_limit's checks left the device's budget changed");
    }
    Ok(())
}

/// Require the audit record to hold, for each of `wanted`, a record of
/// `side`'s process on `node` of that event and status, naming the pin
/// budget and the old and new values (condition 1 of the consultant's
/// review of N0f).
fn audited(
    side: &Side,
    node: &DeviceNode,
    wanted: &[(audit::Event, u16, [u64; 2])],
) -> Result<(), &'static str> {
    let mut high = vec![audit::Record::EMPTY; audit::HIGH_RECORDS];
    let mut refusals = vec![audit::Record::EMPTY; audit::REFUSAL_RECORDS];
    let kept = audit::read(audit::Which::High, 0, &mut high).copied;
    high.truncate(kept);
    let kept = audit::read(audit::Which::Refusals, 0, &mut refusals).copied;
    refusals.truncate(kept);
    let pid = side.process.pid();
    let device = u32::try_from(node.index()).unwrap_or(u32::MAX);
    for &(event, errno, [old, new]) in wanted {
        let detail = [
            audit::saturated(DEVICE_LIMIT_PIN_PAGES),
            audit::saturated(old),
            audit::saturated(new),
        ];
        let found = high.iter().chain(refusals.iter()).any(|record| {
            record.is(event)
                && record.pid == pid
                && record.status == i16::try_from(errno).unwrap_or(i16::MAX)
                && record.target_kind == audit::target::DEVICE
                && record.target_id == [device, 0]
                && record.detail == detail
        });
        if !found {
            return Err(
                "device_set_limit left no audit record of the device, the limit and \
                        its old and new values",
            );
        }
    }
    Ok(())
}

/// The counts of a device with nothing pinned, quarantined or kept, at
/// `budget`.
const fn idle(budget: usize) -> Counts {
    Counts {
        budget,
        live: 0,
        quarantined: 0,
        kept: 0,
    }
}

/// What the device's own budget stays while a check pins under its own:
/// the default, which nothing has set otherwise at stage 10.
const BUDGET_UNCHANGED: usize = DEFAULT_PIN_BUDGET_PAGES;

/// A node the checks pin for, and its domain.
type Checked = (Arc<DeviceNode>, Arc<Domain>);

/// The first PCI node, and its domain if it is translated.
fn translated_pci_domain(nodes: &[Arc<DeviceNode>]) -> Result<Option<Checked>, &'static str> {
    let Some(node) = nodes
        .iter()
        .find(|node| matches!(node.location(), crate::device::Location::Pci(_)))
    else {
        return Ok(None);
    };
    let domain = node
        .domain()
        .map_err(|_| "no memory for a device's domain")?;
    Ok(domain.translated().then(|| (Arc::clone(node), domain)))
}

/// A pin of `pages` fresh pages into `domain` for `owner`, under the
/// checks' budget of [`BUDGET`], quietly.
fn check_pin(domain: &Arc<Domain>, owner: &Arc<Exit>, pages: u64) -> Result<Pin, PinError> {
    check_pin_under(domain, owner, pages, Some(BUDGET))
}

/// [`check_pin`] under `budget`, or the device's own for `None`.
fn check_pin_under(
    domain: &Arc<Domain>,
    owner: &Arc<Exit>,
    pages: u64,
    budget: Option<usize>,
) -> Result<Pin, PinError> {
    let vmo = crate::user::vmo::Vmo::new_anonymous(pages).map_err(|_| PinError::NoMemory)?;
    let held = vmo.hold(0, pages).map_err(|_| PinError::NoMemory)?;
    Pin::with_budget(
        Arc::clone(domain),
        held,
        MapFlags::DMA,
        Arc::clone(owner),
        budget,
        true,
    )
}

/// What F-59's checks did, for the boot log.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Untranslated {
    /// Pins a live driver closed, each given back at once, past twice the
    /// checks' budget.
    pub(crate) given_back: usize,
    /// Pages a dead driver's pins quarantined and released at a HELLO.
    pub(crate) released: usize,
}

/// F-59's two checks on the first untranslated domain: a device tree or
/// virtio-mmio node's, or a PCI node's already made untranslated. `None` on a
/// machine where every domain made is translated (x86-64 and AArch64 under
/// QEMU); ARMv7-A has only untranslated ones.
///
/// # Errors
///
/// The first thing that is not so.
pub(crate) fn check_untranslated(
    nodes: &[Arc<DeviceNode>],
) -> Result<Option<Untranslated>, &'static str> {
    let Some((node, domain)) = untranslated_domain(nodes)? else {
        return Ok(None);
    };
    let before = counts(&domain);
    if before != idle(before.budget) {
        return Err("the checked untranslated device had pins counted before its checks");
    }
    let owners = Owners {
        dead: Exit::for_check(true).map_err(|_| "no memory for a check's end")?,
        live: Exit::for_check(false).map_err(|_| "no memory for a check's end")?,
    };
    let given_back = check_live_close(&domain, &owners)?;
    let released = check_dead_close(&node, &domain, &owners)?;
    Ok(Some(Untranslated {
        given_back,
        released,
    }))
}

/// How many times [`check_live_close`] pins and closes: three times twice
/// the checks' budget, in pages.
const CLOSES: usize = 3 * BUDGET;

/// P7: on an untranslated domain, two-page pins a live driver opens and
/// closes, past twice a budget of two pages, are each taken, given back at
/// once and their frames freed, and leave nothing counted.
///
/// Verifies: L.object.126
fn check_live_close(domain: &Arc<Domain>, owners: &Owners) -> Result<usize, &'static str> {
    for _ in 0..CLOSES {
        let (pin, frames) = watched_pin(domain, &owners.live)?;
        drop(pin);
        if counts(domain) != idle(BUDGET_UNCHANGED) {
            return Err("a pin closed by a live driver on an untranslated domain was kept");
        }
        freed(
            &frames,
            "a pin closed by a live driver on an untranslated domain kept its frames",
        )?;
    }
    Ok(CLOSES)
}

/// P8: on an untranslated domain a dead driver's pin is quarantined, its
/// frames held and nothing kept, and the next accepted HELLO
/// (`DeviceNode::hello_accepted`, after the configuration's read-back)
/// gives it back and frees its frames.
///
/// Verifies: L.object.127
fn check_dead_close(
    node: &Arc<DeviceNode>,
    domain: &Arc<Domain>,
    owners: &Owners,
) -> Result<usize, &'static str> {
    let (pin, frames) = watched_pin(domain, &owners.dead)?;
    drop(pin);
    let now = counts(domain);
    if (now.live, now.quarantined, now.kept) != (0, 2, 0) {
        return Err("a dead driver's pin on an untranslated domain was not quarantined");
    }
    if frames
        .iter()
        .any(|&frame| crate::mm::frame_references(frame) < 2)
    {
        // Ours and the quarantine's: put ours back before saying so.
        for &frame in &frames {
            let _ = crate::mm::release_frame(frame);
        }
        return Err("a dead driver's quarantined pin did not hold its frames");
    }
    node.hello_accepted();
    if counts(domain) != idle(BUDGET_UNCHANGED) {
        return Err("a HELLO did not release a dead driver's pin on an untranslated domain");
    }
    freed(
        &frames,
        "a released quarantined pin on an untranslated domain kept its frames",
    )?;
    Ok(frames.len())
}

/// A pin of two fresh pages into `domain` for `owner` under the checks'
/// budget, with a reference of the check's own on each frame, and its VMO
/// already gone: so that only the pin, and whatever it leaves, holds them.
fn watched_pin(domain: &Arc<Domain>, owner: &Arc<Exit>) -> Result<(Pin, Vec<u64>), &'static str> {
    let vmo = crate::user::vmo::Vmo::new_anonymous(2).map_err(|_| "no memory for a check's VMO")?;
    let held = vmo
        .hold(0, 2)
        .map_err(|_| "a check's pages could not be held")?;
    let frames = held.frames().to_vec();
    for &frame in &frames {
        if crate::mm::share_frame(frame).is_none() {
            return Err("a check could not take a reference to its own frame");
        }
    }
    let pin = Pin::with_budget(
        Arc::clone(domain),
        held,
        MapFlags::DMA,
        Arc::clone(owner),
        Some(BUDGET),
        true,
    );
    drop(vmo);
    match pin {
        Ok(pin) => Ok((pin, frames)),
        Err(_) => {
            for &frame in &frames {
                let _ = crate::mm::release_frame(frame);
            }
            Err("a pin on an untranslated domain within twice the budget was refused")
        }
    }
}

/// Give back the check's own reference to each of `frames`, and require
/// that it was the last: nothing else held them.
fn freed(frames: &[u64], kept: &'static str) -> Result<(), &'static str> {
    let mut all = true;
    for &frame in frames {
        all &= crate::mm::release_frame(frame);
    }
    if all { Ok(()) } else { Err(kept) }
}

/// The first node whose domain is untranslated: a device tree or
/// virtio-mmio node, whose domain is always untranslated, or a PCI node
/// whose domain was already made so. No translated domain is made here.
fn untranslated_domain(nodes: &[Arc<DeviceNode>]) -> Result<Option<Checked>, &'static str> {
    for node in nodes {
        let domain = match node.location() {
            crate::device::Location::Pci(_) => match node.domain_made() {
                Some(domain) => domain,
                None => continue,
            },
            _ => node
                .domain()
                .map_err(|_| "no memory for a device's domain")?,
        };
        if !domain.translated() {
            return Ok(Some((Arc::clone(node), domain)));
        }
    }
    Ok(None)
}
