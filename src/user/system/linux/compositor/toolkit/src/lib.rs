//! The Wayland client runtime Ferrix's desktop clients share.
//!
//! the term app, `src/user/system/linux/compositor/lock` and `src/user/system/linux/compositor/pattern` each speak
//! Wayland by hand over `src/user/system/linux/compositor/wire`: fixed object ids, a registry read
//! into a map, a `Reader` loop, a memfd from `src/user/system/linux/compositor/shm`. That is the
//! right size for a test client that makes five objects. waybar, fuzzel,
//! hyprlock and hypridle make surfaces on every screen, follow screens as
//! they come and go, read the keyboard and the pointer, run programs and
//! wait on timers and signals; four more copies of the hand-rolled shape
//! would be four places to fix each bug. This crate is the one copy.
//!
//! # Shape
//!
//! [`Client`] is the connection and everything bound on it. A program
//! connects, looks at [`Client::outputs`], makes the surfaces it wants
//! ([`Client::layer_surface`], [`Client::lock_surface`], [`Client::popup`],
//! [`Client::toplevel`]),
//! and then turns [`Client::dispatch`], which blocks until something happens
//! -- the compositor said something, a timer came due, a child wrote a line,
//! a signal arrived -- and hands back what happened as [`Event`]s. Drawing is
//! [`Client::draw`]: a closure is given a `tiny_skia::PixmapMut` of the
//! surface's size in buffer pixels, and the runtime attaches, damages and
//! commits it.
//!
//! Pull rather than callbacks: the program owns its state and matches on the
//! events, so nothing here needs a trait object or a `RefCell`.
//!
//! # What it binds
//!
//! Everything below, each at the lower of the version this crate speaks and
//! the version the compositor offers ([`Client::bound_version`] says which):
//! `wl_compositor`, `wl_shm`, `wl_seat` (keyboard and pointer), every
//! `wl_output`, `xdg_wm_base` (for popups and windows), `zwlr_layer_shell_v1`,
//! `ext_session_lock_manager_v1`, `wp_cursor_shape_manager_v1`,
//! `ext_idle_notifier_v1`. Anything missing is simply not there: a call that
//! needs it answers [`Error::Missing`].
//!
//! Anything else a program needs -- `zwlr_screencopy_v1`, say -- it binds
//! itself with [`Client::bind`], makes objects with [`Client::new_object`],
//! sends with [`Client::request`] and hears back as [`Event::Object`]. The
//! runtime reads those events with the interface's own table, so nothing
//! about the protocol has to be known here.
//!
//! # Pixels
//!
//! Buffers are `wl_shm` `ARGB8888`, premultiplied, which is the format every
//! compositor must accept. tiny-skia draws premultiplied RGBA, so the runtime
//! swaps red and blue as it copies a frame out; a closure only ever sees
//! tiny-skia's own order. Each surface has two buffers and a third when the
//! compositor holds both (`wl_buffer.release`), so a frame is never drawn
//! into memory the compositor is reading.

mod buffer;
mod client;
mod event;
mod keyboard;
mod keymap;
mod loop_sources;
mod object;
mod output;
mod sources;
mod spawn;
mod surface;

pub use client::{Client, Error, Global};
pub use event::{Event, KeyboardEvent, PointerEvent};
pub use keyboard::{Key, Modifiers};
pub use loop_sources::{ChildId, ChildOutput, Command, TimerId, Waker, WatchId};
pub use object::Value;
pub use output::{Output, OutputId, Transform};
pub use spawn::spawn;
pub use surface::{
    Anchor, CursorShape, IdleId, KeyboardInteractivity, Layer, LayerOptions, Margin, PopupOptions,
    Rect, SurfaceId, ToplevelOptions,
};

/// The protocol tables, for a program that binds something of its own.
pub use compositor_protocol as protocol;
/// Object ids, for [`Client::request`] and [`Event::Object`].
pub use compositor_wire::ObjectId;
/// The rasteriser a surface is drawn with, at the version this crate pins.
pub use tiny_skia;

#[cfg(test)]
mod tests;
