//! Where a layer surface goes, against wlroots' own rules.

use compositor_config::Gaps;

use super::{Request, place};
use crate::Rect;

const MONITOR: Rect = Rect {
    x: 0,
    y: 0,
    width: 1024,
    height: 768,
};

fn bar(height: u32, zone: i32) -> Request {
    Request {
        top: true,
        left: true,
        right: true,
        size: (0, height),
        exclusive_zone: zone,
        ..Request::default()
    }
}

#[test]
fn a_bar_anchored_across_the_top_is_the_width_of_the_monitor() {
    let (placed, reserved) = place(MONITOR, &[bar(30, 30)]);
    assert_eq!(placed[0].rect, Rect::new(0, 0, 1024, 30));
    assert_eq!(
        reserved,
        Gaps {
            top: 30,
            ..Gaps::all(0)
        }
    );
}

/// A wallpaper is anchored to all four edges and asks for nothing: it is the
/// whole monitor, and it reserves nothing because there is no edge to
/// reserve from.
#[test]
fn a_wallpaper_is_the_whole_monitor_and_reserves_nothing() {
    let wallpaper = Request {
        top: true,
        bottom: true,
        left: true,
        right: true,
        exclusive_zone: -1,
        ..Request::default()
    };
    let (placed, reserved) = place(MONITOR, &[wallpaper]);
    assert_eq!(placed[0].rect, MONITOR);
    assert_eq!(reserved, Gaps::all(0));
}

/// Two bars on one edge stack rather than overlap: the second is placed in
/// what is left after the first's zone.
#[test]
fn two_bars_on_one_edge_stack() {
    let (placed, reserved) = place(MONITOR, &[bar(30, 30), bar(20, 20)]);
    assert_eq!(placed[0].rect, Rect::new(0, 0, 1024, 30));
    assert_eq!(placed[1].rect, Rect::new(0, 30, 1024, 20));
    assert_eq!(reserved.top, 50);
}

#[test]
fn a_margin_moves_a_surface_off_its_edge_and_is_reserved_with_it() {
    let mut floating = bar(30, 30);
    floating.margin = (10, 5, 0, 5);
    let (placed, reserved) = place(MONITOR, &[floating]);
    assert_eq!(placed[0].rect, Rect::new(5, 10, 1014, 30));
    // wlroots adds the margin to the zone, so the windows start below both.
    assert_eq!(reserved.top, 40);
}

#[test]
fn a_surface_anchored_to_one_edge_takes_the_size_it_asked_for() {
    let side = Request {
        left: true,
        top: true,
        bottom: true,
        size: (200, 0),
        exclusive_zone: 200,
        ..Request::default()
    };
    let (placed, reserved) = place(MONITOR, &[side]);
    assert_eq!(placed[0].rect, Rect::new(0, 0, 200, 768));
    assert_eq!(reserved.left, 200);
}

/// Anchored to neither edge of an axis, it is centred on that axis: a
/// launcher in the middle of the screen.
#[test]
fn a_surface_anchored_to_nothing_is_centred() {
    let launcher = Request {
        size: (400, 200),
        exclusive_zone: -1,
        ..Request::default()
    };
    let (placed, reserved) = place(MONITOR, &[launcher]);
    assert_eq!(placed[0].rect, Rect::new(312, 284, 400, 200));
    assert_eq!(reserved, Gaps::all(0));
}

#[test]
fn a_bar_on_the_bottom_and_one_on_the_right_reserve_their_own_edges() {
    let bottom = Request {
        bottom: true,
        left: true,
        right: true,
        size: (0, 24),
        exclusive_zone: 24,
        ..Request::default()
    };
    let right = Request {
        right: true,
        top: true,
        bottom: true,
        size: (64, 0),
        exclusive_zone: 64,
        ..Request::default()
    };
    let (placed, reserved) = place(MONITOR, &[bottom, right]);
    assert_eq!(placed[0].rect, Rect::new(0, 744, 1024, 24));
    // The second is placed in what is left, which is the monitor less the
    // bottom bar.
    assert_eq!(placed[1].rect, Rect::new(960, 0, 64, 744));
    assert_eq!(
        reserved,
        Gaps {
            top: 0,
            right: 64,
            bottom: 24,
            left: 0
        }
    );
}

/// A zone of zero reserves nothing but is still placed, which is what a
/// surface that wants to sit on an edge without pushing the windows asks
/// for.
#[test]
fn a_zone_of_zero_places_and_reserves_nothing() {
    let (placed, reserved) = place(MONITOR, &[bar(30, 0)]);
    assert_eq!(placed[0].rect, Rect::new(0, 0, 1024, 30));
    assert_eq!(reserved, Gaps::all(0));
}

/// A surface larger than the monitor is cut to it rather than drawn outside
/// it, and one placed in an area a previous zone has used up is empty rather
/// than negative.
#[test]
fn nothing_is_placed_outside_the_monitor_or_with_a_negative_size() {
    let (placed, _) = place(MONITOR, &[bar(2000, 0)]);
    assert_eq!(placed[0].rect.height, 768);

    let (placed, reserved) = place(MONITOR, &[bar(768, 768), bar(30, 30)]);
    assert_eq!(placed[0].rect.height, 768);
    assert_eq!(placed[1].rect.height, 0);
    assert!(reserved.top >= 768);
}

/// A surface that moves for others' zones (zone 0) is placed in what they
/// left even when it came first: a banner that started before the bar.
#[test]
fn a_zone_of_zero_moves_for_a_bar_that_came_later() {
    let banner = Request {
        top: true,
        size: (400, 100),
        margin: (4, 0, 0, 0),
        exclusive_zone: 0,
        ..Request::default()
    };
    let (placed, reserved) = place(MONITOR, &[banner, bar(30, 30)]);
    assert_eq!(placed[0].rect, Rect::new(312, 34, 400, 100));
    assert_eq!(placed[1].rect, Rect::new(0, 0, 1024, 30));
    assert_eq!(reserved.top, 30);
}

/// A zone of −1 is not moved for a bar: a wallpaper anchored to every edge
/// is the whole monitor under the bar, as swaybg's is on wlroots.
#[test]
fn a_zone_of_minus_one_covers_the_monitor_whatever_came_first() {
    let wallpaper = Request {
        top: true,
        bottom: true,
        left: true,
        right: true,
        exclusive_zone: -1,
        ..Request::default()
    };
    let (placed, reserved) = place(MONITOR, &[bar(30, 30), wallpaper]);
    assert_eq!(placed[1].rect, MONITOR);
    assert_eq!(reserved.top, 30);
}
