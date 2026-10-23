//! The protocols a desktop session asks for beyond a window and a bar.
//!
//! Eleven of them, each small, and all bound by something a person runs:
//! a bar reads `xdg-output` for the screen's logical size and name, a
//! toolkit asks `presentation-time` when its frame actually reached the
//! screen, an idle daemon waits on `ext-idle-notify` and a video player
//! holds it off with `idle-inhibit`, a compositor-side shadow is one
//! `single-pixel-buffer`, a player says "this is video" through
//! `content-type` and fades itself with `alpha-modifier`, a dialog says it
//! is modal, a terminal rings the bell, a toolkit tags its windows, and KDE
//! software asks who draws the title bar in KDE's own words.
//!
//! They are here rather than in `client.rs` because that file is the window
//! protocols and is long enough; a child module sees its parent's private
//! fields, so nothing had to be opened up to move them.

use std::time::Duration;

use compositor_protocol::alpha_modifier::{wp_alpha_modifier_surface_v1, wp_alpha_modifier_v1};
use compositor_protocol::content_type::{wp_content_type_manager_v1, wp_content_type_v1};
use compositor_protocol::idle_inhibit::zwp_idle_inhibit_manager_v1;
use compositor_protocol::idle_notify::{ext_idle_notification_v1, ext_idle_notifier_v1};
use compositor_protocol::kde_decoration::{
    org_kde_kwin_server_decoration, org_kde_kwin_server_decoration_manager,
};
use compositor_protocol::presentation::{wp_presentation, wp_presentation_feedback};
use compositor_protocol::single_pixel::wp_single_pixel_buffer_manager_v1;
use compositor_protocol::system_bell::xdg_system_bell_v1;
use compositor_protocol::toplevel_tag::xdg_toplevel_tag_manager_v1;
use compositor_protocol::xdg_dialog::{xdg_dialog_v1, xdg_wm_dialog_v1};
use compositor_protocol::xdg_output::{zxdg_output_manager_v1, zxdg_output_v1};
use compositor_protocol::{
    alpha_modifier, content_type, idle_inhibit, idle_notify, kde_decoration, presentation,
    xdg_dialog, xdg_output,
};
use compositor_wire::{Arg, ArgType, ObjectId};

use crate::client::{Client, Event, Fatal};
use crate::role::Role;
use crate::shm::Buffer;
use crate::surface::Surface;

/// One `ext_idle_notification_v1`: how long it waits, and whether it has
/// said the seat went idle.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Idle {
    /// The client's timeout, in milliseconds.
    timeout: u32,
    /// Whether `idled` has been sent and `resumed` has not.
    idled: bool,
    /// Whether an inhibitor holds it off: `get_input_idle_notification`
    /// asks for one that ignores them.
    inhibitable: bool,
}

impl Client {
    /// Answer a request to one of this module's objects.
    ///
    /// Gives whether the role was one of them, so `client.rs`'s table can
    /// hand on everything it does not know itself.
    pub(super) fn desktop(
        &mut self,
        sender: ObjectId,
        role: Role,
        version: u32,
        opcode: u16,
        args: &[Arg<'_>],
    ) -> bool {
        match role {
            Role::XdgOutputManager => self.xdg_output_manager(version, opcode, args),
            Role::Presentation => self.presentation(version, opcode, args),
            Role::IdleNotifier => self.idle_notifier(version, opcode, args),
            Role::IdleInhibitManager => self.idle_inhibit_manager(version, opcode, args),
            Role::SinglePixelManager => self.single_pixel_manager(version, opcode, args),
            Role::ContentTypeManager => self.content_type_manager(version, opcode, args),
            Role::ContentType => self.content_type(sender, opcode, args),
            Role::AlphaModifier => self.alpha_manager(version, opcode, args),
            Role::AlphaSurface => self.alpha_surface(sender, opcode, args),
            Role::DialogManager => self.dialog_manager(version, opcode, args),
            Role::Dialog => self.dialog(sender, opcode),
            Role::SystemBell => self.system_bell(opcode, args),
            Role::ToplevelTagManager => self.toplevel_tag_manager(opcode, args),
            Role::KdeDecorationManager => self.kde_manager(version, opcode, args),
            Role::KdeDecoration => self.kde_decoration(sender, opcode, args),
            // Not one of this module's: `client.rs` reads it, or nothing
            // does.
            _ => return false,
        }
        true
    }

    /// Drop what one of this module's objects held.
    pub(super) fn forget_desktop(&mut self, id: ObjectId, role: Role) {
        match role {
            Role::XdgOutput => {
                let _ = self.xdg_outputs.remove(&id);
            }
            Role::IdleNotification => {
                let _ = self.idles.remove(&id);
            }
            Role::IdleInhibitor => {
                let _ = self.inhibitors.remove(&id);
            }
            Role::ContentType => {
                let _ = self.contents.remove(&id);
            }
            Role::AlphaSurface => {
                // The protocol says destroying it puts the surface back to
                // fully opaque, on the next commit like every other part of
                // a surface's state.
                if let Some(surface) = self.alphas.remove(&id)
                    && let Some(state) = self.surfaces.get_mut(&surface)
                {
                    state.pending.alpha = None;
                }
            }
            Role::Dialog => {
                if let Some(toplevel) = self.dialogs.remove(&id) {
                    self.events.push(Event::ToplevelModal {
                        toplevel,
                        modal: false,
                    });
                }
            }
            Role::KdeDecoration => {
                let _ = self.kde_decorations.remove(&id);
            }
            Role::ToplevelDecoration => {
                let _ = self.xdg_decorations.remove(&id);
            }
            _ => {}
        }
    }

    /// `zxdg_output_manager_v1.get_xdg_output`: a screen's logical position,
    /// size, name and description.
    ///
    /// Every bar and every screen-sharing portal asks for this rather than
    /// reading `wl_output`, because `wl_output.mode` is in buffer pixels and
    /// what a bar needs is the logical ones the compositor lays windows out
    /// in. Both are sent here, and on a scaled monitor they differ.
    fn xdg_output_manager(&mut self, version: u32, opcode: u16, args: &[Arg<'_>]) {
        if opcode != zxdg_output_manager_v1::request::GET_XDG_OUTPUT {
            return;
        }
        let (Some(id), Some(output)) = (
            args.first().and_then(Arg::as_object),
            args.get(1).and_then(Arg::as_object),
        ) else {
            return;
        };
        let Some(which) = self.output_objects.get(&output).copied() else {
            self.fail(Fatal::WrongInterface {
                object: output,
                wanted: "wl_output",
            });
            return;
        };
        if !self.make(id, &xdg_output::ZXDG_OUTPUT_V1, version, Role::XdgOutput) {
            return;
        }
        let _ = self.xdg_outputs.insert(id, which);
        let Some(screen) = self.outputs.get(which).cloned() else {
            return;
        };
        // The logical size is the screen's own divided by its scale, which
        // is what every window's rectangle is in -- and turned, for a
        // monitor stood on its edge: its mode is wide and what is laid out
        // on it is tall.
        let scale = screen.scale.max(1);
        let turned = screen.transform & 1 == 1;
        let (wide, high) = if turned {
            (screen.height, screen.width)
        } else {
            (screen.width, screen.height)
        };
        let _ = self.out.write(
            id,
            zxdg_output_v1::event::LOGICAL_POSITION,
            &[ArgType::Int, ArgType::Int],
            &[Arg::Int(screen.x), Arg::Int(screen.y)],
        );
        let _ = self.out.write(
            id,
            zxdg_output_v1::event::LOGICAL_SIZE,
            &[ArgType::Int, ArgType::Int],
            &[Arg::Int(wide / scale), Arg::Int(high / scale)],
        );
        let _ = self.out.write(
            id,
            zxdg_output_v1::event::NAME,
            &[ArgType::Str { nullable: false }],
            &[Arg::Str(Some(&screen.name))],
        );
        let _ = self.out.write(
            id,
            zxdg_output_v1::event::DESCRIPTION,
            &[ArgType::Str { nullable: false }],
            &[Arg::Str(Some(&screen.description))],
        );
        let _ = self.out.write(id, zxdg_output_v1::event::DONE, &[], &[]);
    }

    /// `wp_presentation.feedback`: tell this client when its frame reached
    /// the screen.
    ///
    /// A toolkit that animates reads the presentation time to know how far
    /// ahead to draw the next frame; without it, it guesses from the frame
    /// callback, which fires when the compositor *began* the frame.
    fn presentation(&mut self, version: u32, opcode: u16, args: &[Arg<'_>]) {
        if opcode != wp_presentation::request::FEEDBACK {
            return;
        }
        let (Some(surface), Some(id)) = (
            args.first().and_then(Arg::as_object),
            args.get(1).and_then(Arg::as_object),
        ) else {
            return;
        };
        if !self.surfaces.contains_key(&surface) {
            self.fail(Fatal::WrongInterface {
                object: surface,
                wanted: "wl_surface",
            });
            return;
        }
        if !self.make(
            id,
            &presentation::WP_PRESENTATION_FEEDBACK,
            version,
            Role::PresentationFeedback,
        ) {
            return;
        }
        self.feedback.entry(surface).or_default().push(id);
    }

    /// `ext_idle_notifier_v1`: both its requests, which differ only in
    /// whether an inhibitor holds the notification off.
    fn idle_notifier(&mut self, version: u32, opcode: u16, args: &[Arg<'_>]) {
        let inhibitable = match opcode {
            ext_idle_notifier_v1::request::GET_IDLE_NOTIFICATION => true,
            ext_idle_notifier_v1::request::GET_INPUT_IDLE_NOTIFICATION => false,
            _ => return,
        };
        let (Some(id), Some(timeout)) = (
            args.first().and_then(Arg::as_object),
            args.get(1).and_then(Arg::as_uint),
        ) else {
            return;
        };
        if !self.make(
            id,
            &idle_notify::EXT_IDLE_NOTIFICATION_V1,
            version,
            Role::IdleNotification,
        ) {
            return;
        }
        let _ = self.idles.insert(
            id,
            Idle {
                timeout,
                idled: false,
                inhibitable,
            },
        );
    }

    /// `zwp_idle_inhibit_manager_v1.create_inhibitor`: while this lives and
    /// its surface is visible, the seat does not go idle.
    fn idle_inhibit_manager(&mut self, version: u32, opcode: u16, args: &[Arg<'_>]) {
        if opcode != zwp_idle_inhibit_manager_v1::request::CREATE_INHIBITOR {
            return;
        }
        let (Some(id), Some(surface)) = (
            args.first().and_then(Arg::as_object),
            args.get(1).and_then(Arg::as_object),
        ) else {
            return;
        };
        if !self.surfaces.contains_key(&surface) {
            self.fail(Fatal::WrongInterface {
                object: surface,
                wanted: "wl_surface",
            });
            return;
        }
        if !self.make(
            id,
            &idle_inhibit::ZWP_IDLE_INHIBITOR_V1,
            version,
            Role::IdleInhibitor,
        ) {
            return;
        }
        let _ = self.inhibitors.insert(id, surface);
    }

    /// `wp_single_pixel_buffer_manager_v1.create_u32_rgba_buffer`: a
    /// `wl_buffer` one pixel across, of one colour.
    ///
    /// It exists so that a client can put a coloured rectangle on the screen
    /// -- a shadow, a dim layer, a solid background -- without sharing a
    /// megabyte of memory holding the same four bytes over and over. The
    /// buffer has no pool: the colour *is* the buffer.
    fn single_pixel_manager(&mut self, version: u32, opcode: u16, args: &[Arg<'_>]) {
        if opcode != wp_single_pixel_buffer_manager_v1::request::CREATE_U32_RGBA_BUFFER {
            return;
        }
        let numbers: Vec<u32> = args.iter().filter_map(Arg::as_uint).collect();
        let (Some(id), [red, green, blue, alpha]) =
            (args.first().and_then(Arg::as_object), numbers.as_slice())
        else {
            return;
        };
        if !self.make(
            id,
            &compositor_protocol::core::WL_BUFFER,
            version,
            Role::Buffer,
        ) {
            return;
        }
        // The protocol's channels are 32-bit; the renderer's are eight, and
        // the top byte is the one that survives.
        let byte = |value: u32| (value >> 24) as u8;
        let _ = self.buffers.insert(
            id,
            Buffer::solid(byte(*alpha), byte(*red), byte(*green), byte(*blue)),
        );
    }

    /// `wp_content_type_manager_v1.get_surface_content_type`.
    fn content_type_manager(&mut self, version: u32, opcode: u16, args: &[Arg<'_>]) {
        if opcode != wp_content_type_manager_v1::request::GET_SURFACE_CONTENT_TYPE {
            return;
        }
        let (Some(id), Some(surface)) = (
            args.first().and_then(Arg::as_object),
            args.get(1).and_then(Arg::as_object),
        ) else {
            return;
        };
        if !self.surfaces.contains_key(&surface) {
            self.fail(Fatal::WrongInterface {
                object: surface,
                wanted: "wl_surface",
            });
            return;
        }
        if !self.make(
            id,
            &content_type::WP_CONTENT_TYPE_V1,
            version,
            Role::ContentType,
        ) {
            return;
        }
        let _ = self.contents.insert(id, surface);
    }

    /// `wp_content_type_v1.set_content_type`: what the surface is showing.
    ///
    /// Recorded and not acted on: a compositor uses it to choose a refresh
    /// rate or to leave tearing on for a game, and both of those are the
    /// GPU path's. A client that says it is a video is answered, which is
    /// what stops the toolkit warning.
    fn content_type(&mut self, sender: ObjectId, opcode: u16, args: &[Arg<'_>]) {
        if opcode != wp_content_type_v1::request::SET_CONTENT_TYPE {
            return;
        }
        let (Some(surface), Some(kind)) = (
            self.contents.get(&sender).copied(),
            args.first().and_then(Arg::as_uint),
        ) else {
            return;
        };
        if let Some(state) = self.surfaces.get_mut(&surface) {
            state.pending.content = kind;
        }
    }

    /// `wp_alpha_modifier_v1.get_surface`.
    fn alpha_manager(&mut self, version: u32, opcode: u16, args: &[Arg<'_>]) {
        if opcode != wp_alpha_modifier_v1::request::GET_SURFACE {
            return;
        }
        let (Some(id), Some(surface)) = (
            args.first().and_then(Arg::as_object),
            args.get(1).and_then(Arg::as_object),
        ) else {
            return;
        };
        if !self.surfaces.contains_key(&surface) {
            self.fail(Fatal::WrongInterface {
                object: surface,
                wanted: "wl_surface",
            });
            return;
        }
        if !self.make(
            id,
            &alpha_modifier::WP_ALPHA_MODIFIER_SURFACE_V1,
            version,
            Role::AlphaSurface,
        ) {
            return;
        }
        let _ = self.alphas.insert(id, surface);
    }

    /// `wp_alpha_modifier_surface_v1.set_multiplier`: how much of the
    /// surface shows.
    ///
    /// The whole range of a `u32` is the protocol's, with `u32::MAX` fully
    /// opaque; the renderer's is a fraction, and this is where the two meet.
    fn alpha_surface(&mut self, sender: ObjectId, opcode: u16, args: &[Arg<'_>]) {
        if opcode != wp_alpha_modifier_surface_v1::request::SET_MULTIPLIER {
            return;
        }
        let (Some(surface), Some(factor)) = (
            self.alphas.get(&sender).copied(),
            args.first().and_then(Arg::as_uint),
        ) else {
            return;
        };
        if let Some(state) = self.surfaces.get_mut(&surface) {
            #[expect(
                clippy::cast_precision_loss,
                reason = "a fraction of u32::MAX is a fraction; the low bits are below what a byte of alpha can show anyway"
            )]
            let fraction = factor as f32 / u32::MAX as f32;
            state.pending.alpha = Some(fraction);
        }
    }

    /// `xdg_wm_dialog_v1.get_xdg_dialog`.
    fn dialog_manager(&mut self, version: u32, opcode: u16, args: &[Arg<'_>]) {
        if opcode != xdg_wm_dialog_v1::request::GET_XDG_DIALOG {
            return;
        }
        let (Some(id), Some(toplevel)) = (
            args.first().and_then(Arg::as_object),
            args.get(1).and_then(Arg::as_object),
        ) else {
            return;
        };
        if !self.toplevels.contains_key(&toplevel) {
            self.fail(Fatal::WrongInterface {
                object: toplevel,
                wanted: "xdg_toplevel",
            });
            return;
        }
        if !self.make(id, &xdg_dialog::XDG_DIALOG_V1, version, Role::Dialog) {
            return;
        }
        let _ = self.dialogs.insert(id, toplevel);
    }

    /// `xdg_dialog_v1`: `set_modal` and `unset_modal`.
    ///
    /// A modal dialog is one the application will not let you look past, so
    /// a tiling compositor floats it: Hyprland's `windowrule = float,
    /// xdg_dialog` is the same thing written by hand, and this is the
    /// protocol saying it for itself.
    fn dialog(&mut self, sender: ObjectId, opcode: u16) {
        let modal = match opcode {
            xdg_dialog_v1::request::SET_MODAL => true,
            xdg_dialog_v1::request::UNSET_MODAL => false,
            _ => return,
        };
        let Some(toplevel) = self.dialogs.get(&sender).copied() else {
            return;
        };
        if let Some(top) = self.toplevels.get_mut(&toplevel) {
            top.modal = modal;
        }
        self.events.push(Event::ToplevelModal { toplevel, modal });
    }

    /// `xdg_system_bell_v1.ring`: the terminal bell.
    ///
    /// There is nothing to ring -- this compositor has no sound -- so the
    /// event goes up and the compositor says so. That is more than the
    /// warning a toolkit logs when the global is missing, and it is what a
    /// notification daemon would hang off.
    fn system_bell(&mut self, opcode: u16, args: &[Arg<'_>]) {
        if opcode != xdg_system_bell_v1::request::RING {
            return;
        }
        let surface = args.first().and_then(Arg::as_object);
        self.events.push(Event::Bell { surface });
    }

    /// `xdg_toplevel_tag_manager_v1`: a name a window keeps across
    /// restarts, and a sentence describing it.
    ///
    /// A toolkit sets these so that a session manager can put a window back
    /// where it was. They are recorded on the toplevel, which is where
    /// `hyprctl clients` and a `windowrule` can reach them.
    fn toplevel_tag_manager(&mut self, opcode: u16, args: &[Arg<'_>]) {
        let (Some(toplevel), Some(text)) = (
            args.first().and_then(Arg::as_object),
            args.get(1).and_then(Arg::as_str),
        ) else {
            return;
        };
        let Some(top) = self.toplevels.get_mut(&toplevel) else {
            return;
        };
        match opcode {
            xdg_toplevel_tag_manager_v1::request::SET_TOPLEVEL_TAG => {
                top.tag = text.to_owned();
            }
            xdg_toplevel_tag_manager_v1::request::SET_TOPLEVEL_DESCRIPTION => {
                top.description = text.to_owned();
            }
            _ => {}
        }
    }

    /// `org_kde_kwin_server_decoration_manager.create`.
    ///
    /// KDE's own answer to `xdg-decoration`, which a good deal of software
    /// still asks first. The answer is the same one: the compositor draws
    /// the decoration, because a tiling compositor draws the border and a
    /// client that drew its own would draw a second one inside it.
    fn kde_manager(&mut self, version: u32, opcode: u16, args: &[Arg<'_>]) {
        if opcode != org_kde_kwin_server_decoration_manager::request::CREATE {
            return;
        }
        let (Some(id), Some(surface)) = (
            args.first().and_then(Arg::as_object),
            args.get(1).and_then(Arg::as_object),
        ) else {
            return;
        };
        if !self.surfaces.contains_key(&surface) {
            self.fail(Fatal::WrongInterface {
                object: surface,
                wanted: "wl_surface",
            });
            return;
        }
        if !self.make(
            id,
            &kde_decoration::ORG_KDE_KWIN_SERVER_DECORATION,
            version,
            Role::KdeDecoration,
        ) {
            return;
        }
        let _ = self.kde_decorations.insert(id, surface);
        self.kde_mode(id);
    }

    /// `org_kde_kwin_server_decoration.request_mode`: answered with the
    /// server's, whatever was asked for.
    fn kde_decoration(&mut self, sender: ObjectId, opcode: u16, args: &[Arg<'_>]) {
        if opcode != org_kde_kwin_server_decoration::request::REQUEST_MODE {
            return;
        }
        let _ = args;
        self.kde_mode(sender);
    }

    /// Tell one `org_kde_kwin_server_decoration` that the server draws it.
    fn kde_mode(&mut self, id: ObjectId) {
        let _ = self.out.write(
            id,
            org_kde_kwin_server_decoration::event::MODE,
            &[ArgType::Uint],
            &[Arg::Uint(org_kde_kwin_server_decoration::mode::SERVER)],
        );
    }

    /// Tell every `wp_presentation_feedback` owed for `surface` that the
    /// frame reached the screen.
    ///
    /// The time is the compositor's clock as seconds and nanoseconds, and
    /// `refresh` how long a frame lasts in nanoseconds, which is what the
    /// protocol's own fields are. The flags say what kind of presentation it was: this
    /// compositor copies into the screen's buffer itself, so it is neither
    /// zero-copy nor hardware-completed, and the copy is what a vsync is
    /// here.
    pub fn presented(
        &mut self,
        surface: ObjectId,
        (seconds, nanos): (u64, u32),
        refresh: u32,
        sequence: u64,
    ) {
        let Some(owed) = self.feedback.remove(&surface) else {
            return;
        };
        for id in owed {
            let _ = self.out.write(
                id,
                wp_presentation_feedback::event::PRESENTED,
                &[
                    ArgType::Uint,
                    ArgType::Uint,
                    ArgType::Uint,
                    ArgType::Uint,
                    ArgType::Uint,
                    ArgType::Uint,
                    ArgType::Uint,
                ],
                &[
                    Arg::Uint(u32::try_from(seconds >> 32).unwrap_or(0)),
                    Arg::Uint(u32::try_from(seconds & 0xffff_ffff).unwrap_or(0)),
                    Arg::Uint(nanos),
                    Arg::Uint(refresh),
                    Arg::Uint(u32::try_from(sequence >> 32).unwrap_or(0)),
                    Arg::Uint(u32::try_from(sequence & 0xffff_ffff).unwrap_or(0)),
                    Arg::Uint(wp_presentation_feedback::kind::VSYNC),
                ],
            );
            // The object is the client's and the protocol says it is
            // destroyed by this event, so the server drops it here.
            let _ = self.objects.remove(id);
        }
    }

    /// Tell every `wp_presentation_feedback` owed for `surface` that its
    /// frame was never shown, which is what an unmapped window's is.
    pub fn discarded(&mut self, surface: ObjectId) {
        let Some(owed) = self.feedback.remove(&surface) else {
            return;
        };
        for id in owed {
            let _ = self
                .out
                .write(id, wp_presentation_feedback::event::DISCARDED, &[], &[]);
            let _ = self.objects.remove(id);
        }
    }

    /// Whether this client holds an idle inhibitor on a surface that is
    /// showing something.
    ///
    /// An inhibitor on an unmapped surface holds nothing off: the protocol
    /// says it applies "while the surface is visible", and a video player
    /// whose window is gone is not playing anything.
    #[must_use]
    pub fn inhibits_idle(&self) -> bool {
        self.inhibitors
            .values()
            .any(|surface| self.surfaces.get(surface).is_some_and(Surface::is_mapped))
    }

    /// Tell each `ext_idle_notification_v1` whether the seat has gone idle.
    ///
    /// `idle` is how long the seat has been without input, in milliseconds,
    /// and `inhibited` whether anything is holding it off. Gives whether
    /// anything was said, so the loop knows it has to flush.
    pub fn idle_tick(&mut self, idle: u64, inhibited: bool) -> bool {
        let mut said = false;
        let mut changes = Vec::new();
        for (id, notification) in &self.idles {
            let held = inhibited && notification.inhibitable;
            let now = !held && idle >= u64::from(notification.timeout);
            if now != notification.idled {
                changes.push((*id, now));
            }
        }
        for (id, now) in changes {
            if let Some(notification) = self.idles.get_mut(&id) {
                notification.idled = now;
            }
            let event = if now {
                ext_idle_notification_v1::event::IDLED
            } else {
                ext_idle_notification_v1::event::RESUMED
            };
            let _ = self.out.write(id, event, &[], &[]);
            said = true;
        }
        said
    }

    /// How long until this client needs another idle check.
    ///
    /// Notifications that have already said `idled`, or that an inhibitor is
    /// holding off, cannot change until input or another client request wakes
    /// the compositor.  The remaining one with the shortest timeout decides
    /// when the event loop's timer expires.
    #[must_use]
    pub fn idle_wait(&self, idle: u64, inhibited: bool) -> Option<Duration> {
        self.idles
            .values()
            .filter(|notification| !notification.idled && !(inhibited && notification.inhibitable))
            .map(|notification| {
                Duration::from_millis(u64::from(notification.timeout).saturating_sub(idle))
            })
            .min()
    }

    /// What each of this client's surfaces is showing, for the compositor:
    /// the alpha `wp_alpha_modifier_v1` set on it, if any.
    #[must_use]
    pub fn surface_alpha(&self, surface: ObjectId) -> Option<f32> {
        self.surfaces.get(&surface)?.current.alpha
    }
}
