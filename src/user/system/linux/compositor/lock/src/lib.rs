//! Locking the screen, from outside the compositor.
//!
//! `hyprlock` is what a person runs on Hyprland and `swaylock` on sway;
//! both speak `ext-session-lock-v1` and nothing else, and neither is on
//! Ferrix. This is the same protocol in the smallest honest form: bind the
//! manager, ask for the lock, make a surface for every screen, draw
//! something on each, wait to be told the screen is covered, and unlock.
//!
//! What it does *not* do is ask for a password. A lock screen that asked
//! would need a password to check, which Ferrix has no notion of yet; the
//! part that can be tested is the part that matters to the compositor --
//! that the windows stop being drawn the moment the lock is taken, that the
//! keyboard stops reaching them, and that the screen comes back when the
//! lock is let go.

/// The Wayland client.
pub mod client;

pub use client::{Locked, lock, lock_drawing, lock_until};
