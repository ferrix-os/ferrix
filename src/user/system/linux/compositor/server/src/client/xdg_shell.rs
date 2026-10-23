//! `xdg-shell`: what makes a surface a window or a popup.
//!
//! A client asks `xdg_wm_base` to wrap a surface in an `xdg_surface`, and
//! then gives that one of two roles: an `xdg_toplevel`, a window the layout
//! places, or an `xdg_popup`, a menu or a tooltip placed against its parent
//! by the numbers an `xdg_positioner` holds.
//!
//! # The configure conversation
//!
//! Neither side decides a window's size alone. The compositor sends a
//! `configure` with a serial, the client draws at that size and acks the
//! serial, and only then may it attach a buffer. A surface that commits one
//! before its first ack is `unconfigured_buffer`, which is checked where
//! `wl_surface.commit` is read. [`Client::configure_toplevel`] and
//! [`Client::configure_popup`] are the compositor's half.

use compositor_protocol::xdg_shell::{self, xdg_surface, xdg_toplevel, xdg_wm_base};
use compositor_wire::{Arg, ArgType, ObjectId};

use crate::client::{Client, Event, Fatal};
use crate::role::Role;
use crate::xdg::{Popup, Positioner, Toplevel, XdgRole, XdgSurface};

impl Client {
    /// `xdg_wm_base`: `create_positioner`, `get_xdg_surface` and `pong`.
    pub(super) fn xdg_wm_base(&mut self, version: u32, opcode: u16, args: &[Arg<'_>]) {
        match opcode {
            xdg_wm_base::request::CREATE_POSITIONER => {
                let Some(id) = args.first().and_then(Arg::as_object) else {
                    return;
                };
                // A positioner is a bag of numbers a popup reads, and it
                // starts empty: the protocol requires `set_size` and
                // `set_anchor_rect` before `get_popup` uses it.
                if self.make(id, &xdg_shell::XDG_POSITIONER, version, Role::XdgPositioner) {
                    let _ = self.positioners.insert(id, Positioner::default());
                }
            }
            xdg_wm_base::request::GET_XDG_SURFACE => {
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
                // A surface that already has a buffer may not be given a
                // role: `xdg_surface`'s description says it must be unmapped
                // and have no buffer attached or committed.
                let has_buffer = self
                    .surfaces
                    .get(&surface)
                    .is_some_and(|state| state.is_mapped() || state.pending.buffer.is_some());
                if has_buffer {
                    self.fail(Fatal::Interface {
                        object: id,
                        code: xdg_surface::error::UNCONFIGURED_BUFFER,
                        text: "a surface with a buffer cannot be given a role".to_owned(),
                    });
                    return;
                }
                if self.xdg_surfaces.values().any(|xdg| xdg.surface == surface) {
                    self.fail(Fatal::Interface {
                        object: id,
                        code: xdg_wm_base::error::ROLE,
                        text: "that surface already has an xdg_surface".to_owned(),
                    });
                    return;
                }
                if self.make(id, &xdg_shell::XDG_SURFACE, version, Role::XdgSurface) {
                    let _ = self.xdg_surfaces.insert(id, XdgSurface::new(surface));
                }
            }
            // `pong` answers the `ping` that asks whether a client is still
            // there. Nothing pings yet, so an unsolicited pong is ignored
            // rather than refused: the protocol gives no error for one.
            xdg_wm_base::request::PONG => {}
            _ => {}
        }
    }

    /// `xdg_surface`: the role requests, the window geometry and the ack.
    pub(super) fn xdg_surface_request(
        &mut self,
        sender: ObjectId,
        version: u32,
        opcode: u16,
        args: &[Arg<'_>],
    ) {
        match opcode {
            xdg_surface::request::GET_TOPLEVEL => self.get_toplevel(sender, version, args),
            xdg_surface::request::GET_POPUP => self.get_popup(sender, version, args),
            xdg_surface::request::SET_WINDOW_GEOMETRY => {
                let numbers: Vec<i32> = (0..4)
                    .filter_map(|index| args.get(index).and_then(Arg::as_int))
                    .collect();
                let [x, y, width, height] = numbers.as_slice() else {
                    return;
                };
                if *width <= 0 || *height <= 0 {
                    self.fail(Fatal::Interface {
                        object: sender,
                        code: xdg_surface::error::INVALID_SIZE,
                        text: format!("a window geometry of {width}x{height}"),
                    });
                    return;
                }
                if let Some(xdg) = self.xdg_surfaces.get_mut(&sender) {
                    xdg.geometry = Some((*x, *y, *width, *height));
                }
            }
            xdg_surface::request::ACK_CONFIGURE => {
                let Some(serial) = args.first().and_then(Arg::as_uint) else {
                    return;
                };
                let acked = self
                    .xdg_surfaces
                    .get_mut(&sender)
                    .is_some_and(|xdg| xdg.ack(serial));
                if !acked {
                    self.fail(Fatal::Interface {
                        object: sender,
                        code: xdg_surface::error::INVALID_SERIAL,
                        text: format!("serial {serial} was never sent or is already acked"),
                    });
                }
            }
            _ => {}
        }
    }

    /// `xdg_surface.get_toplevel`: the surface becomes a window.
    fn get_toplevel(&mut self, sender: ObjectId, version: u32, args: &[Arg<'_>]) {
        let Some(id) = args.first().and_then(Arg::as_object) else {
            return;
        };
        let Some(xdg) = self.xdg_surfaces.get(&sender) else {
            return;
        };
        if xdg.role.is_some() {
            self.fail(Fatal::Interface {
                object: sender,
                code: xdg_surface::error::ALREADY_CONSTRUCTED,
                text: "this xdg_surface already has a role".to_owned(),
            });
            return;
        }
        let surface = xdg.surface;
        if !self.make(id, &xdg_shell::XDG_TOPLEVEL, version, Role::XdgToplevel) {
            return;
        }
        if let Some(xdg) = self.xdg_surfaces.get_mut(&sender) {
            xdg.role = Some(XdgRole::Toplevel(id));
        }
        let _ = self.toplevels.insert(
            id,
            Toplevel {
                xdg_surface: sender,
                surface,
                ..Toplevel::default()
            },
        );
        self.events.push(Event::ToplevelCreated {
            toplevel: id,
            surface,
        });
    }

    /// `xdg_surface.get_popup`: the surface becomes a popup, placed by the
    /// numbers its positioner holds when this arrives.
    fn get_popup(&mut self, sender: ObjectId, version: u32, args: &[Arg<'_>]) {
        let Some(id) = args.first().and_then(Arg::as_object) else {
            return;
        };
        // The parent is nullable in the protocol: another extension
        // gives such a popup its parent, and the one this compositor
        // has is `zwlr_layer_surface_v1.get_popup`, which is how a
        // bar's tooltips and menus are made. Such a popup is kept
        // and placed once a layer surface takes it.
        let (parent, positioner) = (
            args.get(1).and_then(Arg::as_object),
            args.get(2).and_then(Arg::as_object),
        );
        let Some(xdg) = self.xdg_surfaces.get(&sender) else {
            return;
        };
        if xdg.role.is_some() {
            self.fail(Fatal::Interface {
                object: sender,
                code: xdg_surface::error::ALREADY_CONSTRUCTED,
                text: "this xdg_surface already has a role".to_owned(),
            });
            return;
        }
        let surface = xdg.surface;
        let parent = parent.unwrap_or(ObjectId::NULL);
        if !parent.is_null() && !self.xdg_surfaces.contains_key(&parent) {
            self.fail(Fatal::WrongInterface {
                object: parent,
                wanted: "xdg_surface",
            });
            return;
        }
        let Some(held) = positioner.and_then(|id| self.positioners.get(&id)).copied() else {
            self.fail(Fatal::WrongInterface {
                object: positioner.unwrap_or(ObjectId::NULL),
                wanted: "xdg_positioner",
            });
            return;
        };
        // A positioner without a size or an anchor rectangle is the
        // one error a popup can earn before it exists.
        if !held.is_complete() {
            self.fail(Fatal::Interface {
                object: sender,
                code: xdg_wm_base::error::INVALID_POSITIONER,
                text: "the positioner has no size or no anchor rectangle".to_owned(),
            });
            return;
        }
        if !self.make(id, &xdg_shell::XDG_POPUP, version, Role::XdgPopup) {
            return;
        }
        if let Some(xdg) = self.xdg_surfaces.get_mut(&sender) {
            xdg.role = Some(XdgRole::Popup(id));
        }
        let _ = self.popups.insert(
            id,
            Popup {
                surface,
                xdg_surface: sender,
                parent,
                layer_parent: None,
                positioner: held,
                placed: None,
                grabbed: false,
            },
        );
        // A parentless popup waits for the layer surface that will
        // take it; the compositor places it then.
        if parent.is_null() {
            return;
        }
        self.events.push(Event::PopupCreated {
            popup: id,
            surface,
            parent,
        });
    }

    /// `xdg_toplevel`: what a client says about its window.
    pub(super) fn xdg_toplevel_request(&mut self, sender: ObjectId, opcode: u16, args: &[Arg<'_>]) {
        match opcode {
            xdg_toplevel::request::SET_TITLE | xdg_toplevel::request::SET_APP_ID => {
                let text = args.first().and_then(Arg::as_str).unwrap_or("").to_owned();
                let Some(top) = self.toplevels.get_mut(&sender) else {
                    return;
                };
                if opcode == xdg_toplevel::request::SET_TITLE {
                    top.title = text;
                } else {
                    top.app_id = text;
                }
                self.events
                    .push(Event::ToplevelRenamed { toplevel: sender });
            }
            xdg_toplevel::request::SET_PARENT => {
                let Some(parent) = args.first().and_then(Arg::as_object) else {
                    return;
                };
                if !parent.is_null() && !self.toplevels.contains_key(&parent) {
                    self.fail(Fatal::WrongInterface {
                        object: parent,
                        wanted: "xdg_toplevel",
                    });
                    return;
                }
                if let Some(top) = self.toplevels.get_mut(&sender) {
                    top.parent = (!parent.is_null()).then_some(parent);
                }
            }
            xdg_toplevel::request::SET_MAX_SIZE | xdg_toplevel::request::SET_MIN_SIZE => {
                let (Some(width), Some(height)) = (
                    args.first().and_then(Arg::as_int),
                    args.get(1).and_then(Arg::as_int),
                ) else {
                    return;
                };
                if width < 0 || height < 0 {
                    self.fail(Fatal::Interface {
                        object: sender,
                        code: xdg_toplevel::error::INVALID_SIZE,
                        text: format!("a size of {width}x{height}"),
                    });
                    return;
                }
                let Some(top) = self.toplevels.get_mut(&sender) else {
                    return;
                };
                if opcode == xdg_toplevel::request::SET_MAX_SIZE {
                    top.max_size = (width, height);
                } else {
                    top.min_size = (width, height);
                }
            }
            xdg_toplevel::request::SET_MAXIMIZED
            | xdg_toplevel::request::UNSET_MAXIMIZED
            | xdg_toplevel::request::SET_FULLSCREEN
            | xdg_toplevel::request::UNSET_FULLSCREEN => {
                let Some(top) = self.toplevels.get_mut(&sender) else {
                    return;
                };
                match opcode {
                    xdg_toplevel::request::SET_MAXIMIZED => top.maximized = true,
                    xdg_toplevel::request::UNSET_MAXIMIZED => top.maximized = false,
                    xdg_toplevel::request::SET_FULLSCREEN => top.fullscreen = true,
                    _ => top.fullscreen = false,
                }
                let (maximized, fullscreen) = (top.maximized, top.fullscreen);
                self.events.push(Event::ToplevelAsked {
                    toplevel: sender,
                    maximized,
                    fullscreen,
                });
            }
            // `move` and `resize`: the client was pressed on its own title
            // bar or edge and hands the drag to the compositor. Whether one
            // starts is the loop's, which knows the buttons and whether the
            // window floats; the edges are checked here, as the protocol
            // makes a wrong one an error.
            xdg_toplevel::request::MOVE | xdg_toplevel::request::RESIZE => {
                let edges = if opcode == xdg_toplevel::request::MOVE {
                    xdg_toplevel::resize_edge::NONE
                } else {
                    let edges = args.get(2).and_then(Arg::as_uint).unwrap_or(0);
                    if !matches!(edges, 1 | 2 | 4..=6 | 8..=10) {
                        self.fail(Fatal::Interface {
                            object: sender,
                            code: xdg_toplevel::error::INVALID_RESIZE_EDGE,
                            text: format!("a resize edge of {edges}"),
                        });
                        return;
                    }
                    edges
                };
                if self.toplevels.contains_key(&sender) {
                    self.events.push(Event::ToplevelDragAsked {
                        toplevel: sender,
                        edges,
                    });
                }
            }
            // `set_minimized`: the compositor decides; a floating desktop
            // minimizes (`docs/DESKTOP.md` §1.2).
            xdg_toplevel::request::SET_MINIMIZED if self.toplevels.contains_key(&sender) => {
                self.events
                    .push(Event::ToplevelMinimizeAsked { toplevel: sender });
            }
            // `show_window_menu` asks for a thing this compositor does not
            // do. The protocol says a compositor may ignore it, and
            // Hyprland does.
            _ => {}
        }
    }

    /// `xdg_positioner`: the numbers a popup is placed with.
    ///
    /// Each is recorded and none is acted on: where a popup goes is worked
    /// out once, when `get_popup` reads the positioner, because the protocol
    /// says a positioner is a value copied at that moment and a change after
    /// it moves nothing.
    pub(super) fn positioner(&mut self, sender: ObjectId, opcode: u16, args: &[Arg<'_>]) {
        use xdg_shell::xdg_positioner::request;
        let numbers: Vec<i32> = args.iter().filter_map(Arg::as_int).collect();
        let Some(held) = self.positioners.get_mut(&sender) else {
            return;
        };
        match opcode {
            request::SET_SIZE => {
                let [width, height] = numbers.as_slice() else {
                    return;
                };
                if *width <= 0 || *height <= 0 {
                    self.fail(Fatal::Interface {
                        object: sender,
                        code: xdg_shell::xdg_positioner::error::INVALID_INPUT,
                        text: format!("a popup of {width}x{height}"),
                    });
                    return;
                }
                held.size = (*width, *height);
            }
            request::SET_ANCHOR_RECT => {
                let [x, y, width, height] = numbers.as_slice() else {
                    return;
                };
                if *width <= 0 || *height <= 0 {
                    self.fail(Fatal::Interface {
                        object: sender,
                        code: xdg_shell::xdg_positioner::error::INVALID_INPUT,
                        text: format!("an anchor rectangle of {width}x{height}"),
                    });
                    return;
                }
                held.anchor_rect = (*x, *y, *width, *height);
            }
            request::SET_ANCHOR | request::SET_GRAVITY => {
                let Some(value) = args.first().and_then(Arg::as_uint) else {
                    return;
                };
                if value > 8 {
                    self.fail(Fatal::Interface {
                        object: sender,
                        code: xdg_shell::xdg_positioner::error::INVALID_INPUT,
                        text: format!("{value} is not an anchor or a gravity"),
                    });
                    return;
                }
                if opcode == request::SET_ANCHOR {
                    held.anchor = value;
                } else {
                    held.gravity = value;
                }
            }
            request::SET_CONSTRAINT_ADJUSTMENT => {
                held.adjust = args.first().and_then(Arg::as_uint).unwrap_or(0);
            }
            request::SET_OFFSET => {
                let [x, y] = numbers.as_slice() else {
                    return;
                };
                held.offset = (*x, *y);
            }
            request::SET_REACTIVE => held.reactive = true,
            // `set_parent_size` and `set_parent_configure` are for a
            // reactive popup whose parent is being resized: this compositor
            // places a popup against the parent's geometry as it is, so both
            // are read and nothing is kept.
            _ => {}
        }
    }

    /// `xdg_popup`: `grab`, `reposition` and `destroy`.
    pub(super) fn popup_request(&mut self, sender: ObjectId, opcode: u16, args: &[Arg<'_>]) {
        use xdg_shell::xdg_popup::request;
        match opcode {
            request::GRAB => {
                if let Some(popup) = self.popups.get_mut(&sender) {
                    popup.grabbed = true;
                }
                self.events.push(Event::PopupGrabbed { popup: sender });
            }
            request::REPOSITION => {
                let held = args
                    .first()
                    .and_then(Arg::as_object)
                    .and_then(|id| self.positioners.get(&id))
                    .copied();
                let token = args.get(1).and_then(Arg::as_uint).unwrap_or(0);
                if let (Some(held), Some(popup)) = (held, self.popups.get_mut(&sender)) {
                    popup.positioner = held;
                    popup.placed = None;
                }
                // `repositioned` goes before the `configure` the compositor
                // will send, which is what tells the client the configure
                // that follows is the answer to this request and not to
                // something else.
                let _ = self.out.write(
                    sender,
                    xdg_shell::xdg_popup::event::REPOSITIONED,
                    &[ArgType::Uint],
                    &[Arg::Uint(token)],
                );
                let (surface, parent) = self
                    .popups
                    .get(&sender)
                    .map_or((ObjectId::NULL, ObjectId::NULL), |popup| {
                        (popup.surface, popup.parent)
                    });
                self.events.push(Event::PopupCreated {
                    popup: sender,
                    surface,
                    parent,
                });
            }
            request::DESTROY => {
                let _ = self.popups.remove(&sender);
                self.events.push(Event::PopupGone { popup: sender });
            }
            _ => {}
        }
    }

    /// The `xdg_surface` `id` names, if it is one.
    #[must_use]
    pub fn xdg_surface(&self, id: ObjectId) -> Option<&XdgSurface> {
        self.xdg_surfaces.get(&id)
    }

    /// The part of the `wl_surface` `surface` that is its window or its
    /// popup, as its `xdg_surface.set_window_geometry` said, in surface
    /// coordinates: `None` for a surface with no xdg role or that never
    /// said, whose window is the whole surface. A popup's positioner places
    /// this part, not the shadow a menu draws around it.
    #[must_use]
    pub fn window_geometry(&self, surface: ObjectId) -> Option<(i32, i32, i32, i32)> {
        self.xdg_surfaces
            .values()
            .find(|xdg| xdg.surface == surface && xdg.role.is_some())
            .and_then(|xdg| xdg.geometry)
    }

    /// The toplevel `id` names, if it is one.
    #[must_use]
    pub fn toplevel(&self, id: ObjectId) -> Option<&Toplevel> {
        self.toplevels.get(&id)
    }

    /// Every toplevel, in id order: the windows this client has.
    pub fn toplevels(&self) -> impl Iterator<Item = (ObjectId, &Toplevel)> {
        self.toplevels.iter().map(|(id, top)| (*id, top))
    }

    /// Tell a toplevel what size to be and what state it is in.
    ///
    /// This is the compositor's half of the configure conversation: the
    /// layout decides a size, the client draws at it and acks. `states` is
    /// `xdg_toplevel.state` values, which for a tiling compositor is mostly
    /// `activated` and the `tiled_*` edges.
    pub fn configure_toplevel(
        &mut self,
        toplevel: ObjectId,
        width: i32,
        height: i32,
        states: &[u32],
    ) {
        let Some(top) = self.toplevels.get_mut(&toplevel) else {
            return;
        };
        top.configured = (width, height);
        top.states = states.to_vec();
        let xdg = top.xdg_surface;
        let packed: Vec<u8> = states
            .iter()
            .flat_map(|state| state.to_le_bytes())
            .collect();
        let _ = self.out.write(
            toplevel,
            xdg_toplevel::event::CONFIGURE,
            &[ArgType::Int, ArgType::Int, ArgType::Array],
            &[Arg::Int(width), Arg::Int(height), Arg::Array(&packed)],
        );
        let serial = self.next_serial();
        let _ = self.out.write(
            xdg,
            xdg_surface::event::CONFIGURE,
            &[ArgType::Uint],
            &[Arg::Uint(serial)],
        );
        if let Some(surface) = self.xdg_surfaces.get_mut(&xdg) {
            surface.configure_sent(serial);
        }
    }

    /// Ask a toplevel to close, as `killactive` does.
    ///
    /// It is a request, not an order: a client may put up "save your work?"
    /// and never close. Hyprland's `killactive` sends this and nothing else.
    pub fn close_toplevel(&mut self, toplevel: ObjectId) {
        if !self.toplevels.contains_key(&toplevel) {
            return;
        }
        let _ = self
            .out
            .write(toplevel, xdg_toplevel::event::CLOSE, &[], &[]);
    }

    /// Tell a popup where it is, and its `xdg_surface` that the state is
    /// whole.
    ///
    /// `x` and `y` are in the parent's surface-local coordinates, which is
    /// what the protocol says `xdg_popup.configure` carries.
    pub fn configure_popup(&mut self, popup: ObjectId, at: (i32, i32, i32, i32)) {
        let Some(held) = self.popups.get_mut(&popup) else {
            return;
        };
        held.placed = Some(at);
        let xdg_surface = held.xdg_surface;
        let (x, y, width, height) = at;
        let _ = self.out.write(
            popup,
            xdg_shell::xdg_popup::event::CONFIGURE,
            &[ArgType::Int, ArgType::Int, ArgType::Int, ArgType::Int],
            &[Arg::Int(x), Arg::Int(y), Arg::Int(width), Arg::Int(height)],
        );
        let serial = self.next_serial();
        let _ = self.out.write(
            xdg_surface,
            xdg_surface::event::CONFIGURE,
            &[ArgType::Uint],
            &[Arg::Uint(serial)],
        );
        if let Some(surface) = self.xdg_surfaces.get_mut(&xdg_surface) {
            surface.configure_sent(serial);
        }
    }

    /// Tell a popup it has been dismissed.
    ///
    /// The client is to destroy it: a menu that was clicked away is gone,
    /// and the compositor says so rather than waiting to be told.
    pub fn popup_done(&mut self, popup: ObjectId) {
        if !self.popups.contains_key(&popup) {
            return;
        }
        let _ = self
            .out
            .write(popup, xdg_shell::xdg_popup::event::POPUP_DONE, &[], &[]);
    }

    /// One popup, if this client has it.
    #[must_use]
    pub fn popup(&self, popup: ObjectId) -> Option<&Popup> {
        self.popups.get(&popup)
    }

    /// Every popup it has, in the order they were made.
    pub fn popups(&self) -> impl Iterator<Item = (ObjectId, &Popup)> {
        self.popups.iter().map(|(id, popup)| (*id, popup))
    }
}
