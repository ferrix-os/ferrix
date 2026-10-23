//! `wl_compositor` and what it makes: surfaces, regions and subsurfaces.
//!
//! A `wl_surface` is a rectangle of pixels a client fills and commits, and
//! nothing on the screen is anything else: a window, a bar, a cursor and a
//! lock screen are each a surface that another protocol gave a role. Its
//! state is double-buffered -- `attach`, `damage`, the two regions, the
//! transform and the scale all go to a pending state that `commit` makes
//! current at once -- so a client is never seen half way through a frame.
//!
//! Beside it, the three protocols that say where a surface sits and at what
//! size: `wl_subcompositor`, which hangs one surface off another (a title
//! bar, a shadow, the video inside a player); `wp_viewporter`, which crops
//! the buffer and scales it to a size the client chooses; and
//! `wp_fractional_scale_v1`, which tells a client what scale to draw at.
//!
//! Which role a surface has is the shells' business, in `xdg_shell.rs` and
//! `layer_shell.rs`. This module keeps the surface itself.

use compositor_protocol::core::{
    self, wl_compositor, wl_output, wl_region, wl_subcompositor, wl_subsurface, wl_surface,
};
use compositor_protocol::fractional_scale::{
    self, wp_fractional_scale_manager_v1, wp_fractional_scale_v1,
};
use compositor_protocol::viewporter::{self, wp_viewport, wp_viewporter};
use compositor_protocol::xdg_shell::xdg_surface;
use compositor_wire::{Arg, ArgType, Fixed, ObjectId};

use crate::client::{Client, Event, Fatal};
use crate::role::Role;
use crate::surface::{Rect, Region, Subsurface, Surface};

impl Client {
    /// `wl_compositor`: `create_surface` and `create_region`.
    pub(super) fn compositor(&mut self, version: u32, opcode: u16, args: &[Arg<'_>]) {
        let Some(id) = args.first().and_then(Arg::as_object) else {
            return;
        };
        match opcode {
            wl_compositor::request::CREATE_SURFACE => {
                // A surface inherits its `wl_compositor`'s version: the
                // protocol's rule for every object made by another, and the
                // reason a client bound at 4 is never sent
                // `preferred_buffer_scale`, which arrived in 6.
                if self.make(id, &core::WL_SURFACE, version, Role::Surface) {
                    let _ = self.surfaces.insert(id, Surface::new());
                }
            }
            // A wl_region is version 1 whatever its compositor was bound
            // at, because the interface has only ever had one.
            wl_compositor::request::CREATE_REGION
                if self.make(id, &core::WL_REGION, 1, Role::Region) =>
            {
                let _ = self.regions.insert(id, Region::new());
            }
            _ => {}
        }
    }

    /// `wl_surface`: everything a client says about what it is drawing.
    pub(super) fn surface_request(&mut self, sender: ObjectId, opcode: u16, args: &[Arg<'_>]) {
        match opcode {
            wl_surface::request::ATTACH => {
                let Some(buffer) = args.first().and_then(Arg::as_object) else {
                    return;
                };
                if !buffer.is_null() && !self.buffers.contains_key(&buffer) {
                    self.fail(Fatal::WrongInterface {
                        object: buffer,
                        wanted: "wl_buffer",
                    });
                    return;
                }
                let (x, y) = (
                    args.get(1).and_then(Arg::as_int).unwrap_or(0),
                    args.get(2).and_then(Arg::as_int).unwrap_or(0),
                );
                let Some(surface) = self.surfaces.get_mut(&sender) else {
                    return;
                };
                surface.pending.buffer = (!buffer.is_null()).then_some(buffer);
                if let Some(orphan) = self.orphans.get_mut(&sender) {
                    orphan.replaced = true;
                }
                // Before version 5 `attach` carries the offset; from 5 it
                // must be zero and `offset` carries it. A client bound below
                // 5 that sends one is obeyed.
                if x != 0 || y != 0 {
                    surface.pending.offset = (x, y);
                }
            }
            wl_surface::request::DAMAGE | wl_surface::request::DAMAGE_BUFFER => {
                let rect = Rect::new(
                    args.first().and_then(Arg::as_int).unwrap_or(0),
                    args.get(1).and_then(Arg::as_int).unwrap_or(0),
                    args.get(2).and_then(Arg::as_int).unwrap_or(0),
                    args.get(3).and_then(Arg::as_int).unwrap_or(0),
                );
                let Some(surface) = self.surfaces.get_mut(&sender) else {
                    return;
                };
                if let Some(rect) = rect {
                    if opcode == wl_surface::request::DAMAGE {
                        surface.pending.damage.push(rect);
                    } else {
                        surface.pending.buffer_damage.push(rect);
                    }
                }
            }
            wl_surface::request::FRAME => {
                let Some(id) = args.first().and_then(Arg::as_object) else {
                    return;
                };
                if !self.make(id, &core::WL_CALLBACK, 1, Role::FrameCallback) {
                    return;
                }
                if let Some(surface) = self.surfaces.get_mut(&sender) {
                    surface.frame_callbacks.push(id);
                }
            }
            wl_surface::request::SET_OPAQUE_REGION | wl_surface::request::SET_INPUT_REGION => {
                let Some(id) = args.first().and_then(Arg::as_object) else {
                    return;
                };
                let region = if id.is_null() {
                    None
                } else {
                    match self.regions.get(&id) {
                        Some(region) => Some(region.clone()),
                        None => {
                            self.fail(Fatal::WrongInterface {
                                object: id,
                                wanted: "wl_region",
                            });
                            return;
                        }
                    }
                };
                let Some(surface) = self.surfaces.get_mut(&sender) else {
                    return;
                };
                if opcode == wl_surface::request::SET_OPAQUE_REGION {
                    surface.pending.opaque = region;
                } else {
                    surface.pending.input = region;
                }
            }
            wl_surface::request::COMMIT => self.surface_commit(sender),
            wl_surface::request::SET_BUFFER_TRANSFORM => {
                let Some(transform) = args.first().and_then(Arg::as_int) else {
                    return;
                };
                // wl_surface.error.invalid_transform is the protocol's answer
                // to one that is not a wl_output.transform value.
                let Ok(transform) = u32::try_from(transform) else {
                    self.fail(Fatal::Interface {
                        object: sender,
                        code: wl_surface::error::INVALID_TRANSFORM,
                        text: "a buffer transform that is not one".to_owned(),
                    });
                    return;
                };
                if transform > wl_output::transform::FLIPPED_270 {
                    self.fail(Fatal::Interface {
                        object: sender,
                        code: wl_surface::error::INVALID_TRANSFORM,
                        text: format!("{transform} is not a wl_output transform"),
                    });
                    return;
                }
                if let Some(surface) = self.surfaces.get_mut(&sender) {
                    surface.pending.transform = transform;
                }
            }
            wl_surface::request::SET_BUFFER_SCALE => {
                let Some(scale) = args.first().and_then(Arg::as_int) else {
                    return;
                };
                if scale < 1 {
                    self.fail(Fatal::Interface {
                        object: sender,
                        code: wl_surface::error::INVALID_SCALE,
                        text: format!("a buffer scale of {scale}"),
                    });
                    return;
                }
                if let Some(surface) = self.surfaces.get_mut(&sender) {
                    surface.pending.scale = scale;
                }
            }
            wl_surface::request::OFFSET => {
                let (Some(x), Some(y)) = (
                    args.first().and_then(Arg::as_int),
                    args.get(1).and_then(Arg::as_int),
                ) else {
                    return;
                };
                if let Some(surface) = self.surfaces.get_mut(&sender) {
                    surface.pending.offset = (x, y);
                }
            }
            _ => {}
        }
    }

    /// `wl_surface.commit`: the pending state becomes the current one.
    fn surface_commit(&mut self, sender: ObjectId) {
        // A surface that has been given an `xdg_surface` may not
        // carry a buffer until it has acked a configure. That is the
        // rule that stops a client painting at a size the compositor
        // never agreed to: `xdg_surface`'s description has the
        // client commit once with nothing attached, take the
        // configure, ack it, and only then attach.
        let wants_buffer = self
            .surfaces
            .get(&sender)
            .is_some_and(|state| state.pending.buffer.is_some());
        let unconfigured = self
            .xdg_surfaces
            .iter()
            .find(|(_, xdg)| xdg.surface == sender)
            .filter(|(_, xdg)| !xdg.configured)
            .map(|(id, _)| *id);
        if let Some(xdg) = unconfigured
            && wants_buffer
        {
            self.fail(Fatal::Interface {
                object: xdg,
                code: xdg_surface::error::UNCONFIGURED_BUFFER,
                text: "a buffer was attached before a configure was acked".to_owned(),
            });
            return;
        }
        let Some(surface) = self.surfaces.get_mut(&sender) else {
            return;
        };
        let mut change = surface.commit();
        // What a destroyed buffer showed is shown until an attach replaces
        // it, and a buffer the client destroyed is never released: its id
        // may be another object's by now.
        if let Some(orphan) = self.orphans.get(&sender).copied() {
            if change.released == Some(orphan.id) {
                change.released = None;
            }
            if orphan.replaced {
                self.drop_orphan(sender);
            }
        }
        self.events.push(Event::SurfaceCommitted {
            surface: sender,
            change,
        });
    }

    /// `wl_region`: `add` and `subtract`.
    pub(super) fn region_request(&mut self, sender: ObjectId, opcode: u16, args: &[Arg<'_>]) {
        let rect = Rect::new(
            args.first().and_then(Arg::as_int).unwrap_or(0),
            args.get(1).and_then(Arg::as_int).unwrap_or(0),
            args.get(2).and_then(Arg::as_int).unwrap_or(0),
            args.get(3).and_then(Arg::as_int).unwrap_or(0),
        );
        let Some(region) = self.regions.get_mut(&sender) else {
            return;
        };
        let Some(rect) = rect else {
            return;
        };
        match opcode {
            wl_region::request::ADD => region.add(rect),
            wl_region::request::SUBTRACT => region.subtract(rect),
            _ => {}
        }
    }

    /// `wl_subcompositor`: `get_subsurface`.
    ///
    /// A subsurface is a surface placed relative to another and committed
    /// with it. Every toolkit makes them -- for a title bar, a shadow, a
    /// cursor -- so a compositor that advertises `wl_subcompositor` and does
    /// not answer this refuses every such client at its first window.
    pub(super) fn subcompositor(&mut self, opcode: u16, args: &[Arg<'_>]) {
        if opcode != wl_subcompositor::request::GET_SUBSURFACE {
            return;
        }
        let (Some(id), Some(surface), Some(parent)) = (
            args.first().and_then(Arg::as_object),
            args.get(1).and_then(Arg::as_object),
            args.get(2).and_then(Arg::as_object),
        ) else {
            return;
        };
        for (object, name) in [(surface, "wl_surface"), (parent, "wl_surface")] {
            if !self.surfaces.contains_key(&object) {
                self.fail(Fatal::WrongInterface {
                    object,
                    wanted: name,
                });
                return;
            }
        }
        if surface == parent {
            self.fail(Fatal::Interface {
                object: id,
                code: wl_subcompositor::error::BAD_SURFACE,
                text: "a surface cannot be its own parent".to_owned(),
            });
            return;
        }
        // A surface that already has a role may not be given another, as for
        // `xdg_surface`.
        if self.subsurfaces.values().any(|sub| sub.surface == surface)
            || self.xdg_surfaces.values().any(|xdg| xdg.surface == surface)
        {
            self.fail(Fatal::Interface {
                object: id,
                code: wl_subcompositor::error::BAD_SURFACE,
                text: "that surface already has a role".to_owned(),
            });
            return;
        }
        if self.make(id, &core::WL_SUBSURFACE, 1, Role::Subsurface) {
            let _ = self.subsurfaces.insert(
                id,
                Subsurface {
                    surface,
                    parent,
                    position: (0, 0),
                    synchronised: true,
                },
            );
        }
    }

    /// `wl_subsurface`: where it sits and how it commits.
    ///
    /// The position is kept and the stacking is not: a subsurface is drawn
    /// with its parent, and this compositor draws a window's own surface
    /// only, so `place_above` and `place_below` change nothing yet. They are
    /// taken rather than refused, since the protocol allows them.
    pub(super) fn subsurface_request(&mut self, sender: ObjectId, opcode: u16, args: &[Arg<'_>]) {
        let Some(sub) = self.subsurfaces.get_mut(&sender) else {
            return;
        };
        match opcode {
            wl_subsurface::request::SET_POSITION => {
                let (Some(x), Some(y)) = (
                    args.first().and_then(Arg::as_int),
                    args.get(1).and_then(Arg::as_int),
                ) else {
                    return;
                };
                sub.position = (x, y);
            }
            wl_subsurface::request::SET_SYNC => sub.synchronised = true,
            wl_subsurface::request::SET_DESYNC => sub.synchronised = false,
            _ => {}
        }
    }

    /// `wp_viewporter`: `get_viewport`.
    pub(super) fn viewporter(&mut self, version: u32, opcode: u16, args: &[Arg<'_>]) {
        if opcode != wp_viewporter::request::GET_VIEWPORT {
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
        if self.viewports.values().any(|held| *held == surface) {
            self.fail(Fatal::Interface {
                object: surface,
                code: wp_viewporter::error::VIEWPORT_EXISTS,
                text: "that surface already has a viewport".to_owned(),
            });
            return;
        }
        if self.make(id, &viewporter::WP_VIEWPORT, version, Role::Viewport) {
            let _ = self.viewports.insert(id, surface);
        }
    }

    /// `wp_viewport`: `set_source` and `set_destination`.
    ///
    /// The crop and the scale a surface's buffer is drawn with. Both are
    /// recorded on the surface and applied where the surface's size is
    /// worked out, which is the one place a buffer's size and a window's
    /// stop being the same number.
    pub(super) fn viewport(&mut self, sender: ObjectId, opcode: u16, args: &[Arg<'_>]) {
        let Some(surface) = self.viewports.get(&sender).copied() else {
            return;
        };
        match opcode {
            wp_viewport::request::SET_SOURCE => {
                let numbers: Vec<Fixed> = args.iter().filter_map(Arg::as_fixed).collect();
                let [x, y, width, height] = numbers.as_slice() else {
                    return;
                };
                if let Some(state) = self.surfaces.get_mut(&surface) {
                    state.pending.viewport_source = (width.to_f64() > 0.0)
                        .then(|| (x.to_f64(), y.to_f64(), width.to_f64(), height.to_f64()));
                }
            }
            wp_viewport::request::SET_DESTINATION => {
                let numbers: Vec<i32> = args.iter().filter_map(Arg::as_int).collect();
                let [width, height] = numbers.as_slice() else {
                    return;
                };
                if *width <= 0 && *width != -1 {
                    self.fail(Fatal::Interface {
                        object: sender,
                        code: wp_viewport::error::BAD_VALUE,
                        text: format!("a destination of {width}x{height}"),
                    });
                    return;
                }
                if let Some(state) = self.surfaces.get_mut(&surface) {
                    state.pending.viewport_size = (*width > 0).then_some((*width, *height));
                }
            }
            wp_viewport::request::DESTROY => {
                let _ = self.viewports.remove(&sender);
            }
            _ => {}
        }
    }

    /// `wp_fractional_scale_manager_v1`: `get_fractional_scale`.
    ///
    /// The scale is sent at once, as the protocol allows: this compositor's
    /// monitor scales are whole numbers, so the preferred scale is that
    /// number in the protocol's 120ths and a client that asked is told
    /// rather than left waiting.
    pub(super) fn fractional_manager(&mut self, version: u32, opcode: u16, args: &[Arg<'_>]) {
        if opcode != wp_fractional_scale_manager_v1::request::GET_FRACTIONAL_SCALE {
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
            &fractional_scale::WP_FRACTIONAL_SCALE_V1,
            version,
            Role::FractionalScale,
        ) {
            return;
        }
        let _ = self.fractional.insert(id, surface);
        let scale = self.outputs.first().map_or(1, |output| output.scale.max(1));
        let _ = self.out.write(
            id,
            wp_fractional_scale_v1::event::PREFERRED_SCALE,
            &[ArgType::Uint],
            // 120ths, which is the protocol's own unit.
            &[Arg::Uint(
                u32::try_from(scale).unwrap_or(1).saturating_mul(120),
            )],
        );
    }

    /// The surface `id` names, if it is one.
    #[must_use]
    pub fn surface(&self, id: ObjectId) -> Option<&Surface> {
        self.surfaces.get(&id)
    }

    /// The surface `id` names, to be changed by the compositor above.
    pub fn surface_mut(&mut self, id: ObjectId) -> Option<&mut Surface> {
        self.surfaces.get_mut(&id)
    }

    /// Every surface, in id order.
    pub fn surfaces(&self) -> impl Iterator<Item = (ObjectId, &Surface)> {
        self.surfaces.iter().map(|(id, surface)| (*id, surface))
    }

    /// The region `id` names, if it is one.
    #[must_use]
    pub fn region(&self, id: ObjectId) -> Option<&Region> {
        self.regions.get(&id)
    }

    /// The subsurface `id` names, if it is one.
    #[must_use]
    pub fn subsurface(&self, id: ObjectId) -> Option<&Subsurface> {
        self.subsurfaces.get(&id)
    }

    /// Fire a surface's frame callbacks with `time`, and take them.
    ///
    /// `wl_callback.done`'s argument is milliseconds with an undefined base,
    /// which is what every client treats it as.
    pub fn fire_frame_callbacks(&mut self, surface: ObjectId, time: u32) {
        let Some(state) = self.surfaces.get_mut(&surface) else {
            return;
        };
        for callback in state.take_frame_callbacks() {
            let _ = self.out.write(
                callback,
                core::wl_callback::event::DONE,
                &[ArgType::Uint],
                &[Arg::Uint(time)],
            );
            self.destroy(callback, Role::FrameCallback);
        }
    }
}
