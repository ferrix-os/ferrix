//! `wl_output`: what each screen is, as every client reads it.
//!
//! One global to a monitor, and one description each time a client binds
//! it: where the screen is, its mode, its scale and its name, closed by
//! `done`. The compositor above says what the screens are with
//! [`Client::set_outputs`]. The arrangement a settings program asks for is
//! in `outputs.rs`, and the logical size a bar reads, `xdg-output`, is in
//! `desktop.rs`.

use compositor_protocol::core::{self, wl_output, wl_registry};
use compositor_wire::{Arg, ArgType, ObjectId};

use crate::client::Client;
use crate::role::Role;
use crate::surface::Output;

/// The `wl_output` version every monitor is advertised at, as hyprix's own
/// list has it.
pub(crate) const OUTPUT_VERSION: u32 = 4;

impl Client {
    /// Say what the screen is, before any client binds `wl_output`.
    pub fn set_output(&mut self, output: Output) {
        self.outputs = vec![output];
    }

    /// Say what every screen is, in the order the `wl_output` globals were
    /// added: the first global describes the first monitor and so on, which
    /// is how a client tells two screens apart.
    pub fn set_outputs(&mut self, outputs: Vec<Output>) {
        if !outputs.is_empty() {
            self.outputs = outputs;
        }
    }

    /// A monitor was plugged in: a `wl_output` global more, after the
    /// others, which every registry is told of. Its place is the last of
    /// the list the next [`Client::set_outputs`] gives (`docs/DESKTOP.md`
    /// §3.2).
    pub fn output_added(&mut self) {
        let Some(name) = self
            .globals
            .add(&core::WL_OUTPUT, OUTPUT_VERSION, Role::Output)
        else {
            return;
        };
        for registry in self.registries.clone() {
            let _ = self.out.write(
                registry,
                wl_registry::event::GLOBAL,
                &[
                    ArgType::Uint,
                    ArgType::Str { nullable: false },
                    ArgType::Uint,
                ],
                &[
                    Arg::Uint(name),
                    Arg::Str(Some(core::WL_OUTPUT.name)),
                    Arg::Uint(OUTPUT_VERSION),
                ],
            );
        }
    }

    /// A monitor was unplugged: the `wl_output` global at place `which` is
    /// removed and every registry told. Objects bound from it name no
    /// monitor any more, as the protocol leaves them inert; the ones of the
    /// monitors after it move down a place, as the list
    /// [`Client::set_outputs`] is given next does.
    pub fn output_removed(&mut self, which: usize) {
        let Some(name) = self
            .globals
            .all()
            .iter()
            .filter(|global| global.role == Role::Output)
            .nth(which)
            .map(|global| global.name)
        else {
            return;
        };
        let _ = self.globals.remove(name);
        for registry in self.registries.clone() {
            let _ = self.out.write(
                registry,
                wl_registry::event::GLOBAL_REMOVE,
                &[ArgType::Uint],
                &[Arg::Uint(name)],
            );
        }
        // Everything kept by a monitor's place: what named this one names
        // nothing now, and the places after it move down.
        for places in [
            &mut self.output_objects,
            &mut self.xdg_outputs,
            &mut self.lock_surfaces,
            &mut self.gammas,
            &mut self.powers,
        ] {
            places.retain(|_, at| *at != which);
            for at in places.values_mut() {
                if *at > which {
                    *at -= 1;
                }
            }
        }
        if which < self.outputs.len() {
            let _ = self.outputs.remove(which);
        }
    }

    /// The monitor a bound `wl_output` object names, by its place in the
    /// list [`Client::set_outputs`] was given.
    #[must_use]
    pub fn output_of(&self, object: ObjectId) -> Option<usize> {
        self.output_objects.get(&object).copied()
    }

    /// Tell a fresh `wl_output` what the screen is.
    ///
    /// Every client reads these: a toolkit with no mode has no size to scale
    /// against, and foot reports `(null): 0x0+0x0@0Hz` for an output that
    /// sent none. The `done` at the end is what says the description is
    /// whole, and a client waits for it.
    pub(super) fn describe_output(&mut self, id: ObjectId, version: u32, which: usize) {
        let Some(mode) = self.outputs.get(which).cloned() else {
            return;
        };
        let _ = self.out.write(
            id,
            wl_output::event::GEOMETRY,
            &[
                ArgType::Int,
                ArgType::Int,
                ArgType::Int,
                ArgType::Int,
                ArgType::Int,
                ArgType::Str { nullable: false },
                ArgType::Str { nullable: false },
                ArgType::Int,
            ],
            &[
                Arg::Int(mode.x),
                Arg::Int(mode.y),
                // A size in millimetres. Nothing here has a physical screen,
                // and a zero is what every headless compositor sends: a
                // client reads it as "unknown" and uses the scale instead.
                Arg::Int(0),
                Arg::Int(0),
                Arg::Int(wl_output::subpixel::UNKNOWN.cast_signed()),
                Arg::Str(Some("Ferrix")),
                Arg::Str(Some("hyprix")),
                // How the monitor is turned, which Hyprland sends as its
                // `m_transform`: the transform that makes the buffer the
                // picture a person reads.
                Arg::Int(mode.transform),
            ],
        );
        let _ = self.out.write(
            id,
            wl_output::event::MODE,
            &[ArgType::Uint, ArgType::Int, ArgType::Int, ArgType::Int],
            &[
                Arg::Uint(wl_output::mode::CURRENT | wl_output::mode::PREFERRED),
                Arg::Int(mode.width),
                Arg::Int(mode.height),
                // Millihertz, as the protocol counts it.
                Arg::Int(mode.refresh),
            ],
        );
        if version >= 2 {
            let _ = self.out.write(
                id,
                wl_output::event::SCALE,
                &[ArgType::Int],
                &[Arg::Int(mode.scale)],
            );
        }
        if version >= 4 {
            for (opcode, text) in [
                (wl_output::event::NAME, mode.name.as_str()),
                (wl_output::event::DESCRIPTION, mode.description.as_str()),
            ] {
                let _ = self.out.write(
                    id,
                    opcode,
                    &[ArgType::Str { nullable: false }],
                    &[Arg::Str(Some(text))],
                );
            }
        }
        if version >= 2 {
            let _ = self.out.write(id, wl_output::event::DONE, &[], &[]);
        }
    }
}
