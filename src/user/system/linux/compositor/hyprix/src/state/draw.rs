//! The frame: what each screen is drawn from, drawing it, and what the
//! clients are owed once it is shown.
//!
//! A pass draws a frame when one is owed and the screens' refresh allows
//! it. Each screen is worked out on its own -- where the lock, the bars and
//! the menus go over its windows, the pointer, the counter -- into a plan
//! the damage is compared against, and then drawn from that plan.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use compositor_layout::{Rect, WindowId};

use super::{
    Compositor, FRAME_REPORT, blurs_behind, cursor_shown, cursor_surface, described, drawn_with,
    dump, frames_done, refresh_ns, sync_planes,
};

impl Compositor<'_> {
    /// Draw a frame if one is owed and the screens can take it. `changed`
    /// gains whatever moving the pointer's planes changed. Gives whether
    /// an animation is running, which the wait has to know.
    ///
    /// A frame is drawn when something changed, when nothing has been
    /// drawn yet, and while any window is still on its way somewhere: an
    /// animation is a frame a change does not ask for.
    pub(super) fn frame(
        &mut self,
        changed: &mut bool,
        now: u32,
        popups: &[crate::frame::Placed],
    ) -> Result<bool, String> {
        // The loop's `now` is milliseconds as a `u32`, which is what
        // `wl_keyboard.key` and `wl_pointer.motion` carry; an animation's is
        // the same clock, widened.
        let millis = u64::from(now);
        let animating = self.animations.busy(millis);
        // `settling` is the one pass after the last moving one: the frame
        // that is drawn while a window is still moving is a frame short of
        // its goal, and without this the screen would keep that last frame
        // for ever.
        //
        // And when the screen can take one. A change is owed a frame, not
        // drawn the moment it happens: what changed is kept -- the commits
        // in `commits`, everything else in the plan the next frame is
        // compared with -- and the frame that shows it is the next one the
        // screen's refresh allows.
        // The pointer on the screens with a cursor plane, as it is now: a
        // move there waits for no frame. A screen whose pointer went from
        // its frames to its plane, or back, owes a frame that draws it or
        // takes it away.
        let pointer = (self.lock.is_none()
            && self.devices.has_pointer()
            && self.seat.pointer_used())
        .then(|| crate::frame::Cursor {
            at: (0, 0),
            surface: cursor_surface(&self.slots, &self.focus),
            shown: cursor_shown(&self.slots, &self.focus),
        });
        *changed |= sync_planes(
            &mut self.screens,
            &self.slots,
            pointer.map(|cursor| (cursor, self.seat.pointer())),
            &|name| self.dpms.get(name).copied().unwrap_or(false),
            self.fixed.planes,
            self.report,
        );
        self.owed |= *changed;
        // `debug:overlay`, read every pass so that `hyprctl keyword` turns
        // it on and off at once. While it is up its numbers move every
        // 200 ms, and a frame is owed that often, as Hyprland's
        // `COverlay::draw` schedules one.
        let overlay_on = self.config.int("debug:overlay").unwrap_or(0) == 1;
        if overlay_on != self.overlay_was || (overlay_on && self.overlay.ask(Instant::now())) {
            self.owed = true;
        }
        self.overlay_was = overlay_on;
        if (self.owed || self.tally.drawn == 0 || animating || self.settling)
            && self.pace.due(Instant::now(), refresh_ns(&self.screens))
        {
            self.draw(now, animating, overlay_on, popups)?;
        }
        Ok(animating)
    }

    /// Draw the frame: every screen, then what the clients are owed for it
    /// and what the report says of it.
    fn draw(
        &mut self,
        now: u32,
        animating: bool,
        overlay_on: bool,
        popups: &[crate::frame::Placed],
    ) -> Result<(), String> {
        self.owed = false;
        self.settling = animating;
        // A pass that moves the animations is one of their ticks:
        // Hyprland keeps how long it was since the last.
        if animating {
            self.overlay.tick(Instant::now());
        }
        self.overlay
            .screens(self.screens.iter().map(|screen| screen.name.as_str()));
        let outputs = self.state.layout();
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the pointer is held inside the screen, which is far inside i64"
        )]
        let cursor_at = {
            let (x, y) = self.seat.pointer();
            (x as i64, y as i64)
        };
        // What each window is drawn with: what a `windowrule` gave it,
        // and over that whatever `wp_alpha_modifier_v1` asked for. The
        // protocol's multiplier is the client's own word about how much
        // of its surface shows, so it wins over a rule's opacity.
        let drawn_with = drawn_with(self.window_rules.styles(), &self.slots, &self.sources);
        let began = Instant::now();
        let mut drew = Drew::default();
        let with = Drawing {
            millis: u64::from(now),
            overlay_on,
            cursor_at,
            drawn_with: &drawn_with,
            popups,
        };
        // A frame a monitor: each screen draws the workspace it shows,
        // with the windows' rectangles moved into its own pixels.
        for which in 0..self.screens.len() {
            let Some(output) = self.screens.get(which).and_then(|screen| {
                outputs
                    .iter()
                    .find(|output| output.monitor == screen.monitor)
            }) else {
                continue;
            };
            self.draw_screen(which, output, &with, &mut drew)?;
        }
        self.owed |= drew.waiting;
        // The frame has been drawn from them, so the next one starts
        // from what happens next.
        self.commits.taken();
        self.tally.least = self.tally.least.min(drew.redrew);
        self.tally.drawn = self.tally.drawn.saturating_add(1);
        // A window this frame set moving -- one whose place the tiling
        // changed since the last frame, which `follow` has only now taken
        // up -- was not moving when `animating` was read before it. So the
        // loop is owed the frames after this one too, or it sleeps with the
        // window drawn where it started until some other input wakes it: a
        // `hyprctl dispatch movewindow` with nothing else happening left
        // the swap undrawn for good.
        if self.animations.busy(u64::from(now)) {
            self.settling = true;
        }
        // Every surface that went into the frame is owed two things: a
        // `wl_callback.done` for the `wl_surface.frame` it asked for,
        // and a `wp_presentation_feedback.presented` if it asked for
        // one. A client that waits on the first before drawing again --
        // which every toolkit does -- draws once and stops without it.
        frames_done(
            &mut self.slots,
            &self.sources,
            &self.placed_layers,
            popups,
            self.lock.as_ref(),
            now,
            self.tally.drawn,
            refresh_ns(&self.screens),
        );
        // And only now each screen's flip, which waits for the host to show
        // the frame: on QEMU's GL display that is the window's next repaint,
        // on VNC a read of the whole screen back. Clients told their frame
        // was done before it draw their next one meanwhile; told after it,
        // a video's frames missed one compositor frame in three.
        for (which, damage) in std::mem::take(&mut drew.flips) {
            if let Some(screen) = self.screens.get_mut(which) {
                crate::frame::flip(screen.backend.as_mut(), &damage)?;
            }
        }
        self.count_frame(began.elapsed(), &drew);
        if self.tally.drawn == 1 {
            // The screens are up and the first frame is on them. This is
            // what a watcher waits for, in the shape
            // `src/user/system/linux/compositor/blank`'s marker has.
            (self.report)(&format!(
                "hyprix: {} {display}",
                described(&self.screens),
                display = self.fixed.display,
            ));
        }
        if let Some(directory) = self.options.dump.as_ref() {
            dump(&mut self.screens, directory, self.tally.drawn)?;
        }
        Ok(())
    }

    /// Whether the screen at `which` can be drawn on: a screen whose card
    /// went away is looked for again every frame and drawn for only once it
    /// is back.
    ///
    /// Then as a whole frame, since the card it is on now has seen none of
    /// the last one's, and in software, since the GPU went with it. The
    /// loss is said here too: a cursor that could not be shown or moved
    /// finds it with nothing said, and a driver killed as the pointer first
    /// reaches its plane is lost that way, before any frame or wait could
    /// find it.
    fn screen_back(&mut self, which: usize) -> bool {
        let Some(screen) = self.screens.get_mut(which) else {
            return false;
        };
        let lost = screen.backend.lost();
        if lost {
            screen.say_gone(self.report);
        }
        if lost && !screen.backend.recover() {
            return false;
        }
        if lost {
            (self.report)(&format!(
                "hyprix: {}: the card is back; drawing on it again",
                screen.name
            ));
            screen.said_gone = false;
            screen.gpu = None;
            screen.watch = crate::damage::Watch::default();
            screen.plane.forget();
        }
        true
    }

    /// What is drawn over the windows on the screen at `which`, or over
    /// the lock: the bars and the menus, or the lock's own surface and
    /// whatever a `layerrule = abovelock` asked for over it -- an on-screen
    /// keyboard, which is the whole reason that rule exists, because a
    /// compositor that drew nothing over the lock would leave a person with
    /// no way to type the password.
    fn over_windows(
        &self,
        which: usize,
        rect: Rect,
        dark: bool,
        popups: &[crate::frame::Placed],
    ) -> Vec<crate::frame::Placed> {
        if dark {
            Vec::new()
        } else if let Some(held) = self.lock.as_ref() {
            held.surfaces
                .get(&which)
                .map(|(_, surface)| crate::frame::Placed {
                    client: held.client,
                    surface: *surface,
                    rect,
                    above: true,
                    rules: crate::frame::LayerRules::default(),
                })
                .into_iter()
                .chain(
                    self.placed_layers
                        .iter()
                        .copied()
                        .filter(|placed| placed.rules.above_lock),
                )
                .collect()
        } else {
            self.placed_layers
                .iter()
                .copied()
                .chain(popups.iter().copied())
                .collect()
        }
    }

    /// Draw one screen's frame into `drew`.
    fn draw_screen(
        &mut self,
        which: usize,
        output: &compositor_layout::MonitorLayout,
        with: &Drawing<'_>,
        drew: &mut Drew,
    ) -> Result<(), String> {
        if !self.screen_back(which) {
            drew.waiting = true;
            return Ok(());
        }
        let Some(screen) = self.screens.get(which) else {
            return Ok(());
        };
        // When this screen's frame began, which is what the
        // counter's frame times and render times are taken from.
        let screen_began = Instant::now();
        if with.overlay_on {
            self.overlay.frame(
                &screen.name,
                screen.output().refresh,
                which == 0,
                screen_began,
            );
        }
        // Where each window *is*, rather than where the tiling put
        // it.
        let output = &self.animations.follow(output, with.millis);
        let Some(planned) = self.plan_screen(which, output, with, screen_began) else {
            return Ok(());
        };
        self.present(which, output, with, screen_began, planned, drew)
    }

    /// What the screen at `which` is drawn from this frame, worked out
    /// before any of it is drawn: `output` is where each window is.
    fn plan_screen(
        &mut self,
        which: usize,
        output: &compositor_layout::MonitorLayout,
        with: &Drawing<'_>,
        screen_began: Instant,
    ) -> Option<Planned> {
        let screen = self.screens.get(which)?;
        let (width, height) = screen.size();
        let origin = (screen.rect.x, screen.rect.y);
        let scale = screen.scale;
        // A screen `dpms off` turned off shows nothing at all,
        // before the lock and before the windows: that is what
        // turning a screen off means.
        let dark = self.dpms.get(&screen.name).copied().unwrap_or(false);
        let over = self.over_windows(which, screen.rect, dark, with.popups);
        // The surface a drag is carrying, drawn at the pointer:
        // that is what makes a drag look like one.
        let drag_icon = self
            .carried
            .as_ref()
            .filter(|held| !dark && held.holding())
            .and_then(|held| {
                Some(crate::frame::Placed {
                    client: held.client,
                    surface: held.icon?,
                    rect: Rect::new(with.cursor_at.0, with.cursor_at.1, 0, 0),
                    above: true,
                    rules: crate::frame::LayerRules::default(),
                })
            });
        // The pointer, unless the session is locked: a lock screen
        // draws its own and the compositor's arrow over it would be
        // two pointers.
        // And not where the screen's cursor plane shows it.
        let cursor = (!dark
            && !screen.plane.on
            && self.lock.is_none()
            && self.devices.has_pointer()
            && self.seat.pointer_used())
        .then(|| crate::frame::Cursor {
            at: with.cursor_at,
            surface: cursor_surface(&self.slots, &self.focus),
            shown: cursor_shown(&self.slots, &self.focus),
        });
        // The ramps a night-light set on this screen, applied to
        // the pixels on their way out.
        let gamma = self.gammas.get(&which).copied();
        // A locked screen draws no window, and a screen that is off
        // draws nothing at all: a plan says what is drawn, not what
        // the layout holds.
        let scaled = self.style.at_scale(scale);
        let layout = if dark || self.lock.is_some() {
            compositor_layout::MonitorLayout {
                windows: Vec::new(),
                ..output.clone()
            }
        } else {
            compositor_render::scaled(output, origin, scale)
        };
        let mut blurred = blurs_behind(
            &scaled,
            &layout,
            &over,
            &self.slots,
            &self.sources,
            with.drawn_with,
            (origin, scale),
        );
        // The counter, on the first screen as Hyprland draws it,
        // over everything but a screen that is off. Its boxes blur
        // what is behind them as it stands, which the damage has to
        // know to redraw them whole.
        let picture = (with.overlay_on && which == 0 && !dark)
            .then(|| self.overlay.picture(screen_began, scale));
        if scaled.blur.is_some()
            && let Some(picture) = picture.as_ref()
        {
            blurred.extend(
                picture
                    .blurred()
                    .map(|rect| crate::damage::Blurred { rect, live: true }),
            );
        }
        // Everything this frame is drawn from but the clients' own
        // pixels, which is what the damage is worked out from by
        // comparing it with the frame before's.
        let plan = crate::damage::Plan {
            size: (width, height),
            origin,
            scale,
            style: scaled,
            dark,
            locked: self.lock.is_some(),
            gamma,
            layout,
            blurred,
            styles: with.drawn_with.clone(),
            layers: over.clone(),
            cursor: cursor.and_then(|cursor| {
                Some((
                    cursor,
                    crate::frame::cursor_rect(&self.slots, &cursor, origin, scale)?,
                ))
            }),
            drag_icon: drag_icon.and_then(|icon| {
                Some((
                    icon,
                    crate::frame::drag_rect(&self.slots, &icon, origin, scale)?,
                ))
            }),
            overlay: picture.as_ref().map(|picture| picture.stamp),
            plane: screen
                .plane
                .on
                .then(|| cursor_surface(&self.slots, &self.focus))
                .flatten()
                .map(|(client, surface, _)| (client, surface)),
        };
        Some(Planned {
            plan,
            over,
            drag_icon,
            cursor,
            gamma,
            picture,
            dark,
            origin,
            scale,
        })
    }

    /// Draw the screen at `which` from what `plan_screen` worked out, and
    /// count what it drew into `drew`.
    fn present(
        &mut self,
        which: usize,
        output: &compositor_layout::MonitorLayout,
        with: &Drawing<'_>,
        screen_began: Instant,
        planned: Planned,
        drew: &mut Drew,
    ) -> Result<(), String> {
        let Planned {
            plan,
            over,
            drag_icon,
            cursor,
            gamma,
            picture,
            dark,
            origin,
            scale,
        } = planned;
        let Some(screen) = self.screens.get_mut(which) else {
            return Ok(());
        };
        // And the clients' own pixels: where on this screen each
        // commit since the last frame landed.
        let heard = self.commits.on(&plan, &self.sources);
        let frame = screen.watch.frame(plan, &heard, screen.backend.age());
        drew.redrew = drew.redrew.saturating_add(frame.canvas.area());
        // What the report says of the slowest frame's damage: how much
        // was drawn and in how many pieces, and how much was copied to
        // the screen -- a small change drawn as a large one is a
        // damage question, not a drawing one.
        drew.what = (
            drew.what.0.saturating_add(frame.canvas.area()),
            drew.what.1.saturating_add(frame.canvas.rects().len()),
            drew.what.2.saturating_add(frame.screen.area()),
        );
        drew.from = frame.sources;
        let mut target = crate::frame::Output {
            canvas: &mut screen.canvas,
            backdrop: &mut screen.backdrop,
            gpu: screen.gpu.as_mut(),
            backend: screen.backend.as_mut(),
            origin,
            style: &self.style,
            styles: with.drawn_with,
            scale,
            transform: screen.transform,
            drag_icon,
            gamma,
            cursor,
            present: frame.screen,
            overlay: picture.as_ref(),
            overlay_took: Duration::ZERO,
            flip: None,
        };
        let result = if dark {
            crate::frame::draw_dark(&mut target, &frame.canvas)
        } else if self.lock.is_some() {
            // The lock's own surface is the first of `over`, which
            // is where the damage above expects it too: the drawing
            // and the damage read one list.
            crate::frame::draw_locked(&mut target, output, &self.slots, None, &over, &frame.canvas)
        } else {
            crate::frame::draw(
                &mut target,
                output,
                &self.slots,
                &self.sources,
                &over,
                &frame.canvas,
            )
        };
        let overlay_took = target.overlay_took;
        if let Some(damage) = target.flip.take() {
            drew.flips.push((which, damage));
        }
        if with.overlay_on {
            self.overlay
                .rendered(&screen.name, screen_began.elapsed(), overlay_took);
        }
        match result {
            Ok(()) if screen.backend.lost() => {
                screen.say_gone(self.report);
                drew.waiting = true;
            }
            Ok(()) => {}
            // A GPU that has gone is not a screen that has. The
            // software canvas has drawn nothing while the GPU was
            // drawing, so what it is owed is everything: the watch
            // forgets what it saw, which makes the next frame a
            // whole one, and that frame is owed now.
            Err(why) if screen.gpu.is_some() && why.starts_with(crate::frame::GPU_FAILED) => {
                (self.report)(&format!(
                    "hyprix: {}: {why}; drawing in software from here on",
                    screen.name
                ));
                screen.gpu = None;
                screen.watch = crate::damage::Watch::default();
                self.owed = true;
            }
            Err(why) => return Err(why),
        }
        Ok(())
    }

    /// Count a frame that took `took` and drew what `drew` says, and say
    /// how the frames went once a second.
    fn count_frame(&mut self, took: Duration, drew: &Drew) {
        // The slowest frame, which is the bound `docs/ROADMAP.md` asks
        // each software effect to have: blur is the expensive one, and a
        // number measured on the machine that ran it is worth more than
        // one somebody hoped for.
        self.tally.slowest = self.tally.slowest.max(took);
        // Where the slowest frame of the report's interval spent its
        // time, which the report prints after it: a slow frame on a slow
        // machine is a question whose answer is one of these.
        let phases = compositor_render::timing::take();
        if took >= self.tally.since {
            self.tally.since_phases = phases;
            self.tally.since_drew = drew.what;
            self.tally.since_from = drew.from;
        }
        // Every so many frames, say how long the slowest of them took.
        // The compositor does not end on a machine it is the session of,
        // so a number only in the line it prints when it stops is a
        // number nobody sees: `docs/ROADMAP.md` asks each software
        // effect for a frame-time bound, and this is where it is
        // measured on the machine that ran it.
        //
        // And all of them together, after the slowest, which is what
        // the machine spent drawing: one frame in sixty may be slow for
        // a reason of its own, and a slowest frame cannot tell that
        // from sixty slow ones.
        self.tally.since = self.tally.since.max(took);
        self.tally.spent = self.tally.spent.saturating_add(took);
        self.tally.counted = self.tally.counted.saturating_add(1);
        if self.tally.reported.elapsed() >= FRAME_REPORT {
            (self.report)(&format!(
                "hyprix: frames {drawn} slowest of the last {counted} {} us, all of them {} us ({}; drew {} px in {} rects, showed {} px; from layout {} commits {} backdrop {} blurs {})",
                self.tally.since.as_micros(),
                self.tally.spent.as_micros(),
                compositor_render::timing::describe(&self.tally.since_phases),
                self.tally.since_drew.0,
                self.tally.since_drew.1,
                self.tally.since_drew.2,
                self.tally.since_from[0],
                self.tally.since_from[1],
                self.tally.since_from[2],
                self.tally.since_from[3],
                drawn = self.tally.drawn,
                counted = self.tally.counted,
            ));
            (
                self.tally.since,
                self.tally.spent,
                self.tally.counted,
                self.tally.reported,
            ) = (Duration::ZERO, Duration::ZERO, 0, Instant::now());
        }
    }
}

/// What every screen of one frame is drawn with.
struct Drawing<'a> {
    /// The loop's clock, which the animations follow.
    millis: u64,
    /// Whether `debug:overlay` is up.
    overlay_on: bool,
    /// Where the pointer is, in the space all screens share.
    cursor_at: (i64, i64),
    /// What each window is drawn with.
    drawn_with: &'a BTreeMap<WindowId, compositor_render::WindowStyle>,
    /// The popups, which are drawn over the windows.
    popups: &'a [crate::frame::Placed],
}

/// What one screen's frame is drawn from, worked out before any of it is
/// drawn.
struct Planned {
    /// Everything this frame is drawn from but the clients' own pixels.
    plan: crate::damage::Plan,
    /// What is drawn over the windows, or over the lock.
    over: Vec<crate::frame::Placed>,
    /// The surface a drag is carrying, drawn at the pointer.
    drag_icon: Option<crate::frame::Placed>,
    /// The pointer, where it is drawn into the frame.
    cursor: Option<crate::frame::Cursor>,
    /// The ramps a night-light set on this screen.
    gamma: Option<crate::frame::Gamma>,
    /// The counter, on the first screen while `debug:overlay` is up.
    picture: Option<crate::overlay::Picture>,
    /// Whether `dpms off` turned the screen off.
    dark: bool,
    /// Where the screen is in the space all screens share.
    origin: (i64, i64),
    /// The screen's scale.
    scale: f64,
}

/// What one frame drew, over every screen.
#[derive(Default)]
struct Drew {
    /// How many pixels this frame redrew, over every screen: what
    /// damage tracking is worth is how little a frame that changes
    /// little costs, and the line the loop prints says so.
    redrew: i64,
    /// How much was drawn, in how many pieces, and how much was shown.
    what: (i64, usize, i64),
    /// Where the last screen's frame drew from.
    from: [i64; 4],
    /// Whether a screen is lost and not yet back: the next frame is
    /// owed so that it is looked for again.
    waiting: bool,
    /// Each screen drawn and its flip, still to be made: after the
    /// clients' frame callbacks, so a client draws its next frame while
    /// the host shows this one.
    flips: Vec<(usize, compositor_render::Damage)>,
}
