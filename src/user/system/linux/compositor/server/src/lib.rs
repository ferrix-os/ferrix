//! The Wayland server: what a client's requests do.
//!
//! This crate is the protocol half of the compositor and holds no socket, no
//! descriptor and no pixel. A [`Client`] is handed the bytes that arrived and
//! the descriptors that came with them, and gives back the bytes to send; what
//! moves those bytes is the binary above it. So the whole of the protocol --
//! object lifetimes, versions, every way a client can break the rules and what
//! the server answers -- is host-tested, and running it on Ferrix tests the
//! socket rather than the protocol.
//!
//! # How a request is answered
//!
//! [`Client::read`] takes whole messages out of a buffer. For each one it
//! finds the object the message is addressed to, reads the signature of that
//! interface's request at that opcode, decodes the arguments and hands them to
//! the handler for the object's role. A handler may create objects, destroy
//! them, queue events, and tell the compositor above that something changed.
//!
//! # A protocol error is the end of the connection
//!
//! Wayland has no way to refuse one request and carry on: `wl_display.error`
//! names the object, a code its interface defines and a sentence, and the
//! connection is finished. So every refusal here goes through
//! [`Client::fail`], which queues that event once and stops reading. The
//! caller writes out what is queued and closes the socket. Nothing is answered
//! after the first error, which is what libwayland does and what a client
//! expects: the alternative, answering the rest of the buffer, hands a client
//! that already broke the rules more state to break.
//!
//! # What is here and what is not
//!
//! `wl_display` and `wl_registry` are here, and with them the objects a client
//! needs to put a picture somewhere: `wl_compositor` and its surfaces and
//! regions, and `wl_shm` with its pools and buffers. The globals a client can
//! bind are declared in [`Globals`], and what a bound object is is [`Role`].
//! `xdg_shell` is here too, which is how a surface becomes a window: the
//! configure conversation by which the compositor and the client agree on a
//! size, and the toplevel state a tiling layout needs. `wl_seat`, which is
//! how a window is typed into, lands after this.

mod client;
mod globals;
mod layer;
mod role;
mod shm;
mod surface;
mod xdg;

pub use client::{
    Client, Configuration, Constraint, DRM_FORMAT_ARGB8888, DRM_FORMAT_XRGB8888, Dmabuf, Dragging,
    Event, Export, Fatal, Flavour, ForeignRequest, ForeignToplevel, Frame, GAMMA_SIZE, Hotkey,
    Injected, Listener, MOD_INVALID, MOD_LINEAR, Manager, Outgoing, Plane, Shortcut, Source, Typed,
    Wanted, Workspace, WorkspaceRequest, format_of, offer_linear_only,
};
pub use globals::{Global, Globals};
pub use layer::{Anchors, Layer, LayerSurface, Margin};
pub use role::Role;
pub use shm::{Buffer, BufferError, FORMATS, Format, Pool, PoolKey};
pub use surface::{Committed, Output, Rect, Region, State, Subsurface, Surface};
pub use xdg::{Popup, Positioner, Toplevel, XdgRole, XdgSurface};

pub use compositor_protocol as protocol;
pub use compositor_wire as wire;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod dmabuf_tests;
