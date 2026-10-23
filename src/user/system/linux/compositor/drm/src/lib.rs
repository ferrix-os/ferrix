//! `/dev/dri/card0`, through the legacy mode-setting calls.
//!
//! What a compositor needs from a screen and nothing else: find a connected
//! connector and a mode, find a CRTC that can drive it, make dumb buffers,
//! map them, and show one. No atomic commit, no GEM import, no render node --
//! `docs/DISPLAY.md` says why, and what stage 19 adds.
//!
//! It is the one crate in the compositor that only builds on Linux, because
//! `/dev/dri` is Linux's and Ferrix's through its Linux ABI. The parts that
//! are arithmetic rather than ioctls -- choosing a mode, choosing a CRTC,
//! filling a buffer, naming a plane -- are in [`modeset`] and are tested on
//! any host.

#[cfg(target_os = "linux")]
mod card;
#[cfg(target_os = "linux")]
mod device;
mod edid;
#[cfg(target_os = "linux")]
mod isolation;
#[cfg(target_os = "linux")]
mod master;
pub mod modeset;
#[cfg(target_os = "linux")]
mod render;

#[cfg(target_os = "linux")]
pub use card::{
    Card, Dumb, Flusher, Imported, News, Plan, Shown, cards, connected, plan, planes, plans,
    plans_beside, rename, show,
};
#[cfg(target_os = "linux")]
pub use device::RenderDevice;
pub use edid::{Edid, registered};
#[cfg(target_os = "linux")]
pub use isolation::{PREFIX as ISOLATION, isolation};
#[cfg(target_os = "linux")]
pub use master::{BLANK as MASTER_BLANK, PREFIX as MASTER, bystander, master};
#[cfg(target_os = "linux")]
pub use render::{Box3d, Mapping, Render, ResourceCreate, probe};

#[cfg(test)]
mod tests;
