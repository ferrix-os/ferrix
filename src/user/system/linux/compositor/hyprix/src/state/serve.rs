//! Serving one client: reading what it sent, and carrying out what that
//! asked for once the client's own borrow is over.
//!
//! A client's events are read while its slot is borrowed, and most of what
//! they ask for reaches past that slot: another client, the layout, the
//! clipboard, the screens. So reading fills `Effects`, and applying it
//! comes after, in a fixed order.

use std::collections::BTreeMap;

use compositor_layout::WindowId;
use compositor_server::{Event, ForeignRequest, Role};
use compositor_socket::RecvError;
use compositor_wire::{Fd, ObjectId};

use crate::clipboard::{Through, Which};
use crate::frame::Source;
use crate::pool::Mapping;

use super::{
    Arrangement, Compositor, Drag, ScreenAsks, Shot, activate, apply_rules, as_input, captured,
    configure, configure_first, exported, fit_dialog, follow_own_size, for_the_bar,
    input_method_turn, lock_changed, named, painted, place_popup, rect_of, release_retired_pools,
    remember_size, showing_from, whole_of,
};

/// What reading one client's messages asked for that reaches past its own
/// connection, kept until the client's own borrow is over.
///
/// Reading holds the client's slot, and every one of these reaches another
/// slot, the layout, the clipboard or the screens. They are carried out in
/// the order `Compositor::apply` lists, which is the order they always
/// were: a client told of the windows before its roundtrip comes back, a
/// window closed before a state it asked for is looked at.
#[derive(Default)]
struct Effects {
    /// Whether the layout changed while the messages were read.
    changed: bool,
    /// How many of the descriptors that arrived the messages took.
    claimed: usize,
    /// The clipboard's, kept until the client's own borrow is over: telling
    /// every client what the selection holds needs them all.
    selection: Vec<(Which, Option<ObjectId>, Vec<String>, Through)>,
    /// The sources the client destroyed, which are no selection after the
    /// ones set in this pass have been taken.
    gone_sources: Vec<ObjectId>,
    /// What the client pasted, and the pipe the data goes down.
    wanted: Vec<(Which, ObjectId, String, Fd, Through)>,
    /// Clipboard managers that have just made a device, which are owed both
    /// selections at once whether or not they have a window.
    watching: Vec<ObjectId>,
    /// Where a client asked the pointer to be put, inside one of its own
    /// surfaces: `wp_pointer_warp_v1`.
    warped: Vec<(
        usize,
        ObjectId,
        (compositor_wire::Fixed, compositor_wire::Fixed),
    )>,
    /// The data devices the client made, which are owed the selection.
    made_device: Vec<Which>,
    /// A cursor shape a client named.
    shaped: Option<u32>,
    /// A window it asked to be raised.
    activating: Option<(String, ObjectId)>,
    /// The input method's two halves, which are two connections: the text
    /// inputs enabled and disabled, the method made, what it typed and
    /// whether it went.
    typing: Vec<(ObjectId, bool)>,
    /// The input method this client has just become.
    became_method: Option<ObjectId>,
    /// What the input method typed.
    was_typed: Vec<compositor_server::Typed>,
    /// Whether the input method went.
    method_gone: bool,
    /// The same, for what a bar asked: acting on a window reaches every
    /// client's slot, and this one's borrow is still open.
    asked: Vec<(WindowId, ForeignRequest)>,
    /// The windows `xdg_dialog_v1` called modal in this pass, which the
    /// layout floats once the client's own borrow is over.
    modals: Vec<(WindowId, bool)>,
    /// The windows whose `xdg_toplevel` the client destroyed in this pass,
    /// taken out of the layout once its borrow is over.
    closed: Vec<WindowId>,
    /// What a window asked `xdg_toplevel` for in this pass: the last
    /// `set_fullscreen`/`set_maximized` state each one is in, acted on once
    /// the client's own borrow is over.
    asked_states: Vec<(WindowId, bool, bool)>,
    /// The windows that asked to be moved (edges 0) or resized by the
    /// pointer in this pass, with the edges pulled.
    asked_drags: Vec<(WindowId, u32)>,
    /// Whether this client has just become a bar, which is owed the windows
    /// there already are before the roundtrip it sent after binding comes
    /// back.
    bound_manager: bool,
    /// The popups made in this pass, which are placed once the client's own
    /// borrow is over: placing one reads the layout and the parent window.
    fresh_popups: Vec<ObjectId>,
    /// The session lock's, for the same reason: the screens are the loop's.
    /// The lock taken.
    locking: Option<ObjectId>,
    /// The lock surfaces made, with the screen each covers.
    covered: Vec<(ObjectId, ObjectId, usize)>,
    /// The lock given up, and whether it was asked for.
    unlocking: Option<(ObjectId, bool)>,
}

impl Compositor<'_> {
    /// Read what one connection sent and act on it.
    ///
    /// Gives whether the layout changed.
    pub(super) fn serve(&mut self, index: usize, asks: &mut ScreenAsks) -> Result<bool, String> {
        let Some(slot) = self.slots.get_mut(index) else {
            return Ok(false);
        };
        match slot.connection.receive() {
            Ok(_) => {}
            Err(RecvError::WouldBlock) => {}
            Err(_) => {
                slot.gone = true;
                return Ok(false);
            }
        }
        let effects = self.read(index, asks)?;
        let changed = self.apply(index, asks, effects);
        self.send(index);
        Ok(changed)
    }

    /// Read the messages that arrived on the connection at `index`, and
    /// the events they make. Gives what they asked for that has to wait
    /// until the client's own borrow is over.
    fn read(&mut self, index: usize, asks: &mut ScreenAsks) -> Result<Effects, String> {
        let mut effects = Effects::default();
        let Some(slot) = self.slots.get_mut(index) else {
            return Ok(effects);
        };
        let arrived = slot.connection.fds();
        let consumed = slot.client.read(slot.connection.bytes(), &arrived);
        if consumed > 0 {
            let events = slot.client.take_events();
            for event in events {
                self.take_event(index, event, asks, &mut effects)?;
            }
            if let Some(slot) = self.slots.get_mut(index) {
                slot.connection.consume(consumed, effects.claimed);
            }
        }
        Ok(effects)
    }

    /// One event of the connection at `index`: done now if it reaches only
    /// that connection, the layout or the screenshots, and kept in
    /// `effects` or `asks` if it reaches further.
    fn take_event(
        &mut self,
        index: usize,
        event: Event,
        asks: &mut ScreenAsks,
        effects: &mut Effects,
    ) -> Result<(), String> {
        let Some(slot) = self.slots.get_mut(index) else {
            return Ok(());
        };
        match event {
            Event::PoolCreated { pool, memory } => {
                effects.claimed += 1;
                match Mapping::new(memory.fd, memory.size) {
                    Ok(mapping) => {
                        let _ = slot.pools.insert(pool, mapping);
                    }
                    Err(_) => {
                        // A descriptor that is not memory: the client
                        // gets nothing drawn, and the protocol has no
                        // error the compositor may send after the fact.
                    }
                }
            }
            Event::PoolResized { pool, size } => {
                let resized = match slot.pools.get_mut(&pool) {
                    Some(mapping) => {
                        let _ = mapping.resize(size);
                        true
                    }
                    None => false,
                };
                // A pool mapped again is memory at another address, so
                // what every surface drawn from it shows may have
                // changed with no commit to say so.
                let showing = if resized {
                    showing_from(&slot.client, pool)
                } else {
                    Vec::new()
                };
                for surface in showing {
                    self.commits.painted(whole_of(index, surface));
                }
            }
            // A window the client closed. Destroying the
            // `xdg_toplevel` is how a client with more than one window
            // shuts one of them: the connection stays open, so the loop
            // that takes a gone client's windows away never runs, and
            // without this the layout would keep tiling a window that
            // no longer exists.
            Event::Destroyed {
                object,
                role: Role::XdgToplevel,
            } => {
                if let Some(at) = slot.windows.iter().position(|(top, _)| *top == object) {
                    let (_, window) = slot.windows.remove(at);
                    let _ = slot.answered.remove(&window);
                    effects.closed.push(window);
                }
            }
            // A dmabuf's descriptor, held until its buffer is made or its
            // parameters go (`docs/GPU.md` §3.13).
            Event::DmabufPlane { params, fd } => {
                effects.claimed += 1;
                let _ = slot.planes.insert(params, crate::pool::own(fd.0));
            }
            Event::Destroyed {
                object,
                role: Role::BufferParams,
            } => {
                let _ = slot.planes.remove(&object);
            }
            // The buffer itself: imported into the compositor's own open
            // of the render node, which is where the kernel says whether
            // it is a buffer of this GPU at all, and the answer told.
            Event::DmabufCreated { dmabuf } => {
                let imported = match (slot.planes.remove(&dmabuf.params), self.dmabuf.as_ref()) {
                    (Some(fd), Some(node)) => crate::dmabuf::Imported::new(node, fd, &dmabuf)
                        .map_err(|error| error.to_string()),
                    (None, _) => Err("no plane was handed over".to_owned()),
                    (_, None) => Err("no render node to import into".to_owned()),
                };
                let ok = match imported {
                    Ok(imported) => {
                        let _ = slot.dmabufs.insert(dmabuf.pool, imported);
                        if !core::mem::replace(&mut self.dmabuf_said, true) {
                            (self.report)(&format!(
                                "hyprix: imported a dmabuf, {}x{}, through zwp_linux_dmabuf_v1",
                                dmabuf.width, dmabuf.height
                            ));
                        }
                        true
                    }
                    Err(why) => {
                        (self.report)(&format!("hyprix: a dmabuf could not be imported: {why}"));
                        false
                    }
                };
                slot.client.dmabuf_imported(dmabuf.params, ok);
            }
            Event::PoolRetired { pool } => {
                let _ = slot.retired.insert(pool);
                if release_retired_pools(slot) {
                    self.commits.everything();
                }
            }
            // A buffer going may be the last one of a pool the client
            // has already destroyed, which is when the memory is
            // finally let go. A surface still showing a buffer that has
            // gone is drawn as its border and background instead, which
            // is a change to the screen no commit announced; every
            // other destroy changes nothing, since a client destroys a
            // buffer it has finished with.
            Event::Destroyed {
                object,
                role: Role::Buffer,
            } => {
                let showing: Vec<ObjectId> = slot
                    .client
                    .surfaces()
                    .filter(|(_, state)| state.current.buffer == Some(object))
                    .map(|(id, _)| id)
                    .collect();
                let _ = release_retired_pools(slot);
                for surface in showing {
                    self.commits.painted(whole_of(index, surface));
                }
            }
            Event::LayerSurfaceCreated { layer_surface, .. } => {
                slot.layers.push(layer_surface);
                effects.changed = true;
            }
            Event::LayerSurfaceChanged { .. } => effects.changed = true,
            // The clipboard. What one client copied is the compositor's
            // to remember and to offer to the others; the data never
            // passes through it.
            // A clipboard manager: it has no window and no keyboard,
            // and is told what both selections hold anyway. That is the
            // whole of what `wlr-data-control` and `ext-data-control`
            // are, and why `cliphist` works.
            Event::DataControlBound { device } => effects.watching.push(device),
            Event::DataControlSelection {
                source,
                primary,
                mimes,
            } => effects.selection.push((
                if primary {
                    Which::Primary
                } else {
                    Which::Clipboard
                },
                source,
                mimes.clone(),
                Through::Manager,
            )),
            Event::DataControlPaste {
                offer,
                mime,
                fd,
                primary,
            } => {
                effects.claimed += 1;
                effects.wanted.push((
                    if primary {
                        Which::Primary
                    } else {
                        Which::Clipboard
                    },
                    offer,
                    mime.clone(),
                    fd,
                    Through::Manager,
                ));
            }
            // A night-light's three ramps, on a descriptor. The
            // compositor reads them here and applies them to the
            // pixels on their way out; `None` is the control going
            // away, which puts the screen back.
            Event::Gamma { output, table } => {
                if table.is_some() {
                    effects.claimed += 1;
                }
                asks.ramps.push((index, output, table));
            }
            // `wlopm` turning a screen off, which is what the `dpms`
            // dispatcher does from a keybind.
            Event::OutputPower { output, on } => asks.powered.push((index, output, on)),
            // `wlr-randr` and `kanshi` arranging the screens.
            Event::OutputConfigured {
                configuration,
                testing,
                heads,
            } => asks.arranged.push((
                index,
                Arrangement {
                    configuration,
                    testing,
                    heads,
                },
            )),
            // The newer screenshot: a session is owed the size of the
            // buffer to make, and a frame is filled in.
            Event::CaptureSession { session, source } => {
                self.shots.extend(captured(
                    session,
                    source,
                    None,
                    &self.state,
                    &self.sources,
                    &self.screens,
                ));
            }
            Event::CaptureAsked {
                frame,
                source,
                buffer,
            } => {
                self.shots.extend(captured(
                    frame,
                    source,
                    Some(buffer),
                    &self.state,
                    &self.sources,
                    &self.screens,
                ));
            }
            // A recorder sharing one window rather than a screen.
            Event::ToplevelExportAsked { frame, window } => {
                self.shots.extend(exported(
                    frame,
                    window,
                    None,
                    &self.state,
                    &self.sources,
                    &self.screens,
                ));
            }
            Event::ToplevelExportCopy {
                frame,
                window,
                buffer,
            } => {
                self.shots.extend(exported(
                    frame,
                    window,
                    Some(buffer),
                    &self.state,
                    &self.sources,
                    &self.screens,
                ));
            }
            // A launcher asking for the focus to stay on its own
            // surfaces until a click lands outside them.
            Event::FocusGrabbed { grab, surfaces } => {
                (self.report)(&format!(
                    "hyprix: a client grabbed the focus onto {} surface(s)",
                    surfaces.len()
                ));
                let _ = grab;
            }
            // A client putting the pointer inside its own window.
            Event::PointerWarped {
                surface,
                at,
                serial,
            } => {
                let _ = serial;
                effects.warped.push((index, surface, at));
            }
            // A sandbox handed over a socket of its own. The compositor
            // has one listener and takes connections on it alone, so
            // the descriptors are closed and the sandbox is named in
            // the log -- which is more than silence and less than a
            // pretence that its clients are being told apart.
            Event::SecurityContext {
                listener,
                close,
                engine,
                app_id,
                instance,
            } => {
                effects.claimed += 2;
                crate::clipboard::close(listener);
                crate::clipboard::close(close);
                (self.report)(&format!(
                    "hyprix: a {engine} sandbox for {app_id} ({instance}) asked for a \
                     socket of its own, which this compositor does not make"
                ));
            }
            // A bar clicking a workspace number.
            Event::WorkspaceAsked { workspace, what } => {
                asks.workspaces.push((workspace, what));
            }
            // The requests are carried out as they arrive, so the end
            // of a batch is nothing to do.
            Event::WorkspacesCommitted => {}
            // Drag and drop: the source began it, and the target
            // answers with what it will take.
            Event::DragStarted {
                source,
                origin,
                icon,
                serial,
                mimes,
            } => {
                let _ = (origin, serial);
                asks.drags.push(Drag::Started {
                    client: index,
                    source,
                    icon,
                    mimes: mimes.clone(),
                });
            }
            Event::DragAccepted { offer, mime } => {
                let _ = offer;
                asks.drags.push(Drag::Accepted { mime: mime.clone() });
            }
            Event::DragActions {
                offer,
                actions,
                preferred,
            } => {
                let _ = offer;
                asks.drags.push(Drag::Actions { actions, preferred });
            }
            Event::DragFinished { offer } => {
                let _ = offer;
                asks.drags.push(Drag::Finished);
            }
            Event::Destroyed {
                object,
                role: Role::DataSource | Role::PrimarySource | Role::DataControlSource,
            } => effects.gone_sources.push(object),
            Event::DataDeviceMade { .. } => effects.made_device.push(Which::Clipboard),
            Event::PrimaryDeviceMade { .. } => effects.made_device.push(Which::Primary),
            Event::SelectionSet { source, mimes } => {
                effects
                    .selection
                    .push((Which::Clipboard, source, mimes.clone(), Through::Window));
            }
            Event::PrimarySet { source, mimes } => {
                effects
                    .selection
                    .push((Which::Primary, source, mimes.clone(), Through::Window));
            }
            Event::SelectionWanted { offer, mime, fd } => {
                // An offer made for a *drag* is not the selection's:
                // the data comes from the client that started the
                // drag, and goes on the same kind of pipe.
                if slot.client.drag_offer() == Some(offer) {
                    asks.drags.push(Drag::Receive {
                        mime: mime.clone(),
                        fd,
                    });
                } else {
                    effects.wanted.push((
                        Which::Clipboard,
                        offer,
                        mime.clone(),
                        fd,
                        Through::Window,
                    ));
                }
                effects.claimed += 1;
            }
            Event::PrimaryWanted { offer, mime, fd } => {
                effects
                    .wanted
                    .push((Which::Primary, offer, mime.clone(), fd, Through::Window));
                effects.claimed += 1;
            }
            // A client naming its cursor rather than drawing one, and a
            // client asking for another's window: both reach the loop.
            Event::CursorShaped { shape } => effects.shaped = Some(shape),
            // Typing through an input method: the application's half
            // and the method's half are two connections, and joining
            // them is the whole of what the compositor does here.
            Event::TextInputEnabled {
                text_input,
                enabled,
            } => effects.typing.push((text_input, enabled)),
            Event::InputMethodMade { method } => effects.became_method = Some(method),
            Event::InputMethodTyped { typed, .. } => effects.was_typed.push(typed.clone()),
            Event::InputMethodGone { .. } => effects.method_gone = true,
            Event::ActivationAsked { token, surface } => {
                effects.activating = Some((token.clone(), surface));
            }
            Event::Bound {
                role: Role::ForeignToplevelManager,
                ..
            } => effects.bound_manager = true,
            // A popup: a menu, a tooltip, a dropdown. Where it goes
            // needs the window its parent is, which the layout has.
            Event::PopupCreated { popup, .. } => effects.fresh_popups.push(popup),
            Event::PopupGone { .. } | Event::PopupGrabbed { .. } => effects.changed = true,
            // The session lock. Which screens it covers and when it is
            // told so are the loop's, which has them.
            Event::SessionLocked { lock } => effects.locking = Some(lock),
            Event::SessionLockSurfaceMade {
                lock_surface,
                surface,
                output,
            } => effects.covered.push((lock_surface, surface, output)),
            Event::SessionUnlocked { lock, asked } => effects.unlocking = Some((lock, asked)),
            // A screenshot: the screens are the loop's, not this
            // client's, so both halves are carried out there.
            Event::ScreencopyWanted {
                frame,
                output,
                region,
            } => self.shots.push(Shot::Wanted {
                client: index,
                frame,
                output,
                region,
            }),
            Event::ScreencopyInto {
                frame,
                buffer,
                output,
                region,
                with_damage,
            } => self.shots.push(Shot::Into {
                client: index,
                frame,
                buffer,
                output,
                region,
                with_damage,
            }),
            // A bar asked for something to be done to a window it
            // does not own, which is the whole point of the protocol:
            // clicking a taskbar entry focuses that window, and the
            // middle click closes it.
            Event::ForeignToplevelAsked { window, what } => {
                effects.asked.push((WindowId(window), what));
            }
            // A client asking to be fullscreen or maximized. Hyprland
            // does both, and `windowrule = suppress_event` is how a
            // person turns one off -- which is only worth writing
            // because the compositor obeys it by default.
            Event::ToplevelAsked {
                toplevel,
                maximized,
                fullscreen,
            } => {
                if let Some((_, window)) = slot.windows.iter().find(|(top, _)| *top == toplevel) {
                    effects.asked_states.push((*window, maximized, fullscreen));
                }
            }
            // A press on a window's own title bar or edge, handed to the
            // compositor to drag with.
            Event::ToplevelDragAsked { toplevel, edges } => {
                if let Some((_, window)) = slot.windows.iter().find(|(top, _)| *top == toplevel) {
                    effects.asked_drags.push((*window, edges));
                }
            }
            // A modal dialog is one the application will not let you
            // look past, so it floats: Hyprland's own `windowrule =
            // float, xdg_dialog` says the same thing by hand, and this
            // is the protocol saying it for itself.
            Event::ToplevelModal { toplevel, modal } => {
                if let Some((_, window)) = slot.windows.iter().find(|(top, _)| *top == toplevel) {
                    effects.modals.push((*window, modal));
                }
            }
            // There is nothing to ring: this compositor has no sound.
            // Saying so is more than the warning a toolkit logs when
            // the global is missing.
            Event::Bell { .. } => {
                (self.report)("hyprix: a client rang the bell");
            }
            // A virtual device's input is a person's: it goes to the
            // seat, keybinds and all, which is what makes `wtype` type
            // into whatever is focused and what wlroots gates behind a
            // compositor's own policy. This one offers it to every
            // client, as Hyprland does.
            Event::Injected(what) => {
                if let Some(input) = as_input(what) {
                    self.injected.push(input);
                }
            }
            Event::ToplevelCreated { toplevel, .. } => {
                let window = WindowId(u64::from(self.next_window));
                self.next_window = self.next_window.saturating_add(1);
                // A window with nowhere to go -- no monitor while a card
                // is coming back -- is that window's trouble, not every
                // other client's: it stays out of the layout, and its
                // client goes on.
                match self.state.open_window(window) {
                    Ok(_) => slot.windows.push((toplevel, window)),
                    Err(error) => (self.report)(&format!(
                        "hyprix: a client's window could not be placed: {error:?}"
                    )),
                }
                effects.changed = true;
            }
            Event::SurfaceCommitted { surface, change } => {
                self.surface_committed(index, surface, change);
                effects.changed = true;
            }
            _ => {}
        }
        Ok(())
    }

    /// A surface of the connection at `index` committed: a window's first
    /// commit maps it, a dialog's first buffer sizes it, a floating window
    /// that draws a size of its own after that takes it, and what the client
    /// drew is owed to the next frame.
    fn surface_committed(
        &mut self,
        index: usize,
        surface: ObjectId,
        change: compositor_server::Committed,
    ) {
        let Some(slot) = self.slots.get_mut(index) else {
            return;
        };
        if change.buffer.is_none()
            && let Some((toplevel, window)) = slot
                .windows
                .iter()
                .find(|(top, _)| {
                    slot.client
                        .toplevel(*top)
                        .is_some_and(|state| state.surface == surface)
                })
                .copied()
        {
            // The first commit of a window asks to be
            // configured, and is where Hyprland applies the
            // window's rules: by now the client has said what
            // it is called.
            // The frame is redrawn below whatever the rules
            // did, since a window that has just mapped is a
            // change in itself.
            // What it is called now is what it opened as, and
            // `initialclass:` and `initialtitle:` keep it after
            // the client has renamed the window.
            let called = slot.client.toplevel(toplevel).map_or_else(
                || (String::new(), String::new()),
                |top| (top.app_id.clone(), top.title.clone()),
            );
            let _ = slot.firsts.insert(window, called);
            let _ = apply_rules(
                &mut self.window_rules,
                &slot.client,
                toplevel,
                window,
                &mut self.state,
                self.report,
            );
            configure_first(
                &mut slot.client,
                &mut slot.unsized_dialogs,
                toplevel,
                window,
                &mut self.state,
            );
            let _ = self.sources.insert(
                window,
                Source {
                    client: index,
                    surface,
                },
            );
        }
        if change.mapped
            && let Some((toplevel, window)) = slot
                .windows
                .iter()
                .find(|(top, window)| {
                    slot.unsized_dialogs.contains(window)
                        && slot
                            .client
                            .toplevel(*top)
                            .is_some_and(|state| state.surface == surface)
                })
                .copied()
        {
            let _ = slot.unsized_dialogs.remove(&window);
            fit_dialog(
                &slot.client,
                &slot.windows,
                toplevel,
                window,
                &mut self.state,
            );
            configure(&mut slot.client, &self.state, toplevel, window);
        }
        // A floating window drawn at a size of its own: an X program that
        // resized its window, which yserver shows as buffers of the new
        // size. It floats at that size, not squeezed into the one it had.
        if change.buffer.is_some()
            && let Some((toplevel, window)) = slot
                .windows
                .iter()
                .find(|(top, window)| {
                    !slot.unsized_dialogs.contains(window)
                        && slot
                            .client
                            .toplevel(*top)
                            .is_some_and(|state| state.surface == surface)
                })
                .copied()
        {
            follow_own_size(
                &slot.client,
                &mut slot.answered,
                toplevel,
                window,
                &mut self.state,
            );
        }
        // What the client says it drew, which is the only thing
        // that can change the pixels inside a surface: where
        // that lands on a screen is worked out when the frame
        // is drawn, because only then is it known where the
        // surface is.
        self.commits.painted(painted(&slot.client, index, surface));
        if let Some(old) = change.released {
            slot.client.release_buffer(old);
        }
    }

    /// Carry out what reading the connection at `index` kept for after its
    /// own borrow, one kind at a time and in this order. Gives whether the
    /// layout changed, while reading or since.
    fn apply(&mut self, index: usize, asks: &mut ScreenAsks, effects: Effects) -> bool {
        let Effects {
            changed,
            claimed: _,
            selection,
            gone_sources,
            wanted,
            watching,
            warped,
            made_device,
            shaped,
            activating,
            typing,
            became_method,
            was_typed,
            method_gone,
            asked,
            modals,
            closed,
            asked_states,
            asked_drags,
            bound_manager,
            fresh_popups,
            locking,
            covered,
            unlocking,
        } = effects;
        let mut changed = changed;
        // Now that the client's own borrow is over: what it copied goes to
        // every other client, and what it pasted goes to whoever copied.
        self.offer_selections(index, made_device, watching, selection);
        // After the sources set in this pass: a client that sets a new
        // source and destroys the old one in the same batch keeps the new.
        for source in gone_sources {
            self.clipboard.source_gone(&mut self.slots, index, source);
        }
        changed |= self.shape_cursor(shaped);
        // An activation: one program asking for another's window to be
        // raised, with a token this compositor gave out. A token it did not
        // give out is refused, which is the whole of the protocol's
        // security.
        if let Some((token, surface)) = activating {
            changed |= activate(
                &token,
                surface,
                &mut self.slots,
                index,
                &mut self.state,
                &self.sources,
                &mut self.urgent,
                self.fixed.focus_on_activate,
                self.report,
            );
        }
        input_method_turn(
            &mut self.method,
            &mut self.slots,
            index,
            &typing,
            became_method,
            &was_typed,
            method_gone,
            self.report,
        );
        if bound_manager {
            self.list_windows_for(index);
        }
        for popup in fresh_popups {
            changed |= place_popup(
                popup,
                &mut self.slots,
                index,
                &self.state,
                &self.sources,
                &self.screens,
            );
        }
        changed |= lock_changed(
            &mut self.lock,
            &mut self.slots,
            index,
            &self.screens,
            locking,
            &covered,
            unlocking,
            &mut super::LockSeat {
                grants: &mut self.grants,
                now: std::time::Instant::now(),
                grace: self.lock_grace,
            },
            self.report,
        );
        changed |= self.for_the_bars(asked);
        self.warp(warped, asks);
        changed |= self.close_windows(index, closed);
        changed |= self.set_states(asked_states);
        changed |= self.float_modals(modals);
        for (window, edges) in asked_drags {
            if crate::act::client_drag(window, edges, &self.state, &self.seat, &mut self.drag) {
                (self.report)(&format!(
                    "hyprix: window {} is dragged by its own {}",
                    window.0,
                    if edges == 0 { "move" } else { "resize" }
                ));
            }
        }
        self.paste(index, wanted);
        changed
    }

    /// The selections, to a client that has just made a device or become a
    /// clipboard manager, and what a client copied, to every other.
    fn offer_selections(
        &mut self,
        index: usize,
        made_device: Vec<Which>,
        watching: Vec<ObjectId>,
        selection: Vec<(Which, Option<ObjectId>, Vec<String>, Through)>,
    ) {
        for which in made_device {
            self.clipboard.offer_to(which, &mut self.slots, index);
        }
        for _device in watching {
            self.clipboard.offer_both_to(&mut self.slots, index);
        }
        for (which, source, mimes, through) in selection {
            let held = mimes.len();
            let name = match which {
                Which::Clipboard => "selection",
                Which::Primary => "primary selection",
            };
            self.clipboard
                .copied(which, &mut self.slots, index, source, mimes, through);
            if source.is_some() {
                (self.report)(&format!(
                    "hyprix: the {name} is {held} type(s) from client {index}"
                ));
            } else {
                (self.report)(&format!("hyprix: the {name} was given up"));
            }
        }
    }

    /// A cursor shape is the seat's: the compositor draws its own arrow for
    /// every one of them, and says which was asked for so that what a real
    /// toolkit wanted is on the record. Gives whether one was.
    fn shape_cursor(&mut self, shaped: Option<u32>) -> bool {
        if let Some(shape) = shaped {
            (self.report)(&format!("hyprix: a client asked for cursor shape {shape}"));
            return true;
        }
        false
    }

    /// The windows there are, to a client that has just bound the foreign
    /// toplevel manager.
    ///
    /// Before anything else this client is owed: a `wl_display.sync` sent
    /// after the bind is how every such client knows it has the whole list,
    /// and an answer that arrives after the callback is a list it never sees.
    fn list_windows_for(&mut self, index: usize) {
        let windows = crate::control::toplevels(&crate::control::describe_all(
            &self.state,
            &self.slots,
            &self.sources,
            &BTreeMap::new(),
            "",
        ));
        if let Some(slot) = self.slots.get_mut(index) {
            slot.client_mut().show_toplevels(&windows);
        }
    }

    /// What bars asked to be done to windows they do not own. Gives
    /// whether the layout changed.
    fn for_the_bars(&mut self, asked: Vec<(WindowId, ForeignRequest)>) -> bool {
        let mut changed = false;
        for (window, what) in asked {
            (self.report)(&format!(
                "hyprix: a bar asked for {what:?} of window {}, {}",
                window.0,
                named(window, &self.slots, &self.sources)
            ));
            changed |= for_the_bar(
                window,
                what,
                &mut self.state,
                &mut self.slots,
                &self.sources,
            );
        }
        changed
    }

    /// Where clients asked the pointer to be put, for the loop to move it
    /// once every client has been read.
    fn warp(
        &mut self,
        warped: Vec<(
            usize,
            ObjectId,
            (compositor_wire::Fixed, compositor_wire::Fixed),
        )>,
        asks: &mut ScreenAsks,
    ) {
        for (client, surface, (x, y)) in warped {
            // The place is inside the surface, and the pointer is put in the
            // space all screens share: a client may only move the pointer
            // inside its own window, which is what the protocol is for.
            let Some(rect) = rect_of(&self.state, &self.sources, client, surface) else {
                continue;
            };
            #[expect(
                clippy::cast_precision_loss,
                reason = "a screen's pixels are far inside f64's exact range"
            )]
            let at = (rect.x as f64 + x.to_f64(), rect.y as f64 + y.to_f64());
            asks.warps.push(at);
            (self.report)(&format!(
                "hyprix: client {client} put the pointer at {}, {}",
                at.0, at.1
            ));
        }
    }

    /// The windows the client closed, now that its own borrow is over:
    /// the same things a gone connection's windows are given. Gives whether
    /// there were any.
    fn close_windows(&mut self, index: usize, closed: Vec<WindowId>) -> bool {
        let mut changed = false;
        for window in closed {
            remember_size(
                window,
                &mut self.slots,
                index,
                &mut self.state,
                &mut self.window_rules,
            );
            let _ = self.state.window_gone(window);
            let _ = self.sources.remove(&window);
            self.window_rules.window_gone(window);
            changed = true;
        }
        changed
    }

    /// What windows asked `xdg_toplevel` to be: fullscreen, maximized, or
    /// neither. Gives whether the layout changed.
    fn set_states(&mut self, asked_states: Vec<(WindowId, bool, bool)>) -> bool {
        let mut changed = false;
        for (window, maximized, fullscreen) in asked_states {
            // Hyprland's fullscreen modes: 0 is the whole screen, 1 is
            // maximized inside the gaps. A window asking for both gets the
            // larger, as Hyprland's `set_fullscreen` does.
            let wanted = if fullscreen {
                Some("0")
            } else if maximized {
                Some("1")
            } else {
                None
            };
            let event = if fullscreen { "fullscreen" } else { "maximize" };
            if wanted.is_some() && self.window_rules.suppresses(window, event) {
                (self.report)(&format!(
                    "hyprix: window {} asked for {event}, which a windowrule suppressed",
                    window.0
                ));
                continue;
            }
            let holds = self
                .state
                .workspace_of(window)
                .and_then(|workspace| self.state.fullscreen(workspace))
                .is_some_and(|(id, _)| id == window);
            // `fullscreen <mode>` turns it on and off again, so a window
            // already where it asked to be is left alone.
            let mode = match wanted {
                Some(mode) if !holds => mode,
                None if holds => "0",
                _ => continue,
            };
            let was = self.state.focused_window();
            if self.state.focus_window(window).is_ok() {
                let made = self.state.dispatch_str("fullscreen", mode);
                changed |= made.is_ok_and(|changes| !changes.is_empty());
                if let Some(was) = was {
                    let _ = self.state.focus_window(was);
                }
            }
        }
        changed
    }

    /// The windows `xdg_dialog_v1` called modal, floated, and those it no
    /// longer does, tiled. Gives whether the layout changed.
    fn float_modals(&mut self, modals: Vec<(WindowId, bool)>) -> bool {
        let mut changed = false;
        for (window, modal) in modals {
            // `setfloating` and `settiled` act on the focused window, and this
            // one need not be focused; the layout's own call is what a rule
            // uses, and it takes the window.
            let was = self.state.focused_window();
            if self.state.focus_window(window).is_ok() {
                let made = self
                    .state
                    .dispatch_str(if modal { "setfloating" } else { "settiled" }, "");
                changed |= made.is_ok_and(|changes| !changes.is_empty());
                if let Some(was) = was {
                    let _ = self.state.focus_window(was);
                }
            }
            (self.report)(&format!(
                "hyprix: window {} is {}modal",
                window.0,
                if modal { "" } else { "no longer " }
            ));
        }
        changed
    }

    /// What the client pasted: a pipe to whoever copied.
    fn paste(&mut self, index: usize, wanted: Vec<(Which, ObjectId, String, Fd, Through)>) {
        for (which, offer, mime, fd, through) in wanted {
            if self
                .clipboard
                .pasted(which, &mut self.slots, index, offer, &mime, fd, through)
            {
                (self.report)(&format!(
                    "hyprix: client {index} pasted {mime}, on a pipe to whoever copied"
                ));
            } else {
                (self.report)(&format!(
                    "hyprix: client {index} pasted {mime} through an offer that is not the \
                     selection any more"
                ));
            }
        }
    }

    /// Send the connection at `index` what it is owed, and drop it if it
    /// cannot take it or has broken the protocol.
    fn send(&mut self, index: usize) {
        let Some(slot) = self.slots.get_mut(index) else {
            return;
        };
        let outgoing = slot.client.take_outgoing();
        let sent = if !outgoing.bytes.is_empty() {
            slot.connection.send(&outgoing.bytes, &outgoing.descriptors)
        } else if slot.connection.has_pending_writes() {
            slot.connection.flush()
        } else {
            Ok(())
        };
        match sent {
            Ok(()) => {}
            // A full socket is a client busy for a moment, and what it would not
            // take waits in the queue. A queue past Hyprland's limit is a client
            // that has stopped reading.
            Err(compositor_socket::SendError::Overflow(waiting)) => {
                (self.report)(&format!(
                    "hyprix: client {index} has stopped reading, with {waiting} bytes waiting, \
                     and is dropped"
                ));
                slot.gone = true;
            }
            Err(error) => {
                (self.report)(&format!(
                    "hyprix: client {index} could not be sent to, and is dropped: {error:?}"
                ));
                slot.gone = true;
            }
        }
        // A client the server has sent a protocol error is ended by it, and the
        // client's own log names only the request it was sending when its
        // socket closed -- which is seldom the one that was wrong.
        if let Some(fatal) = slot.client.fatal() {
            (self.report)(&format!(
                "hyprix: client {index} broke the protocol, and is dropped: {}",
                fatal.message()
            ));
            slot.gone = true;
        }
    }
}
