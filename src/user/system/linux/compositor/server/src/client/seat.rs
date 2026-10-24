//! `wl_seat`: the keyboard and the pointer, and what the pointer looks like.
//!
//! A seat is one person's input. A client binds it, is told which of a
//! keyboard, a pointer and a touchscreen it has, and asks for an object for
//! each. The requests are few -- `wl_pointer.set_cursor` and
//! `cursor-shape-v1`'s `set_shape` are all a client says about its input --
//! and the events are the rest: the keymap, the focus coming and going,
//! each key and button, each motion and each click of the wheel. The
//! compositor sends those through the methods here, to whichever client
//! has the focus.
//!
//! The pointer and keyboard protocols beyond these, from relative motion to
//! virtual keyboards, are in `input.rs`.

use compositor_protocol::core::{self, wl_seat};
use compositor_protocol::cursor_shape::{
    self, wp_cursor_shape_device_v1, wp_cursor_shape_manager_v1,
};
use compositor_wire::{Arg, ArgType, Fd, Fixed, ObjectId};
use compositor_xkb::Modifiers;

use crate::client::{Client, Event, Fatal};
use crate::role::Role;

/// How far one wheel click scrolls, in surface coordinates.
///
/// libinput reports a click as 15 units and every toolkit expects that, so
/// `src/user/system/linux/compositor/hyprix`'s devices turn a wheel notch into this much distance
/// -- and this is where it is turned back, because `wl_pointer`'s own unit
/// for a click is 120 and a client reads the two together.
const WHEEL_STEP: f64 = 15.0;

impl Client {
    /// `wl_seat`: the keyboard, the pointer and the touchscreen.
    ///
    /// A client may only ask for a capability the seat announced, and this
    /// one announces what [`Client::set_seat_capabilities`] was told. Asking
    /// for one it did not is `missing_capability`, which is what the protocol
    /// says and what keeps a client from waiting for events that will never
    /// come.
    pub(super) fn seat(&mut self, version: u32, opcode: u16, args: &[Arg<'_>]) {
        let Some(id) = args.first().and_then(Arg::as_object) else {
            return;
        };
        let (interface, role, capability, what) = match opcode {
            wl_seat::request::GET_POINTER => (
                &core::WL_POINTER,
                Role::Pointer,
                wl_seat::capability::POINTER,
                "pointer",
            ),
            wl_seat::request::GET_KEYBOARD => (
                &core::WL_KEYBOARD,
                Role::Keyboard,
                wl_seat::capability::KEYBOARD,
                "keyboard",
            ),
            wl_seat::request::GET_TOUCH => (
                &core::WL_TOUCH,
                Role::Touch,
                wl_seat::capability::TOUCH,
                "touch",
            ),
            _ => return,
        };
        if self.capabilities & capability == 0 {
            self.fail(Fatal::Interface {
                object: id,
                code: wl_seat::error::MISSING_CAPABILITY,
                text: format!("this seat has no {what}"),
            });
            return;
        }
        if !self.make(id, interface, version, role) {
            return;
        }
        if role == Role::Keyboard {
            self.send_keymap(id);
            // `repeat_info` arrived in version 4, and a client that does not
            // get it repeats at whatever it chooses -- or, for a toolkit that
            // waits for it, not at all.
            if version >= 4 {
                let (rate, delay) = self.repeat;
                let _ = self.out.write(
                    id,
                    core::wl_keyboard::event::REPEAT_INFO,
                    &[ArgType::Int, ArgType::Int],
                    &[Arg::Int(rate), Arg::Int(delay)],
                );
            }
        }
    }

    /// Give a fresh `wl_keyboard` the keymap, or say there is none.
    ///
    /// `wl_keyboard.keymap` must be sent before anything else, and a client
    /// that is given `no_keymap` knows it will be told raw keycodes it cannot
    /// name. That is what a compositor with no keymap yet should say, rather
    /// than sending a descriptor that is not one.
    fn send_keymap(&mut self, id: ObjectId) {
        let signature = &[ArgType::Uint, ArgType::Fd, ArgType::Uint];
        match self.keymap {
            Some((fd, size)) => {
                let _ = self.out.write(
                    id,
                    core::wl_keyboard::event::KEYMAP,
                    signature,
                    &[
                        Arg::Uint(core::wl_keyboard::keymap_format::XKB_V1),
                        Arg::Fd(fd),
                        Arg::Uint(size),
                    ],
                );
            }
            None => {
                // The protocol has no way to send nothing, so `no_keymap`
                // goes with a descriptor the client will not map and a size
                // of zero. libwayland's own compositors do the same.
                let _ = self.out.write(
                    id,
                    core::wl_keyboard::event::KEYMAP,
                    signature,
                    &[
                        Arg::Uint(core::wl_keyboard::keymap_format::NO_KEYMAP),
                        Arg::Fd(Fd(-1)),
                        Arg::Uint(0),
                    ],
                );
            }
        }
    }

    /// `wl_pointer`: `set_cursor`, which is how a client says what the
    /// pointer looks like over its window.
    ///
    /// A text field asks for an I-beam, a link for a hand, a resize edge for
    /// an arrow with two heads: all of them are this one request with a
    /// surface the client drew. A null surface hides the pointer, which is
    /// what a video player full-screen does.
    ///
    /// The serial is not checked. libwayland's own compositors check it
    /// against the last `enter` so that a client cannot change the cursor
    /// while the pointer is somebody else's; this compositor has one pointer
    /// and gives it to the surface under it, so the client that is asking is
    /// the client that has it.
    pub(super) fn pointer_request(&mut self, opcode: u16, args: &[Arg<'_>]) {
        if opcode != core::wl_pointer::request::SET_CURSOR {
            return;
        }
        let surface = args.get(1).and_then(Arg::as_object);
        let hotspot = (
            args.get(2).and_then(Arg::as_int).unwrap_or(0),
            args.get(3).and_then(Arg::as_int).unwrap_or(0),
        );
        self.cursor = surface
            .filter(|surface| !surface.is_null() && self.surfaces.contains_key(surface))
            .map(|surface| (surface, hotspot));
        self.said_cursor = true;
        self.events.push(Event::CursorSet {
            surface: self.cursor.map(|(surface, _)| surface),
            hotspot,
        });
    }

    /// `wp_cursor_shape_manager_v1`: `get_pointer` and `get_tablet_tool_v2`.
    ///
    /// A device object is made for each; the tablet tool's is made and never
    /// spoken to, because this compositor has no tablet tool to name a
    /// cursor for.
    pub(super) fn cursor_shape_manager(&mut self, version: u32, opcode: u16, args: &[Arg<'_>]) {
        if !matches!(
            opcode,
            wp_cursor_shape_manager_v1::request::GET_POINTER
                | wp_cursor_shape_manager_v1::request::GET_TABLET_TOOL_V2
        ) {
            return;
        }
        let Some(id) = args.first().and_then(Arg::as_object) else {
            return;
        };
        let _ = self.make(
            id,
            &cursor_shape::WP_CURSOR_SHAPE_DEVICE_V1,
            version,
            Role::CursorShapeDevice,
        );
    }

    /// `wp_cursor_shape_device_v1`: `set_shape`.
    ///
    /// The client names a cursor instead of drawing one, which is what a
    /// toolkit would rather do: it has no idea what the person's theme
    /// looks like and the compositor does. This one draws its own arrow for
    /// every shape it is given -- there is one shape and no theme to pick
    /// another from -- and says which was asked for, so that a client is
    /// answered rather than refused and the log records what a real toolkit
    /// wanted.
    ///
    /// The arrow is what a client that has said nothing about its cursor
    /// gets, so a shape leaves it there: said, with no surface, is what
    /// `set_cursor` with a null surface leaves, which hides the pointer.
    /// Chrome, with no cursor theme to draw from, names a shape for every
    /// cursor, and its windows had no pointer at all.
    pub(super) fn cursor_shape_device(&mut self, opcode: u16, args: &[Arg<'_>]) {
        if opcode != wp_cursor_shape_device_v1::request::SET_SHAPE {
            return;
        }
        let shape = args.get(1).and_then(Arg::as_uint).unwrap_or(0);
        if shape == 0 || shape > wp_cursor_shape_device_v1::shape::ALL_SCROLL {
            self.fail(Fatal::Interface {
                object: ObjectId::DISPLAY,
                code: wp_cursor_shape_device_v1::error::INVALID_SHAPE,
                text: format!("{shape} is not a cursor shape"),
            });
            return;
        }
        self.cursor = None;
        self.said_cursor = false;
        self.events.push(Event::CursorShaped { shape });
    }

    /// Say what the seat has, before any client binds it.
    ///
    /// A client may only ask for a capability the seat announced, so a
    /// compositor with no input yet announces none rather than handing out a
    /// keyboard that will never send a key.
    pub const fn set_seat_capabilities(&mut self, capabilities: u32) {
        self.capabilities = capabilities;
    }

    /// Whether this client has made a `zwlr_virtual_pointer_v1`: a pointer
    /// device the seat has while it lives, as a mouse is one.
    #[must_use]
    pub fn has_virtual_pointer(&self) -> bool {
        !self.virtual_pointers.is_empty()
    }

    /// Say what the seat has now, to a client that may have bound it
    /// already: a keyboard plugged in after the client started, or found
    /// only after the compositor did.
    ///
    /// Every `wl_seat` the client bound is sent `capabilities` again, as
    /// libwayland's compositors send it on a hotplug, and a toolkit that
    /// sees a new capability asks for the device. Nothing is sent when
    /// nothing changed.
    pub fn change_seat_capabilities(&mut self, capabilities: u32) {
        if self.capabilities == capabilities {
            return;
        }
        self.capabilities = capabilities;
        for seat in self.objects_with(Role::Seat) {
            let _ = self.out.write(
                seat,
                wl_seat::event::CAPABILITIES,
                &[ArgType::Uint],
                &[Arg::Uint(capabilities)],
            );
        }
    }

    /// The keymap every `wl_keyboard` is given: a descriptor and its length.
    ///
    /// The same descriptor goes to every keyboard, which is what libwayland's
    /// own compositors do: the file is read-only and each client maps its own
    /// copy.
    pub const fn set_keymap(&mut self, keymap: Option<(Fd, u32)>) {
        self.keymap = keymap;
    }

    /// Say how a held key repeats: `rate` keys a second, `delay`
    /// milliseconds before the first repeat.
    ///
    /// The compositor sends no repeats of its own -- `wl_keyboard.key` has no
    /// way to say one -- so this is the whole of repeat: the client is told
    /// the numbers and repeats for itself. A rate of zero disables it, which
    /// the protocol says in so many words.
    pub const fn set_repeat_info(&mut self, rate: i32, delay: i32) {
        self.repeat = (rate, delay);
    }

    /// [`Self::set_repeat_info`] while the client runs: the numbers are
    /// kept and every `wl_keyboard` bound at version 4 or above is told
    /// them at once, which is what `hyprctl keyword input:repeat_rate`
    /// changes. The protocol lets `repeat_info` come at any time; a
    /// keyboard bound below version 4 never hears of repeat at all.
    pub fn change_repeat_info(&mut self, rate: i32, delay: i32) {
        self.repeat = (rate, delay);
        for keyboard in self.objects_with_version(Role::Keyboard, 4) {
            let _ = self.out.write(
                keyboard,
                core::wl_keyboard::event::REPEAT_INFO,
                &[ArgType::Int, ArgType::Int],
                &[Arg::Int(rate), Arg::Int(delay)],
            );
        }
    }

    /// [`Self::set_keymap`] while the client runs: every `wl_keyboard` it
    /// holds is sent the new keymap, which a client takes in place of the
    /// old one -- how a layout chosen in Settings reaches the windows
    /// already open. The descriptor must stay open until the event has
    /// gone out; the caller keeps the old one for the same reason.
    pub fn change_keymap(&mut self, keymap: Option<(Fd, u32)>) {
        self.keymap = keymap;
        for keyboard in self.objects_with(Role::Keyboard) {
            self.send_keymap(keyboard);
        }
    }

    /// What this client last asked the pointer to look like over its
    /// windows: the surface and where in it the pointer is.
    ///
    /// `None` is a client that has asked for no cursor at all, which is what
    /// hides the pointer.
    #[must_use]
    pub const fn cursor(&self) -> Option<(ObjectId, (i32, i32))> {
        self.cursor
    }

    /// Whether this client has ever said anything about the cursor.
    ///
    /// A client that has not is drawn the compositor's own arrow; one that
    /// has asked for nothing is drawn none, and the two are different
    /// things.
    #[must_use]
    pub const fn said_cursor(&self) -> bool {
        self.said_cursor
    }

    // -----------------------------------------------------------------
    // The seat: what the compositor sends when somebody types or points
    //
    // Every one of these goes to each object of the role this client made,
    // because a client may ask its seat for more than one `wl_keyboard` and
    // the protocol says each gets the events. A client that asked for none
    // gets nothing, which is how a compositor sends a key to the focused
    // window without knowing whether that window wanted keys.
    //
    // The serial each returns is this connection's, from `next_serial`: a
    // client quotes it back in `set_cursor`, in a `start_drag`, or in
    // `xdg_toplevel.move`, and the compositor matches it against what it
    // sent.
    // -----------------------------------------------------------------

    /// Give this client the keyboard focus on `surface`.
    ///
    /// `keys` is every key held at the moment focus arrives, in the order
    /// they were pressed, so that a client focused while a key is down knows
    /// it is down.
    ///
    /// `None` when this client has no `wl_keyboard` yet. That is not a
    /// failure and it is not nothing: a window is often mapped in the same
    /// burst of requests that asks the seat for its keyboard, and whichever
    /// the server reads first, the client has to be told it has the focus. So
    /// the caller is told the focus did not arrive, and asks again. The same
    /// for a `surface` the client has destroyed, which an event may not name.
    pub fn keyboard_enter(
        &mut self,
        surface: ObjectId,
        keys: &[u16],
        modifiers: Modifiers,
    ) -> Option<u32> {
        let keyboards = self.objects_with(Role::Keyboard);
        if keyboards.is_empty() || !self.surfaces.contains_key(&surface) {
            return None;
        }
        let serial = self.next_serial();
        let packed: Vec<u8> = keys
            .iter()
            .flat_map(|key| u32::from(*key).to_le_bytes())
            .collect();
        for keyboard in keyboards {
            let _ = self.out.write(
                keyboard,
                core::wl_keyboard::event::ENTER,
                &[
                    ArgType::Uint,
                    ArgType::Object { nullable: false },
                    ArgType::Array,
                ],
                &[Arg::Uint(serial), Arg::Object(surface), Arg::Array(&packed)],
            );
        }
        // The modifiers are not part of `enter`, and a client that is not
        // told them treats every key as unmodified until the next change.
        let _ = self.keyboard_modifiers(modifiers);
        Some(serial)
    }

    /// Take the keyboard focus away from `surface`.
    ///
    /// `None` when this client has no `wl_keyboard`, as in
    /// [`Client::keyboard_enter`], or when `surface` is no longer one of its
    /// surfaces. A surface the client destroyed has had its `delete_id`, and
    /// an event naming it is one libwayland calls an unknown object and
    /// ends the connection over -- which is how Chrome died when a menu it
    /// had just closed was told the pointer left it.
    pub fn keyboard_leave(&mut self, surface: ObjectId) -> Option<u32> {
        let keyboards = self.objects_with(Role::Keyboard);
        if keyboards.is_empty() || !self.surfaces.contains_key(&surface) {
            return None;
        }
        let serial = self.next_serial();
        for keyboard in keyboards {
            let _ = self.out.write(
                keyboard,
                core::wl_keyboard::event::LEAVE,
                &[ArgType::Uint, ArgType::Object { nullable: false }],
                &[Arg::Uint(serial), Arg::Object(surface)],
            );
        }
        Some(serial)
    }

    /// A key went down or came up, at `time` milliseconds.
    ///
    /// `code` is the evdev keycode, which is what the keymap the client was
    /// given numbers from eight.
    pub fn keyboard_key(&mut self, time: u32, code: u16, pressed: bool) -> Option<u32> {
        let keyboards = self.objects_with(Role::Keyboard);
        if keyboards.is_empty() {
            return None;
        }
        let serial = self.next_serial();
        let state = if pressed {
            core::wl_keyboard::key_state::PRESSED
        } else {
            core::wl_keyboard::key_state::RELEASED
        };
        for keyboard in keyboards {
            let _ = self.out.write(
                keyboard,
                core::wl_keyboard::event::KEY,
                &[ArgType::Uint, ArgType::Uint, ArgType::Uint, ArgType::Uint],
                &[
                    Arg::Uint(serial),
                    Arg::Uint(time),
                    Arg::Uint(u32::from(code)),
                    Arg::Uint(state),
                ],
            );
        }
        Some(serial)
    }

    /// The modifier state changed.
    pub fn keyboard_modifiers(&mut self, modifiers: Modifiers) -> Option<u32> {
        let keyboards = self.objects_with(Role::Keyboard);
        if keyboards.is_empty() {
            return None;
        }
        let serial = self.next_serial();
        for keyboard in keyboards {
            let _ = self.out.write(
                keyboard,
                core::wl_keyboard::event::MODIFIERS,
                &[
                    ArgType::Uint,
                    ArgType::Uint,
                    ArgType::Uint,
                    ArgType::Uint,
                    ArgType::Uint,
                ],
                &[
                    Arg::Uint(serial),
                    Arg::Uint(modifiers.depressed),
                    Arg::Uint(modifiers.latched),
                    Arg::Uint(modifiers.locked),
                    Arg::Uint(modifiers.group),
                ],
            );
        }
        Some(serial)
    }

    /// The pointer came onto `surface` at `(x, y)` in its own coordinates.
    ///
    /// `None` when this client has no `wl_pointer` or no longer has
    /// `surface`, as in [`Client::keyboard_enter`].
    pub fn pointer_enter(&mut self, surface: ObjectId, x: Fixed, y: Fixed) -> Option<u32> {
        let pointers = self.objects_with(Role::Pointer);
        if pointers.is_empty() || !self.surfaces.contains_key(&surface) {
            return None;
        }
        let serial = self.next_serial();
        for pointer in pointers {
            let _ = self.out.write(
                pointer,
                core::wl_pointer::event::ENTER,
                &[
                    ArgType::Uint,
                    ArgType::Object { nullable: false },
                    ArgType::Fixed,
                    ArgType::Fixed,
                ],
                &[
                    Arg::Uint(serial),
                    Arg::Object(surface),
                    Arg::Fixed(x),
                    Arg::Fixed(y),
                ],
            );
        }
        Some(serial)
    }

    /// The pointer left `surface`.
    ///
    /// `None` when this client has no `wl_pointer` or no longer has
    /// `surface`, as in [`Client::keyboard_leave`].
    pub fn pointer_leave(&mut self, surface: ObjectId) -> Option<u32> {
        let pointers = self.objects_with(Role::Pointer);
        if pointers.is_empty() || !self.surfaces.contains_key(&surface) {
            return None;
        }
        let serial = self.next_serial();
        for pointer in pointers {
            let _ = self.out.write(
                pointer,
                core::wl_pointer::event::LEAVE,
                &[ArgType::Uint, ArgType::Object { nullable: false }],
                &[Arg::Uint(serial), Arg::Object(surface)],
            );
        }
        Some(serial)
    }

    /// The pointer moved to `(x, y)` in the focused surface's coordinates.
    pub fn pointer_motion(&mut self, time: u32, x: Fixed, y: Fixed) {
        for pointer in self.objects_with(Role::Pointer) {
            let _ = self.out.write(
                pointer,
                core::wl_pointer::event::MOTION,
                &[ArgType::Uint, ArgType::Fixed, ArgType::Fixed],
                &[Arg::Uint(time), Arg::Fixed(x), Arg::Fixed(y)],
            );
        }
    }

    /// A pointer button went down or came up. `button` is evdev's code, which
    /// is what the protocol asks for in so many words.
    pub fn pointer_button(&mut self, time: u32, button: u32, pressed: bool) -> Option<u32> {
        let pointers = self.objects_with(Role::Pointer);
        if pointers.is_empty() {
            return None;
        }
        let serial = self.next_serial();
        let state = if pressed {
            core::wl_pointer::button_state::PRESSED
        } else {
            core::wl_pointer::button_state::RELEASED
        };
        for pointer in pointers {
            let _ = self.out.write(
                pointer,
                core::wl_pointer::event::BUTTON,
                &[ArgType::Uint, ArgType::Uint, ArgType::Uint, ArgType::Uint],
                &[
                    Arg::Uint(serial),
                    Arg::Uint(time),
                    Arg::Uint(button),
                    Arg::Uint(state),
                ],
            );
        }
        Some(serial)
    }

    /// A scroll: `axis` is `wl_pointer.axis`, `value` the distance.
    pub fn pointer_axis(&mut self, time: u32, axis: u32, value: Fixed) {
        // A wheel click is `WHEEL_STEP` of surface distance, and the
        // protocol's own unit for one is 120 -- so the notches are the
        // distance over the step, which is what `axis_value120` carries and
        // what a client multiplies its own scroll speed by.
        let notches = value.to_f64() / WHEEL_STEP;
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a wheel reports whole clicks, so the product is a small whole number"
        )]
        let value120 = (notches * 120.0).round() as i32;
        for pointer in self.objects_with(Role::Pointer) {
            let version = self.objects.get(pointer).map_or(1, |entry| entry.version);
            // The order inside a frame is the protocol's: what kind of
            // scroll it was, which way it runs, how far in the wheel's own
            // units, and only then the distance.
            if version >= 5 {
                let _ = self.out.write(
                    pointer,
                    core::wl_pointer::event::AXIS_SOURCE,
                    &[ArgType::Uint],
                    &[Arg::Uint(core::wl_pointer::axis_source::WHEEL)],
                );
            }
            if version >= 9 {
                let _ = self.out.write(
                    pointer,
                    core::wl_pointer::event::AXIS_RELATIVE_DIRECTION,
                    &[ArgType::Uint, ArgType::Uint],
                    &[
                        Arg::Uint(axis),
                        // Nothing here inverts a wheel: `input:natural_scroll`
                        // is the seat's and is applied before this.
                        Arg::Uint(core::wl_pointer::axis_relative_direction::IDENTICAL),
                    ],
                );
            }
            // `axis_discrete` is deprecated from version 8, where
            // `axis_value120` replaces it: a client on 8 or above ignores
            // the first and a client below has never heard of the second,
            // so exactly one of them goes out.
            if version >= 8 {
                let _ = self.out.write(
                    pointer,
                    core::wl_pointer::event::AXIS_VALUE120,
                    &[ArgType::Uint, ArgType::Int],
                    &[Arg::Uint(axis), Arg::Int(value120)],
                );
            } else if version >= 5 {
                let _ = self.out.write(
                    pointer,
                    core::wl_pointer::event::AXIS_DISCRETE,
                    &[ArgType::Uint, ArgType::Int],
                    &[Arg::Uint(axis), Arg::Int(value120 / 120)],
                );
            }
            let _ = self.out.write(
                pointer,
                core::wl_pointer::event::AXIS,
                &[ArgType::Uint, ArgType::Uint, ArgType::Fixed],
                &[Arg::Uint(time), Arg::Uint(axis), Arg::Fixed(value)],
            );
        }
    }

    /// End a group of pointer events that belong together.
    ///
    /// Only for version 5 and above; before it, each event stood alone and a
    /// `frame` sent to a version-4 pointer is an opcode it does not have.
    pub fn pointer_frame(&mut self) {
        for pointer in self.objects_with_version(Role::Pointer, 5) {
            let _ = self
                .out
                .write(pointer, core::wl_pointer::event::FRAME, &[], &[]);
        }
    }

    /// Every object this client made with `role`.
    fn objects_with(&self, role: Role) -> Vec<ObjectId> {
        self.objects
            .iter()
            .filter(|(_, entry)| entry.data == role)
            .map(|(id, _)| id)
            .collect()
    }

    /// Every object this client made with `role`, bound at `version` or
    /// above.
    fn objects_with_version(&self, role: Role, version: u32) -> Vec<ObjectId> {
        self.objects
            .iter()
            .filter(|(_, entry)| entry.data == role && entry.version >= version)
            .map(|(id, _)| id)
            .collect()
    }
}
