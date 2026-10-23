//! `wlr-output-management-unstable-v1`: the screens as a program arranges
//! them.
//!
//! `kanshi` and `wlr-randr` speak this, and so does every settings panel on
//! a wlroots compositor: it publishes every screen with its modes, its
//! position, its scale and whether it is on, and takes a whole arrangement
//! back as one atomic configuration. Hyprland answers it too, which is why
//! `wlr-randr` works there.
//!
//! # The serial is the whole safety rule
//!
//! The manager publishes a serial with each `done`; a configuration is made
//! against a serial, and one made against a stale serial is `cancelled`
//! rather than applied. That is what stops a program that read the screens a
//! second ago from moving a monitor that has since been unplugged, and it is
//! the one part of this protocol a compositor must not skip.

use compositor_protocol::output_management;
use compositor_protocol::output_management::{
    zwlr_output_configuration_head_v1, zwlr_output_configuration_v1, zwlr_output_head_v1,
    zwlr_output_manager_v1, zwlr_output_mode_v1,
};
use compositor_wire::{Arg, ArgType, Fixed, ObjectId};

use crate::client::{Client, Event};
use crate::role::Role;
use crate::surface::Output;

/// The version the server makes a head and a mode at.
const HEAD_VERSION: u32 = 4;

/// What a configuration asks one screen to become.
///
/// Every field but `on` is `None` for "leave it as it is", which is what a
/// configuration that enabled a head and said nothing else means.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Wanted {
    /// Whether the screen is on at all.
    pub on: bool,
    /// Where it goes, in logical pixels.
    pub at: Option<(i32, i32)>,
    /// The mode's size.
    pub size: Option<(i32, i32)>,
    /// The mode's refresh, in millihertz.
    pub refresh: Option<i32>,
    /// How many buffer pixels one logical pixel is, in the protocol's own
    /// fixed point so that what the compositor acts on is the number the
    /// client sent.
    pub scale: Option<Fixed>,
    /// A `wl_output.transform`.
    pub transform: Option<i32>,
}

/// One configuration a client is building.
#[derive(Clone, Debug, Default)]
pub struct Configuration {
    /// The serial it was made against, which has to be the current one.
    pub serial: u32,
    /// What it says about each screen, by its place in the outputs.
    pub heads: Vec<(usize, Wanted)>,
    /// Whether it has been applied or tested already: the protocol allows
    /// one of either.
    pub used: bool,
}

impl Client {
    /// Answer a request to one of this module's objects.
    pub(super) fn outputs(
        &mut self,
        sender: ObjectId,
        role: Role,
        version: u32,
        opcode: u16,
        args: &[Arg<'_>],
    ) -> bool {
        match role {
            Role::OutputManager => self.output_manager(sender, version, opcode, args),
            Role::OutputConfiguration => self.configuration(sender, version, opcode, args),
            Role::OutputConfigurationHead => self.configuration_head(sender, opcode, args),
            // A head and a mode have only `release`, which the destructor
            // flag takes.
            Role::OutputHead | Role::OutputMode => {}
            _ => return false,
        }
        true
    }

    /// Drop what one of this module's objects held.
    pub(super) fn forget_outputs(&mut self, id: ObjectId, role: Role) {
        match role {
            Role::OutputManager => self.output_managers.retain(|held| *held != id),
            Role::OutputConfiguration => {
                let _ = self.configurations.remove(&id);
            }
            Role::OutputConfigurationHead => {
                let _ = self.configuration_heads.remove(&id);
            }
            Role::OutputHead => self.heads.retain(|_, held| *held != id),
            _ => {}
        }
    }

    /// `zwlr_output_manager_v1`: `create_configuration` and `stop`.
    fn output_manager(&mut self, sender: ObjectId, version: u32, opcode: u16, args: &[Arg<'_>]) {
        match opcode {
            zwlr_output_manager_v1::request::CREATE_CONFIGURATION => {
                let (Some(id), Some(serial)) = (
                    args.first().and_then(Arg::as_object),
                    args.get(1).and_then(Arg::as_uint),
                ) else {
                    return;
                };
                if !self.make(
                    id,
                    &output_management::ZWLR_OUTPUT_CONFIGURATION_V1,
                    version,
                    Role::OutputConfiguration,
                ) {
                    return;
                }
                let _ = self.configurations.insert(
                    id,
                    Configuration {
                        serial,
                        heads: Vec::new(),
                        used: false,
                    },
                );
            }
            zwlr_output_manager_v1::request::STOP => {
                self.output_managers.retain(|held| *held != sender);
                let _ = self
                    .out
                    .write(sender, zwlr_output_manager_v1::event::FINISHED, &[], &[]);
            }
            _ => {}
        }
    }

    /// `zwlr_output_configuration_v1`: the four requests that build an
    /// arrangement and the two that ask for it.
    fn configuration(&mut self, sender: ObjectId, version: u32, opcode: u16, args: &[Arg<'_>]) {
        match opcode {
            zwlr_output_configuration_v1::request::ENABLE_HEAD => {
                let (Some(id), Some(head)) = (
                    args.first().and_then(Arg::as_object),
                    args.get(1).and_then(Arg::as_object),
                ) else {
                    return;
                };
                let Some(which) = self.head_of(head) else {
                    return;
                };
                if !self.make(
                    id,
                    &output_management::ZWLR_OUTPUT_CONFIGURATION_HEAD_V1,
                    version,
                    Role::OutputConfigurationHead,
                ) {
                    return;
                }
                let _ = self.configuration_heads.insert(id, (sender, which));
                if let Some(held) = self.configurations.get_mut(&sender) {
                    held.heads.push((
                        which,
                        Wanted {
                            on: true,
                            ..Wanted::default()
                        },
                    ));
                }
            }
            zwlr_output_configuration_v1::request::DISABLE_HEAD => {
                let Some(head) = args.first().and_then(Arg::as_object) else {
                    return;
                };
                let Some(which) = self.head_of(head) else {
                    return;
                };
                if let Some(held) = self.configurations.get_mut(&sender) {
                    held.heads.push((
                        which,
                        Wanted {
                            on: false,
                            ..Wanted::default()
                        },
                    ));
                }
            }
            zwlr_output_configuration_v1::request::APPLY
            | zwlr_output_configuration_v1::request::TEST => {
                let testing = opcode == zwlr_output_configuration_v1::request::TEST;
                let Some(held) = self.configurations.get_mut(&sender) else {
                    return;
                };
                if held.used {
                    return;
                }
                held.used = true;
                let (serial, heads) = (held.serial, held.heads.clone());
                if serial != self.output_serial {
                    // Made against screens that have since changed: the
                    // protocol's own answer is `cancelled`, and it is the
                    // one thing this protocol must not skip.
                    let _ = self.out.write(
                        sender,
                        zwlr_output_configuration_v1::event::CANCELLED,
                        &[],
                        &[],
                    );
                    return;
                }
                self.events.push(Event::OutputConfigured {
                    configuration: sender,
                    testing,
                    heads,
                });
            }
            _ => {}
        }
    }

    /// `zwlr_output_configuration_head_v1`: what one screen is to become.
    fn configuration_head(&mut self, sender: ObjectId, opcode: u16, args: &[Arg<'_>]) {
        let Some((configuration, which)) = self.configuration_heads.get(&sender).copied() else {
            return;
        };
        let Some(held) = self.configurations.get_mut(&configuration) else {
            return;
        };
        let Some((_, wanted)) = held.heads.iter_mut().find(|(head, _)| *head == which) else {
            return;
        };
        match opcode {
            zwlr_output_configuration_head_v1::request::SET_CUSTOM_MODE => {
                let numbers: Vec<i32> = args.iter().filter_map(Arg::as_int).collect();
                if let [width, height, refresh] = numbers.as_slice() {
                    wanted.size = Some((*width, *height));
                    wanted.refresh = Some(*refresh);
                }
            }
            zwlr_output_configuration_head_v1::request::SET_POSITION => {
                let numbers: Vec<i32> = args.iter().filter_map(Arg::as_int).collect();
                if let [x, y] = numbers.as_slice() {
                    wanted.at = Some((*x, *y));
                }
            }
            zwlr_output_configuration_head_v1::request::SET_SCALE => {
                if let Some(scale) = args.first().and_then(Arg::as_fixed) {
                    wanted.scale = Some(scale);
                }
            }
            zwlr_output_configuration_head_v1::request::SET_TRANSFORM => {
                if let Some(transform) = args.first().and_then(Arg::as_int) {
                    wanted.transform = Some(transform);
                }
            }
            // `set_mode` names a mode object this compositor published:
            // its size and refresh are what is asked for.
            zwlr_output_configuration_head_v1::request::SET_MODE => {
                if let Some(mode) = args.first().and_then(Arg::as_object)
                    && let Some(&(width, height, refresh)) = self.output_modes.get(&mode)
                {
                    wanted.size = Some((width, height));
                    wanted.refresh = Some(refresh);
                }
            }
            // `set_adaptive_sync` is the GPU path's.
            _ => {}
        }
    }

    /// Which screen a head names.
    fn head_of(&self, head: ObjectId) -> Option<usize> {
        self.heads
            .iter()
            .find(|(_, held)| **held == head)
            .map(|(which, _)| *which)
    }

    /// Tell a configuration whether it was carried out.
    pub fn output_configured(&mut self, configuration: ObjectId, done: bool) {
        let event = if done {
            zwlr_output_configuration_v1::event::SUCCEEDED
        } else {
            zwlr_output_configuration_v1::event::FAILED
        };
        let _ = self.out.write(configuration, event, &[], &[]);
    }

    /// Publish every screen to every `zwlr_output_manager_v1`.
    ///
    /// Called whenever the screens change and once when a manager binds. The
    /// serial goes up each time, and a configuration made against an older
    /// one is refused.
    pub fn publish_outputs(&mut self, outputs: &[Output]) {
        if self.output_managers.is_empty() {
            return;
        }
        self.output_serial = self.output_serial.wrapping_add(1);
        // Heads from the last round are finished: this protocol has no way
        // to change a head's mode list, so a fresh set is published.
        let old: Vec<ObjectId> = self.heads.values().copied().collect();
        for head in old {
            let _ = self
                .out
                .write(head, zwlr_output_head_v1::event::FINISHED, &[], &[]);
            let _ = self.objects.remove(head);
        }
        self.heads.clear();
        self.output_modes.clear();
        for (which, output) in outputs.iter().enumerate() {
            self.publish_head(which, output);
        }
        let managers = self.output_managers.clone();
        for manager in managers {
            let _ = self.out.write(
                manager,
                zwlr_output_manager_v1::event::DONE,
                &[ArgType::Uint],
                &[Arg::Uint(self.output_serial)],
            );
        }
    }

    /// One screen, with its one mode.
    fn publish_head(&mut self, which: usize, output: &Output) {
        let managers = self.output_managers.clone();
        let Ok(head) = self.objects.create(
            &output_management::ZWLR_OUTPUT_HEAD_V1,
            HEAD_VERSION,
            Role::OutputHead,
        ) else {
            return;
        };
        for manager in &managers {
            let _ = self.out.write(
                *manager,
                zwlr_output_manager_v1::event::HEAD,
                &[ArgType::NewId],
                &[Arg::NewId(head)],
            );
        }
        // The description a monitor gives of itself, which is what every
        // other protocol here carries and what a person's configuration
        // names it by.
        for (event, text) in [
            (zwlr_output_head_v1::event::NAME, output.name.clone()),
            (
                zwlr_output_head_v1::event::DESCRIPTION,
                output.description.clone(),
            ),
            (zwlr_output_head_v1::event::MAKE, "Ferrix".to_owned()),
            (zwlr_output_head_v1::event::MODEL, "hyprix".to_owned()),
            (
                zwlr_output_head_v1::event::SERIAL_NUMBER,
                output.name.clone(),
            ),
        ] {
            let _ = self.out.write(
                head,
                event,
                &[ArgType::Str { nullable: false }],
                &[Arg::Str(Some(&text))],
            );
        }
        // Every mode the screen lists, the running one current; a screen
        // that lists none has its running one alone.
        let running = (output.width, output.height, output.refresh);
        let listed: Vec<(i32, i32, i32)> = if output.modes.is_empty() {
            vec![running]
        } else {
            output.modes.clone()
        };
        let mut mode = None;
        for (index, &one) in listed.iter().enumerate() {
            let made = self.publish_mode(head, one, index == 0);
            if one == running || (mode.is_none() && one.0 == running.0 && one.1 == running.1) {
                mode = made.or(mode);
            }
        }
        let _ = self.out.write(
            head,
            zwlr_output_head_v1::event::ENABLED,
            &[ArgType::Int],
            &[Arg::Int(1)],
        );
        if let Some(mode) = mode {
            let _ = self.out.write(
                head,
                zwlr_output_head_v1::event::CURRENT_MODE,
                &[ArgType::Object { nullable: false }],
                &[Arg::Object(mode)],
            );
        }
        let _ = self.out.write(
            head,
            zwlr_output_head_v1::event::POSITION,
            &[ArgType::Int, ArgType::Int],
            &[Arg::Int(output.x), Arg::Int(output.y)],
        );
        let _ = self.out.write(
            head,
            zwlr_output_head_v1::event::TRANSFORM,
            &[ArgType::Int],
            &[Arg::Int(output.transform)],
        );
        let _ = self.out.write(
            head,
            zwlr_output_head_v1::event::SCALE,
            &[ArgType::Fixed],
            &[Arg::Fixed(Fixed::from_f64(f64::from(output.scale.max(1))))],
        );
        let _ = self.heads.insert(which, head);
    }

    /// One mode of a screen, `preferred` for the first it lists.
    fn publish_mode(
        &mut self,
        head: ObjectId,
        (width, height, refresh): (i32, i32, i32),
        preferred: bool,
    ) -> Option<ObjectId> {
        // At the interface's own version, not the head's: a mode stops at
        // 3 where a head goes to 4, and an object made above what its
        // interface offers is no object at all.
        let mode = self
            .objects
            .create(
                &output_management::ZWLR_OUTPUT_MODE_V1,
                HEAD_VERSION.min(output_management::ZWLR_OUTPUT_MODE_V1.version),
                Role::OutputMode,
            )
            .ok()?;
        let _ = self.out.write(
            head,
            zwlr_output_head_v1::event::MODE,
            &[ArgType::NewId],
            &[Arg::NewId(mode)],
        );
        let _ = self.out.write(
            mode,
            zwlr_output_mode_v1::event::SIZE,
            &[ArgType::Int, ArgType::Int],
            &[Arg::Int(width), Arg::Int(height)],
        );
        let _ = self.out.write(
            mode,
            zwlr_output_mode_v1::event::REFRESH,
            &[ArgType::Int],
            &[Arg::Int(refresh)],
        );
        if preferred {
            let _ = self
                .out
                .write(mode, zwlr_output_mode_v1::event::PREFERRED, &[], &[]);
        }
        let _ = self.output_modes.insert(mode, (width, height, refresh));
        Some(mode)
    }
}
