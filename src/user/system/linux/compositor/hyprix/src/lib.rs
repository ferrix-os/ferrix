//! The compositor.
//!
//! A Hyprland-shaped Wayland compositor: it reads a `hyprland.conf`, listens
//! on a Wayland socket, tiles the windows that connect to it with the dwindle
//! or master layout, draws them on the CPU and puts the result on a screen.
//!
//! # What it is made of
//!
//! Nothing here parses a configuration file, works out a layout, draws a
//! pixel or decodes a message: `src/user/system/linux/compositor/config`, `src/user/system/linux/compositor/layout`,
//! `src/user/system/linux/compositor/render` and `src/user/system/linux/compositor/server` do those, and each of them is
//! host-tested without a socket or a screen. This binary is the loop that
//! joins them -- accept, read, lay out, draw, show -- and the two places it
//! touches the world: a client's shared memory, and the screen.
//!
//! # Where the screen is
//!
//! Two backends. `--headless` draws into memory and can write each frame out,
//! which is how the compositor is tested on any machine and how its pixels
//! are compared without a display. `/dev/dri/card0` is the real one on
//! Ferrix, through the same legacy mode-setting `src/user/system/linux/compositor/blank` proved.

pub mod act;
pub mod animate;
pub mod backend;
pub(crate) mod children;
pub mod clipboard;
pub mod command;
pub mod control;
pub(crate) mod damage;
pub mod deliver;
pub mod devices;
pub mod dmabuf;
pub mod dragging;
#[cfg(target_os = "linux")]
mod flush;
pub mod frame;
pub mod grants;
pub mod keymap;
pub mod options;
pub mod overlay;
pub(crate) mod pace;
pub mod plane;
pub mod plugins;
pub mod pool;
pub mod rules;
#[cfg(target_os = "linux")]
pub(crate) mod scanouts;
pub mod scope;
pub mod seat;
pub mod select;
pub mod state;
pub(crate) mod wait;

pub use options::{Options, Renderer};
pub use state::{run, run_with};
