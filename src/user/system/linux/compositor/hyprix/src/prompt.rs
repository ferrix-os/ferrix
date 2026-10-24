//! The trusted prompt (`docs/SETTINGS-WRITER.md` §12.4): the compositor's
//! own question to the person at the seat, before `settingsd` makes a
//! change the desktop's account asked for.
//!
//! `sessiond` relays `settingsd`'s records on descriptor 5
//! (`compositor_seat::prompt`); this draws the prompt and sends back what
//! the person answered. The rules:
//!
//! * **P1.** While a prompt is up every screen is drawn by the compositor
//!   alone: no window, layer, popup, lock surface or client cursor; the
//!   background and the prompt ([`Prompts::up`] is what the frame asks).
//! * **P2.** Its words are `settingsd`'s, every field shown escaped
//!   ([`shown`]): control and bidirectional characters as `\u{..}`, cut to
//!   a length, the programs' paths in quotes.
//! * **P3.** Only the seat's own devices answer it: the frame's loop hands
//!   in only what came from the evdev descriptors, and drops what a virtual
//!   keyboard or pointer, `sendshortcut` or `pass` made while it is up.
//! * **P4.** Typed characters, Backspace, Return or Allow, Escape or
//!   Cancel; the field is drawn as dots and held in a buffer zeroed when
//!   sent or dropped.
//! * **P5.** Nothing in its first [`ARMING`], and a key or button counts
//!   only if it went down after the prompt came up.
//! * **P6.** Screenshots are black while it is up (the frame's loop).
//! * **P7.** None over a lock and none without a screen; one up when a lock
//!   is taken is denied.
//! * **P8.** Ctrl+Alt+Delete is the compositor's alone: with a prompt up it
//!   says the prompt is real, with none it shows [`NOTICE_FOR`] of "no
//!   system prompt is waiting".
//! * **P9.** One at a time; `close`, a denial or the channel's end ends it.
//!
//! While one is up the compositor is not dumpable (`PR_SET_DUMPABLE` 0),
//! so no crash capture copies the field (ledger 3636 (8)).

use std::os::fd::{AsRawFd as _, RawFd};
use std::os::unix::net::UnixDatagram;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use compositor_layout::Rect;
use compositor_render::Color;
use compositor_seat::prompt::{FromSeat, MAX_RECORD, MAX_SECRET, Secret, ToSeat, zero};
use compositor_text::{Ellipsize, FontDescription, Fonts, Rgba, Size, Weight, tiny_skia};

use crate::overlay::{Mark, Picture, Stamp};

/// How long a fresh prompt ignores input (P5): Firefox's
/// `security.dialog_enable_delay`.
pub(crate) const ARMING: Duration = Duration::from_secs(1);

/// How long a prompt's last words stay up after its `close`.
const CLOSING_FOR: Duration = Duration::from_millis(1200);

/// How long P8's "no system prompt is waiting" stays up.
pub(crate) const NOTICE_FOR: Duration = Duration::from_secs(3);

/// The longest a shown field is, in characters.
const SHOWN_MAX: usize = 120;

/// The evdev codes the prompt reads.
mod code {
    pub(super) const ESC: u16 = 1;
    pub(super) const BACKSPACE: u16 = 14;
    pub(super) const TAB: u16 = 15;
    pub(super) const ENTER: u16 = 28;
    pub(super) const LEFT_CTRL: u16 = 29;
    pub(super) const LEFT_ALT: u16 = 56;
    pub(super) const KP_ENTER: u16 = 96;
    pub(super) const RIGHT_CTRL: u16 = 97;
    pub(super) const RIGHT_ALT: u16 = 100;
    pub(super) const DELETE: u16 = 111;
    pub(super) const KP_DOT: u16 = 83;
    /// `BTN_LEFT`.
    pub(super) const BUTTON_LEFT: u32 = 272;
}

/// The channel `main` took, until the compositor's state takes it.
static ADOPTED: Mutex<Option<UnixDatagram>> = Mutex::new(None);

/// Whether `sessiond` named a prompt channel at all.
static NAMED: AtomicBool = AtomicBool::new(false);

/// Take the prompt channel `sessiond` left this process, if it left one.
/// Call it first thing in `main`, beside `grants::adopt`.
///
/// `None`: none was named. `Some(true)`: the channel. `Some(false)`: one
/// was named and could not be taken; `settingsd` then hears nothing from
/// this seat and refuses its account.
pub fn adopt() -> Option<bool> {
    let adopted = compositor_seat::prompt::adopt_prompt()?;
    NAMED.store(true, Ordering::SeqCst);
    let taken = adopted
        .filter(|socket| socket.set_nonblocking(true).is_ok())
        .and_then(|socket| {
            let mut slot = ADOPTED.lock().ok()?;
            *slot = Some(socket);
            Some(())
        });
    Some(taken.is_some())
}

/// Whether `c` is drawn as nothing, or moves the text around it: control,
/// line and paragraph separators, bidirectional marks and overrides, and
/// the format, joiner, filler, variation-selector and tag characters.
fn invisible(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{00ad}'
            | '\u{034f}'
            | '\u{061c}'
            | '\u{115f}'..='\u{1160}'
            | '\u{17b4}'..='\u{17b5}'
            | '\u{180b}'..='\u{180f}'
            | '\u{200b}'..='\u{200f}'
            | '\u{2028}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{3164}'
            | '\u{fe00}'..='\u{fe0f}'
            | '\u{feff}'
            | '\u{ffa0}'
            | '\u{fff0}'..='\u{fffb}'
            | '\u{1d173}'..='\u{1d17a}'
            | '\u{e0000}'..='\u{e0fff}')
}

/// `text` with every character `escape` says to as `\u{..}`, at most
/// [`SHOWN_MAX`] characters.
fn shown_with(text: &str, escape: impl Fn(char) -> bool) -> String {
    let mut out = String::new();
    for (count, c) in text.chars().enumerate() {
        if count >= SHOWN_MAX {
            out.push('…');
            break;
        }
        if escape(c) {
            out.push_str(&format!("\\u{{{:x}}}", u32::from(c)));
        } else {
            out.push(c);
        }
    }
    out
}

/// `text` as a prompt shows it (P2): every invisible or text-moving
/// character as `\u{..}`, at most [`SHOWN_MAX`] characters.
#[must_use]
pub fn shown(text: &str) -> String {
    shown_with(text, invisible)
}

/// A program's path or an account's name as a prompt shows it: the asker
/// chooses its parent's path, so anything but printable ASCII is escaped
/// (ledger 3636 (5), A4).
#[must_use]
pub fn shown_ascii(text: &str) -> String {
    shown_with(text, |c| !(c.is_ascii_graphic() || c == ' '))
}

/// The words for an area, in the prompt's heading.
fn area_words(area: &str) -> &'static str {
    match area {
        "hostname" => "the computer's name",
        "time" => "the clock or the time zone",
        "services" => "which services run",
        _ => "a system setting",
    }
}

/// What the prompt shows under its heading.
#[derive(Debug, PartialEq, Eq)]
enum Stage {
    /// Asked; `authd` has said nothing yet.
    Waiting,
    /// A field for `authd`'s question.
    Field {
        /// Whether what is typed may be shown.
        visible: bool,
        /// `authd`'s words.
        label: String,
    },
    /// Allow and Cancel alone.
    Confirm,
}

/// A button of the prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Button {
    Cancel,
    Allow,
}

/// The prompt up now.
struct Open {
    id: u64,
    heading: String,
    lines: Vec<String>,
    stage: Stage,
    info: String,
    shown_at: Instant,
    /// Keys and buttons held when it came up: their releases and repeats
    /// are nothing (P5).
    held_at_open: Vec<u32>,
    /// Keys and buttons pressed since: only their releases count.
    pressed: Vec<u32>,
    typed: Vec<u8>,
    closing: Option<(String, Instant)>,
}

impl Drop for Open {
    fn drop(&mut self) {
        zero(&mut self.typed);
    }
}

impl core::fmt::Debug for Open {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Never the field, nor its length.
        f.debug_struct("Open")
            .field("id", &self.id)
            .field("stage", &self.stage)
            .finish_non_exhaustive()
    }
}

/// The prompt's state: the channel, the prompt up now, and P8's notice.
pub(crate) struct Prompts {
    channel: Option<UnixDatagram>,
    open: Option<Open>,
    notice_until: Option<Instant>,
    /// Each change to what is drawn: the damage's stamp.
    generation: u64,
    fonts: Option<Fonts>,
    ctrl: [bool; 2],
    alt: [bool; 2],
    /// Where each screen's buttons are, in the layout's coordinates.
    buttons: Vec<(Rect, Button)>,
    /// What was sent and could not be: none of it is retried.
    lost: bool,
    /// Lines for the compositor's log, taken by [`Prompts::take_said`].
    said: Vec<String>,
}

impl core::fmt::Debug for Prompts {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Prompts")
            .field("channel", &self.channel.is_some())
            .field("up", &self.up())
            .finish_non_exhaustive()
    }
}

/// Whether the compositor may be dumped.
fn set_dumpable(dumpable: bool) {
    #[expect(
        unsafe_code,
        reason = "AUDIT: prctl(PR_SET_DUMPABLE) takes plain integers and reads no memory"
    )]
    // SAFETY: as the reason says.
    let _ = unsafe {
        libc::prctl(
            libc::PR_SET_DUMPABLE,
            libc::c_ulong::from(dumpable),
            0,
            0,
            0,
        )
    };
}

impl Prompts {
    /// The channel `main` adopted, if any.
    pub(crate) fn adopted() -> Prompts {
        let channel = ADOPTED.lock().ok().and_then(|mut adopted| adopted.take());
        Prompts::with(channel)
    }

    /// A prompt state on `channel`: for tests, a socket pair's end.
    pub(crate) fn with(channel: Option<UnixDatagram>) -> Prompts {
        Prompts {
            channel,
            open: None,
            notice_until: None,
            generation: 0,
            fonts: None,
            ctrl: [false; 2],
            alt: [false; 2],
            buttons: Vec::new(),
            lost: false,
            said: Vec::new(),
        }
    }

    /// What to say in the log since last asked: when a prompt came up and
    /// went, never what was typed.
    pub(crate) fn take_said(&mut self) -> Vec<String> {
        std::mem::take(&mut self.said)
    }

    /// The channel's descriptor, for the loop's wait.
    pub(crate) fn raw_fd(&self) -> Option<RawFd> {
        self.channel.as_ref().map(UnixDatagram::as_raw_fd)
    }

    /// Whether the frame is the prompt's: a prompt, or P8's notice.
    pub(crate) fn up(&self) -> bool {
        self.open.is_some() || self.notice_until.is_some()
    }

    /// Whether a prompt is up and taking an answer (not its last words).
    pub(crate) fn asking(&self) -> bool {
        self.open
            .as_ref()
            .is_some_and(|open| open.closing.is_none())
    }

    fn send(&mut self, record: &FromSeat) {
        let (Some(channel), Some(mut bytes)) = (self.channel.as_ref(), record.encode()) else {
            return;
        };
        if channel.send(&bytes).is_err() {
            self.lost = true;
        }
        zero(&mut bytes);
    }

    fn changed(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    /// Take the prompt down now, zeroing its field.
    fn drop_open(&mut self) {
        if self.open.take().is_some() {
            set_dumpable(true);
            self.changed();
        }
    }

    /// Deny the open prompt for `why`, and show its last words.
    fn deny(&mut self, why: &str, words: &str) {
        let Some(id) = self.open.as_ref().map(|open| open.id) else {
            return;
        };
        self.send(&FromSeat::Deny {
            id,
            why: why.to_owned(),
        });
        self.closing(words);
    }

    fn closing(&mut self, words: &str) {
        if let Some(open) = self.open.as_mut() {
            zero(&mut open.typed);
            open.typed.clear();
            open.closing = Some((shown(words), Instant::now() + CLOSING_FOR));
        }
        self.changed();
    }

    /// What `sessiond` relayed this pass. `locked` and `screens` say
    /// whether a prompt may come up (P7). Gives whether the frame changed.
    pub(crate) fn read(&mut self, locked: bool, screens: bool) -> bool {
        let mut changed = false;
        let mut buffer = [0_u8; MAX_RECORD + 1];
        while let Some(channel) = self.channel.as_ref() {
            match channel.recv(&mut buffer) {
                Ok(0) => {
                    self.said
                        .push("hyprix: the prompt channel closed".to_owned());
                    self.channel = None;
                    self.drop_open();
                    changed = true;
                    break;
                }
                Ok(got) => {
                    let record = buffer
                        .get(..got)
                        .filter(|_| got <= MAX_RECORD)
                        .and_then(ToSeat::decode);
                    changed |= self.heard(record, locked, screens);
                }
                Err(_) => break,
            }
        }
        zero(&mut buffer);
        changed
    }

    /// One record from `settingsd`, through `sessiond`.
    fn heard(&mut self, record: Option<ToSeat>, locked: bool, screens: bool) -> bool {
        let open_id = self.open.as_ref().map(|open| open.id);
        match record {
            Some(ToSeat::Ask {
                id,
                area,
                program,
                parent,
                account,
                what,
            }) => {
                if open_id.is_some() {
                    self.send(&FromSeat::Deny {
                        id,
                        why: "busy".to_owned(),
                    });
                    return false;
                }
                if locked || !screens {
                    let why = if locked { "locked" } else { "no-screen" };
                    self.send(&FromSeat::Deny {
                        id,
                        why: why.to_owned(),
                    });
                    return false;
                }
                set_dumpable(false);
                self.notice_until = None;
                self.open = Some(Open {
                    id,
                    heading: format!("Allow a change to {}?", area_words(&area)),
                    lines: vec![
                        format!(
                            "“{}”, started by “{}”, asks as {}:",
                            shown_ascii(&program),
                            shown_ascii(&parent),
                            shown_ascii(&account)
                        ),
                        shown(&what),
                    ],
                    stage: Stage::Waiting,
                    info: String::new(),
                    shown_at: Instant::now(),
                    held_at_open: Vec::new(),
                    pressed: Vec::new(),
                    // Never grown, so no copy of a prefix is left behind
                    // in a freed block (ledger 3636 (8), A3).
                    typed: Vec::with_capacity(MAX_SECRET),
                    closing: None,
                });
                self.said.push("hyprix: a trusted prompt is up".to_owned());
                self.changed();
                true
            }
            Some(ToSeat::Question { id, visible, text }) if Some(id) == open_id => {
                if let Some(open) = self.open.as_mut() {
                    open.stage = Stage::Field {
                        visible,
                        label: shown(&text),
                    };
                    zero(&mut open.typed);
                    open.typed.clear();
                }
                self.said.push("hyprix: the trusted prompt asks".to_owned());
                self.changed();
                true
            }
            Some(ToSeat::Info { id, text }) if Some(id) == open_id => {
                if let Some(open) = self.open.as_mut() {
                    open.info = shown(&text);
                }
                self.changed();
                true
            }
            Some(ToSeat::Confirm { id }) if Some(id) == open_id => {
                if let Some(open) = self.open.as_mut() {
                    open.stage = Stage::Confirm;
                }
                self.said.push("hyprix: the trusted prompt asks".to_owned());
                self.changed();
                true
            }
            Some(ToSeat::Close { id, text }) if Some(id) == open_id => {
                self.closing(&text);
                self.said
                    .push("hyprix: the trusted prompt closed".to_owned());
                true
            }
            // Anything else -- for another prompt, or no record -- is
            // nothing to draw and answers nothing.
            _ => false,
        }
    }

    /// The keys and buttons held as a prompt comes up: their releases
    /// answer nothing (P5). The loop calls it with the seat's held keys
    /// and buttons once a prompt is new.
    pub(crate) fn note_held(&mut self, held: impl IntoIterator<Item = u32>) {
        if let Some(open) = self.open.as_mut()
            && open.held_at_open.is_empty()
        {
            open.held_at_open = held.into_iter().collect();
        }
    }

    /// P8: whether this key, from the seat's own devices, is the trusted
    /// key or part of it. Tracks the Ctrl and Alt keys itself. A press of
    /// Delete (or keypad `.`) with both held is the compositor's: it shows
    /// the notice when no prompt is up, and is never anyone else's.
    pub(crate) fn trusted_key(&mut self, code: u16, pressed: bool, repeat: bool) -> bool {
        match code {
            code::LEFT_CTRL => self.ctrl[0] = pressed,
            code::RIGHT_CTRL => self.ctrl[1] = pressed,
            code::LEFT_ALT => self.alt[0] = pressed,
            code::RIGHT_ALT => self.alt[1] = pressed,
            code::DELETE | code::KP_DOT => {
                let chord = self.ctrl.contains(&true) && self.alt.contains(&true);
                if chord {
                    if pressed && !repeat && self.open.is_none() {
                        self.notice_until = Some(Instant::now() + NOTICE_FOR);
                        self.said
                            .push("hyprix: the trusted key: no prompt is waiting".to_owned());
                        self.changed();
                    }
                    return true;
                }
            }
            _ => {}
        }
        false
    }

    /// Whether input aimed at the prompt counts yet (P5).
    fn armed(open: &Open, now: Instant) -> bool {
        open.closing.is_none() && now.duration_since(open.shown_at) >= ARMING
    }

    /// A key from the seat's own devices, with the keysym the seat's layout
    /// makes of it. Gives whether the frame changed.
    pub(crate) fn key(&mut self, code: u16, pressed: bool, keysym: Option<&str>) -> bool {
        let now = Instant::now();
        let Some(open) = self.open.as_mut() else {
            return false;
        };
        let code32 = u32::from(code);
        if !pressed {
            open.pressed.retain(|held| *held != code32);
            open.held_at_open.retain(|held| *held != code32);
            return false;
        }
        if open.held_at_open.contains(&code32) || !Self::armed(open, now) {
            return false;
        }
        if !open.pressed.contains(&code32) {
            open.pressed.push(code32);
        }
        match code {
            code::ESC => {
                self.deny("cancelled", "Cancelled");
                true
            }
            code::ENTER | code::KP_ENTER => self.press(Button::Allow),
            code::TAB => false,
            code::BACKSPACE => {
                if let Some(open) = self.open.as_mut()
                    && matches!(open.stage, Stage::Field { .. })
                {
                    let keep = std::str::from_utf8(&open.typed)
                        .ok()
                        .and_then(|text| text.char_indices().last())
                        .map_or(0, |(at, _)| at);
                    for byte in open.typed.get_mut(keep..).unwrap_or_default() {
                        *byte = 0;
                    }
                    open.typed.truncate(keep);
                }
                self.changed();
                true
            }
            _ => {
                let Some(open) = self.open.as_mut() else {
                    return false;
                };
                let Stage::Field { .. } = open.stage else {
                    return false;
                };
                let Some(c) = keysym.and_then(compositor_xkb::character) else {
                    return false;
                };
                if c.is_control() {
                    return false;
                }
                let mut bytes = [0_u8; 4];
                let encoded = c.encode_utf8(&mut bytes).as_bytes();
                if open.typed.len() + encoded.len() <= MAX_SECRET {
                    open.typed.extend_from_slice(encoded);
                }
                zero(&mut bytes);
                self.changed();
                true
            }
        }
    }

    /// A button press or release from the seat's own pointer at `at`, the
    /// layout's coordinates. Gives whether the frame changed.
    pub(crate) fn button(&mut self, button: u32, pressed: bool, at: (f64, f64)) -> bool {
        let now = Instant::now();
        let Some(open) = self.open.as_mut() else {
            return false;
        };
        let code = button | 0x1_0000;
        if pressed {
            if !open.held_at_open.contains(&code) && Self::armed(open, now) {
                open.pressed.push(code);
            }
            return false;
        }
        let counted = open.pressed.contains(&code);
        open.pressed.retain(|held| *held != code);
        open.held_at_open.retain(|held| *held != code);
        if !counted || button != code::BUTTON_LEFT || !Self::armed(open, now) {
            return false;
        }
        #[expect(clippy::cast_possible_truncation, reason = "a pointer on a screen")]
        let (x, y) = (at.0 as i64, at.1 as i64);
        let hit = self
            .buttons
            .iter()
            .find(|(rect, _)| {
                x >= rect.x && y >= rect.y && x < rect.x + rect.width && y < rect.y + rect.height
            })
            .map(|(_, which)| *which);
        match hit {
            Some(which) => self.press(which),
            None => false,
        }
    }

    /// Allow or Cancel pressed.
    fn press(&mut self, which: Button) -> bool {
        let Some(open) = self.open.as_mut() else {
            return false;
        };
        let id = open.id;
        if which == Button::Cancel {
            self.deny("cancelled", "Cancelled");
            return true;
        }
        match open.stage {
            Stage::Field { .. } => {
                let secret = Secret::new(&open.typed);
                zero(&mut open.typed);
                open.typed.clear();
                open.stage = Stage::Waiting;
                "Checking…".clone_into(&mut open.info);
                if let Some(secret) = secret {
                    self.send(&FromSeat::Answer { id, secret });
                }
            }
            Stage::Confirm => {
                open.stage = Stage::Waiting;
                self.send(&FromSeat::Allow { id });
            }
            Stage::Waiting => return false,
        }
        self.changed();
        true
    }

    /// A lock was taken: a prompt up is denied (P7).
    pub(crate) fn lock_taken(&mut self) {
        if self.asking() {
            self.deny("locked", "The screen was locked");
        }
        self.notice_until = None;
    }

    /// When the prompt's frame next changes by itself: its arming, its
    /// last words, the notice's end.
    pub(crate) fn next_wake(&self) -> Option<Instant> {
        let open = self.open.as_ref().and_then(|open| match &open.closing {
            Some((_, until)) => Some(*until),
            None => {
                let armed = open.shown_at + ARMING;
                (Instant::now() < armed).then_some(armed)
            }
        });
        [open, self.notice_until].into_iter().flatten().min()
    }

    /// What ended by its own clock. Gives whether the frame changed.
    pub(crate) fn tick(&mut self, now: Instant) -> bool {
        let mut changed = false;
        if self
            .open
            .as_ref()
            .and_then(|open| open.closing.as_ref())
            .is_some_and(|(_, until)| now >= *until)
        {
            self.drop_open();
            changed = true;
        }
        if self.notice_until.is_some_and(|until| now >= until) {
            self.notice_until = None;
            self.changed();
            changed = true;
        }
        if let Some(open) = self.open.as_ref()
            && open.closing.is_none()
            && now >= open.shown_at + ARMING
            && now < open.shown_at + ARMING + Duration::from_millis(100)
        {
            // The buttons are drawn live once it is armed.
            self.changed();
            changed = true;
        }
        changed
    }

    /// Forget where the buttons were: the frame lays them out again.
    pub(crate) fn begin_frame(&mut self) {
        self.buttons.clear();
    }

    /// The picture for a screen `width` by `height` pixels at `scale`,
    /// whose top left is `origin` in the layout's coordinates; `None` while
    /// nothing is up.
    pub(crate) fn picture(
        &mut self,
        (width, height): (u32, u32),
        scale: f64,
        origin: (i64, i64),
    ) -> Option<Picture> {
        if !self.up() {
            return None;
        }
        let whole = Rect::new(0, 0, i64::from(width), i64::from(height));
        let mut marks = vec![Mark::Fill {
            rect: whole,
            color: BACKGROUND,
        }];
        #[expect(clippy::cast_possible_truncation, reason = "a scale is small")]
        let s = scale.max(1.0) as f32;
        let card = self.card(s);
        if let Some((pixels, card_w, card_h, buttons)) = card {
            let x = (i64::from(width) - card_w) / 2;
            let y = (i64::from(height) - card_h) / 2;
            marks.push(Mark::Text {
                rect: Rect::new(x, y, card_w, card_h),
                pixels,
            });
            for (rect, which) in buttons {
                // The card's pixels back to the layout's coordinates.
                #[expect(clippy::cast_possible_truncation, reason = "on a screen")]
                let to = |value: i64| (value as f64 / scale) as i64;
                self.buttons.push((
                    Rect::new(
                        origin.0 + to(x + rect.x),
                        origin.1 + to(y + rect.y),
                        to(rect.width),
                        to(rect.height),
                    ),
                    which,
                ));
            }
        }
        Some(Picture::of(
            marks,
            Stamp {
                rect: whole,
                generation: self.generation,
            },
        ))
    }

    /// The card, drawn at scale `s`: its pixels, size, and its buttons'
    /// rectangles within it.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        clippy::too_many_lines,
        reason = "a card of a few hundred pixels, laid out line by line"
    )]
    fn card(&mut self, s: f32) -> Option<Card> {
        let now = Instant::now();
        let (heading, lines, footer, field, info, buttons_live, closing) = match &self.open {
            Some(open) => {
                let field = match &open.stage {
                    Stage::Field { visible, label } => Some((
                        label.clone(),
                        if *visible {
                            String::from_utf8_lossy(&open.typed).into_owned()
                        } else {
                            "•".repeat(
                                std::str::from_utf8(&open.typed).map_or(0, |t| t.chars().count()),
                            )
                        },
                    )),
                    _ => None,
                };
                let live = Self::armed(open, now) && open.stage != Stage::Waiting;
                (
                    open.heading.clone(),
                    open.lines.clone(),
                    "Ctrl+Alt+Delete shows that this prompt is real.".to_owned(),
                    field,
                    open.info.clone(),
                    Some(live),
                    open.closing.as_ref().map(|(words, _)| words.clone()),
                )
            }
            None => (
                "No system prompt is waiting".to_owned(),
                vec![
                    "A window asking for your password now is not Ferrix.".to_owned(),
                    "Ferrix asks only on this screen, which no program can draw.".to_owned(),
                ],
                String::new(),
                None,
                String::new(),
                None,
                None,
            ),
        };
        let w = 560.0 * s;
        let pad = 28.0 * s;
        let line_h = 24.0 * s;
        let mut h = pad + 34.0 * s + lines.len() as f32 * line_h + 12.0 * s;
        if field.is_some() {
            h += 22.0 * s + 40.0 * s + 8.0 * s;
        }
        if !info.is_empty() || closing.is_some() {
            h += line_h;
        }
        if buttons_live.is_some() {
            h += 16.0 * s + 40.0 * s;
        }
        if !footer.is_empty() {
            h += 12.0 * s + 20.0 * s;
        }
        h += pad;
        let (pw, ph) = (w.round() as u32, h.round() as u32);
        let mut pixmap = tiny_skia::Pixmap::new(pw, ph)?;
        pixmap.fill(CARD.to_skia());
        let fonts = self.fonts.get_or_insert_with(Fonts::system);
        let text = |fonts: &mut Fonts,
                    pixmap: &mut tiny_skia::Pixmap,
                    words: &str,
                    (x, baseline): (f32, f32),
                    px: f32,
                    bold: bool,
                    ink: Rgba| {
            let mut description = FontDescription::pango("sans-serif");
            description.size = Size::Pixels(px);
            if bold {
                description.weight = Weight::BOLD;
            }
            let font = fonts.resolve(&description);
            let run = fonts.shape_ellipsized(&font, words, w - 2.0 * pad, Ellipsize::End);
            fonts.draw_run(&run, &mut pixmap.as_mut(), x, baseline, ink);
            run.width
        };
        let mut y = pad + 24.0 * s;
        let _ = text(fonts, &mut pixmap, &heading, (pad, y), 22.0 * s, true, INK);
        y += 10.0 * s;
        for line in &lines {
            y += line_h;
            let _ = text(fonts, &mut pixmap, line, (pad, y), 15.0 * s, false, INK);
        }
        y += 12.0 * s;
        if let Some((label, typed)) = &field {
            y += 18.0 * s;
            let _ = text(fonts, &mut pixmap, label, (pad, y), 15.0 * s, true, INK);
            y += 8.0 * s;
            fill(&mut pixmap, (pad, y, w - 2.0 * pad, 40.0 * s), FIELD);
            let _ = text(
                fonts,
                &mut pixmap,
                typed,
                (pad + 12.0 * s, y + 26.0 * s),
                17.0 * s,
                false,
                INK,
            );
            y += 40.0 * s + 8.0 * s;
        }
        if let Some(words) = &closing {
            y += line_h;
            let _ = text(fonts, &mut pixmap, words, (pad, y), 15.0 * s, true, INK);
        } else if !info.is_empty() {
            y += line_h;
            let _ = text(fonts, &mut pixmap, &info, (pad, y), 15.0 * s, false, WARN);
        }
        let mut buttons = Vec::new();
        if let Some(live) = buttons_live {
            y += 16.0 * s;
            let (bw, bh) = (120.0 * s, 40.0 * s);
            let allow_x = w - pad - bw;
            let cancel_x = allow_x - 12.0 * s - bw;
            for (x, which, label) in [
                (cancel_x, Button::Cancel, "Cancel"),
                (allow_x, Button::Allow, "Allow"),
            ] {
                let colour = match (which, live && closing.is_none()) {
                    (Button::Allow, true) => ACCENT,
                    (_, false) => DIM,
                    (Button::Cancel, true) => FIELD,
                };
                fill(&mut pixmap, (x, y, bw, bh), colour);
                let ink = if which == Button::Allow && live {
                    CARD
                } else {
                    INK
                };
                let mut description = FontDescription::pango("sans-serif");
                description.size = Size::Pixels(15.0 * s);
                description.weight = Weight::BOLD;
                let font = fonts.resolve(&description);
                let run = fonts.shape_ellipsized(&font, label, bw, Ellipsize::End);
                fonts.draw_run(
                    &run,
                    &mut pixmap.as_mut(),
                    x + (bw - run.width) / 2.0,
                    y + 26.0 * s,
                    ink,
                );
                buttons.push((Rect::new(x as i64, y as i64, bw as i64, bh as i64), which));
            }
            y += bh;
        }
        if !footer.is_empty() {
            y += 12.0 * s + 14.0 * s;
            let _ = text(
                fonts,
                &mut pixmap,
                &footer,
                (pad, y),
                13.0 * s,
                false,
                DIM_INK,
            );
        }
        Some((argb(&pixmap), i64::from(pw), i64::from(ph), buttons))
    }
}

/// A drawn card: its pixels, width and height, and its buttons.
type Card = (Vec<u8>, i64, i64, Vec<(Rect, Button)>);

/// The screen behind a prompt: nothing a client drew.
const BACKGROUND: Color = Color(0xff10_1418);

/// The card's colours: the Ferrix light theme's.
const CARD: Rgba = Rgba::rgb(0xf7, 0xf7, 0xf8);
const INK: Rgba = Rgba::rgb(0x1d, 0x1d, 0x1f);
const DIM_INK: Rgba = Rgba::rgb(0x6e, 0x6e, 0x73);
const FIELD: Rgba = Rgba::rgb(0xe3, 0xe3, 0xe8);
const DIM: Rgba = Rgba::rgb(0xd0, 0xd0, 0xd4);
const ACCENT: Rgba = Rgba::rgb(0x2f, 0x6f, 0xeb);
const WARN: Rgba = Rgba::rgb(0xb4, 0x23, 0x18);

/// A rectangle of one colour in `pixmap`.
fn fill(pixmap: &mut tiny_skia::Pixmap, (x, y, w, h): (f32, f32, f32, f32), colour: Rgba) {
    let mut paint = tiny_skia::Paint::default();
    paint.set_color(colour.to_skia());
    if let Some(rect) = tiny_skia::Rect::from_xywh(x, y, w, h) {
        pixmap.fill_rect(rect, &paint, tiny_skia::Transform::identity(), None);
    }
}

/// A tiny-skia pixmap as premultiplied `ARGB8888` in memory order.
fn argb(pixmap: &tiny_skia::Pixmap) -> Vec<u8> {
    let mut out = Vec::with_capacity(pixmap.data().len());
    for pixel in pixmap.data().chunks_exact(4) {
        if let [r, g, b, a] = *pixel {
            out.extend([b, g, r, a]);
        }
    }
    out
}

#[cfg(test)]
mod tests;
