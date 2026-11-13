//! K14's grant (`docs/microkernel/K14.md` §5). `present` flags no node and carries
//! no list, so the check makes up both: a node that needs a certified driver, and
//! a list naming one sealed VMO it makes.

#![allow(clippy::disallowed_methods, reason = "a raised check")]

use alloc::boxed::Box;
use alloc::sync::Arc;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_errno::Errno;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::rights::Rights;
use ferrix_native_abi::status;
use ferrix_native_abi::types::{self, MAP_READ, MAP_WRITE};

use crate::device::{DeviceNode, Location};
use crate::object::Object;
use crate::object::check::{SCRATCH, Side, reg};
use crate::user::space::certified;
use crate::user::vmo::Vmo;

const OFFSET_AT: u64 = SCRATCH + PAGE_SIZE + 0x80;
const SPEC_AT: u64 = SCRATCH + PAGE_SIZE + 0xA0;

/// What the check did, for the boot log.
#[derive(Debug, Default)]
pub struct Report {
    /// Calls refused as the grant says.
    pub refusals: u32,
    /// Grants that passed.
    pub granted: u32,
}

fn refused(
    answer: Result<usize, Errno>,
    wanted: Errno,
    what: &'static str,
    report: &mut Report,
) -> Result<(), &'static str> {
    if answer == Err(wanted) {
        report.refusals += 1;
        Ok(())
    } else {
        Err(what)
    }
}

fn node_needing(at: u64) -> Result<Arc<DeviceNode>, &'static str> {
    let node = crate::fallible::try_arc(DeviceNode::empty(Location::Tree(at)))
        .map_err(|_| "no memory for a made-up node")?;
    node.require_certified();
    node.explicit_for_check(false)?;
    Ok(node)
}

fn device_handle(side: &Side, node: &Arc<DeviceNode>) -> Result<Handle, &'static str> {
    side.process
        .with_handles(|table| table.insert(Object::Device(Arc::clone(node)), Rights::DEVICE))
        .map_err(|_| "no room for a device handle")
}

/// A sealed one-page VMO in `side`, and its digest.
fn sealed(side: &Side, fill: u8) -> Result<(Handle, [u8; 32]), &'static str> {
    let vmo = side.handle(nr::VMO_CREATE, &[PAGE_SIZE], "vmo_create failed")?;
    side.put(SPEC_AT, &[fill; 8])?;
    side.put(OFFSET_AT, &0_u64.to_ne_bytes())?;
    let _ = side
        .call(nr::VMO_WRITE, &[reg(vmo), SPEC_AT, 8, OFFSET_AT])
        .map_err(|_| "vmo_write failed")?;
    let image = side.handle(
        nr::VMO_EXECUTABLE,
        &[reg(vmo), types::VMO_SEAL],
        "a VMO could not be sealed",
    )?;
    let object: Arc<Vmo> = side.process.with_handles(|table| {
        let (object, _) = table.get(image).map_err(|_| "a handle went")?;
        let Object::Vmo(vmo) = object else {
            return Err("not a VMO");
        };
        Ok(Arc::clone(vmo))
    })?;
    let digest = object.digest().ok_or("a sealed VMO had no digest")?;
    Ok((image, digest))
}

/// The process's own space by handle, for `space_map` into itself.
fn own_space_handle(side: &Side) -> Result<Handle, &'static str> {
    let space = crate::object::space::SpaceRef::of(
        Arc::clone(side.process.own_space()),
        side.process.quota_slot(),
        side.process.exit().level(),
    )
    .map_err(|_| "no space handle")?;
    side.process
        .with_handles(|table| table.insert(Object::Space(space), Rights::SPACE))
        .map_err(|_| "no room for a space's handle")
}

fn map_exec(side: &Side, space: Handle, image: Handle) -> Result<usize, Errno> {
    side.call(
        nr::SPACE_MAP,
        &[
            reg(space),
            reg(image),
            0,
            PAGE_SIZE,
            u64::from(MAP_READ | types::MAP_EXECUTE),
            OFFSET_AT,
        ],
    )
}

/// Run it.
///
/// Verifies: L.mm.127, L.mm.128, L.mm.129, L.mm.130, L.device.79, L.device.80
pub fn run() -> Result<Report, &'static str> {
    let mut report = Report::default();
    let lister = Side::new()?;
    let (_, listed) = sealed(&lister, 0x11)?;
    certified::set(Box::new([listed]));
    let result = checks(listed, &mut report);
    lister.close_everything();
    result.map(|()| report)
}

/// G.5's sweep, on a made-up tree: the STM32MP157's DMA1, DMA2 and MDMA would be held to
/// the grant untranslated, and neither of the two nodes the kernel synthesises would.
fn sweep(report: &mut Report) -> Result<(), &'static str> {
    for at in [0x4800_0000_u64, 0x4800_1000, 0x5800_0000] {
        let node = DeviceNode::empty(Location::Tree(at));
        if !node.wants_certified(false) {
            return Err("a DMA controller of a made-up tree was not held to the grant");
        }
        if node.wants_certified(true) {
            return Err("a translated node was held to the grant");
        }
        report.refusals += 1;
    }
    // What such a node gives carries no TRANSFER, and another node's is left as it asked.
    let free = DeviceNode::empty(Location::Tree(0x4800_2000));
    if free.derived_rights(Rights::INTERRUPT) != Rights::INTERRUPT {
        return Err("a node needing no grant lost a right");
    }
    free.require_certified();
    if free
        .derived_rights(Rights::INTERRUPT)
        .contains(Rights::TRANSFER)
        || free
            .derived_rights(Rights::IO_MAPPING)
            .contains(Rights::TRANSFER)
    {
        return Err("what a node held to the grant gave could travel");
    }
    report.refusals += 1;
    if DeviceNode::power().wants_certified(false) {
        return Err("a node that reaches no memory was held to the grant");
    }
    Ok(())
}

#[expect(
    clippy::too_many_lines,
    reason = "one grant from refusal to refusal, in the order a driver meets them"
)]
fn checks(_listed: [u8; 32], report: &mut Report) -> Result<(), &'static str> {
    sweep(report)?;
    // Nothing measured: refused at each of the three.
    let plain = Side::new()?;
    let node = node_needing(0x7100_0000)?;
    let device = device_handle(&plain, &node)?;
    let vmo = plain.handle(nr::VMO_CREATE, &[PAGE_SIZE], "vmo_create failed")?;
    plain.put(SCRATCH + PAGE_SIZE + 0xC0, &0x7100_0000_u64.to_ne_bytes())?;
    plain.put(SCRATCH + PAGE_SIZE + 0xC8, &PAGE_SIZE.to_ne_bytes())?;
    refused(
        plain.call(
            nr::IO_MAPPING_CREATE,
            &[reg(device), SCRATCH + PAGE_SIZE + 0xC0],
        ),
        status::BAD_STATE,
        "an aperture went to a process that does not pass",
        report,
    )?;
    refused(
        plain.call(nr::INTERRUPT_CREATE, &[reg(device), 0]),
        status::BAD_STATE,
        "a vector went to a process that does not pass",
        report,
    )?;
    refused(
        plain.call(nr::VMO_PIN, &[reg(device), reg(vmo), 0, PAGE_SIZE, 0]),
        status::BAD_STATE,
        "a pin went to a process that does not pass",
        report,
    )?;
    plain.close_everything();

    // Measured, a listed image mapped: passes.
    let good = Side::new()?;
    good.process.own_space().measure();
    let (image, digest) = same_listed(&good)?;
    let space = own_space_handle(&good)?;
    let _ = map_exec(&good, space, image).map_err(|_| "a listed image could not be mapped")?;
    if !good.process.own_space().passes() {
        return Err("an unlisted image was granted a device needing certification");
    }
    let _ = digest;
    let node = node_needing(0x7100_1000)?;
    node.bind_domain(true)
        .map_err(|_| "a made-up node would not bind")?;
    let device = device_handle(&good, &node)?;
    let vmo = good.handle(nr::VMO_CREATE, &[PAGE_SIZE], "vmo_create failed")?;
    let pin = good.call(nr::VMO_PIN, &[reg(device), reg(vmo), 0, PAGE_SIZE, 0]);
    if pin.is_err() {
        return Err("a process that passes was refused its pin");
    }
    report.granted += 1;
    if !good.process.own_space().granted() {
        return Err("a grant was not recorded");
    }
    // After a grant, an unlisted sealed image is refused.
    let (unlisted, _) = sealed(&good, 0x22)?;
    refused(
        map_exec(&good, space, unlisted),
        status::BAD_STATE,
        "a granted process mapped an unlisted image",
        report,
    )?;
    // And a change from outside is refused, its supervisors closed.
    let outside = Side::new()?;
    let held = crate::object::space::SpaceRef::of(
        Arc::clone(good.process.own_space()),
        good.process.quota_slot(),
        good.process.exit().level(),
    )
    .map_err(|_| "no space handle")?;
    let reach = outside
        .process
        .with_handles(|table| table.insert(Object::Space(held), Rights::SPACE))
        .map_err(|_| "no room for a space's handle")?;
    refused(
        outside.call(
            nr::SPACE_ALLOCATE,
            &[reg(reach), 0, PAGE_SIZE, u64::from(MAP_READ | MAP_WRITE)],
        ),
        status::BAD_STATE,
        "a granted space took a change from outside",
        report,
    )?;
    if !good.process.job().granted_below() {
        return Err("a grant left no word on its job");
    }
    // One step on `reach` decides, both ways (ledger 6316 P2.2).
    if good.process.own_space().reached(true) {
        return Err("a granted space let a change in");
    }
    report.refusals += 1;
    outside.close_everything();
    good.close_everything();

    // An unlisted image before a grant: the mark, and no pass.
    let bad = Side::new()?;
    bad.process.own_space().measure();
    let (image, _) = sealed(&bad, 0x33)?;
    let space = own_space_handle(&bad)?;
    let _ = map_exec(&bad, space, image).map_err(|_| "a sealed image could not be mapped")?;
    if bad.process.own_space().passes() {
        return Err("a space with an unlisted image passes");
    }
    report.refusals += 1;
    bad.close_everything();

    // A change from a process that does not pass taints.
    let target = Side::new()?;
    target.process.own_space().measure();
    let (image, _) = same_listed(&target)?;
    let space = own_space_handle(&target)?;
    let _ = map_exec(&target, space, image).map_err(|_| "a listed image could not be mapped")?;
    let reacher = Side::new()?;
    let held = crate::object::space::SpaceRef::of(
        Arc::clone(target.process.own_space()),
        target.process.quota_slot(),
        target.process.exit().level(),
    )
    .map_err(|_| "no space handle")?;
    let reach = reacher
        .process
        .with_handles(|table| table.insert(Object::Space(held), Rights::SPACE))
        .map_err(|_| "no room for a space's handle")?;
    let _ = reacher.call(
        nr::SPACE_ALLOCATE,
        &[reg(reach), 0, PAGE_SIZE, u64::from(MAP_READ | MAP_WRITE)],
    );
    if !target.process.own_space().tainted() || target.process.own_space().passes() {
        return Err("a change from a process that does not pass left no taint");
    }
    report.refusals += 1;
    if target.process.own_space().try_grant() {
        return Err("a tainted space took a grant");
    }
    report.refusals += 1;
    reacher.close_everything();
    target.close_everything();

    // A supervisor that passes is held to the list from its attach: it could otherwise
    // lose its pass and keep its control (ledger 6316 P2.4).
    let holder = Side::new()?;
    holder.process.own_space().measure();
    let (image, _) = same_listed(&holder)?;
    let space = own_space_handle(&holder)?;
    let _ = map_exec(&holder, space, image).map_err(|_| "a listed image could not be mapped")?;
    let supervised = crate::object::job::root()
        .new_child()
        .map_err(|_| "no job to supervise")?;
    let job = holder
        .process
        .with_handles(|table| table.insert(Object::Job(supervised), Rights::JOB_CREATED))
        .map_err(|_| "no room for a job's handle")?;
    let _ = holder
        .call(nr::EXCEPTION_CHANNEL_CREATE, &[reg(job), 1000, 0])
        .map_err(|_| "a supervisor that passes was refused")?;
    if !holder.process.own_space().granted() {
        return Err("a supervisor that passes stayed free to lose its pass");
    }
    report.granted += 1;
    let (unlisted, _) = sealed(&holder, 0x44)?;
    refused(
        map_exec(&holder, space, unlisted),
        status::BAD_STATE,
        "a supervisor that passes mapped an unlisted image",
        report,
    )?;
    holder.close_everything();
    Ok(())
}

/// The listed image again, in `side`: the same bytes, so the same digest.
fn same_listed(side: &Side) -> Result<(Handle, [u8; 32]), &'static str> {
    sealed(side, 0x11)
}
