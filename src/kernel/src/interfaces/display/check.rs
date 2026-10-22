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

/// What the mode-list check established, for the boot log.
#[derive(Debug, Default)]
pub(crate) struct ModesReport {
    /// `SETCRTC` requests matched or refused as the rule says.
    pub(crate) matched: usize,
    /// Pages taken, and lying pages refused.
    pub(crate) pages: usize,
}

/// Stage 10's check of the mode-list rules (`docs/DISPLAY.md` §9.3; the
/// consultant's condition 6, ledger 3117), without a driver: `SETCRTC`'s
/// match against a made-up list -- the exact listed timing, the same size
/// at the listed refresh, and neither another refresh nor another size --
/// and a session given a list in two pages, then a page that lies.
///
/// # Errors
///
/// What was not true.
pub(crate) fn modes() -> Result<ModesReport, &'static str> {
    use ferrix_displayctl::message::{
        Hello, Listed, MAX_SCANOUTS, MODES_A_PAGE, Message, ModePage, Refusal, ScanoutMode,
        Timings, VERSION,
    };
    use ferrix_displayctl::modes::{listed_match, synthetic};
    use ferrix_displayctl::session::{Event, Session};

    let mut report = ModesReport::default();
    let at = |width: u32, height: u32, refresh: u32, preferred: bool| Listed {
        timing: synthetic(width, height, refresh),
        preferred,
    };
    let list = [
        at(1920, 1080, 60_000, true),
        at(1280, 720, 60_000, false),
        at(1280, 720, 120_000, false),
    ];
    let small = list.get(1).ok_or("the made-up list")?.timing;
    let mut wanted = super::drm::mode_info(&small);
    if listed_match(&list, &wanted) != Some(small) {
        return Err("SETCRTC refused a mode exactly as its driver listed it");
    }
    report.matched += 1;
    wanted.htotal = wanted.htotal.wrapping_add(2);
    if listed_match(&list, &wanted) != Some(small) {
        return Err("SETCRTC did not take the listed timing of a listed size and refresh");
    }
    report.matched += 1;
    wanted.vrefresh = 75;
    if listed_match(&list, &wanted).is_some() {
        return Err("SETCRTC took a listed size at a refresh nobody listed");
    }
    report.matched += 1;
    if listed_match(&list, &super::drm::mode_info(&synthetic(800, 600, 60_000))).is_some() {
        return Err("SETCRTC took a size nobody listed");
    }
    report.matched += 1;

    let mut modes = [ScanoutMode::default(); MAX_SCANOUTS];
    if let Some(first) = modes.first_mut() {
        *first = ScanoutMode {
            width: 1920,
            height: 1080,
            enabled: true,
            refresh_mhz: 60_000,
        };
    }
    let hello = Hello {
        version: VERSION,
        scanouts: 1,
        location: 0,
        modes,
        virgl: false,
        capsets: 0,
        capset: 0,
        capset_bytes: 0,
        cursor: false,
        timings: Timings::NONE,
        copies: false,
        connectors: [0; MAX_SCANOUTS],
    };
    let page = |first: u32, count: u32, total: u32| {
        let mut entries = [Listed::default(); MODES_A_PAGE];
        for (index, entry) in entries.iter_mut().take(count as usize).enumerate() {
            let line = first + u32::try_from(index).unwrap_or(0);
            *entry = at(640, 400 + line, 60_000, line == 0);
        }
        ModePage {
            scanout: 0,
            total,
            first,
            count,
            entries,
        }
    };
    let mut session = Session::accept(&hello, &Hello::HANDLE_RIGHTS, super::CARD_BYTES)
        .map_err(|_| "the made-up HELLO was refused")?;
    let first = session.receive(&Message::ModeList(page(0, 24, 30)));
    let second = session.receive(&Message::ModeList(page(24, 6, 30)));
    let finished = matches!(
        (first, second),
        (
            Ok(Event::ModeList {
                finishes: false,
                ..
            }),
            Ok(Event::ModeList { finishes: true, .. })
        )
    );
    if !finished || !session.has_list(0) {
        return Err("a list sent in two pages was not taken");
    }
    report.pages += 2;
    // A finished list with two preferred entries: a lie.
    let mut lying = page(0, 2, 2);
    if let Some(second) = lying.entries.get_mut(1) {
        second.preferred = true;
    }
    if session.receive(&Message::ModeList(lying)) != Err(Refusal::Protocol) {
        return Err("a list with two preferred modes was taken");
    }
    report.pages += 1;
    Ok(report)
}
