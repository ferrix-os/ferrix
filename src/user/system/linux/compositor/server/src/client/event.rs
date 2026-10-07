//! What a client's requests leave for the compositor above to do.
//!
//! A request the protocol alone can answer is answered where it is read.
//! The rest -- a window to place, a selection to hand round, a screen to
//! copy, a key a launcher asked for -- become one of these, and the
//! compositor takes them all with
//! [`Client::take_events`](super::Client::take_events) after each read.
//!
//! Every protocol family in `client/` adds its own, so the list is longer
//! than any one family's code; it is here, on its own, so that a reader of
//! `client.rs` sees the connection and not the catalogue.

use compositor_wire::{Fd, Fixed, ObjectId};

use crate::client::{ForeignRequest, Injected, Source, Typed, Wanted, WorkspaceRequest};
use crate::role::Role;
use crate::shm::{Pool, PoolKey};
use crate::surface::{Committed, Rect};

/// Something the compositor above has to act on, which the protocol alone
/// cannot answer.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Event {
    /// The client bound a global. The compositor learns of every binding so
    /// it can send what a fresh object is owed -- `wl_shm`'s formats, a
    /// seat's capabilities, an output's mode.
    Bound {
        /// The object the client made.
        object: ObjectId,
        /// What it is.
        role: Role,
        /// The version it was bound at, which is at most the global's.
        version: u32,
    },
    /// The client destroyed an object.
    Destroyed {
        /// The object that is gone.
        object: ObjectId,
        /// What it was.
        role: Role,
    },
    /// `xdg_dialog_v1`: the toplevel said whether it is modal. A modal
    /// dialog floats, which is what Hyprland does with one.
    ToplevelModal {
        /// The `xdg_toplevel`.
        toplevel: ObjectId,
        /// Whether it is modal now.
        modal: bool,
    },
    /// A virtual device asked the seat to do something, which the
    /// compositor hands on as if a real one had reported it.
    Injected(Injected),
    /// `zwlr_gamma_control_v1.set_gamma`: the three ramps on a descriptor,
    /// or `None` where the control was destroyed and the screen goes back
    /// to what it was.
    Gamma {
        /// Which screen, by its place in the outputs.
        output: usize,
        /// The descriptor the ramps are on.
        table: Option<Fd>,
    },
    /// `zwlr_output_power_v1.set_mode`: turn a screen off or on.
    OutputPower {
        /// Which screen.
        output: usize,
        /// Whether it is to be on.
        on: bool,
    },
    /// A clipboard manager made a device, which is owed both selections at
    /// once whether or not it has the keyboard.
    DataControlBound {
        /// The device.
        device: ObjectId,
    },
    /// A clipboard manager put something on a selection.
    DataControlSelection {
        /// Its source, or `None` for giving the selection up.
        source: Option<ObjectId>,
        /// Whether it is the primary selection.
        primary: bool,
        /// The types the source offered.
        mimes: Vec<String>,
    },
    /// A clipboard manager asked for what is on a selection.
    DataControlPaste {
        /// The offer it asked through.
        offer: ObjectId,
        /// The type it asked for.
        mime: String,
        /// The pipe to write it to.
        fd: Fd,
        /// Whether it is the primary selection.
        primary: bool,
    },
    /// A program asked for the screens to be arranged.
    OutputConfigured {
        /// The configuration, which is owed `succeeded` or `failed`.
        configuration: ObjectId,
        /// Whether it only asked whether the arrangement would work.
        testing: bool,
        /// What it asks each screen to become.
        heads: Vec<(usize, Wanted)>,
    },
    /// A bar asked for something to be done to a workspace.
    WorkspaceAsked {
        /// Which workspace, by the number the compositor calls it.
        workspace: i64,
        /// What it asked for.
        what: WorkspaceRequest,
    },
    /// `ext_workspace_manager_v1.commit`: carry out what was asked.
    WorkspacesCommitted,
    /// `hyprland_focus_grab_v1.commit`: a launcher asking for the focus to
    /// stay on its own surfaces until a click lands outside them.
    FocusGrabbed {
        /// The grab.
        grab: ObjectId,
        /// The surfaces it covers.
        surfaces: Vec<ObjectId>,
    },
    /// `hyprland_toplevel_export_manager_v1.capture_toplevel`: a screenshot
    /// of one window, which is owed the size of the buffer to make.
    ToplevelExportAsked {
        /// The frame.
        frame: ObjectId,
        /// Which window, by the address every other protocol calls it.
        window: u64,
    },
    /// The client made that buffer and handed it over.
    ToplevelExportCopy {
        /// The frame.
        frame: ObjectId,
        /// Which window.
        window: u64,
        /// The `wl_buffer` to write into.
        buffer: ObjectId,
    },
    /// `ext_image_copy_capture_manager_v1.create_session`: a recorder
    /// began taking frames out of a screen or a window, and is owed the
    /// size of the buffer to make.
    CaptureSession {
        /// The session.
        session: ObjectId,
        /// What it is looking at.
        source: Source,
    },
    /// `ext_image_copy_capture_frame_v1.capture`: the client made the
    /// buffer and handed it over.
    CaptureAsked {
        /// The frame.
        frame: ObjectId,
        /// What to copy.
        source: Source,
        /// The `wl_buffer` to write into.
        buffer: ObjectId,
    },
    /// `wp_pointer_warp_v1.warp_pointer`: a client asked for the pointer
    /// to be put somewhere inside its own window.
    PointerWarped {
        /// The surface it named, which is one of its own.
        surface: ObjectId,
        /// Where inside it, in the protocol's own fixed point so that
        /// what the compositor acts on is the number the client sent.
        at: (Fixed, Fixed),
        /// The serial of the input event it quoted.
        serial: u32,
    },
    /// `wp_security_context_v1.commit`: a sandbox handed over a socket of
    /// its own to accept connections on.
    SecurityContext {
        /// The listening socket.
        listener: Fd,
        /// The descriptor that is closed when the sandbox ends.
        close: Fd,
        /// What made the sandbox: `flatpak`, `snap`.
        engine: String,
        /// What is running in it.
        app_id: String,
        /// Which instance of it.
        instance: String,
    },
    /// `wl_data_device.start_drag`: a client began a drag.
    DragStarted {
        /// What it is dragging, or `None` for an icon with nothing on it.
        source: Option<ObjectId>,
        /// The surface the drag started on.
        origin: ObjectId,
        /// The surface drawn at the pointer, if it gave one.
        icon: Option<ObjectId>,
        /// The serial of the press that began it.
        serial: u32,
        /// The types the source can give the data in.
        mimes: Vec<String>,
    },
    /// `wl_data_offer.accept`: the client under the pointer said which type
    /// it would take, or `None` for none of them.
    DragAccepted {
        /// The offer it said it on.
        offer: ObjectId,
        /// The type, or `None`.
        mime: Option<String>,
    },
    /// `wl_data_offer.set_actions`: what the target will do with it.
    DragActions {
        /// The offer.
        offer: ObjectId,
        /// What it can do.
        actions: u32,
        /// Which of those it would rather.
        preferred: u32,
    },
    /// `wl_data_offer.finish`: the target has taken what it took, which is
    /// when a move may delete the original.
    DragFinished {
        /// The offer.
        offer: ObjectId,
    },
    /// `xdg_system_bell_v1.ring`: the terminal bell, for the surface that
    /// rang it or for the whole seat.
    Bell {
        /// The `wl_surface`, or `None` for the seat.
        surface: Option<ObjectId>,
    },
    /// A surface's pending state became current. What it shows may have
    /// changed, and so may the region it takes input in.
    SurfaceCommitted {
        /// The surface.
        surface: ObjectId,
        /// What the commit did.
        change: Committed,
    },
    /// A pool was made over a descriptor the client sent. The compositor
    /// above maps it; nothing here touches it.
    PoolCreated {
        /// Which pool, by its key rather than its object id, which the
        /// client may give to another object once this one is destroyed.
        pool: PoolKey,
        /// The pool's descriptor and size.
        memory: Pool,
    },
    /// A surface became a window. The layout has to place it, and the
    /// compositor has to configure it before the client may attach a buffer.
    ToplevelCreated {
        /// The `xdg_toplevel`.
        toplevel: ObjectId,
        /// The `wl_surface` under it.
        surface: ObjectId,
    },
    /// A surface became a layer surface: a bar, a wallpaper, a launcher.
    /// The compositor has to place it and configure it before the client
    /// may attach a buffer.
    LayerSurfaceCreated {
        /// The `zwlr_layer_surface_v1`.
        layer_surface: ObjectId,
        /// The `wl_surface` under it.
        surface: ObjectId,
    },
    /// A layer surface changed something the compositor places it by: its
    /// anchor, its size, its margin, its zone or its layer.
    LayerSurfaceChanged {
        /// The `zwlr_layer_surface_v1`.
        layer_surface: ObjectId,
    },
    /// A client made a `wl_data_device`. Whatever the selection holds has
    /// to be offered to it, since a client that binds after a copy must
    /// still be able to paste.
    DataDeviceMade {
        /// The `wl_data_device`.
        device: ObjectId,
    },
    /// A client set the selection: it copied something. The compositor
    /// remembers which client and which source, and offers it to the
    /// others.
    SelectionSet {
        /// The `wl_data_source`, or `None` for a selection being cleared.
        source: Option<ObjectId>,
        /// The types it offered, in order.
        mimes: Vec<String>,
    },
    /// A client asked for the selection's data on a descriptor: it pasted.
    /// The compositor passes the descriptor to whoever owns the selection.
    SelectionWanted {
        /// The `wl_data_offer` it asked through.
        offer: ObjectId,
        /// The type it asked for.
        mime: String,
        /// Where the data is to be written.
        fd: Fd,
    },
    /// A program asked for a screenshot of a screen: it is owed the size
    /// and format of the buffer it must make.
    ScreencopyWanted {
        /// The `zwlr_screencopy_frame_v1` it will be given.
        frame: ObjectId,
        /// Which screen, by its place in the outputs the globals advertise.
        output: usize,
        /// The part of it, or `None` for all of it.
        region: Option<Rect>,
    },
    /// A program handed over the buffer its screenshot is to be written
    /// into.
    ScreencopyInto {
        /// The frame it belongs to.
        frame: ObjectId,
        /// The `wl_buffer`.
        buffer: ObjectId,
        /// Which screen, by its place in the outputs.
        output: usize,
        /// The part of it, or `None` for all of it.
        region: Option<Rect>,
        /// Whether `copy_with_damage` was used, which asks for a `damage`
        /// event before `ready`.
        with_damage: bool,
    },
    /// A program locked the session. Everything else stops being drawn and
    /// stops being given input until it unlocks.
    SessionLocked {
        /// The `ext_session_lock_v1` it holds.
        lock: ObjectId,
    },
    /// It made the surface for one screen, which the compositor is to
    /// configure at that screen's size and then draw instead of everything.
    SessionLockSurfaceMade {
        /// The `ext_session_lock_surface_v1`.
        lock_surface: ObjectId,
        /// The `wl_surface` under it.
        surface: ObjectId,
        /// Which screen, by its place in the outputs.
        output: usize,
    },
    /// It unlocked, or it went away while holding the lock. The payload
    /// says which: a lock that was *released* leaves the screen to the
    /// windows again, and one whose client died leaves it locked with
    /// nothing drawn on it, which is what the protocol requires.
    SessionUnlocked {
        /// The `ext_session_lock_v1` it was asked on: only the lock the
        /// compositor gave this client may be unlocked, never one it refused.
        lock: ObjectId,
        /// Whether the client asked, rather than having gone.
        asked: bool,
    },
    /// An application said it wants to be typed into through an input
    /// method, or that it no longer does.
    TextInputEnabled {
        /// Its `zwp_text_input_v3`.
        text_input: ObjectId,
        /// Whether it is enabled now.
        enabled: bool,
    },
    /// It said what is around the cursor, which an input method uses to
    /// guess the next word.
    TextInputSurrounded {
        /// Its `zwp_text_input_v3`.
        text_input: ObjectId,
        /// The text.
        text: String,
        /// Where the cursor is in it, in bytes.
        cursor: i32,
        /// Where the selection's other end is.
        anchor: i32,
    },
    /// It said where the cursor is on the screen, which is where an input
    /// method puts its candidate window.
    TextInputCursorAt {
        /// Its `zwp_text_input_v3`.
        text_input: ObjectId,
        /// The rectangle, in the surface's own coordinates.
        rect: Rect,
    },
    /// It applied everything it has said since the last commit.
    TextInputCommitted {
        /// Its `zwp_text_input_v3`.
        text_input: ObjectId,
    },
    /// A program became the input method for the seat.
    InputMethodMade {
        /// Its `zwp_input_method_v2`.
        method: ObjectId,
    },
    /// The input method typed something.
    InputMethodTyped {
        /// Its `zwp_input_method_v2`.
        method: ObjectId,
        /// What it typed.
        typed: Typed,
    },
    /// The input method went.
    InputMethodGone {
        /// Its `zwp_input_method_v2`.
        method: ObjectId,
    },
    /// A client named the cursor it wants rather than drawing one.
    CursorShaped {
        /// Which of `wp_cursor_shape_device_v1`'s shapes.
        shape: u32,
    },
    /// A client made a `zwp_primary_selection_device_v1`: it can paste the
    /// primary selection, and is owed whatever is in it.
    PrimaryDeviceMade {
        /// The device.
        device: ObjectId,
    },
    /// A client set the primary selection, which is what a middle click
    /// pastes.
    PrimarySet {
        /// The source, or `None` for one being cleared.
        source: Option<ObjectId>,
        /// The types it offered.
        mimes: Vec<String>,
    },
    /// A client asked for the primary selection's data on a descriptor.
    PrimaryWanted {
        /// The offer it asked through.
        offer: ObjectId,
        /// The type it asked for.
        mime: String,
        /// Where the data is to be written.
        fd: Fd,
    },
    /// A client asked for another program's window to be focused, with a
    /// token the compositor gave out.
    ActivationAsked {
        /// The token it was given.
        token: String,
        /// The `wl_surface` it wants raised.
        surface: ObjectId,
    },
    /// A client said what the pointer looks like over its windows.
    CursorSet {
        /// The surface it drew, or `None` for a pointer it wants hidden.
        surface: Option<ObjectId>,
        /// Where in that surface the pointer is.
        hotspot: (i32, i32),
    },
    /// A client made a popup: a menu, a tooltip, a dropdown. It has to be
    /// placed and configured before it may draw anything at all.
    PopupCreated {
        /// The `xdg_popup`.
        popup: ObjectId,
        /// The `wl_surface` its pixels come from.
        surface: ObjectId,
        /// The `xdg_surface` it hangs off.
        parent: ObjectId,
    },
    /// It asked for a grab, which is what makes a menu a menu: the keyboard
    /// is the popup's until it is dismissed, and a click outside dismisses
    /// it.
    PopupGrabbed {
        /// The `xdg_popup`.
        popup: ObjectId,
    },
    /// A popup went.
    PopupGone {
        /// The `xdg_popup` that was destroyed.
        popup: ObjectId,
    },
    /// A bar asked the compositor to do something to a window it does not
    /// own, through `zwlr_foreign_toplevel_handle_v1`.
    ForeignToplevelAsked {
        /// The window, as the compositor numbered it.
        window: u64,
        /// What was asked for.
        what: ForeignRequest,
    },
    /// A window's title or app id changed, which `hyprctl clients` prints
    /// and `windowrule` matches on.
    ToplevelRenamed {
        /// The `xdg_toplevel`.
        toplevel: ObjectId,
    },
    /// A window asked for a state a tiling compositor answers by
    /// configuring: `set_maximized`, `set_fullscreen` and their opposites.
    ToplevelAsked {
        /// The `xdg_toplevel`.
        toplevel: ObjectId,
        /// Whether it asked to be maximized.
        maximized: bool,
        /// Whether it asked to be fullscreen.
        fullscreen: bool,
    },
    /// A window asked to be moved or resized by the pointer
    /// (`xdg_toplevel.move` and `resize`): a press on its own title bar or
    /// edge, handed to the compositor.
    ToplevelDragAsked {
        /// The `xdg_toplevel`.
        toplevel: ObjectId,
        /// The `xdg_toplevel.resize_edge` pulled, `NONE` for a move.
        edges: u32,
    },
    /// The client destroyed a pool's object. Its memory lives on while
    /// buffers cut from it do ([`Client::pool_in_use`](super::Client::pool_in_use)).
    PoolRetired {
        /// Which pool, by its key.
        pool: PoolKey,
    },
    /// A `zwp_linux_buffer_params_v1.add` took a descriptor off the
    /// connection. The binary holds it from now on: it imports it when
    /// [`Event::DmabufCreated`] names these parameters, and closes it when
    /// they are destroyed without a buffer.
    DmabufPlane {
        /// The parameters object.
        params: ObjectId,
        /// The descriptor, which nothing here touches again.
        fd: Fd,
    },
    /// A dmabuf buffer was asked for. The binary imports the descriptor its
    /// parameters were given and answers with
    /// [`Client::dmabuf_imported`](super::Client::dmabuf_imported).
    DmabufCreated {
        /// What the buffer is.
        dmabuf: super::dmabuf::Dmabuf,
    },
    /// A pool grew. Whatever mapped it has to map it again.
    PoolResized {
        /// Which pool, by its key.
        pool: PoolKey,
        /// Its size now.
        size: i32,
    },
}
