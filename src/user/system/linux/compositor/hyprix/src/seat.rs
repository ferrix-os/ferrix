//! The seat: what a key or a movement means.
//!
//! This is the part of input that has no descriptor in it. It is handed
//! [`Input`] -- a key, a movement, a button, a scroll, already free of evdev's
//! numbering by the layer below -- and gives back [`Action`]s: run this
//! dispatcher, send this key to the focused window, the pointer is now here.
//! So every rule about what a keybind matches and what a modifier does is
//! host-tested, and running it on Ferrix tests the devices rather than the
//! rules.
//!
//! # What a bind matches
//!
//! Hyprland fires a bind when the key matches *and* the modifiers held are
//! exactly the bind's. Exactly, not "at least": `bind = SUPER, Q` does not
//! fire on `SUPER SHIFT Q`, which is what lets the two be bound to different
//! things. Caps Lock and Num Lock are left out of the comparison, because a
//! keyboard with Caps Lock on would otherwise match nothing.
//!
//! A bind consumes its key: the focused client is not told about it. The `n`
//! flag says not to, and the `i` flag says to fire whatever modifiers are
//! held.
//!
//! # Submaps
//!
//! `submap = resize` in the configuration puts every bind after it in a map
//! of its own, and `submap = reset` goes back to the global one. Only one
//! map is in force at a time: while a submap is entered, the global binds do
//! not fire and the submap's do, which is what makes `submap = resize` a
//! mode a person is in rather than a prefix. The `u` flag is the exception
//! Hyprland gives it -- a bind with `u` fires in every map, so that the one
//! that leaves a submap can be written once.
//!
//! The map is entered by the `submap` dispatcher, which is the compositor's
//! and not the layout's: `bind = SUPER, R, submap, resize` goes in, and
//! `submap, reset` comes back out. A name no bind was written in is refused
//! with Hyprland's own sentence rather than entered, because a submap with
//! nothing in it is a keyboard that has stopped answering.

use compositor_config::{Config, Key, Mods};

use compositor_xkb::generated::Layout;
use compositor_xkb::{Keyboard, Modifiers, generated};

/// What the devices below send up, free of evdev's numbering.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Input {
    /// A key went down, came up, or repeated while held.
    Key {
        /// The evdev keycode.
        code: u16,
        /// Down, as opposed to up.
        pressed: bool,
        /// A repeat of a key already held, which evdev sends as value 2.
        repeat: bool,
    },
    /// The pointer moved by this many pixels.
    Motion {
        /// Rightwards.
        dx: f64,
        /// Downwards.
        dy: f64,
    },
    /// The pointer is at this fraction of the screen, from a tablet or a
    /// touchscreen, which report where they are rather than how far they
    /// moved.
    Absolute {
        /// Across, 0 to 1.
        x: f64,
        /// Down, 0 to 1.
        y: f64,
    },
    /// A pointer button, by its evdev code: `BTN_LEFT` is 272, which is what
    /// `wl_pointer.button` carries.
    Button {
        /// The evdev button code.
        button: u32,
        /// Down, as opposed to up.
        pressed: bool,
    },
    /// A scroll: `axis` is a `wl_pointer.axis` value, `value` the distance in
    /// surface coordinates.
    Axis {
        /// `wl_pointer.axis`.
        axis: u32,
        /// The distance.
        value: f64,
    },
}

/// What the compositor should do about an [`Input`].
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Send this key to the focused window.
    Key {
        /// The evdev keycode.
        code: u16,
        /// Down, as opposed to up.
        pressed: bool,
    },
    /// The modifier state changed; tell the focused window.
    Modifiers(Modifiers),
    /// Run a dispatcher, as `hyprctl dispatch` would.
    Dispatch {
        /// Its name, lower-cased.
        name: String,
        /// Its argument, as the bind wrote it.
        argument: String,
        /// The key that fired it, when a key did: the evdev code and the
        /// modifiers held with it. `pass` sends exactly that key on to
        /// another window, and has nothing to send without it.
        trigger: Option<(u16, u32)>,
    },
    /// The device moved by `(dx, dy)`, whether or not the pointer did: a
    /// locked or confined pointer, or one at the screen's edge, stays put
    /// while `zwp_relative_pointer_v1` still tells its client the distance.
    Relative {
        /// Across.
        dx: f64,
        /// Down.
        dy: f64,
    },
    /// The pointer is at `(x, y)` on the screen.
    Pointer {
        /// Across.
        x: f64,
        /// Down.
        y: f64,
    },
    /// A pointer button, for the window under the pointer.
    Button {
        /// The evdev button code.
        button: u32,
        /// Down, as opposed to up.
        pressed: bool,
    },
    /// A scroll, for the window under the pointer.
    Axis {
        /// `wl_pointer.axis`.
        axis: u32,
        /// The distance.
        value: f64,
    },
}

/// The modifiers a bind is compared on.
///
/// Caps Lock and Num Lock are left out: they are locks a person leaves on,
/// and a bind that stopped working because Num Lock was on would look like a
/// broken compositor. Hyprland leaves the same two out.
const COMPARED: u32 = generated::SHIFT
    | generated::CONTROL
    | generated::MOD1
    | generated::MOD3
    | generated::MOD4
    | generated::MOD5;

/// One bind, with its key already resolved to a code.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Bound {
    mods: u32,
    trigger: Trigger,
    /// `r`: fire on the release rather than the press.
    release: bool,
    /// `e`: fire again on each repeat while held.
    repeat: bool,
    /// `n`: the key reaches the focused client as well.
    non_consuming: bool,
    /// `i`: fire whatever modifiers are held.
    ignore_mods: bool,
    /// `l`: fires while the session is locked.
    while_locked: bool,
    /// The submap it belongs to, `None` for the global map.
    submap: Option<String>,
    /// `u`: fires whichever map is in force.
    universal: bool,
    /// `m`: a drag. It fires on the press *and* on the release, and the
    /// argument it carries is `+action` for the one and `-action` for the
    /// other, which is how Hyprland tells the start of a drag from its end.
    mouse: bool,
    dispatcher: String,
    argument: String,
}

/// Where a `zwp_pointer_constraints_v1` holds the pointer.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Hold {
    /// Nowhere: the pointer goes where the mouse says.
    #[default]
    Free,
    /// Still: `lock_pointer`, which is what a game or a 3D modeller asks
    /// for when it turns the pointer into a direction.
    Locked,
    /// Inside a rectangle: `confine_pointer`.
    Inside(compositor_layout::Rect),
}

/// What sets a bind off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Trigger {
    /// A key, by evdev code.
    Key(u16),
    /// A pointer button, by evdev code.
    Button(u32),
    /// The wheel, by `wl_pointer.axis` and the sign of the movement.
    Wheel {
        axis: u32,
        /// Whether the bind wants a positive movement.
        positive: bool,
    },
}

/// The seat.
#[derive(Clone, Debug, Default)]
pub struct Seat {
    keyboard: Keyboard,
    binds: Vec<Bound>,
    /// Where the pointer is on the screen, in pixels.
    pointer: (f64, f64),
    /// Whether the pointer has ever been used.
    ///
    /// The position above starts in the middle of the screen, which is a
    /// guess: nothing has said where the mouse is until it moves. A
    /// compositor that drew an arrow there would be drawing one at a place
    /// it made up, so the pointer is drawn from its first movement and not
    /// before -- which is also what a machine with a mouse plugged in and
    /// never touched should look like.
    used: bool,
    /// Where the last absolute device put the pointer before any hold, so
    /// the next one's distance is the device's own.
    absolute: Option<(f64, f64)>,
    /// Where the pointer is held, if a client has asked for it:
    /// `zwp_pointer_constraints_v1`.
    hold: Hold,
    /// Whether a client has asked for the compositor's keybinds to be left
    /// alone: `zwp_keyboard_shortcuts_inhibit_manager_v1`.
    shortcuts_inhibited: bool,
    /// The pointer buttons held down now, by their evdev codes. A drag ends
    /// when the last of them comes up.
    buttons: Vec<u32>,
    /// The screen, which the pointer may not leave: its size, from
    /// [`origin`](Self::origin).
    screen: (f64, f64),
    /// Where the screen's top-left corner is in the layout: 0, 0 unless no
    /// monitor is there ([`Seat::place_at`]).
    origin: (f64, f64),
    /// The submap in force, `None` for the global map.
    submap: Option<String>,
    /// Whether the session is locked, which is what `bindl` is for.
    locked: bool,
    /// Every submap a bind was written in, whether or not its key resolved.
    submaps: Vec<String>,
    /// Binds that have not been resolved, with the reason, for the log.
    unresolved: Vec<String>,
    /// The keymap in force: `input:kb_layout` and `input:kb_variant`.
    ///
    /// A bind is written as a keysym's *name* -- `bind = SUPER, Q, …` -- and
    /// which key makes that keysym is the keymap's business, so a German
    /// keyboard's `Q` and an American one's are different keys and the same
    /// line has to find both.
    layout: &'static Layout,
    /// Keys whose press a bind ate, so that their release is eaten too.
    ///
    /// Without this a client is told a key came up that it was never told
    /// went down, and a toolkit that keeps its own idea of what is held --
    /// every toolkit does -- has a key stuck down for ever. The modifiers
    /// have usually been let go by the time the key is, so the bind no longer
    /// matches and the release cannot be judged on its own.
    eaten: Vec<u16>,
}

impl Seat {
    /// A seat for a screen of `width` by `height`, with `config`'s binds.
    #[must_use]
    pub fn new(config: &Config, width: u32, height: u32) -> Self {
        let mut seat = Self {
            keyboard: Keyboard::new(),
            binds: Vec::new(),
            hold: Hold::Free,
            shortcuts_inhibited: false,
            buttons: Vec::new(),
            // The pointer starts in the middle, as Hyprland's does.
            pointer: (f64::from(width) / 2.0, f64::from(height) / 2.0),
            used: false,
            absolute: None,
            screen: (f64::from(width), f64::from(height)),
            origin: (0.0, 0.0),
            submap: None,
            locked: false,
            submaps: Vec::new(),
            unresolved: Vec::new(),
            layout: chosen(config).0,
            eaten: Vec::new(),
        };
        seat.set_binds(config);
        // How many layout groups the keymap has, which is what
        // `hyprctl switchxkblayout` moves between and wraps around:
        // `input:kb_layout = de,us` is a keyboard with two. The count comes
        // from the same two options the keymap itself is built from, so the
        // group the clients are told about is always a group their keymap
        // has.
        seat.keyboard.set_layouts(layouts_of(config));
        seat
    }

    /// Take the binds from `config`, forgetting the ones before.
    pub fn set_binds(&mut self, config: &Config) {
        self.binds.clear();
        self.unresolved.clear();
        // A reload may have changed the keymap, and every bind is resolved
        // against it.
        self.layout = chosen(config).0;
        self.keyboard.set_layouts(layouts_of(config));
        // A reload takes away the map that was in force, as it takes away
        // the binds: a submap the new configuration does not have is one
        // nothing could leave.
        self.submap = None;
        self.submaps.clear();
        for bind in &config.binds {
            if let Some(submap) = &bind.submap
                && !self.submaps.iter().any(|known| known == submap)
            {
                self.submaps.push(submap.clone());
            }
            let Some(trigger) = trigger_of(self.layout, &bind.key) else {
                self.unresolved
                    .push(format!("{} is not a key this keymap has", bind.key));
                continue;
            };
            self.binds.push(Bound {
                mods: bind.mods.0 & COMPARED,
                trigger,
                release: bind.flags.release,
                repeat: bind.flags.repeat,
                non_consuming: bind.flags.non_consuming,
                ignore_mods: bind.flags.ignore_mods,
                while_locked: bind.flags.locked,
                submap: bind.submap.clone(),
                universal: bind.flags.submap_universal,
                mouse: bind.flags.mouse,
                dispatcher: bind.dispatcher.clone(),
                argument: bind.arg.clone(),
            });
        }
    }

    /// How many binds are live, and what could not be resolved.
    #[must_use]
    pub fn binds(&self) -> (usize, &[String]) {
        (self.binds.len(), &self.unresolved)
    }

    /// The keymap in force, which is what every bind was resolved against
    /// and what every client is handed.
    #[must_use]
    pub const fn layout(&self) -> &'static Layout {
        self.layout
    }

    /// Say whether the session is locked.
    ///
    /// A locked session takes every bind out of force but the ones written
    /// `bindl`, which is what that flag is for: the volume keys and the
    /// brightness keys keep working on a locked screen and nothing else
    /// does.
    pub const fn set_locked(&mut self, locked: bool) {
        self.locked = locked;
    }

    /// The submap in force, empty for the global map.
    ///
    /// Empty rather than `None` because that is what `hyprctl submap` prints
    /// and what the event socket carries.
    #[must_use]
    pub fn submap(&self) -> &str {
        self.submap.as_deref().unwrap_or("")
    }

    /// Enter `name`, or leave the submap for `reset` or an empty name.
    ///
    /// Gives whether the map changed, which is when the event socket is told,
    /// or the sentence `setSubmap` refuses with. A name no bind was written
    /// in does not exist: entering it would leave a keyboard on which
    /// nothing but the universal binds work and no way written to get out.
    ///
    /// # Errors
    ///
    /// `Cannot set submap <name>, submap doesn't exist (wasn't registered!)`,
    /// which is Hyprland's own sentence.
    pub fn enter_submap(&mut self, name: &str) -> Result<bool, String> {
        let name = name.trim();
        let wanted = if name.is_empty() || name == "reset" {
            None
        } else if self.submaps.iter().any(|known| known == name) {
            Some(name.to_owned())
        } else {
            return Err(format!(
                "Cannot set submap {name}, submap doesn't exist (wasn't registered!)"
            ));
        };
        if wanted == self.submap {
            return Ok(false);
        }
        self.submap = wanted;
        Ok(true)
    }

    /// The keys held and the locks on, for `wl_keyboard.enter`.
    #[must_use]
    pub const fn keyboard(&self) -> &Keyboard {
        &self.keyboard
    }

    /// Put the keyboard in layout group `group`, and say whether it moved.
    ///
    /// What `hyprctl switchxkblayout` does, and the whole of what it does:
    /// the group wraps into the keymap's range, no new keymap goes to the
    /// clients, and the caller sends the `wl_keyboard.modifiers` that tells
    /// them which group they are in now.
    pub fn set_layout_group(&mut self, group: u32) -> bool {
        self.keyboard.set_group(group)
    }

    /// Where a `zwp_pointer_constraints_v1` is holding the pointer.
    ///
    /// The compositor works this out each pass from which surface the
    /// pointer is over and what that surface's client asked for: a
    /// constraint applies only while its own surface has the pointer, which
    /// is the protocol's rule and not the seat's to judge.
    pub const fn set_hold(&mut self, hold: Hold) {
        self.hold = hold;
    }

    /// Whether any pointer button is held down.
    ///
    /// A drag ends when the button that began it comes up, and the drag is
    /// the compositor's to end -- so it asks the seat, which is the only
    /// thing that sees the buttons.
    #[must_use]
    pub fn buttons_held(&self) -> bool {
        !self.buttons.is_empty()
    }

    /// The modifiers held now, which is what a key sent on to another
    /// window has to carry with it.
    #[must_use]
    pub fn modifiers(&self) -> Modifiers {
        self.keyboard.modifiers()
    }

    /// Whether the keybinds are the client's for now.
    pub const fn set_shortcuts_inhibited(&mut self, inhibited: bool) {
        self.shortcuts_inhibited = inhibited;
    }

    /// Put the pointer at `(x, y)`, as `movecursor` does.
    ///
    /// Gives the actions the move causes, which the caller delivers: a
    /// pointer that moved is a pointer that has entered or left a window,
    /// and a client is told that the same way whether a person moved the
    /// mouse or a dispatcher did.
    pub fn warp(&mut self, x: f64, y: f64) -> Vec<Action> {
        self.move_to(x, y)
    }

    /// Where the pointer is.
    #[must_use]
    pub const fn pointer(&self) -> (f64, f64) {
        self.pointer
    }

    /// Whether the pointer has been used, which is whether its position is
    /// something a device said rather than where it was put to start with.
    #[must_use]
    pub const fn pointer_used(&self) -> bool {
        self.used
    }

    /// Take one input, and say what to do about it.
    pub fn input(&mut self, input: Input) -> Vec<Action> {
        match input {
            Input::Key {
                code,
                pressed,
                repeat,
            } => self.key(code, pressed, repeat),
            Input::Motion { dx, dy } => {
                let mut actions = vec![Action::Relative { dx, dy }];
                actions.extend(self.move_to(self.pointer.0 + dx, self.pointer.1 + dy));
                actions
            }
            Input::Absolute { x, y } => {
                let at = (
                    self.origin.0 + x * self.screen.0,
                    self.origin.1 + y * self.screen.1,
                );
                let mut actions: Vec<Action> = self
                    .absolute
                    .replace(at)
                    .map(|was| (at.0 - was.0, at.1 - was.1))
                    .filter(|(dx, dy)| *dx != 0.0 || *dy != 0.0)
                    .map(|(dx, dy)| Action::Relative { dx, dy })
                    .into_iter()
                    .collect();
                actions.extend(self.move_to(at.0, at.1));
                actions
            }
            Input::Button { button, pressed } => {
                self.used = true;
                // Which buttons are down, for the one thing that has to
                // know: a drag ends when the last of them comes up.
                if pressed {
                    if !self.buttons.contains(&button) {
                        self.buttons.push(button);
                    }
                } else {
                    self.buttons.retain(|held| *held != button);
                }
                let mut actions = self.fired(Trigger::Button(button), pressed, false);
                actions.push(Action::Button { button, pressed });
                actions
            }
            Input::Axis { axis, value } => {
                let mut actions = self.fired(
                    Trigger::Wheel {
                        axis,
                        positive: value > 0.0,
                    },
                    true,
                    false,
                );
                actions.push(Action::Axis { axis, value });
                actions
            }
        }
    }

    /// Put the screen's top-left corner at `x`, `y` in the layout, moving
    /// the pointer with it: where the leftmost monitor is, when a rule put it
    /// somewhere other than 0, 0. A pointer nothing has moved yet is in the
    /// middle, as it starts.
    #[expect(
        clippy::cast_precision_loss,
        reason = "a layout's pixels are far inside f64's exact range"
    )]
    pub fn place_at(&mut self, x: i64, y: i64) {
        let (was_x, was_y) = self.origin;
        self.origin = (x as f64, y as f64);
        if self.used {
            let _ = self.move_to(
                self.pointer.0 - was_x + self.origin.0,
                self.pointer.1 - was_y + self.origin.1,
            );
        } else {
            self.pointer = (
                self.origin.0 + self.screen.0 / 2.0,
                self.origin.1 + self.screen.1 / 2.0,
            );
        }
    }

    /// The screen changed size, so the pointer's limits did.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.screen = (f64::from(width), f64::from(height));
        let _ = self.move_to(self.pointer.0, self.pointer.1);
    }

    fn key(&mut self, code: u16, pressed: bool, repeat: bool) -> Vec<Action> {
        let changed = self.keyboard.key(code, pressed);
        let mut actions = Vec::new();
        if changed && self.keyboard.is_modifier(code) {
            actions.push(Action::Modifiers(self.keyboard.modifiers()));
        }
        let fired = self.fired(Trigger::Key(code), pressed, repeat);
        let mut consumed = !fired.is_empty() && !self.consumes_nothing(Trigger::Key(code));
        actions.extend(fired);

        // The release of a key whose press was eaten is eaten as well.
        if pressed && consumed && !repeat && !self.eaten.contains(&code) {
            self.eaten.push(code);
        }
        if !pressed && let Some(at) = self.eaten.iter().position(|held| *held == code) {
            let _ = self.eaten.remove(at);
            consumed = true;
        }

        // A repeat is not sent: `wl_keyboard.key` has no way to say one, and
        // the client repeats for itself from `repeat_info`.
        if !consumed && !repeat {
            actions.push(Action::Key { code, pressed });
        }
        actions
    }

    /// The dispatchers the binds on `trigger` ask for.
    fn fired(&self, trigger: Trigger, pressed: bool, repeat: bool) -> Vec<Action> {
        // A client holding a shortcuts inhibitor gets every key, including
        // the ones a bind would have eaten. That is the whole of the
        // protocol: a virtual machine or a nested compositor needs `SUPER`.
        if self.shortcuts_inhibited {
            return Vec::new();
        }
        let held = self.keyboard.modifiers().depressed & COMPARED;
        self.binds
            .iter()
            .filter(|bind| bind.trigger == trigger)
            .filter(|bind| self.in_force(bind))
            .filter(|bind| bind.ignore_mods || bind.mods == held)
            // A drag is both halves of the press; everything else is one.
            .filter(|bind| bind.mouse || if bind.release { !pressed } else { pressed })
            .filter(|bind| !repeat || bind.repeat)
            .map(|bind| Action::Dispatch {
                name: bind.dispatcher.clone(),
                argument: if bind.mouse {
                    format!("{}{}", if pressed { '+' } else { '-' }, bind.argument)
                } else {
                    bind.argument.clone()
                },
                trigger: match trigger {
                    Trigger::Key(code) => Some((code, held)),
                    Trigger::Button(_) | Trigger::Wheel { .. } => None,
                },
            })
            .collect()
    }

    /// Whether every bind that matched `trigger` lets the key through.
    fn consumes_nothing(&self, trigger: Trigger) -> bool {
        self.binds
            .iter()
            .filter(|bind| bind.trigger == trigger)
            .filter(|bind| self.in_force(bind))
            .all(|bind| bind.non_consuming)
    }

    /// Whether a bind is in the map that is in force.
    fn in_force(&self, bind: &Bound) -> bool {
        if self.locked && !bind.while_locked {
            return false;
        }
        bind.universal || bind.submap == self.submap
    }

    fn move_to(&mut self, x: f64, y: f64) -> Vec<Action> {
        self.used = true;
        // A locked pointer does not move at all, which is what a game
        // reading relative movement wants: the arrow stays where it was and
        // the client is told the distance.
        let (x, y) = match self.hold {
            Hold::Free => (x, y),
            Hold::Locked => return Vec::new(),
            Hold::Inside(rect) => {
                // The rectangle is in logical pixels and the pointer in
                // the same; the last column and row inside it are where a
                // confined pointer may still be.
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "a screen's pixels are far inside f64's exact range"
                )]
                let edge = |near: i64, far: i64| (near as f64, far.saturating_sub(1) as f64);
                let (left, right) = edge(rect.x, rect.right());
                let (top, bottom) = edge(rect.y, rect.bottom());
                (
                    x.clamp(left, right.max(left)),
                    y.clamp(top, bottom.max(top)),
                )
            }
        };
        // The pointer may not leave the screen, and a NaN from a device that
        // reported nonsense must not become the position.
        let hold = |value: f64, from: f64, limit: f64| {
            if value.is_nan() {
                return from;
            }
            value.clamp(from, from + (limit - 1.0).max(0.0))
        };
        self.pointer = (
            hold(x, self.origin.0, self.screen.0),
            hold(y, self.origin.1, self.screen.1),
        );
        vec![Action::Pointer {
            x: self.pointer.0,
            y: self.pointer.1,
        }]
    }
}

/// The keymap a bind is resolved against, and whether it is the one the
/// configuration asked for.
///
/// The first layout, which is group zero. Hyprland resolves a bind against a
/// state it never gives a group or a modifier to
/// (`CKeybindManager::m_xkbTranslationState`), so a bind stays on the
/// physical key group zero puts it on however many layouts a person
/// configures and whichever of them they are typing in. That is deliberate
/// there and it is deliberate here: `SUPER, Q` must not move when the layout
/// does.
#[must_use]
pub fn chosen(config: &Config) -> (&'static Layout, bool) {
    let asked = chosen_layouts(config);
    asked
        .first()
        .copied()
        .unwrap_or_else(|| compositor_xkb::layout("", ""))
}

/// Every layout `config` asks for, in group order, each with whether it is
/// the one that was asked for.
///
/// `input:kb_layout = de,us` is a keyboard with two groups and
/// `input:kb_variant = nodeadkeys,` gives the first of them a variant: two
/// comma-separated lists read side by side, which is XKB's grammar and so
/// Hyprland's, since Hyprland hands the strings to libxkbcommon unread.
#[must_use]
pub fn chosen_layouts(config: &Config) -> Vec<(&'static Layout, bool)> {
    compositor_xkb::layouts(
        config.str("input:kb_layout").unwrap_or_default(),
        config.str("input:kb_variant").unwrap_or_default(),
    )
}

/// The layouts `config` asks for, in group order: what the keyboard reads
/// each key's effect on the modifiers from, so that right Alt is `AltGr` on a
/// German keyboard and `Alt` on an American one.
fn layouts_of(config: &Config) -> Vec<&'static Layout> {
    let layouts: Vec<&'static Layout> = chosen_layouts(config)
        .into_iter()
        .map(|(layout, _)| layout)
        .collect();
    if layouts.is_empty() {
        vec![chosen(config).0]
    } else {
        layouts
    }
}

/// How many layout groups the keymap `config` asks for has.
///
/// At least one: an unset or unreadable `input:kb_layout` is the fallback
/// layout, which is one group, and a keymap with none is not something a
/// client could be told about.
#[must_use]
pub fn group_count(config: &Config) -> u32 {
    u32::try_from(chosen_layouts(config).len())
        .unwrap_or(1)
        .max(1)
}

/// What sets `key` off, or `None` for a key this keymap does not have.
fn trigger_of(layout: &'static Layout, key: &Key) -> Option<Trigger> {
    Some(match key {
        Key::Sym(name) => Trigger::Key(layout.code_of(name)?),
        // `code:NN` is the keycode as `xev` prints it, which is XKB's and so
        // eight above evdev's. Hyprland reads it the same way.
        Key::Code(code) => {
            Trigger::Key(u16::try_from(code.checked_sub(compositor_xkb::XKB_OFFSET)?).ok()?)
        }
        Key::Mouse(button) => Trigger::Button(*button),
        Key::Wheel(name) => {
            let (axis, positive) = match name.as_str() {
                // Scrolling up is a negative movement along the vertical
                // axis, as `wl_pointer.axis` has it: the value is how far the
                // surface's content moved, not the finger.
                "mouse_up" => (
                    compositor_protocol::core::wl_pointer::axis::VERTICAL_SCROLL,
                    false,
                ),
                "mouse_down" => (
                    compositor_protocol::core::wl_pointer::axis::VERTICAL_SCROLL,
                    true,
                ),
                "mouse_left" => (
                    compositor_protocol::core::wl_pointer::axis::HORIZONTAL_SCROLL,
                    false,
                ),
                "mouse_right" => (
                    compositor_protocol::core::wl_pointer::axis::HORIZONTAL_SCROLL,
                    true,
                ),
                _ => return None,
            };
            Trigger::Wheel { axis, positive }
        }
    })
}

/// What Hyprland's modifier names mean here, so that a `Mods` from the
/// configuration and a mask from the keymap are the same bits.
///
/// They are, and this says so rather than leaving it to be noticed: XKB
/// declares `Shift`, `Lock`, `Control` and `Mod1` to `Mod5` in that order,
/// and `src/user/system/linux/compositor/config`'s `Mods` numbers them the same way.
const _: () = {
    assert!(Mods::SHIFT == generated::SHIFT);
    assert!(Mods::CAPS == generated::LOCK);
    assert!(Mods::CTRL == generated::CONTROL);
    assert!(Mods::ALT == generated::MOD1);
    assert!(Mods::MOD2 == generated::MOD2);
    assert!(Mods::MOD3 == generated::MOD3);
    assert!(Mods::LOGO == generated::MOD4);
    assert!(Mods::MOD5 == generated::MOD5);
};

#[cfg(test)]
mod tests;
