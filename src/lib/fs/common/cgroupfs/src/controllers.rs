//! The controllers, `cgroup.controllers`, and writes to
//! `cgroup.subtree_control`.
//!
//! A set of controllers prints as their names in Linux's subsystem order,
//! one space between, and a newline -- a bare newline when there are none
//! (`cgroup_print_ss_mask`). A write to `cgroup.subtree_control` is parsed as
//! `cgroup_subtree_control_write` parses it: surrounding whitespace dropped,
//! then tokens split on single spaces, empty ones skipped, each `+name` or
//! `-name`. A later token for a controller overrides an earlier one, so
//! `+memory -memory` disables it. A name the kernel does not have, or a token
//! without its sign, refuses the whole write, and nothing of it is applied.

use crate::Refusal;
use crate::write::strip;
use alloc::vec::Vec;

/// The controllers, their sets and the no-internal-process rule:
/// `ferrix-cgroup-model`'s, the same items under this path
/// (`docs/microkernel/CORE-WITHOUT-EXTENSIONS.md` E4).
pub use ferrix_cgroup_model::controllers::{
    ALL, Change, Controller, Set, Standing, vet_destination, vet_enable,
};

/// Append the set as `cgroup.controllers` and `cgroup.subtree_control` print
/// it.
pub fn render(out: &mut Vec<u8>, set: Set) {
    for (index, controller) in set.iter().enumerate() {
        if index != 0 {
            out.push(b' ');
        }
        out.extend_from_slice(controller.name().as_bytes());
    }
    out.push(b'\n');
}

/// Parse a write to `cgroup.subtree_control`. `known` is the set the kernel
/// has built: a name outside it is refused as Linux refuses one whose
/// subsystem is not enabled.
///
/// # Errors
///
/// [`Refusal::Invalid`] for a token with no sign or a name not in `known`.
pub fn parse_change(text: &[u8], known: Set) -> Result<Change, Refusal> {
    let mut change = Change::default();
    for token in strip(text).split(|&byte| byte == b' ') {
        let Some((&sign, name)) = token.split_first() else {
            continue;
        };
        let controller = known
            .iter()
            .find(|controller| controller.name().as_bytes() == name)
            .ok_or(Refusal::Invalid)?;
        match sign {
            b'+' => {
                change.enable = change.enable.with(controller);
                change.disable = change.disable.without(controller);
            }
            b'-' => {
                change.disable = change.disable.with(controller);
                change.enable = change.enable.without(controller);
            }
            _ => return Err(Refusal::Invalid),
        }
    }
    Ok(change)
}
