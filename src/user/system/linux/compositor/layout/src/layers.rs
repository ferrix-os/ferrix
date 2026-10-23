//! Where a layer surface goes, and what it takes away from the windows.
//!
//! `zwlr_layer_shell_v1` gives a client four things to say -- which edges it
//! is anchored to, how big it wants to be, how far from those edges, and how
//! many pixels it reserves -- and the compositor works out a rectangle from
//! them. The rules are wlroots' `wlr_scene_layer_surface_v1_configure`, which
//! Hyprland and every other wlroots-shaped compositor follow:
//!
//! * The surface starts as the whole usable area, less its margins.
//! * An axis anchored to *both* edges keeps that width; anchored to one or
//!   to neither, it takes the size the client asked for and is put against
//!   the edge it is anchored to, or centred if it is anchored to neither.
//! * The exclusive zone is then taken off the usable area for whatever is
//!   configured after it, so two bars on the same edge stack rather than
//!   overlap.
//!
//! Order matters and it is the client's: surfaces are configured in the
//! order they were created, as wlroots does, so a bar that started first
//! gets the edge.

use compositor_config::Gaps;

use crate::Rect;

/// What one surface asked for, free of any protocol object.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Request {
    /// Anchored to the top edge.
    pub top: bool,
    /// To the bottom edge.
    pub bottom: bool,
    /// To the left edge.
    pub left: bool,
    /// To the right edge.
    pub right: bool,
    /// The size it asked for; zero on an axis means the usable area's.
    pub size: (u32, u32),
    /// The margins from the edges, in the protocol's order.
    pub margin: (i32, i32, i32, i32),
    /// Pixels to reserve, or −1 to be left out of the reckoning.
    pub exclusive_zone: i32,
}

/// Where a surface goes and what it reserved.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Placement {
    /// Its rectangle on the monitor.
    pub rect: Rect,
    /// What it took off the usable area.
    pub reserved: Gaps,
}

/// Place each surface in turn on `monitor`, and say what is left for the
/// windows.
///
/// The surfaces are taken in order, and each one's exclusive zone comes off
/// the area the next is placed in, which is what makes two bars on one edge
/// stack. The `Gaps` returned is every zone added up, which is what a
/// monitor's `reserved` is.
///
/// A surface with a zone of 0 is placed after all the others, in what every
/// zone left, as wlroots' arrangement does in its second pass: a
/// notification banner that came before the bar still sits under it. One
/// with a zone of −1 is placed on the whole monitor.
#[must_use]
pub fn place(monitor: Rect, requests: &[Request]) -> (Vec<Placement>, Gaps) {
    let mut usable = monitor;
    let mut total = Gaps::all(0);
    let mut out: Vec<Option<Placement>> = vec![None; requests.len()];
    for (slot, request) in out.iter_mut().zip(requests) {
        if request.exclusive_zone == 0 {
            continue;
        }
        // A zone of −1 asks not to be moved for anybody's zone: the
        // protocol's "will not be moved to accommodate for other surfaces'
        // exclusive zones". It is placed on the whole monitor -- a
        // wallpaper goes under the bar, not below it -- and takes nothing
        // away from anybody.
        let within = if request.exclusive_zone < 0 { monitor } else { usable };
        let placement = one(within, *request);
        usable = shrink(usable, placement.reserved);
        total = add(total, placement.reserved);
        *slot = Some(placement);
    }
    let out = out
        .into_iter()
        .zip(requests)
        .map(|(placed, request)| placed.unwrap_or_else(|| one(usable, *request)))
        .collect();
    (out, total)
}

/// One surface in `usable`.
fn one(usable: Rect, request: Request) -> Placement {
    let (top, right, bottom, left) = (
        i64::from(request.margin.0),
        i64::from(request.margin.1),
        i64::from(request.margin.2),
        i64::from(request.margin.3),
    );
    // The whole usable area, less the margins: where an axis anchored to
    // both edges ends up.
    let mut x = usable.x.saturating_add(left);
    let mut y = usable.y.saturating_add(top);
    let mut width = usable
        .width
        .saturating_sub(left)
        .saturating_sub(right)
        .max(0);
    let mut height = usable
        .height
        .saturating_sub(top)
        .saturating_sub(bottom)
        .max(0);

    if !(request.left && request.right) {
        let wanted = i64::from(request.size.0);
        let asked = if wanted > 0 { wanted } else { width };
        // Against the edge it is anchored to, or centred between them.
        x = if request.left {
            usable.x.saturating_add(left)
        } else if request.right {
            usable.right().saturating_sub(right).saturating_sub(asked)
        } else {
            usable
                .x
                .saturating_add(usable.width.saturating_sub(asked) / 2)
        };
        width = asked.min(usable.width);
    }
    if !(request.top && request.bottom) {
        let wanted = i64::from(request.size.1);
        let asked = if wanted > 0 { wanted } else { height };
        y = if request.top {
            usable.y.saturating_add(top)
        } else if request.bottom {
            usable.bottom().saturating_sub(bottom).saturating_sub(asked)
        } else {
            usable
                .y
                .saturating_add(usable.height.saturating_sub(asked) / 2)
        };
        height = asked.min(usable.height);
    }

    Placement {
        rect: Rect::new(x, y, width, height),
        reserved: reserved(request, (width, height)),
    }
}

/// What a surface of `size` reserves, on the edge it is anchored to.
///
/// wlroots takes the zone off the edge the surface is anchored to and only
/// when it is anchored to exactly one edge of that axis -- a surface
/// stretched across an axis has no edge to reserve from. A zone of zero
/// reserves nothing, and −1 asks to be left out.
fn reserved(request: Request, size: (i64, i64)) -> Gaps {
    if request.exclusive_zone <= 0 {
        return Gaps::all(0);
    }
    let zone = i64::from(request.exclusive_zone);
    let (top, right, bottom, left) = (
        i64::from(request.margin.0),
        i64::from(request.margin.1),
        i64::from(request.margin.2),
        i64::from(request.margin.3),
    );
    let _ = size;
    let mut gaps = Gaps::all(0);
    match (request.top, request.bottom, request.left, request.right) {
        (true, false, _, _) => gaps.top = zone.saturating_add(top),
        (false, true, _, _) => gaps.bottom = zone.saturating_add(bottom),
        (_, _, true, false) => gaps.left = zone.saturating_add(left),
        (_, _, false, true) => gaps.right = zone.saturating_add(right),
        // Stretched across both axes, or anchored to nothing: there is no
        // edge to take the zone off, and wlroots reserves nothing.
        _ => {}
    }
    gaps
}

/// `rect` with `gaps` taken off each side.
fn shrink(rect: Rect, gaps: Gaps) -> Rect {
    Rect::new(
        rect.x.saturating_add(gaps.left),
        rect.y.saturating_add(gaps.top),
        rect.width
            .saturating_sub(gaps.left)
            .saturating_sub(gaps.right)
            .max(0),
        rect.height
            .saturating_sub(gaps.top)
            .saturating_sub(gaps.bottom)
            .max(0),
    )
}

/// Two sets of reserved strips added together.
const fn add(one: Gaps, other: Gaps) -> Gaps {
    Gaps {
        top: one.top.saturating_add(other.top),
        right: one.right.saturating_add(other.right),
        bottom: one.bottom.saturating_add(other.bottom),
        left: one.left.saturating_add(other.left),
    }
}

#[cfg(test)]
mod tests;
