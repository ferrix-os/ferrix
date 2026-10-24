//! Where the frame goes: memory, or a screen.

use std::io;
use std::path::Path;

/// A client's own buffer, to be shown as it is ([`Backend::scan_out`]):
/// linear `XRGB8888` (or `ARGB8888` read as opaque) from the dmabuf's first
/// byte, the screen's size.
#[derive(Debug, Clone, Copy)]
pub struct Scanout<'a> {
    /// The dmabuf.
    pub fd: std::os::fd::BorrowedFd<'a>,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Bytes from one row's start to the next.
    pub stride: u32,
}

/// A place a frame can be put.
///
/// `Debug` so a caller holding one can derive it; a backend's own debug
/// output is its name and size, never its pixels.
pub trait Backend: core::fmt::Debug {
    /// The size of the screen in pixels.
    fn size(&self) -> (u32, u32);

    /// How often the screen refreshes, in millihertz: what `wl_output.mode`
    /// says, what clients' presentation times are spaced by, and what the
    /// compositor paces its frames to. 60 Hz for a screen with no mode.
    fn refresh_mhz(&self) -> i32 {
        60_000
    }

    /// Bytes of an `XRGB8888` buffer of that size, to draw into.
    fn buffer(&mut self) -> &mut [u8];

    /// The bytes just shown, to read back: a client's own buffer while the
    /// screen shows one directly ([`Backend::scan_out`]).
    fn drawn(&self) -> &[u8];

    /// Bytes from one row's start to the next of [`Backend::buffer`].
    fn stride(&self) -> u32;

    /// Bytes from one row's start to the next of [`Backend::drawn`].
    fn drawn_stride(&self) -> u32 {
        self.stride()
    }

    /// Show a client's own buffer as the next frame, rather than one drawn
    /// into [`Backend::buffer`]: direct scanout (`docs/DISPLAY.md` §8.2),
    /// which [`Backend::present`] then flips to, whatever damage it is
    /// given. Called again each frame the screen should go on showing a
    /// client's buffer; a frame drawn into this backend's buffer after it is
    /// shown instead, the whole of it owed ([`Backend::scanning_out`]).
    ///
    /// # Errors
    ///
    /// A screen or card that cannot show it; the frame is then drawn as
    /// before.
    fn scan_out(&mut self, buffer: &Scanout<'_>) -> io::Result<()> {
        let _ = buffer;
        Err(io::Error::other("this screen shows only its own buffer"))
    }

    /// Whether the card shows a client's own buffer now: the next frame
    /// drawn into this backend's buffer owes all of it.
    fn scanning_out(&self) -> bool {
        false
    }

    /// Show what was drawn.
    ///
    /// `drawn` is what of the buffer has been written since it was last
    /// handed over, in its own pixels: a screen that is sent its picture
    /// rather than scanning it out of this memory is sent that much.
    ///
    /// # Errors
    ///
    /// Whatever the screen said.
    fn present(&mut self, drawn: &compositor_render::Damage) -> io::Result<()>;

    /// Whether a frame [`Backend::present`] handed over is still on its way
    /// to the screen: its answer is [`Backend::landed`]'s, and until then
    /// nothing is drawn into this backend -- its buffer may be the one the
    /// card is still copying or about to show.
    fn in_flight(&self) -> bool {
        false
    }

    /// A descriptor that becomes readable when the frame in flight has
    /// landed, for the compositor's wait: `None` while none is.
    fn flight_fd(&self) -> Option<std::os::fd::RawFd> {
        None
    }

    /// A frame handed over that has landed: what the screen said, and when
    /// it was handed over and answered. What follows a shown frame is done
    /// by then: the next buffer to draw into, the client's buffer the card
    /// shows. `None` while the frame is still on its way, and when none is
    /// left to tell of -- asked until then, since a frame the backend had
    /// to wait for itself is told of here too.
    fn landed(&mut self) -> Option<Landed> {
        None
    }

    /// Show what was drawn on the GPU, from now on, rather than this
    /// backend's own buffer: `fd` names the buffer object the renderer drew
    /// into, which the card is given and points its screen at.
    ///
    /// A backend that cannot -- one with no card, which is every backend
    /// but a real screen's -- says so, and the renderer fetches its frame
    /// into [`Backend::buffer`] as before. `docs/GPU.md` §3.5 piece 6 is
    /// what this is for: a frame that is already the device's, shown
    /// without being handed back to it.
    ///
    /// # Errors
    ///
    /// Whatever the card said. A backend that adopted nothing is unchanged.
    fn adopt(
        &mut self,
        fd: std::os::fd::BorrowedFd<'_>,
        width: u32,
        height: u32,
        pitch: u32,
    ) -> io::Result<()> {
        let _ = (fd, width, height, pitch);
        Err(io::Error::other("this screen shows only its own buffer"))
    }

    /// The size of image this screen's cursor plane shows, if it has one: a
    /// pointer on a plane moves without a frame being drawn
    /// (`crate::plane`).
    fn cursor_plane(&self) -> Option<(u32, u32)> {
        None
    }

    /// Show `image` -- premultiplied `ARGB8888`, rows packed, the plane's
    /// size -- on the cursor plane with its hotspot at `hot` and its
    /// top-left corner at `at`, in the screen's own pixels. Returns once the
    /// image is on the screen.
    ///
    /// # Errors
    ///
    /// A screen with no plane, and whatever the screen said.
    fn set_cursor(&mut self, image: &[u8], hot: (i32, i32), at: (i32, i32)) -> io::Result<()> {
        let _ = (image, hot, at);
        Err(io::Error::other("this screen has no cursor plane"))
    }

    /// Put the cursor plane's image's top-left corner at `at`, waiting for
    /// nothing.
    ///
    /// # Errors
    ///
    /// A screen with no plane, and whatever the screen said.
    fn move_cursor(&mut self, at: (i32, i32)) -> io::Result<()> {
        let _ = at;
        Err(io::Error::other("this screen has no cursor plane"))
    }

    /// Whether [`Backend::adopt`] took: a frame that is shown from the
    /// device needs no fetching.
    fn adopted(&self) -> bool {
        false
    }

    /// Show this backend's own buffer again rather than the one
    /// [`Backend::adopt`] took: what a screen that has given its GPU up
    /// needs, or the card goes on showing the GPU's last frame while the
    /// software frames go where nothing reads them.
    ///
    /// # Errors
    ///
    /// Whatever the card said; it is then still adopted.
    fn forsake(&mut self) -> io::Result<()> {
        Ok(())
    }

    /// How many frames old the buffer [`Backend::buffer`] gives is: one for
    /// a screen with one buffer, two for one that draws into two in turn.
    ///
    /// What a frame has to copy into it: its own damage, and that of every
    /// frame the buffer missed.
    fn age(&self) -> u32 {
        1
    }

    /// Whether the screen went away under the compositor: its card's driver
    /// died, and every call to it answers `ENODEV` until one is started
    /// again (`docs/DEVMGR.md` §4). A lost screen shows nothing and is not
    /// drawn for; the compositor and its clients carry on.
    fn lost(&self) -> bool {
        false
    }

    /// Try to have a lost screen back: open its card again and set the mode
    /// it had. Whether it is back, which a screen that was never lost always
    /// is. Cheap to call every frame: a backend tries only now and then.
    fn recover(&mut self) -> bool {
        true
    }

    /// A descriptor that becomes readable when the screen may have gone,
    /// for the compositor's wait: `None` for a screen that cannot go, and
    /// for one that already has.
    fn raw_fd(&self) -> Option<std::os::fd::RawFd> {
        None
    }

    /// Look, after [`Backend::raw_fd`] was readable, whether the screen has
    /// gone: `true` when it went just now. Without this a screen nothing is
    /// redrawn on would not find out until its next frame. A change of its
    /// modes found on the way is kept for [`Backend::modes_changed`].
    fn check(&mut self) -> bool {
        false
    }

    /// Whether the screen's modes changed since this was last asked: a
    /// virtio-gpu whose window on the host was resized, which now prefers
    /// the window's size.
    fn modes_changed(&mut self) -> bool {
        false
    }

    /// The size the screen prefers now, read from the card again: `None`
    /// for a screen that cannot say.
    fn preferred(&self) -> Option<(u32, u32)> {
        None
    }

    /// The refresh, in hertz, of the mode the screen prefers now: `None`
    /// for a screen that cannot say.
    fn preferred_refresh(&self) -> Option<f64> {
        None
    }

    /// Run the screen at another of its modes, `size` at the listed
    /// refresh nearest `refresh` hertz (60 when `None`), with buffers made
    /// afresh. A frame the GPU drew is no longer shown: the renderer's
    /// target is the old size, and has to be made again and adopted.
    ///
    /// # Errors
    ///
    /// A screen that has no such mode or cannot change, and whatever the
    /// card said; the screen is then as it was.
    fn resize(&mut self, size: (u32, u32), refresh: Option<f64>) -> io::Result<()> {
        let _ = (size, refresh);
        Err(io::Error::other("this screen has one size"))
    }

    /// Every mode the connector lists, as `(width, height, refresh in
    /// millihertz)`, in the connector's order: what `hyprctl monitors`
    /// prints as `availableModes`. The running mode alone for a screen that
    /// cannot say.
    fn modes(&self) -> Vec<(u32, u32, i32)> {
        let (width, height) = self.size();
        vec![(width, height, self.refresh_mhz())]
    }

    /// What to say about this backend in the compositor's log line.
    fn describe(&self) -> String;

    /// What the monitor on it is called: the connector's name, which
    /// `hyprctl monitors`, a `monitor =` line and `focusmonitor` all use.
    fn name(&self) -> String;

    /// What the monitor says it is: its make, model and serial with spaces
    /// between them, which is Hyprland's `m_shortDescription`.
    ///
    /// The thing `monitor = desc:...`, `hyprctl monitors`,
    /// `wl_output.description` and a bar's own `"output"` setting all match
    /// on. A connector's *name* moves when a cable does and a description
    /// does not, which is why a person writes the description.
    fn description(&self) -> String;

    /// The three parts of that description, for `hyprctl monitors`, which
    /// prints them apart as well as together.
    fn made(&self) -> (String, String, String) {
        (String::new(), String::new(), String::new())
    }

    /// Whether a monitor is still plugged into this screen's connector,
    /// asked after the card said its connectors changed: `None` for a
    /// screen with no connector, which never goes (`docs/DESKTOP.md` §3.2).
    fn connected(&self) -> Option<bool> {
        None
    }

    /// Which card this screen is on, as a number the screens of one card
    /// share, and the connector and CRTC it drives: what
    /// [`Backend::plugged_in`] is told of the screens already running.
    fn card_place(&self) -> Option<(i32, (u32, u32))> {
        None
    }

    /// Screens for the monitors plugged into this screen's card beside
    /// `held`, its running screens' connectors and CRTCs, each with the
    /// mode its `monitor =` line in `rules` asks where the connector lists
    /// it.
    fn plugged_in(
        &self,
        held: &[(u32, u32)],
        rules: &[compositor_config::MonitorRule],
    ) -> Vec<Box<dyn Backend>> {
        let _ = (held, rules);
        Vec::new()
    }

    /// The card this screen is on, kept open after the screen goes: what
    /// hears the monitor plugged back in when its screen was the card's
    /// last. `None` for a screen on no card, or on one that is lost.
    fn park(&self) -> Option<ParkedCard> {
        None
    }
}

/// A frame that has landed ([`Backend::landed`]).
#[derive(Debug)]
pub struct Landed {
    /// What the screen said: `Ok` for a frame shown. A card whose driver
    /// went has been lost by now ([`Backend::lost`]), and says `Ok`.
    pub result: io::Result<()>,
    /// When the frame was handed to the screen.
    pub began: std::time::Instant,
    /// When the screen answered: on a card that answers in its vertical
    /// blank, the blank.
    pub ended: std::time::Instant,
}

impl Landed {
    /// A frame's answer as the flipping thread gave it.
    fn from(answer: crate::flush::Answer, result: io::Result<()>) -> Self {
        Self {
            result,
            began: answer.began,
            ended: answer.ended,
        }
    }
}

/// A card with no screen running on it: its last monitor was unplugged.
/// It stays open, and is listened to, so that a monitor plugged back in is
/// found -- with no screen on the card nothing else read its events, and
/// the N150's TV plugged back in stayed dark.
#[derive(Debug)]
pub struct ParkedCard {
    card: std::rc::Rc<compositor_drm::Card>,
    /// Whether its connectors changed since this was last asked.
    changed: bool,
}

impl ParkedCard {
    /// The card's number, as [`Backend::card_place`] gives it.
    #[must_use]
    pub fn card(&self) -> i32 {
        self.card.raw_fd()
    }

    /// Its descriptor, for the compositor's wait.
    #[must_use]
    pub fn raw_fd(&self) -> std::os::fd::RawFd {
        self.card.raw_fd()
    }

    /// Read what the card said. `false` once the card is gone (its driver
    /// went), when it is no longer worth keeping.
    pub fn check(&mut self) -> bool {
        let news = self.card.news();
        self.changed |= news.connectors;
        !news.gone
    }

    /// Whether its connectors changed since this was last asked.
    pub fn changed(&mut self) -> bool {
        core::mem::take(&mut self.changed)
    }

    /// Screens for the monitors plugged into it now, as
    /// [`Backend::plugged_in`] makes them.
    #[cfg(target_os = "linux")]
    #[must_use]
    pub fn plugged_in(&self, rules: &[compositor_config::MonitorRule]) -> Vec<Box<dyn Backend>> {
        Drm::plugged_in_on(&self.card, &[], rules)
    }
}

/// A screen that is only memory: the everyday one, and the one a test reads.
#[derive(Debug)]
pub struct Headless {
    width: u32,
    height: u32,
    bytes: Vec<u8>,
    frames: u32,
    /// How many more frames it refuses to show.
    refusing: u32,
    /// What it is called: `HEADLESS-1`, or the name `output create
    /// headless` gave it.
    name: Option<String>,
    /// A card's flips, played: how long each takes on a flipping thread,
    /// and how long the first hangs before it times out.
    slow: Option<SlowFlips>,
}

/// A headless screen whose flips are a slow card's ([`Headless::slow`]).
#[derive(Debug)]
struct SlowFlips {
    /// How long a flip takes.
    takes: std::time::Duration,
    /// How long the next flip hangs before it says `ETIMEDOUT`, as a stuck
    /// driver's does after the kernel's five seconds.
    hangs: Option<std::time::Duration>,
    /// The thread they are made on, from the first.
    thread: Option<crate::flush::FlipThread>,
}

impl Headless {
    /// A `width` by `height` screen of memory.
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        let bytes = vec![0; width as usize * height as usize * 4];
        Self {
            width,
            height,
            bytes,
            frames: 0,
            refusing: 0,
            name: None,
            slow: None,
        }
    }

    /// The same screen, its flips a slow card's: each takes `takes` on a
    /// thread of its own, as the Intel driver's card0 answers in its
    /// vertical blank; and the first hangs `hangs`, if given, then times
    /// out, as a stuck driver's does. What a test of the loop going on
    /// while a flip is out asks for. Neither, and it flips as it always did.
    #[must_use]
    pub fn slow(
        mut self,
        takes: Option<std::time::Duration>,
        hangs: Option<std::time::Duration>,
    ) -> Self {
        self.slow = (takes.is_some() || hangs.is_some()).then(|| SlowFlips {
            takes: takes.unwrap_or_default(),
            hangs,
            thread: None,
        });
        self
    }

    /// Call it `name`, as `hyprctl output create headless NAME` does.
    pub fn call(&mut self, name: &str) {
        self.name = Some(name.to_owned());
    }

    /// The same screen, refusing its first `frames` frames as a card whose
    /// driver did not answer in time does.
    #[must_use]
    pub const fn refusing(mut self, frames: u32) -> Self {
        self.refusing = frames;
        self
    }

    /// How many frames have been shown.
    #[must_use]
    pub const fn frames(&self) -> u32 {
        self.frames
    }
}

impl Backend for Headless {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn buffer(&mut self) -> &mut [u8] {
        &mut self.bytes
    }

    fn drawn(&self) -> &[u8] {
        &self.bytes
    }

    fn stride(&self) -> u32 {
        self.width.saturating_mul(4)
    }

    fn present(&mut self, _drawn: &compositor_render::Damage) -> io::Result<()> {
        if self.refusing > 0 {
            self.refusing -= 1;
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the card did not answer in time",
            ));
        }
        if let Some(slow) = self.slow.as_mut() {
            let hangs = slow.hangs.take();
            let takes = slow.takes;
            let flip = move || {
                std::thread::sleep(hangs.unwrap_or(takes));
                match hangs {
                    Some(_) => Err(io::Error::from_raw_os_error(libc::ETIMEDOUT)),
                    None => Ok(()),
                }
            };
            if !crate::knobs::sync_flip() && slow.thread.is_none() {
                slow.thread = crate::flush::FlipThread::start().ok();
            }
            match slow.thread.as_mut() {
                Some(thread) if !crate::knobs::sync_flip() => {
                    thread.send(Box::new(flip));
                    return Ok(());
                }
                _ => flip()?,
            }
        }
        self.frames = self.frames.saturating_add(1);
        Ok(())
    }

    fn in_flight(&self) -> bool {
        self.slow
            .as_ref()
            .and_then(|slow| slow.thread.as_ref())
            .is_some_and(crate::flush::FlipThread::out)
    }

    fn flight_fd(&self) -> Option<std::os::fd::RawFd> {
        self.slow
            .as_ref()
            .and_then(|slow| slow.thread.as_ref())
            .filter(|thread| thread.out())
            .map(crate::flush::FlipThread::raw_fd)
    }

    fn landed(&mut self) -> Option<Landed> {
        let mut answer = self.slow.as_mut()?.thread.as_mut()?.answer()?;
        let result = core::mem::replace(&mut answer.result, Ok(()));
        if result.is_ok() {
            self.frames = self.frames.saturating_add(1);
        }
        Some(Landed::from(answer, result))
    }

    fn describe(&self) -> String {
        format!("headless {}x{}", self.width, self.height)
    }

    fn name(&self) -> String {
        // What Hyprland's own headless backend calls its output.
        self.name.clone().unwrap_or_else(|| "HEADLESS-1".to_owned())
    }

    fn description(&self) -> String {
        // Hyprland's headless backend gives its outputs this description,
        // and a configuration written for one matches on it.
        "Headless output 1".to_owned()
    }
}

/// A mode a connector lists: width, height and refresh in millihertz.
pub type ListedMode = (u32, u32, i32);

/// The mode a `monitor =` line's resolution field picks among `listed`,
/// `(width, height, refresh in millihertz)`: a size and refresh to take,
/// or `None` for `preferred`, which is the connector's to say. `highres` is
/// the most pixels, the fastest of them; `highrr` the fastest, the largest
/// of them (Hyprland's `CMonitor::applyMonitorRule`).
#[must_use]
pub fn pick_mode(
    mode: compositor_config::Mode,
    listed: &[ListedMode],
) -> Option<((u32, u32), Option<f64>)> {
    let chosen = |key: &dyn Fn(&ListedMode) -> (u64, i64)| {
        listed
            .iter()
            .max_by_key(|mode| key(mode))
            .map(|&(width, height, mhz)| ((width, height), Some(f64::from(mhz) / 1000.0)))
    };
    match mode {
        compositor_config::Mode::Preferred => None,
        compositor_config::Mode::Fixed {
            width,
            height,
            refresh,
        } => Some(((width, height), refresh)),
        compositor_config::Mode::HighRes => {
            chosen(&|&(width, height, mhz)| (u64::from(width) * u64::from(height), i64::from(mhz)))
        }
        compositor_config::Mode::HighRr => chosen(&|&(width, height, mhz)| {
            (
                u64::try_from(mhz).unwrap_or(0),
                i64::from(width) * i64::from(height),
            )
        }),
    }
}

/// The screen: `/dev/dri/card0`, through the legacy mode-setting calls.
///
/// Two dumb buffers, drawn into in turn and shown with a page flip. One
/// buffer would work and would tear: the card scans out of the same memory
/// the compositor is writing, so half a frame is on screen while the other
/// half is being drawn.
///
/// Unless the card does not scan out of that memory at all. A virtio-gpu's
/// host holds a copy of the buffer and shows the copy, which changes only
/// when the guest says what to bring up to date -- so nothing tears, and a
/// flip is the expensive way to say it: the scanout is set again and the
/// *whole* buffer sent, whatever the frame changed. On that card the screen
/// is one buffer and a frame is `DRM_IOCTL_MODE_DIRTYFB` over its damage:
/// a pointer's frame sends a pointer's worth of pixels to the host rather
/// than two million. Linux's own `virtio_gpu` answers the call the same way.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub struct Drm {
    /// The card, shared with the other screens on it: only one open of a
    /// card is allowed, and a card with two connectors is two screens.
    card: std::rc::Rc<compositor_drm::Card>,
    plan: compositor_drm::Plan,
    buffers: [compositor_drm::Dumb; 2],
    /// Which buffer is being drawn into.
    back: usize,
    /// Whether the card shows a copy of the buffer it is told to bring up
    /// to date, so that the first buffer is the only one drawn into.
    copied: bool,
    /// What the GPU drew, once a renderer has handed it over: the screen is
    /// pointed at it and the buffers above are never shown again.
    adopted: Option<compositor_drm::Imported>,
    width: u32,
    height: u32,
    frames: u32,
    /// When the card went away, or when it was last looked for since: `None`
    /// while the screen is there.
    lost: Option<std::time::Instant>,
    /// The size of image the card's cursor plane shows, if it has one.
    cursor_size: Option<(u32, u32)>,
    /// The cursor plane's two images, each made the first time it is
    /// needed, and which the next image is drawn into.
    ///
    /// Two, because a card may show the plane from the buffer itself: the
    /// DK1's LTDC scans its second layer straight out of it, where
    /// virtio-gpu's host takes a copy. Drawn into while shown, the pointer
    /// would be half the old image and half the new, at the old hotspot's
    /// place, until the card was told; drawn into the other one, the image
    /// and its place change together at the card's next frame.
    cursors: [Option<compositor_drm::Dumb>; 2],
    next_cursor: usize,
    /// Whether the card said its connectors changed and nobody has asked
    /// [`Backend::modes_changed`] since.
    modes_changed: bool,
    /// The thread that flushes what the GPU drew, made at the first such
    /// flush (`crate::flush`): the loop does not wait for the host to show
    /// a frame.
    flushing: Option<crate::flush::FlushThread>,
    /// Clients' own buffers the card has a framebuffer for, to show them
    /// directly ([`Backend::scan_out`]): a game's swap chain, at most
    /// [`DIRECT_KEPT`].
    direct: Vec<Direct>,
    /// The handle of the one of them the next present shows.
    direct_next: Option<u32>,
    /// The handle of the one the card shows now.
    direct_shown: Option<u32>,
    /// Why the card would not take a client's buffer, once it would not:
    /// asked no more, since every frame would be refused the same way.
    direct_refused: Option<String>,
    /// The thread page flips and dirty flushes are made on, and the
    /// descriptor they are made with, made at the first
    /// (`crate::flush::FlipThread`): the loop does not wait for the card.
    flipper: Option<(
        crate::flush::FlipThread,
        std::sync::Arc<compositor_drm::Flusher>,
    )>,
    /// What the frame out on it does once the card has shown it.
    flight: Option<Flight>,
    /// A frame's answer taken before the loop asked for it -- a change of
    /// mode waits for the frame out first -- kept for [`Backend::landed`].
    settled: Option<Landed>,
    /// What the cursor plane was asked for while a frame was out, made once
    /// it lands: the kernel holds the card for a flip until its driver
    /// answers, and a cursor call meanwhile would wait that out on the
    /// loop.
    cursor_due: Option<CursorDue>,
}

/// What a frame handed to the card does once it is shown.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy)]
enum Flight {
    /// One of the screen's own buffers: drawn into next is the other, when
    /// `flipped`; and the client's buffer shown before it is shown no more,
    /// when `leaving_direct`.
    Own { flipped: bool, leaving_direct: bool },
    /// The client's buffer this handle names.
    Direct(u32),
}

/// The call that shows a frame, made on the flipping thread or on the loop.
#[cfg(target_os = "linux")]
enum Call {
    /// `PAGE_FLIP` to this framebuffer on this CRTC.
    Flip { crtc: u32, framebuffer: u32 },
    /// `DIRTYFB` of these rectangles of this framebuffer.
    Dirty {
        framebuffer: u32,
        clips: Vec<(u32, u32, u32, u32)>,
    },
}

#[cfg(target_os = "linux")]
impl Call {
    /// Make it on `card`, the flipping thread's descriptor.
    fn on(self, card: &compositor_drm::Flusher) -> io::Result<()> {
        match self {
            Self::Flip { crtc, framebuffer } => card.page_flip(crtc, framebuffer),
            Self::Dirty { framebuffer, clips } => card.dirty(framebuffer, &clips),
        }
    }
}

/// A framebuffer by its id, for a call made on the loop.
#[cfg(target_os = "linux")]
struct Framebuffer(u32);

#[cfg(target_os = "linux")]
impl compositor_drm::Shown for Framebuffer {
    fn framebuffer(&self) -> u32 {
        self.0
    }
}

/// A cursor plane call put off until the frame out has landed.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy)]
enum CursorDue {
    /// Show the image drawn into the next of the plane's buffers, its
    /// hotspot at the first and its corner at the second.
    Image((i32, i32), (i32, i32)),
    /// Move the image shown to here.
    Move((i32, i32)),
}

/// A client's buffer the card can show: its framebuffer, and its pixels
/// mapped for a screenshot of a frame that was it.
#[cfg(target_os = "linux")]
struct Direct {
    imported: compositor_drm::Imported,
    mapping: compositor_drm::Mapping,
    size: (u32, u32),
    stride: u32,
    /// The frame it was last asked for in.
    used: u32,
}

#[cfg(target_os = "linux")]
impl core::fmt::Debug for Direct {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("Direct")
            .field("imported", &self.imported)
            .field("size", &self.size)
            .field("used", &self.used)
            .finish_non_exhaustive()
    }
}

/// How many clients' buffers a screen keeps framebuffers for: a swap
/// chain's three or four, each a buffer of its card driver's too.
#[cfg(target_os = "linux")]
const DIRECT_KEPT: usize = 4;

/// Frames drawn by the compositor after which the clients' buffers it kept
/// are let go of: a game that shows a menu for a second keeps its
/// framebuffers, one that has gone gives them back.
#[cfg(target_os = "linux")]
const DIRECT_IDLE_FRAMES: u32 = 120;

/// How often a lost screen's card is looked for.
#[cfg(target_os = "linux")]
const RETRY: std::time::Duration = std::time::Duration::from_millis(250);

/// Whether `error` is the card saying its driver has gone.
#[cfg(target_os = "linux")]
fn gone(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::ENODEV)
}

#[cfg(target_os = "linux")]
impl Drm {
    /// Open the card, set its preferred mode and make the buffers.
    ///
    /// # Errors
    ///
    /// Whatever the card said. A compositor with no screen is the one thing
    /// it cannot do without, so this is fatal.
    pub fn open() -> io::Result<Self> {
        let card = std::rc::Rc::new(compositor_drm::Card::open()?);
        let plan = compositor_drm::plan(&card)?;
        Self::on(card, plan)
    }

    /// Every screen the machine has: each connected connector of each card,
    /// in card and connector order.
    ///
    /// A machine with two monitors has them on one card's two connectors or
    /// on a card each, and both are two screens here. The order is the
    /// order the kernel lists them in, which is what decides which monitor
    /// is the first.
    ///
    /// # Errors
    ///
    /// Nothing: a card that will not open or a connector that will not take
    /// a mode is left out, and a machine with no screen at all is the
    /// caller's to report.
    ///
    /// `rules` are the configuration's `monitor =` lines. One that names a
    /// resolution has it taken before the mode is set, where the connector
    /// lists it; the rest of what a rule says -- where the monitor goes, its
    /// scale, whether it is used -- is `Screen::all`'s.
    pub fn open_all(rules: &[compositor_config::MonitorRule]) -> Vec<Self> {
        let mut cards = Vec::new();
        let mut plans = Vec::new();
        for card in compositor_drm::cards() {
            let card = std::rc::Rc::new(card);
            let Ok(found) = compositor_drm::plans(&card) else {
                continue;
            };
            for plan in found {
                cards.push(std::rc::Rc::clone(&card));
                plans.push(plan);
            }
        }
        // The names are numbered across every card, so two cards with a
        // `Virtual-1` each become `Virtual-1` and `Virtual-2`.
        compositor_drm::rename(&mut plans);
        let mut screens = Vec::new();
        for (card, mut plan) in cards.into_iter().zip(plans) {
            let description = plan
                .edid
                .as_ref()
                .map(|edid| edid.describe(compositor_drm::registered))
                .unwrap_or_default();
            // The rule for this monitor, as everywhere else.
            let rule = compositor_config::MonitorRule::for_monitor(rules, &plan.name, &description);
            let listed: Vec<(u32, u32, i32)> = plan
                .modes
                .iter()
                .map(|mode| {
                    (
                        u32::from(mode.hdisplay),
                        u32::from(mode.vdisplay),
                        compositor_drm::modeset::refresh_mhz(mode),
                    )
                })
                .collect();
            if let Some((size, refresh)) = rule.and_then(|rule| pick_mode(rule.mode, &listed)) {
                let _ = plan.take(size, refresh);
            }
            if let Ok(screen) = Self::on(card, plan) {
                screens.push(screen);
            }
        }
        screens
    }

    /// Screens for the monitors plugged into `card` beside `held`, its
    /// running screens' connectors and CRTCs, each at the mode its
    /// `monitor =` line in `rules` asks where the connector lists it.
    fn plugged_in_on(
        card: &std::rc::Rc<compositor_drm::Card>,
        held: &[(u32, u32)],
        rules: &[compositor_config::MonitorRule],
    ) -> Vec<Box<dyn Backend>> {
        let Ok(plans) = compositor_drm::plans_beside(card, held) else {
            return Vec::new();
        };
        let mut screens: Vec<Box<dyn Backend>> = Vec::new();
        for mut plan in plans {
            let description = plan
                .edid
                .as_ref()
                .map(|edid| edid.describe(compositor_drm::registered))
                .unwrap_or_default();
            let rule = compositor_config::MonitorRule::for_monitor(rules, &plan.name, &description);
            let listed: Vec<(u32, u32, i32)> = plan
                .modes
                .iter()
                .map(|mode| {
                    (
                        u32::from(mode.hdisplay),
                        u32::from(mode.vdisplay),
                        compositor_drm::modeset::refresh_mhz(mode),
                    )
                })
                .collect();
            if let Some((size, refresh)) = rule.and_then(|rule| pick_mode(rule.mode, &listed)) {
                let _ = plan.take(size, refresh);
            }
            if let Ok(screen) = Self::on(std::rc::Rc::clone(card), plan) {
                screens.push(Box::new(screen));
            }
        }
        screens
    }

    /// One screen: two buffers on `plan`'s connector, with its mode set.
    fn on(card: std::rc::Rc<compositor_drm::Card>, plan: compositor_drm::Plan) -> io::Result<Self> {
        let (width, height) = plan.size();
        let first = compositor_drm::Dumb::new(&card, width, height)?;
        let second = compositor_drm::Dumb::new(&card, width, height)?;
        // A page flip needs a mode already set, so the first buffer is shown
        // the long way round.
        let _ = card.set_mode(&plan, &first)?;
        // A card that will not say what it is is drawn on the way that is
        // right for every card.
        let copied = card.driver().is_ok_and(|driver| driver == "virtio_gpu");
        let cursor_size = card.cursor_size();
        Ok(Self {
            card,
            plan,
            adopted: None,
            buffers: [first, second],
            back: usize::from(!copied),
            copied,
            width,
            height,
            frames: 0,
            lost: None,
            cursor_size,
            cursors: [None, None],
            next_cursor: 0,
            modes_changed: false,
            flushing: None,
            direct: Vec::new(),
            direct_next: None,
            direct_shown: None,
            direct_refused: None,
            flipper: None,
            flight: None,
            settled: None,
            cursor_due: None,
        })
    }

    /// Let go of the clients' buffers kept to be shown directly that the
    /// card does not show, or all of them when `all` -- once the card shows
    /// a buffer of this screen's own, since a framebuffer that goes takes
    /// the screen with it.
    ///
    /// Never the one a frame out is flipping to: the card may show it yet.
    fn let_go_direct(&mut self, all: bool) {
        let shown = if all { None } else { self.direct_shown };
        let flying = self.flying_direct();
        let mut kept = Vec::new();
        for direct in self.direct.drain(..) {
            let handle = Some(direct.imported.handle());
            if handle == shown || handle == flying {
                kept.push(direct);
            } else {
                let _ = direct.imported.release(&self.card);
            }
        }
        self.direct = kept;
        self.direct_next = None;
    }

    /// Flip to the client's buffer `handle` names.
    fn show_direct(&mut self, handle: u32) -> io::Result<()> {
        let Some(direct) = self
            .direct
            .iter()
            .find(|direct| direct.imported.handle() == handle)
        else {
            return Err(io::Error::other("a client's buffer that is not kept"));
        };
        let call = Call::Flip {
            crtc: self.plan.crtc,
            framebuffer: compositor_drm::Shown::framebuffer(&direct.imported),
        };
        self.send(Flight::Direct(handle), call)
    }

    /// The handle of the client's buffer a frame out is flipping to.
    fn flying_direct(&self) -> Option<u32> {
        match self.flight {
            Some(Flight::Direct(handle)) => Some(handle),
            _ => None,
        }
    }

    /// Show a frame by `call`, and do what `flight` says once it is shown:
    /// on the flipping thread, returning at once, unless
    /// `HYPRIX_SYNC_FLIP=1` or the thread cannot be had, when the call is
    /// made here and waited for as it always was.
    fn send(&mut self, flight: Flight, call: Call) -> io::Result<()> {
        // Never two out: the gate in the loop draws nothing while one is,
        // and anything else that shows a frame waits for it first.
        self.settle();
        if !crate::knobs::sync_flip() && self.flipper.is_none() {
            self.flipper = self.card.flusher().ok().and_then(|card| {
                crate::flush::FlipThread::start()
                    .ok()
                    .map(|thread| (thread, std::sync::Arc::new(card)))
            });
        }
        match self.flipper.as_mut() {
            Some((thread, card)) if !crate::knobs::sync_flip() => {
                let card = std::sync::Arc::clone(card);
                thread.send(Box::new(move || call.on(&card)));
                self.flight = Some(flight);
                Ok(())
            }
            _ => {
                let result = match call {
                    Call::Flip { framebuffer, .. } => {
                        self.card.page_flip(&self.plan, &Framebuffer(framebuffer))
                    }
                    Call::Dirty { framebuffer, clips } => {
                        self.card.dirty(&Framebuffer(framebuffer), &clips)
                    }
                };
                self.land(flight, result)
            }
        }
    }

    /// What follows a frame the card answered `result` for: the buffer
    /// drawn into next, the client's buffer it shows, the frame counted --
    /// or, for a card whose driver went, the screen lost, which is no
    /// error of the frame's.
    fn land(&mut self, flight: Flight, result: io::Result<()>) -> io::Result<()> {
        match result {
            Ok(()) => {
                match flight {
                    Flight::Own {
                        flipped,
                        leaving_direct,
                    } => {
                        if leaving_direct {
                            self.direct_shown = None;
                        }
                        if flipped {
                            self.back = 1 - self.back;
                        }
                    }
                    Flight::Direct(handle) => self.direct_shown = Some(handle),
                }
                self.frames = self.frames.saturating_add(1);
                Ok(())
            }
            // The driver died. The compositor, its clients and the frame
            // just drawn are all still good; only the card is not, until
            // one is started again.
            Err(error) if gone(&error) => {
                self.lose();
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// The frame out, landed, if its answer has come; or waited for when
    /// `wait`. Then the cursor plane's calls put off meanwhile are made.
    fn take_flight(&mut self, wait: bool) -> Option<Landed> {
        let flight = self.flight?;
        let (thread, _) = self.flipper.as_mut()?;
        let mut answer = if wait {
            thread.wait()?
        } else {
            thread.answer()?
        };
        self.flight = None;
        let result = core::mem::replace(&mut answer.result, Ok(()));
        let result = self.land(flight, result);
        if self.lost.is_none()
            && let Some(due) = self.cursor_due.take()
        {
            let _ = match due {
                CursorDue::Image(hot, at) => self.show_cursor(hot, at),
                CursorDue::Move(at) => self.move_cursor(at),
            };
        }
        Some(Landed::from(answer, result))
    }

    /// Wait for the frame out, if one is, before a change that cannot be
    /// made under it -- a new mode, other buffers, another frame -- and
    /// keep its answer for the loop.
    fn settle(&mut self) {
        if let Some(landed) = self.take_flight(true) {
            self.settled = Some(landed);
        }
    }

    /// This screen's connector as the card describes it now, named as it
    /// was: `rename` numbered it among every card's.
    fn replanned(&self) -> io::Result<compositor_drm::Plan> {
        let mut plan = compositor_drm::plans(&self.card)?
            .into_iter()
            .find(|plan| plan.connector == self.plan.connector)
            .ok_or_else(|| io::Error::other("the connector is not there any more"))?;
        plan.name.clone_from(&self.plan.name);
        Ok(plan)
    }

    /// The screen again, on a card opened afresh: the same connector, at the
    /// same size, so nothing laid out on it moves. `None` while the card is
    /// not there yet, or is there with no such connector or mode.
    fn reopened(&self) -> Option<Self> {
        let index: u32 = self.card.name().strip_prefix("card")?.parse().ok()?;
        // Not asked for until the node is there: under sessiond each open
        // is a request it answers, and a card whose driver is down would
        // be asked for four times a second until it is back.
        if !Path::new(&format!("/dev/dri/card{index}")).exists() {
            return None;
        }
        let card = std::rc::Rc::new(compositor_drm::Card::open_index(index).ok()?);
        let mut plans = compositor_drm::plans(&card).ok()?;
        let at = plans
            .iter()
            .position(|plan| plan.connector == self.plan.connector)
            .unwrap_or(0);
        let mut plan = (at < plans.len()).then(|| plans.swap_remove(at))?;
        let _ = plan.take((self.width, self.height), None);
        if plan.size() != (self.width, self.height) {
            return None;
        }
        // Named as it was: `rename` numbered it among every card's.
        plan.name.clone_from(&self.plan.name);
        let mut screen = Self::on(card, plan).ok()?;
        screen.frames = self.frames;
        Some(screen)
    }

    /// A cursor call's result, with a refusal taken to mean the card has no
    /// plane after all: asked again every pass, it would refuse every pass.
    fn refused(&mut self, result: io::Result<()>) -> io::Result<()> {
        if result.is_err() {
            self.cursor_size = None;
        }
        result
    }

    /// Show the image drawn into the cursor plane's next buffer, its
    /// hotspot at `hot` and its corner at `at`.
    fn show_cursor(&mut self, hot: (i32, i32), at: (i32, i32)) -> io::Result<()> {
        let shown = self.cursors.get(self.next_cursor).and_then(Option::as_ref);
        let result = self.card.set_cursor(&self.plan, shown, hot, at);
        // The card now shows this one, and the call returned once it did:
        // the other is no longer read, and takes the next image.
        if result.is_ok() {
            self.next_cursor ^= 1;
        }
        match result {
            Err(error) if gone(&error) => {
                self.lose();
                Ok(())
            }
            result => self.refused(result),
        }
    }

    /// Note that the card went away, for [`Backend::recover`] to look for it
    /// from the next frame on.
    fn lose(&mut self) {
        self.flushing = None;
        // A flip still out on the card that went answers nobody: the
        // thread is let go of, and ends when its call does.
        self.flipper = None;
        self.flight = None;
        self.cursor_due = None;
        // The card's handles went with its driver's open.
        self.direct.clear();
        self.direct_next = None;
        self.direct_shown = None;
        self.lost = Some(
            std::time::Instant::now()
                .checked_sub(RETRY)
                .unwrap_or_else(std::time::Instant::now),
        );
    }

    /// How many frames have been shown.
    #[must_use]
    pub const fn frames(&self) -> u32 {
        self.frames
    }
}

#[cfg(target_os = "linux")]
impl Backend for Drm {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The running mode's: its pixel clock over its total pixels, which is
    /// exact where `vrefresh` is rounded (59.94 Hz is 60 there), or
    /// `vrefresh` for a mode without timings.
    fn refresh_mhz(&self) -> i32 {
        compositor_drm::modeset::refresh_mhz(&self.plan.mode)
    }

    fn buffer(&mut self) -> &mut [u8] {
        match self.buffers.get_mut(self.back) {
            Some(buffer) => buffer.pixels(),
            // `back` is 0 or 1 and the array has two; an empty slice would
            // draw nothing rather than draw somewhere else.
            None => &mut [],
        }
    }

    fn drawn(&self) -> &[u8] {
        // A client's own buffer, while the card shows one.
        if let Some(direct) = self
            .direct_shown
            .and_then(|handle| self.direct.iter().find(|d| d.imported.handle() == handle))
        {
            return direct.mapping.bytes();
        }
        // The buffer just drawn into, which `present` has not yet swapped
        // away from.
        match self.buffers.get(self.back) {
            Some(buffer) => buffer.shown(),
            None => &[],
        }
    }

    fn stride(&self) -> u32 {
        self.buffers
            .get(self.back)
            .map_or_else(|| self.width.saturating_mul(4), |buffer| buffer.pitch)
    }

    fn drawn_stride(&self) -> u32 {
        self.direct_shown
            .and_then(|handle| self.direct.iter().find(|d| d.imported.handle() == handle))
            .map_or_else(|| self.stride(), |direct| direct.stride)
    }

    fn scan_out(&mut self, buffer: &Scanout<'_>) -> io::Result<()> {
        if self.lost.is_some() || self.adopted.is_some() {
            return Err(io::Error::other("the screen shows what the GPU drew"));
        }
        if let Some(why) = self.direct_refused.as_ref() {
            return Err(io::Error::other(why.clone()));
        }
        if (buffer.width, buffer.height) != (self.width, self.height) {
            return Err(io::Error::other("not the screen's size"));
        }
        let handle = match compositor_drm::Imported::handle_of(&self.card, buffer.fd) {
            Ok(handle) => handle,
            // The card's word for a buffer it will not show: one whose
            // driver does not copy, or a kernel without the import.
            Err(error) if error.raw_os_error() == Some(libc::EINVAL) => {
                let why = format!("the card will not show a client's buffer ({error})");
                self.direct_refused = Some(why.clone());
                return Err(io::Error::other(why));
            }
            Err(error) => return Err(error),
        };
        let frame = self.frames;
        if let Some(direct) = self
            .direct
            .iter_mut()
            .find(|direct| direct.imported.handle() == handle)
        {
            if direct.size != (buffer.width, buffer.height) || direct.stride != buffer.stride {
                return Err(io::Error::other("a kept buffer of another shape"));
            }
            direct.used = frame;
            self.direct_next = Some(handle);
            return Ok(());
        }
        let imported = match compositor_drm::Imported::with_handle(
            &self.card,
            handle,
            buffer.width,
            buffer.height,
            buffer.stride,
        ) {
            Ok(imported) => imported,
            Err(error) => {
                let _ = compositor_drm::Imported::forget_handle(&self.card, handle);
                return Err(error);
            }
        };
        let mapping = match compositor_drm::Mapping::of_dmabuf(buffer.fd) {
            Ok(mapping) => mapping,
            Err(error) => {
                let _ = imported.release(&self.card);
                return Err(error);
            }
        };
        // Room for it: the longest unused that the card is not showing, nor
        // about to.
        let flying = self.flying_direct();
        while self.direct.len() >= DIRECT_KEPT {
            let Some(at) = self
                .direct
                .iter()
                .enumerate()
                .filter(|(_, direct)| {
                    let handle = Some(direct.imported.handle());
                    handle != self.direct_shown && handle != flying
                })
                .min_by_key(|(_, direct)| direct.used)
                .map(|(at, _)| at)
            else {
                break;
            };
            let _ = self.direct.remove(at).imported.release(&self.card);
        }
        self.direct.push(Direct {
            imported,
            mapping,
            size: (buffer.width, buffer.height),
            stride: buffer.stride,
            used: frame,
        });
        self.direct_next = Some(handle);
        Ok(())
    }

    fn scanning_out(&self) -> bool {
        self.direct_shown.is_some()
    }

    fn adopt(
        &mut self,
        fd: std::os::fd::BorrowedFd<'_>,
        width: u32,
        height: u32,
        pitch: u32,
    ) -> io::Result<()> {
        self.settle();
        let imported = compositor_drm::Imported::new(&self.card, fd, width, height, pitch)?;
        // Shown the long way round, as the first frame is: a page flip needs
        // a mode already set, and the mode is set to another buffer now.
        let _ = self.card.set_mode(&self.plan, &imported)?;
        self.adopted = Some(imported);
        Ok(())
    }

    fn adopted(&self) -> bool {
        self.adopted.is_some()
    }

    fn forsake(&mut self) -> io::Result<()> {
        if self.adopted.is_none() {
            return Ok(());
        }
        self.settle();
        // The buffer the next frame is not drawn into: one buffer copied
        // into is the only one, and of two flipped between, the front.
        let front = if self.copied {
            self.back
        } else {
            1 - self.back
        };
        let Some(buffer) = self.buffers.get(front) else {
            return Ok(());
        };
        // Shown before the adopted one is let go, as `resize` does.
        let _ = self.card.set_mode(&self.plan, buffer)?;
        self.flushing = None;
        self.adopted = None;
        Ok(())
    }

    fn cursor_plane(&self) -> Option<(u32, u32)> {
        self.cursor_size.filter(|_| self.lost.is_none())
    }

    fn set_cursor(&mut self, image: &[u8], hot: (i32, i32), at: (i32, i32)) -> io::Result<()> {
        if self.lost.is_some() {
            return Ok(());
        }
        let (width, height) = self
            .cursor_size
            .ok_or_else(|| io::Error::other("this card has no cursor plane"))?;
        let Some(slot) = self.cursors.get_mut(self.next_cursor) else {
            return Ok(());
        };
        if slot.is_none() {
            *slot = Some(compositor_drm::Dumb::new(&self.card, width, height)?);
        }
        let Some(buffer) = slot.as_mut() else {
            return Ok(());
        };
        let pitch = buffer.pitch as usize;
        let row = width as usize * 4;
        let pixels = buffer.pixels();
        for (y, from) in image.chunks(row).take(height as usize).enumerate() {
            if let Some(to) = pixels.get_mut(y * pitch..y * pitch + from.len()) {
                to.copy_from_slice(from);
            }
        }
        if self.flight.is_some() {
            self.cursor_due = Some(CursorDue::Image(hot, at));
            return Ok(());
        }
        self.show_cursor(hot, at)
    }

    fn move_cursor(&mut self, at: (i32, i32)) -> io::Result<()> {
        if self.lost.is_some() {
            return Ok(());
        }
        if self.flight.is_some() {
            self.cursor_due = Some(match self.cursor_due {
                Some(CursorDue::Image(hot, _)) => CursorDue::Image(hot, at),
                _ => CursorDue::Move(at),
            });
            return Ok(());
        }
        match self.card.move_cursor(&self.plan, at) {
            Err(error) if gone(&error) => {
                self.lose();
                Ok(())
            }
            result => self.refused(result),
        }
    }

    fn present(&mut self, drawn: &compositor_render::Damage) -> io::Result<()> {
        if self.lost.is_some() {
            return Ok(());
        }
        // The loop draws nothing while a frame is out; anything else waits
        // for it here, since the buffer about to be shown may be its.
        self.settle();
        // A client's own buffer, shown directly: the card copies it, or
        // points its plane at it, and nothing of this screen's buffers is
        // read.
        if let Some(handle) = self.direct_next.take() {
            return self.show_direct(handle);
        }
        // Back from a client's buffer to this screen's own: a flip, which
        // the frame drawn whole into it is shown by, then the clients'
        // buffers let go of once none is shown.
        if self.direct_shown.is_some() && self.adopted.is_none() {
            let Some(buffer) = self.buffers.get(self.back) else {
                return Ok(());
            };
            let call = Call::Flip {
                crtc: self.plan.crtc,
                framebuffer: buffer.framebuffer,
            };
            let flight = Flight::Own {
                flipped: !self.copied,
                leaving_direct: true,
            };
            return self.send(flight, call);
        }
        if !self.direct.is_empty()
            && self
                .direct
                .iter()
                .all(|direct| self.frames.wrapping_sub(direct.used) > DIRECT_IDLE_FRAMES)
        {
            self.let_go_direct(true);
        }
        // What the GPU drew is one texture, shown where it lies: there is no
        // second buffer to flip to and nothing to copy into it, so every
        // frame is a dirty rectangle -- which on this card costs the host
        // being told what changed and not one pixel.
        let shown: &dyn compositor_drm::Shown = match self.adopted.as_ref() {
            Some(imported) => imported,
            None => match self.buffers.get(self.back) {
                Some(buffer) => buffer,
                None => return Ok(()),
            },
        };
        let framebuffer = shown.framebuffer();
        let flipped = !(self.copied || self.adopted.is_some());
        let flight = Flight::Own {
            flipped,
            leaving_direct: false,
        };
        if flipped {
            let call = Call::Flip {
                crtc: self.plan.crtc,
                framebuffer,
            };
            return self.send(flight, call);
        }
        let edge = |value: i64| u32::try_from(value.max(0)).unwrap_or(u32::MAX);
        let clips: Vec<(u32, u32, u32, u32)> = drawn
            .rects()
            .iter()
            .map(|rect| {
                (
                    edge(rect.x),
                    edge(rect.y),
                    edge(rect.width),
                    edge(rect.height),
                )
            })
            .collect();
        // A frame that drew nothing has nothing to send, and an empty
        // list would say "all of it".
        if clips.is_empty() {
            return self.land(flight, Ok(()));
        }
        if self.adopted.is_none() {
            // A copied dumb buffer: the next frame is drawn into it, so it
            // is a frame out like a flip until the host has copied it.
            return self.send(flight, Call::Dirty { framebuffer, clips });
        }
        // What the GPU drew is flushed by a thread of its own, the loop
        // going on meanwhile and nothing waiting for it (`crate::flush`):
        // the drawing and the flush reach the host in order on one queue,
        // so the next frame's drawing cannot tear this one.
        if self.flushing.is_none() {
            self.flushing = self
                .card
                .flusher()
                .and_then(crate::flush::FlushThread::start)
                .ok();
        }
        let result = match self.flushing.as_ref() {
            Some(thread) if thread.gone() => Err(io::Error::from_raw_os_error(libc::ENODEV)),
            Some(thread) => {
                thread.flush(framebuffer, &clips);
                match thread.refused() {
                    Some(refusal) => Err(io::Error::other(refusal)),
                    None => Ok(()),
                }
            }
            None => self.card.dirty(&Framebuffer(framebuffer), &clips),
        };
        self.land(flight, result)
    }

    fn in_flight(&self) -> bool {
        self.flight.is_some()
    }

    fn flight_fd(&self) -> Option<std::os::fd::RawFd> {
        self.flight
            .and(self.flipper.as_ref())
            .map(|(thread, _)| thread.raw_fd())
    }

    fn landed(&mut self) -> Option<Landed> {
        self.settled.take().or_else(|| self.take_flight(false))
    }

    fn lost(&self) -> bool {
        self.lost.is_some()
    }

    fn raw_fd(&self) -> Option<std::os::fd::RawFd> {
        // A lost card's descriptor is readable for good, and a wait on it
        // would never sleep: while lost, the frame loop looks for it instead.
        self.lost.is_none().then(|| self.card.raw_fd())
    }

    fn check(&mut self) -> bool {
        if self.lost.is_some() {
            return false;
        }
        let news = self.card.news();
        self.modes_changed |= news.connectors;
        // What `hyprctl monitors` lists follows the connector.
        if news.connectors
            && let Ok(plan) = self.replanned()
        {
            self.plan.modes = plan.modes;
        }
        if !news.gone {
            return false;
        }
        self.lose();
        true
    }

    fn modes_changed(&mut self) -> bool {
        core::mem::take(&mut self.modes_changed)
    }

    fn preferred(&self) -> Option<(u32, u32)> {
        self.replanned().ok().map(|plan| plan.size())
    }

    fn preferred_refresh(&self) -> Option<f64> {
        self.replanned()
            .ok()
            .map(|plan| f64::from(compositor_drm::modeset::refresh_mhz(&plan.mode)) / 1000.0)
    }

    fn modes(&self) -> Vec<(u32, u32, i32)> {
        self.plan
            .modes
            .iter()
            .map(|mode| {
                (
                    u32::from(mode.hdisplay),
                    u32::from(mode.vdisplay),
                    compositor_drm::modeset::refresh_mhz(mode),
                )
            })
            .collect()
    }

    fn resize(&mut self, (width, height): (u32, u32), refresh: Option<f64>) -> io::Result<()> {
        let mut plan = self.replanned()?;
        if !plan.take((width, height), refresh) || plan.size() != (width, height) {
            return Err(io::Error::other(format!(
                "the connector has no {width}x{height} mode"
            )));
        }
        if plan.mode == self.plan.mode {
            // The mode it runs: nothing to set.
            self.plan.modes = plan.modes;
            return Ok(());
        }
        // The frame out flips to a buffer about to be let go of.
        self.settle();
        let first = compositor_drm::Dumb::new(&self.card, width, height)?;
        let second = compositor_drm::Dumb::new(&self.card, width, height)?;
        // Shown before anything old is let go: the card never points at a
        // buffer that is gone, the GPU's adopted one included.
        let _ = self.card.set_mode(&plan, &first)?;
        self.adopted = None;
        self.direct_shown = None;
        self.let_go_direct(true);
        self.buffers = [first, second];
        self.back = usize::from(!self.copied);
        self.plan = plan;
        self.width = width;
        self.height = height;
        Ok(())
    }

    fn recover(&mut self) -> bool {
        let Some(tried) = self.lost else {
            return true;
        };
        if tried.elapsed() < RETRY {
            return false;
        }
        self.lost = Some(std::time::Instant::now());
        match self.reopened() {
            Some(screen) => {
                *self = screen;
                true
            }
            None => false,
        }
    }

    fn age(&self) -> u32 {
        // What the GPU drew is one texture drawn into every frame, so a
        // frame owes it only its own damage, as a copied buffer does.
        if self.copied || self.adopted.is_some() {
            1
        } else {
            2
        }
    }

    fn describe(&self) -> String {
        format!(
            "{} {} {} {}x{}",
            self.card.name(),
            self.plan.name,
            compositor_drm::modeset::mode_name(&self.plan.mode),
            self.width,
            self.height
        )
    }

    fn name(&self) -> String {
        self.plan.name.clone()
    }

    fn connected(&self) -> Option<bool> {
        if self.lost.is_some() {
            // A card that went is the driver's restart, not an unplug.
            return Some(true);
        }
        compositor_drm::connected(&self.card, self.plan.connector).ok()
    }

    fn card_place(&self) -> Option<(i32, (u32, u32))> {
        Some((self.card.raw_fd(), (self.plan.connector, self.plan.crtc)))
    }

    fn plugged_in(
        &self,
        held: &[(u32, u32)],
        rules: &[compositor_config::MonitorRule],
    ) -> Vec<Box<dyn Backend>> {
        if self.lost.is_some() {
            return Vec::new();
        }
        Self::plugged_in_on(&self.card, held, rules)
    }

    fn park(&self) -> Option<ParkedCard> {
        self.lost.is_none().then(|| ParkedCard {
            card: std::rc::Rc::clone(&self.card),
            changed: false,
        })
    }

    fn description(&self) -> String {
        self.plan
            .edid
            .as_ref()
            .map(|edid| edid.describe(compositor_drm::registered))
            .unwrap_or_default()
    }

    fn made(&self) -> (String, String, String) {
        self.plan
            .edid
            .as_ref()
            .map_or_else(Default::default, |edid| {
                (
                    compositor_drm::registered(&edid.manufacturer)
                        .unwrap_or_else(|| edid.manufacturer.clone()),
                    edid.model.clone(),
                    edid.serial.clone(),
                )
            })
    }
}

/// Write a backend's last frame as a binary PPM, which every image viewer
/// reads and which needs no library to produce.
///
/// The rows are taken a stride at a time: a card's pitch is whatever the
/// kernel chose and is usually wider than the mode, so a dump that walked
/// the buffer straight through would slant.
///
/// # Errors
///
/// Whatever the write said.
pub fn write_ppm(backend: &dyn Backend, path: &Path) -> io::Result<()> {
    use std::io::Write;
    let (width, height) = backend.size();
    let stride = backend.drawn_stride() as usize;
    let bytes = backend.drawn();
    let mut out = Vec::with_capacity(width as usize * height as usize * 3 + 32);
    write!(out, "P6\n{width} {height}\n255\n")?;
    for y in 0..height as usize {
        let row = bytes
            .get(y * stride..y * stride + width as usize * 4)
            .unwrap_or(&[]);
        for pixel in row.chunks_exact(4) {
            // The buffer is XRGB8888 little-endian: blue, green, red, unused.
            if let [blue, green, red, _] = pixel {
                out.extend_from_slice(&[*red, *green, *blue]);
            }
        }
    }
    std::fs::write(path, out)
}

#[cfg(test)]
mod pick_tests {
    use super::pick_mode;
    use compositor_config::Mode;

    #[test]
    fn a_rule_picks_among_the_listed_modes_as_hyprland_does() {
        let listed = [
            (1920, 1080, 60_000),
            (1920, 1080, 144_000),
            (2560, 1440, 59_951),
            (1280, 720, 240_000),
        ];
        assert_eq!(pick_mode(Mode::Preferred, &listed), None);
        assert_eq!(
            pick_mode(Mode::HighRes, &listed),
            Some(((2560, 1440), Some(59.951)))
        );
        assert_eq!(
            pick_mode(Mode::HighRr, &listed),
            Some(((1280, 720), Some(240.0)))
        );
        assert_eq!(
            pick_mode(
                Mode::Fixed {
                    width: 1920,
                    height: 1080,
                    refresh: Some(144.0)
                },
                &listed
            ),
            Some(((1920, 1080), Some(144.0)))
        );
        assert_eq!(pick_mode(Mode::HighRes, &[]), None);
    }
}
