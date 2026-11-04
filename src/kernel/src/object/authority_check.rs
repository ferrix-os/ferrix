//! K8's rules on devmgr's right to bind (`docs/microkernel/K8.md` §3).
//!
//! `present` and `desktop` publish no explicit node, so the check makes up its
//! own and drives them through the native dispatch; the real function it
//! borrows for the bus master's half is turned back to the rule it had.

#![allow(clippy::disallowed_methods, reason = "a raised check")]

use alloc::sync::Arc;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::rights::Rights;
use ferrix_native_abi::status;
use ferrix_native_abi::types;

use crate::device::{self, BindRefusal, DeviceNode, Location};
use crate::object::Object;
use crate::object::check::{SCRATCH, Side, reg};

/// Where the check stages a spec for `io_mapping_create`.
const SPEC: u64 = SCRATCH + PAGE_SIZE + 0x60;

/// What the check did, for the boot log.
#[derive(Debug, Default)]
pub struct Report {
    /// Calls refused with the status the rule names.
    pub refusals: u32,
    /// Made-up nodes it ran on.
    pub nodes: u32,
    /// Whether a real function showed bus mastering following its bind.
    pub master: bool,
}

/// A device handle with `rights` to a made-up tree node.
fn held(side: &Side, node: &Arc<DeviceNode>, rights: Rights) -> Result<Handle, &'static str> {
    side.process
        .with_handles(|table| table.insert(Object::Device(Arc::clone(node)), rights))
        .map_err(|_| "no room for a device handle")
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

/// Run it.
///
/// Verifies: L.device.66, L.device.67, L.device.68, L.device.69
/// Verifies: L.device.72
/// Verifies: H.DEV.9, H.DEV.10
pub fn run() -> Result<Report, &'static str> {
    let side = Side::new()?;
    let mut report = Report::default();
    let checked = run_on(&side, &mut report);
    side.close_everything();
    checked.map(|()| report)
}

fn run_on(side: &Side, report: &mut Report) -> Result<(), &'static str> {
    right_and_refusals(side, report)?;
    unbound_gives_nothing(side, report)?;
    borrowed_function(side, report)
}

/// A made-up node that reaches memory, under the explicit rule.
fn made_up(at: u64) -> Result<Arc<DeviceNode>, &'static str> {
    let node = crate::fallible::try_arc(DeviceNode::empty(Location::Tree(at)))
        .map_err(|_| "no memory for a made-up node")?;
    node.explicit_for_check(true)?;
    Ok(node)
}

/// `L.device.66`, `L.device.69`, `L.device.72`.
fn right_and_refusals(side: &Side, report: &mut Report) -> Result<(), &'static str> {
    let node = made_up(0x7000_0000)?;
    report.nodes += 1;
    // A tree node's domain never translates: refused without the opt-out,
    // and nothing kept (K8.3).
    if node.bind_domain(false) != Err(BindRefusal::Untranslated) || node.domain_made().is_some() {
        return Err("an untranslated function was bound without the opt-out");
    }
    report.refusals += 1;
    let right = held(
        side,
        &node,
        Rights(Rights::DEVICE.0 | Rights::BIND_DOMAIN.0),
    )?;
    if !crate::discovery::devmgr::unisolated_allowed() {
        refused(
            side.call(nr::DEVICE_BIND_DOMAIN, &[reg(right)]),
            status::UNTRANSLATED,
            "device_bind_domain bound an untranslated function without the opt-out",
            report,
        )?;
    }
    if node.bind_domain(true).is_err() || node.domain_made().is_none() {
        return Err("a bind with the opt-out kept no domain");
    }
    // Bound, it stays bound and a second answers so, the domain untouched.
    let domain = node.domain_made();
    if node.bind_domain(true) != Err(BindRefusal::Already)
        || !domain
            .zip(node.domain_made())
            .is_some_and(|(one, two)| Arc::ptr_eq(&one, &two))
    {
        return Err("a second bind changed a bound node");
    }
    report.refusals += 1;
    // The kernel's own no-memory node has nothing to bind.
    let power = crate::fallible::try_arc(DeviceNode::power())
        .map_err(|_| "no memory for a made-up node")?;
    power.explicit_for_check(true)?;
    report.nodes += 1;
    if power.bind_domain(true) != Err(BindRefusal::NoDma) || power.domain_made().is_some() {
        return Err("a node that reaches no memory was bound");
    }
    report.refusals += 1;
    // There the answers tell the right from the rest: with it WRONG_TYPE,
    // without it ACCESS_DENIED.
    let right = held(
        side,
        &power,
        Rights(Rights::DEVICE.0 | Rights::BIND_DOMAIN.0),
    )?;
    refused(
        side.call(nr::DEVICE_BIND_DOMAIN, &[reg(right)]),
        status::WRONG_TYPE,
        "device_bind_domain with the right did not reach the node",
        report,
    )?;
    let plain = held(side, &power, Rights::DEVICE)?;
    refused(
        side.call(nr::DEVICE_BIND_DOMAIN, &[reg(plain)]),
        status::ACCESS_DENIED,
        "a device handle without BIND_DOMAIN bound a domain",
        report,
    )?;
    // A refused node answers BAD_STATE, as every MANAGE call does.
    let gone = made_up(0x7000_1000)?;
    report.nodes += 1;
    gone.refuse_for_check();
    let handle = held(
        side,
        &gone,
        Rights(Rights::DEVICE.0 | Rights::BIND_DOMAIN.0),
    )?;
    refused(
        side.call(nr::DEVICE_BIND_DOMAIN, &[reg(handle)]),
        status::BAD_STATE,
        "a refused node was bound",
        report,
    )?;
    Ok(())
}

/// `L.device.68`: unbound, an explicit node gives no vector, aperture or pin,
/// its isolation says untranslated, and no call makes it a domain.
fn unbound_gives_nothing(side: &Side, report: &mut Report) -> Result<(), &'static str> {
    let node = made_up(0x7000_2000)?;
    report.nodes += 1;
    let handle = held(side, &node, Rights::DEVICE)?;
    let vmo = side.handle(nr::VMO_CREATE, &[PAGE_SIZE], "vmo_create for a pin failed")?;
    side.put(SPEC, &0x7000_2000_u64.to_ne_bytes())?;
    side.put(SPEC + 8, &PAGE_SIZE.to_ne_bytes())?;
    refused(
        side.call(nr::IO_MAPPING_CREATE, &[reg(handle), SPEC]),
        status::BAD_STATE,
        "an unbound node gave an aperture",
        report,
    )?;
    refused(
        side.call(nr::INTERRUPT_CREATE, &[reg(handle), 0]),
        status::BAD_STATE,
        "an unbound node gave a vector",
        report,
    )?;
    refused(
        side.call(nr::VMO_PIN, &[reg(handle), reg(vmo), 0, PAGE_SIZE, 0]),
        status::BAD_STATE,
        "a pin on an unbound node made a domain",
        report,
    )?;
    let limit = Rights(Rights::DEVICE.0 | Rights::SET_LIMIT.0);
    let limited = held(side, &node, limit)?;
    refused(
        side.call(
            nr::DEVICE_SET_LIMIT,
            &[reg(limited), types::DEVICE_LIMIT_PIN_PAGES, 1],
        ),
        status::BAD_STATE,
        "a limit on an unbound node made a domain",
        report,
    )?;
    let bits = side
        .call(nr::DEVICE_ISOLATION, &[reg(handle)])
        .map_err(|_| "device_isolation refused an unbound node")?;
    if bits as u64 & types::DEVICE_ISOLATION_DMA_TRANSLATED != 0 {
        return Err("an unbound node answered translated");
    }
    if node.domain_made().is_some() || node.bus_master_on() {
        return Err("a call on an unbound node made it a domain");
    }
    Ok(())
}

/// `H.DEV.10`, on a real function: not mastering before its bind, and only
/// through a pin after it.
fn borrowed_function(side: &Side, report: &mut Report) -> Result<(), &'static str> {
    let Some(node) = device::devices()
        .iter()
        .find(|node| {
            matches!(node.location(), Location::Pci(_))
                && node.config_phys().is_some()
                && !node.bus_master_on()
                && node.reaches_memory()
                && !node.intel_gpu()
                && !node.is_refused()
                && !node.is_bridge()
        })
        .cloned()
    else {
        return Ok(());
    };
    // A function may have its domain from an earlier check: out for the
    // length of this, so that it is the unbound function the rule meets.
    let earlier = node.take_domain_for_check()?;
    node.explicit_for_check(true)?;
    let answered = (|| {
        let handle = held(side, &node, Rights::DEVICE)?;
        let vmo = side.handle(nr::VMO_CREATE, &[PAGE_SIZE], "vmo_create for a pin failed")?;
        refused(
            side.call(nr::VMO_PIN, &[reg(handle), reg(vmo), 0, PAGE_SIZE, 0]),
            status::BAD_STATE,
            "a pin on an unbound function made a domain",
            report,
        )?;
        if node.bus_master_on() || node.domain_made().is_some() {
            return Err("an unbound function became a bus master");
        }
        match earlier {
            Some(domain) => node.give_check_domain(domain)?,
            None => node
                .bind_domain(true)
                .map_err(|_| "a real function could not be bound")?,
        }
        if node.bus_master_on() {
            return Err("a bind alone made a function a bus master");
        }
        let pin = side.handle(
            nr::VMO_PIN,
            &[reg(handle), reg(vmo), 0, PAGE_SIZE, 0],
            "a pin on a bound function failed",
        )?;
        if !node.bus_master_on() {
            return Err("a pin on a bound function left it no bus master");
        }
        let _ = side.call(nr::HANDLE_CLOSE, &[reg(pin)]);
        node.disable_dma()
    })();
    node.explicit_for_check(device::EXPLICIT_BIND)?;
    answered?;
    report.master = true;
    Ok(())
}
