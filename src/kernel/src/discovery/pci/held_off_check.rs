//! Stage 10's check of the held-off path, moved here unchanged from
//! `discovery/pci.rs` with the kernel's split into crates
//! (`docs/KERNEL-CRATES.md`): it runs the core's check code
//! (`device::check::check_held_off`), which only check code in another
//! crate may name, and enumeration calls it under `ferrix.checks`.

use ferrix_pci::ConfigSpace as _;
use ferrix_pci::bar::Region;
use ferrix_pci::header::COMMAND;
use ferrix_pci::msix as pci_msix;
use ferrix_pci::walk::Function;

use super::{Capability, MsiX, Space, table_ranges};
use crate::device::{self, DeviceNode, Reserved, Seen};

/// Stage 10's check of the held-off path (`device::check::check_held_off`),
/// on QEMU's `e1000e` only (`ferrix_pci::msix::held_off_check_case`, under
/// `ferrix.checks`): its MSI-X held off through this walk's space, a
/// scratch node built from `seen` as if the function were in
/// `MSI_PREFERRED`, the check run on it, and the node dropped unpublished.
///
/// One function, one truth (the consultant's V2, choice (a)): every write
/// is stage 10's, before any node is published; the command register, MSI
/// control and MSI-X control are written back as found afterwards -- MSI
/// off, MSI-X off, bus mastering and `INTx` as they were -- and the caller
/// builds the published node from fresh reads. The scratch node's mapping
/// of configuration space is never unmapped, as no node's is.
pub(super) fn held_off_check(
    space: &mut Space,
    function: &Function,
    seen: &Seen<'_>,
    regions: &[Region],
    msix: Option<&(Capability, MsiX)>,
    reserved: &Reserved,
) {
    let (Some((capability, table)), Some(msi)) = (msix, seen.msi) else {
        return;
    };
    let address = function.address;
    let command = space.read16(address, COMMAND);
    let msix_control_at = capability.offset + pci_msix::CAPABILITY_CONTROL;
    let msix_control = space.read16(address, msix_control_at);
    let msi_control_at = msi.capability + ferrix_pci::msi::CONTROL;
    let msi_control = space.read16(address, msi_control_at);
    let outcome = if pci_msix::hold_off(space, address, capability.offset) {
        let scratch = DeviceNode::pci(address, seen, regions, msix, true, reserved);
        let outcome = device::check::check_held_off(&scratch, table_ranges(regions, table));
        drop(scratch);
        outcome
    } else {
        Err(device::Failure {
            location: device::Location::Pci(address),
            what: "QEMU's e1000e would not hold its MSI-X off",
        })
    };
    space.write16(
        address,
        msi_control_at,
        msi_control & !ferrix_pci::msi::CONTROL_ENABLE,
    );
    space.write16(address, msix_control_at, msix_control);
    space.write16(address, COMMAND, command);
    let _ = device::check::HELD_OFF.call_once(|| outcome);
}
