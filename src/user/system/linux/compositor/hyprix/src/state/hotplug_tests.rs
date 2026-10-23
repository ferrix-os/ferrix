//! A monitor unplugged and plugged back in (the N150's TV on its one
//! port): the card of the last screen is kept to hear it come back, and
//! the screens take the places they had.

use super::{STAND_IN, last_on_card, next_x};

/// The last screen on a card keeps it open; one of two on a card, a screen
/// on no card, and a card kept already do not.
#[test]
fn the_last_screen_on_a_card_keeps_it() {
    assert_eq!(
        last_on_card(&[Some(7)], 0, &[]),
        Some(7),
        "the TV on port A"
    );
    assert_eq!(
        last_on_card(&[Some(7), Some(7)], 0, &[]),
        None,
        "the card has another"
    );
    assert_eq!(last_on_card(&[Some(7), Some(9)], 1, &[]), Some(9));
    assert_eq!(last_on_card(&[None], 0, &[]), None, "a headless screen");
    assert_eq!(last_on_card(&[Some(7)], 0, &[7]), None, "kept already");
    assert_eq!(last_on_card(&[Some(7)], 3, &[]), None);
}

/// The stand-in does not push a monitor plugged back in to its right: the
/// TV comes back at 0,0, where it was, not at 1920,0.
#[test]
fn a_monitor_plugged_back_in_takes_its_place() {
    assert_eq!(next_x(&[(STAND_IN, 1920)]), 0);
    assert_eq!(next_x(&[("HDMI-A-1", 1920)]), 1920);
    assert_eq!(next_x(&[("HDMI-A-1", 1920), (STAND_IN, 3840)]), 1920);
    assert_eq!(next_x(&[]), 0);
}
