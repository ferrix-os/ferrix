//! Surface damage turned into buffer pixels, through the scale and the
//! viewport.

use compositor_layout::Rect;

use super::from_surface;

#[test]
fn without_a_viewport_the_scale_multiplies() {
    let rect = Rect::new(10, 20, 30, 40);
    assert_eq!(
        from_surface(rect, 2, Some((800, 600)), None, None),
        Rect::new(20, 40, 60, 80)
    );
}

#[test]
fn a_destination_stretches_the_whole_buffer() {
    // Chromium: a 1080x2400 buffer at scale 1, shown as a 540x1200 surface.
    let whole = Rect::new(0, 0, 540, 1200);
    assert_eq!(
        from_surface(whole, 1, Some((1080, 2400)), None, Some((540, 1200))),
        Rect::new(0, 0, 1080, 2400)
    );
    let part = Rect::new(100, 200, 10, 10);
    assert_eq!(
        from_surface(part, 1, Some((1080, 2400)), None, Some((540, 1200))),
        Rect::new(200, 400, 20, 20)
    );
}

#[test]
fn a_source_is_the_part_stretched() {
    // The right half of a 200x100 buffer at scale 2, shown 100x100.
    let rect = Rect::new(0, 0, 100, 100);
    let source = Some((50.0, 0.0, 50.0, 50.0));
    assert_eq!(
        from_surface(rect, 2, Some((200, 100)), source, Some((100, 100))),
        Rect::new(100, 0, 100, 100)
    );
    // No destination: the surface is the source's size.
    assert_eq!(
        from_surface(Rect::new(0, 0, 50, 50), 2, Some((200, 100)), source, None),
        Rect::new(100, 0, 100, 100)
    );
}

#[test]
fn fractions_round_outward() {
    // A 100-wide buffer over a 30-wide surface: one surface pixel is 3.33.
    assert_eq!(
        from_surface(
            Rect::new(1, 1, 1, 1),
            1,
            Some((100, 100)),
            None,
            Some((30, 30))
        ),
        Rect::new(3, 3, 4, 4)
    );
}

#[test]
fn everything_stays_everything() {
    let all = Rect::new(0, 0, i64::from(i32::MAX), i64::from(i32::MAX));
    let held = from_surface(all, 1, Some((1080, 2400)), None, Some((540, 1200)));
    assert!(held.width >= 1080 && held.height >= 2400);
}

/// A 64x36 screen showing one window over all of it, fullscreen or not,
/// whose pixels come from surface 5 of connection 0; and what a commit to
/// that surface of a `buffer`-sized buffer, damaging its top-left 2x2
/// pixels, changes on the screen.
fn heard_of(buffer: (i64, i64), fullscreen: bool) -> super::Heard {
    use compositor_layout::{MonitorId, MonitorLayout, Placed, WindowId, WorkspaceId};
    let plan = super::Plan {
        size: (64, 36),
        origin: (0, 0),
        scale: 1.0,
        style: compositor_render::Style::default(),
        dark: false,
        locked: false,
        gamma: None,
        layout: MonitorLayout {
            monitor: MonitorId(1),
            workspace: WorkspaceId(1),
            windows: vec![Placed {
                window: WindowId(1),
                rect: Rect::new(0, 0, 64, 36),
                border: 0,
                focused: true,
                floating: false,
                fullscreen,
            }],
        },
        styles: std::collections::BTreeMap::new(),
        layers: Vec::new(),
        cursor: None,
        drag_icon: None,
        plane: None,
        blurred: Vec::new(),
        overlay: None,
        hidden: Vec::new(),
    };
    let sources = std::collections::BTreeMap::from([(
        WindowId(1),
        crate::frame::Source {
            client: 0,
            surface: compositor_wire::ObjectId(5),
        },
    )]);
    let mut told = super::Told::default();
    told.painted(super::Painted {
        client: 0,
        surface: compositor_wire::ObjectId(5),
        buffer: Some(buffer),
        rects: vec![Rect::new(0, 0, 2, 2)],
    });
    told.on(&plan, &sources)
}

/// A fullscreen window's buffer of another shape than the screen is drawn
/// scaled to fit between black bars (`compositor_render::letterboxed`), so
/// its top-left corner is not the screen's: a commit to it is owed the
/// whole window. Mapped as a stretched one is, the 4:3 game's top-left
/// pixels were damaged in the left bar and its own corner kept the frame
/// before.
#[test]
fn a_fitted_fullscreen_buffer_is_owed_its_whole_window() {
    let fitted = heard_of((24, 18), true);
    assert_eq!(fitted.region.area(), 64 * 36, "{:?}", fitted.region);
    // At the screen's size, or stretched in a window that is not
    // fullscreen, the damage is where it was.
    for (buffer, fullscreen) in [((64, 36), true), ((24, 18), false), ((32, 18), true)] {
        let heard = heard_of(buffer, fullscreen);
        assert!(
            heard.region.area() < 64 * 36 / 4,
            "{buffer:?} fullscreen {fullscreen}: {:?}",
            heard.region
        );
    }
}
