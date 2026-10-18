//! Stage 10's check of which display drivers may copy (`docs/DISPLAY.md`
//! §2.1 and §8): the one card rule the core holds before READY, without a
//! driver.
//!
//! A device tree node that is not the firmware framebuffer's -- made up
//! here, so the check runs on every machine -- is refused, as is every
//! published node that is neither the firmware framebuffer's nor one the
//! chardev core serves; the firmware framebuffer's node, where
//! `ferrix.efifb` published one, is allowed. A driver allowed to copy is
//! handed the card with `READ | MAP` and never `WRITE`, and one that is not
//! with no `MAP` either.

use alloc::sync::Arc;

use ferrix_displayctl::message::Ready;
use ferrix_native_abi::rights::Rights;

use super::may_copy;
use crate::device::{self, DeviceNode, Location};

/// What the check established, for the boot log.
#[derive(Debug, Default)]
pub(crate) struct Report {
    /// Nodes whose drivers may not copy, refused.
    pub(crate) refused: usize,
    /// Nodes whose drivers may, allowed.
    pub(crate) allowed: usize,
}

/// Run the check.
///
/// # Errors
///
/// What was not true.
pub(crate) fn run() -> Result<Report, &'static str> {
    let mut report = Report::default();
    // FATAL-ALLOC: boot only: one made-up node, once, for the check.
    let other = Arc::new(DeviceNode::empty(Location::Tree(0x1000)));
    if may_copy(&other) {
        return Err("a device tree node other than the firmware framebuffer's may copy");
    }
    report.refused += 1;
    for node in device::devices() {
        let allowed = node.framebuffer_mode().is_some()
            || crate::interfaces::chardev::publishes_for(node)
            || node.may_copy_lent();
        if may_copy(node) != allowed {
            return Err("a published node's driver may copy other than the rule says");
        }
        if allowed {
            report.allowed += 1;
        } else {
            report.refused += 1;
        }
    }
    let copying = Ready::card_rights(true);
    let showing = Ready::card_rights(false);
    if !copying.contains(Rights::READ)
        || !copying.contains(Rights::MAP)
        || copying.contains(Rights::WRITE)
        || showing.contains(Rights::MAP)
        || showing.contains(Rights::WRITE)
    {
        return Err("a driver is handed the card with other rights than READ and MAP, never WRITE");
    }
    Ok(report)
}
