//! What [`crate::Client::dispatch`] hands back.

use crate::{
    ChildId, IdleId, Key, Modifiers, ObjectId, OutputId, SurfaceId, TimerId, Value, WatchId,
};

/// Something that happened.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// A screen came, and its first `wl_output.done` arrived: its
    /// [`crate::Output`] is complete.
    OutputAdded(OutputId),
    /// A screen's description changed after it was added (a new mode, a
    /// new scale), closed by another `done`.
    OutputChanged(OutputId),
    /// A screen went. Surfaces on it are closed by the compositor, which
    /// arrives as [`Event::Closed`].
    OutputRemoved(OutputId),
    /// The compositor gave a surface its size, in logical pixels. The
    /// configure has been acknowledged; the next [`crate::Client::draw`]
    /// draws at this size. A zero is "choose yourself".
    Configure {
        /// Which surface.
        surface: SurfaceId,
        /// Width.
        width: u32,
        /// Height.
        height: u32,
    },
    /// Who draws a window's frame changed, or was first said: `own` is
    /// true where the program draws its own title bar and buttons -- the
    /// compositor answered `client_side`, or has no decoration manager --
    /// and false where the compositor draws them. Sent for a window that
    /// said who it wanted ([`crate::ToplevelOptions::own_frame`]), with
    /// the configure that commits it ([`crate::Client::own_frame`]).
    Decorated {
        /// Which window.
        surface: SurfaceId,
        /// Whether the program draws its own frame.
        own: bool,
    },
    /// The compositor took a surface away: a layer surface's `closed`, a
    /// popup's `popup_done`. The program should forget it and
    /// [`crate::Client::destroy`] it.
    Closed(SurfaceId),
    /// A person asked for a window to close (`xdg_toplevel.close`). Nothing
    /// has been torn down: the window stays until the program
    /// [`crate::Client::destroy`]s it, which it may do at once, or after
    /// asking whether to save.
    CloseRequested(SurfaceId),
    /// The frame callback [`crate::Client::request_frame`] asked for: now is
    /// a good time to draw the next frame.
    Frame {
        /// Which surface.
        surface: SurfaceId,
        /// The compositor's timestamp in milliseconds.
        time: u32,
    },
    /// The integer scale a surface's buffers should now be drawn at changed
    /// (`wl_surface.preferred_buffer_scale`, or the scale of the screen it
    /// entered where the compositor does not send that).
    Scale {
        /// Which surface.
        surface: SurfaceId,
        /// The scale.
        scale: u32,
    },
    /// The keyboard.
    Keyboard(KeyboardEvent),
    /// The pointer.
    Pointer(PointerEvent),
    /// `ext_session_lock_v1.locked`: every screen is covered.
    Locked,
    /// `ext_session_lock_v1.finished`: the compositor refused the lock or
    /// ended it. The program must not unlock; it should exit.
    LockFinished,
    /// `ext_idle_notification_v1.idled`.
    Idled(IdleId),
    /// `ext_idle_notification_v1.resumed`.
    Resumed(IdleId),
    /// A timer came due.
    Timer(TimerId),
    /// A line a child wrote to its standard output, without its newline.
    ChildLine {
        /// Which child.
        child: ChildId,
        /// The line.
        line: String,
    },
    /// A child's output ended and it was reaped.
    ChildExited {
        /// Which child.
        child: ChildId,
        /// Its exit code, or `None` if a signal ended it.
        status: Option<i32>,
        /// Its standard output, for [`crate::ChildOutput::Whole`]; what was
        /// left after the last newline, for [`crate::ChildOutput::Lines`].
        output: String,
    },
    /// A signal [`crate::Client::watch_signals`] asked for arrived.
    Signal(i32),
    /// A descriptor [`crate::Client::watch_fd`] watches is readable.
    Readable(WatchId),
    /// A [`crate::Waker`] was woken.
    Woken,
    /// An event for an object the program made itself
    /// ([`crate::Client::bind`], [`crate::Client::new_object`]).
    Object {
        /// The object.
        object: ObjectId,
        /// Its interface's name.
        interface: &'static str,
        /// The event's opcode.
        opcode: u16,
        /// Its arguments, owned.
        args: Vec<Value>,
    },
}

/// What the keyboard did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyboardEvent {
    /// A surface got the keyboard.
    Enter(SurfaceId),
    /// It lost it. Any held key stops repeating.
    Leave(SurfaceId),
    /// A key went down, came up or repeated.
    Key(Key),
    /// The modifiers changed.
    Modifiers(Modifiers),
}

/// What the pointer did. Positions are in the surface's logical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PointerEvent {
    /// It came onto a surface.
    Enter {
        /// Which.
        surface: SurfaceId,
        /// Where across.
        x: f64,
        /// Where down.
        y: f64,
    },
    /// It left one.
    Leave {
        /// Which.
        surface: SurfaceId,
    },
    /// It moved.
    Motion {
        /// Over which.
        surface: SurfaceId,
        /// Where across.
        x: f64,
        /// Where down.
        y: f64,
    },
    /// A button went down or up. `button` is the evdev code: `0x110` left,
    /// `0x111` right, `0x112` middle.
    Button {
        /// Over which.
        surface: SurfaceId,
        /// Where across.
        x: f64,
        /// Where down.
        y: f64,
        /// Which button.
        button: u32,
        /// Down or up.
        pressed: bool,
        /// The event's serial, which a grabbing popup needs.
        serial: u32,
    },
    /// The wheel. Positive is down (or right), in the protocol's units,
    /// with `discrete` the wheel's clicks where it sends them.
    Axis {
        /// Over which.
        surface: SurfaceId,
        /// Down (positive) or up.
        vertical: f64,
        /// Right (positive) or left.
        horizontal: f64,
        /// Clicks, vertical then horizontal: `wl_pointer.axis_discrete`, or
        /// `axis_value120` / 120.
        discrete: (i32, i32),
    },
}
