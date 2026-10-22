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
//!
//! And a client's own buffer shown directly (`docs/DISPLAY.md` §8.2; the
//! consultant's condition 7, ledger line 3108): a made-up *shared* range
//! takes none of the card's bytes but counts as one of an open's buffers,
//! and is reclaimed without a decommit and without giving bytes back,
//! while a range the core made is decommitted and given back.

use alloc::sync::Arc;

use ferrix_displayctl::message::Ready;
use ferrix_native_abi::rights::Rights;

use super::{budget_bytes, may_copy, reclaim_plan, tally};
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
    check_shared()?;
    Ok(report)
}

/// A shared range against the budget and the reclaim, made up: a 1080p
/// frame's pages, held by an open that is not master.
fn check_shared() -> Result<(), &'static str> {
    const FRAME: u64 = 1920 * 1080 * 4;
    if budget_bytes(true, FRAME) != 0 || budget_bytes(false, FRAME) != FRAME {
        return Err("a client's buffer shown directly is counted in the card's bytes");
    }
    let master = Some(1);
    let shared = tally(0, master, [(2, FRAME, true)].into_iter());
    if shared.others != 0 || shared.others_buffers != 1 {
        return Err("a client's buffer shown directly is not one buffer of no bytes");
    }
    let made = tally(FRAME, master, [(2, FRAME, false)].into_iter());
    if made.others != FRAME || made.others_buffers != 1 {
        return Err("a buffer the core made is not counted in the card's bytes");
    }
    let plan = reclaim_plan(true);
    if plan.decommit || plan.give_back {
        return Err("a client's buffer shown directly is decommitted when it is let go of");
    }
    let plan = reclaim_plan(false);
    if !plan.decommit || !plan.give_back {
        return Err("a buffer the core made is not decommitted when it is let go of");
    }
    Ok(())
}
