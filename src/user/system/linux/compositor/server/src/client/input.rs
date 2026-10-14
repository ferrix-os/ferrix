//! The pointer and keyboard protocols beyond `wl_seat`.
//!
//! `wl_pointer` says where the pointer is on the screen, which is the wrong
//! question for a game, a 3D modeller or a remote-desktop viewer: they want
//! how far it *moved*, and they want it to stay inside their window while
//! they have it. That is `relative-pointer` and `pointer-constraints`, and
//! they come as a pair -- a client locks the pointer and then reads the
//! movement.
//!
//! Beside them: `keyboard-shortcuts-inhibit`, which is how a virtual machine
//! or a nested compositor gets `SUPER` instead of the compositor eating it;
//! and the two virtual devices, `zwp_virtual_keyboard_v1` and
//! `zwlr_virtual_pointer_v1`, which are how `wtype`, `ydotool` and an
//! on-screen keyboard type into whatever is focused.
//!
//! # What a virtual device is allowed to do
//!
//! Everything a real one is: its input goes to the seat and is treated as a
//! person's, keybinds and all. That is what makes `wtype 'hello'` work and
//! it is also why wlroots gates the protocol behind a compositor's own
//! policy. This compositor offers it to every client, as Hyprland does.

use compositor_protocol::pointer_constraints::{
    zwp_confined_pointer_v1, zwp_locked_pointer_v1, zwp_pointer_constraints_v1,
};
use compositor_protocol::pointer_gestures::zwp_pointer_gestures_v1;
use compositor_protocol::relative_pointer::{
    zwp_relative_pointer_manager_v1, zwp_relative_pointer_v1,
};
use compositor_protocol::shortcuts_inhibit::{
    zwp_keyboard_shortcuts_inhibit_manager_v1, zwp_keyboard_shortcuts_inhibitor_v1,
};
use compositor_protocol::virtual_keyboard::{
    zwp_virtual_keyboard_manager_v1, zwp_virtual_keyboard_v1,
};
use compositor_protocol::virtual_pointer::{
    zwlr_virtual_pointer_manager_v1, zwlr_virtual_pointer_v1,
};
use compositor_protocol::{
    pointer_constraints, pointer_gestures, relative_pointer, shortcuts_inhibit, virtual_keyboard,
    virtual_pointer,
};
use compositor_wire::{Arg, ArgType, Fixed, ObjectId};

use crate::client::{Client, Event, Fatal};
use crate::role::Role;

/// A pointer constraint a client holds on one of its surfaces.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Constraint {
    /// The surface it applies to, which has to have the pointer.
    pub surface: ObjectId,
    /// Whether the pointer may not move at all, rather than only not leave.
    pub locked: bool,
    /// Whether it may come back after it stops applying once, which is the
    /// protocol's `persistent` lifetime.
    pub persistent: bool,
    /// Whether the client has been told it is in force.
    pub active: bool,
}

/// What a virtual device asked the seat to do.
///
/// One variant a request, in the order a client sends them. The compositor
/// hands each to the seat as if a device had reported it, which is what the
/// protocol is for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Injected {
    /// `zwp_virtual_keyboard_v1.key`.
    Key {
        /// The evdev keycode.
        key: u32,
        /// Down, as opposed to up.
        pressed: bool,
    },
    /// `zwp_virtual_keyboard_v1.modifiers`.
    Modifiers {
        /// Held down now.
        depressed: u32,
        /// Latched.
        latched: u32,
        /// Locked, as Caps Lock is.
        locked: u32,
        /// The keymap group.
        group: u32,
    },
    /// `zwlr_virtual_pointer_v1.motion`: a movement, in surface pixels.
    ///
    /// The protocol's own `fixed` rather than a float, so that what the
    /// compositor acts on is the number the client sent.
    Motion {
        /// Across.
        dx: Fixed,
        /// Down.
        dy: Fixed,
    },
    /// `motion_absolute`: a place given as a fraction, which is what a
    /// tablet and a remote viewer both report. The client chooses the unit
    /// and sends the whole it is out of.
    MotionAbsolute {
        /// Across, out of `width`.
        x: u32,
        /// Down, out of `height`.
        y: u32,
        /// What `x` is a fraction of.
        width: u32,
        /// What `y` is a fraction of.
        height: u32,
    },
    /// `zwlr_virtual_pointer_v1.button`.
    Button {
        /// The evdev button code.
        button: u32,
        /// Down, as opposed to up.
        pressed: bool,
    },
    /// `zwlr_virtual_pointer_v1.axis`: a scroll.
    Axis {
        /// `wl_pointer.axis`.
        axis: u32,
        /// How far.
        value: Fixed,
    },
}

impl Client {
    /// Answer a request to one of this module's objects.
    ///
    /// Gives whether the role was one of them.
    pub(super) fn input(
        &mut self,
        sender: ObjectId,
        role: Role,
        version: u32,
        opcode: u16,
        args: &[Arg<'_>],
    ) -> bool {
        match role {
            Role::RelativePointerManager => self.relative_manager(version, opcode, args),
            Role::PointerConstraints => self.constraints_manager(version, opcode, args),
            Role::LockedPointer | Role::ConfinedPointer => {
                self.constraint(sender, role, opcode, args);
            }
            Role::ShortcutsInhibitManager => self.inhibit_manager(version, opcode, args),
            Role::VirtualKeyboardManager => self.virtual_keyboard_manager(version, opcode, args),
            Role::VirtualKeyboard => self.virtual_keyboard(opcode, args),
            Role::VirtualPointerManager => self.virtual_pointer_manager(version, opcode, args),
            Role::VirtualPointer => self.virtual_pointer(opcode, args),
            Role::PointerGestures => self.gestures(version, opcode, args),
            _ => return false,
        }
        true
    }

    /// Drop what one of this module's objects held.
    pub(super) fn forget_input(&mut self, id: ObjectId, role: Role) {
        match role {
            Role::RelativePointer => {
                self.relative_pointers.retain(|held| *held != id);
            }
            Role::LockedPointer | Role::ConfinedPointer => {
                let _ = self.constraints.remove(&id);
            }
            Role::ShortcutsInhibitor => {
                let _ = self.shortcut_inhibitors.remove(&id);
            }
            Role::VirtualKeyboard => {
                self.virtual_keyboards.retain(|held| *held != id);
            }
            Role::VirtualPointer => {
                self.virtual_pointers.retain(|held| *held != id);
            }
            _ => {}
        }
    }

    /// `zwp_relative_pointer_manager_v1.get_relative_pointer`.
    fn relative_manager(&mut self, version: u32, opcode: u16, args: &[Arg<'_>]) {
        if opcode != zwp_relative_pointer_manager_v1::request::GET_RELATIVE_POINTER {
            return;
        }
        let (Some(id), Some(pointer)) = (
            args.first().and_then(Arg::as_object),
            args.get(1).and_then(Arg::as_object),
        ) else {
            return;
        };
        if self.objects.get(pointer).map(|entry| entry.data) != Some(Role::Pointer) {
            self.fail(Fatal::WrongInterface {
                object: pointer,
                wanted: "wl_pointer",
            });
            return;
        }
        if !self.make(
            id,
            &relative_pointer::ZWP_RELATIVE_POINTER_V1,
            version,
            Role::RelativePointer,
        ) {
            return;
        }
        self.relative_pointers.push(id);
    }

    /// `zwp_pointer_constraints_v1`: `lock_pointer` and `confine_pointer`.
    fn constraints_manager(&mut self, version: u32, opcode: u16, args: &[Arg<'_>]) {
        let locked = match opcode {
            zwp_pointer_constraints_v1::request::LOCK_POINTER => true,
            zwp_pointer_constraints_v1::request::CONFINE_POINTER => false,
            _ => return,
        };
        let (Some(id), Some(surface), Some(lifetime)) = (
            args.first().and_then(Arg::as_object),
            args.get(1).and_then(Arg::as_object),
            args.get(4).and_then(Arg::as_uint),
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
        let (interface, role) = if locked {
            (
                &pointer_constraints::ZWP_LOCKED_POINTER_V1,
                Role::LockedPointer,
            )
        } else {
            (
                &pointer_constraints::ZWP_CONFINED_POINTER_V1,
                Role::ConfinedPointer,
            )
        };
        if !self.make(id, interface, version, role) {
            return;
        }
        let _ = self.constraints.insert(
            id,
            Constraint {
                surface,
                locked,
                persistent: lifetime == zwp_pointer_constraints_v1::lifetime::PERSISTENT,
                active: false,
            },
        );
    }

    /// `zwp_locked_pointer_v1` and `zwp_confined_pointer_v1`, whose requests
    /// only narrow what is already there.
    ///
    /// `set_region` and `set_cursor_position_hint` are read and recorded as
    /// nothing: the region a constraint holds the pointer to is the
    /// surface's own here, which is the reading the protocol allows ("if the
    /// region is null, the surface's input region is used"), and the hint is
    /// where the client would like the pointer to appear when the lock
    /// ends -- which this compositor answers by leaving it where it was.
    fn constraint(&mut self, sender: ObjectId, role: Role, opcode: u16, args: &[Arg<'_>]) {
        let _ = args;
        let known = match role {
            Role::LockedPointer => matches!(
                opcode,
                zwp_locked_pointer_v1::request::SET_REGION
                    | zwp_locked_pointer_v1::request::SET_CURSOR_POSITION_HINT
            ),
            _ => opcode == zwp_confined_pointer_v1::request::SET_REGION,
        };
        if !known {
            return;
        }
        let _ = self.constraints.get(&sender);
    }

    /// `zwp_keyboard_shortcuts_inhibit_manager_v1.inhibit_shortcuts`.
    ///
    /// Told `active` at once: this compositor has no policy that would say
    /// no, and the protocol requires one of `active` or `inactive` so the
    /// client knows whether its `SUPER` will arrive.
    fn inhibit_manager(&mut self, version: u32, opcode: u16, args: &[Arg<'_>]) {
        if opcode != zwp_keyboard_shortcuts_inhibit_manager_v1::request::INHIBIT_SHORTCUTS {
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
            &shortcuts_inhibit::ZWP_KEYBOARD_SHORTCUTS_INHIBITOR_V1,
            version,
            Role::ShortcutsInhibitor,
        ) {
            return;
        }
        let _ = self.shortcut_inhibitors.insert(id, surface);
        let _ = self.out.write(
            id,
            zwp_keyboard_shortcuts_inhibitor_v1::event::ACTIVE,
            &[],
            &[],
        );
    }

    /// `zwp_virtual_keyboard_manager_v1.create_virtual_keyboard`.
    fn virtual_keyboard_manager(&mut self, version: u32, opcode: u16, args: &[Arg<'_>]) {
        if opcode != zwp_virtual_keyboard_manager_v1::request::CREATE_VIRTUAL_KEYBOARD {
            return;
        }
        let Some(id) = args.get(1).and_then(Arg::as_object) else {
            return;
        };
        if !self.make(
            id,
            &virtual_keyboard::ZWP_VIRTUAL_KEYBOARD_V1,
            version,
            Role::VirtualKeyboard,
        ) {
            return;
        }
        self.virtual_keyboards.push(id);
    }

    /// `zwp_virtual_keyboard_v1`: `keymap`, `key` and `modifiers`.
    ///
    /// The keymap is read and dropped: this compositor has one keymap for
    /// the seat, and a virtual keyboard that brought its own would have to
    /// be a second `wl_keyboard` to every client. `wtype` sends one and then
    /// sends keycodes from it, which for the ASCII range is the same layout
    /// either way.
    fn virtual_keyboard(&mut self, opcode: u16, args: &[Arg<'_>]) {
        match opcode {
            zwp_virtual_keyboard_v1::request::KEY => {
                let (Some(key), Some(state)) = (
                    args.get(1).and_then(Arg::as_uint),
                    args.get(2).and_then(Arg::as_uint),
                ) else {
                    return;
                };
                self.events.push(Event::Injected(Injected::Key {
                    key,
                    pressed: state == compositor_protocol::core::wl_keyboard::key_state::PRESSED,
                }));
            }
            zwp_virtual_keyboard_v1::request::MODIFIERS => {
                let numbers: Vec<u32> = args.iter().filter_map(Arg::as_uint).collect();
                let [depressed, latched, locked, group] = numbers.as_slice() else {
                    return;
                };
                self.events.push(Event::Injected(Injected::Modifiers {
                    depressed: *depressed,
                    latched: *latched,
                    locked: *locked,
                    group: *group,
                }));
            }
            _ => {}
        }
    }

    /// `zwlr_virtual_pointer_manager_v1`: both ways of making one.
    fn virtual_pointer_manager(&mut self, version: u32, opcode: u16, args: &[Arg<'_>]) {
        // The `with_output` form takes the output between the seat and the
        // id, so the id is the last object either way.
        let id = match opcode {
            zwlr_virtual_pointer_manager_v1::request::CREATE_VIRTUAL_POINTER => {
                args.get(1).and_then(Arg::as_object)
            }
            zwlr_virtual_pointer_manager_v1::request::CREATE_VIRTUAL_POINTER_WITH_OUTPUT => {
                args.get(2).and_then(Arg::as_object)
            }
            _ => return,
        };
        let Some(id) = id else {
            return;
        };
        if !self.make(
            id,
            &virtual_pointer::ZWLR_VIRTUAL_POINTER_V1,
            version,
            Role::VirtualPointer,
        ) {
            return;
        }
        self.virtual_pointers.push(id);
    }

    /// `zwlr_virtual_pointer_v1`: the movements, buttons and scrolls a
    /// client reports as if it were a mouse.
    fn virtual_pointer(&mut self, opcode: u16, args: &[Arg<'_>]) {
        let injected = match opcode {
            zwlr_virtual_pointer_v1::request::MOTION => {
                let (Some(dx), Some(dy)) = (
                    args.get(1).and_then(Arg::as_fixed),
                    args.get(2).and_then(Arg::as_fixed),
                ) else {
                    return;
                };
                Injected::Motion { dx, dy }
            }
            zwlr_virtual_pointer_v1::request::MOTION_ABSOLUTE => {
                let numbers: Vec<u32> = args.iter().filter_map(Arg::as_uint).collect();
                let [_time, x, y, width, height] = numbers.as_slice() else {
                    return;
                };
                Injected::MotionAbsolute {
                    x: *x,
                    y: *y,
                    width: *width,
                    height: *height,
                }
            }
            zwlr_virtual_pointer_v1::request::BUTTON => {
                let (Some(button), Some(state)) = (
                    args.get(1).and_then(Arg::as_uint),
                    args.get(2).and_then(Arg::as_uint),
                ) else {
                    return;
                };
                Injected::Button {
                    button,
                    pressed: state == compositor_protocol::core::wl_pointer::button_state::PRESSED,
                }
            }
            zwlr_virtual_pointer_v1::request::AXIS => {
                let (Some(axis), Some(value)) = (
                    args.get(1).and_then(Arg::as_uint),
                    args.get(2).and_then(Arg::as_fixed),
                ) else {
                    return;
                };
                Injected::Axis { axis, value }
            }
            // `frame`, `axis_source`, `axis_stop` and `axis_discrete` say
            // how a scroll was made rather than that one happened; the seat
            // takes each movement on its own.
            _ => return,
        };
        self.events.push(Event::Injected(injected));
    }

    /// `zwp_pointer_gestures_v1`: swipe, pinch and hold.
    ///
    /// The objects are made and never sent to. Gestures come from a
    /// touchpad through libinput's own gesture recogniser, and this
    /// compositor reads evdev directly, so there is nothing to report. A
    /// toolkit that binds the global and hears nothing behaves exactly as it
    /// does on a machine with a mouse; one that finds no global logs a
    /// warning on every start.
    fn gestures(&mut self, version: u32, opcode: u16, args: &[Arg<'_>]) {
        let (interface, role) = match opcode {
            zwp_pointer_gestures_v1::request::GET_SWIPE_GESTURE => (
                &pointer_gestures::ZWP_POINTER_GESTURE_SWIPE_V1,
                Role::GestureSwipe,
            ),
            zwp_pointer_gestures_v1::request::GET_PINCH_GESTURE => (
                &pointer_gestures::ZWP_POINTER_GESTURE_PINCH_V1,
                Role::GesturePinch,
            ),
            zwp_pointer_gestures_v1::request::GET_HOLD_GESTURE => (
                &pointer_gestures::ZWP_POINTER_GESTURE_HOLD_V1,
                Role::GestureHold,
            ),
            _ => return,
        };
        let Some(id) = args.first().and_then(Arg::as_object) else {
            return;
        };
        let _ = self.make(id, interface, version, role);
    }

    /// Tell every `zwp_relative_pointer_v1` how far the pointer moved.
    ///
    /// The time is microseconds, which is this protocol's own unit and not
    /// `wl_pointer`'s milliseconds. The accelerated and unaccelerated
    /// movements are the same number here: this compositor does not
    /// accelerate the pointer, so there is nothing for the two to differ by.
    pub fn relative_motion(&mut self, micros: u64, dx: f64, dy: f64) {
        for id in self.relative_pointers.clone() {
            let _ = self.out.write(
                id,
                zwp_relative_pointer_v1::event::RELATIVE_MOTION,
                RELATIVE_MOTION,
                &relative_motion_args(micros, dx, dy),
            );
        }
    }

    /// This client's `zwp_relative_pointer_v1`s, which
    /// [`relative_motion_message`] is for: the ones it has not destroyed.
    #[must_use]
    pub fn relative_pointers(&self) -> &[ObjectId] {
        &self.relative_pointers
    }

    /// The constraint this client holds on `surface`, if it holds one.
    #[must_use]
    pub fn constraint_on(&self, surface: ObjectId) -> Option<Constraint> {
        self.constraints
            .values()
            .find(|held| held.surface == surface)
            .copied()
    }

    /// Turn a constraint on `surface` on or off, telling the client.
    ///
    /// A constraint applies only while its surface has the pointer, so the
    /// compositor decides and the client is told; a one-shot constraint that
    /// has been turned off is destroyed, as the protocol says, and a
    /// persistent one waits to come back.
    pub fn constrain(&mut self, surface: ObjectId, on: bool) {
        let holders: Vec<ObjectId> = self
            .constraints
            .iter()
            .filter(|(_, held)| held.surface == surface && held.active != on)
            .map(|(id, _)| *id)
            .collect();
        for id in holders {
            let Some(held) = self.constraints.get_mut(&id) else {
                continue;
            };
            held.active = on;
            let (locked, persistent) = (held.locked, held.persistent);
            let event = match (locked, on) {
                (true, true) => zwp_locked_pointer_v1::event::LOCKED,
                (true, false) => zwp_locked_pointer_v1::event::UNLOCKED,
                (false, true) => zwp_confined_pointer_v1::event::CONFINED,
                (false, false) => zwp_confined_pointer_v1::event::UNCONFINED,
            };
            let _ = self.out.write(id, event, &[], &[]);
            if !on && !persistent {
                let _ = self.constraints.remove(&id);
                let role = if locked {
                    Role::LockedPointer
                } else {
                    Role::ConfinedPointer
                };
                self.destroy(id, role);
            }
        }
    }

    /// Whether this client has asked for the compositor's keybinds to be
    /// left alone while `surface` has the keyboard.
    #[must_use]
    pub fn inhibits_shortcuts(&self, surface: ObjectId) -> bool {
        self.shortcut_inhibitors
            .values()
            .any(|held| *held == surface)
    }
}

/// `zwp_relative_pointer_v1.relative_motion`'s signature.
const RELATIVE_MOTION: &[ArgType] = &[
    ArgType::Uint,
    ArgType::Uint,
    ArgType::Fixed,
    ArgType::Fixed,
    ArgType::Fixed,
    ArgType::Fixed,
];

/// Its arguments: the time in microseconds, high half first, and the
/// distance twice, accelerated and not, which are the same here.
fn relative_motion_args(micros: u64, dx: f64, dy: f64) -> [Arg<'static>; 6] {
    [
        Arg::Uint(u32::try_from(micros >> 32).unwrap_or(0)),
        Arg::Uint(u32::try_from(micros & 0xffff_ffff).unwrap_or(0)),
        Arg::Fixed(Fixed::from_f64(dx)),
        Arg::Fixed(Fixed::from_f64(dy)),
        Arg::Fixed(Fixed::from_f64(dx)),
        Arg::Fixed(Fixed::from_f64(dy)),
    ]
}

/// `relative_motion` to each of `pointers`, as the bytes
/// [`Client::relative_motion`] would queue: for a compositor's input
/// thread, which writes a locked pointer's motion to the client itself
/// while the loop that owns the client is busy (`docs/GAME-PATH.md` §4.7).
#[must_use]
pub fn relative_motion_message(pointers: &[ObjectId], micros: u64, dx: f64, dy: f64) -> Vec<u8> {
    let mut out = compositor_wire::Writer::new();
    for id in pointers {
        let _ = out.write(
            *id,
            zwp_relative_pointer_v1::event::RELATIVE_MOTION,
            RELATIVE_MOTION,
            &relative_motion_args(micros, dx, dy),
        );
    }
    out.bytes().to_vec()
}
