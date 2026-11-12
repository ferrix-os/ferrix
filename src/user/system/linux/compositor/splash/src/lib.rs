//! The boot splash from the display driver's start to the compositor's
//! (`docs/SPLASH.md`).
//!
//! The loader paints the FX mark on the firmware's framebuffer and nothing
//! redraws it while the kernel and the drivers start. Once a display driver
//! has the screen, `/bin/splash` draws the same picture on `/dev/dri/card0`
//! and keeps it true: the slash rises as init reaches its targets, one line
//! under the mark says what a silent screen would hide, and Esc shows the
//! boot log. It leaves the card when the compositor's unit asks it to.
//!
//! This library is the part that touches no device: what the boot has said
//! ([`boot`]) and how that is drawn ([`picture`]). The program around it is
//! `main.rs`.

pub mod boot;
pub mod picture;

/// The abstract socket the splash listens on, without its leading NUL: a
/// connection asks it to leave, and its end says the card is free.
pub const SOCKET: &[u8] = b"ferrix.splash";

/// The file `splash --quit` leaves when no splash was listening, so that one
/// still starting leaves the card alone.
pub const LEFT: &str = "/run/splash.left";
