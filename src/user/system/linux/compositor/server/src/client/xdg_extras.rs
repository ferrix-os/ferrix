//! Three `xdg` extensions that say more about a window than `xdg-shell`
//! does.
//!
//! * `xdg-decoration` -- who draws the title bar. The compositor, always:
//!   a tiling layout draws a border and no title, and a toolkit told
//!   `server_side` draws nothing of its own inside the rectangle it was
//!   given.
//! * `xdg-toplevel-icon` -- the icon a taskbar shows for a window, as a
//!   name in an icon theme or as pixels. The name is kept and the pixels
//!   are not, since nothing here draws them.
//! * `xdg-activation` -- a token one program hands another so that the
//!   second may take the focus. Only a token the compositor made is taken,
//!   and only once, which is what stops any program stealing the focus.

use compositor_protocol::toplevel_icon::{
    self, xdg_toplevel_icon_manager_v1, xdg_toplevel_icon_v1,
};
use compositor_protocol::xdg_activation::{self, xdg_activation_token_v1, xdg_activation_v1};
use compositor_protocol::xdg_decoration::{
    self, zxdg_decoration_manager_v1, zxdg_toplevel_decoration_v1,
};
use compositor_wire::{Arg, ArgType, ObjectId};

use crate::client::{Client, Event, Fatal};
use crate::role::Role;

impl Client {
    /// `zxdg_decoration_manager_v1`: `get_toplevel_decoration`.
    ///
    /// A tiling compositor draws the border and the client draws nothing, so
    /// the answer is always `server_side` and it is sent at once rather than
    /// waited for: the protocol says a compositor may configure a decoration
    /// as soon as it is made, and a client that asked for `client_side` and
    /// is told `server_side` draws no title bar, which is the point of
    /// offering this at all.
    ///
    /// Offering it matters more than it looks. A toolkit that finds no
    /// `zxdg_decoration_manager_v1` assumes client-side decorations and
    /// draws a title bar, a shadow and a resize border into its own surface
    /// -- inside the rectangle the tiling gave it.
    pub(super) fn decoration_manager(&mut self, version: u32, opcode: u16, args: &[Arg<'_>]) {
        if opcode != zxdg_decoration_manager_v1::request::GET_TOPLEVEL_DECORATION {
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
        if !self.make(
            id,
            &xdg_decoration::ZXDG_TOPLEVEL_DECORATION_V1,
            version,
            Role::ToplevelDecoration,
        ) {
            return;
        }
        let _ = self.xdg_decorations.insert(id, toplevel);
        self.configure_decoration(id);
    }

    /// Whether `toplevel` asked for the compositor's frame -- through
    /// `xdg-decoration` or KDE's server decoration -- and so draws no title
    /// bar of its own: what a compositor that draws title bars draws one
    /// for (`docs/DESKTOP.md` §2.2).
    #[must_use]
    pub fn wants_server_frame(&self, toplevel: ObjectId) -> bool {
        if self
            .xdg_decorations
            .iter()
            .any(|(decoration, top)| *top == toplevel && !self.client_frames.contains(decoration))
        {
            return true;
        }
        let Some(surface) = self.toplevels.get(&toplevel).map(|top| top.surface) else {
            return false;
        };
        self.kde_decorations.values().any(|on| *on == surface)
    }

    /// Answer a client's `set_mode(client_side)` with `client_side`, so it
    /// draws its own frame and the compositor draws no title bar over it --
    /// Chrome's tab strip under a second bar was the customer's "double
    /// taskbar" (2026-10-24) -- rather than `server_side` whatever was
    /// asked. hyprix turns it on where it draws title bars itself.
    pub const fn set_honour_client_frames(&mut self, on: bool) {
        self.honour_client_frames = on;
    }

    /// `zxdg_toplevel_decoration_v1`: `set_mode` and `unset_mode`.
    ///
    /// Answered `server_side`, the compositor drawing the decorations,
    /// unless [`Client::set_honour_client_frames`] is on and the client
    /// asked to draw its own: then `client_side`. `unset_mode` is the
    /// compositor's choice again.
    pub(super) fn toplevel_decoration(&mut self, sender: ObjectId, opcode: u16, args: &[Arg<'_>]) {
        use zxdg_toplevel_decoration_v1::request;
        match opcode {
            request::SET_MODE => {
                let mode = args.first().and_then(Arg::as_uint).unwrap_or(0);
                if mode != zxdg_toplevel_decoration_v1::mode::CLIENT_SIDE
                    && mode != zxdg_toplevel_decoration_v1::mode::SERVER_SIDE
                {
                    self.fail(Fatal::Interface {
                        object: sender,
                        code: zxdg_toplevel_decoration_v1::error::INVALID_MODE,
                        text: format!("{mode} is not a decoration mode"),
                    });
                    return;
                }
                if self.honour_client_frames
                    && mode == zxdg_toplevel_decoration_v1::mode::CLIENT_SIDE
                {
                    let _ = self.client_frames.insert(sender);
                } else {
                    let _ = self.client_frames.remove(&sender);
                }
                self.configure_decoration(sender);
            }
            request::UNSET_MODE => {
                let _ = self.client_frames.remove(&sender);
                self.configure_decoration(sender);
            }
            _ => {}
        }
    }

    /// Tell a decoration who draws it: the client where it asked and that
    /// is honoured, else the compositor.
    fn configure_decoration(&mut self, decoration: ObjectId) {
        let mode = if self.client_frames.contains(&decoration) {
            zxdg_toplevel_decoration_v1::mode::CLIENT_SIDE
        } else {
            zxdg_toplevel_decoration_v1::mode::SERVER_SIDE
        };
        let _ = self.out.write(
            decoration,
            zxdg_toplevel_decoration_v1::event::CONFIGURE,
            &[ArgType::Uint],
            &[Arg::Uint(mode)],
        );
    }

    /// `xdg_toplevel_icon_manager_v1`: `create_icon` and `set_icon`.
    ///
    /// The sizes are announced at bind and the icon is taken as given: what
    /// a compositor does with one is draw it in a taskbar, and this one has
    /// no taskbar of its own -- the bar is a client, and what it draws is
    /// its own business. Answering rather than refusing is what matters: a
    /// toolkit that finds no manager logs a warning on every start.
    pub(super) fn icon_manager(&mut self, version: u32, opcode: u16, args: &[Arg<'_>]) {
        match opcode {
            xdg_toplevel_icon_manager_v1::request::CREATE_ICON => {
                let Some(id) = args.first().and_then(Arg::as_object) else {
                    return;
                };
                let _ = self.make(
                    id,
                    &toplevel_icon::XDG_TOPLEVEL_ICON_V1,
                    version,
                    Role::Icon,
                );
            }
            xdg_toplevel_icon_manager_v1::request::SET_ICON => {
                let (Some(toplevel), icon) = (
                    args.first().and_then(Arg::as_object),
                    args.get(1).and_then(Arg::as_object),
                ) else {
                    return;
                };
                let name = icon
                    .and_then(|icon| self.icons.get(&icon))
                    .cloned()
                    .unwrap_or_default();
                if let Some(top) = self.toplevels.get_mut(&toplevel) {
                    top.icon = name;
                }
            }
            _ => {}
        }
    }

    /// `xdg_toplevel_icon_v1`: `set_name` and `add_buffer`.
    ///
    /// The name is kept, which is what a taskbar looks up in an icon theme.
    /// The buffers are accepted and not kept: this compositor draws no icon
    /// itself, and holding a client's pixels for something nobody draws is
    /// memory nobody asked for.
    pub(super) fn icon(&mut self, sender: ObjectId, opcode: u16, args: &[Arg<'_>]) {
        if opcode != xdg_toplevel_icon_v1::request::SET_NAME {
            return;
        }
        if let Some(name) = args.first().and_then(Arg::as_str) {
            let _ = self.icons.insert(sender, name.to_owned());
        }
    }

    /// `xdg_activation_v1`: `get_activation_token` and `activate`.
    ///
    /// A program that wants another raised asks for a token, hands it over
    /// by whatever means it has -- an environment variable, a command line
    /// -- and the other program passes it to `activate`. The token is a
    /// string the compositor makes and only it can make, which is what stops
    /// any program stealing the focus whenever it likes.
    pub(super) fn activation(&mut self, version: u32, opcode: u16, args: &[Arg<'_>]) {
        match opcode {
            xdg_activation_v1::request::GET_ACTIVATION_TOKEN => {
                let Some(id) = args.first().and_then(Arg::as_object) else {
                    return;
                };
                let _ = self.make(
                    id,
                    &xdg_activation::XDG_ACTIVATION_TOKEN_V1,
                    version,
                    Role::ActivationToken,
                );
            }
            xdg_activation_v1::request::ACTIVATE => {
                let (Some(token), Some(surface)) = (
                    args.first().and_then(Arg::as_str),
                    args.get(1).and_then(Arg::as_object),
                ) else {
                    return;
                };
                self.events.push(Event::ActivationAsked {
                    token: token.to_owned(),
                    surface,
                });
            }
            _ => {}
        }
    }

    /// `xdg_activation_token_v1`: `commit` is where the token is handed
    /// back.
    ///
    /// `set_serial`, `set_app_id` and `set_surface` say who is asking and
    /// why; they are read and the token is given whatever they said, because
    /// this compositor grants an activation to whoever has a token it made.
    pub(super) fn activation_token(&mut self, sender: ObjectId, opcode: u16, _args: &[Arg<'_>]) {
        if opcode != xdg_activation_token_v1::request::COMMIT {
            return;
        }
        // One token a request, made here and never twice: a client that
        // could guess another's token could steal the focus.
        self.token = self.token.wrapping_add(1);
        let token = format!("hyprix-{}-{}", self.serial, self.token);
        let _ = self.tokens.insert(token.clone());
        let _ = self.out.write(
            sender,
            xdg_activation_token_v1::event::DONE,
            &[ArgType::Str { nullable: false }],
            &[Arg::Str(Some(&token))],
        );
    }

    /// Whether `token` is one this client was given and has not used.
    #[must_use]
    pub fn takes_token(&mut self, token: &str) -> bool {
        self.tokens.remove(token)
    }
}
