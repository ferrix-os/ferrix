//! What a bind matches, and what a key does when none does.

use compositor_config::{Config, NoSources};

use super::{Action, Hold, Input, Seat};
use compositor_xkb::generated;

/// The evdev codes the tests name.
const KEY_Q: u16 = 16;
const KEY_A: u16 = 30;
const KEY_LEFTSHIFT: u16 = 42;
const KEY_LEFTMETA: u16 = 125;
const BTN_LEFT: u32 = 272;

fn config(text: &str) -> Config {
    compositor_config::parse("test", text, &mut NoSources).config
}

fn seat(text: &str) -> Seat {
    Seat::new(&config(text), 1024, 768)
}

fn press(seat: &mut Seat, code: u16) -> Vec<Action> {
    seat.input(Input::Key {
        code,
        pressed: true,
        repeat: false,
    })
}

fn release(seat: &mut Seat, code: u16) -> Vec<Action> {
    seat.input(Input::Key {
        code,
        pressed: false,
        repeat: false,
    })
}

#[test]
fn a_key_with_no_bind_goes_to_the_window() {
    let mut seat = seat("");
    assert_eq!(
        press(&mut seat, KEY_A),
        [Action::Key {
            code: KEY_A,
            pressed: true
        }]
    );
    assert_eq!(
        release(&mut seat, KEY_A),
        [Action::Key {
            code: KEY_A,
            pressed: false
        }]
    );
}

#[test]
fn a_modifier_key_reports_the_new_state_and_still_reaches_the_window() {
    let mut seat = seat("");
    let actions = press(&mut seat, KEY_LEFTSHIFT);
    assert_eq!(actions.len(), 2, "{actions:?}");
    assert!(matches!(
        actions.first(),
        Some(Action::Modifiers(modifiers)) if modifiers.depressed == generated::SHIFT
    ));
    assert_eq!(
        actions.get(1),
        Some(&Action::Key {
            code: KEY_LEFTSHIFT,
            pressed: true
        })
    );
}

#[test]
fn a_bind_fires_on_its_modifiers_and_eats_the_key() {
    let mut seat = seat("bind = SUPER, Q, killactive\n");
    assert_eq!(seat.binds().0, 1);

    // Without the modifier the key is the window's.
    assert_eq!(
        press(&mut seat, KEY_Q),
        [Action::Key {
            code: KEY_Q,
            pressed: true
        }]
    );
    let _ = release(&mut seat, KEY_Q);

    let _ = press(&mut seat, KEY_LEFTMETA);
    let actions = press(&mut seat, KEY_Q);
    assert_eq!(
        actions,
        [Action::Dispatch {
            name: "killactive".to_owned(),
            argument: String::new(),
            trigger: Some((KEY_Q, generated::MOD4)),
        }],
        "the bind fired and the key did not reach the window"
    );
    // The release is eaten too, so a client is not told a key came up that
    // it was never told went down.
    assert!(release(&mut seat, KEY_Q).is_empty());
}

/// Hyprland compares the modifiers exactly, which is what lets `SUPER, Q` and
/// `SUPER SHIFT, Q` be two different binds.
#[test]
fn a_bind_does_not_fire_with_a_modifier_it_did_not_ask_for() {
    let mut seat = seat("bind = SUPER, Q, killactive\nbind = SUPER SHIFT, Q, exit\n");
    let _ = press(&mut seat, KEY_LEFTMETA);
    let _ = press(&mut seat, KEY_LEFTSHIFT);
    let actions = press(&mut seat, KEY_Q);
    assert_eq!(
        actions,
        [Action::Dispatch {
            name: "exit".to_owned(),
            argument: String::new(),
            trigger: Some((KEY_Q, generated::MOD4 | generated::SHIFT)),
        }]
    );
}

/// A lock a person left on must not stop every bind working.
#[test]
fn caps_lock_is_not_compared() {
    let mut seat = seat("bind = SUPER, Q, killactive\n");
    let _ = press(&mut seat, 58);
    let _ = release(&mut seat, 58);
    assert_eq!(seat.keyboard().modifiers().locked, generated::LOCK);
    let _ = press(&mut seat, KEY_LEFTMETA);
    assert_eq!(press(&mut seat, KEY_Q).len(), 1, "the bind still fires");
}

#[test]
fn the_flags_decide_when_a_bind_fires_and_whether_the_key_goes_on() {
    // `r`: on the release. `n`: the key reaches the client too.
    let mut seat = seat("bindrn = , A, exec, true\n");
    assert_eq!(
        press(&mut seat, KEY_A),
        [Action::Key {
            code: KEY_A,
            pressed: true
        }],
        "a release bind does not fire on the press"
    );
    let actions = release(&mut seat, KEY_A);
    assert_eq!(actions.len(), 2, "{actions:?}");
    assert!(matches!(actions.first(), Some(Action::Dispatch { .. })));
    assert_eq!(
        actions.get(1),
        Some(&Action::Key {
            code: KEY_A,
            pressed: false
        })
    );
}

/// `wl_keyboard.key` has no way to say a repeat, and the client repeats for
/// itself; a compositor that forwarded evdev's repeats would double every
/// held key.
#[test]
fn a_repeat_reaches_no_window_and_only_a_bind_that_asked() {
    let mut seat = seat("binde = , A, resizeactive, 10 0\nbind = , Q, exit\n");
    let repeat = |seat: &mut Seat, code| {
        seat.input(Input::Key {
            code,
            pressed: true,
            repeat: true,
        })
    };
    let _ = press(&mut seat, KEY_A);
    let actions = repeat(&mut seat, KEY_A);
    assert_eq!(
        actions,
        [Action::Dispatch {
            name: "resizeactive".to_owned(),
            argument: "10 0".to_owned(),
            trigger: Some((KEY_A, 0)),
        }]
    );

    let _ = press(&mut seat, KEY_Q);
    assert!(
        repeat(&mut seat, KEY_Q).is_empty(),
        "a bind without `e` fires once and the repeat goes nowhere"
    );
}

#[test]
fn a_bind_on_a_key_the_keymap_does_not_have_is_reported_not_dropped() {
    let seat = seat("bind = SUPER, NoSuchKey, exit\n");
    let (live, unresolved) = seat.binds();
    assert_eq!(live, 0);
    assert_eq!(unresolved.len(), 1);
    assert!(unresolved[0].contains("NoSuchKey"), "{unresolved:?}");
}

/// `code:24` is `q` on a `us` keyboard, because XKB numbers keys eight above
/// evdev and `xev` prints XKB's number.
#[test]
fn a_bind_by_keycode_is_read_as_xkb_numbers_one() {
    let mut seat = seat("bind = , code:24, exit\n");
    assert_eq!(seat.binds().0, 1);
    assert_eq!(
        press(&mut seat, KEY_Q),
        [Action::Dispatch {
            name: "exit".to_owned(),
            argument: String::new(),
            trigger: Some((KEY_Q, 0)),
        }]
    );
}

#[test]
fn the_pointer_starts_in_the_middle_and_stays_on_the_screen() {
    let mut seat = seat("");
    assert_eq!(seat.pointer(), (512.0, 384.0));

    assert_eq!(
        seat.input(Input::Motion {
            dx: 10.0,
            dy: -20.0
        }),
        [
            Action::Relative {
                dx: 10.0,
                dy: -20.0
            },
            Action::Pointer { x: 522.0, y: 364.0 }
        ]
    );
    // Past the edge in both directions, and past the other edge.
    let _ = seat.input(Input::Motion {
        dx: -10_000.0,
        dy: -10_000.0,
    });
    assert_eq!(seat.pointer(), (0.0, 0.0));
    let _ = seat.input(Input::Motion {
        dx: 10_000.0,
        dy: 10_000.0,
    });
    assert_eq!(seat.pointer(), (1023.0, 767.0));

    // A tablet says where it is, as a fraction of the screen.
    let _ = seat.input(Input::Absolute { x: 0.5, y: 0.25 });
    assert_eq!(seat.pointer(), (512.0, 192.0));
    // A device that reported nonsense must not make the position nonsense.
    let _ = seat.input(Input::Absolute {
        x: f64::NAN,
        y: 0.5,
    });
    assert_eq!(seat.pointer(), (0.0, 384.0));
}

#[test]
fn a_button_bind_fires_and_the_button_still_reaches_the_window() {
    let mut seat = seat("bindm = SUPER, mouse:272, movewindow\n");
    let _ = press(&mut seat, KEY_LEFTMETA);
    let actions = seat.input(Input::Button {
        button: BTN_LEFT,
        pressed: true,
    });
    assert_eq!(actions.len(), 2, "{actions:?}");
    assert!(matches!(actions.first(), Some(Action::Dispatch { .. })));
    assert_eq!(
        actions.get(1),
        Some(&Action::Button {
            button: BTN_LEFT,
            pressed: true
        })
    );
}

#[test]
fn a_wheel_bind_fires_on_the_direction_it_asked_for() {
    let mut seat = seat("bind = SUPER, mouse_down, workspace, e+1\n");
    let _ = press(&mut seat, KEY_LEFTMETA);
    let vertical = compositor_protocol::core::wl_pointer::axis::VERTICAL_SCROLL;
    let down = seat.input(Input::Axis {
        axis: vertical,
        value: 15.0,
    });
    assert!(
        matches!(down.first(), Some(Action::Dispatch { .. })),
        "{down:?}"
    );
    let up = seat.input(Input::Axis {
        axis: vertical,
        value: -15.0,
    });
    assert!(matches!(up.first(), Some(Action::Axis { .. })), "{up:?}");
}

/// A submap's binds are live objects -- they have to be, or entering the
/// map would find nothing -- but none of them fires in the global map.
#[test]
fn a_bind_in_a_submap_is_not_in_the_global_map() {
    let mut seat = seat("submap = resize\nbind = , Q, exit\nsubmap = reset\n");
    assert_eq!(seat.binds().0, 1, "the bind was resolved");
    assert_eq!(seat.submap(), "", "the global map is the one in force");
    assert!(
        press(&mut seat, KEY_Q)
            .iter()
            .all(|action| !matches!(action, Action::Dispatch { .. })),
        "a submap's bind fired in the global map"
    );
}

/// And the other way round: in the submap, the submap's bind fires and the
/// global one does not. That is what makes `submap` a mode rather than a
/// prefix, and it is the whole of the feature.
#[test]
fn entering_a_submap_swaps_which_binds_fire() {
    let mut seat = seat(
        "bind = , A, killactive\n\
         submap = resize\n\
         bind = , A, resizeactive, 10 0\n\
         submap = reset\n",
    );
    let fired = |actions: &[Action]| {
        actions.iter().find_map(|action| match action {
            Action::Dispatch { name, .. } => Some(name.clone()),
            _ => None,
        })
    };
    assert_eq!(
        fired(&press(&mut seat, KEY_A)).as_deref(),
        Some("killactive")
    );
    let _ = release(&mut seat, KEY_A);

    assert_eq!(seat.enter_submap("resize"), Ok(true));
    assert_eq!(seat.submap(), "resize");
    assert_eq!(
        fired(&press(&mut seat, KEY_A)).as_deref(),
        Some("resizeactive"),
        "the global bind fired inside the submap"
    );
    let _ = release(&mut seat, KEY_A);

    assert_eq!(seat.enter_submap("reset"), Ok(true));
    assert_eq!(seat.submap(), "");
    assert_eq!(
        fired(&press(&mut seat, KEY_A)).as_deref(),
        Some("killactive")
    );
}

/// The `u` flag is how the bind that leaves a submap is written once: it
/// fires whichever map is in force.
#[test]
fn a_universal_bind_fires_in_every_map() {
    let mut seat = seat(
        "submap = resize\n\
         bindu = , Q, submap, reset\n\
         submap = reset\n",
    );
    let fires = |seat: &mut Seat| {
        let actions = press(seat, KEY_Q);
        let fired = actions
            .iter()
            .any(|action| matches!(action, Action::Dispatch { .. }));
        let _ = release(seat, KEY_Q);
        fired
    };
    assert!(fires(&mut seat), "the universal bind did not fire globally");
    assert_eq!(seat.enter_submap("resize"), Ok(true));
    assert!(
        fires(&mut seat),
        "the universal bind did not fire in the map"
    );
}

/// `setSubmap` refuses a name nothing was bound in, and this refuses it with
/// the same sentence: entering one would leave a keyboard on which nothing
/// works and no bind written to get out of it.
#[test]
fn a_submap_nothing_was_bound_in_is_refused() {
    let mut seat = seat("submap = resize\nbind = , Q, exit\nsubmap = reset\n");
    assert_eq!(
        seat.enter_submap("resiez"),
        Err("Cannot set submap resiez, submap doesn't exist (wasn't registered!)".to_owned())
    );
    assert_eq!(seat.submap(), "", "the refused name was entered anyway");
    // Leaving always works, even from the global map, where it changes
    // nothing and so is not announced.
    assert_eq!(seat.enter_submap("reset"), Ok(false));
}

/// A reload takes the binds away, so it takes the map in force away too: a
/// submap the new configuration does not have is one nothing could leave.
#[test]
fn reading_the_configuration_again_leaves_the_submap() {
    let mut seat = seat("submap = resize\nbind = , Q, exit\nsubmap = reset\n");
    assert_eq!(seat.enter_submap("resize"), Ok(true));
    seat.set_binds(&config("bind = , Q, exit\n"));
    assert_eq!(seat.submap(), "");
}

/// The modifier is usually let go before the key is, so the release cannot be
/// judged on its own: a client told a key came up that it was never told went
/// down has that key stuck down for ever.
#[test]
fn the_release_of_an_eaten_key_is_eaten_even_after_the_modifier_went() {
    let mut seat = seat("bind = SUPER, Q, killactive\n");
    let _ = press(&mut seat, KEY_LEFTMETA);
    assert_eq!(press(&mut seat, KEY_Q).len(), 1, "the bind fired");
    // The person lets go of SUPER first, which is what a hand does.
    let _ = release(&mut seat, KEY_LEFTMETA);
    assert!(
        release(&mut seat, KEY_Q).is_empty(),
        "the release reached the window"
    );
    // And the next press, with no modifier, is the window's again.
    assert_eq!(
        press(&mut seat, KEY_Q),
        [Action::Key {
            code: KEY_Q,
            pressed: true
        }]
    );
}

/// A bind is resolved against the keymap the configuration asked for.
///
/// `bind = SUPER, Z, killactive` is a keysym's *name*, and which key makes
/// that keysym is the keymap's business: on a German keyboard `z` is where
/// an American one has `y`. A compositor that resolved every bind against
/// the US keymap would fire the wrong bind on a German keyboard and give no
/// reason, and `example`'s own configuration is `kb_layout = de`.
#[test]
fn a_bind_is_resolved_against_the_keymap_the_configuration_asked_for() {
    /// `KEY_Y` in evdev, which a German keymap calls `z`.
    const KEY_Y: u16 = 21;
    /// `KEY_Z` in evdev, which a German keymap calls `y`.
    const KEY_Z: u16 = 44;

    let fired = |seat: &mut Seat, code: u16| {
        press(seat, code)
            .iter()
            .any(|action| matches!(action, Action::Dispatch { name, .. } if name == "killactive"))
    };

    let mut german = seat(
        "input:kb_layout = de\n\
         input:kb_variant = nodeadkeys\n\
         bind = , Z, killactive\n",
    );
    assert_eq!(german.layout().described(), "de, nodeadkeys");
    assert!(
        fired(&mut german, KEY_Y),
        "the key a German keyboard calls `z` fires the bind"
    );
    assert!(!fired(&mut german, KEY_Z), "and the other does not");

    let mut american = seat("bind = , Z, killactive\n");
    assert_eq!(american.layout().described(), "us");
    assert!(fired(&mut american, KEY_Z));
    assert!(!fired(&mut american, KEY_Y));
}

/// The seat's keyboard has as many layout groups as the configuration named
/// layouts, and `hyprctl switchxkblayout` moves between exactly those.
///
/// The count is what the numeric form of the request is range-checked
/// against and what `next` wraps around, so a seat that did not take it
/// from the configuration would answer `layout idx out of range of 1` for a
/// keyboard the person configured with three layouts.
#[test]
fn the_keyboard_has_a_group_for_every_layout_the_configuration_named() {
    let mut three = seat("input:kb_layout = de,us,fr\n");
    assert_eq!(three.keyboard().groups(), 3);
    assert_eq!(three.keyboard().group(), 0);

    // A group in force, and the modifiers that say so: a client is told the
    // index and reads the layout out of the keymap it already holds.
    assert!(three.set_layout_group(2));
    assert_eq!(three.modifiers().group, 2);
    // The same group again moves nothing, which is what keeps a repeated
    // request from putting an event on the socket.
    assert!(!three.set_layout_group(2));
    // And past the last comes round, which is what `next` relies on.
    assert!(three.set_layout_group(3));
    assert_eq!(three.keyboard().group(), 0);

    // An unset `input:kb_layout` is one group: the fallback layout.
    let one = seat("");
    assert_eq!(one.keyboard().groups(), 1);
    assert_eq!(super::group_count(&config("input:kb_layout = de,us\n")), 2);
}

#[test]
fn a_screen_placed_away_from_the_corner_is_where_the_pointer_moves() {
    // `monitor = desc:…, preferred, 2560x0, 1` on the only monitor: the
    // pointer starts in its middle and never leaves it, and a tablet's whole
    // range is that monitor and not the empty 2560 pixels left of it.
    let mut seat = seat("");
    seat.place_at(2560, 0);
    assert_eq!(seat.pointer(), (2560.0 + 512.0, 384.0));
    let _ = seat.input(Input::Absolute { x: 0.0, y: 0.0 });
    assert_eq!(seat.pointer(), (2560.0, 0.0));
    let _ = seat.input(Input::Absolute { x: 0.5, y: 0.25 });
    assert_eq!(seat.pointer(), (2560.0 + 512.0, 192.0));
    let _ = seat.input(Input::Motion {
        dx: -10_000.0,
        dy: -10_000.0,
    });
    assert_eq!(seat.pointer(), (2560.0, 0.0));
    let _ = seat.input(Input::Motion {
        dx: 10_000.0,
        dy: 10_000.0,
    });
    assert_eq!(seat.pointer(), (2560.0 + 1023.0, 767.0));
    // The screen moving takes a used pointer with it.
    seat.place_at(0, 0);
    assert_eq!(seat.pointer(), (1023.0, 767.0));
}

#[test]
fn a_locked_pointer_stays_put_and_still_reports_the_distance() {
    let mut seat = seat("");
    seat.set_hold(Hold::Locked);
    assert_eq!(
        seat.input(Input::Motion { dx: 5.0, dy: 7.0 }),
        [Action::Relative { dx: 5.0, dy: 7.0 }]
    );
    assert_eq!(seat.pointer(), (512.0, 384.0));
    // A tablet's distance is between its own reports, held or not.
    assert_eq!(seat.input(Input::Absolute { x: 0.5, y: 0.5 }), []);
    assert_eq!(
        seat.input(Input::Absolute { x: 0.75, y: 0.5 }),
        [Action::Relative { dx: 256.0, dy: 0.0 }]
    );
    assert_eq!(seat.pointer(), (512.0, 384.0));
}
